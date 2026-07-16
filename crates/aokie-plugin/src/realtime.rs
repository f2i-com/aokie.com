//! Volatile realtime lane (guide §9.1/§9.2, v1): bounded, throttled,
//! DROPPABLE `realtime.emit` notifications for live UI observation — the
//! caller's in-progress partial transcript and the session phase. These
//! frames are NOT events: never journalled, never acked, never flow
//! dispatched; the desktop fans them out to local observers and forgets
//! them. The durable `event.emit` plane is untouched.

use serde_json::json;

/// Producer-side frame cap (guide: keep frames small even though the stdio
/// protocol permits 1 MiB lines).
const MAX_PARTIAL_CHARS: usize = 2048;

/// Minimum interval between `user.partial` frames — the guide's 2-4 Hz UI
/// publication rate, enforced at the producer.
const PARTIAL_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

/// Per-call sequencer + throttle for the volatile lane. One lane per call
/// epoch; recreated in the per-call reset block, so `seq`/`revision` never
/// leak across calls. All methods return the JSON-RPC notification LINE to
/// write (None = throttled/unchanged) — pure against an injected `now`, so
/// the whole thing unit-tests without a radio.
pub struct RealtimeLane {
    call_id: String,
    call_epoch: u64,
    session_nonce: String,
    seq: u64,
    started: std::time::Instant,
    last_partial_at: Option<std::time::Instant>,
    partial_revision: u64,
    turn_id: u64,
    delivery_revision: u64,
    last_phase: Option<&'static str>,
}

impl RealtimeLane {
    pub fn new(call_id: String, call_epoch: u64, now: std::time::Instant) -> Self {
        Self {
            call_id,
            call_epoch,
            session_nonce: uuid::Uuid::new_v4().simple().to_string(),
            seq: 0,
            started: now,
            last_partial_at: None,
            partial_revision: 0,
            turn_id: 1,
            delivery_revision: 0,
            last_phase: None,
        }
    }

    fn line(
        &mut self,
        kind: &str,
        extra: serde_json::Value,
        data: serde_json::Value,
        now: std::time::Instant,
    ) -> String {
        self.seq += 1;
        let mut frame = json!({
            "schemaVersion": 1,
            "callId": self.call_id,
            "callEpoch": self.call_epoch,
            "sessionNonce": self.session_nonce,
            "seq": self.seq,
            "occurredAt": aokie_core::events::now_iso8601(),
            "monotonicMs": now.duration_since(self.started).as_millis() as u64,
            "kind": kind,
            "data": data,
        });
        if let (Some(obj), Some(e)) = (frame.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                obj.insert(k.clone(), v.clone());
            }
        }
        json!({ "jsonrpc": "2.0", "method": "realtime.emit", "params": { "frame": frame } })
            .to_string()
    }

    /// Throttled live caption of the caller's in-progress utterance. The UI
    /// replaces the previous partial by `(callId, turnId, revision)`.
    pub fn user_partial(&mut self, text: &str, now: std::time::Instant) -> Option<String> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if let Some(at) = self.last_partial_at {
            if now.duration_since(at) < PARTIAL_MIN_INTERVAL {
                return None;
            }
        }
        self.last_partial_at = Some(now);
        self.partial_revision += 1;
        let clipped: String = text.chars().take(MAX_PARTIAL_CHARS).collect();
        Some(self.line(
            "user.partial",
            json!({ "turnId": format!("t{}", self.turn_id), "revision": self.partial_revision }),
            json!({ "providerStableText": clipped, "unstableText": "" }),
            now,
        ))
    }

    /// The caller's turn finalized (the durable turn event is the authority):
    /// later partials belong to the NEXT turn at revision 1.
    pub fn turn_final(&mut self) {
        self.turn_id += 1;
        self.partial_revision = 0;
        self.delivery_revision = 0;
        self.last_partial_at = None;
    }

    /// One frame per SPOKEN reply sentence (sentence-rate is inherently
    /// bounded — no throttle needed). `state` per the guide: sent_to_sco /
    /// estimated_playing / interrupted.
    pub fn delivery(&mut self, text: &str, state: &str, now: std::time::Instant) -> Option<String> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        self.delivery_revision += 1;
        let clipped: String = text.chars().take(MAX_PARTIAL_CHARS).collect();
        Some(self.line(
            "assistant.delivery",
            json!({ "turnId": format!("t{}", self.turn_id), "revision": self.delivery_revision }),
            json!({ "text": clipped, "state": state, "boundaryUncertain": false }),
            now,
        ))
    }

    /// Session phase transitions (listening/thinking/speaking/paused) —
    /// emitted only on CHANGE, so the frame rate is bounded by real
    /// state changes, not the pump cadence.
    pub fn phase(&mut self, phase: &'static str, now: std::time::Instant) -> Option<String> {
        if self.last_phase == Some(phase) {
            return None;
        }
        self.last_phase = Some(phase);
        Some(self.line("session.phase", json!({}), json!({ "phase": phase }), now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn partial_throttles_revisions_and_seq() {
        let t0 = Instant::now();
        let mut lane = RealtimeLane::new("call_x".into(), 7, t0);
        let a = lane.user_partial("hello", t0).expect("first partial emits");
        assert!(a.contains("\"realtime.emit\""));
        assert!(a.contains("\"user.partial\""));
        assert!(a.contains("\"revision\":1"));
        assert!(a.contains("\"turnId\":\"t1\""));
        assert!(a.contains("\"callEpoch\":7"));
        // Inside the throttle window: dropped.
        assert!(lane
            .user_partial("hello wor", t0 + Duration::from_millis(100))
            .is_none());
        // Past it: revision 2, seq advanced.
        let b = lane
            .user_partial("hello world", t0 + Duration::from_millis(400))
            .unwrap();
        assert!(b.contains("\"revision\":2"));
        assert!(b.contains("\"seq\":2"));
        // Finalized turn: next partial is t2 revision 1.
        lane.turn_final();
        let c = lane
            .user_partial("next", t0 + Duration::from_millis(500))
            .unwrap();
        assert!(c.contains("\"turnId\":\"t2\""));
        assert!(c.contains("\"revision\":1"));
        // Empty text never emits.
        assert!(lane
            .user_partial("  ", t0 + Duration::from_secs(9))
            .is_none());
    }

    #[test]
    fn phase_emits_on_change_only() {
        let t0 = Instant::now();
        let mut lane = RealtimeLane::new("call_x".into(), 1, t0);
        assert!(lane.phase("listening", t0).is_some());
        assert!(lane.phase("listening", t0).is_none());
        let f = lane.phase("thinking", t0).unwrap();
        assert!(f.contains("\"session.phase\""));
        assert!(f.contains("\"thinking\""));
    }

    #[test]
    fn delivery_frames_carry_state_and_reset_per_turn() {
        let t0 = Instant::now();
        let mut lane = RealtimeLane::new("c".into(), 1, t0);
        let a = lane.delivery("Ahoy there!", "sent_to_sco", t0).unwrap();
        assert!(a.contains("\"assistant.delivery\""));
        assert!(a.contains("\"sent_to_sco\""));
        assert!(a.contains("\"revision\":1"));
        let b = lane
            .delivery("Second sentence.", "sent_to_sco", t0)
            .unwrap();
        assert!(b.contains("\"revision\":2"));
        lane.turn_final();
        let c = lane.delivery("Next reply.", "sent_to_sco", t0).unwrap();
        assert!(c.contains("\"turnId\":\"t2\""));
        assert!(c.contains("\"revision\":1"));
        assert!(lane.delivery("  ", "sent_to_sco", t0).is_none());
    }

    #[test]
    fn long_partials_are_clipped() {
        let t0 = Instant::now();
        let mut lane = RealtimeLane::new("c".into(), 1, t0);
        let long = "word ".repeat(2000);
        let f = lane.user_partial(&long, t0).unwrap();
        assert!(f.len() < 16 * 1024, "frame stays under the 16 KiB app cap");
    }
}
