//! RAG migrations (ADR 0019, brick 2): the reviewable DDL that gives a model its
//! pgvector index, and the triggers that keep it fed.
//!
//! Pure — `RagMeta` + `ModelMeta` in, SQL strings out, no I/O — so the generated
//! SQL can be snapshot-tested and reviewed in a migration file, the same
//! discipline as `dorm_migrations::ddl`. It lives here, not in `dorm-migrations`,
//! so the migrator never depends on `dorm-rag`: a project wires RAG migrations in
//! by appending [`up_for_registered`] to a `Migration`'s statements, and a
//! project that does not use RAG is untouched.
//!
//! For each RAG model `tickets` this emits a **shadow table** `dorm_rag_tickets`
//! (the record key, the tenant when scoped, the chunk, its vector, a content
//! hash, the embedder, the filterable metadata columns, timestamps), an **HNSW**
//! index on the vector with the configured metric, a **GIN** index on a
//! `tsvector` of the content for the full-text half of hybrid search, and
//! **`AFTER INSERT/UPDATE/DELETE` triggers** on the model table that enqueue work
//! into the shared [`OUTBOX_TABLE`]. Change capture is a trigger, by decision, so
//! every write path — typed, `:raw`, `dorm db execute`, Studio, `db seed`, `psql`
//! — is indexed and the core write path is untouched.

use dorm_core::error::{Error, Result};
use dorm_core::meta::ModelMeta;
use dorm_core::registry;
use dorm_migrations::ddl::Backend;

use crate::meta::RagMeta;

/// The single shared outbox every RAG trigger writes into.
pub const OUTBOX_TABLE: &str = "dorm_rag_outbox";

/// The shadow table for a model table: `tickets` → `dorm_rag_tickets`.
pub fn shadow_table(model_table: &str) -> String {
    format!("dorm_rag_{model_table}")
}

/// One model's forward and inverse RAG DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagMigration {
    /// Forward statements, in apply order (shadow table, indexes, triggers).
    /// Each is one statement; the trigger function carries its own `;`s inside
    /// `$$…$$`, so it is a single element and must not be split.
    pub up: Vec<String>,
    /// Inverse statements: drop the triggers, the function, the indexes and the
    /// shadow table. The `vector` extension and the shared outbox are left — other
    /// models may use them.
    pub down: Vec<String>,
}

/// The forward+inverse DDL for one RAG model, for `backend`.
///
/// **PostgreSQL only.** On SQLite (or MySQL) this returns an explicit error
/// naming `sqlite-vec` as the future path — never a silent non-vector fallback
/// (ADR 0019, the ADR 0002 rule applied to retrieval). `dim` is the embedder's
/// output dimension (e.g. 384 for `all-minilm`), which the caller resolves from
/// the embedder config.
pub fn migration_for(
    rag: &RagMeta,
    model: &ModelMeta,
    dim: u32,
    backend: Backend,
) -> Result<RagMigration> {
    if backend != Backend::Postgres {
        return Err(Error::Config(format!(
            "RAG needs PostgreSQL + pgvector; backend {backend:?} is not supported \
             (SQLite via sqlite-vec is future work — ADR 0019). No silent fallback."
        )));
    }

    let pk = model.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` has no primary key, so its RAG shadow table has no record key",
            model.name
        ))
    })?;
    let table = shadow_table(model.table);
    let fn_name = format!("{table}_enqueue");

    // The shadow table's columns, in a fixed order. Metadata columns (and the
    // tenant, when scoped) are copied in for pre-filtering; deduplicated by column
    // so a model that lists its tenant in `metadata` does not get it twice.
    let mut cols: Vec<String> = vec![
        format!("record_pk {} NOT NULL", pk.ty.sql_type()),
        "chunk_index INTEGER NOT NULL".to_string(),
        "content TEXT NOT NULL".to_string(),
        format!("embedding vector({dim})"),
        "content_hash TEXT NOT NULL".to_string(),
        "embedder TEXT NOT NULL".to_string(),
        "embedder_version TEXT NOT NULL".to_string(),
    ];
    let mut seen_cols: Vec<&str> = Vec::new();
    if let Some(tenant) = model.tenant_field() {
        cols.insert(1, format!("tenant {} NOT NULL", tenant.ty.sql_type()));
        seen_cols.push(tenant.column);
    }
    for name in rag.metadata {
        let f = model.field(name).ok_or_else(|| Error::UnknownField {
            model: model.name.to_string(),
            field: (*name).to_string(),
        })?;
        if seen_cols.contains(&f.column) {
            continue;
        }
        seen_cols.push(f.column);
        let nullable = if f.nullable { "" } else { " NOT NULL" };
        cols.push(format!("{} {}{nullable}", f.column, f.ty.sql_type()));
    }
    cols.push("created_at TIMESTAMPTZ NOT NULL DEFAULT now()".to_string());
    cols.push("updated_at TIMESTAMPTZ NOT NULL DEFAULT now()".to_string());
    // A chunk is identified by (record, chunk index): the natural key the worker
    // upserts on.
    cols.push("PRIMARY KEY (record_pk, chunk_index)".to_string());

    let create_table = format!(
        "CREATE TABLE IF NOT EXISTS {table} (\n    {}\n)",
        cols.join(",\n    ")
    );

    // HNSW on the vector, with the configured metric's operator class; GIN on a
    // simple-config tsvector of the content for the full-text half of hybrid
    // search; a plain index on record_pk for the worker's per-record upsert/delete.
    let hnsw = format!(
        "CREATE INDEX IF NOT EXISTS {table}_embedding_idx \
         ON {table} USING hnsw (embedding {})",
        rag.distance.ops()
    );
    let gin = format!(
        "CREATE INDEX IF NOT EXISTS {table}_fts_idx \
         ON {table} USING gin (to_tsvector('simple', content))"
    );
    let pk_idx = format!("CREATE INDEX IF NOT EXISTS {table}_record_idx ON {table} (record_pk)");

    // The enqueue function: a DELETE enqueues the old key, anything else the new.
    // The record key goes into the outbox as text, uniform across pk types; the
    // worker casts it back using the model's pk type (brick 3).
    let trigger_fn = format!(
        "CREATE OR REPLACE FUNCTION {fn_name}() RETURNS trigger AS $$\n\
         BEGIN\n\
         \u{20}  IF (TG_OP = 'DELETE') THEN\n\
         \u{20}    INSERT INTO {OUTBOX_TABLE} (model, record_pk, op) \
         VALUES ('{model_name}', OLD.{pk_col}::text, 'delete');\n\
         \u{20}    RETURN OLD;\n\
         \u{20}  ELSE\n\
         \u{20}    INSERT INTO {OUTBOX_TABLE} (model, record_pk, op) \
         VALUES ('{model_name}', NEW.{pk_col}::text, 'upsert');\n\
         \u{20}    RETURN NEW;\n\
         \u{20}  END IF;\n\
         END;\n\
         $$ LANGUAGE plpgsql",
        model_name = model.name,
        pk_col = pk.column,
    );

    // Three triggers over one function. INSERT and DELETE are unconditional; the
    // UPDATE trigger carries a WHEN clause so an edit that touches no RAG or
    // metadata column enqueues nothing — the "only when relevant columns change"
    // rule. `CREATE OR REPLACE TRIGGER` (PG14+) keeps these idempotent.
    let ins_trg = format!(
        "CREATE OR REPLACE TRIGGER {table}_ins AFTER INSERT ON {model_table} \
         FOR EACH ROW EXECUTE FUNCTION {fn_name}()",
        model_table = model.table,
    );
    let del_trg = format!(
        "CREATE OR REPLACE TRIGGER {table}_del AFTER DELETE ON {model_table} \
         FOR EACH ROW EXECUTE FUNCTION {fn_name}()",
        model_table = model.table,
    );
    let upd_trg = {
        let watched = watched_columns(rag, model)?;
        let when = if watched.is_empty() {
            String::new()
        } else {
            let preds: Vec<String> = watched
                .iter()
                .map(|c| format!("OLD.{c} IS DISTINCT FROM NEW.{c}"))
                .collect();
            format!(" WHEN ({})", preds.join(" OR "))
        };
        format!(
            "CREATE OR REPLACE TRIGGER {table}_upd AFTER UPDATE ON {model_table} \
             FOR EACH ROW{when} EXECUTE FUNCTION {fn_name}()",
            model_table = model.table,
        )
    };

    let up = vec![
        create_table,
        hnsw,
        gin,
        pk_idx,
        trigger_fn,
        ins_trg,
        upd_trg,
        del_trg,
    ];
    let down = vec![
        format!("DROP TRIGGER IF EXISTS {table}_ins ON {}", model.table),
        format!("DROP TRIGGER IF EXISTS {table}_upd ON {}", model.table),
        format!("DROP TRIGGER IF EXISTS {table}_del ON {}", model.table),
        format!("DROP FUNCTION IF EXISTS {fn_name}()"),
        format!("DROP INDEX IF EXISTS {table}_embedding_idx"),
        format!("DROP INDEX IF EXISTS {table}_fts_idx"),
        format!("DROP INDEX IF EXISTS {table}_record_idx"),
        format!("DROP TABLE IF EXISTS {table}"),
    ];
    Ok(RagMigration { up, down })
}

/// The columns an UPDATE must touch to be worth re-indexing: the RAG `fields` and
/// the `metadata`, as database columns, deduplicated.
fn watched_columns(rag: &RagMeta, model: &ModelMeta) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for name in rag.fields.iter().chain(rag.metadata.iter()) {
        let f = model.field(name).ok_or_else(|| Error::UnknownField {
            model: model.name.to_string(),
            field: (*name).to_string(),
        })?;
        if !out.iter().any(|c| c == f.column) {
            out.push(f.column.to_string());
        }
    }
    Ok(out)
}

/// The shared bricks: `CREATE EXTENSION vector` and the outbox table. Idempotent,
/// emitted once before any shadow table.
pub fn preamble() -> Vec<String> {
    vec![
        "CREATE EXTENSION IF NOT EXISTS vector".to_string(),
        format!(
            "CREATE TABLE IF NOT EXISTS {OUTBOX_TABLE} (\n    \
             id BIGSERIAL PRIMARY KEY,\n    \
             model TEXT NOT NULL,\n    \
             record_pk TEXT NOT NULL,\n    \
             op TEXT NOT NULL,\n    \
             attempts INTEGER NOT NULL DEFAULT 0,\n    \
             last_error TEXT,\n    \
             claimed_at TIMESTAMPTZ,\n    \
             enqueued_at TIMESTAMPTZ NOT NULL DEFAULT now(),\n    \
             updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\n)"
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS {OUTBOX_TABLE}_drain_idx \
             ON {OUTBOX_TABLE} (enqueued_at)"
        ),
    ]
}

/// The forward DDL for **every registered RAG model** — the preamble, then each
/// model's shadow table, indexes and triggers. The statements a project appends
/// to its initial `Migration` so RAG flows through `migrate deploy`.
///
/// `dim` is the embedding dimension for this pass (one embedder in Phase 2; a
/// per-embedder dimension is brick 4). PostgreSQL only — the first non-Postgres
/// RAG model returns the explicit error from [`migration_for`].
pub fn up_for_registered(dim: u32, backend: Backend) -> Result<Vec<String>> {
    let mut out = preamble();
    for rag in crate::rags() {
        let model = registry::model(rag.model)?;
        out.extend(migration_for(rag, model, dim, backend)?.up);
    }
    Ok(out)
}

/// The inverse of [`up_for_registered`]: drop every RAG model's objects, in
/// reverse. The extension and the shared outbox are left in place.
pub fn down_for_registered(dim: u32, backend: Backend) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for rag in crate::rags() {
        let model = registry::model(rag.model)?;
        out.extend(migration_for(rag, model, dim, backend)?.down);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::DistanceMetric;
    use dorm_core::meta::FieldMeta;
    use dorm_core::value::FieldType;

    const FIELDS: &[FieldMeta] = &[
        FieldMeta {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
        FieldMeta {
            name: "tenant",
            column: "tenant",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
        FieldMeta {
            name: "title",
            column: "title",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
        FieldMeta {
            name: "status",
            column: "status",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
    ];

    const TICKET: ModelMeta = ModelMeta {
        name: "Ticket",
        table: "tickets",
        fields: FIELDS,
        ordering: &[],
        tenant_scoped: true,
        unique_together: &[],
        checks: &[],
    };

    const RAG: RagMeta = RagMeta {
        model: "Ticket",
        fields: &["title"],
        template: "{title}",
        embedder: "default",
        chunk_size: 800,
        chunk_overlap: 100,
        metadata: &["status", "tenant"],
        distance: DistanceMetric::Cosine,
    };

    #[test]
    fn the_shadow_table_has_the_key_tenant_vector_and_metadata_columns() {
        let m = migration_for(&RAG, &TICKET, 384, Backend::Postgres).unwrap();
        let create = &m.up[0];
        assert!(create.contains("CREATE TABLE IF NOT EXISTS dorm_rag_tickets"));
        assert!(create.contains("record_pk BIGINT NOT NULL"));
        assert!(create.contains("tenant TEXT NOT NULL"));
        assert!(create.contains("embedding vector(384)"));
        assert!(create.contains("content_hash TEXT NOT NULL"));
        assert!(create.contains("status TEXT NOT NULL"));
        assert!(create.contains("PRIMARY KEY (record_pk, chunk_index)"));
        // `tenant` is in both tenant_scoped and metadata — it must appear once.
        assert_eq!(create.matches("tenant ").count(), 1, "{create}");
    }

    #[test]
    fn the_indexes_are_hnsw_with_the_metric_and_gin_for_fts() {
        let m = migration_for(&RAG, &TICKET, 384, Backend::Postgres).unwrap();
        assert!(m
            .up
            .iter()
            .any(|s| s.contains("USING hnsw (embedding vector_cosine_ops)")));
        assert!(m
            .up
            .iter()
            .any(|s| s.contains("USING gin (to_tsvector('simple', content))")));
    }

    #[test]
    fn the_update_trigger_only_fires_on_a_watched_column_change() {
        let m = migration_for(&RAG, &TICKET, 384, Backend::Postgres).unwrap();
        let upd =
            m.up.iter()
                .find(|s| s.contains("_upd AFTER UPDATE"))
                .unwrap();
        assert!(upd.contains("WHEN ("), "{upd}");
        assert!(
            upd.contains("OLD.title IS DISTINCT FROM NEW.title"),
            "{upd}"
        );
        assert!(
            upd.contains("OLD.status IS DISTINCT FROM NEW.status"),
            "{upd}"
        );
        // INSERT and DELETE have no WHEN.
        let ins =
            m.up.iter()
                .find(|s| s.contains("_ins AFTER INSERT"))
                .unwrap();
        assert!(!ins.contains("WHEN"), "{ins}");
    }

    #[test]
    fn the_trigger_enqueues_delete_for_old_and_upsert_for_new() {
        let m = migration_for(&RAG, &TICKET, 384, Backend::Postgres).unwrap();
        let func = m.up.iter().find(|s| s.contains("RETURNS trigger")).unwrap();
        assert!(func.contains("VALUES ('Ticket', OLD.id::text, 'delete')"));
        assert!(func.contains("VALUES ('Ticket', NEW.id::text, 'upsert')"));
    }

    #[test]
    fn the_down_drops_objects_but_keeps_the_extension_and_outbox() {
        let m = migration_for(&RAG, &TICKET, 384, Backend::Postgres).unwrap();
        assert!(m
            .down
            .iter()
            .any(|s| s == "DROP TABLE IF EXISTS dorm_rag_tickets"));
        assert!(m
            .down
            .iter()
            .any(|s| s.contains("DROP TRIGGER IF EXISTS dorm_rag_tickets_upd")));
        assert!(!m.down.iter().any(|s| s.contains("EXTENSION")));
        assert!(!m.down.iter().any(|s| s.contains(OUTBOX_TABLE)));
    }

    #[test]
    fn sqlite_is_an_explicit_error_not_a_silent_fallback() {
        let err = migration_for(&RAG, &TICKET, 384, Backend::Sqlite).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("pgvector"), "{msg}");
        assert!(msg.contains("sqlite-vec"), "{msg}");
    }

    #[test]
    fn the_preamble_creates_the_extension_and_the_outbox() {
        let p = preamble();
        assert_eq!(p[0], "CREATE EXTENSION IF NOT EXISTS vector");
        assert!(p[1].contains("CREATE TABLE IF NOT EXISTS dorm_rag_outbox"));
        assert!(p[1].contains("attempts INTEGER NOT NULL DEFAULT 0"));
        assert!(p[1].contains("last_error TEXT"));
    }
}
