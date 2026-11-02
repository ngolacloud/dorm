//! Live proof of the MCP write tools (ADR 0020, brick 2): `dry_run` previews
//! without executing, the tenant is forced on insert, an unfiltered delete is
//! refused, and the row cap bites. Needs a DSN; skips otherwise (no Ollama
//! needed — writes don't embed).
//!
//! ```bash
//! DELONIX_ORM_DSN='host=localhost port=5433 user=dorm password=dorm dbname=dorm_rag' \
//!   cargo test -p dorm-mcp --test mcp_write_live -- --nocapture
//! ```

use dorm_core::db;
use dorm_core::model::Model;
use dorm_core::value::Value;
use dorm_derive::Model;
use dorm_mcp::{handle_message, ServerConfig};
use dorm_migrations::ddl;
use dorm_rag::Rag;
use serde_json::{json, Value as Json};

#[derive(Model, Rag, Debug, Clone)]
#[model(table = "tickets", tenant_scoped)]
#[rag(fields = ["title"], metadata = ["status", "tenant"])]
#[allow(dead_code)]
struct Ticket {
    #[field(primary_key)]
    id: i64,
    #[field(column_type = "text")]
    tenant: String,
    title: String,
    status: String,
}

/// Call a tool, returning the parsed MCP result (content payload + isError).
async fn call(cfg: &ServerConfig, name: &str, args: Json) -> (bool, Json) {
    let line = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args }
    })
    .to_string();
    let out = handle_message(cfg, &line).await.expect("response");
    let v: Json = serde_json::from_str(&out).unwrap();
    let is_error = v["result"]["isError"].as_bool().unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let payload = text
        .find('{')
        .and_then(|i| serde_json::from_str(&text[i..]).ok())
        .unwrap_or(json!({ "message": text }));
    (is_error, payload)
}

async fn scalar_i64(sql: &str, params: &[Value]) -> i64 {
    db::fetch(sql, params).await.expect("q")[0]
        .try_get::<i64>(0)
        .expect("i64")
}

#[tokio::test]
async fn the_write_tools_preview_force_the_tenant_and_refuse_unfiltered() {
    let Ok(dsn) = std::env::var("DELONIX_ORM_DSN").or_else(|_| std::env::var("PAAS_DATABASE_URL"))
    else {
        eprintln!("skipped: set DELONIX_ORM_DSN");
        return;
    };
    std::env::set_var(db::DSN_ENV, &dsn);
    // handle_message does not flip the guard (serve does); writes need it off.
    db::set_read_only(false);

    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .ok();
    db::run_ddl(&ddl::schema_for(&[Ticket::meta()]).unwrap())
        .await
        .expect("model table");

    let cfg = ServerConfig {
        writable: true,
        tenant: Some("acme".into()),
        max_write_rows: 5,
        ..ServerConfig::default()
    };

    // dry_run previews and writes nothing.
    let (err, prev) = call(
        &cfg,
        "insert",
        json!({ "model": "Ticket", "values": { "title": "A", "status": "open" }, "dry_run": true }),
    )
    .await;
    assert!(!err);
    assert_eq!(prev["dry_run"], json!(true));
    let sql = prev["sql"].as_str().unwrap();
    assert!(
        sql.contains("INSERT INTO") && sql.contains("tickets"),
        "{sql}"
    );
    assert_eq!(
        scalar_i64("SELECT count(*) FROM tickets", &[]).await,
        0,
        "dry_run inserted nothing"
    );

    // A real insert — and the tenant in the arguments is overridden by the server.
    let (err, _) = call(
        &cfg,
        "insert",
        json!({ "model": "Ticket", "values": { "title": "A", "status": "open", "tenant": "globex" } }),
    )
    .await;
    assert!(!err);
    assert_eq!(
        scalar_i64("SELECT count(*) FROM tickets WHERE tenant = 'acme'", &[]).await,
        1,
        "the server forced tenant=acme, ignoring the globex argument"
    );
    assert_eq!(
        scalar_i64("SELECT count(*) FROM tickets WHERE tenant = 'globex'", &[]).await,
        0
    );

    // update with a filter works.
    let (err, out) = call(
        &cfg,
        "update",
        json!({ "model": "Ticket", "filters": { "status": "open" }, "set": { "status": "closed" } }),
    )
    .await;
    assert!(!err);
    assert_eq!(out["updated"], json!(1));

    // An unfiltered delete on a server with NO tenant is refused.
    let cfg_no_tenant = ServerConfig {
        writable: true,
        tenant: None,
        ..ServerConfig::default()
    };
    let (err, out) = call(&cfg_no_tenant, "delete", json!({ "model": "Ticket" })).await;
    assert!(err, "unfiltered delete must be refused");
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no filter"),
        "{out}"
    );

    // The row cap bites: insert 6, then a filtered delete that would hit all 6.
    for i in 0..6 {
        db::run(
            "INSERT INTO tickets (tenant, title, status) VALUES ('acme', $1, 'bulk')",
            &[Value::Text(format!("t{i}"))],
        )
        .await
        .unwrap();
    }
    let (err, out) = call(
        &cfg,
        "delete",
        json!({ "model": "Ticket", "filters": { "status": "bulk" } }),
    )
    .await;
    assert!(err, "delete over the cap must be refused: {out}");
    assert!(
        out["message"].as_str().unwrap_or_default().contains("cap"),
        "{out}"
    );
    // Nothing was deleted.
    assert_eq!(
        scalar_i64("SELECT count(*) FROM tickets WHERE status='bulk'", &[]).await,
        6
    );

    // A read-only server refuses the write tool entirely.
    let ro = ServerConfig::default();
    let (err, out) = call(&ro, "insert", json!({ "model": "Ticket", "values": {} })).await;
    assert!(err);
    assert!(
        out["message"]
            .as_str()
            .unwrap_or_default()
            .contains("writes are disabled"),
        "{out}"
    );

    db::run_ddl("DROP TABLE IF EXISTS tickets CASCADE")
        .await
        .ok();
}
