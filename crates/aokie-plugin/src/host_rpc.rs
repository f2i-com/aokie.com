//! Plugin → Desktop REQUESTS (guide §9.3/P1-14: "make the stdio protocol
//! genuinely bidirectional").
//!
//! The desktop has serviced plugin-originated `flow.run` requests for a while
//! (`plugins/rpc.rs` answers them via `RpcClient::respond`), but the plugin
//! never had the CLIENT half: a way to send a request with an id over stdout
//! and correlate the response arriving on stdin. This module is that half.
//!
//! Shape:
//! - Any thread calls [`HostRpc::begin`] to mint an id + request line + a
//!   receiver, writes the line through its own [`Sink`] (stdout writes are
//!   line-atomic), and blocks on the receiver with a timeout.
//! - The MAIN stdio thread feeds every incoming line to
//!   [`HostRpc::try_route_response`]; response-shaped lines (id + result or
//!   error, NO method) resolve the matching receiver, everything else flows
//!   to the normal RPC dispatch untouched.
//! - A timed-out request is [`HostRpc::forget`]-ten so a late response can't
//!   leak memory or resolve a recycled id.
//!
//! First consumer: mid-call business lookups (`flow.run` of a read-only pack
//! flow) so the live agent can answer from RECORDS instead of memory.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// One pending request's resolution.
pub type HostResult = Result<Value, String>;

pub struct HostRpc {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<HostResult>>>,
}

impl HostRpc {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            // Start high so plugin-minted ids can never collide with the ids
            // the DESKTOP mints for its own requests to us (small integers) —
            // purely for log readability; correlation maps are separate.
            next_id: AtomicU64::new(1_000_000),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Mint a request: returns (id, the JSON-RPC line to write, the receiver
    /// the response will arrive on). The caller writes the line via its own
    /// sink and blocks on the receiver; on timeout it MUST call [`forget`].
    pub fn begin(&self, method: &str, params: Value) -> (u64, String, mpsc::Receiver<HostResult>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        (id, line, rx)
    }

    /// Drop a pending request (timeout/abandon): a late response then routes
    /// to nobody instead of a recycled receiver.
    pub fn forget(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// Route a RESPONSE-shaped incoming line (`id` + `result`/`error`, no
    /// `method`) to its waiting request. Returns true when the line was a
    /// response to one of ours (consumed), false when the caller should
    /// dispatch it normally. Unknown ids are consumed too (late responses
    /// after a timeout) — they must not fall through to the RPC dispatcher,
    /// which would answer them with an error and confuse the host.
    pub fn try_route_response(&self, v: &Value) -> bool {
        if v.get("method").is_some() {
            return false; // a request/notification, never a response
        }
        let Some(id) = v.get("id").and_then(Value::as_u64) else {
            return false;
        };
        let has_result = v.get("result").is_some();
        let has_error = v.get("error").is_some();
        if !has_result && !has_error {
            return false;
        }
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let outcome = if has_result {
                Ok(v["result"].clone())
            } else {
                let e = &v["error"];
                Err(format!(
                    "host error {}: {}",
                    e.get("code").and_then(Value::as_i64).unwrap_or(0),
                    e.get("message").and_then(Value::as_str).unwrap_or("?")
                ))
            };
            let _ = tx.send(outcome);
        } else {
            eprintln!("[aokie-plugin] late host response for forgotten request {id} — dropped");
        }
        true
    }

    /// How many requests are in flight (tests + diagnostics).
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn request_line_shape_and_response_routing() {
        let rpc = HostRpc::new();
        let (id, line, rx) = rpc.begin("flow.run", json!({"flowSlug": "business-lookup"}));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["method"], json!("flow.run"));
        assert_eq!(v["id"], json!(id));
        assert_eq!(v["params"]["flowSlug"], json!("business-lookup"));
        assert_eq!(rpc.pending_count(), 1);

        // The matching response resolves the receiver and is consumed.
        let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"status": "succeeded"}});
        assert!(rpc.try_route_response(&resp));
        let got = rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .unwrap();
        assert_eq!(got["status"], json!("succeeded"));
        assert_eq!(rpc.pending_count(), 0);
    }

    #[test]
    fn error_responses_and_late_responses() {
        let rpc = HostRpc::new();
        let (id, _line, rx) = rpc.begin("flow.run", json!({}));
        let resp = json!({"id": id, "error": {"code": -32000, "message": "not linked"}});
        assert!(rpc.try_route_response(&resp));
        let err = rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap()
            .unwrap_err();
        assert!(err.contains("not linked"), "{err}");

        // Timeout path: forget, then the late response is consumed silently.
        let (id2, _line, rx2) = rpc.begin("flow.run", json!({}));
        rpc.forget(id2);
        assert!(rx2.recv_timeout(Duration::from_millis(50)).is_err());
        let late = json!({"id": id2, "result": {}});
        assert!(
            rpc.try_route_response(&late),
            "late responses are consumed, not dispatched"
        );
    }

    #[test]
    fn requests_and_notifications_pass_through() {
        let rpc = HostRpc::new();
        // Host requests (have method) and notifications never route here.
        assert!(!rpc.try_route_response(&json!({"id": 5, "method": "connector.request"})));
        assert!(!rpc.try_route_response(&json!({"method": "event.ack", "params": {}})));
        // A response-shaped line with a non-numeric id is not ours.
        assert!(!rpc.try_route_response(&json!({"id": "srv-1", "result": {}})));
    }
}
