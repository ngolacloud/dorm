//! Live proof that the pool recovers when the server kills its connections
//! (ADR 0015): after `pg_terminate_backend` drops every pooled connection, the
//! next queries still succeed, because `Verified` recycling detects a dead
//! connection on checkout and replaces it instead of handing it to a query.
//!
//! Ignored by default — it needs a throwaway PostgreSQL (not CockroachDB:
//! `pg_terminate_backend`/`pg_stat_activity` are PostgreSQL's):
//!
//! ```console
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=55432 user=dorm password=dorm dbname=dorm_dev' \
//!   cargo test -p dorm-core --test resilience_live -- --ignored --nocapture
//! ```

use dorm_core::db;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a throwaway PostgreSQL; set DELONIX_ORM_TEST_DSN"]
async fn the_pool_recovers_after_the_server_kills_its_connections() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_TEST_DSN").or_else(|_| std::env::var(db::DSN_ENV))
    else {
        eprintln!("skipped: set DELONIX_ORM_TEST_DSN to a throwaway Postgres");
        return;
    };
    // All of this must be set before the first `db::*` call — the pool reads it
    // once, when it is first built.
    std::env::set_var(db::DSN_ENV, &dsn);
    std::env::set_var(db::POOL_RECYCLING_ENV, "verified");
    std::env::set_var(db::POOL_MAX_ENV, "4");

    // Warm several connections concurrently, so the pool holds more than one and
    // the kill below leaves dead *idle* connections behind, not just the live one
    // the kill statement itself runs on.
    let warm: Vec<_> = (0..4)
        .map(|_| tokio::spawn(async { db::query("SELECT pg_sleep(0.1)", &[]).await }))
        .collect();
    for h in warm {
        h.await.expect("join").expect("warm-up query");
    }

    // The server drops every connection for this database except the one running
    // this statement — so the pool's other three are now dead sockets.
    db::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE pid <> pg_backend_pid() AND datname = current_database() \
         AND backend_type = 'client backend'",
        &[],
    )
    .await
    .expect("terminating the other backends is itself a query");

    // Every query from here must still succeed: a checkout that pops a killed
    // connection verifies it, finds it dead, and replaces it — the query never
    // sees the corpse. Ten of them, to be sure a dead one is actually drawn.
    for i in 0..10 {
        let rows = db::query("SELECT 1", &[])
            .await
            .unwrap_or_else(|e| panic!("query {i} after the kill must recover, got: {e}"));
        assert_eq!(rows.len(), 1, "query {i} returned a row");
    }
}
