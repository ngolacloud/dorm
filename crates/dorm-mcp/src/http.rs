//! The MCP **Streamable HTTP** transport (ADR 0020, brick 2, phase A).
//!
//! One endpoint over HTTP/1.1:
//! * `POST` a JSON-RPC message. A request yields a response framed as either
//!   `application/json` or, when the client's `Accept` prefers it,
//!   `text/event-stream` (one SSE `message` event, then the stream closes). A
//!   notification yields `202 Accepted` with no body.
//! * `GET` opens a server→client SSE stream (kept alive with comments); the server
//!   has no unsolicited messages yet, so it is a long-lived heartbeat.
//! * **Sessions**: `initialize` mints an `Mcp-Session-Id`, returned in the
//!   response header; every later request must carry it, or the server answers
//!   `404` so the client re-initializes — the session lifecycle the spec defines.
//!
//! Security is the resource-server side of the MCP auth spec: a bearer token maps
//! to a principal (tenant + write scope); the tenant is the token's, never a tool
//! argument; writing needs both a `rw` token and a server started with `--write`.
//! Loopback by default, rate-limited. The full OAuth 2.1 *authorization server*
//! (discovery, token issuance, PKCE) is phase B — see ADR 0020. No `rmcp`, no
//! axum: a hand-written HTTP/1.1 endpoint on `tokio`, so it builds at MSRV 1.75.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value as Json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::oauth::{self, Principal, TokenStore};
use crate::server::{handle_message, HttpToken, ServerConfig};

/// Validate a bearer against the static tokens (`DELONIX_ORM_MCP_TOKENS`); the
/// OAuth-issued tokens are checked separately via the [`TokenStore`].
pub fn authorize(tokens: &[HttpToken], bearer: &str) -> Option<Principal> {
    tokens
        .iter()
        .find(|t| t.token == bearer)
        .map(|t| Principal {
            tenant: t.tenant.clone(),
            can_write: t.can_write,
        })
}

/// Validate a bearer against either source: a static token, or an unexpired
/// OAuth-issued one.
fn authenticate(cfg: &ServerConfig, store: &TokenStore, bearer: &str) -> Option<Principal> {
    authorize(&cfg.http_tokens, bearer).or_else(|| store.validate(bearer))
}

/// The issuer base URL for discovery: explicit, or derived from the bind.
fn issuer_of(cfg: &ServerConfig) -> String {
    cfg.issuer
        .clone()
        .unwrap_or_else(|| format!("http://{}", cfg.bind))
}

/// Extract the token from an `Authorization: Bearer <token>` header value.
pub fn bearer_of(header: &str) -> Option<&str> {
    header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .map(str::trim)
}

/// Session registry: mints opaque `Mcp-Session-Id`s and checks them. The id is
/// not a credential (the bearer is) — it only routes a client to its session, so
/// a process-unique value suffices.
struct Sessions {
    live: Mutex<HashSet<String>>,
    counter: AtomicU64,
    salt: u64,
}

impl Sessions {
    fn new() -> Self {
        let salt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self {
            live: Mutex::new(HashSet::new()),
            counter: AtomicU64::new(1),
            salt,
        }
    }
    fn mint(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = format!("{:016x}-{n:x}", self.salt);
        self.live.lock().unwrap().insert(id.clone());
        id
    }
    fn valid(&self, id: &str) -> bool {
        self.live.lock().unwrap().contains(id)
    }
}

/// A fixed-window rate limiter, shared across connections.
#[derive(Clone)]
struct RateLimiter {
    inner: Arc<Mutex<(Instant, u32)>>,
    per_min: u32,
}

impl RateLimiter {
    fn new(per_min: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new((Instant::now(), 0))),
            per_min,
        }
    }
    fn allow(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.0.elapsed().as_secs() >= 60 {
            *g = (Instant::now(), 0);
        }
        if g.1 >= self.per_min {
            return false;
        }
        g.1 += 1;
        true
    }
}

/// Whether an `Accept` header prefers SSE framing.
pub fn prefers_sse(accept: &str) -> bool {
    accept.contains("text/event-stream")
}

/// Bind `cfg.bind` and serve. Warns (does not refuse) if not loopback.
pub async fn serve_http(cfg: ServerConfig) -> dorm_core::Result<()> {
    if cfg.http_tokens.is_empty() && cfg.clients.is_empty() {
        return Err(dorm_core::Error::Config(
            "HTTP transport needs credentials — set DELONIX_ORM_MCP_TOKENS \
             (static bearer, e.g. `secret=acme:ro`) or DELONIX_ORM_MCP_CLIENTS \
             (OAuth, e.g. `id:secret=acme:rw`)"
                .into(),
        ));
    }
    if !cfg.bind.starts_with("127.0.0.1") && !cfg.bind.starts_with("[::1]") {
        tracing::warn!(target: "dorm::mcp", bind = %cfg.bind, "MCP HTTP is bound beyond loopback");
    }
    let listener = TcpListener::bind(&cfg.bind)
        .await
        .map_err(|e| dorm_core::Error::Config(format!("binding {}: {e}", cfg.bind)))?;
    serve_on(listener, cfg).await
}

/// Serve on an already-bound listener (so a test can pick an ephemeral port).
pub async fn serve_on(listener: TcpListener, cfg: ServerConfig) -> dorm_core::Result<()> {
    db::set_read_only(!cfg.writable);
    let limiter = RateLimiter::new(cfg.rate_per_min);
    let sessions = Arc::new(Sessions::new());
    let store = Arc::new(TokenStore::new());
    let cfg = Arc::new(cfg);
    tracing::info!(target: "dorm::mcp", "mcp server ready (streamable http)");
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cfg = cfg.clone();
        let limiter = limiter.clone();
        let sessions = sessions.clone();
        let store = store.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, &cfg, &limiter, &sessions, &store).await;
        });
    }
}

use dorm_core::db;

/// One parsed HTTP request.
struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    cfg: &ServerConfig,
    limiter: &RateLimiter,
    sessions: &Sessions,
    store: &TokenStore,
) -> std::io::Result<()> {
    let Some(req) = read_http(&mut stream).await? else {
        return write_simple(&mut stream, 400, "bad request").await;
    };

    // OAuth endpoints are unauthenticated (they are how you get a token) and are
    // matched before the bearer check.
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/.well-known/oauth-authorization-server") => {
            let body = oauth::authorization_server_metadata(&issuer_of(cfg)).to_string();
            return write_json(&mut stream, 200, &body, None).await;
        }
        ("GET", "/.well-known/oauth-protected-resource") => {
            let body = oauth::protected_resource_metadata(&issuer_of(cfg)).to_string();
            return write_json(&mut stream, 200, &body, None).await;
        }
        ("POST", "/token") => {
            if !limiter.allow() {
                return write_simple(&mut stream, 429, "rate limit exceeded").await;
            }
            return handle_token(&mut stream, cfg, store, &req).await;
        }
        _ => {}
    }

    // Everything else is the MCP endpoint: authenticate the bearer (static or
    // OAuth-issued) for GET and POST alike.
    let principal = req
        .header("authorization")
        .and_then(bearer_of)
        .and_then(|tok| authenticate(cfg, store, tok));
    let Some(principal) = principal else {
        return write_simple(&mut stream, 401, "missing or invalid bearer token").await;
    };

    match req.method.as_str() {
        // The server→client SSE stream. We have no unsolicited messages yet, so it
        // is a heartbeat the client can hold open.
        "GET" => {
            if !prefers_sse(req.header("accept").unwrap_or("")) {
                return write_simple(
                    &mut stream,
                    405,
                    "use POST, or GET with Accept: text/event-stream",
                )
                .await;
            }
            serve_sse_stream(&mut stream).await
        }
        "POST" => handle_post(&mut stream, cfg, limiter, sessions, &req, &principal).await,
        _ => write_simple(&mut stream, 405, "method not allowed").await,
    }
}

/// `POST /token`: the OAuth 2.1 `client_credentials` grant.
async fn handle_token(
    stream: &mut TcpStream,
    cfg: &ServerConfig,
    store: &TokenStore,
    req: &HttpRequest,
) -> std::io::Result<()> {
    let tr = oauth::parse_token_request(&req.body, req.header("authorization"));
    let ttl = Duration::from_secs(cfg.token_ttl_secs);
    match oauth::grant_client_credentials(&cfg.clients, store, &tr, ttl) {
        Ok(body) => write_json(stream, 200, &body.to_string(), None).await,
        Err((status, body)) => write_json(stream, status, &body.to_string(), None).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_post(
    stream: &mut TcpStream,
    cfg: &ServerConfig,
    limiter: &RateLimiter,
    sessions: &Sessions,
    req: &HttpRequest,
    principal: &Principal,
) -> std::io::Result<()> {
    if !limiter.allow() {
        return write_simple(stream, 429, "rate limit exceeded").await;
    }

    // Peek at the JSON-RPC method and whether it is a request, for session rules.
    let parsed: Json = serde_json::from_str(&req.body).unwrap_or(Json::Null);
    let rpc_method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let is_initialize = rpc_method == "initialize";

    // Session lifecycle: everything after `initialize` must carry a known
    // `Mcp-Session-Id`, else 404 so the client re-initializes.
    if !is_initialize {
        let known = req
            .header("mcp-session-id")
            .is_some_and(|s| sessions.valid(s));
        if !known {
            return write_simple(stream, 404, "unknown or missing Mcp-Session-Id").await;
        }
    }

    // Per-request config: tenant and write scope come from the token.
    let mut req_cfg = cfg.clone();
    req_cfg.tenant = principal.tenant.clone();
    req_cfg.writable = cfg.writable && principal.can_write;

    let response = handle_message(&req_cfg, &req.body).await;
    let new_session = is_initialize.then(|| sessions.mint());
    let sse = prefers_sse(req.header("accept").unwrap_or(""));

    match response {
        // A request: answer with the JSON-RPC result, framed as the client asked.
        Some(body) => {
            if sse {
                write_sse_message(stream, &body, new_session.as_deref()).await
            } else {
                write_json(stream, 200, &body, new_session.as_deref()).await
            }
        }
        // A notification: nothing to return.
        None => write_json(stream, 202, "", new_session.as_deref()).await,
    }
}

/// A long-lived SSE stream (server→client). Sends an initial comment then a
/// keep-alive every 15s until the client disconnects.
async fn serve_sse_stream(stream: &mut TcpStream) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(b": connected\n\n").await?;
    stream.flush().await?;
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if stream.write_all(b": keep-alive\n\n").await.is_err() || stream.flush().await.is_err() {
            break; // client gone
        }
    }
    Ok(())
}

/// Read one HTTP request: the method, the headers, and the body by
/// `Content-Length`. `None` if malformed.
async fn read_http(stream: &mut TcpStream) -> std::io::Result<Option<HttpRequest>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1 << 20 {
            return Ok(None);
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut rl = request_line.split_whitespace();
    let method = rl.next().unwrap_or("").to_string();
    let path = rl.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    Ok(Some(HttpRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    }))
}

/// A JSON (or empty, for 202) HTTP response, optionally minting a session.
async fn write_json(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
    session: Option<&str>,
) -> std::io::Result<()> {
    let reason = reason(status);
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(sid) = session {
        head.push_str(&format!("Mcp-Session-Id: {sid}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

/// The JSON-RPC response as a single SSE `message` event, then the stream closes.
async fn write_sse_message(
    stream: &mut TcpStream,
    json: &str,
    session: Option<&str>,
) -> std::io::Result<()> {
    let mut head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\nConnection: close\r\n"
        .to_string();
    if let Some(sid) = session {
        head.push_str(&format!("Mcp-Session-Id: {sid}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream
        .write_all(format!("event: message\ndata: {json}\n\n").as_bytes())
        .await?;
    stream.flush().await
}

/// A plain-text status response (errors, 405, …), JSON-RPC-shaped for the body.
async fn write_simple(stream: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let body = crate::protocol::err(Json::Null, crate::protocol::INTERNAL_ERROR, message);
    let reason = reason(status);
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if status == 401 {
        head.push_str("WWW-Authenticate: Bearer\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        _ => "Error",
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::parse_tokens;

    #[test]
    fn bearer_is_extracted_case_insensitively() {
        assert_eq!(bearer_of("Bearer abc"), Some("abc"));
        assert_eq!(bearer_of("bearer  xyz "), Some("xyz"));
        assert_eq!(bearer_of("Basic abc"), None);
    }

    #[test]
    fn authorize_maps_a_token_to_its_principal() {
        let tokens = parse_tokens("sek=acme:rw, ro=globex:ro, bare=:ro");
        assert_eq!(
            authorize(&tokens, "sek"),
            Some(Principal {
                tenant: Some("acme".into()),
                can_write: true
            })
        );
        assert_eq!(
            authorize(&tokens, "bare"),
            Some(Principal {
                tenant: None,
                can_write: false
            })
        );
        assert_eq!(authorize(&tokens, "nope"), None);
    }

    #[test]
    fn the_rate_limiter_blocks_past_the_budget() {
        let rl = RateLimiter::new(2);
        assert!(rl.allow());
        assert!(rl.allow());
        assert!(!rl.allow());
    }

    #[test]
    fn sessions_are_minted_unique_and_validated() {
        let s = Sessions::new();
        let a = s.mint();
        let b = s.mint();
        assert_ne!(a, b);
        assert!(s.valid(&a) && s.valid(&b));
        assert!(!s.valid("bogus"));
    }

    #[test]
    fn accept_header_drives_sse_framing() {
        assert!(prefers_sse("application/json, text/event-stream"));
        assert!(!prefers_sse("application/json"));
    }
}
