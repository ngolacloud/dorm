//! The RAG worker (ADR 0019, brick 3): drain `dorm_rag_outbox`, embed what
//! changed, keep each model's shadow table in step.
//!
//! The loop claims a batch with `FOR UPDATE SKIP LOCKED`, so several workers run
//! in parallel without stepping on each other. For each record it loads the row
//! through the dynamic API, renders the template, chunks it, re-embeds **only the
//! chunks whose content hash (or embedder) changed**, upserts them, and trims any
//! now-orphaned chunks. A `delete` event removes the record's chunks. Failures
//! raise the event's `attempts`; past `max_attempts` the event is left as a
//! dead-letter (the drain query skips it) rather than retried forever.
//!
//! Tracing is emitted on the `dorm::rag` target in the ADR 0008 style — events,
//! never a subscriber, never a secret.

use std::collections::HashMap;
use std::time::Duration;

use dorm_core::compiler::Plan;
use dorm_core::db;
use dorm_core::error::{Error, Result};
use dorm_core::expr::Q;
use dorm_core::meta::ModelMeta;
use dorm_core::queryset::{fetch_value_rows, ValueRow};
use dorm_core::registry;
use dorm_core::value::{FieldType, Value};

use crate::embedder::Embedder;
use crate::meta::RagMeta;
use crate::migrations::{shadow_table, OUTBOX_TABLE};
use crate::{chunk, rags};

/// How the worker runs.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Events claimed per batch.
    pub batch_size: i64,
    /// After this many failed attempts an event is left as a dead-letter.
    pub max_attempts: i32,
    /// How long [`run`] sleeps when the outbox is empty.
    pub poll_interval: Duration,
    /// The claim lease: a claimed event is invisible to other workers for this
    /// long. If a worker crashes mid-process the lease expires and the event is
    /// re-claimed, so a slow embedder (run **outside** the transaction) never
    /// holds a row lock and a crash never strands an event. Set it comfortably
    /// above the slowest expected embed of one record.
    pub lease_secs: i32,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            batch_size: 64,
            max_attempts: 5,
            poll_interval: Duration::from_secs(2),
            lease_secs: 300,
        }
    }
}

/// What one [`run_once`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkerStats {
    /// Records whose events were handled (upserts + deletes).
    pub processed: u64,
    pub upserts: u64,
    pub deletes: u64,
    /// Records whose processing errored this pass (their events keep their place
    /// in the outbox, with `attempts` raised).
    pub failed: u64,
}

enum Handled {
    Upsert,
    Delete,
}

/// One coalesced unit of work: a model, a record, the last op, and the outbox row
/// ids it stands for.
struct Claim {
    model: String,
    pk: String,
    op: String,
    ids: Vec<i64>,
}

/// Drain one batch. Returns what it did; `processed == 0` means the outbox was
/// empty (or every pending event is dead-lettered or already claimed elsewhere).
///
/// The embedding — a network round-trip — runs **outside** any transaction, so a
/// slow embedder never holds a database lock. The two are decoupled by a lease:
/// phase 1 claims a batch in a short transaction (stamping `claimed_at`, which
/// hides the rows from other workers for `lease_secs`) and releases the row
/// locks; phase 2 embeds and writes with no transaction open; phase 3 drops the
/// handled events or, on failure, raises `attempts` and clears the lease. A crash
/// in phase 2 leaves the lease to expire, after which the event is re-claimed —
/// so delivery stays at-least-once without the batch holding locks across I/O.
pub async fn run_once<E: Embedder>(embedder: &E, cfg: &WorkerConfig) -> Result<WorkerStats> {
    let claims = claim_batch(cfg).await?;
    let mut stats = WorkerStats::default();
    for claim in claims {
        match process_record(embedder, &claim.model, &claim.pk, &claim.op).await {
            Ok(kind) => {
                stats.processed += 1;
                match kind {
                    Handled::Upsert => stats.upserts += 1,
                    Handled::Delete => stats.deletes += 1,
                }
                finish_ok(&claim.ids).await?;
            }
            Err(e) => {
                stats.failed += 1;
                tracing::warn!(
                    target: "dorm::rag",
                    model = %claim.model,
                    record_pk = %claim.pk,
                    error = %e,
                    "rag worker failed an event; will retry"
                );
                finish_fail(&claim.ids, &e.to_string()).await?;
            }
        }
    }
    Ok(stats)
}

/// Phase 1: claim a batch under a lease, in a short transaction. Rows already
/// claimed (and not yet lease-expired) and dead-lettered rows are skipped.
async fn claim_batch(cfg: &WorkerConfig) -> Result<Vec<Claim>> {
    let max = cfg.max_attempts;
    let lease = cfg.lease_secs;
    let batch = cfg.batch_size;
    db::atomic(|_tx| async move {
        let rows = db::fetch(
            &format!(
                "SELECT id, model, record_pk, op FROM {OUTBOX_TABLE} \
                 WHERE attempts < $1 \
                   AND (claimed_at IS NULL OR claimed_at < now() - ($2::integer * interval '1 second')) \
                 ORDER BY id FOR UPDATE SKIP LOCKED LIMIT $3"
            ),
            &[Value::I32(max), Value::I32(lease), Value::I64(batch)],
        )
        .await?;

        // Coalesce events per (model, record): several edits to one row collapse to
        // one re-index, taking the last op. First-seen order is stable/debuggable.
        let mut order: Vec<(String, String)> = Vec::new();
        let mut groups: HashMap<(String, String), (String, Vec<i64>)> = HashMap::new();
        let mut all_ids: Vec<i64> = Vec::new();
        for r in &rows {
            let id = r.try_get::<i64>(0)?;
            let model = r.try_get::<String>(1)?;
            let pk = r.try_get::<String>(2)?;
            let op = r.try_get::<String>(3)?;
            all_ids.push(id);
            let key = (model, pk);
            let entry = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                (op.clone(), Vec::new())
            });
            entry.0 = op;
            entry.1.push(id);
        }

        // Stamp the lease on the claimed rows, still holding their locks, so no
        // other worker can take them until we finish or the lease expires.
        if !all_ids.is_empty() {
            db::run(
                &format!(
                    "UPDATE {OUTBOX_TABLE} SET claimed_at = now() WHERE id IN ({})",
                    in_list(1, all_ids.len())
                ),
                &id_params(&all_ids),
            )
            .await?;
        }

        Ok(order
            .into_iter()
            .map(|key| {
                let (op, ids) = groups.remove(&key).expect("group present");
                Claim {
                    model: key.0,
                    pk: key.1,
                    op,
                    ids,
                }
            })
            .collect())
    })
    .await
}

/// Phase 3 (success): the record is indexed, so its events are done.
async fn finish_ok(ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    db::run(
        &format!(
            "DELETE FROM {OUTBOX_TABLE} WHERE id IN ({})",
            in_list(1, ids.len())
        ),
        &id_params(ids),
    )
    .await
    .map(|_| ())
}

/// Phase 3 (failure): raise `attempts` and release the lease so the event retries
/// (or dead-letters once `attempts` reaches the ceiling).
async fn finish_fail(ids: &[i64], err: &str) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut params = vec![Value::Text(truncate(err, 500))];
    params.extend(id_params(ids));
    db::run(
        &format!(
            "UPDATE {OUTBOX_TABLE} SET attempts = attempts + 1, last_error = $1, \
             claimed_at = NULL, updated_at = now() WHERE id IN ({})",
            in_list(2, ids.len())
        ),
        &params,
    )
    .await
    .map(|_| ())
}

/// Run until cancelled: drain, and when the outbox is empty, sleep for the poll
/// interval. Returns only on a fatal error (a lost database, say).
pub async fn run<E: Embedder>(embedder: &E, cfg: &WorkerConfig) -> Result<()> {
    tracing::info!(
        target: "dorm::rag",
        embedder = embedder.model_id(),
        dimension = embedder.dimension(),
        "rag worker started"
    );
    loop {
        let stats = run_once(embedder, cfg).await?;
        if stats.processed == 0 && stats.failed == 0 {
            tokio::time::sleep(cfg.poll_interval).await;
        }
    }
}

/// Handle one record: upsert its chunks, or delete them.
async fn process_record<E: Embedder>(
    embedder: &E,
    model_name: &str,
    pk_str: &str,
    op: &str,
) -> Result<Handled> {
    let meta = registry::model(model_name)?;
    let rag = rags()
        .find(|r| r.model == model_name)
        .ok_or_else(|| Error::Config(format!("no RAG configuration for model `{model_name}`")))?;
    let pk = meta
        .primary_key()
        .ok_or_else(|| Error::Config(format!("model `{model_name}` has no primary key for RAG")))?;
    let shadow = shadow_table(meta.table);

    if op == "delete" {
        delete_chunks(&shadow, pk_str).await?;
        return Ok(Handled::Delete);
    }

    // Load the record through the dynamic API; the pk arrives as text (uniform in
    // the outbox) and is converted to the model's key type before binding.
    let pk_val = pk_value_from_text(pk_str, pk.ty, pk.column)?;
    let mut plan = Plan::new(meta);
    plan.filters
        .push(Q::new(pk.name.to_string(), pk_val.clone()));
    plan.limit = Some(1);
    let Some(row) = fetch_value_rows(&plan).await?.into_iter().next() else {
        // The row was deleted between enqueue and now: remove its chunks.
        delete_chunks(&shadow, pk_str).await?;
        return Ok(Handled::Delete);
    };

    let document = render_template(rag.template, &row);
    let chunks = chunk::chunk(
        &document,
        rag.chunk_size as usize,
        rag.chunk_overlap as usize,
    );
    let hashes: Vec<String> = chunks.iter().map(|c| chunk::content_hash(c)).collect();

    // What is already indexed, to re-embed only what changed (by hash) or went
    // stale (a different embedder/dimension).
    let existing = db::fetch(
        &format!(
            "SELECT chunk_index, content_hash, embedder, embedder_version \
             FROM {shadow} WHERE record_pk::text = $1"
        ),
        &[Value::Text(pk_str.to_string())],
    )
    .await?;
    let mut existing_by_index: HashMap<i32, (String, String, String)> = HashMap::new();
    for r in &existing {
        existing_by_index.insert(
            r.try_get::<i32>(0)?,
            (
                r.try_get::<String>(1)?,
                r.try_get::<String>(2)?,
                r.try_get::<String>(3)?,
            ),
        );
    }

    let model_id = embedder.model_id().to_string();
    let version = embedder.dimension().to_string();
    let mut embed_indices: Vec<usize> = Vec::new();
    let mut embed_texts: Vec<String> = Vec::new();
    for (i, hash) in hashes.iter().enumerate() {
        let reuse = existing_by_index
            .get(&(i as i32))
            .is_some_and(|(h, emb, ver)| h == hash && *emb == model_id && *ver == version);
        if !reuse {
            embed_indices.push(i);
            embed_texts.push(chunks[i].clone());
        }
    }

    let vectors = if embed_texts.is_empty() {
        Vec::new()
    } else {
        embedder.embed(&embed_texts).await?
    };

    for (k, &i) in embed_indices.iter().enumerate() {
        upsert_chunk(
            meta,
            rag,
            &shadow,
            &pk_val,
            &row,
            i as i32,
            &chunks[i],
            &vectors[k],
            &hashes[i],
            &model_id,
            &version,
        )
        .await?;
    }

    // Trim chunks that no longer exist (the document got shorter).
    db::run(
        &format!("DELETE FROM {shadow} WHERE record_pk::text = $1 AND chunk_index >= $2"),
        &[
            Value::Text(pk_str.to_string()),
            Value::I32(chunks.len() as i32),
        ],
    )
    .await?;

    tracing::debug!(
        target: "dorm::rag",
        model = model_name,
        record_pk = pk_str,
        chunks = chunks.len(),
        embedded = embed_indices.len(),
        "rag record indexed"
    );
    Ok(Handled::Upsert)
}

/// Turn the outbox's text record key back into the model's typed key. The
/// integer cases parse directly (`Value::coerce` deliberately refuses a lossy
/// text→integer); the rest (`Uuid`, `Decimal`, …) go through `coerce`, which
/// parses text into exactly those.
pub(crate) fn pk_value_from_text(pk_str: &str, ty: FieldType, column: &str) -> Result<Value> {
    let bad = || Error::UnexpectedColumnType {
        column: column.to_string(),
        want: ty.describe(),
    };
    Ok(match ty {
        FieldType::I32 => Value::I32(pk_str.parse().map_err(|_| bad())?),
        FieldType::I64 => Value::I64(pk_str.parse().map_err(|_| bad())?),
        FieldType::Text => Value::Text(pk_str.to_string()),
        _ => Value::Text(pk_str.to_string()).coerce(ty, column)?,
    })
}

async fn delete_chunks(shadow: &str, pk_str: &str) -> Result<()> {
    db::run(
        &format!("DELETE FROM {shadow} WHERE record_pk::text = $1"),
        &[Value::Text(pk_str.to_string())],
    )
    .await
    .map(|_| ())
}

/// Insert or replace one chunk. The vector is bound as a parameter with a
/// `::vector` cast — never spliced — and no `Value` variant is added for it
/// (ADR 0019): it travels as the pgvector text literal `[f1,f2,…]`.
#[allow(clippy::too_many_arguments)]
async fn upsert_chunk(
    meta: &ModelMeta,
    rag: &RagMeta,
    shadow: &str,
    pk_val: &Value,
    row: &ValueRow,
    chunk_index: i32,
    content: &str,
    vector: &[f32],
    hash: &str,
    embedder: &str,
    version: &str,
) -> Result<()> {
    // Columns, bound values and any cast, built as one list so the placeholder
    // numbers and the parameter vector cannot drift. `embedding` is the only cast.
    let mut binds: Vec<(String, Value, Option<&str>)> = Vec::new();
    binds.push(("record_pk".to_string(), pk_val.clone(), None));
    if let Some(tenant) = meta.tenant_field() {
        binds.push(("tenant".to_string(), value_of(row, tenant.name), None));
    }
    binds.push(("chunk_index".to_string(), Value::I32(chunk_index), None));
    binds.push((
        "content".to_string(),
        Value::Text(content.to_string()),
        None,
    ));
    // `::text::vector`, not `::vector`: a bare `$n::vector` makes Postgres infer
    // the parameter's type as `vector`, so the driver would send our text in the
    // binary vector format and pgvector would read the leading `[` as a dimension
    // count (a spurious "> 16000 dimensions"). The inner `::text` pins the
    // parameter to text, then the text→vector cast parses the literal.
    binds.push((
        "embedding".to_string(),
        Value::Text(vector_literal(vector)),
        Some("text::vector"),
    ));
    binds.push((
        "content_hash".to_string(),
        Value::Text(hash.to_string()),
        None,
    ));
    binds.push((
        "embedder".to_string(),
        Value::Text(embedder.to_string()),
        None,
    ));
    binds.push((
        "embedder_version".to_string(),
        Value::Text(version.to_string()),
        None,
    ));
    for name in rag.metadata {
        let f = meta.field(name).ok_or_else(|| Error::UnknownField {
            model: meta.name.to_string(),
            field: (*name).to_string(),
        })?;
        // The tenant was already bound above; do not bind it twice.
        if meta.tenant_field().is_some_and(|t| t.column == f.column) {
            continue;
        }
        binds.push((f.column.to_string(), value_of(row, f.name), None));
    }

    let cols: Vec<&str> = binds.iter().map(|(c, _, _)| c.as_str()).collect();
    let params: Vec<Value> = binds.iter().map(|(_, v, _)| v.clone()).collect();
    let placeholders: Vec<String> = binds
        .iter()
        .enumerate()
        .map(|(i, (_, _, cast))| {
            let n = i + 1;
            match cast {
                Some(c) => format!("${n}::{c}"),
                None => format!("${n}"),
            }
        })
        .collect();

    // Everything but the key is refreshed on conflict.
    let updates: Vec<String> = cols
        .iter()
        .filter(|c| **c != "record_pk" && **c != "chunk_index")
        .map(|c| format!("{c} = EXCLUDED.{c}"))
        .chain(std::iter::once("updated_at = now()".to_string()))
        .collect();

    let sql = format!(
        "INSERT INTO {shadow} ({}) VALUES ({}) \
         ON CONFLICT (record_pk, chunk_index) DO UPDATE SET {}",
        cols.join(", "),
        placeholders.join(", "),
        updates.join(", "),
    );
    db::run(&sql, &params).await.map(|_| ())
}

/// Render a `{field}` template from a row, by the row's **field names**. An
/// unset value renders empty (not the display dash), so a template never shows a
/// `-` for a NULL.
fn render_template(template: &str, row: &ValueRow) -> String {
    let mut out = template.to_string();
    for (name, value) in row {
        let rendered = if value.is_null() {
            String::new()
        } else {
            value.to_string()
        };
        out = out.replace(&format!("{{{name}}}"), &rendered);
    }
    out
}

/// A value from the row by field name, or `Null` if absent.
fn value_of(row: &ValueRow, field: &str) -> Value {
    row.get(field).cloned().unwrap_or(Value::Null)
}

/// `[f1,f2,…]` — pgvector's text form, for a `$n::vector` bind.
pub(crate) fn vector_literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// `$start, $start+1, …` for `count` placeholders — for the outbox `IN (…)`
/// bookkeeping, where the ids are the ORM's own `BIGSERIAL`s, never caller input.
fn in_list(start: usize, count: usize) -> String {
    (start..start + count)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn id_params(ids: &[i64]) -> Vec<Value> {
    ids.iter().map(|id| Value::I64(*id)).collect()
}

/// Truncate an error message to `max` chars for the outbox `last_error` column.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> ValueRow {
        let mut r = ValueRow::new();
        r.insert("title".into(), Value::Text("Email down".into()));
        r.insert("body".into(), Value::Text("after migration".into()));
        r.insert("note".into(), Value::Null);
        r
    }

    #[test]
    fn the_template_fills_named_fields_and_blanks_nulls() {
        assert_eq!(
            render_template("{title}\n\n{body}", &row()),
            "Email down\n\nafter migration"
        );
        assert_eq!(render_template("[{note}]", &row()), "[]");
    }

    #[test]
    fn the_vector_literal_is_pgvector_shaped() {
        assert_eq!(vector_literal(&[1.0, 0.5, -0.25]), "[1,0.5,-0.25]");
        assert_eq!(vector_literal(&[]), "[]");
    }

    #[test]
    fn the_in_list_numbers_placeholders_from_the_start() {
        assert_eq!(in_list(1, 3), "$1, $2, $3");
        assert_eq!(in_list(2, 2), "$2, $3");
    }

    #[test]
    fn truncate_counts_characters_not_bytes() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("日本語", 2), "日本");
    }
}
