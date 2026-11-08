//! JSON-RPC 2.0 and MCP lifecycle pieces that do not depend on HTTP: message
//! classification, error shapes, protocol-version negotiation and the
//! `initialize` result.

use serde_json::{Value, json};

/// MCP revisions this server speaks, newest first. All three use the
/// Streamable HTTP transport.
pub const SUPPORTED_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
/// Offered back when the client asks for a version not in the list.
pub const LATEST_VERSION: &str = SUPPORTED_VERSIONS[0];

// JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
// Server-defined codes for HTTP-level refusals (carried in the body of a 4xx).
pub const UNAUTHORIZED: i64 = -32001;
pub const FORBIDDEN: i64 = -32002;
pub const SESSION_REQUIRED: i64 = -32003;
pub const SESSION_NOT_FOUND: i64 = -32004;
pub const BAD_TRANSPORT: i64 = -32005;

/// A JSON-RPC error object.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn to_value(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

/// `{"jsonrpc":"2.0","id":..,"result":..}`
pub fn result_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// `{"jsonrpc":"2.0","id":..,"error":{..}}`; `id` is null when it could not be read.
pub fn error_response(id: &Value, err: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": err.to_value() })
}

/// What one element of a POST body is.
#[derive(Debug, PartialEq)]
pub enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
    },
    /// A client's answer to a server request; this server sends none, so
    /// these are accepted and ignored.
    Response,
    /// Not a valid JSON-RPC message; answer with this error (id may be null).
    Invalid {
        id: Value,
        error: RpcError,
    },
}

/// Classifies one JSON value per JSON-RPC 2.0.
pub fn classify(v: &Value) -> Message {
    let Some(obj) = v.as_object() else {
        return Message::Invalid {
            id: Value::Null,
            error: RpcError::new(INVALID_REQUEST, "Invalid request: each JSON-RPC message must be a JSON object."),
        };
    };
    let id = obj.get("id").cloned();
    let id_ok = matches!(&id, None | Some(Value::String(_)) | Some(Value::Number(_)));
    let reply_id = if id_ok { id.clone().unwrap_or(Value::Null) } else { Value::Null };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Message::Invalid { id: reply_id, error: RpcError::new(INVALID_REQUEST, "Invalid request: \"jsonrpc\" must be \"2.0\".") };
    }
    if !id_ok {
        return Message::Invalid {
            id: Value::Null,
            error: RpcError::new(INVALID_REQUEST, "Invalid request: \"id\" must be a string or a number."),
        };
    }
    match obj.get("method") {
        Some(Value::String(method)) => {
            let params = obj.get("params").cloned().unwrap_or(Value::Null);
            if !matches!(params, Value::Null | Value::Object(_) | Value::Array(_)) {
                return Message::Invalid {
                    id: reply_id,
                    error: RpcError::new(INVALID_REQUEST, "Invalid request: \"params\" must be an object."),
                };
            }
            match id {
                Some(id) => Message::Request { id, method: method.clone(), params },
                None => Message::Notification { method: method.clone() },
            }
        }
        Some(_) => {
            Message::Invalid { id: reply_id, error: RpcError::new(INVALID_REQUEST, "Invalid request: \"method\" must be a string.") }
        }
        None if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) => Message::Response,
        None => Message::Invalid { id: reply_id, error: RpcError::new(INVALID_REQUEST, "Invalid request: a request needs a \"method\".") },
    }
}

/// A parsed POST body: one message or a batch.
#[derive(Debug)]
pub enum Body {
    Single(Value),
    Batch(Vec<Value>),
}

pub fn parse_body(bytes: &[u8]) -> Result<Body, RpcError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|e| RpcError::new(PARSE_ERROR, format!("Parse error: the request body is not valid JSON ({e}).")))?;
    match v {
        Value::Array(items) if items.is_empty() => Err(RpcError::new(INVALID_REQUEST, "Invalid request: empty batch.")),
        Value::Array(items) => Ok(Body::Batch(items)),
        other => Ok(Body::Single(other)),
    }
}

/// The version to answer an `initialize` with: the client's when supported,
/// otherwise the latest this server speaks (the client then decides).
pub fn negotiate(requested: Option<&str>) -> &'static str {
    requested.and_then(|r| SUPPORTED_VERSIONS.iter().copied().find(|v| *v == r)).unwrap_or(LATEST_VERSION)
}

pub fn is_supported(version: &str) -> bool {
    SUPPORTED_VERSIONS.contains(&version)
}

pub fn initialize_result(version: &str) -> Value {
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "pmxspan", "title": "PMXSpan", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "PMXSpan exposes the Proxmox VE cluster it is connected to. Start with list_servers or list_guests; \
    use find_by_ip to map an address (for example an SSH session's host) to a guest. Destructive operations are never available.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_requests_notifications_and_responses() {
        let r = classify(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        assert_eq!(r, Message::Request { id: json!(1), method: "ping".into(), params: Value::Null });
        let r = classify(&json!({"jsonrpc":"2.0","id":"a","method":"tools/list","params":{}}));
        assert!(matches!(r, Message::Request { ref id, .. } if id == "a"));
        assert_eq!(
            classify(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
            Message::Notification { method: "notifications/initialized".into() }
        );
        assert_eq!(classify(&json!({"jsonrpc":"2.0","id":7,"result":{}})), Message::Response);
    }

    #[test]
    fn invalid_messages_carry_explaining_errors() {
        for (v, id) in [
            (json!(5), Value::Null),
            (json!({"id":1,"method":"ping"}), json!(1)),
            (json!({"jsonrpc":"2.0","id":{"x":1},"method":"ping"}), Value::Null),
            (json!({"jsonrpc":"2.0","id":2,"method":3}), json!(2)),
            (json!({"jsonrpc":"2.0","id":3}), json!(3)),
            (json!({"jsonrpc":"2.0","id":4,"method":"x","params":"no"}), json!(4)),
        ] {
            match classify(&v) {
                Message::Invalid { id: got, error } => {
                    assert_eq!(got, id, "{v}");
                    assert_eq!(error.code, INVALID_REQUEST);
                    assert!(error.message.starts_with("Invalid request"), "{}", error.message);
                }
                other => panic!("{v} classified as {other:?}"),
            }
        }
    }

    #[test]
    fn error_shapes_are_json_rpc() {
        let e = error_response(&json!(9), &RpcError::new(METHOD_NOT_FOUND, "Method not found: x"));
        assert_eq!(e, json!({"jsonrpc":"2.0","id":9,"error":{"code":-32601,"message":"Method not found: x"}}));
        let r = result_response(&json!("q"), json!({}));
        assert_eq!(r, json!({"jsonrpc":"2.0","id":"q","result":{}}));
    }

    #[test]
    fn parses_single_and_batch_bodies() {
        assert!(matches!(parse_body(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#), Ok(Body::Single(_))));
        assert!(matches!(parse_body(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#), Ok(Body::Batch(v)) if v.len() == 1));
        assert_eq!(parse_body(b"[]").unwrap_err().code, INVALID_REQUEST);
        assert_eq!(parse_body(b"{nope").unwrap_err().code, PARSE_ERROR);
    }

    #[test]
    fn negotiates_versions() {
        assert_eq!(negotiate(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate(Some("2025-11-25")), "2025-11-25");
        assert_eq!(negotiate(Some("2024-11-05")), LATEST_VERSION);
        assert_eq!(negotiate(None), LATEST_VERSION);
        let r = initialize_result("2025-06-18");
        assert_eq!(r["protocolVersion"], "2025-06-18");
        assert_eq!(r["serverInfo"]["name"], "pmxspan");
        assert!(r["capabilities"]["tools"].is_object());
    }
}
