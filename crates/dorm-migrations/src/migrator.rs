//! Apply migrations: versioned, idempotent, backend-agnostic, and able to adopt
//! a database that already carries the schema.
//!
//! This mirrors the discipline `delonix-api`'s hand-written `CpStore` migrator
//! already keeps — a versions table, one transaction per step, and (on
//! PostgreSQL) an advisory lock — so that when ADR 0072 folds the two, there is
//! one migrator, not two with different rules. [`migrate`] runs on PostgreSQL
//! and SQLite alike; see its docs for how each backend is serialised.

use crate::{ddl, Result};
use dorm_core::db;
use dorm_core::meta::ModelMeta;
use dorm_core::value::Value;

/// A versioned migration: a monotonically increasing version, a name, the
/// statements to apply, and the statements to undo it. The `statements` are
/// expected to be idempotent (`IF NOT EXISTS`), which is what lets [`migrate`]
/// adopt a pre-existing schema.
///
/// `down` is the inverse — what [`revert`] runs to roll the migration back. It is
/// **empty when the migration is not losslessly reversible**: a migration that
/// drops a column or changes a type cannot be undone without the data it
/// discarded, so rather than fabricate a wrong inverse, DORM records no `down`
/// and [`revert`] refuses to cross it (the same honesty Django's
/// non-reversible migrations keep). An additive migration — new tables, columns,
/// indexes, renames — has a full `down`.
#[derive(Debug, Clone)]
pub struct Migration {
    pub version: i64,
    pub name: String,
    pub statements: Vec<String>,
    /// The inverse statements, or empty when the migration is irreversible.
    pub down: Vec<String>,
}

/// The initial migration (version 1): create every table and index for `models`.
///
/// This is the `expand` step of `expand → migrate → contract`: run against an
/// empty database it builds the schema; run against one that already has the
/// tables it changes nothing (every statement is `IF NOT EXISTS`) and only
/// records that version 1 is present.
pub fn initial(models: &[&'static ModelMeta]) -> Result<Migration> {
    let schema = ddl::schema_for(models)?;
    let statements = schema
        .split(";\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    // The inverse of "create every table" is "drop every table", in reverse
    // declaration order so a child table goes before the parent it references.
    let down = models
        .iter()
        .rev()
        .map(|m| ddl::drop_table(m.table))
        .collect();
    Ok(Migration {
        version: 1,
        name: "initial".to_string(),
        statements,
        down,
    })
}

/// The migrations ledger. Separate from `delonix-api`'s `schema_migrations` on
/// purpose: while the two migrators coexist (ADR 0072 transition) they must not
/// write the same table. The `contract` step folds them.
const LEDGER: &str = "orm_schema_migrations";

/// A fixed, arbitrary advisory-lock key. Every process applying migrations takes
/// it, so two `migrate` calls serialise instead of racing on `CREATE TABLE`.
const MIGRATE_LOCK: i64 = 0x0072_0d61;

/// Apply the migrations not yet recorded in `orm_schema_migrations`, in order,
/// each inside the transaction that records it — the `prisma migrate deploy` /
/// Django `migrate` shape. Backend-agnostic: it runs on PostgreSQL and SQLite
/// alike, choosing its statement forms from the DSN's backend.
///
/// Idempotent in two ways: an already-recorded version is skipped, and because
/// the statements use `IF NOT EXISTS`, a first run against a database that
/// already carries the schema records the version without altering anything.
///
/// # How the two backends are kept safe
///
/// On PostgreSQL an advisory transaction lock serialises two `migrate` calls in
/// *different processes*; within one process the surrounding transaction already
/// does. SQLite needs no advisory lock: [`db::atomic`] holds its single
/// connection for the whole scope, and SQLite's own file lock serialises writers
/// across processes. The ledger's `BIGINT`/`TEXT` column types and the
/// `IF NOT EXISTS` guard are accepted verbatim by both.
///
/// Returns the versions applied this call — empty when everything was current.
pub async fn migrate(migrations: &[Migration]) -> Result<Vec<i64>> {
    db::guard_write("migrate")?;
    let is_postgres = db::backend_of(&db::dsn()?) == db::Backend::Postgres;
    let applied = db::atomic(|_tx| async move {
        if is_postgres {
            db::run(
                "SELECT pg_advisory_xact_lock($1)",
                &[Value::I64(MIGRATE_LOCK)],
            )
            .await?;
        }
        db::run(
            &format!(
                "CREATE TABLE IF NOT EXISTS {LEDGER} (\
                 version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_unix BIGINT NOT NULL)"
            ),
            &[],
        )
        .await?;

        let done: std::collections::HashSet<i64> =
            db::fetch(&format!("SELECT version FROM {LEDGER}"), &[])
                .await?
                .iter()
                .map(|r| r.try_get::<i64>(0))
                .collect::<dorm_core::Result<_>>()?;

        // `$n` on PostgreSQL, `?n` on SQLite — the one placeholder difference the
        // two dialects force; the values are identical.
        let insert = if is_postgres {
            format!("INSERT INTO {LEDGER} (version, name, applied_unix) VALUES ($1, $2, $3)")
        } else {
            format!("INSERT INTO {LEDGER} (version, name, applied_unix) VALUES (?1, ?2, ?3)")
        };

        let mut just_applied = Vec::new();
        for m in migrations {
            if done.contains(&m.version) {
                continue;
            }
            for stmt in &m.statements {
                db::run(stmt, &[]).await?;
            }
            db::run(
                &insert,
                &[
                    Value::I64(m.version),
                    Value::Text(m.name.clone()),
                    Value::I64(now_unix()),
                ],
            )
            .await?;
            just_applied.push(m.version);
        }
        Ok(just_applied)
    })
    .await?;
    Ok(applied)
}

/// Roll the schema back to `to_version`, undoing every migration applied after it
/// in reverse order. Pass `0` to undo everything. This is Django's `migrate <app>
/// <version>` run backwards; Prisma has no exact twin.
///
/// Each crossed migration's [`down`](Migration::down) is run and its ledger row
/// removed, newest first, all in one transaction under the same advisory lock as
/// [`migrate`]. A migration that is **not reversible** — empty `down`, because it
/// dropped a column or changed a type — stops the revert with a named error
/// *before* anything past it is undone: DORM will not fabricate an inverse that
/// loses data. Idempotent: a version already absent from the ledger is skipped.
///
/// Returns the versions reverted, newest-first.
pub async fn revert(migrations: &[Migration], to_version: i64) -> Result<Vec<i64>> {
    db::guard_write("revert")?;
    let is_postgres = db::backend_of(&db::dsn()?) == db::Backend::Postgres;

    // Only migrations strictly above the target, newest first.
    let mut to_undo: Vec<&Migration> = migrations
        .iter()
        .filter(|m| m.version > to_version)
        .collect();
    to_undo.sort_by_key(|m| std::cmp::Reverse(m.version));

    let reverted = db::atomic(|_tx| async move {
        if is_postgres {
            db::run(
                "SELECT pg_advisory_xact_lock($1)",
                &[Value::I64(MIGRATE_LOCK)],
            )
            .await?;
        }
        db::run(
            &format!(
                "CREATE TABLE IF NOT EXISTS {LEDGER} (\
                 version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_unix BIGINT NOT NULL)"
            ),
            &[],
        )
        .await?;

        let done: std::collections::HashSet<i64> =
            db::fetch(&format!("SELECT version FROM {LEDGER}"), &[])
                .await?
                .iter()
                .map(|r| r.try_get::<i64>(0))
                .collect::<dorm_core::Result<_>>()?;

        let delete = if is_postgres {
            format!("DELETE FROM {LEDGER} WHERE version = $1")
        } else {
            format!("DELETE FROM {LEDGER} WHERE version = ?1")
        };

        let mut undone = Vec::new();
        for m in &to_undo {
            if !done.contains(&m.version) {
                continue; // not applied — nothing to undo
            }
            if m.down.is_empty() && !m.statements.is_empty() {
                return Err(dorm_core::Error::Config(format!(
                    "migration {} (`{}`) is not reversible: it has no `down` (it dropped data \
                     or changed a type), so revert stops before undoing it",
                    m.version, m.name
                )));
            }
            for stmt in &m.down {
                db::run(stmt, &[]).await?;
            }
            db::run(&delete, &[Value::I64(m.version)]).await?;
            undone.push(m.version);
        }
        Ok(undone)
    })
    .await?;
    Ok(reverted)
}

/// Bring the live schema in line with `models`, **additively**: create any table
/// that is missing (with its indexes), and add any column a model declares that
/// its table does not yet have. Returns the DDL it actually ran, in order.
///
/// This is the quick-iteration path — the shape `prisma db push` has — as opposed
/// to the versioned [`migrate`]: no ledger, no history, just "make the database
/// match the models". It is deliberately **only additive**: it never drops a
/// table or column and never changes a type, so it cannot lose data. Destructive
/// changes stay a human's decision.
///
/// The set difference is computed from `information_schema`, under the same
/// advisory lock and single transaction as [`migrate`], so two concurrent syncs
/// serialise rather than racing on `ALTER TABLE`.
pub async fn sync(models: &[&'static ModelMeta]) -> Result<Vec<String>> {
    db::guard_write("sync")?;
    let plans = build_plans(models)?;
    let backend = ddl::Backend::of_dsn(&db::dsn()?);

    // Indexes build `CONCURRENTLY` on PostgreSQL (B-22), which PostgreSQL forbids
    // inside a transaction — so they are collected now and applied *after* the
    // transactional changes, each on its own connection outside any transaction.
    // They are idempotent (`IF NOT EXISTS`), so re-running `sync` is safe, and
    // they are not among the reported changes (as before).
    let index_stmts: Vec<String> = models
        .iter()
        .flat_map(|m| ddl::indexes_concurrent(m, backend))
        .collect();

    // Transactional part: create missing tables and add missing columns, under
    // the advisory lock so two concurrent syncs serialise rather than racing on
    // `ALTER TABLE`.
    let changes = db::atomic(|_tx| async move {
        db::execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK]).await?;
        let (tables, columns) = introspect().await?;
        let changes = changes_for(&plans, &tables, &columns);
        for stmt in &changes {
            db::execute(stmt, &[]).await?;
        }
        Ok::<Vec<String>, dorm_core::Error>(changes)
    })
    .await?;

    // Non-transactional part: the indexes. Outside the transaction so
    // `CREATE INDEX CONCURRENTLY` is legal and does not block writes to a
    // populated table.
    for idx in &index_stmts {
        db::execute(idx, &[]).await?;
    }

    Ok(changes)
}

/// The additive changes [`sync`] *would* apply, without touching the database —
/// `prisma migrate diff` for the quick-iteration path. Read-only: it takes no
/// write lock and runs in a read-only session, so it is safe to point at
/// production to see what a `push` would do. An empty result means the schema
/// already matches the models.
pub async fn diff(models: &[&'static ModelMeta]) -> Result<Vec<String>> {
    let plans = build_plans(models)?;
    let (tables, columns) = introspect().await?;
    Ok(changes_for(&plans, &tables, &columns))
}

/// One table's intended DDL: `(table, CREATE TABLE, [CREATE INDEX…],
/// [(column, ADD COLUMN)…])`. Built purely from the models so the fallible DDL
/// generation happens outside any transaction.
type Plan = (
    &'static str,
    String,
    Vec<String>,
    Vec<(&'static str, String)>,
);

fn build_plans(models: &[&'static ModelMeta]) -> Result<Vec<Plan>> {
    let mut plans: Vec<Plan> = Vec::with_capacity(models.len());
    for m in models {
        let mut adds = Vec::new();
        for f in m.fields {
            // A missing primary key is not something additive sync adds.
            if !f.primary_key {
                adds.push((f.column, ddl::add_column(m, f)?));
            }
        }
        plans.push((m.table, ddl::create_table(m)?, ddl::indexes(m), adds));
    }
    Ok(plans)
}

/// Read the public schema's tables and their columns from `information_schema`.
async fn introspect() -> dorm_core::Result<(
    std::collections::HashSet<String>,
    std::collections::HashMap<String, std::collections::HashSet<String>>,
)> {
    let tables: std::collections::HashSet<String> = db::query(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
        &[],
    )
    .await?
    .iter()
    .map(|r| r.get::<_, String>(0))
    .collect();

    let mut columns: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for r in db::query(
        "SELECT table_name, column_name FROM information_schema.columns \
         WHERE table_schema = 'public'",
        &[],
    )
    .await?
    {
        columns
            .entry(r.get::<_, String>(0))
            .or_default()
            .insert(r.get::<_, String>(1));
    }
    Ok((tables, columns))
}

/// The additive statements needed to bring the schema to the models: a
/// `CREATE TABLE` for each missing table, an `ADD COLUMN` for each missing
/// column. Pure — the caller decides whether to run them. Indexes are not
/// included: they are idempotent and applied separately by [`sync`].
fn changes_for(
    plans: &[Plan],
    tables: &std::collections::HashSet<String>,
    columns: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) -> Vec<String> {
    let mut changes = Vec::new();
    let empty = std::collections::HashSet::new();
    for (table, create, _indexes, adds) in plans {
        if !tables.contains(*table) {
            changes.push(create.clone());
        } else {
            let have = columns.get(*table).unwrap_or(&empty);
            for (col, stmt) in adds {
                if !have.contains(*col) {
                    changes.push(stmt.clone());
                }
            }
        }
    }
    changes
}

/// Seconds since the epoch. `SystemTime`, not a database `now()`, because the
/// ledger stamp is wall-clock metadata, not a value a query reasons about.
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
