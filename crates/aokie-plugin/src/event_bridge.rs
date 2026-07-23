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
    crate::contract::events::CALL_INCOMING,
    crate::contract::events::CALL_ANSWERED,
    crate::contract::events::CALL_TURN_FINAL,
    crate::contract::events::CALL_TURN_CORRECTED,
    crate::contract::events::CALL_TRANSCRIPT_SETTLED,
    crate::contract::events::CALL_ENDED,
    crate::contract::events::CALL_ASSISTANCE_REQUESTED,
    crate::contract::events::CALL_ASSISTANCE_RESOLVED,
    crate::contract::events::APPOINTMENT_REQUESTED,
    crate::contract::events::SMS_RECEIVED,
    crate::contract::events::SMS_SENT,
    crate::contract::events::SMS_FAILED,
    crate::contract::events::MANAGER_ACTION,
    crate::contract::events::HARDWARE_ERROR,
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

/// How emission success is bookkept in the outbox (audit INT-003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitMode {
    /// Non-ack hosts with the EXPLICIT compatibility override
    /// (`AOKIE_ALLOW_LEGACY_HOST=1`) or dev mode: a successful WRITE marks
    /// the row `sent` — the pre-ack behaviour.
    Legacy,
    /// Ack-capable hosts: a successful write only schedules the next
    /// re-emission (`mark_emitted`); the row becomes `sent` when the host's
    /// `event.ack` notification confirms it DURABLY received the envelope.
    AckExpected,
    /// AOK-DUR-001 item 3: the production default for a host WITHOUT
    /// `eventAck`. Essential events are journaled to the outbox but HELD —
    /// not emitted — because a non-ack host cannot confirm durable receipt
    /// (and re-delivery to it would create duplicate business records). The
    /// rows deliver when an ack-capable host connects; health reports the
    /// incompatible host meanwhile.
    RequireAck,
}

/// Whether the operator explicitly accepted legacy (write-means-sent)
/// delivery to a host without `eventAck`.
pub fn legacy_host_allowed() -> bool {
    std::env::var("AOKIE_ALLOW_LEGACY_HOST").as_deref() == Ok("1")
}

impl EmitMode {
    /// Mode for the connected host. `allow_legacy` = dev mode or the
    /// explicit `AOKIE_ALLOW_LEGACY_HOST=1` override — production non-ack
    /// hosts get [`EmitMode::RequireAck`].
    pub fn for_host(ack: bool, allow_legacy: bool) -> Self {
        if ack {
            EmitMode::AckExpected
        } else if allow_legacy {
            EmitMode::Legacy
        } else {
            EmitMode::RequireAck
        }
    }
}

/// Emit one event as an `event.emit` notification.
///
/// * `force_outbox` routes even non-essential events through the
///   outbox (the dev-mode scripted lifecycle records every step).
/// * Essential events are ALWAYS outboxed: insert `pending` → emit →
///   then either `sent` (Legacy) or "await ack" (AckExpected); a failed
///   write records `failed` with the error either way.
pub fn emit_event(
    sink: &mut dyn Sink,
    outbox: &Outbox,
    event: &DesktopEvent,
    force_outbox: bool,
    mode: EmitMode,
) -> Result<(), String> {
    let outboxed = force_outbox || is_essential(&event.name);
    if outboxed {
        let outcome = outbox
            .insert_pending(event, TARGET_DESKTOP)
            .map_err(|e| format!("outbox write failed for {}: {e}", event.idempotency_key))?;
        match outcome {
            crate::outbox::InsertOutcome::PayloadCollision => {
                // A DIFFERENT occurrence collided on this key (key-derivation
                // bug — audit AOK-EVENT-001). The stored row is authoritative;
                // emitting the rejected content under its key would make the
                // host record the wrong occurrence. Refuse, loudly.
                return Err(format!(
                    "event.emit refused for {}: idempotency key {} already holds different content",
                    event.name, event.idempotency_key
                ));
            }
            crate::outbox::InsertOutcome::QuarantinedProtectFailed => {
                // AOK-DUR-001: the sensitive payload could not be protected at
                // rest, so it was quarantined (typed dead row, no payload).
                // Emitting anyway would report success for an event whose
                // durability just failed — refuse instead.
                return Err(format!(
                    "event.emit refused for {}: payload protection failed — event quarantined in the outbox",
                    event.name
                ));
            }
            crate::outbox::InsertOutcome::Inserted | crate::outbox::InsertOutcome::Duplicate => {}
        }
        if mode == EmitMode::RequireAck {
            // AOK-DUR-001 item 3: the host cannot acknowledge durable receipt.
            // The event is safely journaled; HOLD it for an ack-capable host
            // instead of degrading to write-means-sent (or duplicating records
            // via un-deduped re-delivery).
            return Err(format!(
                "event.emit held for {}: host does not support eventAck — event retained in the outbox (set AOKIE_ALLOW_LEGACY_HOST=1 to accept legacy delivery)",
                event.name
            ));
        }
    }
    let line = rpc::notification_line("event.emit", json!({ "event": event }));
    match sink.send_line(&line) {
        Ok(()) => {
            if outboxed {
                match mode {
                    EmitMode::Legacy => {
                        outbox
                            .mark_sent(&event.idempotency_key)
                            .map_err(|e| format!("outbox mark_sent failed: {e}"))?;
                    }
                    EmitMode::AckExpected => {
                        outbox
                            .mark_emitted(&event.idempotency_key, None)
                            .map_err(|e| format!("outbox mark_emitted failed: {e}"))?;
                    }
                    // Outboxed + RequireAck returned early above (held).
                    EmitMode::RequireAck => {}
                }
            }
            Ok(())
        }
        Err(e) => {
            if outboxed {
                // Best-effort: the row stays pending/failed for the
                // retry loop even if this bookkeeping write fails too.
                let _ = outbox.mark_failed(&event.idempotency_key, &e.to_string(), None);
            }
            Err(format!("event.emit failed for {}: {e}", event.name))
        }
    }
}

/// One replay pass (ack mode): re-emit every DUE unacknowledged row —
/// crash recovery ("written to the outbox but never delivered"), lost
/// host writes, and un-acked emissions all funnel through here with the
/// SAME idempotency key, so the host's receipt dedupe makes redelivery
/// harmless. Returns how many rows were re-emitted. The caller owns the
/// timer (the plugin's replay thread; tests call it directly).
pub fn replay_once(sink: &mut dyn Sink, outbox: &Outbox, limit: u32) -> usize {
    let rows = match outbox.due_for_retry(limit) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("[aokie-plugin] outbox replay query failed: {e}");
            return 0;
        }
    };
    let mut emitted = 0usize;
    for row in rows {
        let event: DesktopEvent = match serde_json::from_str(&row.payload_json) {
            Ok(ev) => ev,
            Err(e) => {
                // Unparseable payload can never deliver — typed dead letter
                // (AOK-DUR-001; undecryptable payloads are already filtered
                // and dead-lettered inside due_for_retry, this is the JSON
                // backstop for a corrupt legacy row).
                eprintln!(
                    "[aokie-plugin] outbox row {} has an unparseable payload ({e}) — dead-lettering",
                    row.idempotency_key
                );
                let _ = outbox.mark_dead_typed(
                    &row.idempotency_key,
                    &format!("payload_unreadable: not valid JSON ({e})"),
                );
                continue;
            }
        };
        let line = rpc::notification_line("event.emit", json!({ "event": event }));
        // Bookkeeping is conditional on the attempt GENERATION this pass read
        // (audit AOK-EVENT-001): if the ack thread marked the row `sent` — or
        // another pass already booked this generation — the update is a no-op
        // instead of dragging the row backward or double-counting.
        match sink.send_line(&line) {
            Ok(()) => {
                let _ = outbox.mark_emitted(&row.idempotency_key, Some(row.attempts));
                emitted += 1;
                if row.attempts > 0 {
                    eprintln!(
                        "[aokie-plugin] replayed unacknowledged event {} (attempt {})",
                        row.idempotency_key,
                        row.attempts + 1
                    );
                }
            }
            Err(e) => {
                let _ =
                    outbox.mark_failed(&row.idempotency_key, &e.to_string(), Some(row.attempts));
            }
        }
    }
    emitted
}

/// Spawn the ack-mode replay thread: an initial pass picks up rows a
/// previous process crashed with (pending/failed, never acknowledged),
/// then every second re-emits whatever has come due, and periodically
/// prunes acknowledged rows past retention. Runs for the process's
/// life — the main loop exits on stdin EOF, taking this with it.
/// Seconds since the Unix epoch — the replay heartbeat's clock.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Spawns the replay thread and returns its HEARTBEAT (audit AOK-OUTBOX-002):
/// the thread stamps it every loop; `plugin.health` reads it so a dead or
/// stalled replay thread degrades readiness instead of silently freezing
/// durable delivery. Value 0 = the thread failed to start at all.
pub fn spawn_replay_thread(
    outbox_path: std::path::PathBuf,
) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    use std::sync::atomic::Ordering;
    let heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(unix_now()));
    let beat = heartbeat.clone();
    let _ = std::thread::Builder::new()
        .name("aokie-outbox-replay".into())
        .spawn(move || {
            let outbox = match Outbox::open(&outbox_path) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!(
                        "[aokie-plugin] replay thread cannot open outbox {}: {e}",
                        outbox_path.display()
                    );
                    beat.store(0, Ordering::Relaxed);
                    return;
                }
            };
            let mut sink = StdoutSink::new();
            // Let the host finish its side of the handshake before the
            // crash-recovery pass floods it.
            std::thread::sleep(std::time::Duration::from_secs(2));
            let recovered = replay_once(&mut sink, &outbox, 64);
            if recovered > 0 {
                eprintln!("[aokie-plugin] replayed {recovered} undelivered event(s) from a previous run");
            }
            let mut last_prune = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                beat.store(unix_now(), Ordering::Relaxed);
                replay_once(&mut sink, &outbox, 16);
                if last_prune.elapsed().as_secs() >= 600 {
                    last_prune = std::time::Instant::now();
                    match outbox.prune_sent(crate::outbox::SENT_RETENTION_DAYS) {
                        Ok(0) | Err(_) => {}
                        Ok(n) => eprintln!(
                            "[aokie-plugin] pruned {n} acknowledged outbox row(s) past {} day retention",
                            crate::outbox::SENT_RETENTION_DAYS
                        ),
                    }
                    // Dead-letter retention rides the same maintenance tick
                    // (audit AOK-OUTBOX-001): failed transcript/SMS payloads
                    // must not sit in the file forever.
                    match outbox.prune_dead(crate::outbox::DEAD_RETENTION_DAYS) {
                        Ok(0) | Err(_) => {}
                        Ok(n) => eprintln!(
                            "[aokie-plugin] pruned {n} dead outbox row(s) past {} day retention",
                            crate::outbox::DEAD_RETENTION_DAYS
                        ),
                    }
                }
            }
        });
    heartbeat
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
        assert!(is_essential(crate::contract::events::CALL_INCOMING));
        assert!(is_essential(
            crate::contract::events::CALL_ASSISTANCE_REQUESTED
        ));
        assert!(is_essential(
            crate::contract::events::CALL_ASSISTANCE_RESOLVED
        ));
        assert!(is_essential(crate::contract::events::CALL_TURN_CORRECTED));
        assert!(is_essential(
            crate::contract::events::CALL_TRANSCRIPT_SETTLED
        ));
        assert!(is_essential(crate::contract::events::SMS_SENT));
        assert!(is_essential(crate::contract::events::HARDWARE_ERROR));
        assert!(!is_essential(crate::contract::events::DONGLE_DETECTED));
        assert!(!is_essential(crate::contract::events::CALL_TURN_PARTIAL));
        assert!(!is_essential(crate::contract::events::CALL_REJECTED));
    }

    #[test]
    fn essential_event_is_outboxed_and_marked_sent() {
        let outbox = Outbox::open_in_memory().unwrap();
        let mut sink = VecSink::default();
        let ev = aokie_event(
            crate::contract::events::CALL_INCOMING,
            "call_a",
            json!({"from": "x"}),
        );
        emit_event(&mut sink, &outbox, &ev, false, EmitMode::Legacy).unwrap();

        assert_eq!(sink.lines.len(), 1);
        let v: Value = serde_json::from_str(&sink.lines[0]).unwrap();
        assert_eq!(v["method"], json!("event.emit"));
        assert_eq!(
            v["params"]["event"]["name"],
            json!(crate::contract::events::CALL_INCOMING)
        );
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
    }

    #[test]
    fn non_essential_event_skips_outbox_unless_forced() {
        let outbox = Outbox::open_in_memory().unwrap();
        let mut sink = VecSink::default();
        let ev = aokie_event(
            crate::contract::events::DONGLE_DETECTED,
            "call_b",
            json!({}),
        );
        emit_event(&mut sink, &outbox, &ev, false, EmitMode::Legacy).unwrap();
        assert_eq!(outbox.status_of(&ev.idempotency_key).unwrap(), None);

        emit_event(&mut sink, &outbox, &ev, true, EmitMode::Legacy).unwrap();
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
        let ev = aokie_event(crate::contract::events::SMS_SENT, "sms_1", json!({}));
        let err = emit_event(&mut sink, &outbox, &ev, false, EmitMode::Legacy).unwrap_err();
        assert!(err.contains("event.emit failed"), "got: {err}");
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Failed)
        );
        let rows = outbox.retryable(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempts, 1);
    }

    /// Audit INT-003, the crash-boundary table:
    /// - crash BEFORE send: row pending, no next_attempt_at → replayed;
    /// - crash AFTER send, before ack: row pending with a due backoff →
    ///   replayed with the SAME idempotency key (host dedupes);
    /// - after ack: row sent → never replayed.
    #[test]
    fn ack_mode_replays_until_acknowledged() {
        let outbox = Outbox::open_in_memory().unwrap();
        let mut sink = VecSink::default();
        let ev = aokie_event(
            crate::contract::events::CALL_INCOMING,
            "call_a",
            json!({"from": "x"}),
        );

        // Live emission in ack mode: written, but NOT sent — awaiting ack.
        emit_event(&mut sink, &outbox, &ev, false, EmitMode::AckExpected).unwrap();
        assert_eq!(sink.lines.len(), 1);
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Pending)
        );

        // Freshly emitted → backoff gates the replay loop (nothing due).
        assert_eq!(replay_once(&mut sink, &outbox, 10), 0);

        // Simulate the backoff elapsing (crash-recovery equivalence: a row
        // whose next attempt is due). Then the replay pass re-emits the SAME
        // envelope with the SAME idempotency key.
        force_due(&outbox, &ev.idempotency_key);
        assert_eq!(replay_once(&mut sink, &outbox, 10), 1);
        assert_eq!(sink.lines.len(), 2);
        let a: Value = serde_json::from_str(&sink.lines[0]).unwrap();
        let b: Value = serde_json::from_str(&sink.lines[1]).unwrap();
        assert_eq!(
            a["params"]["event"]["idempotencyKey"], b["params"]["event"]["idempotencyKey"],
            "replay uses the same occurrence id so the host can dedupe"
        );

        // The host acks → sent; no more replays even when 'due'.
        outbox.mark_sent(&ev.idempotency_key).unwrap();
        force_due(&outbox, &ev.idempotency_key);
        assert_eq!(replay_once(&mut sink, &outbox, 10), 0);
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
    }

    /// Crash BEFORE the first send: the row sits pending with no
    /// next_attempt_at — the replay thread's startup pass delivers it.
    #[test]
    fn ack_mode_startup_pass_delivers_rows_a_crash_stranded() {
        let outbox = Outbox::open_in_memory().unwrap();
        let ev = aokie_event(
            crate::contract::events::SMS_RECEIVED,
            "sms_9",
            json!({"from": "+61", "body": "hi"}),
        );
        // insert_pending happened, then the process died before send_line.
        outbox.insert_pending(&ev, TARGET_DESKTOP).unwrap();

        let mut sink = VecSink::default();
        assert_eq!(replay_once(&mut sink, &outbox, 10), 1);
        let v: Value = serde_json::from_str(&sink.lines[0]).unwrap();
        assert_eq!(v["method"], json!("event.emit"));
        assert_eq!(
            v["params"]["event"]["idempotencyKey"],
            json!(ev.idempotency_key)
        );
        // Still awaiting ack — not marked sent by mere re-emission.
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Pending)
        );
    }

    /// Backdate a row's next_attempt_at so it is due NOW (test helper for
    /// the backoff window without sleeping).
    fn force_due(outbox: &Outbox, key: &str) {
        // Reuse the public surface: a fresh insert keeps NULL, but an
        // emitted row needs its schedule rewound — go through a tiny SQL
        // shim exposed for tests via mark-then-rewind semantics.
        outbox.rewind_next_attempt_for_tests(key);
    }

    // ── AOK-DUR-001 ─────────────────────────────────────────────────────────

    #[test]
    fn for_host_matrix_requires_ack_in_production() {
        assert_eq!(EmitMode::for_host(true, false), EmitMode::AckExpected);
        assert_eq!(EmitMode::for_host(true, true), EmitMode::AckExpected);
        assert_eq!(
            EmitMode::for_host(false, true),
            EmitMode::Legacy,
            "explicit override only"
        );
        assert_eq!(
            EmitMode::for_host(false, false),
            EmitMode::RequireAck,
            "production default"
        );
    }

    /// Item 3: on a host without eventAck (no override), an essential event
    /// is journaled but HELD — nothing goes to the sink, the row stays
    /// pending for an ack-capable host, and the caller gets a typed error
    /// instead of a fake success. Non-essential events still flow.
    #[test]
    fn require_ack_holds_essential_events_in_the_outbox() {
        let outbox = Outbox::open_in_memory().unwrap();
        let mut sink = VecSink::default();
        let ev = aokie_event(
            crate::contract::events::SMS_RECEIVED,
            "sms_hold",
            json!({"from": "+61", "body": "hi"}),
        );
        let err = emit_event(&mut sink, &outbox, &ev, false, EmitMode::RequireAck).unwrap_err();
        assert!(err.contains("held"), "typed hold: {err}");
        assert!(sink.lines.is_empty(), "nothing emitted to a non-ack host");
        assert_eq!(
            outbox.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Pending),
            "the business event is durably retained for an ack-capable host"
        );

        // Non-essential events carry no durability claim and still emit.
        let info = aokie_event(crate::contract::events::DONGLE_DETECTED, "d1", json!({}));
        emit_event(&mut sink, &outbox, &info, false, EmitMode::RequireAck).unwrap();
        assert_eq!(sink.lines.len(), 1);
    }

    /// Item 1: a quarantined insert (protect failure) refuses emission — the
    /// host never receives an event whose local durability failed.
    #[test]
    fn quarantined_event_is_not_emitted() {
        let outbox = crate::outbox::Outbox::open_in_memory_with_protection(
            crate::outbox::PayloadProtection::Unavailable,
        )
        .unwrap();
        let mut sink = VecSink::default();
        let ev = aokie_event(
            crate::contract::events::CALL_TURN_FINAL,
            "call_qq",
            json!({"text": "secret"}),
        );
        let err = emit_event(&mut sink, &outbox, &ev, false, EmitMode::AckExpected).unwrap_err();
        assert!(err.contains("quarantined"), "typed: {err}");
        assert!(sink.lines.is_empty(), "no emission after a protect failure");
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
