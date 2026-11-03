//! The OAuth 2.1 `client_credentials` flow over the MCP HTTP transport (ADR 0020,
//! brick 2, phase B): discovery → `/token` → an authenticated MCP call with the
//! issued token. Over a real socket, no database.

use dorm_mcp::{parse_clients, serve_on, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Send a raw HTTP request; return the whole response.
async fn send(addr: &str, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in extra {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.expect("read");
    resp
}

fn status(resp: &str) -> &str {
    resp.lines().next().unwrap_or("")
}
fn body(resp: &str) -> &str {
    resp.split("\r\n\r\n").nth(1).unwrap_or("")
}
fn json(resp: &str) -> serde_json::Value {
    serde_json::from_str(body(resp)).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn oauth_client_credentials_issues_a_token_that_authenticates_mcp() {
    let cfg = ServerConfig {
        clients: parse_clients("agent:s3cr3t=acme:rw"),
        issuer: Some("http://127.0.0.1".to_string()),
        ..ServerConfig::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = serve_on(listener, cfg).await;
    });

    // Discovery points at /token.
    let r = send(
        &addr,
        "GET",
        "/.well-known/oauth-authorization-server",
        &[],
        "",
    )
    .await;
    assert!(status(&r).contains("200"), "{}", status(&r));
    assert_eq!(
        json(&r)["token_endpoint"],
        serde_json::json!("http://127.0.0.1/token")
    );
    assert_eq!(
        json(&r)["grant_types_supported"],
        serde_json::json!(["client_credentials"])
    );

    // Wrong secret → 401 invalid_client.
    let r = send(
        &addr,
        "POST",
        "/token",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        "grant_type=client_credentials&client_id=agent&client_secret=wrong",
    )
    .await;
    assert!(status(&r).contains("401"), "{}", status(&r));
    assert_eq!(json(&r)["error"], serde_json::json!("invalid_client"));

    // Correct credentials → an access token.
    let r = send(
        &addr,
        "POST",
        "/token",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        "grant_type=client_credentials&client_id=agent&client_secret=s3cr3t",
    )
    .await;
    assert!(status(&r).contains("200"), "{}", status(&r));
    assert_eq!(json(&r)["token_type"], serde_json::json!("Bearer"));
    assert_eq!(json(&r)["scope"], serde_json::json!("read write"));
    let token = json(&r)["access_token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64, "256-bit opaque token");

    // The issued token authenticates an MCP call (no static tokens configured).
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let r = send(
        &addr,
        "POST",
        "/",
        &[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("Authorization", &format!("Bearer {token}")),
        ],
        init,
    )
    .await;
    assert!(
        status(&r).contains("200"),
        "issued token should authenticate: {}",
        status(&r)
    );
    assert_eq!(
        json(&r)["result"]["protocolVersion"],
        serde_json::json!("2024-11-05")
    );

    // A bogus bearer is still rejected.
    let r = send(
        &addr,
        "POST",
        "/",
        &[
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("Authorization", "Bearer deadbeef"),
        ],
        init,
    )
    .await;
    assert!(status(&r).contains("401"), "{}", status(&r));
}
