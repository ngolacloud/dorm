//! Live proof of ADR 0019 brick 2 against a real pgvector database.
//!
//! Gated on `DELONIX_ORM_DSN` (or `PAAS_DATABASE_URL`), like every other live
//! suite: with no DSN it prints a skip and returns, so `cargo test` is green on a
//! machine with no database. To run it:
//!
//! ```bash
//! docker compose -f docker-compose.rag.yml up -d
//! DELONIX_ORM_DSN='host=localhost port=5433 user=dorm password=dorm dbname=dorm_rag' \
//!   cargo test -p dorm-rag --test migration_live -- --nocapture
//! ```
//!
//! It proves the generated DDL *applies* (extension, shadow table, HNSW + GIN
//! indexes, triggers), that the triggers *enqueue* on INSERT/UPDATE/DELETE
//! through the outbox, and that `down` removes the objects.

use dorm_core::db;
use dorm_core::model::Model;
use dorm_core::value::Value;
use dorm_derive::Model;
use dorm_migrations::ddl::{self, Backend};
use dorm_rag::Rag;

#[derive(Model, Rag, Debug, Clone)]
#[model(table = "tickets", tenant_scoped)]
#[rag(fields = ["title"], embedder = "default", metadata = ["status", "tenant"])]
#[allow(dead_code)]
struct Ticket {
    #[field(primary_key)]
    id: i64,
    #[field(column_type = "text")]
    tenant: String,
    title: String,
    status: String,
}

const DIM: u32 = 384; // all-minilm

async fn run_all(statements: &[String]) -> dorm_core::Result<()> {
    db::run_ddl(&statements.join(";\n")).await
}

async fn scalar_i64(sql: &str) -> i64 {
    let rows = db::fetch(sql, &[]).await.expect("query");
    rows[0].try_get::<i64>(0).expect("i64")
}

#[tokio::test]
async fn rag_migration_applies_and_triggers_enqueue_on_pgvector() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_DSN").or_else(|_| std::env::var("PAAS_DATABASE_URL"))
    else {
        eprintln!("skipped: set DELONIX_ORM_DSN to a throwaway pgvector to run this");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);

    // A clean slate: drop the model table (CASCADE takes its triggers) and the
    // RAG objects from any previous run, then rebuild.
    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .expect("drop model table");
    let _ =
        run_all(&dorm_rag::migrations::down_for_registered(DIM, Backend::Postgres).unwrap()).await;

    // 1. The model table itself (ordinary migrator DDL), then the RAG DDL.
    let model_ddl = ddl::schema_for(&[Ticket::meta()]).expect("model ddl");
    db::run_ddl(&model_ddl).await.expect("create model table");
    let up = dorm_rag::migrations::up_for_registered(DIM, Backend::Postgres).unwrap();
    run_all(&up).await.expect("apply RAG up");

    // 2. The objects exist.
    assert_eq!(
        scalar_i64("SELECT count(*) FROM pg_extension WHERE extname='vector'").await,
        1,
        "the vector extension was created"
    );
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM information_schema.tables WHERE table_name='dorm_rag_tickets'"
        )
        .await,
        1,
        "the shadow table was created"
    );
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM pg_indexes \
             WHERE indexname IN ('dorm_rag_tickets_embedding_idx','dorm_rag_tickets_fts_idx')"
        )
        .await,
        2,
        "the HNSW and GIN indexes were created"
    );
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM pg_trigger \
             WHERE tgname IN ('tickets_ins','tickets_upd','tickets_del') \
             AND NOT tgisinternal"
        )
        .await,
        0,
        "trigger names are prefixed by the SHADOW table, not the model table"
    );
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM pg_trigger \
             WHERE tgname IN ('dorm_rag_tickets_ins','dorm_rag_tickets_upd','dorm_rag_tickets_del') \
             AND NOT tgisinternal"
        )
        .await,
        3,
        "the three enqueue triggers were created on the model table"
    );

    // 3. The triggers enqueue. Start from an empty outbox.
    db::run("TRUNCATE dorm_rag_outbox", &[])
        .await
        .expect("truncate");

    db::run(
        "INSERT INTO tickets (tenant, title, status) VALUES ($1, $2, $3)",
        &[
            Value::Text("acme".into()),
            Value::Text("email broken after migration".into()),
            Value::Text("open".into()),
        ],
    )
    .await
    .expect("insert ticket");
    assert_eq!(
        scalar_i64("SELECT count(*) FROM dorm_rag_outbox WHERE op='upsert'").await,
        1,
        "INSERT enqueued one upsert"
    );

    // An UPDATE of a watched column (title) enqueues again.
    db::run("UPDATE tickets SET title = 'email still broken'", &[])
        .await
        .expect("update title");
    assert_eq!(
        scalar_i64("SELECT count(*) FROM dorm_rag_outbox WHERE op='upsert'").await,
        2,
        "UPDATE of a watched column enqueued a second upsert"
    );

    // A DELETE enqueues a delete carrying the old key.
    db::run("DELETE FROM tickets", &[]).await.expect("delete");
    assert_eq!(
        scalar_i64("SELECT count(*) FROM dorm_rag_outbox WHERE op='delete'").await,
        1,
        "DELETE enqueued one delete"
    );

    // 4. `down` removes the shadow table (and leaves the extension/outbox).
    let down = dorm_rag::migrations::down_for_registered(DIM, Backend::Postgres).unwrap();
    run_all(&down).await.expect("apply RAG down");
    assert_eq!(
        scalar_i64(
            "SELECT count(*) FROM information_schema.tables WHERE table_name='dorm_rag_tickets'"
        )
        .await,
        0,
        "down dropped the shadow table"
    );
    assert_eq!(
        scalar_i64("SELECT count(*) FROM pg_extension WHERE extname='vector'").await,
        1,
        "down left the shared extension in place"
    );

    // Tidy up so a re-run starts clean.
    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .ok();
}
