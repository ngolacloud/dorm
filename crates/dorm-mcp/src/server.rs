//! The MCP server: configuration, the stdio loop, and method dispatch (ADR 0020).
//!
//! Read-only in this brick: the tool catalogue has no writes, and the server sets
//! `db::set_read_only(true)` so the core's `guard_write` refuses any mutation even
//! if a tool were mis-registered — defence in depth, not a single check. The
//! tenant is taken from the server config (the launch `--tenant`, or an HTTP
//! token later), never from a tool argument.

use dorm_core::db;
use serde_json::{json, Value as Json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::protocol::{self, Request};
use crate::{resources, tools};

/// How the server is configured for a run.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Writes are refused while this is false (the whole of this brick).
    pub writable: bool,
    /// The tenant every tenant-scoped query is constrained to, if any.
    pub tenant: Option<String>,
    /// If `Some`, only these models are exposed; `None` exposes all.
    pub models: Option<Vec<String>>,
    /// Hard cap on rows per response.
    pub max_rows: i64,
    /// Hard cap on rows a single write may affect (insert/update/delete).
    pub max_write_rows: i64,
    /// HTTP transport: the address to bind. Loopback by default, like Studio.
    pub bind: String,
    /// HTTP transport: bearer tokens → principal (tenant + write scope).
    pub http_tokens: Vec<HttpToken>,
    /// HTTP transport: requests allowed per minute (per process).
    pub rate_per_min: u32,
    /// OAuth 2.1: the headless-agent clients allowed at `POST /token`.
    pub clients: Vec<crate::oauth::OAuthClient>,
    /// OAuth 2.1: the issuer base URL for discovery; defaults to `http://<bind>`.
    pub issuer: Option<String>,
    /// OAuth 2.1: issued-token lifetime, in seconds.
    pub token_ttl_secs: u64,
    pub server_name: String,
    pub server_version: String,
}

/// An HTTP bearer token and the principal it authenticates: the tenant it is
/// scoped to (the one every tenant-scoped query is forced to), and whether it may
/// write. The token itself is never logged.
#[derive(Clone)]
pub struct HttpToken {
    pub token: String,
    pub tenant: Option<String>,
    pub can_write: bool,
}

impl std::fmt::Debug for HttpToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the secret, even in a debug dump.
        f.debug_struct("HttpToken")
            .field("token", &"***")
            .field("tenant", &self.tenant)
            .field("can_write", &self.can_write)
            .finish()
    }
}

/// Parse `DELONIX_ORM_MCP_TOKENS`: `token=tenant:rw, token2=tenant2:ro`. An empty
/// tenant means no tenant scoping; the mode defaults to read-only.
pub fn parse_tokens(spec: &str) -> Vec<HttpToken> {
    spec.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (token, rest) = entry.split_once('=').unwrap_or((entry, ""));
            let (tenant, mode) = rest.split_once(':').unwrap_or((rest, "ro"));
            Some(HttpToken {
                token: token.trim().to_string(),
                tenant: {
                    let t = tenant.trim();
                    (!t.is_empty()).then(|| t.to_string())
                },
                can_write: mode.trim() == "rw",
            })
        })
        .filter(|t| !t.token.is_empty())
        .collect()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            writable: false,
            tenant: None,
            models: None,
            max_rows: 100,
            max_write_rows: 50,
            bind: "127.0.0.1:8000".to_string(),
            http_tokens: Vec::new(),
            rate_per_min: 120,
            clients: Vec::new(),
            issuer: None,
            token_ttl_secs: 3600,
            server_name: "dorm-mcp".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl ServerConfig {
    /// Parse `--tenant`, `--models a,b`, `--write` from a generated binary's args.
    /// `--transport http` is recognised but not yet served (returns via [`serve`]).
    pub fn from_args(args: &[String]) -> Self {
        let mut cfg = ServerConfig::default();
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        cfg.writable = args.iter().any(|a| a == "--write");
        cfg.tenant = flag("--tenant");
        cfg.models = flag("--models").map(|s| {
            s.split(',')
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect()
        });
        if let Some(n) = flag("--max-rows").and_then(|s| s.parse().ok()) {
            cfg.max_rows = n;
        }
        if let Some(b) = flag("--bind") {
            cfg.bind = b;
        }
        // HTTP bearer tokens and rate limit come from the environment, never the
        // command line, so they do not land in `ps`/shell history.
        if let Ok(spec) = std::env::var("DELONIX_ORM_MCP_TOKENS") {
            cfg.http_tokens = parse_tokens(&spec);
        }
        if let Ok(Ok(n)) = std::env::var("DELONIX_ORM_MCP_RATE_PER_MIN").map(|s| s.parse::<u32>()) {
            cfg.rate_per_min = n;
        }
        // OAuth 2.1 clients and issuer, from the environment (secrets off argv).
        if let Ok(spec) = std::env::var("DELONIX_ORM_MCP_CLIENTS") {
            cfg.clients = crate::oauth::parse_clients(&spec);
        }
        cfg.issuer = std::env::var("DELONIX_ORM_MCP_ISSUER").ok();
        if let Ok(Ok(n)) = std::env::var("DELONIX_ORM_MCP_TOKEN_TTL").map(|s| s.parse::<u64>()) {
            cfg.token_ttl_secs = n;
        }
        cfg
    }

    /// Whether a model is exposed by this server.
    pub fn model_allowed(&self, name: &str) -> bool {
        match &self.models {
            None => true,
            Some(list) => list.iter().any(|m| m == name),
        }
    }

    /// The transport named on the command line (default stdio).
    pub fn transport(args: &[String]) -> String {
        args.iter()
            .position(|a| a == "--transport")
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| "stdio".to_string())
    }
}

/// Serve MCP over stdio: read newline-delimited JSON-RPC from stdin, write
/// responses to stdout, until stdin closes. Read-only — writes are refused by the
/// core guard regardless of what a tool tries.
pub async fn serve(cfg: ServerConfig) -> dorm_core::Result<()> {
    // Defence in depth: unless writes were explicitly enabled, the whole process
    // is read-only, so the core's `guard_write` refuses any mutation even if a
    // write tool slipped through.
    db::set_read_only(!cfg.writable);

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();

    tracing::info!(
        target: "dorm::mcp",
        tenant = cfg.tenant.as_deref().unwrap_or("-"),
        "mcp server ready (stdio, read-only)"
    );

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| dorm_core::Error::Db(format!("reading stdin: {e}")))?
    {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(out) = handle_message(&cfg, &line).await {
            stdout
                .write_all(out.as_bytes())
                .await
                .and(stdout.write_all(b"\n").await)
                .and(stdout.flush().await)
                .map_err(|e| dorm_core::Error::Db(format!("writing stdout: {e}")))?;
        }
    }
    Ok(())
}

/// Handle one JSON-RPC message, returning the response line — or `None` for a
/// notification or an unparsable line (which JSON-RPC says a server ignores).
/// Public so a host (a test, or the HTTP transport) can drive the server one
/// message at a time without owning stdio.
pub async fn handle_message(cfg: &ServerConfig, line: &str) -> Option<String> {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return None,
    };
    if req.is_notification() {
        return None;
    }
    let id = req.id.clone().unwrap_or(Json::Null);
    Some(dispatch(cfg, &req.method, &req.params, id).await)
}

async fn dispatch(cfg: &ServerConfig, method: &str, params: &Json, id: Json) -> String {
    // Audit every call (ADR 0020): subject-less here (stdio), but tool and tenant.
    tracing::debug!(target: "dorm::mcp", method, tenant = cfg.tenant.as_deref().unwrap_or("-"), "mcp call");
    match method {
        "initialize" => protocol::ok(id, initialize_result(cfg)),
        "ping" => protocol::ok(id, json!({})),
        "tools/list" => protocol::ok(id, json!({ "tools": tools::catalog(cfg) })),
        "tools/call" => tools_call(cfg, params, id).await,
        "resources/list" => protocol::ok(id, json!({ "resources": resources::list(cfg) })),
        "resources/read" => resources::read(cfg, params, id).await,
        "prompts/list" => protocol::ok(id, json!({ "prompts": resources::prompts() })),
        "prompts/get" => resources::prompt_get(params, id),
        other => protocol::err(
            id,
            protocol::METHOD_NOT_FOUND,
            format!("unknown method `{other}`"),
        ),
    }
}

fn initialize_result(cfg: &ServerConfig) -> Json {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {},
            "resources": {},
            "prompts": {},
        },
        "serverInfo": { "name": cfg.server_name, "version": cfg.server_version },
    })
}

/// `tools/call`: run the tool. A tool failure is an MCP result with `isError:
/// true` (not a JSON-RPC error) so the agent can read the message and recover.
async fn tools_call(cfg: &ServerConfig, params: &Json, id: Json) -> String {
    let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
        return protocol::err(id, protocol::INVALID_PARAMS, "tools/call needs a `name`");
    };
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let started = std::time::Instant::now();
    let result = tools::execute(cfg, name, &args).await;
    // Audit (ADR 0020): one event per call — tool, tenant, the target model and
    // the argument *keys* (never their values, which could be secret), outcome
    // and duration. On the `dorm::mcp` target, no subscriber imposed.
    let keys: Vec<&str> = args
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    tracing::info!(
        target: "dorm::mcp",
        tool = name,
        tenant = cfg.tenant.as_deref().unwrap_or("-"),
        model = args.get("model").and_then(|v| v.as_str()).unwrap_or("-"),
        arg_keys = ?keys,
        ok = result.is_ok(),
        duration_ms = started.elapsed().as_millis() as u64,
        "mcp tool call"
    );
    match result {
        Ok(text) => protocol::ok(
            id,
            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        ),
        Err(e) => protocol::ok(
            id,
            json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_tenant_models_and_write() {
        let args: Vec<String> = ["--tenant", "acme", "--models", "Ticket, App", "--write"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cfg = ServerConfig::from_args(&args);
        assert_eq!(cfg.tenant.as_deref(), Some("acme"));
        assert!(cfg.writable);
        assert!(cfg.model_allowed("Ticket"));
        assert!(cfg.model_allowed("App"));
        assert!(!cfg.model_allowed("Secret"));
    }

    #[test]
    fn no_allowlist_exposes_everything() {
        let cfg = ServerConfig::default();
        assert!(cfg.model_allowed("Anything"));
    }

    #[tokio::test]
    async fn initialize_advertises_the_three_capabilities() {
        let cfg = ServerConfig::default();
        let out = dispatch(&cfg, "initialize", &json!({}), json!(1)).await;
        let v: Json = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["protocolVersion"], json!("2024-11-05"));
        assert!(v["result"]["capabilities"]["tools"].is_object());
        assert!(v["result"]["capabilities"]["resources"].is_object());
        assert!(v["result"]["capabilities"]["prompts"].is_object());
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_method_not_found_error() {
        let cfg = ServerConfig::default();
        let out = dispatch(&cfg, "nope", &json!({}), json!(2)).await;
        assert!(out.contains("-32601"));
    }
}
