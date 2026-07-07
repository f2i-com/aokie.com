//! Plugin → Desktop notifications: `event.emit` and `log.emit`.
//!
//! The [`Sink`] trait abstracts "one protocol line out" so the mock
//! lifecycle and command handlers are unit-testable without a real
//! stdout; the binary wires in [`StdoutSink`], the ONLY stdout writer
//! in the process (SDK rule: never write non-protocol output there).
//!
//! Reliability coupling (AOKIE_PLUGIN_CONTRACT.md §5): essential
//! events are written to the [`Outbox`](crate::outbox::Outbox) as
//! `pending` BEFORE emission, marked `sent` on success and `failed`
//! (with retry/backoff bookkeeping) when the write to Desktop fails.

use std::io::{self, Write};

use aokie_core::events::DesktopEvent;
use serde_json::json;

use crate::outbox::{Outbox, TARGET_DESKTOP};
use crate::rpc;

/// Event names whose raw record must survive a Desktop restart —
/// always routed through the outbox (contract §5).
pub const ESSENTIAL_EVENTS: &[&str] = &[
    "aokie.call.incoming",
    "aokie.call.answered",
    "aokie.call.turn.final",
    "aokie.call.ended",
    "aokie.sms.received",
    "aokie.sms.sent",
    "aokie.hardware.error",
];

pub fn is_essential(name: &str) -> bool {
    ESSENTIAL_EVENTS.contains(&name)
}

/// One outbound protocol line. Implementations must append the
/// newline themselves and flush per line (Desktop reads NDJSON).
pub trait Sink {
    fn send_line(&mut self, line: &str) -> io::Result<()>;
}

/// The production sink: locked stdout, one flush per line.
pub struct StdoutSink {
    out: io::Stdout,
}

impl StdoutSink {
    pub fn new() -> Self {
        StdoutSink { out: io::stdout() }
    }
}

impl Default for StdoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for StdoutSink {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        let mut lock = self.out.lock();
        lock.write_all(line.as_bytes())?;
        lock.write_all(b"\n")?;
        lock.flush()
    }
}

/// Test sink capturing every protocol line.
#[derive(Default)]
pub struct VecSink {
    pub lines: Vec<String>,
    /// When set, `send_line` fails — for exercising the outbox
    /// failure path.
    pub fail: bool,
}

impl Sink for VecSink {
    fn send_line(&mut self, line: &str) -> io::Result<()> {
        if self.fail {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink closed"));
        }
        self.lines.push(line.to_string());
        Ok(())
    }
}

/// Emit one event as an `event.emit` notification.
///
/// * `force_outbox` routes even non-essential events through the
///   outbox (the dev-mode scripted lifecycle records every step).
/// * Essential events are ALWAYS outboxed: insert `pending` →
///   emit → `sent`, or → `failed` with the error recorded.
pub fn emit_event(
    sink: &mut dyn Sink,
    outbox: &Outbox,
    event: &DesktopEvent,
    force_outbox: bool,
) -> Result<(), String> {
    let outboxed = force_outbox || is_essential(&event.name);
    if outboxed {
        outbox
            .insert_pending(event, TARGET_DESKTOP)
            .map_err(|e| format!("outbox write failed for {}: {e}", event.idempotency_key))?;
    }
    let line = rpc::notification_line("event.emit", json!({ "event": event }));
    match sink.send_line(&line) {
        Ok(()) => {
            if outboxed {
                outbox
                    .mark_sent(&event.idempotency_key)
                    .map_err(|e| format!("outbox mark_sent failed: {e}"))?;
            }
            Ok(())
        }
        Err(e) => {
            if outboxed {
                // Best-effort: the row stays pending/failed for the
                // retry loop even if this bookkeeping write fails too.
                let _ = outbox.mark_failed(&event.idempotency_key, &e.to_string());
            }
            Err(format!("event.emit failed for {}: {e}", event.name))
        }
    }
}

/// Emit a `log.emit` notification. The MESSAGE MUST ALREADY BE
/// REDACTED (`aokie_core::redact`) — this is the same rule as the
/// legacy `audit` helper: the transport does not redact for you.
pub fn emit_log(sink: &mut dyn Sink, level: &str, message: &str) {
    // ≤ 2 KiB per the SDK; truncate defensively rather than have
    // Desktop drop the whole line.
    let message: String = message.chars().take(2000).collect();
    let line = rpc::notification_line("log.emit", json!({"level": level, "message": message}));
    // A failed log line is not worth crashing over; stderr is the
    // fallback channel and is captured by Desktop's ring buffer.
    if let Err(e) = sink.send_line(&line) {
        eprintln!("[aokie-plugin] log.emit failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::OutboxStatus;
    use aokie_core::events::aokie_event;
    use serde_json::{json, Value};

    #[test]
    fn essential_set_matches_contract() {
        assert!(is_essential("aokie.call.incoming"));
        assert!(is_essential("aokie.sms.sent"));
        assert!(is_essential("aokie.hardware.error"));
        assert!(!is_essential("aokie.dongle.detected"));
        assert!(!is_essential("aokie.call.turn.partial"));
        assert!(!is_essential("aokie.call.rejected"));
    }

    #[test]
    fn essential_event_is_outboxed_and_marked_sent() {
        let outbox = Outbox::open_in_memory().unwrap();
        let mut sink = VecSink::default();
        let ev = aokie_event("aokie.call.incoming", "call_a", json!({"from": "x"}));
        emit_event(&mut sink, &outbox, &ev, false).unwrap();

        assert_eq!(sink.lines.len(), 1);
        let v: Value = serde_json::from_str(&sink.lines[0]).unwrap();
        assert_eq!(v["method"], json!("event.emit"));
        assert_eq!(v["params"]["event"]["name"], json!("aokie.call.incoming"));
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
    }

    #[test]
    fn non_essential_event_skips_outbox_unless_forced() {
        let outbox = Outbox::open_in_memory().unwrap();
        let mut sink = VecSink::default();
        let ev = aokie_event("aokie.dongle.detected", "call_b", json!({}));
        emit_event(&mut sink, &outbox, &ev, false).unwrap();
        assert_eq!(outbox.status_of(&ev.idempotency_key).unwrap(), None);

        emit_event(&mut sink, &outbox, &ev, true).unwrap();
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
    }

    #[test]
    fn sink_failure_leaves_row_failed_for_retry() {
        let outbox = Outbox::open_in_memory().unwrap();
        let mut sink = VecSink {
            fail: true,
            ..Default::default()
        };
        let ev = aokie_event("aokie.sms.sent", "sms_1", json!({}));
        let err = emit_event(&mut sink, &outbox, &ev, false).unwrap_err();
        assert!(err.contains("event.emit failed"), "got: {err}");
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Failed)
        );
        let rows = outbox.retryable(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempts, 1);
    }

    #[test]
    fn log_emit_truncates_to_limit() {
        let mut sink = VecSink::default();
        emit_log(&mut sink, "info", &"a".repeat(5000));
        let v: Value = serde_json::from_str(&sink.lines[0]).unwrap();
        assert_eq!(v["method"], json!("log.emit"));
        assert_eq!(v["params"]["message"].as_str().unwrap().len(), 2000);
    }
}
