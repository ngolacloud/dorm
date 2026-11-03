//! Live end-to-end proof of the RAG worker (ADR 0019, brick 3) against pgvector
//! and a real Ollama embedding model — the validation the whole stack is for.
//!
//! Gated on the `embed-ollama` feature **and** a DSN; skips otherwise. Run it:
//!
//! ```bash
//! docker compose -f docker-compose.rag.yml up -d
//! ollama pull all-minilm
//! DELONIX_ORM_DSN='host=localhost port=5433 user=dorm password=dorm dbname=dorm_rag' \
//!   cargo test -p dorm-rag --features embed-ollama --test worker_live -- --nocapture
//! ```
//!
//! It proves: the worker drains the outbox and embeds (upserts chunks with the
//! model id), a semantic nearest-neighbour query returns the *right* record (so
//! the vectors are meaningful), editing a record re-embeds only it, and deleting
//! a record removes its chunks.

#![cfg(feature = "embed-ollama")]

use dorm_core::db;
use dorm_core::model::Model;
use dorm_core::value::Value;
use dorm_derive::Model;
use dorm_migrations::ddl::{self, Backend};
use dorm_rag::embedder::{Embedder, OllamaEmbedder};
use dorm_rag::worker::{run_once, WorkerConfig};
use dorm_rag::Rag;

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

async fn scalar_i64(sql: &str, params: &[Value]) -> i64 {
    db::fetch(sql, params).await.expect("query")[0]
        .try_get::<i64>(0)
        .expect("i64")
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
    .expect("insert ticket");
}

#[tokio::test]
async fn the_worker_indexes_records_and_semantic_search_finds_the_right_one() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_DSN").or_else(|_| std::env::var("PAAS_DATABASE_URL"))
    else {
        eprintln!("skipped: set DELONIX_ORM_DSN to a pgvector database");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    let embedder = match OllamaEmbedder::from_env().await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipped: Ollama not reachable ({e}); `ollama pull all-minilm` and retry");
            return;
        }
    };
    let dim = embedder.dimension();
    assert!(dim > 0, "the embedder reported a dimension");
    eprintln!("embedder {} dim {dim}", embedder.model_id());

    // Clean slate, then the model table and the RAG objects sized to this model.
    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .expect("drop");
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
    // The outbox is shared across models/tests; start this run from empty.
    db::run("TRUNCATE dorm_rag_outbox", &[])
        .await
        .expect("truncate outbox");

    // Three tickets with clearly different meaning.
    insert(
        "acme",
        "Não consigo aceder ao email",
        "Depois da migração o webmail recusa a minha palavra-passe e não abre a caixa de correio.",
        "open",
    )
    .await;
    insert(
        "acme",
        "Fatura cobrada em duplicado",
        "Fui cobrado duas vezes no cartão de crédito este mês.",
        "open",
    )
    .await;
    insert(
        "acme",
        "Aumento de quota de disco",
        "Preciso de mais espaço de armazenamento no servidor de ficheiros.",
        "closed",
    )
    .await;
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM dorm_rag_outbox WHERE op='upsert'",
            &[]
        )
        .await,
        3,
        "three inserts enqueued three upserts"
    );

    // Drain: the worker embeds all three.
    let stats = run_once(&embedder, &WorkerConfig::default())
        .await
        .expect("worker run");
    assert_eq!(stats.upserts, 3, "indexed three records: {stats:?}");
    assert_eq!(stats.failed, 0, "no failures: {stats:?}");
    assert_eq!(
        scalar_i64("SELECT count(*) FROM dorm_rag_outbox", &[]).await,
        0,
        "the outbox drained"
    );
    assert!(
        scalar_i64("SELECT count(*) FROM dorm_rag_tickets", &[]).await >= 3,
        "chunks were written"
    );
    assert_eq!(
        scalar_i64(
            "SELECT count(DISTINCT embedder) FROM dorm_rag_tickets WHERE embedder = $1",
            &[Value::Text(embedder.model_id().into())],
        )
        .await,
        1,
        "chunks record the embedder that wrote them"
    );

    // The real test: a natural-language query lands on the email ticket.
    let qvec = embedder
        .embed(&["utilizador perdeu o acesso à caixa de correio".to_string()])
        .await
        .expect("embed query");
    let literal = format!(
        "[{}]",
        qvec[0]
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let nearest: String = db::fetch(
        "SELECT t.title FROM dorm_rag_tickets r \
         JOIN tickets t ON t.id = r.record_pk \
         ORDER BY r.embedding <=> $1::text::vector LIMIT 1",
        &[Value::Text(literal)],
    )
    .await
    .expect("knn")[0]
        .try_get::<String>(0)
        .expect("title");
    assert_eq!(
        nearest, "Não consigo aceder ao email",
        "semantic search returned the right record"
    );

    // Incremental: edit one record; only its chunk is re-embedded, with new text.
    db::run(
        "UPDATE tickets SET body = 'A conta de email voltou a funcionar após repor a palavra-passe.' \
         WHERE title = 'Não consigo aceder ao email'",
        &[],
    )
    .await
    .expect("update");
    let stats = run_once(&embedder, &WorkerConfig::default())
        .await
        .expect("worker run 2");
    assert_eq!(
        stats.upserts, 1,
        "only the edited record re-embedded: {stats:?}"
    );
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM dorm_rag_tickets WHERE content LIKE '%voltou a funcionar%'",
            &[],
        )
        .await,
        1,
        "the chunk holds the new text"
    );

    // Delete: the record's chunks go.
    db::run("DELETE FROM tickets WHERE status = 'closed'", &[])
        .await
        .expect("delete");
    let before = scalar_i64(
        "SELECT count(DISTINCT record_pk) FROM dorm_rag_tickets",
        &[],
    )
    .await;
    let stats = run_once(&embedder, &WorkerConfig::default())
        .await
        .expect("worker run 3");
    assert_eq!(stats.deletes, 1, "one delete processed: {stats:?}");
    let after = scalar_i64(
        "SELECT count(DISTINCT record_pk) FROM dorm_rag_tickets",
        &[],
    )
    .await;
    assert_eq!(
        after,
        before - 1,
        "the deleted record's chunks were removed"
    );

    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .ok();
}
