//! The explicit call-session state machine (audit AK-001).
//!
//! ONE place owns a call's identity and lifecycle: an immutable `call_…` id, a
//! monotonically increasing **generation** (never reused, so async voice work
//! can be stamped and stale results discarded — audit AK-002/C-05), the phase,
//! millisecond-accurate timing, caller-id updates, the operator's termination
//! intent, and the emitted/greeted/answered/toned once-per-call flags that were
//! previously six scattered `Option<String>` variables in the radio loop.
//!
//! Deliberately dependency-free and target-independent so the whole lifecycle
//! is unit-testable off-hardware: the radio loop feeds it Bluetooth events and
//! control intents; it answers questions ("is this result stale?", "what
//! outcome does this call end with?") — it does not touch the radio itself.

use std::time::Instant;

/// Which side/action asked for the call to end — recorded when the operator
/// control is DISPATCHED so the eventual `CallTerminated` can tell a rejected
/// call from a missed one (audit AK-01: rejected ≠ missed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationIntent {
    OperatorReject,
    OperatorHangup,
    /// The in-plugin voice agent decided the call was fully handled and hung up
    /// itself (user feature): after a completed conversation it says goodbye and
    /// terminates so the caller does not have to. Outcome is a normal completion.
    AgentHangup,
    /// Phase 1 abuse handling (call-policy spec): the agent flagged the caller
    /// as abusive ([[ABUSE]]); deterministic code spoke the notice and ended
    /// the call. Its own outcome — `terminated_abuse` — so the Calls row is an
    /// honest audit trail, never a look-alike "completed".
    AgentTerminateAbuse,
    /// The dongle/phone link vanished under a live call (audit AOK-LIF-003):
    /// the radio synthesizes termination rather than leaving the session —
    /// and the operator UI — stuck "live" on hardware that is gone.
    DeviceLost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Ringing,
    Active,
}

/// A live (ringing or active) call.
#[derive(Debug)]
pub struct CallSession {
    /// Immutable for the session's whole life; every event/turn/utterance of
    /// this call carries it. `call_<uuid>` — never reused.
    pub id: String,
    /// Monotonic across the plugin's life, starting at 1. Async voice work is
    /// stamped with this; a result whose generation is not the CURRENT one is
    /// from a previous call and must be dropped, never spoken or recorded.
    pub generation: u64,
    pub phase: Phase,
    /// ISO-8601 instant the call started ringing (the UI timer runs from it).
    pub started_at_iso: String,
    started: Instant,
    answered: Option<Instant>,
    pub caller_id: Option<String>,
    intent: Option<TerminationIntent>,
    /// `aokie.call.incoming` is delayed briefly for caller-id collection;
    /// this holds "not yet emitted" plus when the wait started.
    pending_incoming_since: Option<Instant>,
    /// Once-per-call action flags (greeting spoken, auto-answer sent,
    /// diagnostic answer tone played).
    pub greeted: bool,
    pub auto_answered: bool,
    pub toned: bool,
    /// Per-call utterance counter (STT job ids — observability + ordering).
    next_utterance: u32,
}

impl CallSession {
    pub fn is_active(&self) -> bool {
        self.phase == Phase::Active
    }

    /// Milliseconds since the call started ringing.
    pub fn ringing_for_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// True while `aokie.call.incoming` has not been emitted yet.
    pub fn incoming_pending(&self) -> bool {
        self.pending_incoming_since.is_some()
    }

    /// Milliseconds we have been holding the incoming event for caller id.
    pub fn incoming_pending_ms(&self) -> u128 {
        self.pending_incoming_since
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0)
    }

    pub fn mark_incoming_emitted(&mut self) {
        self.pending_incoming_since = None;
    }

    /// Allocate the next utterance id for this call (1-based).
    pub fn next_utterance_id(&mut self) -> u32 {
        self.next_utterance += 1;
        self.next_utterance
    }
}

/// Summary of a finished call — everything `aokie.call.ended` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndedCall {
    pub id: String,
    pub generation: u64,
    pub caller_id: Option<String>,
    /// Time from ANSWER to termination, milliseconds. 0 when never answered.
    pub duration_ms: u128,
    /// Legacy whole-seconds duration (the pack's after-call bindings gate on
    /// `durationSeconds > 5`) — still truncated, but no longer the outcome
    /// signal, so a 100 ms answered call reads durationSeconds 0 AND
    /// outcome "completed".
    pub duration_seconds: u64,
    /// "completed" | "rejected" | "missed" — from the answered flag and the
    /// recorded termination intent, NOT from duration truncation.
    pub outcome: &'static str,
    /// "operator_reject" | "operator_hangup" | "remote_or_operator" (the
    /// legacy value when no operator intent was recorded).
    pub reason: &'static str,
}

/// Owns the current session (at most one call at a time — HFP) and the
/// never-reused generation counter.
#[derive(Debug, Default)]
pub struct SessionTracker {
    session: Option<CallSession>,
    last_generation: u64,
}

impl SessionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self) -> Option<&CallSession> {
        self.session.as_ref()
    }

    pub fn current_mut(&mut self) -> Option<&mut CallSession> {
        self.session.as_mut()
    }

    /// The CURRENT call's generation, or 0 when idle. Async voice results
    /// stamped with any other value are stale.
    pub fn generation(&self) -> u64 {
        self.session.as_ref().map(|s| s.generation).unwrap_or(0)
    }

    pub fn call_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.id.as_str())
    }

    /// A ring indicator arrived. Starts a NEW session only when idle —
    /// phones re-emit ring/callsetup for one call, so a duplicate ring on a
    /// live session is a no-op (the double-greeting fix, kept). Returns the
    /// new call's id when one actually started.
    pub fn ring(&mut self, id: String, started_at_iso: String) -> Option<&CallSession> {
        if self.session.is_some() {
            return None;
        }
        self.last_generation += 1;
        self.session = Some(CallSession {
            id,
            generation: self.last_generation,
            phase: Phase::Ringing,
            started_at_iso,
            started: Instant::now(),
            answered: None,
            caller_id: None,
            intent: None,
            pending_incoming_since: Some(Instant::now()),
            greeted: false,
            auto_answered: false,
            toned: false,
            next_utterance: 0,
        });
        self.session.as_ref()
    }

    /// CLIP caller id arrived (any time before or after answer).
    pub fn caller_id(&mut self, number: String) {
        if let Some(s) = self.session.as_mut() {
            s.caller_id = Some(number);
        }
    }

    /// The call went active. Idempotent; ignored when idle (a stray
    /// answered indicator with no call must not fabricate a session).
    pub fn answered(&mut self) {
        if let Some(s) = self.session.as_mut() {
            s.phase = Phase::Active;
            if s.answered.is_none() {
                s.answered = Some(Instant::now());
            }
        }
    }

    /// Record that the operator asked to reject/hang up — called when the
    /// control is dispatched, so the eventual termination knows WHY.
    pub fn note_intent(&mut self, intent: TerminationIntent) {
        if let Some(s) = self.session.as_mut() {
            // First intent wins: a reject followed by a redundant hangup is
            // still a rejection.
            if s.intent.is_none() {
                s.intent = Some(intent);
            }
        }
    }

    /// The call terminated. Consumes the session and computes the outcome:
    /// answered → completed (regardless of duration); never answered →
    /// rejected when the operator rejected it, else missed.
    pub fn terminate(&mut self) -> Option<EndedCall> {
        let s = self.session.take()?;
        let duration_ms = s.answered.map(|t| t.elapsed().as_millis()).unwrap_or(0);
        let (outcome, reason) = match (s.answered.is_some(), s.intent) {
            // Device loss is its own truth (audit AOK-LIF-003): the call did
            // not complete or get rejected — the hardware went away.
            (true, Some(TerminationIntent::DeviceLost)) => ("completed", "device_lost"),
            (false, Some(TerminationIntent::DeviceLost)) => ("missed", "device_lost"),
            (true, Some(TerminationIntent::OperatorHangup)) => ("completed", "operator_hangup"),
            // The agent hung up after handling the call: a normal completion.
            (true, Some(TerminationIntent::AgentHangup)) => ("completed", "agent_hangup"),
            // Abuse termination is its own truth (Phase 1): the notice was
            // spoken and the call ended by policy — not a completion.
            (true, Some(TerminationIntent::AgentTerminateAbuse)) => {
                ("terminated_abuse", "agent_abuse")
            }
            (true, _) => ("completed", "remote_or_operator"),
            (false, Some(TerminationIntent::OperatorReject)) => ("rejected", "operator_reject"),
            (false, Some(TerminationIntent::OperatorHangup)) => ("rejected", "operator_hangup"),
            // Defensive: the agent only hangs up after answering, so this cannot
            // normally occur — classify as missed rather than leave it unmatched.
            (false, Some(TerminationIntent::AgentHangup)) => ("missed", "agent_hangup"),
            (false, Some(TerminationIntent::AgentTerminateAbuse)) => {
                ("rejected", "agent_abuse")
            }
            (false, None) => ("missed", "remote_or_operator"),
        };
        Some(EndedCall {
            id: s.id,
            generation: s.generation,
            caller_id: s.caller_id,
            duration_ms,
            duration_seconds: (duration_ms / 1000) as u64,
            outcome,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(t: &mut SessionTracker, id: &str) {
        t.ring(id.to_string(), "2026-07-10T00:00:00Z".to_string());
    }

    #[test]
    fn incoming_always_precedes_and_duplicates_are_idempotent() {
        let mut t = SessionTracker::new();
        assert_eq!(t.generation(), 0);
        ring(&mut t, "call_a");
        assert_eq!(t.call_id(), Some("call_a"));
        assert_eq!(t.generation(), 1);
        assert!(t.current().unwrap().incoming_pending());

        // Phones re-emit the ring indicator: the session must not restart.
        assert!(t.ring("call_b".into(), "x".into()).is_none());
        assert_eq!(t.call_id(), Some("call_a"));
        assert_eq!(t.generation(), 1);

        // Duplicate answered indicators keep the FIRST answer instant.
        t.answered();
        let first = t.current().unwrap().answered;
        t.answered();
        assert_eq!(t.current().unwrap().answered, first);
    }

    #[test]
    fn rejected_is_not_missed() {
        let mut t = SessionTracker::new();
        ring(&mut t, "call_a");
        t.caller_id("+61400000001".into());
        t.note_intent(TerminationIntent::OperatorReject);
        let ended = t.terminate().unwrap();
        assert_eq!(ended.outcome, "rejected");
        assert_eq!(ended.reason, "operator_reject");
        assert_eq!(ended.duration_ms, 0);
        assert_eq!(ended.caller_id.as_deref(), Some("+61400000001"));
    }

    #[test]
    fn a_sub_second_answered_call_is_completed_not_missed() {
        let mut t = SessionTracker::new();
        ring(&mut t, "call_a");
        t.answered();
        // Terminated ~immediately: duration truncates to 0 whole seconds but
        // the call WAS answered, so it is completed (the audit's 100 ms case).
        let ended = t.terminate().unwrap();
        assert_eq!(ended.outcome, "completed");
        assert_eq!(ended.duration_seconds, 0);
    }

    #[test]
    fn unanswered_remote_abandon_is_missed() {
        let mut t = SessionTracker::new();
        ring(&mut t, "call_a");
        let ended = t.terminate().unwrap();
        assert_eq!(ended.outcome, "missed");
        assert_eq!(ended.reason, "remote_or_operator");
    }

    #[test]
    fn hangup_on_an_active_call_is_completed_with_operator_reason() {
        let mut t = SessionTracker::new();
        ring(&mut t, "call_a");
        t.answered();
        t.note_intent(TerminationIntent::OperatorHangup);
        let ended = t.terminate().unwrap();
        assert_eq!(ended.outcome, "completed");
        assert_eq!(ended.reason, "operator_hangup");
    }

    /// Phase 1: an abuse termination is its own outcome — the Calls row must
    /// read `terminated_abuse`, never a look-alike "completed".
    #[test]
    fn abuse_termination_has_its_own_outcome() {
        let mut t = SessionTracker::new();
        ring(&mut t, "call_a");
        t.answered();
        t.note_intent(TerminationIntent::AgentTerminateAbuse);
        let ended = t.terminate().unwrap();
        assert_eq!(ended.outcome, "terminated_abuse");
        assert_eq!(ended.reason, "agent_abuse");
        // First intent still wins: a later redundant hangup can't relabel it.
        ring(&mut t, "call_b");
        t.answered();
        t.note_intent(TerminationIntent::AgentTerminateAbuse);
        t.note_intent(TerminationIntent::AgentHangup);
        assert_eq!(t.terminate().unwrap().outcome, "terminated_abuse");
    }

    #[test]
    fn generations_are_never_reused_and_rapid_calls_stay_isolated() {
        let mut t = SessionTracker::new();
        ring(&mut t, "call_a");
        assert_eq!(t.generation(), 1);
        t.terminate().unwrap();
        assert_eq!(t.generation(), 0, "idle between calls");

        ring(&mut t, "call_b");
        assert_eq!(t.generation(), 2, "generation increments, never reused");
        // A result stamped with call_a's generation is stale for call_b.
        assert_ne!(t.generation(), 1);
        // Per-call state reset with the new session.
        let s = t.current().unwrap();
        assert!(!s.greeted && !s.auto_answered && !s.toned);
        assert!(s.caller_id.is_none());
    }

    #[test]
    fn utterance_ids_are_per_call_and_start_at_one() {
        let mut t = SessionTracker::new();
        ring(&mut t, "call_a");
        assert_eq!(t.current_mut().unwrap().next_utterance_id(), 1);
        assert_eq!(t.current_mut().unwrap().next_utterance_id(), 2);
        t.terminate();
        ring(&mut t, "call_b");
        assert_eq!(t.current_mut().unwrap().next_utterance_id(), 1);
    }

    #[test]
    fn terminate_when_idle_is_a_no_op() {
        let mut t = SessionTracker::new();
        assert!(t.terminate().is_none());
        // Stray indicators with no call never fabricate a session.
        t.answered();
        t.caller_id("+61".into());
        t.note_intent(TerminationIntent::OperatorHangup);
        assert!(t.current().is_none());
    }
}
