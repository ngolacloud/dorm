//! A runnable MCP server over a sample model, to see the protocol at the console
//! without a whole project (ADR 0020). It links one `Ticket` model and serves
//! stdio; pipe JSON-RPC in:
//!
//! ```bash
//! printf '%s\n' \
//!   '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}' \
//!   '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
//!   '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_models","arguments":{}}}' \
//!   | cargo run -p dorm-mcp --example demo
//! ```

use dorm_derive::Model;
use dorm_mcp::{serve, serve_http, ServerConfig};
use dorm_rag::Rag;

#[derive(Model, Rag, Debug, Clone)]
#[model(table = "tickets", tenant_scoped)]
#[rag(fields = ["title", "body"], metadata = ["status", "tenant"])]
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

#[tokio::main]
async fn main() {
    // Touch the model so the derive's registry submission is linked in.
    let _ = <Ticket as dorm_core::model::Model>::meta();

    // `--transport stdio` (default) or `http`; reads --tenant/--write/--bind and
    // the DELONIX_ORM_MCP_* env (tokens, OAuth clients, issuer) via `from_args`.
    let args: Vec<String> = std::env::args().collect();
    let cfg = ServerConfig::from_args(&args);
    let result = match ServerConfig::transport(&args).as_str() {
        "http" => serve_http(cfg).await,
        _ => serve(cfg).await,
    };
    if let Err(e) = result {
        eprintln!("demo mcp: {e}");
    }
}
