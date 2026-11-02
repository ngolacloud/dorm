//! `dorm-rag` — optional RAG for DORM (ADR 0019).
//!
//! A model marked `#[derive(Rag)]` (or `@@rag(..)` in `dorm.schema`) is indexed
//! in pgvector and gains semantic and hybrid search with the **same filters and
//! the same tenant isolation** as any other DORM query. The crate is opt-in:
//! `dorm-core` never depends on it, so a project that does not use RAG sees an
//! unchanged core dependency graph and an unchanged `ModelMeta`.
//!
//! # The shape of it
//!
//! `#[derive(Rag)]` emits a [`RagMeta`] and submits it into a
//! distributed slice this crate owns — the RAG analogue of the `ModelMeta` slice
//! in `dorm-core`. The RagMeta is tied to its model by **name**, so the two
//! derives stay independent and `ModelMeta` does not change.
//!
//! This is **brick 1** of ADR 0019: the declaration (RagMeta, the derive, the
//! schema attribute and validation). Storage/migrations, the worker, embedders,
//! search and the CLI land in later bricks.

#![forbid(unsafe_code)]

pub mod chunk;
pub mod embedder;
pub mod meta;
pub mod migrations;
pub mod search;
pub mod status;
pub mod worker;

pub use embedder::{default_embedder, install_embedder, Embedder};
pub use meta::{DistanceMetric, RagMeta};
pub use search::{semantic_search, RagQuery, RagQuerySetExt, Scored, ScoredRow, SearchOptions};
pub use status::{reindex, status, ModelRagStatus};
pub use worker::{run_once, WorkerConfig, WorkerStats};

// Re-exported so `#[derive(Rag)]`-generated code can name them without the
// model's crate depending on them directly — the same move `dorm-core` makes for
// the `Model` derive.
pub use dorm_derive::Rag;
pub use inventory;

use dorm_core::meta::ModelMeta;
use dorm_core::registry::{self, Issue, Severity};
use dorm_core::value::FieldType;

/// Every registered RAG configuration, in link order.
pub fn rags() -> impl Iterator<Item = &'static RagMeta> {
    inventory::iter::<RagMeta>.into_iter()
}

/// Validate the registered RAG configurations against the registered models.
///
/// The runtime half of the ADR 0019 validation: it cross-checks each
/// [`RagMeta`] against the live [`ModelMeta`] a hand-written model submitted, so
/// a model written in Rust (bypassing `dorm.schema`, where the schema-level check
/// lives) is covered too. Reuses `dorm-core`'s [`Issue`]/[`Severity`] so the
/// generated `validate` binary can merge these findings with the registry's.
///
/// Pure and read-only, like `registry::validate`. An empty result means every
/// RAG model is consistent with its model.
pub fn validate() -> Vec<Issue> {
    let models: Vec<&'static ModelMeta> = registry::models().collect();
    let rags: Vec<&'static RagMeta> = rags().collect();
    validate_against(&rags, &models)
}

/// The pure core of [`validate()`], taking its inputs explicitly so it can be
/// unit-tested without a linked registry.
///
/// Per RAG model it checks: the named model is registered; every `fields` entry
/// is a real column **of type `Text`** (the only thing an embedder reads); every
/// `metadata` entry is a real column (so it can become a filterable shadow
/// column); and the model has a primary key (so a chunk can be tied to its row).
pub fn validate_against(rags: &[&RagMeta], models: &[&ModelMeta]) -> Vec<Issue> {
    let mut issues = Vec::new();
    for rag in rags {
        let Some(model) = models.iter().find(|m| m.name == rag.model) else {
            issues.push(Issue {
                severity: Severity::Error,
                model: rag.model.to_string(),
                message: "has `#[rag]`/`@@rag` but is not a registered model — is the \
                          model crate linked, and does the name match?"
                    .to_string(),
            });
            continue;
        };

        if model.primary_key().is_none() {
            issues.push(Issue {
                severity: Severity::Error,
                model: rag.model.to_string(),
                message: "is indexed for RAG but has no primary key, so a chunk cannot be \
                          tied back to its row"
                    .to_string(),
            });
        }

        if rag.fields.is_empty() {
            issues.push(Issue {
                severity: Severity::Error,
                model: rag.model.to_string(),
                message: "declares `#[rag(fields = [])]` — RAG needs at least one text field \
                          to embed"
                    .to_string(),
            });
        }

        for field in rag.fields {
            match model.field(field) {
                None => issues.push(Issue {
                    severity: Severity::Error,
                    model: rag.model.to_string(),
                    message: format!("RAG field `{field}` is not a column of the model"),
                }),
                Some(f) if f.ty != FieldType::Text => issues.push(Issue {
                    severity: Severity::Error,
                    model: rag.model.to_string(),
                    message: format!(
                        "RAG field `{field}` is `{}`, but only `text` fields can be embedded",
                        f.ty.describe()
                    ),
                }),
                Some(_) => {}
            }
        }

        for field in rag.metadata {
            if model.field(field).is_none() {
                issues.push(Issue {
                    severity: Severity::Error,
                    model: rag.model.to_string(),
                    message: format!("RAG metadata field `{field}` is not a column of the model"),
                });
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use dorm_core::meta::FieldMeta;

    const TICKET_FIELDS: &[FieldMeta] = &[
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
            name: "priority",
            column: "priority",
            ty: FieldType::I32,
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

    fn ticket_meta() -> ModelMeta {
        ModelMeta {
            name: "Ticket",
            table: "tickets",
            fields: TICKET_FIELDS,
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[],
            checks: &[],
        }
    }

    const GOOD: RagMeta = RagMeta {
        model: "Ticket",
        fields: &["title"],
        template: "{title}",
        embedder: "default",
        chunk_size: 800,
        chunk_overlap: 100,
        metadata: &["priority"],
        distance: DistanceMetric::Cosine,
    };

    #[test]
    fn a_well_formed_rag_model_passes() {
        let m = ticket_meta();
        assert!(validate_against(&[&GOOD], &[&m]).is_empty());
    }

    #[test]
    fn a_rag_on_an_unregistered_model_is_an_error() {
        let orphan = RagMeta {
            model: "Nope",
            ..GOOD
        };
        let issues = validate_against(&[&orphan], &[]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Error);
        assert!(issues[0].message.contains("not a registered model"));
    }

    #[test]
    fn a_non_text_rag_field_is_rejected() {
        let m = ticket_meta();
        let bad = RagMeta {
            fields: &["priority"],
            ..GOOD
        };
        let issues = validate_against(&[&bad], &[&m]);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("only `text` fields can be embedded")));
    }

    #[test]
    fn a_missing_field_and_a_missing_metadata_column_are_both_named() {
        let m = ticket_meta();
        let bad = RagMeta {
            fields: &["titel"],
            metadata: &["staus"],
            ..GOOD
        };
        let issues = validate_against(&[&bad], &[&m]);
        assert!(issues.iter().any(|i| i.message.contains("`titel`")));
        assert!(issues.iter().any(|i| i.message.contains("`staus`")));
    }

    const KEYLESS_FIELDS: &[FieldMeta] = &[FieldMeta {
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
    }];

    #[test]
    fn a_rag_model_without_a_primary_key_is_an_error() {
        let keyless = ModelMeta {
            name: "Ticket",
            table: "tickets",
            fields: KEYLESS_FIELDS,
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[],
            checks: &[],
        };
        let issues = validate_against(&[&GOOD], &[&keyless]);
        assert!(issues.iter().any(|i| i.message.contains("no primary key")));
    }
}
