//! Live proof that `DELONIX_ORM_STATEMENT_TIMEOUT_MS` is enforced by the server
//! and surfaces as a named [`dorm_core::Error::Timeout`] (ADR 0004).
//!
//! Ignored by default — it needs a throwaway PostgreSQL. Run it with a DSN and
//! the timeout set:
//!
//! ```console
//! DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=dorm_dev' \
//!   cargo test -p dorm-core --test statement_timeout_live -- --ignored --nocapture
//! ```
//!
//! The unit tests in `db.rs`/`error.rs` already pin the pure halves — the option
//! string and the SQLSTATE→`Error` mapping. This closes the loop against a real
//! server: a statement that outruns the timeout is actually cancelled.

use dorm_core::db;
use dorm_core::Error;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a throwaway PostgreSQL; run with DELONIX_ORM_DSN set"]
async fn a_statement_past_the_timeout_is_cancelled_and_named_a_timeout() {
    // Set the ceiling low before the pool is ever built — it is read once, when
    // the process-global pool is first constructed.
    std::env::set_var(db::STATEMENT_TIMEOUT_MS_ENV, "200");

    // `pg_sleep(2)` runs for two seconds; the 200 ms ceiling must cut it short.
    let err = db::query("select pg_sleep(2)", &[])
        .await
        .expect_err("a 2s sleep under a 200ms statement_timeout must fail");

    assert!(
        matches!(err, Error::Timeout(_)),
        "expected a named Timeout, got: {err}"
    );
    assert!(
        err.to_string().contains("timed out"),
        "the message should read as a timeout: {err}"
    );
}
