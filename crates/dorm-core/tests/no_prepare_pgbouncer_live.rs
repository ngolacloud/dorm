//! Live proof for B-04 / DBA-03 / H2: with `DELONIX_ORM_NO_PREPARE` set, the ORM
//! sends statements unprepared and runs cleanly behind **PgBouncer in
//! `transaction` pooling mode**, where the default `prepare_cached` path fails
//! with `26000 prepared statement "sN" does not exist` — a named statement lives
//! on a server backend that PgBouncer hands to a different client on the next
//! transaction.
//!
//! The regression (the prepared path failing with `26000`) was reproduced in the
//! review's Phase 2 (`docs/review/VERIFICATION.md`); this test proves the fix's
//! positive side: many transactions through PgBouncer, each forcing a backend
//! rotation, all succeed when unprepared.
//!
//! Gated on `DELONIX_ORM_PGBOUNCER_DSN` (a DSN pointing at a PgBouncer in
//! transaction mode). The PgBouncer must run with `max_prepared_statements = 0`
//! — the conservative setting where the pooler does **not** track prepared
//! statements, which is exactly the case DORM cannot negotiate and this flag is
//! for. (PgBouncer ≥ 1.23 defaults that to non-zero and then re-prepares across
//! backends transparently, hiding the problem; we target the setting that does
//! not.) This is the only test in its file, so it owns the process and the
//! one-time pool init reads these variables.
//!
//! Proven both ways against PgBouncer 1.26.1 (`pool_mode=transaction`,
//! `default_pool_size=2`, `max_prepared_statements=0`): without the flag the
//! prepared path fails with `26000 prepared statement "s1" does not exist`; with
//! it, the 128 concurrent transactions below all pass.
//!
//! ```bash
//! DELONIX_ORM_PGBOUNCER_DSN='host=127.0.0.1 port=6432 user=dorm password=dorm dbname=dorm_dev' \
//!   cargo test -p dorm-core --test no_prepare_pgbouncer_live -- --nocapture
//! ```

use dorm_core::db;

#[test]
fn unprepared_execution_survives_pgbouncer_transaction_mode() {
    let Some(dsn) = std::env::var("DELONIX_ORM_PGBOUNCER_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        eprintln!("skipped: set DELONIX_ORM_PGBOUNCER_DSN to run this");
        return;
    };

    // Set before the first pool use so the one-time init picks them up.
    std::env::set_var(db::NO_PREPARE_ENV, "1");
    std::env::set_var(db::DSN_ENV, &dsn);

    let rt = tokio::runtime::Runtime::new().expect("a tokio runtime");
    rt.block_on(async {
        // CONCURRENT transactions, so PgBouncer must spread them across several
        // server backends and then hand a pooled client a *different* backend on
        // its next transaction. With `prepare_cached` a named statement from the
        // old backend is then missing (`26000`); unprepared, every one is clean.
        // (A sequential loop would not rotate backends and so would not tell the
        // two modes apart — the rotation is the whole point.)
        let mut handles = Vec::new();
        for t in 0..16i64 {
            handles.push(tokio::spawn(async move {
                for r in 0..8i64 {
                    let v = t * 100 + r;
                    let got: i64 = db::atomic(|_tx| async move {
                        let rows = db::query("SELECT $1::int8 AS n", &[&v]).await?;
                        let n: i64 = rows[0].get(0);
                        Ok(n)
                    })
                    .await
                    .expect("a query through PgBouncer transaction mode succeeds unprepared");
                    assert_eq!(got, v, "the value round-trips");
                }
            }));
        }
        for h in handles {
            h.await.expect("the task joins");
        }
    });
}
