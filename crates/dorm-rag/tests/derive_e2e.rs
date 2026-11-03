//! End-to-end proof of ADR 0019 brick 1: a real `#[derive(Model, Rag)]` struct
//! compiles, its `RagMeta` is emitted and gathered by `inventory`, and
//! `dorm_rag::validate()` reads it against the live registry and passes.
//!
//! This is the Rust-written path (no `dorm.schema`), so it also exercises the
//! runtime-level validation that the schema-level check cannot.

use dorm_core::prelude::*;
use dorm_core::value::FieldType;
// The derive macro `Model` (a macro-namespace item) coexists with the `Model`
// trait the prelude brings in — both are needed, as in generated `models.rs`.
use dorm_derive::Model;
use dorm_rag::{DistanceMetric, Rag};

#[derive(Model, Rag, Debug, Clone)]
#[model(table = "tickets", tenant_scoped)]
#[rag(
    fields = ["title", "body"],
    template = "{title}\n\n{body}",
    embedder = "default",
    chunk_size = 800,
    chunk_overlap = 100,
    metadata = ["status"],
    distance = "cosine"
)]
#[allow(dead_code)]
struct Ticket {
    #[field(primary_key)]
    id: i64,
    #[field(column_type = "text")]
    tenant: String,
    title: String,
    body: String,
    status: String,
}

#[test]
fn the_rag_derive_emits_a_ragmeta_that_inventory_gathers() {
    let ticket = dorm_rag::rags()
        .find(|r| r.model == "Ticket")
        .expect("the Ticket RagMeta was submitted into the inventory slice");

    assert_eq!(ticket.fields, ["title", "body"]);
    assert_eq!(ticket.template, "{title}\n\n{body}");
    assert_eq!(ticket.embedder, "default");
    assert_eq!(ticket.chunk_size, 800);
    assert_eq!(ticket.chunk_overlap, 100);
    assert_eq!(ticket.metadata, ["status"]);
    assert_eq!(ticket.distance, DistanceMetric::Cosine);
}

#[test]
fn the_model_meta_is_untouched_by_the_rag_derive() {
    // The point of a separate derive: `ModelMeta` gains nothing. The model looks
    // exactly as it would without `#[derive(Rag)]`.
    let meta = Ticket::meta();
    assert_eq!(meta.name, "Ticket");
    assert_eq!(meta.table, "tickets");
    assert!(meta.tenant_scoped);
    assert!(meta.primary_key().is_some());
    assert_eq!(meta.field("title").unwrap().ty, FieldType::Text);
}

#[test]
fn validate_sees_the_live_model_and_passes() {
    // Runs against the real registry: Model derive submitted the ModelMeta,
    // Rag derive submitted the RagMeta, and the two agree.
    let issues = dorm_rag::validate();
    let about_ticket: Vec<_> = issues.iter().filter(|i| i.model == "Ticket").collect();
    assert!(
        about_ticket.is_empty(),
        "a well-formed RAG model should validate clean: {about_ticket:?}"
    );
}
