//! Online-migration follow-ups against a real PostgreSQL (ADR 0038).
//!
//! ```text
//! DELONIX_ORM_TEST_DSN='host=/var/run/postgresql port=5432 user=walter dbname=ormtest' \
//!   cargo test -p dorm-migrations --test online_live -- --ignored --nocapture
//! ```
//!
//! Three live proofs, each on a uniquely named `l2_*` table so the suite is
//! re-runnable:
//!
//! 1. **backfill** — a ~50k-row table filled in keyset batches (never one UPDATE),
//!    resumable from its checkpoint.
//! 2. **expand/contract** — a nullable column backfilled, then made effectively
//!    `NOT NULL` with `CHECK … NOT VALID` + `VALIDATE` on a populated table.
//! 3. **migrate_online** — `CREATE INDEX CONCURRENTLY` applied from the *versioned*
//!    applier (not `sync`), with a per-statement ledger that resumes a migration
//!    that failed part-way, and no `25001`.

use dorm_core::db;
use dorm_core::value::Value;
use dorm_migrations::ddl;
use dorm_migrations::{backfill, migrate_online, BackfillSpec, Migration};
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

async fn exec(sql: &str) {
    db::execute(sql, &[])
        .await
        .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
}

/// Cleanup that tolerates a missing table — the ledger/checkpoint tables may not
/// exist yet on a fresh database (the helpers under test create them).
async fn try_exec(sql: &str) {
    let _ = db::execute(sql, &[]).await;
}

async fn scalar_i64(sql: &str) -> i64 {
    db::fetch(sql, &[])
        .await
        .expect("query")
        .first()
        .expect("a row")
        .try_get::<i64>(0)
        .expect("an i64")
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn backfill_fills_fifty_thousand_rows_in_keyset_batches_and_resumes() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    runtime().block_on(async {
        exec("DROP TABLE IF EXISTS l2_backfill").await;
        try_exec("DELETE FROM orm_backfill_checkpoints WHERE job IN ('l2_fill', 'l2_resume')")
            .await;
        exec(
            "CREATE TABLE l2_backfill (id BIGINT PRIMARY KEY, n BIGINT NOT NULL, \
             doubled BIGINT)",
        )
        .await;
        // ~50k rows, `doubled` left NULL — the column to backfill.
        exec(
            "INSERT INTO l2_backfill (id, n) \
             SELECT g, g FROM generate_series(1, 50000) AS g",
        )
        .await;
        assert_eq!(scalar_i64("SELECT count(*) FROM l2_backfill").await, 50_000);

        // Backfill `doubled = n * 2` in batches of 5000 — ten batches, no single
        // table-wide UPDATE.
        let spec = BackfillSpec::new("l2_backfill", "id", "doubled = t.n * 2", 5_000);
        let report = backfill("l2_fill", &spec).await.expect("backfill");

        assert!(report.done, "the job finished");
        assert_eq!(report.rows, 50_000, "every row was updated");
        assert_eq!(
            report.batches, 10,
            "50000 / 5000 = 10 batches — not one UPDATE"
        );
        assert!(
            report.batches > 1,
            "proof it was batched, not a single statement"
        );
        assert_eq!(
            scalar_i64("SELECT count(*) FROM l2_backfill WHERE doubled = n * 2").await,
            50_000,
            "the value is correct on every row"
        );
        assert_eq!(
            scalar_i64("SELECT count(*) FROM l2_backfill WHERE doubled IS NULL").await,
            0,
            "nothing left unfilled"
        );

        // Re-running the same job is a no-op: the checkpoint says it is done.
        let again = backfill("l2_fill", &spec).await.expect("re-run");
        assert_eq!(again.batches, 0, "a completed job runs no batches");
        assert!(again.done);

        // Resumability: a job interrupted part-way continues from its checkpoint.
        // Fake an interrupted run by seeding the checkpoint at cursor 30000 with a
        // filter that only touches still-NULL rows, then fill a fresh column.
        exec("ALTER TABLE l2_backfill ADD COLUMN tripled BIGINT").await;
        exec("UPDATE l2_backfill SET tripled = n * 3 WHERE id <= 30000").await;
        // Seed a checkpoint as if 6 batches (30000 rows) had already committed.
        db::run(
            "INSERT INTO orm_backfill_checkpoints (job, cursor_pk, rows_done, done, updated_unix) \
             VALUES ('l2_resume', 30000, 30000, false, 0)",
            &[],
        )
        .await
        .expect("seed checkpoint");
        let resume_spec = BackfillSpec {
            filter_sql: Some("tripled IS NULL"),
            ..BackfillSpec::new("l2_backfill", "id", "tripled = t.n * 3", 5_000)
        };
        let resumed = backfill("l2_resume", &resume_spec).await.expect("resume");
        assert!(resumed.done);
        assert_eq!(
            resumed.last_cursor, 50_000,
            "it walked from the checkpoint to the end"
        );
        // Only the remaining 20000 rows were touched this run (cumulative 50000).
        assert_eq!(
            resumed.rows, 50_000,
            "cumulative count includes the seeded 30000"
        );
        assert_eq!(
            scalar_i64("SELECT count(*) FROM l2_backfill WHERE tripled = n * 3").await,
            50_000,
            "the resumed job finished the column"
        );

        exec("DROP TABLE IF EXISTS l2_backfill").await;
        println!("backfill: 50000 rows in 10 keyset batches + resume from checkpoint — OK");
    });
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn expand_contract_makes_a_populated_column_not_null_without_a_long_lock() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    runtime().block_on(async {
        exec("DROP TABLE IF EXISTS l2_expand").await;
        try_exec("DELETE FROM orm_backfill_checkpoints WHERE job = 'l2_expand_fill'").await;
        // A populated table with a nullable column to tighten.
        exec("CREATE TABLE l2_expand (id BIGINT PRIMARY KEY, status TEXT)").await;
        exec(
            "INSERT INTO l2_expand (id, status) \
             SELECT g, CASE WHEN g % 3 = 0 THEN NULL ELSE 'kept' END \
             FROM generate_series(1, 20000) AS g",
        )
        .await;
        let nulls = scalar_i64("SELECT count(*) FROM l2_expand WHERE status IS NULL").await;
        assert!(nulls > 0, "some rows are NULL before the backfill");

        // 1) backfill the NULLs in batches.
        let spec = BackfillSpec {
            filter_sql: Some("status IS NULL"),
            ..BackfillSpec::new("l2_expand", "id", "status = 'backfilled'", 4_000)
        };
        let report = backfill("l2_expand_fill", &spec).await.expect("backfill");
        assert!(report.done);
        assert_eq!(
            scalar_i64("SELECT count(*) FROM l2_expand WHERE status IS NULL").await,
            0,
            "no NULLs left"
        );

        // 2) add the NOT NULL guard NOT VALID (brief lock, no scan).
        exec(&ddl::add_not_null_check_not_valid("l2_expand", "status").unwrap()).await;
        // 3) validate it (scan under a lock that lets reads/writes through).
        exec(&ddl::validate_not_null("l2_expand", "status").unwrap()).await;

        // The constraint now rejects a NULL insert — the column is effectively NOT NULL.
        let rejected = db::run(
            "INSERT INTO l2_expand (id, status) VALUES ($1, NULL)",
            &[Value::I64(999_999)],
        )
        .await;
        assert!(
            rejected.is_err(),
            "the validated CHECK rejects a NULL, so the column is effectively NOT NULL"
        );

        // 4) and the validated check lets SET NOT NULL skip the scan (PG 12+).
        exec(&ddl::set_not_null("l2_expand", "status").unwrap()).await;
        exec(&ddl::drop_constraint("l2_expand", &ddl::not_null_constraint("l2_expand", "status")).unwrap())
            .await;
        let is_not_null = db::fetch(
            "SELECT attnotnull FROM pg_attribute \
             WHERE attrelid = 'l2_expand'::regclass AND attname = 'status'",
            &[],
        )
        .await
        .unwrap()
        .first()
        .unwrap()
        .try_get::<bool>(0)
        .unwrap();
        assert!(is_not_null, "the column is a genuine NOT NULL now");

        exec("DROP TABLE IF EXISTS l2_expand").await;
        println!("expand/contract: nullable -> backfill -> CHECK NOT VALID -> VALIDATE -> SET NOT NULL — OK");
    });
}

#[test]
#[ignore = "needs a Postgres (DELONIX_ORM_TEST_DSN)"]
fn migrate_online_applies_concurrently_and_resumes_a_partial_migration() {
    let Some(dsn) = dsn() else {
        eprintln!("skipped: DELONIX_ORM_TEST_DSN is not set");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    runtime().block_on(async {
        exec("DROP TABLE IF EXISTS l2_online").await;
        // Clear any ledger rows from a prior run of this version so it is re-runnable.
        try_exec("DELETE FROM orm_schema_migrations WHERE version = 990001").await;
        try_exec("DELETE FROM orm_migration_statements WHERE version = 990001").await;

        // First attempt: the migration's 3rd statement is broken on purpose, to
        // simulate a crash/failure part-way through.
        let broken = Migration {
            version: 990001,
            name: "l2_online_broken".into(),
            statements: vec![
                "CREATE TABLE IF NOT EXISTS l2_online (id BIGINT PRIMARY KEY, label TEXT)".into(),
                "INSERT INTO l2_online (id, label) VALUES (1, 'a'), (2, 'b')".into(),
                "THIS IS NOT VALID SQL".into(),
            ],
            down: vec![],
        };
        let err = migrate_online(std::slice::from_ref(&broken)).await;
        assert!(err.is_err(), "the broken statement fails the migration");

        // The per-statement ledger recorded the two good statements (ord 0, 1) but
        // not the broken one (ord 2); the version ledger has nothing.
        let stmts =
            scalar_i64("SELECT count(*) FROM orm_migration_statements WHERE version = 990001")
                .await;
        assert_eq!(
            stmts, 2,
            "only the two statements that succeeded are recorded"
        );
        let versions =
            scalar_i64("SELECT count(*) FROM orm_schema_migrations WHERE version = 990001").await;
        assert_eq!(versions, 0, "the version is not recorded while incomplete");
        assert_eq!(
            scalar_i64("SELECT count(*) FROM l2_online").await,
            2,
            "the rows the first two statements wrote are there"
        );

        // Fix the migration: same version, same first two statements, with the
        // broken one replaced by a CREATE INDEX CONCURRENTLY — a statement that
        // would raise 25001 inside a transaction. Re-running must SKIP ord 0/1 and
        // apply the index outside any transaction.
        let fixed = Migration {
            version: 990001,
            name: "l2_online_fixed".into(),
            statements: vec![
                "CREATE TABLE IF NOT EXISTS l2_online (id BIGINT PRIMARY KEY, label TEXT)".into(),
                "INSERT INTO l2_online (id, label) VALUES (1, 'a'), (2, 'b')".into(),
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS l2_online_label_idx \
                 ON l2_online (label)"
                    .into(),
            ],
            down: vec![],
        };
        let applied = migrate_online(std::slice::from_ref(&fixed))
            .await
            .expect("the resumed migration applies cleanly (no 25001)");
        assert_eq!(applied, vec![990001], "the version applied on the resume");

        // It did NOT re-run ord 0/1 (no duplicate rows), and the index exists.
        assert_eq!(
            scalar_i64("SELECT count(*) FROM l2_online").await,
            2,
            "the insert did not run a second time — ord 1 was skipped on resume"
        );
        let has_index = scalar_i64(
            "SELECT count(*) FROM pg_indexes \
             WHERE tablename = 'l2_online' AND indexname = 'l2_online_label_idx'",
        )
        .await;
        assert_eq!(
            has_index, 1,
            "CREATE INDEX CONCURRENTLY succeeded from migrate"
        );
        // The index is valid (a concurrent build that failed would leave it INVALID).
        let valid = scalar_i64(
            "SELECT count(*) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
             WHERE c.relname = 'l2_online_label_idx' AND i.indisvalid",
        )
        .await;
        assert_eq!(valid, 1, "the concurrent index is valid");
        assert_eq!(
            scalar_i64("SELECT count(*) FROM orm_schema_migrations WHERE version = 990001").await,
            1,
            "the version is now recorded"
        );

        // A third run is a no-op.
        let again = migrate_online(std::slice::from_ref(&fixed))
            .await
            .expect("re-run");
        assert!(again.is_empty(), "nothing to apply: {again:?}");

        exec("DROP TABLE IF EXISTS l2_online").await;
        println!(
            "migrate_online: CREATE INDEX CONCURRENTLY from the versioned applier + \
             per-statement resume, no 25001 — OK"
        );
    });
}
