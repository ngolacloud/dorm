//! The Streamable HTTP transport (ADR 0020, brick 2, phase A): bearer auth, the
//! `Mcp-Session-Id` lifecycle, and SSE framing — over a real socket. No database
//! (`initialize`/`tools/list` need none), so it runs in `make check`.

use dorm_mcp::{parse_tokens, serve_on, ServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Send a raw HTTP POST and return the whole response (status line + headers + body).
async fn post(
    addr: &str,
    bearer: Option<&str>,
    accept: &str,
    session: Option<&str>,
    body: &str,
) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut req = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: {accept}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(b) = bearer {
        req.push_str(&format!("Authorization: Bearer {b}\r\n"));
    }
    if let Some(s) = session {
        req.push_str(&format!("Mcp-Session-Id: {s}\r\n"));
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

fn header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    let head = resp.split("\r\n\r\n").next().unwrap_or("");
    head.lines()
        .skip(1)
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        })
        .map(|(_, v)| v.trim())
}

fn body(resp: &str) -> &str {
    resp.split("\r\n\r\n").nth(1).unwrap_or("")
}

#[tokio::test]
async fn streamable_http_does_bearer_sessions_and_sse() {
    let cfg = ServerConfig {
        http_tokens: parse_tokens("sek=acme:ro"),
        ..ServerConfig::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = serve_on(listener, cfg).await;
    });

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
    let accept = "application/json, text/event-stream";

    // No bearer → 401.
    let r = post(&addr, None, accept, None, init).await;
    assert!(status(&r).contains("401"), "{}", status(&r));

    // initialize with a bearer → 200 JSON, and a session id is minted.
    let r = post(&addr, Some("sek"), "application/json", None, init).await;
    assert!(status(&r).contains("200"), "{}", status(&r));
    let session = header(&r, "Mcp-Session-Id")
        .expect("initialize mints a session")
        .to_string();
    let v: serde_json::Value = serde_json::from_str(body(&r)).expect("json");
    assert_eq!(
        v["result"]["protocolVersion"],
        serde_json::json!("2024-11-05")
    );

    // A post-initialize call WITHOUT the session → 404 (client must re-init).
    let r = post(&addr, Some("sek"), accept, None, list).await;
    assert!(
        status(&r).contains("404"),
        "expected 404 without session: {}",
        status(&r)
    );

    // WITH the session → 200 and the tool list.
    let r = post(&addr, Some("sek"), "application/json", Some(&session), list).await;
    assert!(status(&r).contains("200"), "{}", status(&r));
    assert!(body(&r).contains("semantic_search"), "{}", body(&r));

    // Accept: text/event-stream → the response is framed as an SSE event.
    let r = post(
        &addr,
        Some("sek"),
        "text/event-stream",
        Some(&session),
        list,
    )
    .await;
    assert!(status(&r).contains("200"));
    assert!(header(&r, "Content-Type")
        .unwrap_or("")
        .contains("text/event-stream"));
    assert!(
        body(&r).starts_with("event: message\ndata: "),
        "{}",
        body(&r)
    );
    assert!(body(&r).contains("semantic_search"));

    // GET opens the long-lived server→client SSE heartbeat. Read the first bytes
    // with a timeout (the stream never closes on its own).
    let mut s = TcpStream::connect(&addr).await.unwrap();
    s.write_all(
        format!("GET / HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\nAuthorization: Bearer sek\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(std::time::Duration::from_secs(3), s.read(&mut buf))
        .await
        .expect("sse head within 3s")
        .expect("read");
    let head = String::from_utf8_lossy(&buf[..n]);
    assert!(
        head.contains("200") && head.contains("text/event-stream"),
        "{head}"
    );
    assert!(head.contains(": connected"), "{head}");
}
