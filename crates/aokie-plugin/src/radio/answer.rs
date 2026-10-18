//! Auto-answer holds: caller-id/personalization waits and greeting settle.

#[allow(unused_imports)]
use super::*;

/// Ring-time personalization window (user idea 2026-07-14): +CLIP rides the
/// RING, so waiting a beat before auto-answering lets the caller id land and
/// the personalize flow deliver its call-scoped overlay BEFORE the call is
/// even picked up — "Hi <name>!" from the very first word, and ~one ring of
/// pickup latency reads as natural telephony, not lag. No caller id yet:
/// wait only ANSWER_ID_WAIT (a withheld number never gets one). Id known:
/// wait up to ANSWER_OVERLAY_WAIT for the flow. Overlay ready: answer NOW.
#[cfg(feature = "voice")]
pub(super) const ANSWER_ID_WAIT: std::time::Duration = std::time::Duration::from_millis(1200);
#[cfg(feature = "voice")]
pub(super) const ANSWER_OVERLAY_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);

#[cfg(feature = "voice")]
pub(super) fn hold_auto_answer(
    caller_id_known: bool,
    overlay_ready: bool,
    elapsed: std::time::Duration,
) -> bool {
    if overlay_ready {
        return false;
    }
    if !caller_id_known {
        return elapsed < ANSWER_ID_WAIT;
    }
    elapsed < ANSWER_OVERLAY_WAIT
}

/// Who takes a call to us that still rings, as far as OAIY's session for it
/// goes ([`ring_responder`]).
#[cfg(feature = "voice")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RingResponder {
    /// Not known yet: its number may still come, and the answer waits for it.
    Undecided,
    /// The owner: left ringing for them (the allowed list, the expired
    /// consent grant), or auto-answer is off and nobody has answered it.
    Owner,
    /// The screening's own way: a screened caller, a manager's line that
    /// needs Aokie's own voice. No OAIY session.
    Screened,
    /// The receptionist answers it: OAIY's session is opened now, so OAIY
    /// makes the greeting while the phone still rings.
    Receptionist,
}

/// Who takes a call to us that still rings, from what the radio loop answers
/// it by: `action` (`ring_action_for`), auto-answer, the consent grant, and
/// whether the owner handed it to the receptionist (Answer in Aokie, which
/// wins over the allowed list and an expiry that came after). `screened` is
/// a screening verdict or a manager's line kept to Aokie's own voice. Until
/// the number is known (or the phone said it is hidden) only an answer that
/// stopped waiting for one (`id_wait_over`, ANSWER_ID_WAIT) decides it, as
/// before: a number that comes late may still be screened. Pure for tests.
#[cfg(feature = "voice")]
pub(super) fn ring_responder(
    action: crate::screen::RingAction,
    auto_answer: bool,
    consent_expired: bool,
    handed_over: bool,
    screened: bool,
    identity_known: bool,
    id_wait_over: bool,
) -> RingResponder {
    use crate::screen::RingAction;
    if !handed_over
        && (matches!(action, RingAction::Wait | RingAction::Leave) || consent_expired)
    {
        return RingResponder::Owner;
    }
    if !identity_known && !id_wait_over {
        return RingResponder::Undecided;
    }
    if screened {
        return RingResponder::Screened;
    }
    if handed_over || (auto_answer && action == RingAction::Answer) {
        RingResponder::Receptionist
    } else {
        RingResponder::Owner
    }
}

/// What the screening does with a call to us while it rings (review 3 #21).
/// With auto-answer on, a screened caller with a message to hear is answered
/// and told it at the greeting, as before; with it off nobody answers to say
/// it, so once the number is known (or the phone said it is hidden) that
/// caller is refused while it rings. A number that is only late is not.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(super) fn ring_action_for(
    action: crate::screen::RingAction,
    auto_answer: bool,
    identity_known: bool,
    verdict: Option<&'static str>,
) -> crate::screen::RingAction {
    match (action, verdict) {
        (crate::screen::RingAction::Answer, Some(reason)) if !auto_answer && identity_known => {
            crate::screen::RingAction::Refuse(reason)
        }
        (action, _) => action,
    }
}

/// Whether a reject (AT+CHUP) may go to the call that rings: only while the
/// phone shows no live call and no other caller waits or is on hold, where
/// it would end that call instead (review 3 #21, review 1 #1).
pub(super) fn ring_refusal_safe(call_live: bool, caller_waiting: bool, caller_held: bool) -> bool {
    !call_live && !caller_waiting && !caller_held
}

/// Connect the in-plugin agent's LLM client (llama.cpp :8080 / ollama :11434
/// / the configured aiEndpoint) and keep the health slot truthful. Shared by
/// the lazy first-reply path and the ring-time pre-warm.
#[cfg(all(aokie_radio, feature = "voice"))]
pub(super) fn connect_agent_client(
    agent_endpoint: &Arc<Mutex<Option<String>>>,
    agent_model: Option<String>,
    status: &Arc<RadioStatus>,
) -> Option<crate::agent::LlmClient> {
    let configured = agent_endpoint.lock().unwrap().clone();
    match crate::agent::discover_endpoint(configured.as_deref()) {
        Some(ep) => {
            let c = crate::agent::LlmClient::new(ep, agent_model);
            eprintln!(
                "[aokie-plugin] voice agent LLM: {} (model {:?})",
                c.endpoint(),
                c.model()
            );
            *status.llm_error.lock().unwrap() = None;
            Some(c)
        }
        None => {
            eprintln!("[aokie-plugin] voice agent: no local LLM reachable (:8080/:11434)");
            *status.llm_error.lock().unwrap() = Some(
                "no reachable LLM at reply time (tried llama.cpp :8080 and ollama :11434)"
                    .to_string(),
            );
            None
        }
    }
}

/// §9.3 greeting-personalization race: how long the greeting may WAIT for
/// the caller-id flow's call-scoped overlay before speaking the configured
/// default. The overlay typically lands 1–3 s after answer (caller-id event →
/// sync flow → call.configureAgent); greeting synthesis becomes ready in a
/// similar window, so the real added delay is usually well under the cap.
/// Bounded hard: a slow or absent flow costs at most this much extra silence.
#[cfg(feature = "voice")]
pub(super) const GREETING_PERSONALIZE_HOLD: std::time::Duration = std::time::Duration::from_millis(1500);

/// Post-answer SETTLE before the greeting's first AUDIO frame reaches the
/// SCO. The channel is often already up at answer (this Pixel opens it during
/// RINGING for the in-band ringtone), but the carrier's answer transition
/// (ringback -> voice path, roughly 0.5-1s on VoLTE) is still routing — a
/// greeting audible the instant of ATA loses its first words and the caller
/// hears it mid-sentence (live call 8576ba9e 2026-07-18). Applied as an
/// audio EGRESS gate inside the speak path, never as a pre-speak hold: the
/// greeting occupies the floor from the first loop pass after answer (caller
/// speech in the window rides overlap capture), synthesis runs during the
/// gate, and the gate is anchored at the greet clock's start — perceived
/// delay is max(settle, synth/overlay time), never the sum. ⚠️ A pre-speak
/// hold variant shipped briefly and let the caller's pickup word become a
/// replied-to first turn that CANCELLED the personalized greeting (live call
/// e77457c6 2026-07-18) — do not reintroduce it.
#[cfg(feature = "voice")]
pub(super) const GREETING_ANSWER_SETTLE_DEFAULT_MS: u64 = 700;

/// Env override AOKIE_GREETING_SETTLE_MS: 0 disables, capped at 3000 (a
/// misconfigured huge value must never add seconds of post-answer dead air).
/// Unset / unparsable = the default. Pure for tests.
#[cfg(feature = "voice")]
pub(super) fn parse_greeting_settle_ms(v: Option<&str>) -> u64 {
    match v.and_then(|s| s.trim().parse::<u64>().ok()) {
        Some(n) => n.min(3000),
        None => GREETING_ANSWER_SETTLE_DEFAULT_MS,
    }
}

#[cfg(feature = "voice")]
pub(super) fn greeting_answer_settle() -> std::time::Duration {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    std::time::Duration::from_millis(*MS.get_or_init(|| {
        parse_greeting_settle_ms(std::env::var("AOKIE_GREETING_SETTLE_MS").ok().as_deref())
    }))
}

/// §9.3: should the greeting WAIT for the personalization overlay? Only when
/// the caller id is KNOWN (the flow that pushes the overlay triggers on the
/// caller-id event — no id, no push coming) and the overlay hasn't arrived,
/// and never past the bounded hold. Pure for tests.
#[cfg(feature = "voice")]
pub(super) fn hold_greeting_for_overlay(
    overlay_matches_call: bool,
    caller_id_known: bool,
    hold_elapsed: std::time::Duration,
    cap: std::time::Duration,
) -> bool {
    !overlay_matches_call && caller_id_known && hold_elapsed < cap
}
