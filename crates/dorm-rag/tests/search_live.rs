//! Live proof of RAG search (ADR 0019, brick 5) — above all **acceptance #4**:
//! a `search()` on a `tenant_scoped` model never returns another tenant's rows,
//! because the tenant filter is pre-applied through the one compiler.
//!
//! Gated on `embed-ollama` + a DSN; skips otherwise.
//!
//! ```bash
//! DELONIX_ORM_DSN='host=localhost port=5433 user=dorm password=dorm dbname=dorm_rag' \
//!   cargo test -p dorm-rag --features embed-ollama --test search_live -- --nocapture
//! ```

#![cfg(feature = "embed-ollama")]

use dorm_core::db;
use dorm_core::model::Model;
use dorm_core::value::Value;
use dorm_derive::Model;
use dorm_migrations::ddl::{self, Backend};
use dorm_rag::embedder::OllamaEmbedder;
use dorm_rag::worker::{run_once, WorkerConfig};
use dorm_rag::{install_embedder, semantic_search, Rag, RagQuerySetExt, SearchOptions};

#[derive(Model, Rag, Debug, Clone)]
#[model(table = "tickets", tenant_scoped)]
#[rag(fields = ["title", "body"], template = "{title}\n\n{body}", embedder = "all-minilm", metadata = ["status", "tenant"])]
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

async fn insert(tenant: &str, title: &str, body: &str, status: &str) {
    db::run(
        "INSERT INTO tickets (tenant, title, body, status) VALUES ($1, $2, $3, $4)",
        &[
            Value::Text(tenant.into()),
            Value::Text(title.into()),
            Value::Text(body.into()),
            Value::Text(status.into()),
        ],
    )
    .await
    .expect("insert");
}

#[tokio::test]
async fn search_is_tenant_isolated_and_semantically_relevant() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_DSN").or_else(|_| std::env::var("PAAS_DATABASE_URL"))
    else {
        eprintln!("skipped: set DELONIX_ORM_DSN to a pgvector database");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    let embedder = match OllamaEmbedder::from_env().await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipped: Ollama not reachable ({e})");
            return;
        }
    };
    let dim = dorm_rag::Embedder::dimension(&embedder);

    // Rebuild the schema, then install the embedder as the process default so the
    // no-argument `.search()` and `semantic_search` can use it.
    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .ok();
    let _ = db::run_ddl(
        &dorm_rag::migrations::down_for_registered(dim, Backend::Postgres)
            .unwrap()
            .join(";\n"),
    )
    .await;
    db::run_ddl(&ddl::schema_for(&[Ticket::meta()]).unwrap())
        .await
        .expect("model table");
    db::run_ddl(
        &dorm_rag::migrations::up_for_registered(dim, Backend::Postgres)
            .unwrap()
            .join(";\n"),
    )
    .await
    .expect("rag up");
    db::run("TRUNCATE dorm_rag_outbox", &[])
        .await
        .expect("truncate");
    install_embedder(embedder);

    // Two tenants, each with an email-problem ticket and an unrelated one.
    insert(
        "acme",
        "Não consigo aceder ao email",
        "O webmail recusa a palavra-passe desde a migração.",
        "open",
    )
    .await;
    insert(
        "acme",
        "Fatura cobrada em duplicado",
        "Pagamento em dobro no cartão este mês.",
        "open",
    )
    .await;
    insert(
        "globex",
        "Falha de login no email corporativo",
        "A caixa de correio não autentica desde ontem.",
        "open",
    )
    .await;
    insert(
        "globex",
        "Pedido de férias",
        "Quero marcar férias para dezembro.",
        "closed",
    )
    .await;

    // Index all four.
    let stats = run_once(
        &dorm_rag::embedder::OllamaEmbedder::from_env()
            .await
            .unwrap(),
        &WorkerConfig::default(),
    )
    .await
    .expect("worker");
    assert_eq!(stats.upserts, 4, "indexed four records: {stats:?}");

    // TYPED, tenant-scoped: ask acme about the email problem.
    let hits = Ticket::objects()
        .filter(ticket::tenant.eq("acme"))
        .semantic("o webmail recusa a palavra-passe e não abre a caixa de correio")
        .limit(10)
        .search()
        .await
        .expect("typed search");
    assert!(!hits.is_empty(), "acme has matching tickets");
    // ACCEPTANCE #4: never another tenant's rows.
    for h in &hits {
        assert_eq!(
            h.item.tenant, "acme",
            "tenant isolation breached: {:?}",
            h.item.title
        );
    }
    // The most relevant acme ticket is the email one — and the billing query
    // flips the order, so ranking is semantic, not incidental.
    assert_eq!(
        hits[0].item.title, "Não consigo aceder ao email",
        "the email query ranks the email ticket first"
    );
    let billing = Ticket::objects()
        .filter(ticket::tenant.eq("acme"))
        .semantic("fui cobrado duas vezes no cartão de crédito")
        .search()
        .await
        .expect("billing search");
    assert_eq!(
        billing[0].item.title, "Fatura cobrada em duplicado",
        "the billing query ranks the billing ticket first"
    );

    // The symmetric check: globex's email ticket is reachable only under globex.
    let globex = Ticket::objects()
        .filter(ticket::tenant.eq("globex"))
        .semantic("não consigo entrar no email")
        .search()
        .await
        .expect("globex search");
    assert!(globex.iter().all(|h| h.item.tenant == "globex"));
    assert!(globex
        .iter()
        .any(|h| h.item.title == "Falha de login no email corporativo"));

    // HYBRID + min_score still respects the tenant.
    let hybrid = Ticket::objects()
        .filter(ticket::tenant.eq("acme"))
        .semantic("email")
        .hybrid(0.7)
        .limit(5)
        .search()
        .await
        .expect("hybrid search");
    assert!(hybrid.iter().all(|h| h.item.tenant == "acme"));

    // DYNAMIC path: same isolation, resolved through the registry.
    let rows = semantic_search(
        "Ticket",
        "problema de email",
        &[("tenant".to_string(), Value::Text("globex".into()))],
        &SearchOptions::default(),
    )
    .await
    .expect("dynamic search");
    assert!(!rows.is_empty());
    // Every returned pk belongs to globex — check against the table.
    for r in &rows {
        let owner: String = db::fetch(
            "SELECT tenant FROM tickets WHERE id::text = $1",
            &[Value::Text(r.pk.clone())],
        )
        .await
        .expect("owner")[0]
            .try_get::<String>(0)
            .expect("tenant");
        assert_eq!(owner, "globex", "dynamic search crossed tenants");
    }

    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .ok();
}
