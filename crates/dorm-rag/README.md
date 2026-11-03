# dorm-rag

Optional RAG for [DORM](https://github.com/ngolacloud/dorm). A model marked
`#[derive(Rag)]` (or `@@rag(..)` in `dorm.schema`) is indexed in pgvector and
gains semantic and hybrid search with the **same filters and the same tenant
isolation** as any other DORM query.

It is strictly opt-in: `dorm-core` never depends on `dorm-rag`, so a project that
does not use RAG sees an unchanged core dependency graph and an unchanged
`ModelMeta`. See [ADR 0019](https://github.com/ngolacloud/dorm/blob/main/docs/adr/0019-rag.md).

```rust
use dorm_core::prelude::*;
use dorm_rag::Rag;

#[derive(Model, Rag, Debug, Clone)]
#[model(table = "tickets", tenant_scoped)]
#[rag(fields = ["title", "body"], embedder = "default", metadata = ["status"])]
pub struct Ticket {
    #[field(primary_key)]
    pub id: i64,
    pub title: String,
    pub body: String,
    pub status: String,
}
```

This crate is being delivered in bricks (ADR 0019). Brick 1 — the declaration
(`RagMeta`, the derive, the `@@rag` schema attribute and validation) — is what is
present today. Storage/migrations, the worker, embedders, search and the CLI
follow.
