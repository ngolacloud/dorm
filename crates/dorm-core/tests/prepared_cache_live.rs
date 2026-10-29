//! Live proof that `db::query` caches its prepared statement per connection
//! (ADR 0007), the companion to the `prepared_cache` benchmark's timing.
//!
//! The tell is `pg_prepared_statements`: it lists a connection's **named**
//! prepared statements and says nothing of unnamed ones. The old code path,
//! `client.query(sql, …)`, parsed an unnamed statement on every call — so the
//! same SQL run five times would leave *zero* rows there. `prepare_cached` names
//! the statement and reuses it, so the same SQL leaves exactly *one*, however
//! many times it ran. One row is the assertion.
//!
//! Pinning the pool to a single connection (`DELONIX_ORM_POOL_MAX=1`) is what
//! makes the count observable: the probe queries and the `pg_prepared_statements`
//! read all land on the same backend, so the catalog reflects what the probes
//! prepared. This is the only test in its file, so it owns the process and the
//! one-time pool initialisation reads these variables.
//!
//! Gated on `DELONIX_ORM_DSN`, like the other live suites:
//!
//! ```bash
//! DELONIX_ORM_DSN=postgresql:///dorm_bench \
//!   cargo test -p dorm-core --test prepared_cache_live -- --nocapture
//! ```

use dorm_core::db;

#[test]
fn query_caches_one_named_statement_per_connection() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_DSN").or_else(|_| std::env::var("PAAS_DATABASE_URL"))
    else {
        eprintln!("skipped: set DELONIX_ORM_DSN to a throwaway Postgres to run this");
        return;
    };
    // Pin to one connection so every query below shares a backend session; set
    // before the first `db::*` call, which is when the pool reads the env.
    std::env::set_var(db::POOL_MAX_ENV, "1");
    std::env::set_var(db::DSN_ENV, &dsn);

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        db::run_ddl("DROP TABLE IF EXISTS cache_probe; CREATE TABLE cache_probe (id int)")
            .await
            .expect("create probe table");

        // The same SQL, five times. Under the old unnamed-statement path this
        // leaves nothing in pg_prepared_statements; under the cache, one entry.
        const PROBE: &str = "SELECT id FROM cache_probe WHERE id = $1";
        for _ in 0..5 {
            db::query(PROBE, &[&1i32]).await.expect("probe query");
        }

        // Count only the probe's statement by its leading text — the counting
        // query itself does not start with that prefix, so it never counts
        // itself even though its own body mentions the table name.
        let rows = db::query(
            "SELECT count(*) FROM pg_prepared_statements \
             WHERE statement LIKE 'SELECT id FROM cache_probe%'",
            &[],
        )
        .await
        .expect("inspect pg_prepared_statements");
        let cached: i64 = rows[0].get(0);
        assert_eq!(
            cached, 1,
            "the repeated query must be prepared once and cached, not re-parsed per call"
        );

        db::run_ddl("DROP TABLE IF EXISTS cache_probe")
            .await
            .expect("drop probe table");
    });
}
