//! Drive the built `dorm` binary's `db execute` (ADR 0005 raw-SQL valve) against a
//! real PostgreSQL: a write with `--write`, a parameterised read, the refusal of a
//! write without `--write`, and that the refusal did not touch the data.
//!
//! Gated on `DELONIX_ORM_DSN` — the test skips (green) when it is unset, so a plain
//! `cargo test -p dorm-cli` passes with or without a database. The DSN must name a
//! host explicitly: tokio-postgres rejects `postgresql:///db` ("both host and
//! hostaddr are missing"), so use the libpq keyword form.
//!
//! ```text
//! DELONIX_ORM_DSN='host=/var/run/postgresql user=me dbname=dorm_cli' \
//!   cargo test -p dorm-cli --test db_execute_live -- --nocapture
//! ```
//!
//! Every statement runs on a table named for this test, created and dropped here,
//! so the suite is safe to re-run and to run beside any other.

use std::process::Command;

/// The configured DSN, or `None` when the suite should skip.
fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// The table this test owns. Distinct from anything a migration or another suite
/// creates, so creating and dropping it is self-contained.
const TABLE: &str = "dorm_cli_execute_probe";

/// Run `dorm <args>` with the DSN in the environment, returning stdout, stderr and
/// whether it exited 0. The binary is the one `cargo` built for this test.
fn run(dsn: &str, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_dorm"))
        .args(args)
        .env("DELONIX_ORM_DSN", dsn)
        .output()
        .expect("the dorm binary runs");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn execute_writes_with_write_reads_with_params_and_refuses_a_write_without_write() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_DSN is not set");
        return;
    };

    // Start clean — a previous failed run may have left the table behind.
    let drop = format!("DROP TABLE IF EXISTS {TABLE}");
    run(&dsn, &["db", "execute", "--write", &drop]);

    // Create, via the write path (DDL needs --write).
    let create = format!("CREATE TABLE {TABLE} (id INT PRIMARY KEY, name TEXT, active BOOL)");
    let (out, err, ok) = run(&dsn, &["db", "execute", "--write", &create]);
    assert!(ok, "create succeeds (out={out} err={err})");

    // Insert two rows. Each value is a bound `$n`, never spliced in. A text value
    // goes straight into a TEXT column; a non-text target is reached by routing the
    // text parameter through text (`$1::text::int`, `$3::text::bool`) — the honest
    // idiom the command's help documents.
    let insert = format!(
        "INSERT INTO {TABLE} (id, name, active) \
         VALUES ($1::text::int, $2, $3::text::bool), ($4::text::int, $5, $6::text::bool)"
    );
    let (out, err, ok) = run(
        &dsn,
        &[
            "db", "execute", "--write", &insert, //
            "-p", "1", "-p", "Alice", "-p", "true", //
            "-p", "2", "-p", "Bob", "-p", "false",
        ],
    );
    assert!(ok, "insert succeeds (err={err})");
    assert!(
        out.contains("2 rows affected"),
        "insert reports the count:\n{out}"
    );

    // Read one row back with a bound parameter. The column is cast to text for the
    // comparison, so the text parameter matches — the other read-side idiom.
    let select = format!("SELECT id, name, active FROM {TABLE} WHERE id::text = $1");
    let (out, err, ok) = run(&dsn, &["db", "execute", &select, "-p", "1"]);
    assert!(ok, "select succeeds (err={err})");
    assert!(out.contains("Alice"), "the row reads back:\n{out}");
    assert!(out.contains("(1 row)"), "exactly one row:\n{out}");
    // `active` renders as a real boolean, decoded by type — not a text artefact.
    assert!(out.contains("true"), "the bool column renders:\n{out}");

    // The same read as a stable JSON object, keyed by column name.
    let count = format!("SELECT count(*)::int AS n FROM {TABLE}");
    let (out, err, ok) = run(&dsn, &["--json", "db", "execute", &count]);
    assert!(ok, "json read succeeds (err={err})");
    assert!(
        out.contains("\"n\":\"2\""),
        "the JSON object carries the count under its column name:\n{out}"
    );

    // A write WITHOUT --write is refused up front, by name, before any SQL runs.
    let delete = format!("DELETE FROM {TABLE}");
    let (_out, err, ok) = run(&dsn, &["db", "execute", &delete]);
    assert!(!ok, "a write without --write fails");
    assert!(
        err.contains("--write") && err.contains("DELETE"),
        "the refusal names the keyword and --write:\n{err}"
    );

    // …and the refusal really did not run: both rows are still there.
    let (out, _err, _ok) = run(&dsn, &["db", "execute", &count]);
    assert!(
        out.contains('2'),
        "the refused DELETE left the data untouched:\n{out}"
    );

    // Cleanup.
    run(&dsn, &["db", "execute", "--write", &drop]);
}
