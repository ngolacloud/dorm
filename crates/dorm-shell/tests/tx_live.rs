//! Drive the built `dorm` REPL binary against a real PostgreSQL and prove the
//! interactive transaction verbs (ADR 0006, brick 2): `:begin` opens a
//! transaction that spans several REPL lines, `:commit` persists them all
//! atomically, and `:rollback` — or an error mid-transaction — leaves nothing
//! written.
//!
//! This is the part [`dorm_core::db::atomic`] cannot cover: each REPL line is a
//! separate `block_on`, so the transaction has to be held in the shell and
//! re-entered per line, which is exactly what this exercises end to end by
//! piping a multi-line script into the binary's stdin.
//!
//! Ignored unless `DELONIX_ORM_TEST_DSN` is set, the same convention as the DML
//! suite. Run it on its own — several live suites rewrite the `public` schema
//! and would collide under `--workspace`:
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql user=me dbname=dorm_dev' \
//!   cargo test -p dorm-shell --test tx_live -- --ignored --nocapture
//! ```

use std::io::Write;
use std::process::{Command, Stdio};

fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Stand the dorm-models schema up once, so the shell has tables to write to.
///
/// Guarded by a [`Once`]: the three tests in this binary run in parallel, and
/// PostgreSQL's `CREATE TABLE IF NOT EXISTS` is not race-safe — two concurrent
/// creators both pass the existence check and the loser collides on a system
/// catalog index. Running the DDL exactly once sidesteps that; `IF NOT EXISTS`
/// then keeps a re-run against the same database clean.
fn ensure_schema(dsn: &str) {
    use std::sync::Once;
    static SCHEMA: Once = Once::new();
    use dorm_core::db;
    std::env::set_var(db::DSN_ENV, dsn);
    SCHEMA.call_once(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(async {
            for statement in dorm_models::schema_sql().split(";\n") {
                let s = statement.trim();
                if !s.is_empty() {
                    db::run_ddl(s).await.expect("the schema statement runs");
                }
            }
        });
    });
}

/// Run the `dorm` binary with `args`, feeding `script` on stdin, and return its
/// combined stdout and stderr.
fn drive(dsn: &str, args: &[&str], script: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dorm"))
        .args(args)
        .env("DELONIX_ORM_DSN", dsn)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the dorm binary spawns");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .expect("the script is written");
    let out = child.wait_with_output().expect("the shell exits");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// How many rows a read of `slug__startswith=<prefix>` prints — read off the
/// count line the shell prints after a query (`(N rows)` or `(0 rows)`).
fn probe_count(dsn: &str, prefix: &str) -> String {
    drive(
        dsn,
        &[],
        &format!("Tenant slug__startswith={prefix}\n:quit\n"),
    )
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn commit_persists_a_multi_statement_transaction_atomically() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    ensure_schema(&dsn);
    let prefix = "tx-commit";

    // Clean slate — a previous failed run may have left rows behind.
    drive(
        &dsn,
        &["--write"],
        &format!("delete Tenant slug__startswith={prefix} all\n:quit\n"),
    );

    // Two inserts inside one transaction, read back *before* the commit (the
    // session sees its own uncommitted work), then committed.
    let script = format!(
        ":begin\n\
         insert Tenant slug={prefix}-a name=Alpha\n\
         insert Tenant slug={prefix}-b name=Beta\n\
         Tenant slug__startswith={prefix}\n\
         :commit\n\
         :quit\n"
    );
    let out = drive(&dsn, &["--write"], &script);

    assert!(out.contains("BEGIN"), "the transaction opened:\n{out}");
    assert!(
        out.contains("name=Alpha") && out.contains("name=Beta"),
        "both rows are visible inside the transaction:\n{out}"
    );
    assert!(out.contains("COMMIT"), "the transaction committed:\n{out}");

    // After the commit, a *fresh* session finds both rows — they were persisted.
    let after = probe_count(&dsn, prefix);
    assert!(
        after.contains("name=Alpha") && after.contains("name=Beta") && after.contains("(2 rows)"),
        "both rows survive the commit in a new session:\n{after}"
    );

    // Tidy up.
    drive(
        &dsn,
        &["--write"],
        &format!("delete Tenant slug__startswith={prefix} all\n:quit\n"),
    );
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn rollback_discards_every_statement_in_the_transaction() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    ensure_schema(&dsn);
    let prefix = "tx-rollback";

    drive(
        &dsn,
        &["--write"],
        &format!("delete Tenant slug__startswith={prefix} all\n:quit\n"),
    );

    // Seed one committed row, so there is something a rolled-back delete must
    // *not* remove and a rolled-back insert must sit beside.
    drive(
        &dsn,
        &["--write"],
        &format!("insert Tenant slug={prefix}-keep name=Keep\n:quit\n"),
    );

    // Inside a transaction: delete the seeded row and insert a new one, read
    // back the intermediate state (empty of the deleted, full of the inserted),
    // then roll it all back.
    let script = format!(
        ":begin\n\
         delete Tenant slug={prefix}-keep\n\
         insert Tenant slug={prefix}-new name=New\n\
         Tenant slug__startswith={prefix}\n\
         :rollback\n\
         :quit\n"
    );
    let out = drive(&dsn, &["--write"], &script);

    assert!(out.contains("BEGIN"), "the transaction opened:\n{out}");
    assert!(
        out.contains("name=New"),
        "the uncommitted insert is visible inside the transaction:\n{out}"
    );
    assert!(
        out.contains("ROLLBACK"),
        "the transaction rolled back:\n{out}"
    );

    // After the rollback: the seeded row is back (its delete was undone) and the
    // inserted row is gone (its insert was undone). Exactly one row, the seed.
    let after = probe_count(&dsn, prefix);
    assert!(
        after.contains("name=Keep") && after.contains("(1 row)"),
        "the seed row survives and the insert is gone after rollback:\n{after}"
    );
    assert!(
        !after.contains("name=New"),
        "the rolled-back insert left nothing written:\n{after}"
    );

    drive(
        &dsn,
        &["--write"],
        &format!("delete Tenant slug__startswith={prefix} all\n:quit\n"),
    );
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn an_error_mid_transaction_then_rollback_leaves_nothing_written() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    ensure_schema(&dsn);
    let prefix = "tx-error";

    drive(
        &dsn,
        &["--write"],
        &format!("delete Tenant slug__startswith={prefix} all\n:quit\n"),
    );

    // A good insert, then a second insert of the *same* unique slug — a
    // constraint violation that PostgreSQL rejects, aborting the transaction at
    // the server. The following `:rollback` then discards the whole thing,
    // including the first insert.
    let script = format!(
        ":begin\n\
         insert Tenant slug={prefix}-x name=First\n\
         insert Tenant slug={prefix}-x name=Dup\n\
         :rollback\n\
         :quit\n"
    );
    let out = drive(&dsn, &["--write"], &script);

    assert!(out.contains("BEGIN"), "the transaction opened:\n{out}");
    assert!(
        out.contains("inserted 1 row"),
        "the first insert ran inside the transaction:\n{out}"
    );
    assert!(
        out.contains("error:"),
        "the duplicate insert is reported as an error:\n{out}"
    );
    assert!(
        out.contains("ROLLBACK"),
        "the transaction rolled back:\n{out}"
    );

    // The first, successful insert left nothing behind: it was undone with the
    // aborted transaction.
    let after = probe_count(&dsn, prefix);
    assert!(
        after.contains("(0 rows)"),
        "nothing from the aborted transaction was written:\n{after}"
    );
}
