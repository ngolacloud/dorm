//! CQRS proven live against PostgreSQL (ADR 0012): a command lands on the
//! primary, a query is served by the replica, and a query cannot write.
//!
//! Reuses the two-database trick from `read_replica_routing_live`: the "replica"
//! is a separate database with its own sentinel, so a read that returns the
//! replica's sentinel could only have run there. Ignored by default — needs two
//! throwaway databases:
//!
//! ```console
//! DORM_ROUTE_PRIMARY_DSN='host=127.0.0.1 port=55432 user=dorm password=dorm dbname=dorm_route_primary' \
//! DORM_ROUTE_REPLICA_DSN='host=127.0.0.1 port=55432 user=dorm password=dorm dbname=dorm_route_replica' \
//!   cargo test -p dorm-core --test cqrs_routing_live -- --ignored --nocapture
//! ```

use std::sync::Arc;

use dorm_core::cqrs;
use dorm_core::db::{self, Database};
use dorm_core::{Error, Result};

async fn seed(dsn: &str, who: &str) {
    let only = Arc::new(Database::connect(dsn, None, false).expect("connect seed db"));
    db::with_session(only, async move {
        db::run_ddl("DROP TABLE IF EXISTS cqrs_probe")
            .await
            .unwrap();
        db::run_ddl("CREATE TABLE cqrs_probe (who text NOT NULL, hits int NOT NULL DEFAULT 0)")
            .await
            .unwrap();
        db::execute("INSERT INTO cqrs_probe (who) VALUES ($1)", &[&who])
            .await
            .unwrap();
    })
    .await;
}

/// A read through the typed funnel (`fetch_read`) — replica-eligible, so the
/// sentinel it returns reveals which database served the read.
async fn who() -> Result<String> {
    let rows = db::fetch_read("SELECT who FROM cqrs_probe", &[]).await?;
    rows[0].try_get::<String>(0)
}

/// `hits` on a named database, read directly (primary) — to check where a write
/// landed.
async fn hits_on(dsn: &str) -> i32 {
    let only = Arc::new(Database::connect(dsn, None, false).unwrap());
    db::with_session(only, async move {
        db::query("SELECT hits FROM cqrs_probe", &[]).await.unwrap()[0].get::<_, i32>(0)
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs two throwaway PostgreSQL databases; set DORM_ROUTE_PRIMARY_DSN and DORM_ROUTE_REPLICA_DSN"]
async fn commands_hit_the_primary_and_queries_hit_the_replica() {
    let primary_dsn = std::env::var("DORM_ROUTE_PRIMARY_DSN")
        .or_else(|_| std::env::var(db::DSN_ENV))
        .expect("set DORM_ROUTE_PRIMARY_DSN");
    let replica_dsn = std::env::var("DORM_ROUTE_REPLICA_DSN")
        .or_else(|_| std::env::var(db::REPLICA_DSN_ENV))
        .expect("set DORM_ROUTE_REPLICA_DSN");

    seed(&primary_dsn, "primary").await;
    seed(&replica_dsn, "replica").await;

    let dbh = Arc::new(
        Database::connect(&primary_dsn, Some(&replica_dsn), false)
            .expect("connect primary+replica"),
    );

    db::with_session(dbh, async move {
        // A query's read is served by the replica — it sees the replica sentinel.
        assert_eq!(cqrs::query(who).await.expect("query runs"), "replica");

        // A query CANNOT write — the raw escape hatch is refused by name.
        let err = cqrs::query(|| async {
            db::sql_execute("UPDATE cqrs_probe SET hits = hits + 1", &[]).await
        })
        .await
        .expect_err("a query must not be able to write");
        assert!(
            matches!(err, Error::ReadOnly(_)),
            "expected ReadOnly, got {err}"
        );

        // A command writes, atomically, on the primary.
        let affected = cqrs::command(|| async {
            db::sql_execute("UPDATE cqrs_probe SET hits = hits + 1", &[]).await
        })
        .await
        .expect("command runs");
        assert_eq!(affected, 1, "the command updated the primary's one row");
    })
    .await;

    // The write landed on the primary, and only the primary.
    assert_eq!(
        hits_on(&primary_dsn).await,
        1,
        "the command hit the primary"
    );
    assert_eq!(
        hits_on(&replica_dsn).await,
        0,
        "the command did not touch the replica"
    );
}
