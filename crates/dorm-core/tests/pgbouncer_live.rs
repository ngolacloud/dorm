//! PgBouncer transaction-pooling awareness (ADR 0049 / L15) against a live
//! PgBouncer in front of a real PostgreSQL.
//!
//! Set up (what this test expects):
//! ```text
//! # pgbouncer.ini
//! #   pool_mode = transaction
//! #   max_prepared_statements = 200   ; the authoritative fix (PgBouncer ≥1.21)
//!
//! DORM_PGBOUNCER_DSN='host=127.0.0.1 port=6432 user=walter dbname=ormtest' \
//! DORM_PGBOUNCER_ADMIN_DSN='host=127.0.0.1 port=6432 user=walter dbname=pgbouncer' \
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-core --test pgbouncer_live -- --ignored --nocapture
//! ```
//!
//! Under transaction pooling a client connection is multiplexed over many backend
//! connections, so a named prepared statement may be *missing* (`26000`) or
//! *collide* (`42P05`) across transactions. The authoritative fix is PgBouncer's
//! own `max_prepared_statements` (it tracks and remaps the names); DORM detects
//! the mode, and falls back to unprepared statements when told to.

use dorm_core::db;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

/// The detector reads `pool_mode` from the PgBouncer admin console, and a real
/// PostgreSQL reports no pooler. Touches no process-wide state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a PgBouncer admin console (DORM_PGBOUNCER_ADMIN_DSN)"]
async fn detects_pool_mode_from_the_admin_console_and_none_for_direct_postgres() {
    if let Some(admin) = var("DORM_PGBOUNCER_ADMIN_DSN") {
        let mode = db::detect_pgbouncer_pool_mode(&admin)
            .await
            .expect("admin query");
        assert_eq!(
            mode.as_deref(),
            Some("transaction"),
            "the admin console should report its pool_mode"
        );
    } else {
        eprintln!("skipped admin check: DORM_PGBOUNCER_ADMIN_DSN is not set");
        return;
    }

    if let Some(direct) = var("DELONIX_ORM_TEST_DSN") {
        let mode = db::detect_pgbouncer_pool_mode(&direct)
            .await
            .expect("direct query");
        assert_eq!(mode, None, "a real PostgreSQL is not a pooler");
    }
}

/// DORM operates through a transaction-mode PgBouncer: with the pooler's
/// `max_prepared_statements` on, the cached-prepared path works even under
/// concurrency; and `autoconfigure_from_pgbouncer` reports the mode and (absent an
/// explicit override) switches this process to unprepared, which also works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a transaction-mode PgBouncer (DORM_PGBOUNCER_DSN)"]
async fn operates_through_transaction_pooling() {
    let Some(dsn) = var("DORM_PGBOUNCER_DSN") else {
        eprintln!("skipped: DORM_PGBOUNCER_DSN is not set");
        return;
    };
    std::env::remove_var(db::NO_PREPARE_ENV);
    std::env::set_var("DELONIX_ORM_POOL_MAX", "8");
    std::env::set_var(db::DSN_ENV, &dsn);

    // (a) The cached-prepared path (DORM's default) under concurrency: 50 queries
    // fanned across 8 pooled connections, each multiplexed over PgBouncer's
    // backends. All succeed because the pooler forwards prepared statements
    // (`max_prepared_statements`). Without that this is where 26000/42P05 strike.
    let mut set = tokio::task::JoinSet::new();
    for i in 0..50i64 {
        set.spawn(async move {
            let rows = db::query("SELECT $1::bigint + 1", &[&i]).await?;
            Ok::<i64, dorm_core::Error>(rows[0].get::<_, i64>(0))
        });
    }
    let mut ok = 0;
    while let Some(j) = set.join_next().await {
        if let Ok(v) = j.unwrap() {
            ok += 1;
            assert!(v >= 1);
        }
    }
    assert_eq!(
        ok, 50,
        "every prepared query must succeed through a transaction pooler with \
         max_prepared_statements"
    );

    // (b) Proactive detection reports the mode and trips unprepared for this
    // process (the operator set no explicit DELONIX_ORM_NO_PREPARE).
    if let Some(admin) = var("DORM_PGBOUNCER_ADMIN_DSN") {
        let mode = db::autoconfigure_from_pgbouncer(&admin)
            .await
            .expect("autoconfigure");
        assert_eq!(mode.as_deref(), Some("transaction"));
    }

    // (c) Work still flows after the switch to unprepared.
    for i in 0..10i64 {
        let rows = db::query("SELECT $1::bigint", &[&i])
            .await
            .expect("unprepared query after autoconfigure");
        assert_eq!(rows[0].get::<_, i64>(0), i);
    }

    eprintln!(
        "ok: DORM operates through transaction pooling (prepared + autoconfigured unprepared)"
    );
}
