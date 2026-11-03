//! OAuth 2.1 for the MCP HTTP transport (ADR 0020, brick 2, phase B).
//!
//! The headless-agent half of the spec: the **`client_credentials`** grant, which
//! fits a service calling the MCP server (no human in the loop, so no
//! authorization-code redirect). A client presents `client_id`/`client_secret`
//! (in the body or HTTP Basic) at `POST /token` and receives an **opaque** bearer
//! — 256 bits from the OS CSPRNG — bound to a tenant, a scope and an expiry, held
//! in memory. The MCP endpoint then accepts that token exactly like a static one.
//!
//! Discovery is served so a client can find the token endpoint:
//! `/.well-known/oauth-authorization-server` (RFC 8414) and
//! `/.well-known/oauth-protected-resource` (RFC 9728).
//!
//! Scope: `write` maps to the write tools; anything else is read-only. A client
//! can never exceed its configured scope, and the server must still be started
//! with `--write` for any write to land (defence in depth).
//!
//! What this is deliberately not (yet): the authorization-code + PKCE flow for
//! interactive users, token refresh/revocation endpoints, and JWT access tokens
//! for multi-process validation. Opaque in-memory tokens are correct for a single
//! server; a shared store or signed tokens are the next step.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

/// The principal a validated credential authenticates: the tenant every
/// tenant-scoped query is forced to, and whether it may write.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub tenant: Option<String>,
    pub can_write: bool,
}

/// A configured OAuth client (a headless agent's credentials).
#[derive(Clone)]
pub struct OAuthClient {
    pub id: String,
    pub secret: String,
    pub tenant: Option<String>,
    pub can_write: bool,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("id", &self.id)
            .field("secret", &"***")
            .field("tenant", &self.tenant)
            .field("can_write", &self.can_write)
            .finish()
    }
}

/// Parse `DELONIX_ORM_MCP_CLIENTS`: `id:secret=tenant:rw, id2:secret2=tenant2:ro`.
/// An empty tenant means unscoped; the mode defaults to read-only.
pub fn parse_clients(spec: &str) -> Vec<OAuthClient> {
    spec.split(',')
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (creds, rest) = entry.split_once('=').unwrap_or((entry, ""));
            let (id, secret) = creds.split_once(':')?;
            let (tenant, mode) = rest.split_once(':').unwrap_or((rest, "ro"));
            let id = id.trim().to_string();
            if id.is_empty() {
                return None;
            }
            Some(OAuthClient {
                id,
                secret: secret.trim().to_string(),
                tenant: {
                    let t = tenant.trim();
                    (!t.is_empty()).then(|| t.to_string())
                },
                can_write: mode.trim() == "rw",
            })
        })
        .collect()
}

/// The issued-token store: opaque token → principal + expiry. In-memory, so it is
/// correct for one server process; a shared store or JWTs would be the multi-
/// process step.
#[derive(Default)]
pub struct TokenStore {
    issued: Mutex<HashMap<String, (Principal, Instant)>>,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a token for `principal`, valid for `ttl`. Returns `(token, expiry)`.
    ///
    /// Sweeps expired entries on each mint, so a long-lived server does not leak
    /// memory on tokens whose client never presents them again (lazy removal on
    /// [`TokenStore::validate`] alone would keep those forever). Minting is
    /// infrequent relative to tool calls, so the `O(n)` sweep is cheap.
    pub fn mint(&self, principal: Principal, ttl: Duration) -> (String, Instant) {
        let token = random_token();
        let expires = Instant::now() + ttl;
        let mut g = self.issued.lock().unwrap();
        let now = Instant::now();
        g.retain(|_, (_, exp)| *exp > now);
        g.insert(token.clone(), (principal, expires));
        (token, expires)
    }

    /// Validate a token, returning its principal if present and unexpired. Expired
    /// entries are dropped lazily.
    pub fn validate(&self, token: &str) -> Option<Principal> {
        let mut g = self.issued.lock().unwrap();
        match g.get(token) {
            Some((p, exp)) if *exp > Instant::now() => Some(p.clone()),
            Some(_) => {
                g.remove(token);
                None
            }
            None => None,
        }
    }
}

/// 256 bits of opaque token, hex-encoded, from the OS CSPRNG (`/dev/urandom`),
/// with a non-crypto fallback that still varies per call.
pub fn random_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !ok {
        // Fallback: time + address entropy. Not cryptographic; flagged by the
        // `/dev/urandom` path being the real source.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let a = &bytes as *const _ as usize as u128;
        let mix = n ^ a;
        bytes[..16].copy_from_slice(&mix.to_le_bytes());
        bytes[16..].copy_from_slice(&(mix.rotate_left(57)).to_le_bytes());
    }
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// RFC 8414 authorization-server metadata.
pub fn authorization_server_metadata(issuer: &str) -> Json {
    json!({
        "issuer": issuer,
        "token_endpoint": format!("{issuer}/token"),
        "grant_types_supported": ["client_credentials"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        "scopes_supported": ["read", "write"],
        "response_types_supported": [],
    })
}

/// RFC 9728 protected-resource metadata: points a client at the auth server.
pub fn protected_resource_metadata(issuer: &str) -> Json {
    json!({
        "resource": issuer,
        "authorization_servers": [issuer],
        "scopes_supported": ["read", "write"],
        "bearer_methods_supported": ["header"],
    })
}

/// A parsed `/token` request's credentials and grant.
pub struct TokenRequest {
    pub grant_type: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: Option<String>,
}

/// Parse the token request from the form body and an optional HTTP Basic header.
/// Credentials in the Basic header win over the body (RFC 6749 §2.3.1).
pub fn parse_token_request(form: &str, basic: Option<&str>) -> TokenRequest {
    let mut grant_type = String::new();
    let mut client_id = String::new();
    let mut client_secret = String::new();
    let mut scope = None;
    for (k, v) in form_pairs(form) {
        match k.as_str() {
            "grant_type" => grant_type = v,
            "client_id" => client_id = v,
            "client_secret" => client_secret = v,
            "scope" => scope = Some(v),
            _ => {}
        }
    }
    if let Some((id, secret)) = basic.and_then(decode_basic) {
        client_id = id;
        client_secret = secret;
    }
    TokenRequest {
        grant_type,
        client_id,
        client_secret,
        scope,
    }
}

/// The `client_credentials` grant: validate the client and mint a token.
///
/// On success returns the RFC 6749 token response JSON. On failure returns the
/// HTTP status and an `{error, error_description}` body.
pub fn grant_client_credentials(
    clients: &[OAuthClient],
    store: &TokenStore,
    req: &TokenRequest,
    ttl: Duration,
) -> Result<Json, (u16, Json)> {
    if req.grant_type != "client_credentials" {
        return Err((
            400,
            oauth_error(
                "unsupported_grant_type",
                "only client_credentials is supported",
            ),
        ));
    }
    let client = clients
        .iter()
        .find(|c| c.id == req.client_id && constant_time_eq(&c.secret, &req.client_secret))
        .ok_or_else(|| {
            (
                401,
                oauth_error("invalid_client", "unknown client or bad secret"),
            )
        })?;

    // The granted scope never exceeds the client's. If `write` is asked for but
    // the client is read-only, the token is read-only (narrowed, not refused).
    let wants_write = req
        .scope
        .as_deref()
        .map(|s| s.split_whitespace().any(|w| w == "write"))
        .unwrap_or(client.can_write);
    let can_write = client.can_write && wants_write;
    let principal = Principal {
        tenant: client.tenant.clone(),
        can_write,
    };
    let (token, _) = store.mint(principal, ttl);
    Ok(json!({
        "access_token": token,
        "token_type": "Bearer",
        "expires_in": ttl.as_secs(),
        "scope": if can_write { "read write" } else { "read" },
    }))
}

fn oauth_error(code: &str, desc: &str) -> Json {
    json!({ "error": code, "error_description": desc })
}

/// Length-aware constant-time compare, so a bad secret cannot be timed out.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// `application/x-www-form-urlencoded` → pairs, with minimal percent-decoding.
fn form_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Decode `Basic base64(id:secret)` into `(id, secret)`.
fn decode_basic(header: &str) -> Option<(String, String)> {
    let b64 = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let decoded = base64_decode(b64.trim())?;
    let s = String::from_utf8(decoded).ok()?;
    let (id, secret) = s.split_once(':')?;
    Some((id.to_string(), secret.to_string()))
}

/// Minimal standard-alphabet base64 decoder (no dependency).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s: Vec<u8> = s
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut buf = [0u8; 4];
        let mut n = 0;
        for (i, &c) in chunk.iter().enumerate() {
            buf[i] = val(c)?;
            n += 1;
        }
        if n >= 2 {
            out.push((buf[0] << 2) | (buf[1] >> 4));
        }
        if n >= 3 {
            out.push((buf[1] << 4) | (buf[2] >> 2));
        }
        if n >= 4 {
            out.push((buf[2] << 6) | buf[3]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_parse_with_scope_and_tenant() {
        let cs = parse_clients("agent:s3cr3t=acme:rw, reader:pw=globex:ro");
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].id, "agent");
        assert_eq!(cs[0].tenant.as_deref(), Some("acme"));
        assert!(cs[0].can_write);
        assert!(!cs[1].can_write);
    }

    #[test]
    fn client_credentials_mints_a_token_and_wrong_secret_is_invalid_client() {
        let clients = parse_clients("agent:s3cr3t=acme:rw");
        let store = TokenStore::new();
        let ok = parse_token_request(
            "grant_type=client_credentials&client_id=agent&client_secret=s3cr3t",
            None,
        );
        let resp =
            grant_client_credentials(&clients, &store, &ok, Duration::from_secs(3600)).unwrap();
        let token = resp["access_token"].as_str().unwrap();
        assert_eq!(resp["token_type"], json!("Bearer"));
        assert_eq!(resp["scope"], json!("read write"));
        // The minted token validates to the client's principal.
        assert_eq!(
            store.validate(token),
            Some(Principal {
                tenant: Some("acme".into()),
                can_write: true
            })
        );

        let bad = parse_token_request(
            "grant_type=client_credentials&client_id=agent&client_secret=nope",
            None,
        );
        let err =
            grant_client_credentials(&clients, &store, &bad, Duration::from_secs(60)).unwrap_err();
        assert_eq!(err.0, 401);
        assert_eq!(err.1["error"], json!("invalid_client"));
    }

    #[test]
    fn a_read_only_client_cannot_get_write_scope() {
        let clients = parse_clients("r:pw=acme:ro");
        let store = TokenStore::new();
        let req = parse_token_request(
            "grant_type=client_credentials&client_id=r&client_secret=pw&scope=write",
            None,
        );
        let resp =
            grant_client_credentials(&clients, &store, &req, Duration::from_secs(60)).unwrap();
        assert_eq!(resp["scope"], json!("read"));
        assert_eq!(
            store.validate(resp["access_token"].as_str().unwrap()),
            Some(Principal {
                tenant: Some("acme".into()),
                can_write: false
            })
        );
    }

    #[test]
    fn an_expired_token_does_not_validate() {
        let store = TokenStore::new();
        let (tok, _) = store.mint(
            Principal {
                tenant: None,
                can_write: false,
            },
            Duration::from_millis(0),
        );
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(store.validate(&tok), None);
    }

    #[test]
    fn basic_auth_credentials_override_the_body() {
        // base64("agent:s3cr3t") = YWdlbnQ6czNjcjN0
        let req = parse_token_request(
            "grant_type=client_credentials",
            Some("Basic YWdlbnQ6czNjcjN0"),
        );
        assert_eq!(req.client_id, "agent");
        assert_eq!(req.client_secret, "s3cr3t");
    }

    #[test]
    fn tokens_are_opaque_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[test]
    fn discovery_points_at_the_token_endpoint() {
        let m = authorization_server_metadata("http://localhost:8000");
        assert_eq!(m["token_endpoint"], json!("http://localhost:8000/token"));
        assert_eq!(m["grant_types_supported"], json!(["client_credentials"]));
    }
}
