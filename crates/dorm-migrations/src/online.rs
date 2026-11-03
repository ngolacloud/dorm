//! Online-migration follow-ups (ADR 0038): the pieces that let a populated table
//! evolve under a rolling upgrade — where version N and N-1 of the application run
//! at once over the same schema — without a stop-the-world lock.
//!
//! This builds on ADR 0022's first bricks (`lock_timeout`, `migrate lint`) and on
//! the machinery `sync` already proved: `CREATE INDEX CONCURRENTLY` applied on its
//! own connection *outside* `db::atomic`. Three capabilities live here:
//!
//! * [`backfill`] — fill a column on a large table in keyset batches, resumable
//!   from a persisted checkpoint, with optional throttling, so there is never a
//!   single table-wide `UPDATE`.
//! * [`migrate_online`] — the versioned applier that recognises statements
//!   PostgreSQL forbids inside a transaction (anything `CONCURRENTLY`,
//!   `ALTER TYPE … ADD VALUE`, `VACUUM`) and runs each one on its own connection
//!   outside `db::atomic`, recording the ledger **per statement** so a crash
//!   mid-migration resumes rather than restarts.
//! * [`check_compat`] — prove a new schema still serves the N-1 code: no table or
//!   column it depends on is removed, and no column it writes is tightened to
//!   `NOT NULL` (without a default) or retyped.
//!
//! The expand/contract DDL builders (`CHECK … NOT VALID` + `VALIDATE`) live in
//! [`crate::ddl`] next to the rest of the DDL; this module is the *applier* side.
//!
//! PostgreSQL is the backend these target: the online primitives —
//! `CONCURRENTLY`, `NOT VALID`/`VALIDATE`, keyset `UPDATE … FROM … RETURNING`,
//! `ON CONFLICT` — are PostgreSQL's. [`migrate_online`] still runs on SQLite (it
//! simply finds no non-transactional statements there); [`backfill`] is
//! PostgreSQL-only and says so.

use crate::autodetect::{Column, Snapshot, Table};
use crate::migrator::LEDGER;
use crate::Result;
use dorm_core::db;
use dorm_core::value::Value;
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

/// Reject anything that is not a plain SQL identifier before it is spliced into
/// SQL — table and column names are configuration, never bound parameters (I1).
fn ident(name: &str) -> Result<&str> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(name)
    } else {
        Err(crate::Error::Orm(dorm_core::Error::Config(format!(
            "`{name}` is not a usable SQL identifier"
        ))))
    }
}

/// Seconds since the epoch — wall-clock metadata for a ledger/checkpoint row, not
/// a value a query reasons about (the same choice `migrator::now_unix` makes).
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ======================================================================= backfill

/// A keyset-batched backfill of one column (ADR 0038).
///
/// The shape is the one a large-table backfill wants: walk the table in primary-key
/// order, updating a bounded window of rows at a time
/// (`WHERE pk > $cursor ORDER BY pk LIMIT $batch`), committing each window on its
/// own so progress survives a crash. It reuses ADR 0024's stable PK order.
///
/// `set_sql` and `filter_sql` are the author's SQL expressions (the trust
/// `ModelMeta::checks` and ADR 0005 raw SQL already carry); `table` and `pk_col`
/// are validated as identifiers. Values the applier itself supplies — the cursor
/// and the batch size — bind as `$1`/`$2` (I2); nothing a caller spells reaches
/// the text unchecked.
#[derive(Debug, Clone)]
pub struct BackfillSpec<'a> {
    /// The table to backfill.
    pub table: &'a str,
    /// The primary-key column to page by. Must be an `i64`/`BIGINT` key — the
    /// keyset cursor is an `i64`.
    pub pk_col: &'a str,
    /// The `SET` clause body, e.g. `"status = 'active'"` or `"label = upper(name)"`.
    /// Author SQL, spliced after `SET`.
    pub set_sql: &'a str,
    /// An optional extra predicate narrowing which rows a batch touches, e.g.
    /// `"status IS NULL"`. When set, only matching rows are updated and the cursor
    /// still advances by the window's largest key, so already-done rows are skipped.
    pub filter_sql: Option<&'a str>,
    /// Rows per batch. Clamped to at least 1.
    pub batch: i64,
    /// Where the keyset starts — rows with `pk <= start_after` are left alone. Use
    /// `i64::MIN` to cover every row (a fresh job also defaults here). A persisted
    /// checkpoint overrides this on resume.
    pub start_after: i64,
    /// An optional pause between batches, to cap the write rate (throttling).
    pub throttle: Option<Duration>,
}

impl BackfillSpec<'_> {
    /// A spec that updates every row from the start, no filter, no throttle.
    pub fn new<'a>(
        table: &'a str,
        pk_col: &'a str,
        set_sql: &'a str,
        batch: i64,
    ) -> BackfillSpec<'a> {
        BackfillSpec {
            table,
            pk_col,
            set_sql,
            filter_sql: None,
            batch,
            start_after: i64::MIN,
            throttle: None,
        }
    }
}

/// What a [`backfill`] run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    /// How many batches this call ran (0 when the job was already complete).
    pub batches: u64,
    /// Rows updated across the whole job so far (cumulative, survives resume).
    pub rows: i64,
    /// The keyset cursor the job has reached.
    pub last_cursor: i64,
    /// Whether the job has finished — the last batch found no more rows.
    pub done: bool,
}

/// The checkpoint table — one row per named backfill job, so a crash resumes from
/// the last committed cursor instead of starting over.
const BACKFILL_CHECKPOINTS: &str = "orm_backfill_checkpoints";

/// The per-batch keyset `UPDATE … RETURNING` for a spec. Pure, so it can be tested
/// by comparing text. `$1` is the cursor, `$2` the batch size.
fn batch_update_sql(spec: &BackfillSpec) -> Result<String> {
    let table = ident(spec.table)?;
    let pk = ident(spec.pk_col)?;
    let filter = match spec.filter_sql {
        Some(f) if !f.trim().is_empty() => format!(" AND ({})", f.trim()),
        _ => String::new(),
    };
    Ok(format!(
        "WITH batch AS (\
            SELECT {pk} AS k FROM {table} WHERE {pk} > $1{filter} ORDER BY {pk} LIMIT $2\
         ) \
         UPDATE {table} AS t SET {set} FROM batch WHERE t.{pk} = batch.k \
         RETURNING t.{pk}",
        set = spec.set_sql
    ))
}

/// Fill a column in keyset batches, resumable, throttled (ADR 0038). PostgreSQL only.
///
/// `job` names the backfill so its progress is checkpointed in
/// `orm_backfill_checkpoints`; re-running the same `job` resumes from the last
/// committed cursor, and a completed job is a no-op. Each batch runs in its own
/// transaction together with its checkpoint write, so the two commit atomically —
/// the cursor on disk is never ahead of the rows actually updated.
pub async fn backfill(job: &str, spec: &BackfillSpec<'_>) -> Result<BackfillReport> {
    db::guard_write("backfill")?;
    let sql = batch_update_sql(spec)?; // also validates the identifiers
    let batch = spec.batch.max(1);
    let job = job.to_string();

    ensure_checkpoint_table().await?;
    let (mut cursor, mut rows_done, done) =
        load_checkpoint(&job)
            .await?
            .unwrap_or((spec.start_after, 0, false));
    if done {
        return Ok(BackfillReport {
            batches: 0,
            rows: rows_done,
            last_cursor: cursor,
            done: true,
        });
    }

    let upsert = format!(
        "INSERT INTO {BACKFILL_CHECKPOINTS} (job, cursor_pk, rows_done, done, updated_unix) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (job) DO UPDATE SET \
             cursor_pk = EXCLUDED.cursor_pk, rows_done = EXCLUDED.rows_done, \
             done = EXCLUDED.done, updated_unix = EXCLUDED.updated_unix"
    );

    let mut batches = 0u64;
    loop {
        let sql = sql.clone();
        let upsert = upsert.clone();
        let job = job.clone();
        let (n, next_cursor, done_now): (i64, i64, bool) = db::atomic(|_tx| async move {
            let rows = db::fetch(&sql, &[Value::I64(cursor), Value::I64(batch)]).await?;
            let n = rows.len() as i64;
            let mut max_pk = cursor;
            for r in &rows {
                let pk = r.try_get::<i64>(0)?;
                if pk > max_pk {
                    max_pk = pk;
                }
            }
            let done_now = n == 0;
            let next_cursor = if done_now { cursor } else { max_pk };
            let total = rows_done + n;
            db::run(
                &upsert,
                &[
                    Value::Text(job),
                    Value::I64(next_cursor),
                    Value::I64(total),
                    Value::Bool(done_now),
                    Value::I64(now_unix()),
                ],
            )
            .await?;
            Ok::<(i64, i64, bool), dorm_core::Error>((n, next_cursor, done_now))
        })
        .await?;

        cursor = next_cursor;
        rows_done += n;
        if done_now {
            break;
        }
        batches += 1;
        if let Some(pause) = spec.throttle {
            tokio::time::sleep(pause).await;
        }
    }

    Ok(BackfillReport {
        batches,
        rows: rows_done,
        last_cursor: cursor,
        done: true,
    })
}

async fn ensure_checkpoint_table() -> Result<()> {
    db::run(
        &format!(
            "CREATE TABLE IF NOT EXISTS {BACKFILL_CHECKPOINTS} (\
                job TEXT PRIMARY KEY, \
                cursor_pk BIGINT NOT NULL, \
                rows_done BIGINT NOT NULL, \
                done BOOLEAN NOT NULL DEFAULT FALSE, \
                updated_unix BIGINT NOT NULL)"
        ),
        &[],
    )
    .await?;
    Ok(())
}

/// The checkpoint for `job` as `(cursor, rows_done, done)`, or `None` if it has
/// never run.
async fn load_checkpoint(job: &str) -> Result<Option<(i64, i64, bool)>> {
    let rows = db::fetch(
        &format!("SELECT cursor_pk, rows_done, done FROM {BACKFILL_CHECKPOINTS} WHERE job = $1"),
        &[Value::Text(job.to_string())],
    )
    .await?;
    match rows.first() {
        None => Ok(None),
        Some(r) => Ok(Some((
            r.try_get::<i64>(0)?,
            r.try_get::<i64>(1)?,
            r.try_get::<bool>(2)?,
        ))),
    }
}

// ================================================================= online applier

/// Whether a statement is one PostgreSQL forbids inside a transaction block, so it
/// must run on its own connection outside `db::atomic` (error `25001` otherwise).
///
/// Covers everything `CONCURRENTLY` (`CREATE`/`DROP INDEX CONCURRENTLY`,
/// `REINDEX … CONCURRENTLY`, `DETACH PARTITION CONCURRENTLY`), `ALTER TYPE … ADD
/// VALUE`, and `VACUUM`. Pure and case-insensitive.
pub fn is_non_transactional(sql: &str) -> bool {
    let up = sql.to_ascii_uppercase();
    let trimmed = up.trim_start();
    up.contains("CONCURRENTLY")
        || (up.contains("ALTER TYPE") && up.contains("ADD VALUE"))
        || trimmed.starts_with("VACUUM")
}

/// The per-statement ledger: one row per (version, ordinal) applied, so an online
/// migration that failed part-way is resumed, not restarted. Separate from the
/// version ledger (`orm_schema_migrations`) because its grain is the statement, not
/// the version.
const STATEMENT_LEDGER: &str = "orm_migration_statements";

/// Apply migrations like [`crate::migrate`], but aware of statements that cannot
/// run inside a transaction (ADR 0038).
///
/// For each not-yet-recorded version, statements are applied **in order**:
/// a [non-transactional](is_non_transactional) one runs on its own pooled
/// connection in autocommit (what `CREATE INDEX CONCURRENTLY` needs); every other
/// one runs inside a one-statement `db::atomic`. Either way the statement's
/// completion is written to the per-statement ledger `orm_migration_statements`
/// (keyed by `(version, ord)`), so a re-run after a crash skips the statements
/// already done and continues. Once every statement of a version is in the
/// statement ledger, the version is recorded in the ordinary `orm_schema_migrations`
/// ledger and reported.
///
/// Idempotent: an already-recorded version is skipped, an already-recorded
/// statement is skipped, and the `CONCURRENTLY`/`IF NOT EXISTS` forms tolerate a
/// re-applied statement.
///
/// # Minimal path (honest scope)
///
/// Because its statements run on independent connections, this applier does **not**
/// hold a cross-statement advisory lock — it assumes the ordinary single-runner
/// deploy (one migration Job/pod at a time), and leans on the per-statement ledger
/// plus `IF NOT EXISTS` for safety. Cleaning up an `INVALID` index left by a failed
/// `CREATE INDEX CONCURRENTLY` is **not** done here (ADR 0038, deferred). For a
/// migration with no non-transactional statements, [`crate::migrate`] — which wraps
/// the whole chain in one transaction under an advisory lock — remains the default.
pub async fn migrate_online(migrations: &[crate::Migration]) -> Result<Vec<i64>> {
    db::guard_write("migrate_online")?;
    let is_postgres = db::backend_of(&db::dsn()?) == db::Backend::Postgres;
    ensure_online_ledgers().await?;

    let done_versions = fetch_done_versions().await?;
    let done_stmts = fetch_done_statements().await?;

    // `$n` on PostgreSQL, `?n` on SQLite — the one placeholder difference.
    let stmt_insert = if is_postgres {
        format!(
            "INSERT INTO {STATEMENT_LEDGER} (version, ord, applied_unix) VALUES ($1, $2, $3) \
             ON CONFLICT (version, ord) DO NOTHING"
        )
    } else {
        format!(
            "INSERT OR IGNORE INTO {STATEMENT_LEDGER} (version, ord, applied_unix) \
             VALUES (?1, ?2, ?3)"
        )
    };
    let version_insert = if is_postgres {
        format!(
            "INSERT INTO {LEDGER} (version, name, applied_unix) VALUES ($1, $2, $3) \
             ON CONFLICT (version) DO NOTHING"
        )
    } else {
        format!("INSERT OR IGNORE INTO {LEDGER} (version, name, applied_unix) VALUES (?1, ?2, ?3)")
    };

    let mut applied = Vec::new();
    for m in migrations {
        if done_versions.contains(&m.version) {
            continue;
        }
        for (i, stmt) in m.statements.iter().enumerate() {
            let ord = i as i64;
            if done_stmts.contains(&(m.version, ord)) {
                continue;
            }
            let record = [
                Value::I64(m.version),
                Value::I64(ord),
                Value::I64(now_unix()),
            ];
            if is_postgres && is_non_transactional(stmt) {
                // Autocommit, outside any transaction: the only shape that lets
                // `CONCURRENTLY` run. Statement first, then its ledger row — a crash
                // between the two re-applies a statement that is idempotent anyway.
                db::run(stmt, &[]).await?;
                db::run(&stmt_insert, &record).await?;
            } else {
                // One statement, its ledger row, one transaction: both or neither.
                let stmt = stmt.clone();
                let stmt_insert = stmt_insert.clone();
                db::atomic(|_tx| async move {
                    db::run(&stmt, &[]).await?;
                    db::run(&stmt_insert, &record).await?;
                    Ok::<(), dorm_core::Error>(())
                })
                .await?;
            }
        }
        db::run(
            &version_insert,
            &[
                Value::I64(m.version),
                Value::Text(m.name.clone()),
                Value::I64(now_unix()),
            ],
        )
        .await?;
        applied.push(m.version);
    }
    Ok(applied)
}

async fn ensure_online_ledgers() -> Result<()> {
    db::run(
        &format!(
            "CREATE TABLE IF NOT EXISTS {LEDGER} (\
             version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_unix BIGINT NOT NULL)"
        ),
        &[],
    )
    .await?;
    db::run(
        &format!(
            "CREATE TABLE IF NOT EXISTS {STATEMENT_LEDGER} (\
                version BIGINT NOT NULL, ord BIGINT NOT NULL, applied_unix BIGINT NOT NULL, \
                PRIMARY KEY (version, ord))"
        ),
        &[],
    )
    .await?;
    Ok(())
}

async fn fetch_done_versions() -> Result<HashSet<i64>> {
    Ok(db::fetch(&format!("SELECT version FROM {LEDGER}"), &[])
        .await?
        .iter()
        .map(|r| r.try_get::<i64>(0))
        .collect::<dorm_core::Result<_>>()?)
}

async fn fetch_done_statements() -> Result<HashSet<(i64, i64)>> {
    let mut set = HashSet::new();
    for r in db::fetch(&format!("SELECT version, ord FROM {STATEMENT_LEDGER}"), &[]).await? {
        set.insert((r.try_get::<i64>(0)?, r.try_get::<i64>(1)?));
    }
    Ok(set)
}

// ================================================================= check-compat

/// A way the new schema fails to serve the N-1 code (ADR 0038 / [`check_compat`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incompatibility {
    /// The table the problem is on.
    pub table: String,
    /// The column, or `None` when it is the whole table.
    pub column: Option<String>,
    /// What N-1 depended on that the new schema broke.
    pub reason: String,
}

impl std::fmt::Display for Incompatibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.column {
            Some(c) => write!(f, "{}.{}: {}", self.table, c, self.reason),
            None => write!(f, "{}: {}", self.table, self.reason),
        }
    }
}

/// Verify that `new` still serves the code built against `old` — the check a
/// rolling upgrade's *expand* window needs before deploy (ADR 0038).
///
/// N and N-1 run over one schema during a rolling upgrade, so the new schema must
/// keep everything N-1 reads and writes. This flags, per `old` table and column:
///
/// * a table or column `old` had that `new` drops — N-1 still names it;
/// * a column `new` tightens from nullable to `NOT NULL` with no default — an N-1
///   `INSERT` that omits it would fail;
/// * a column `new` tightens to `UNIQUE` — an N-1 `INSERT` of a duplicate fails;
/// * a column whose type `new` changed — N-1 reads/writes may no longer round-trip.
///
/// An empty result means `new` is backward-compatible with `old`. Pure: both sides
/// are [`Snapshot`]s, so a project can run it on `makemigrations` output with no
/// database. Additions (new tables/columns N-1 does not know about) are always
/// compatible and are not reported.
pub fn check_compat(old: &Snapshot, new: &Snapshot) -> Vec<Incompatibility> {
    let new_tables: BTreeMap<&str, &Table> =
        new.tables.iter().map(|t| (t.name.as_str(), t)).collect();

    let mut issues = Vec::new();
    for ot in &old.tables {
        let Some(nt) = new_tables.get(ot.name.as_str()) else {
            issues.push(Incompatibility {
                table: ot.name.clone(),
                column: None,
                reason: "table removed, but the N-1 code still reads it".to_string(),
            });
            continue;
        };
        let new_cols: BTreeMap<&str, &Column> =
            nt.columns.iter().map(|c| (c.name.as_str(), c)).collect();

        for oc in &ot.columns {
            let Some(nc) = new_cols.get(oc.name.as_str()) else {
                issues.push(Incompatibility {
                    table: ot.name.clone(),
                    column: Some(oc.name.clone()),
                    reason: "column removed, but the N-1 code still uses it".to_string(),
                });
                continue;
            };
            let has_default = nc.default.is_some() || nc.auto_now_add || nc.auto_now;
            if oc.nullable && !nc.nullable && !has_default {
                issues.push(Incompatibility {
                    table: ot.name.clone(),
                    column: Some(oc.name.clone()),
                    reason: "tightened to NOT NULL with no default; an N-1 INSERT that \
                             omits it would fail"
                        .to_string(),
                });
            }
            if !oc.unique && nc.unique {
                issues.push(Incompatibility {
                    table: ot.name.clone(),
                    column: Some(oc.name.clone()),
                    reason: "tightened to UNIQUE; an N-1 INSERT of a duplicate would fail"
                        .to_string(),
                });
            }
            if oc.ty != nc.ty {
                issues.push(Incompatibility {
                    table: ot.name.clone(),
                    column: Some(oc.name.clone()),
                    reason: format!(
                        "type changed from `{}` to `{}`; N-1 reads/writes may not round-trip",
                        oc.ty, nc.ty
                    ),
                });
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autodetect::{Column, Table};

    fn col(name: &str, ty: &str, nullable: bool) -> Column {
        Column {
            name: name.to_string(),
            ty: ty.to_string(),
            nullable,
            unique: false,
            primary_key: false,
            index: false,
            default: None,
            auto_now_add: false,
            auto_now: false,
            references: None,
        }
    }

    fn table(name: &str, columns: Vec<Column>) -> Table {
        Table {
            name: name.to_string(),
            columns,
            unique_together: vec![],
            checks: vec![],
        }
    }

    #[test]
    fn a_plain_backfill_pages_by_the_key_and_binds_cursor_and_batch() {
        let spec = BackfillSpec::new("accounts", "id", "status = 'active'", 1000);
        assert_eq!(
            batch_update_sql(&spec).unwrap(),
            "WITH batch AS (SELECT id AS k FROM accounts WHERE id > $1 ORDER BY id LIMIT $2) \
             UPDATE accounts AS t SET status = 'active' FROM batch WHERE t.id = batch.k \
             RETURNING t.id"
        );
    }

    #[test]
    fn a_filtered_backfill_only_touches_matching_rows() {
        let spec = BackfillSpec {
            filter_sql: Some("status IS NULL"),
            ..BackfillSpec::new("accounts", "id", "status = 'active'", 500)
        };
        assert_eq!(
            batch_update_sql(&spec).unwrap(),
            "WITH batch AS (SELECT id AS k FROM accounts WHERE id > $1 AND (status IS NULL) \
             ORDER BY id LIMIT $2) UPDATE accounts AS t SET status = 'active' \
             FROM batch WHERE t.id = batch.k RETURNING t.id"
        );
    }

    #[test]
    fn a_backfill_refuses_an_injected_table_or_key() {
        assert!(batch_update_sql(&BackfillSpec::new("x; DROP TABLE y", "id", "a = 1", 1)).is_err());
        assert!(batch_update_sql(&BackfillSpec::new("x", "id; DROP", "a = 1", 1)).is_err());
    }

    #[test]
    fn concurrently_and_friends_are_non_transactional_but_plain_ddl_is_not() {
        assert!(is_non_transactional(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS t_c_idx ON t (c)"
        ));
        assert!(is_non_transactional("DROP INDEX CONCURRENTLY t_c_idx"));
        assert!(is_non_transactional("REINDEX INDEX CONCURRENTLY t_c_idx"));
        assert!(is_non_transactional("alter type mood add value 'excited'"));
        assert!(is_non_transactional("  VACUUM ANALYZE t"));
        assert!(!is_non_transactional("CREATE INDEX t_c_idx ON t (c)"));
        assert!(!is_non_transactional("ALTER TABLE t ADD COLUMN c TEXT"));
        assert!(!is_non_transactional(
            "ALTER TABLE t ALTER COLUMN c SET NOT NULL"
        ));
    }

    #[test]
    fn a_compatible_superset_schema_has_no_incompatibilities() {
        let old = Snapshot {
            tables: vec![table(
                "a",
                vec![col("id", "i64", false), col("name", "text", false)],
            )],
        };
        // New adds a nullable column and a whole new table — both additive.
        let new = Snapshot {
            tables: vec![
                table(
                    "a",
                    vec![
                        col("id", "i64", false),
                        col("name", "text", false),
                        col("note", "text", true),
                    ],
                ),
                table("b", vec![col("id", "i64", false)]),
            ],
        };
        assert!(check_compat(&old, &new).is_empty());
    }

    #[test]
    fn check_compat_refuses_a_dropped_column_the_old_code_uses() {
        let old = Snapshot {
            tables: vec![table(
                "a",
                vec![col("id", "i64", false), col("name", "text", false)],
            )],
        };
        let new = Snapshot {
            tables: vec![table("a", vec![col("id", "i64", false)])],
        };
        let issues = check_compat(&old, &new);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].table, "a");
        assert_eq!(issues[0].column.as_deref(), Some("name"));
        assert!(issues[0].reason.contains("removed"));
    }

    #[test]
    fn check_compat_refuses_a_dropped_table_a_new_not_null_and_a_retype() {
        let old = Snapshot {
            tables: vec![
                table("gone", vec![col("id", "i64", false)]),
                table(
                    "a",
                    vec![
                        col("id", "i64", false),
                        col("opt", "text", true),
                        col("n", "i32", false),
                    ],
                ),
            ],
        };
        // `gone` dropped; `opt` tightened to NOT NULL with no default; `n` retyped.
        let new = Snapshot {
            tables: vec![table(
                "a",
                vec![
                    col("id", "i64", false),
                    col("opt", "text", false),
                    col("n", "i64", false),
                ],
            )],
        };
        let issues = check_compat(&old, &new);
        assert_eq!(issues.len(), 3, "{issues:#?}");
        assert!(issues
            .iter()
            .any(|i| i.table == "gone" && i.column.is_none()));
        assert!(issues
            .iter()
            .any(|i| i.column.as_deref() == Some("opt") && i.reason.contains("NOT NULL")));
        assert!(issues
            .iter()
            .any(|i| i.column.as_deref() == Some("n") && i.reason.contains("type changed")));
    }

    #[test]
    fn a_new_not_null_column_with_a_default_is_compatible() {
        let old = Snapshot {
            tables: vec![table(
                "a",
                vec![col("id", "i64", false), col("opt", "text", true)],
            )],
        };
        let mut tightened = col("opt", "text", false);
        tightened.default = Some("''".to_string());
        let new = Snapshot {
            tables: vec![table("a", vec![col("id", "i64", false), tightened])],
        };
        assert!(
            check_compat(&old, &new).is_empty(),
            "a default fills an omitted INSERT, so N-1 still works"
        );
    }
}
