//! `lock_timeout` on migrations against a real PostgreSQL (ADR 0022).
//!
//! Proves a migration that cannot get its lock **fails fast** instead of hanging
//! behind a long transaction: one task holds `ACCESS EXCLUSIVE` on a table, a
//! second runs a migration that needs the same lock with
//! `DELONIX_ORM_MIGRATE_LOCK_TIMEOUT_MS=100`, and the migration errors within the
//! timeout rather than blocking.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=127.0.0.1 port=5432 user=orm dbname=ormtest' \
//!   cargo test -p dorm-models --test migrate_lock_timeout_live -- --ignored --nocapture
//! ```

use dorm_core::prelude::*;
use dorm_migrations::{migrate, Migration};
use tokio::runtime::Runtime;

fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn runtime() -> &'static Runtime {
    static RT: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn a_blocked_migration_fails_fast_under_lock_timeout() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(dorm_core::db::DSN_ENV, &dsn);
    std::env::set_var("DELONIX_ORM_MIGRATE_LOCK_TIMEOUT_MS", "100");

    runtime().block_on(async {
        dorm_core::db::execute(
            "CREATE TABLE IF NOT EXISTS lock_probe (id BIGINT PRIMARY KEY)",
            &[],
        )
        .await
        .expect("probe table");
        // A clean column state so the ALTER below is a real lock request.
        dorm_core::db::execute("ALTER TABLE lock_probe DROP COLUMN IF EXISTS extra", &[])
            .await
            .expect("reset");

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        // Holder: take ACCESS EXCLUSIVE and keep the transaction open until told.
        let holder = tokio::spawn(async move {
            db::atomic(|_tx| async move {
                dorm_core::db::execute("LOCK TABLE lock_probe IN ACCESS EXCLUSIVE MODE", &[])
                    .await?;
                ready_tx.send(()).ok();
                // Hold the lock while the migration tries (and fails) to get it.
                done_rx.await.ok();
                Ok(())
            })
            .await
        });

        ready_rx.await.expect("the holder took the lock");

        // The migration wants ACCESS EXCLUSIVE on the same table; with a 100 ms
        // lock_timeout it must give up quickly instead of hanging.
        let started = std::time::Instant::now();
        let migration = Migration {
            version: 990_100,
            name: "lock_timeout_probe".into(),
            statements: vec!["ALTER TABLE lock_probe ADD COLUMN extra TEXT".into()],
            down: vec!["ALTER TABLE lock_probe DROP COLUMN extra".into()],
        };
        let result = migrate(std::slice::from_ref(&migration)).await;
        let elapsed = started.elapsed();

        // Release the holder.
        done_tx.send(()).ok();
        let _ = holder.await;

        let err = result.expect_err("the migration must fail, not block");
        let msg = err.to_string();
        // PostgreSQL raises SQLSTATE 55P03 (lock_not_available) on lock_timeout.
        assert!(
            msg.contains("55P03") || msg.to_lowercase().contains("lock timeout"),
            "expected a lock-timeout error, got: {msg}"
        );
        // Fast: well under a second, not a hang. (Generous bound for CI noise.)
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the migration should have failed fast, took {elapsed:?}"
        );

        // The failed migration left nothing applied: its whole transaction rolled
        // back, so the column the `ALTER` would have added is absent.
        let has_col = dorm_core::db::sql(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_name = 'lock_probe' AND column_name = 'extra'",
            &[],
        )
        .await
        .expect("column check");
        assert!(
            has_col.is_empty(),
            "the blocked ALTER must have rolled back, leaving no `extra` column"
        );
    });
}
