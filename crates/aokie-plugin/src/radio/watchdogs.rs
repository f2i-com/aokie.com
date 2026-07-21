//! Silence timer, no-SCO audio watchdog and the agent hangup verdict.

#[allow(unused_imports)]
use super::*;

/// Spoken once when the caller has been silent for a whole window (agent mode).
#[cfg(feature = "voice")]
pub(super) const SILENCE_CHECK_LINE: &str = "Hello? Are you still there?";
/// Spoken before the max-silence hangup — plain ASCII (TTS + health-path safe).
#[cfg(feature = "voice")]
pub(super) const SILENCE_GOODBYE_LINE: &str = "I haven't heard anything for a while, so I'll hang up now. \
Please call back if you still need us. Goodbye.";

/// What the silence timer wants done when a window expires.
#[cfg(feature = "voice")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SilenceAction {
    /// First expiry: check in with the caller ("are you still there?").
    Prompt,
    /// A second window elapsed with still nothing: say goodbye and hang up.
    HangUp,
}

/// Call-level max-silence timer (VOICE-001 deferral → AOK-CTRL-001): a live
/// call where NEITHER side has produced audio activity for `window` gets a
/// check-in prompt, then — if the silence persists a second window — a polite
/// goodbye and a clean hangup, so a dead line never holds the phone forever.
/// Pure: both mutators take `now`, so tests drive it with fabricated instants.
#[cfg(feature = "voice")]
pub(super) struct SilenceTimer {
    pub(super) window: std::time::Duration,
    pub(super) last_activity: std::time::Instant,
    pub(super) prompted: bool,
}

#[cfg(feature = "voice")]
impl SilenceTimer {
    pub(super) fn new(window: std::time::Duration, now: std::time::Instant) -> Self {
        Self {
            window,
            last_activity: now,
            prompted: false,
        }
    }

    /// Any conversational activity (caller speech, a spoken reply) resets the
    /// window AND forgives an earlier check-in prompt.
    pub(super) fn note_activity(&mut self, now: std::time::Instant) {
        self.last_activity = now;
        self.prompted = false;
    }

    pub(super) fn check(&mut self, now: std::time::Instant) -> Option<SilenceAction> {
        if self.window.is_zero() || now.duration_since(self.last_activity) < self.window {
            return None;
        }
        if self.prompted {
            Some(SilenceAction::HangUp)
        } else {
            // The prompt itself restarts the window (its playback is also
            // stamped by the caller, belt-and-braces).
            self.prompted = true;
            self.last_activity = now;
            Some(SilenceAction::Prompt)
        }
    }
}

/// One continuous interval in which an answered call has no SCO channel.
/// Total call age is deliberately irrelevant: any recovered sample rate or
/// expected switchboard transition resets this outage from scratch.
#[derive(Debug)]
pub(super) struct NoScoOutage {
    pub(super) call_id: String,
    pub(super) since: std::time::Instant,
    pub(super) remote_return_requested: bool,
    pub(super) codec_nudge_sent: bool,
    pub(super) hangup_sent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoScoAction {
    RequestRemoteReturn,
    /// `AT+BCC` — ask the phone to (re)establish the audio channel before
    /// giving up on the call. Observed live 2026-07-21: an outbound callback
    /// answered with sample_rate=0 (a rapid teardown race ate the SCO) and
    /// the phone never re-offered audio on its own.
    NudgeCodecConnection,
    HangUp,
}

#[derive(Debug, Default)]
pub(super) struct NoScoWatchdog {
    pub(super) outage: Option<NoScoOutage>,
}

impl NoScoWatchdog {
    pub(super) const GRACE: std::time::Duration = std::time::Duration::from_secs(8);
    /// How long an active Aokie-owned call may sit without audio before the
    /// AT+BCC self-heal fires (once per outage) — well before the hangup.
    pub(super) const NUDGE_GRACE: std::time::Duration = std::time::Duration::from_millis(2_500);

    pub(super) fn check(
        &mut self,
        call_id: Option<&str>,
        sco_available: bool,
        switch_suppressed: bool,
        aokie_owner_current: bool,
        remote_reserved: bool,
        now: std::time::Instant,
    ) -> Option<NoScoAction> {
        let Some(call_id) = call_id else {
            self.outage = None;
            return None;
        };
        if sco_available || switch_suppressed {
            self.outage = None;
            return None;
        }

        if self
            .outage
            .as_ref()
            .is_none_or(|outage| outage.call_id != call_id)
        {
            self.outage = Some(NoScoOutage {
                call_id: call_id.to_string(),
                since: now,
                remote_return_requested: false,
                codec_nudge_sent: false,
                hangup_sent: false,
            });
        }
        let outage = self.outage.as_mut().expect("installed above");

        if remote_reserved {
            if !outage.remote_return_requested {
                outage.remote_return_requested = true;
                return Some(NoScoAction::RequestRemoteReturn);
            }
            // A Companion-owned or returning route can never be ended by this
            // watchdog. The media state machine must return the caller first.
            return None;
        }

        if !aokie_owner_current {
            return None;
        }
        if outage.remote_return_requested {
            // Return to Aokie succeeded. Give SCO one complete fresh grace
            // period to recover before considering a hardware hangup.
            outage.remote_return_requested = false;
            outage.codec_nudge_sent = false;
            outage.hangup_sent = false;
            outage.since = now;
            return None;
        }
        if !outage.codec_nudge_sent
            && now.saturating_duration_since(outage.since) >= Self::NUDGE_GRACE
        {
            outage.codec_nudge_sent = true;
            return Some(NoScoAction::NudgeCodecConnection);
        }
        if !outage.hangup_sent && now.saturating_duration_since(outage.since) >= Self::GRACE {
            outage.hangup_sent = true;
            return Some(NoScoAction::HangUp);
        }
        None
    }

    pub(super) fn rearm_after_owner_race(&mut self, now: std::time::Instant) {
        if let Some(outage) = self.outage.as_mut() {
            outage.hangup_sent = false;
            outage.codec_nudge_sent = false;
            outage.remote_return_requested = false;
            // The old owner fence can fail because a complete Companion
            // claim-and-return crossed the physical action. Treat that as a
            // fresh Aokie-owned outage, exactly like a return observed by
            // `check`, rather than carrying the stale eight-second clock into
            // the replacement owner.
            outage.since = now;
        }
    }
}

/// Read the configured maximum conversational-silence window.
#[cfg(feature = "voice")]
pub(super) fn max_silence_window() -> std::time::Duration {
    let secs = std::env::var("AOKIE_MAX_SILENCE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|v| if v == 0 { 0 } else { v.clamp(10, 600) })
        .unwrap_or(30);
    std::time::Duration::from_secs(secs)
}

/// How long to let the SCO queue drain audio that was QUEUED (synthesis often
/// outruns realtime playout) before an intentional hangup cuts the channel:
/// the remaining playout computed from what was actually queued, plus a small
/// margin, bounded — never the old blind 900 ms guess, never unbounded.
#[cfg(feature = "voice")]
pub(super) fn playout_drain_wait(
    t0: std::time::Instant,
    queued: std::time::Duration,
    now: std::time::Instant,
) -> std::time::Duration {
    // The cap must clear a full spoken farewell/apology (~6s) — it only
    // guards against a pathological queue, never a normal goodbye.
    const MARGIN: std::time::Duration = std::time::Duration::from_millis(400);
    const CAP: std::time::Duration = std::time::Duration::from_secs(8);
    (t0 + queued + MARGIN)
        .saturating_duration_since(now)
        .min(CAP)
}

/// The agent-hangup POLICY (AOK-CTRL-001): the LLM's end-call marker is only a
/// REQUEST — this validates it against what actually happened on the call
/// before the plugin may hang up. `Proceed.wait` is the computed farewell
/// playout drain.
#[cfg(feature = "voice")]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum HangupVerdict {
    Proceed { wait: std::time::Duration },
    Skip(&'static str),
}

#[cfg(feature = "voice")]
#[allow(clippy::too_many_arguments)]
pub(super) fn agent_hangup_verdict(
    requested: bool,
    aokie_owner_current: bool,
    barged: bool,
    operator_ended: bool,
    ended_by_failsafe: bool,
    farewell_audible: bool,
    farewell_asks_question: bool,
    t0: std::time::Instant,
    reply_dur: std::time::Duration,
    now: std::time::Instant,
) -> HangupVerdict {
    if !requested {
        return HangupVerdict::Skip("no hangup requested");
    }
    if !aokie_owner_current {
        return HangupVerdict::Skip("the exact Aokie caller-owner fence changed");
    }
    if barged {
        return HangupVerdict::Skip("the caller barged in — they may have more to say");
    }
    if operator_ended {
        return HangupVerdict::Skip("an operator action already owns the call");
    }
    if ended_by_failsafe {
        return HangupVerdict::Skip("the dead-air fail-safe already ended the call");
    }
    if !farewell_audible {
        // Nothing played: the dead-air fail-safe fires for this reply attempt
        // (apology + hangup) — proceeding here would race it.
        return HangupVerdict::Skip("the farewell never played — the fail-safe owns the ending");
    }
    if farewell_asks_question {
        // Live report 2026-07-13: the model appended [[END_CALL]] to
        // "Is there anything else I can help you with?" and the call hung up
        // on its own question. A farewell that ASKS the caller anything is
        // not a farewell — stay on the line for the answer; the next clean
        // goodbye (no question) carries the hangup.
        return HangupVerdict::Skip(
            "the farewell asks the caller a question — waiting for their answer",
        );
    }
    HangupVerdict::Proceed {
        wait: playout_drain_wait(t0, reply_dur, now),
    }
}
