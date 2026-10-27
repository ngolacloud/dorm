//! Apply migrations: versioned, advisory-locked, idempotent, and able to adopt a
//! database that already carries the schema.
//!
//! This mirrors the discipline `delonix-api`'s hand-written `CpStore` migrator
//! already keeps — a versions table, an advisory lock, one transaction per
//! step — so that when ADR 0072 folds the two, there is one migrator, not two
//! with different rules.

use crate::{ddl, Result};
use dorm_core::db;
use dorm_core::meta::ModelMeta;

/// A versioned migration: a monotonically increasing version, a name, and the
/// statements to run. The statements are expected to be idempotent
/// (`IF NOT EXISTS`), which is what lets [`migrate`] adopt a pre-existing schema.
#[derive(Debug, Clone)]
pub struct Migration {
    pub version: i64,
    pub name: String,
    pub statements: Vec<String>,
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
    Ok(Migration {
        version: 1,
        name: "initial".to_string(),
        statements,
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
/// under an advisory lock, each inside the transaction that records it.
///
/// Idempotent in two ways: an already-recorded version is skipped, and because
/// the statements use `IF NOT EXISTS`, a first run against a database that
/// already carries the schema records the version without altering anything.
///
/// Returns the versions applied this call — empty when everything was current.
pub async fn migrate(migrations: &[Migration]) -> Result<Vec<i64>> {
    db::guard_write("migrate")?;
    let applied = db::atomic(|_tx| async move {
        db::execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK]).await?;
        db::execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {LEDGER} (\
                 version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_unix BIGINT NOT NULL)"
            ),
            &[],
        )
        .await?;

        let done: std::collections::HashSet<i64> =
            db::query(&format!("SELECT version FROM {LEDGER}"), &[])
                .await?
                .iter()
                .map(|r| r.get::<_, i64>(0))
                .collect();

        let mut just_applied = Vec::new();
        for m in migrations {
            if done.contains(&m.version) {
                continue;
            }
            for stmt in &m.statements {
                db::execute(stmt, &[]).await?;
            }
            db::execute(
                &format!("INSERT INTO {LEDGER} (version, name, applied_unix) VALUES ($1, $2, $3)"),
                &[&m.version, &m.name, &now_unix()],
            )
            .await?;
            just_applied.push(m.version);
        }
        Ok(just_applied)
    })
    .await?;
    Ok(applied)
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
