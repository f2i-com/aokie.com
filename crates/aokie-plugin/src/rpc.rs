//! JSON-RPC 2.0 framing over newline-delimited stdio.
//!
//! One JSON object per `\n`-terminated UTF-8 line, no Content-Length
//! framing, max line 1 MiB (DESKTOP_PLUGIN_SDK.md §3). This module
//! owns parse + serialise only; dispatch lives in [`crate::connector`].

use serde_json::{json, Value};

/// Max protocol line size (1 MiB per the SDK contract). Longer lines
/// are rejected with `-32600` and the remainder of the line drained.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

// JSON-RPC 2.0 error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
/// Server-defined code carrying the typed connector error in
/// `error.data = {code, message}` (connector-response.schema.json).
pub const COMMAND_ERROR: i64 = -32000;

/// A parsed incoming message. `id: None` means notification — the
/// plugin must not answer it (JSON-RPC 2.0 §4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct RpcMessage {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// A framing-level failure with everything needed to build the error
/// response line (or to stay silent when the input had no usable id).
#[derive(Debug, Clone, PartialEq)]
pub struct RpcParseError {
    pub id: Option<Value>,
    pub code: i64,
    pub message: String,
}

/// Parse one protocol line. Enforces the 1 MiB cap, JSON validity,
/// `jsonrpc: "2.0"`, and a string `method`.
pub fn parse_line(line: &str) -> Result<RpcMessage, RpcParseError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(RpcParseError {
            id: None,
            code: INVALID_REQUEST,
            message: format!("line exceeds {} bytes", MAX_LINE_BYTES),
        });
    }
    let value: Value = serde_json::from_str(line).map_err(|e| RpcParseError {
        id: None,
        code: PARSE_ERROR,
        message: format!("parse error: {e}"),
    })?;
    let obj = value.as_object().ok_or_else(|| RpcParseError {
        id: None,
        code: INVALID_REQUEST,
        message: "request must be a JSON object".to_string(),
    })?;
    // Pull the id first so later failures can echo it back.
    let id = obj.get("id").cloned().filter(|v| !v.is_null());
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RpcParseError {
            id,
            code: INVALID_REQUEST,
            message: "jsonrpc must be \"2.0\"".to_string(),
        });
    }
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcParseError {
            id: id.clone(),
            code: INVALID_REQUEST,
            message: "method must be a string".to_string(),
        })?
        .to_string();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    Ok(RpcMessage { id, method, params })
}

/// Serialise a success response (no trailing newline — the sink adds it).
pub fn success_line(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// Serialise an error response. `id: None` renders `"id": null`
/// (parse/framing errors where the request id is unknowable).
pub fn error_line(id: Option<&Value>, code: i64, message: &str, data: Option<Value>) -> String {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({
        "jsonrpc": "2.0",
        "id": id.cloned().unwrap_or(Value::Null),
        "error": error,
    })
    .to_string()
}

/// Serialise a plugin → desktop notification (`event.emit`, `log.emit`).
pub fn notification_line(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_with_id_and_params() {
        let msg = parse_line(
            r#"{"jsonrpc":"2.0","id":7,"method":"plugin.health","params":{}}"#,
        )
        .unwrap();
        assert_eq!(msg.id, Some(json!(7)));
        assert_eq!(msg.method, "plugin.health");
        assert_eq!(msg.params, json!({}));
    }

    #[test]
    fn parses_notification_without_id() {
        let msg =
            parse_line(r#"{"jsonrpc":"2.0","method":"event.ack","params":{"x":1}}"#).unwrap();
        assert_eq!(msg.id, None);
    }

    #[test]
    fn missing_params_defaults_to_null() {
        let msg = parse_line(r#"{"jsonrpc":"2.0","id":"a","method":"plugin.health"}"#).unwrap();
        assert_eq!(msg.params, Value::Null);
    }

    #[test]
    fn rejects_invalid_json_as_parse_error() {
        let err = parse_line("{not json").unwrap_err();
        assert_eq!(err.code, PARSE_ERROR);
        assert_eq!(err.id, None);
    }

    #[test]
    fn rejects_non_object() {
        let err = parse_line("[1,2,3]").unwrap_err();
        assert_eq!(err.code, INVALID_REQUEST);
    }

    #[test]
    fn rejects_wrong_jsonrpc_version_but_keeps_id() {
        let err = parse_line(r#"{"jsonrpc":"1.0","id":3,"method":"m"}"#).unwrap_err();
        assert_eq!(err.code, INVALID_REQUEST);
        assert_eq!(err.id, Some(json!(3)));
    }

    #[test]
    fn rejects_missing_method() {
        let err = parse_line(r#"{"jsonrpc":"2.0","id":3}"#).unwrap_err();
        assert_eq!(err.code, INVALID_REQUEST);
        assert_eq!(err.id, Some(json!(3)));
    }

    #[test]
    fn rejects_oversized_line() {
        let padding = "x".repeat(MAX_LINE_BYTES);
        let line = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{padding}"}}"#);
        let err = parse_line(&line).unwrap_err();
        assert_eq!(err.code, INVALID_REQUEST);
        assert!(err.message.contains("exceeds"));
    }

    #[test]
    fn line_at_cap_is_accepted() {
        // Exactly-1MiB lines are legal; only strictly-larger is rejected.
        let overhead = r#"{"jsonrpc":"2.0","id":1,"method":"m","params":""}"#.len();
        let pad = "y".repeat(MAX_LINE_BYTES - overhead);
        let line = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"m","params":"{pad}"}}"#);
        assert_eq!(line.len(), MAX_LINE_BYTES);
        parse_line(&line).unwrap();
    }

    #[test]
    fn success_and_error_lines_are_single_line_json() {
        let s = success_line(&json!(1), json!({"ok": true}));
        assert!(!s.contains('\n'));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));

        let e = error_line(
            Some(&json!(1)),
            COMMAND_ERROR,
            "boom",
            Some(json!({"code": "command_failed", "message": "boom"})),
        );
        let v: Value = serde_json::from_str(&e).unwrap();
        assert_eq!(v["error"]["code"], json!(COMMAND_ERROR));
        assert_eq!(v["error"]["data"]["code"], json!("command_failed"));
    }

    #[test]
    fn error_line_without_id_uses_null() {
        let e = error_line(None, PARSE_ERROR, "bad", None);
        let v: Value = serde_json::from_str(&e).unwrap();
        assert_eq!(v["id"], Value::Null);
        assert!(v["error"].get("data").is_none());
    }

    #[test]
    fn notification_has_no_id() {
        let n = notification_line("event.emit", json!({"event": {}}));
        let v: Value = serde_json::from_str(&n).unwrap();
        assert!(v.get("id").is_none());
        assert_eq!(v["method"], json!("event.emit"));
    }
}
