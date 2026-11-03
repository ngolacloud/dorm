//! JSON-RPC 2.0 — the wire the MCP stdio transport speaks (ADR 0020).
//!
//! MCP over stdio is newline-delimited JSON-RPC 2.0. These are just the envelope
//! types; what the methods *mean* lives in [`crate::server`]. Kept deliberately
//! small — no `rmcp`, so it builds offline at MSRV 1.75.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A request or a notification (a notification has no `id`).
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    /// Absent for a notification; present (number, string, or null) for a call.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// A notification expects no response.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A successful response.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub result: Value,
}

/// An error response.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    pub error: RpcError,
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

// The JSON-RPC error codes MCP uses.
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// Build a success response line.
pub fn ok(id: Value, result: Value) -> String {
    serde_json::to_string(&Response {
        jsonrpc: "2.0",
        id,
        result,
    })
    .unwrap_or_default()
}

/// Build an error response line.
pub fn err(id: Value, code: i64, message: impl Into<String>) -> String {
    serde_json::to_string(&ErrorResponse {
        jsonrpc: "2.0",
        id,
        error: RpcError {
            code,
            message: message.into(),
        },
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_has_an_id_a_notification_does_not() {
        let call: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert!(!call.is_notification());
        let note: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(note.is_notification());
    }

    #[test]
    fn responses_carry_the_request_id_and_the_2_0_tag() {
        let line = ok(serde_json::json!(7), serde_json::json!({"ok": true}));
        assert!(line.contains("\"id\":7"));
        assert!(line.contains("\"jsonrpc\":\"2.0\""));
        let e = err(serde_json::json!("x"), METHOD_NOT_FOUND, "nope");
        assert!(e.contains("-32601"));
        assert!(e.contains("\"id\":\"x\""));
    }
}
