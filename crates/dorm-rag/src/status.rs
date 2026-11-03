//! `dorm rag status` and `dorm rag reindex` (ADR 0019, brick 6), as library
//! functions the generated `rag` binary calls.

use dorm_core::db;
use dorm_core::error::{Error, Result};
use dorm_core::registry;
use dorm_core::value::Value;

use crate::migrations::{shadow_table, OUTBOX_TABLE};
use crate::rags;

/// One RAG model's index health.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ModelRagStatus {
    pub model: String,
    pub table: String,
    /// Distinct records with at least one chunk indexed.
    pub indexed_records: i64,
    /// Outbox events still to process (below the attempt ceiling).
    pub pending: i64,
    /// Outbox events parked after too many failures.
    pub dead_letter: i64,
    /// The embedder that wrote the chunks, if any are indexed.
    pub embedder: Option<String>,
    /// Its recorded version (the dimension), if any are indexed.
    pub embedder_version: Option<String>,
}

/// Index health for every registered RAG model. `max_attempts` is the worker's
/// dead-letter ceiling, so `pending` and `dead_letter` partition the outbox the
/// same way the worker sees it.
pub async fn status(max_attempts: i32) -> Result<Vec<ModelRagStatus>> {
    let mut out = Vec::new();
    for rag in rags() {
        let meta = registry::model(rag.model)?;
        let shadow = shadow_table(meta.table);

        let (indexed_records, embedder, embedder_version) = if table_exists(&shadow).await? {
            let rows = db::fetch(
                &format!("SELECT count(DISTINCT record_pk) FROM {shadow}"),
                &[],
            )
            .await?;
            let n = rows[0].try_get::<i64>(0)?;
            let meta_row = db::fetch(
                &format!("SELECT embedder, embedder_version FROM {shadow} LIMIT 1"),
                &[],
            )
            .await?;
            let (emb, ver) = match meta_row.first() {
                Some(r) => (Some(r.try_get::<String>(0)?), Some(r.try_get::<String>(1)?)),
                None => (None, None),
            };
            (n, emb, ver)
        } else {
            (0, None, None)
        };

        let pending = scalar(
            &format!("SELECT count(*) FROM {OUTBOX_TABLE} WHERE model = $1 AND attempts < $2"),
            &[Value::Text(rag.model.to_string()), Value::I32(max_attempts)],
        )
        .await?;
        let dead_letter = scalar(
            &format!("SELECT count(*) FROM {OUTBOX_TABLE} WHERE model = $1 AND attempts >= $2"),
            &[Value::Text(rag.model.to_string()), Value::I32(max_attempts)],
        )
        .await?;

        out.push(ModelRagStatus {
            model: rag.model.to_string(),
            table: meta.table.to_string(),
            indexed_records,
            pending,
            dead_letter,
            embedder,
            embedder_version,
        });
    }
    Ok(out)
}

/// Enqueue every matching record for re-indexing by writing `upsert` events into
/// the outbox — the worker then re-embeds them. Scoped to one `model` and/or one
/// `tenant` when given. Returns how many events were enqueued.
///
/// It enqueues rather than embedding inline, so reindexing is just more outbox
/// work: it parallelises across workers and never blocks the caller.
pub async fn reindex(model: Option<&str>, tenant: Option<&str>) -> Result<u64> {
    let mut total = 0;
    for rag in rags() {
        if let Some(only) = model {
            if rag.model != only {
                continue;
            }
        }
        let meta = registry::model(rag.model)?;
        let pk = meta
            .primary_key()
            .ok_or_else(|| Error::Config(format!("model `{}` has no primary key", rag.model)))?;

        let mut sql = format!(
            "INSERT INTO {OUTBOX_TABLE} (model, record_pk, op) \
             SELECT '{model}', {pk}::text, 'upsert' FROM {table}",
            model = meta.name,
            pk = pk.column,
            table = meta.table,
        );
        let mut params: Vec<Value> = Vec::new();
        if let Some(t) = tenant {
            let tenant_field = meta.tenant_field().ok_or_else(|| {
                Error::Config(format!(
                    "model `{}` is not tenant-scoped, so --tenant does not apply",
                    rag.model
                ))
            })?;
            sql.push_str(&format!(" WHERE {} = $1", tenant_field.column));
            params.push(Value::Text(t.to_string()));
        }
        // `meta.name`, `pk.column` and `table` are all `&'static` from the derive,
        // never caller input — the same identifier-safety rule as the compiler.
        total += db::run(&sql, &params).await?;
    }
    Ok(total)
}

async fn table_exists(name: &str) -> Result<bool> {
    Ok(scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = $1",
        &[Value::Text(name.to_string())],
    )
    .await?
        > 0)
}

async fn scalar(sql: &str, params: &[Value]) -> Result<i64> {
    let rows = db::fetch(sql, params).await?;
    rows[0].try_get::<i64>(0)
}
