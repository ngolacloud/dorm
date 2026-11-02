//! Print the reviewable RAG DDL for a sample model — what `migrate make`/`deploy`
//! would write for a `@@rag` model (ADR 0019, brick 2). No database needed:
//!
//! ```bash
//! cargo run -p dorm-rag --example print_rag_ddl
//! ```

use dorm_core::model::Model;
use dorm_derive::Model;
use dorm_migrations::ddl::Backend;
use dorm_rag::Rag;

#[derive(Model, Rag, Debug, Clone)]
#[model(table = "tickets", tenant_scoped)]
#[rag(fields = ["title", "body"], template = "{title}\n\n{body}", embedder = "default", metadata = ["status", "tenant"])]
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

fn main() {
    let rag = dorm_rag::rags().find(|r| r.model == "Ticket").unwrap();
    let m = dorm_rag::migrations::migration_for(rag, Ticket::meta(), 384, Backend::Postgres)
        .expect("postgres");

    println!("-- ============ preamble (once) ============");
    for s in dorm_rag::migrations::preamble() {
        println!("{s};\n");
    }
    println!("-- ============ up: dorm_rag_tickets ============");
    for s in &m.up {
        println!("{s};\n");
    }
    println!("-- ============ down ============");
    for s in &m.down {
        println!("{s};");
    }
}
