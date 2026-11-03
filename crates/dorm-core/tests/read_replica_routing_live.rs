//! Live proof that an explicit [`Database`] handle routes reads to a read
//! replica and keeps writes — and reads inside a transaction — on the primary
//! (B-13 / the database-handle ADR).
//!
//! There is no real streaming replica here: the "replica" is a *separate*
//! database holding a different sentinel, so a read that comes back with the
//! replica's sentinel could only have run on the replica. That is a stricter
//! proof of routing than a true replica would give, where both sides return the
//! same row and routing is invisible.
//!
//! Ignored by default — it needs two throwaway PostgreSQL databases. Create them
//! and run it with their DSNs:
//!
//! ```console
//! psql "$BASE" -c 'CREATE DATABASE dorm_route_primary' -c 'CREATE DATABASE dorm_route_replica'
//! DORM_ROUTE_PRIMARY_DSN='host=127.0.0.1 port=55432 user=dorm password=dorm dbname=dorm_route_primary' \
//! DORM_ROUTE_REPLICA_DSN='host=127.0.0.1 port=55432 user=dorm password=dorm dbname=dorm_route_replica' \
//!   cargo test -p dorm-core --test read_replica_routing_live -- --ignored --nocapture
//! ```

use std::sync::Arc;

use dorm_core::db::{self, Database};

/// Stand up the probe table on one database and stamp it with `who`, by running
/// against a handle that has only that database — so the write can only land there.
async fn seed(dsn: &str, who: &str) {
    let only = Arc::new(Database::connect(dsn, None, false).expect("connect to seed database"));
    db::with_session(only, async move {
        db::run_ddl("DROP TABLE IF EXISTS route_probe")
            .await
            .unwrap();
        db::run_ddl("CREATE TABLE route_probe (who text NOT NULL, hits int NOT NULL DEFAULT 0)")
            .await
            .unwrap();
        db::execute("INSERT INTO route_probe (who) VALUES ($1)", &[&who])
            .await
            .unwrap();
    })
    .await;
}

/// The single `who` value `route_probe` holds, read through a given path.
async fn who_via_query(read: bool) -> String {
    let rows = if read {
        db::query_read("SELECT who FROM route_probe", &[]).await
    } else {
        db::query("SELECT who FROM route_probe", &[]).await
    }
    .expect("select who");
    rows[0].get::<_, String>(0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs two throwaway PostgreSQL databases; set DORM_ROUTE_PRIMARY_DSN and DORM_ROUTE_REPLICA_DSN"]
async fn read_queries_route_to_replica() {
    let primary_dsn = std::env::var("DORM_ROUTE_PRIMARY_DSN")
        .or_else(|_| std::env::var(db::DSN_ENV))
        .expect("set DORM_ROUTE_PRIMARY_DSN (the primary DSN)");
    let replica_dsn = std::env::var("DORM_ROUTE_REPLICA_DSN")
        .or_else(|_| std::env::var(db::REPLICA_DSN_ENV))
        .expect("set DORM_ROUTE_REPLICA_DSN (the replica DSN)");

    // Each database carries its own sentinel, so a read reveals where it ran.
    seed(&primary_dsn, "primary").await;
    seed(&replica_dsn, "replica").await;

    let db = Arc::new(
        Database::connect(&primary_dsn, Some(&replica_dsn), false)
            .expect("connect primary+replica"),
    );

    db::with_session(db, async move {
        // A read goes to the replica: it comes back with the replica's sentinel.
        assert_eq!(
            who_via_query(true).await,
            "replica",
            "a read must hit the replica"
        );

        // The value-based read funnel the typed QuerySet uses (`fetch_read`) routes
        // the same way — this is the seam a `SELECT` model query flows through.
        let rows = db::fetch_read("SELECT who FROM route_probe", &[])
            .await
            .unwrap();
        assert_eq!(
            rows[0].try_get::<String>(0).unwrap(),
            "replica",
            "the typed read funnel must hit the replica too"
        );

        // A plain `query` (the primary route) and every write stay on the primary.
        assert_eq!(
            who_via_query(false).await,
            "primary",
            "a primary read stays on the primary"
        );
        let affected = db::execute("UPDATE route_probe SET hits = hits + 1", &[])
            .await
            .expect("a write runs");
        assert_eq!(
            affected, 1,
            "the write ran on exactly the primary's one row"
        );

        // Inside a transaction a read must see its own writes, so it is pinned to
        // the primary even though it is a read: the replica is not an option here.
        db::atomic(|_tx| async move {
            let who = who_via_query(true).await;
            assert_eq!(
                who, "primary",
                "a read inside a transaction stays on the primary"
            );
            Ok(())
        })
        .await
        .expect("transaction commits");

        // The primary's write took effect (confirming the UPDATE hit the primary).
        let hits = db::query("SELECT hits FROM route_probe", &[])
            .await
            .unwrap()[0]
            .get::<_, i32>(0);
        assert_eq!(hits, 1, "the primary recorded the write");
    })
    .await;
}
