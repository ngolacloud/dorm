//! Drive the built `dorm` REPL binary against a real PostgreSQL and check that
//! the DML verbs (ADR 0006) actually write — and that the read-only default
//! refuses a write.
//!
//! Ignored unless `DELONIX_ORM_TEST_DSN` is set, the same convention as
//! `dorm-models`'s live suites. It spawns the binary `cargo` built for this
//! test (`CARGO_BIN_EXE_dorm`) and pipes a script into its stdin, exactly as a
//! person would type it.
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql user=me dbname=dorm_dev' \
//!   cargo test -p dorm-shell --test dml_live -- --ignored --nocapture
//! ```

use std::io::Write;
use std::process::{Command, Stdio};

fn dsn() -> Option<String> {
    std::env::var("DELONIX_ORM_TEST_DSN")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Stand the dorm-models schema up once, so the shell has tables to write to.
fn ensure_schema(dsn: &str) {
    use dorm_core::db;
    std::env::set_var(db::DSN_ENV, dsn);
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
}

/// Run the `dorm` binary with `args`, feeding `script` on stdin, and return its
/// combined stdout.
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
    // Both streams matter: errors (the read-only refusal) go to stderr.
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn the_shell_inserts_updates_and_deletes_with_write_and_refuses_without_it() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    ensure_schema(&dsn);

    // A slug unique to this test, so it is safe to re-run and to run beside the
    // other suites.
    let slug = "shell-dml-probe";

    // Start clean — a previous failed run may have left the row behind.
    drive(
        &dsn,
        &["--write"],
        &format!("delete Tenant slug={slug} all\n:quit\n"),
    );

    // The full write cycle, read back at each step.
    let script = format!(
        "insert Tenant slug={slug} name=Original\n\
         Tenant slug={slug}\n\
         update Tenant slug={slug} set name=Renamed\n\
         Tenant slug={slug}\n\
         delete Tenant slug={slug}\n\
         Tenant slug={slug}\n\
         :quit\n"
    );
    let out = drive(&dsn, &["--write"], &script);

    assert!(
        out.contains("inserted 1 row"),
        "insert reports success:\n{out}"
    );
    assert!(
        out.contains("name=Original"),
        "the inserted row reads back:\n{out}"
    );
    assert!(
        out.contains("updated 1 row"),
        "update reports success:\n{out}"
    );
    assert!(
        out.contains("name=Renamed"),
        "the update is visible:\n{out}"
    );
    assert!(
        out.contains("deleted 1 row"),
        "delete reports success:\n{out}"
    );
    // The final read after the delete finds nothing.
    assert!(
        out.contains("(0 rows)"),
        "the row is gone after delete:\n{out}"
    );

    // Without --write, the same insert is refused by name, before any SQL.
    let refused = drive(
        &dsn,
        &[],
        &format!("insert Tenant slug={slug}-ro name=No\n:quit\n"),
    );
    assert!(
        refused.contains("read-only") && refused.contains("insert"),
        "a write without --write is refused by name:\n{refused}"
    );
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn sql_preview_shows_bound_parameters_without_running() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    // `:sql` needs no schema — it compiles, it does not run.
    let out = drive(&dsn, &[], ":sql Tenant slug=acme\n:quit\n");
    assert!(out.contains("SELECT"), "the preview shows the SQL:\n{out}");
    assert!(
        out.contains("$1"),
        "the value is a bound placeholder, not inlined:\n{out}"
    );
    assert!(
        out.contains("params:"),
        "the bound params are listed:\n{out}"
    );
}
