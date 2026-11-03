//! Live end-to-end proof of the MCP server (ADR 0020, brick 1) — acceptance #5
//! (the `semantic_search → query` flow works) and the tenant forced from the
//! server config, through the JSON-RPC surface.
//!
//! Gated at runtime on a DSN and a reachable Ollama; skips otherwise.
//!
//! ```bash
//! DELONIX_ORM_DSN='host=localhost port=5433 user=dorm password=dorm dbname=dorm_rag' \
//!   cargo test -p dorm-mcp --test mcp_live -- --nocapture
//! ```

use dorm_core::db;
use dorm_core::model::Model;
use dorm_core::value::Value;
use dorm_derive::Model;
use dorm_mcp::{handle_message, ServerConfig};
use dorm_migrations::ddl::{self, Backend};
use dorm_rag::embedder::OllamaEmbedder;
use dorm_rag::worker::{run_once, WorkerConfig};
use dorm_rag::Rag;
use serde_json::{json, Value as Json};

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

/// Call a tool, parse the JSON payload out of the `content` text (skipping the
/// untrusted-data note).
async fn call_tool(cfg: &ServerConfig, name: &str, args: Json) -> Json {
    let line = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    })
    .to_string();
    let out = handle_message(cfg, &line).await.expect("response");
    let v: Json = serde_json::from_str(&out).unwrap();
    assert_eq!(v["result"]["isError"], json!(false), "tool errored: {out}");
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let start = text.find('{').expect("json payload");
    serde_json::from_str(&text[start..]).expect("payload json")
}

async fn owner_of(pk: &str) -> String {
    db::fetch(
        "SELECT tenant FROM tickets WHERE id::text = $1",
        &[Value::Text(pk.into())],
    )
    .await
    .expect("owner")[0]
        .try_get::<String>(0)
        .expect("tenant")
}

#[tokio::test]
async fn the_mcp_server_speaks_the_protocol_and_forces_the_tenant() {
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
    dorm_rag::install_embedder(embedder);

    // An email ticket in each tenant — so missing isolation would leak globex.
    insert(
        "acme",
        "Não consigo aceder ao email",
        "O webmail recusa a palavra-passe.",
        "open",
    )
    .await;
    insert(
        "globex",
        "Falha de login no email",
        "A caixa de correio não autentica.",
        "open",
    )
    .await;
    let stats = run_once(
        &OllamaEmbedder::from_env().await.unwrap(),
        &WorkerConfig::default(),
    )
    .await
    .expect("worker");
    assert_eq!(stats.upserts, 2);

    // The server is scoped to acme by config — not by any tool argument.
    let cfg = ServerConfig::from_args(&["--tenant".into(), "acme".into()]);

    // initialize handshake.
    let init = handle_message(
        &cfg,
        &json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} }).to_string(),
    )
    .await
    .expect("init response");
    assert!(
        init.contains("\"protocolVersion\":\"2024-11-05\""),
        "{init}"
    );

    // tools/list advertises the read tools.
    let tl = handle_message(
        &cfg,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string(),
    )
    .await
    .unwrap();
    assert!(
        tl.contains("semantic_search") && tl.contains("\"query\""),
        "{tl}"
    );

    // semantic_search with NO tenant in the arguments — the server forces acme.
    let hits = call_tool(
        &cfg,
        "semantic_search",
        json!({ "model": "Ticket", "query": "não consigo entrar no meu email" }),
    )
    .await;
    let arr = hits["hits"].as_array().expect("hits array");
    assert!(!arr.is_empty(), "semantic_search found something");
    for h in arr {
        let pk = h["pk"].as_str().unwrap();
        assert_eq!(
            owner_of(pk).await,
            "acme",
            "MCP semantic_search crossed tenants"
        );
    }

    // query is tenant-scoped too.
    let rows = call_tool(
        &cfg,
        "query",
        json!({ "model": "Ticket", "filters": { "status": "open" } }),
    )
    .await;
    let rows = rows["rows"].as_array().expect("rows");
    assert!(!rows.is_empty());
    for r in rows {
        assert_eq!(r["tenant"], json!("acme"), "MCP query crossed tenants");
    }

    // list_models shows Ticket with RAG.
    let models = call_tool(&cfg, "list_models", json!({})).await;
    let found = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["name"] == json!("Ticket") && m["has_rag"] == json!(true));
    assert!(found, "list_models reports Ticket with RAG");

    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .ok();
}
