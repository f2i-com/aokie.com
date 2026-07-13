//! Live Bluetooth radio integration.
//!
//! Runs the real `aokie_radio` `AokieRuntime` (via
//! [`aokie_dongle::bluetooth::BluetoothManager`]) on a dedicated background
//! thread, maps its `BluetoothEvent`s onto the `aokie.*` Desktop-event
//! contract (emitted straight to stdout **and** the durable outbox), and
//! accepts control requests â€” answer / reject / hangup / send-SMS / speak â€”
//! from the main RPC thread over an mpsc channel.
//!
//! ## Why a second `Outbox` + `StdoutSink` on this thread
//! The plugin's main loop blocks on stdin, so an asynchronous call event (a
//! call can ring at any instant) must be delivered without waiting for the
//! next RPC. `StdoutSink` writes one whole line under the stdout lock
//! (line-atomic), so this thread's sink and the main thread's sink never
//! interleave. The [`Outbox`] here is a *second* SQLite connection to the
//! same `outbox.sqlite` file â€” `idempotency_key` is UNIQUE and SQLite
//! serialises writers, so essential call/SMS records survive a Desktop
//! restart exactly as they do on the command path.
//!
//! The whole radio surface is Windows-only (WinUSB); on other targets
//! [`spawn`] returns an error and the plugin simply never has a radio.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
#[cfg(feature = "voice")]
use std::time::{Duration, Instant};

use aokie_core::events::DesktopEvent;
use serde_json::json;

use crate::config::PairedDevice;
use crate::event_bridge::{emit_event, Sink};
use crate::outbox::Outbox;

/// A control request from the main RPC thread to the radio thread. Every
/// variant is fire-and-forget: the *result* of the action arrives back as an
/// asynchronous `aokie.*` event from the radio thread, matching the mock
/// contract (`call.answer` â†’ later `aokie.call.answered`, etc.).
/// §9.3 call-scoped agent configuration (`call.configureAgent`): a caller-
/// specific persona/greeting bound to ONE call id. Held in `run_loop` and
/// wiped in the per-call reset block, so a failed or raced next-call setup
/// can never leak the previous caller's personalization into a different
/// caller's conversation (the durable `settings.set` path remains for
/// caller-INDEPENDENT config).
#[cfg(feature = "voice")]
struct CallAgentOverlay {
    call_id: String,
    persona: Option<String>,
    greeting: Option<String>,
}

pub enum RadioControl {
    /// AOK-CTRL-001: call controls carry the operation id minted by the
    /// connector when it ACCEPTED the command, so the radio can attribute an
    /// asynchronous failure (`aokie.hardware.error` code `control_failed`) to
    /// the exact request. `None` = internally-generated (no caller waiting).
    Answer {
        op: Option<String>,
    },
    Reject {
        op: Option<String>,
    },
    Hangup {
        op: Option<String>,
    },
    SendSms {
        to: String,
        body: String,
    },
    /// Speak text to the caller. The connector result is `accepted/queued`;
    /// the bot `call.turn.final` event is the authoritative confirmation the
    /// text actually played (a silent synthesis emits `speak_failed` instead).
    Speak {
        text: String,
        op: Option<String>,
    },
    /// Live-reconfigure the in-plugin voice agent without a reconnect. Each
    /// field is `Some` only when it changed; `None` leaves the current value
    /// alone. A flow (or `settings.set`) pushes this so the receptionist's
    /// persona/greeting/voice/model can be edited from FormLogic and take
    /// effect on the very next caller turn (or the next call's greeting).
    Configure {
        persona: Option<String>,
        greeting: Option<String>,
        voice: Option<String>,
        model: Option<String>,
        endpoint: Option<String>,
        stt_endpoint: Option<String>,
        tts_endpoint: Option<String>,
    },
    /// §9.3 call-scoped agent config (`call.configureAgent`): persona /
    /// greeting for ONE named call, wiped at the call boundary. The
    /// connector validated the call id, but the radio re-checks against the
    /// CURRENT session before applying — the command may have raced the
    /// call's end, and applying it to the next call is the exact failure
    /// this command exists to prevent.
    ConfigureCallAgent {
        call_id: String,
        persona: Option<String>,
        greeting: Option<String>,
    },
    /// AOK-BT-001: open a bounded, discoverable pairing window for `seconds`. At
    /// rest the radio is connectable-only, so an unknown phone can only pair while
    /// the window is open. A successful bond (or `StopPairing`, or timeout) closes it.
    StartPairing {
        seconds: u64,
    },
    /// AOK-BT-001: close the pairing window now (operator cancel / done).
    StopPairing,
    /// AOK-BT-001: forget a bonded device; replies whether a link key was removed.
    RemovePaired {
        address: String,
        reply: std::sync::mpsc::Sender<Result<bool, String>>,
    },
    /// Disconnect the connected phone but KEEP the bond (remote reconnect/
    /// unstick); replies whether a live link was actually dropped.
    Disconnect {
        address: String,
        reply: std::sync::mpsc::Sender<Result<bool, String>>,
    },
    /// HARD-001: reconnect a bonded phone from OUR side — the radio pages it
    /// and drives the HFP setup itself. Replies whether the attempt started
    /// (true) or the phone was already connected (false); errors for
    /// not-bonded / busy / HCI failures.
    Connect {
        address: String,
        reply: std::sync::mpsc::Sender<Result<bool, String>>,
    },
    /// PAIR-001: resolve the held SSP numeric comparison for `address` —
    /// `accept` completes the bond, `false` refuses it.
    ConfirmPairing {
        address: String,
        accept: bool,
        reply: std::sync::mpsc::Sender<Result<(), String>>,
    },
    /// AOK-BT-001: list bonded (revocable) devices with names; answered over
    /// `reply` as (address, friendly-name).
    ListBonded {
        reply: std::sync::mpsc::Sender<Vec<(String, Option<String>)>>,
    },
    /// The connected phone's captured friendly name/model, if known yet.
    ConnectedName {
        reply: std::sync::mpsc::Sender<Option<String>>,
    },
    Shutdown,
}

/// Default receptionist system prompt when none is configured (voice build) —
/// a goal-directed SCRIPT, not just a style: greet, get the caller's name and
/// reason, capture the key details, and book them in or take a message, one
/// short spoken question at a time. THE persona lives in the always-compiled
/// contract module (audit CROSS-SCHEMA-001) and is test-locked to the shared
/// cross-repo fixture; editable live via the `persona` setting / a flow push.
#[cfg(all(target_os = "windows", feature = "voice"))]
use crate::contract::DEFAULT_AGENT_PERSONA;

/// Spoken on answer when no greeting is configured. A BLANK greeting setting means
/// "use the default", never "answer silently" — a desktop settings-form save (which
/// writes the full settings bag, greeting included) or a flow push with an empty form
/// field must not silence the receptionist. Shared by the spawn path (connector.rs)
/// and the live `RadioControl::Configure` path below.
pub const DEFAULT_GREETING: &str = "Hello, thanks for calling. How can I help you today?";

/// Appended to the system prompt when `agentHangup` is on. The agent asks the LLM
/// to emit an [[END_CALL]] marker at the very end of its farewell so the plugin
/// knows the conversation is complete and can hang up after the goodbye plays.
#[cfg(feature = "voice")]
const END_CALL_INSTRUCTION: &str = "\n\nEnding the call: ordinary replies just answer - do NOT tack \"anything else?\" onto them (live transcripts show it seven times in one call; it reads as rushing the caller off the line). Only when the conversation itself sounds finished - the caller's requests are handled and they have nothing more - ask ONCE whether they need anything else, with NO marker. After the caller confirms they are done (\"no, that's all\", \"that's everything, thanks\", \"no thank you\") or says goodbye themselves, reply with a brief, warm goodbye and append the exact marker [[END_CALL]] at the very end of that goodbye. Never ask the anything-else question twice in a row. The goodbye carrying the marker must never contain a question - the system refuses to hang up while a question is waiting for an answer. Never write the marker mid-conversation.";

/// Appended to the system prompt in agent mode: how the model participates in
/// spoken delivery (validated pacing markers, the [[WAIT]] intentional-silence
/// marker, and continuation over backchannels). All markers are UNTRUSTED
/// input — the speech planner clamps rates, caps protection and strips every
/// bracketed token before anything is spoken or recorded. Plain ASCII (the
/// text is model-facing but lives next to caller-spoken constants).
#[cfg(feature = "voice")]
const SPEECH_STYLE_INSTRUCTION: &str = "\n\nSpoken delivery: your words are read aloud to the caller by a voice synthesizer.\n- This is a LIVE phone conversation: keep every reply to ONE or TWO short sentences, then let the caller speak. Long replies get talked over and feel rude. Ask at most one question per reply. Only go longer when reading back details the caller asked for.\n- When reading back dates, times or booking details from your notes, copy them EXACTLY as written - never approximate, merge or reorder them. If a detail is not in your notes, say you will have the team confirm it rather than guessing.\n- Phone numbers and codes are automatically read slowly, digit by digit; you do not need to do anything special for them.\n- You may wrap a short critical detail in [[slow]]...[[/slow]] to have it spoken more slowly.\n- Rarely, you may wrap ONE short vital sentence in [[important]]...[[/important]] so a brief overlap does not cut it off. The caller can always stop you by saying stop or wait.\n- If the caller asks you to wait, says they are thinking, or clearly needs a moment, reply with exactly [[WAIT]] and nothing else - staying silent is the right response. Never fill their pause with chatter; when they speak again, continue naturally.\n- If the caller's words were only a brief acknowledgement (yeah, okay, mm-hm) while you were talking, continue where you left off instead of starting over - or reply with [[WAIT]] if nothing needs saying.\nThe double-bracket markers are never spoken and never shown to anyone.";

/// VOICE-001 fail-safe: what the caller hears when the responder breaks
/// MID-call (LLM died / synthesis went silent) — a plain apology, then a
/// clean hangup. Local + fixed so it needs nothing but TTS; when TTS itself
/// is the broken half, the hangup still happens (silence must END, never
/// stretch on).
#[cfg(feature = "voice")]
const FALLBACK_LINE: &str = "I'm sorry, I'm having technical trouble taking your call right now. \
Please call back shortly. Goodbye.";

/// VOICE-001, pure for tests: after a reply attempt, is the caller sitting in
/// DEAD AIR? True only when nothing audibly played AND nothing else explains
/// the silence — a barge-in means the caller is talking (their turn is already
/// accumulating), and an operator action means a human has the call.
#[cfg(feature = "voice")]
fn reply_left_dead_air(audible: bool, barged: bool, operator_ended: bool) -> bool {
    !audible && !barged && !operator_ended
}

/// Remove any end-of-call marker the LLM emitted (tolerant to small-model
/// variants: bracketed or bare, any case) and report whether one was present.
/// Returns the cleaned, trimmed text so the marker is never spoken or recorded.
#[cfg(feature = "voice")]
fn strip_end_call_marker(s: &str) -> (String, bool) {
    // Longest / most-bracketed variants first so the bare token never leaves a
    // stray bracket behind.
    const VARIANTS: [&str; 6] = [
        "[[END_CALL]]", "[[END CALL]]", "[END_CALL]", "[END CALL]", "END_CALL", "END CALL",
    ];
    let mut out = s.to_string();
    let mut found = false;
    for v in VARIANTS {
        let vl = v.to_lowercase();
        loop {
            let lower = out.to_lowercase();
            match lower.find(&vl) {
                Some(pos) => {
                    out.replace_range(pos..pos + v.len(), "");
                    found = true;
                }
                None => break,
            }
        }
    }
    (out.trim().to_string(), found)
}

// ── AOK-CTRL-001: cancellable, deadline-bounded call control ────────────────
//
// The pieces below are PURE (fake-clock testable — every decision takes `now`
// as a parameter, which is the clock seam the VOICE-001 deferral asked for):
// the reply-deadline watchdog, the call-level silence timer, the agent-hangup
// policy and the playout-drain math. The impure halves (the reply worker
// thread, the control probe inside TTS playback) live in `run_loop`.

/// An urgent operator action observed while speech was playing — detected by
/// [`ControlProbe`] inside the TTS chunk loop, EXECUTED by the caller right
/// after the speak returns (the `BluetoothManager` is mutably borrowed for
/// the whole playback, so the probe can only record the intent).
#[cfg(feature = "voice")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum CancelAction {
    Hangup { op: Option<String> },
    Reject { op: Option<String> },
}

/// Control-channel probe threaded through speech playback so a hangup/reject
/// lands mid-SENTENCE (within ~one audio chunk, ≈20 ms) instead of waiting for
/// the sentence to finish playing. Urgent actions stop playback and are
/// recorded in `action`; every other control is parked, in arrival order, for
/// the main control loop (same contract as the old per-sentence poll).
#[cfg(feature = "voice")]
struct ControlProbe<'a> {
    rx: &'a std::sync::mpsc::Receiver<RadioControl>,
    parked: &'a mut std::collections::VecDeque<RadioControl>,
    action: Option<CancelAction>,
}

#[cfg(feature = "voice")]
impl<'a> ControlProbe<'a> {
    fn new(
        rx: &'a std::sync::mpsc::Receiver<RadioControl>,
        parked: &'a mut std::collections::VecDeque<RadioControl>,
    ) -> Self {
        Self {
            rx,
            parked,
            action: None,
        }
    }

    /// Drain newly-arrived controls; `true` = an urgent action wants playback
    /// stopped NOW (sticky once set).
    fn poll(&mut self) -> bool {
        if self.action.is_some() {
            return true;
        }
        while let Ok(c) = self.rx.try_recv() {
            match c {
                RadioControl::Hangup { op } => {
                    self.action = Some(CancelAction::Hangup { op });
                    return true;
                }
                RadioControl::Reject { op } => {
                    self.action = Some(CancelAction::Reject { op });
                    return true;
                }
                other => self.parked.push_back(other),
            }
        }
        false
    }
}

/// One message from the detached reply worker to the radio thread. The
/// bounded channel (see `REPLY_CHANNEL_BOUND`) is the backpressure: synthesis
/// paces consumption, so a runaway generation blocks the WORKER, never grows
/// a queue.
#[cfg(feature = "voice")]
enum ReplyMsg {
    Sentence(String),
    /// The stream finished (full text) or failed (reason). Always the last
    /// message the worker sends.
    Done(Result<String, String>),
}

#[cfg(feature = "voice")]
const REPLY_CHANNEL_BOUND: usize = 8;

/// Deadlines for one agent reply, enforced by the radio thread's pump (the
/// worker may be stuck in a blocking read — reqwest 0.11 has no per-read
/// timeout — so the PUMP owns the deadline and abandons the worker, whose
/// whole-request timeout is the eventual backstop).
#[cfg(feature = "voice")]
struct ReplyDeadlines {
    /// The endpoint accepted the request but produced NO stream data yet.
    first_activity: std::time::Duration,
    /// Mid-stream: no data for this long (the per-read idle deadline).
    idle: std::time::Duration,
    /// Whole-reply cap, regardless of progress.
    total: std::time::Duration,
}

#[cfg(feature = "voice")]
const REPLY_DEADLINES: ReplyDeadlines = ReplyDeadlines {
    first_activity: std::time::Duration::from_secs(10),
    idle: std::time::Duration::from_secs(8),
    total: std::time::Duration::from_secs(60),
};

/// Pure deadline verdict: `Some(reason)` when the reply must be abandoned.
/// `last_activity` is `None` until the stream's first line arrives.
#[cfg(feature = "voice")]
fn reply_deadline_exceeded(
    cfg: &ReplyDeadlines,
    started: std::time::Instant,
    last_activity: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<String> {
    if now.duration_since(started) >= cfg.total {
        return Some(format!(
            "the reply exceeded the total deadline ({}s) — abandoned",
            cfg.total.as_secs()
        ));
    }
    match last_activity {
        None if now.duration_since(started) >= cfg.first_activity => Some(format!(
            "the LLM produced no stream data within {}s (first-activity deadline)",
            cfg.first_activity.as_secs()
        )),
        Some(at) if now.duration_since(at) >= cfg.idle => Some(format!(
            "the LLM stream stalled — no data for {}s (idle deadline)",
            cfg.idle.as_secs()
        )),
        _ => None,
    }
}

/// Spoken once when the caller has been silent for a whole window (agent mode).
#[cfg(feature = "voice")]
const SILENCE_CHECK_LINE: &str = "Hello? Are you still there?";
/// Spoken before the max-silence hangup — plain ASCII (TTS + health-path safe).
#[cfg(feature = "voice")]
const SILENCE_GOODBYE_LINE: &str = "I haven't heard anything for a while, so I'll hang up now. \
Please call back if you still need us. Goodbye.";

/// What the silence timer wants done when a window expires.
#[cfg(feature = "voice")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SilenceAction {
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
struct SilenceTimer {
    window: std::time::Duration,
    last_activity: std::time::Instant,
    prompted: bool,
}

#[cfg(feature = "voice")]
impl SilenceTimer {
    fn new(window: std::time::Duration, now: std::time::Instant) -> Self {
        Self {
            window,
            last_activity: now,
            prompted: false,
        }
    }

    /// Any conversational activity (caller speech, a spoken reply) resets the
    /// window AND forgives an earlier check-in prompt.
    fn note_activity(&mut self, now: std::time::Instant) {
        self.last_activity = now;
        self.prompted = false;
    }

    fn check(&mut self, now: std::time::Instant) -> Option<SilenceAction> {
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

/// The configured max-silence window: `maxSilenceSecs` setting →
/// AOKIE_MAX_SILENCE_SECS env (set by the connector at radio start).
/// 0 disables; anything else clamps to a sane band. Default 30s.
#[cfg(feature = "voice")]
fn max_silence_window() -> std::time::Duration {
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
fn playout_drain_wait(
    t0: std::time::Instant,
    queued: std::time::Duration,
    now: std::time::Instant,
) -> std::time::Duration {
    // The cap must clear a full spoken farewell/apology (~6s) — it only
    // guards against a pathological queue, never a normal goodbye.
    const MARGIN: std::time::Duration = std::time::Duration::from_millis(400);
    const CAP: std::time::Duration = std::time::Duration::from_secs(8);
    (t0 + queued + MARGIN).saturating_duration_since(now).min(CAP)
}

/// The agent-hangup POLICY (AOK-CTRL-001): the LLM's end-call marker is only a
/// REQUEST — this validates it against what actually happened on the call
/// before the plugin may hang up. `Proceed.wait` is the computed farewell
/// playout drain.
#[cfg(feature = "voice")]
#[derive(Debug, PartialEq, Eq)]
enum HangupVerdict {
    Proceed { wait: std::time::Duration },
    Skip(&'static str),
}

#[cfg(feature = "voice")]
#[allow(clippy::too_many_arguments)]
fn agent_hangup_verdict(
    requested: bool,
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

/// Live radio status, shared (via `Arc`) between the radio thread (writer)
/// and the main RPC thread (reader) so `phone.status` / `dongle.diagnostics`
/// answer without round-tripping the radio thread.
#[derive(Default)]
pub struct RadioStatus {
    pub initialized: AtomicBool,
    pub connected: AtomicBool,
    pub call_active: AtomicBool,
    pub local_address: Mutex<Option<String>>,
    pub connected_address: Mutex<Option<String>>,
    pub current_caller: Mutex<Option<String>>,
    /// The current call's id (the `call_…` correlation id every event for
    /// this call carries). Set the moment a call starts ringing, cleared on
    /// termination — this is what lets `call.current` recover a live call
    /// after a page refresh and lets call controls verify a `callId`.
    pub current_call_id: Mutex<Option<String>>,
    /// The settings revision in effect (audit AOK-CONFIG-002): stamped by the
    /// connector at spawn and on every settings.set, embedded in call.ended
    /// so a call record identifies exactly which configuration it ran under.
    pub config_version: AtomicU64,
    /// ISO-8601 instant the current call started RINGING (not answered) —
    /// the Live Call screen's timer runs from here.
    pub call_started_at: Mutex<Option<String>>,
    /// Devices seen connected during this radio session (the durable link
    /// keys live in the aokie pairing store; this is the live view).
    pub paired: Mutex<Vec<PairedDevice>>,
    /// Last fatal reason the radio reported (no dongle, driver not bound, â€¦).
    pub last_error: Mutex<Option<String>>,
    /// Speech results that arrived for a call that was no longer current and
    /// were DROPPED instead of being attributed to the wrong caller (audit
    /// C-05). Observable via dongle.diagnostics.
    pub stale_stt_results: AtomicU64,
    /// AOK-BT-001: a lock-free handle to the radio's pairing window, set once the
    /// radio starts. Lets `phone.status` report pairing state (remaining seconds)
    /// without round-tripping the radio thread. `None` until the radio is up.
    pub pairing_window: Mutex<Option<aokie_dongle::bluetooth::PairingWindow>>,
    /// PAIR-001: a lock-free handle to the held SSP numeric comparison, set
    /// once the radio starts. Lets `phone.status` surface {address,
    /// numericValue} for the operator confirm prompt without an RPC.
    pub pairing_confirm: Mutex<Option<aokie_dongle::bluetooth::PairingConfirmSlot>>,
    /// AOK-VOICE-001: the last KNOWN speech-to-text failure (missing/corrupt
    /// model, ORT DLL gone, engine load error). `None` = no known failure.
    /// Seeded by the startup asset preflight, updated by live engine loads.
    /// A set value degrades plugin.health and blocks auto-answer (a
    /// receptionist that can't hear must not answer).
    pub stt_error: Mutex<Option<String>>,
    /// AOK-VOICE-001: the last KNOWN text-to-speech failure — same lifecycle
    /// as `stt_error`, additionally updated by every live speech attempt
    /// (zero-audio outcome sets it, audible speech clears it). A set value
    /// degrades plugin.health and blocks auto-answer (never answer into
    /// silence).
    pub tts_error: Mutex<Option<String>>,
    /// PROC-001: the last KNOWN LLM-reachability failure. Only meaningful when
    /// the IN-PLUGIN agent owns replies (`aiReceptionist` on): a background
    /// probe re-checks the resolved endpoint (aiEndpoint / llama :8080 /
    /// ollama :11434) every ~30s so a dead brain is visible BEFORE a call,
    /// degrades plugin.health, and blocks auto-answer — a receptionist that
    /// can hear and speak but cannot think must not pick up.
    pub llm_error: Mutex<Option<String>>,
    /// VOICE-001: the measured TTS→STT loopback self-test outcome. `None` =
    /// still running (auto-answer stays blocked until it lands — never arm on
    /// unproven engines); populated with ok/failed + duration once done.
    /// Skip cases (HTTP endpoints, env override, non-voice build) record an
    /// ok report with the skip reason so arming isn't held hostage.
    pub self_test: Mutex<Option<VoiceSelfTest>>,
    /// Duplex/floor tuning counters (round 4, content-free): how often each
    /// full-duplex mechanism fired — observable via dongle.diagnostics so
    /// live behaviour is tuned from numbers, not vibes.
    pub early_stt_hits: AtomicU64,
    pub probes_sent: AtomicU64,
    pub probe_commands: AtomicU64,
    pub boundary_yields: AtomicU64,
    pub gap_yields: AtomicU64,
    pub semantic_cuts: AtomicU64,
    pub barge_cuts: AtomicU64,
    /// §9.2 within-call staleness: the turn number of the NEWEST caller turn
    /// emitted for the current call (0 = none yet / call boundary). The
    /// connector validates `call.operatorSpeak`'s optional `inResponseTo`
    /// against this — a flow reply to an older turn gets a typed
    /// `stale_turn` instead of speaking into a conversation that moved on.
    pub last_caller_turn: AtomicU32,
    /// AOK-CTRL-001: whether the RUNNING radio's in-plugin agent owns replies
    /// (the env snapshot the radio actually started with, not the settings bag
    /// which may have changed since). The connector refuses `call.operatorSpeak`
    /// against this — the radio would silently drop it anyway (double-responder
    /// guard), and an accepted-then-dropped command is a lie.
    pub agent_enabled: AtomicBool,
}

/// VOICE-001: one loopback self-test outcome (always compiled — non-voice
/// builds just never populate it with a real run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSelfTest {
    pub ok: bool,
    /// ISO-8601 completion instant.
    pub at: String,
    /// Wall-clock cost of the whole loopback (engine loads + inference).
    pub duration_ms: u64,
    /// What was heard / why it failed / why it was skipped.
    pub detail: String,
}

/// Handle held by the [`Plugin`](crate::connector::Plugin): send control
/// requests and read live status. Dropping it (process shutdown) drops the
/// `control_tx`, which ends the radio loop and shuts the runtime down.
pub struct RadioHandle {
    control_tx: Sender<RadioControl>,
    pub status: Arc<RadioStatus>,
}

impl RadioHandle {
    /// Test-only: a handle wired to a bare channel (no radio thread) so
    /// connector tests can exercise the radio-backed command paths —
    /// acceptance results, operation ids and the agent-owns-replies refusal.
    #[cfg(test)]
    pub fn test_handle() -> (RadioHandle, std::sync::mpsc::Receiver<RadioControl>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            RadioHandle {
                control_tx: tx,
                status: Arc::new(RadioStatus::default()),
            },
            rx,
        )
    }

    pub fn send(&self, c: RadioControl) -> Result<(), String> {
        self.control_tx
            .send(c)
            .map_err(|_| "the radio thread is not running".to_string())
    }

    pub fn is_initialized(&self) -> bool {
        self.status.initialized.load(Ordering::Relaxed)
    }
    pub fn is_connected(&self) -> bool {
        self.status.connected.load(Ordering::Relaxed)
    }
    pub fn is_call_active(&self) -> bool {
        self.status.call_active.load(Ordering::Relaxed)
    }
    pub fn local_address(&self) -> Option<String> {
        self.status.local_address.lock().unwrap().clone()
    }
    pub fn connected_address(&self) -> Option<String> {
        self.status.connected_address.lock().unwrap().clone()
    }
    pub fn current_caller(&self) -> Option<String> {
        self.status.current_caller.lock().unwrap().clone()
    }
    pub fn current_call_id(&self) -> Option<String> {
        self.status.current_call_id.lock().unwrap().clone()
    }
    pub fn call_started_at(&self) -> Option<String> {
        self.status.call_started_at.lock().unwrap().clone()
    }
    pub fn paired(&self) -> Vec<PairedDevice> {
        self.status.paired.lock().unwrap().clone()
    }
    pub fn last_error(&self) -> Option<String> {
        self.status.last_error.lock().unwrap().clone()
    }
    pub fn stale_stt_results(&self) -> u64 {
        self.status.stale_stt_results.load(Ordering::Relaxed)
    }
    /// AOK-VOICE-001: the last known STT failure (None = no known failure).
    pub fn stt_error(&self) -> Option<String> {
        self.status.stt_error.lock().unwrap().clone()
    }
    /// AOK-VOICE-001: the last known TTS failure (None = no known failure).
    pub fn tts_error(&self) -> Option<String> {
        self.status.tts_error.lock().unwrap().clone()
    }
    /// PROC-001: the last known LLM-reachability failure (None = reachable or
    /// not probed — the probe only runs while the in-plugin agent owns replies).
    pub fn llm_error(&self) -> Option<String> {
        self.status.llm_error.lock().unwrap().clone()
    }
    /// VOICE-001: the loopback self-test outcome (None = still running).
    pub fn self_test(&self) -> Option<VoiceSelfTest> {
        self.status.self_test.lock().unwrap().clone()
    }

    /// Round 4: content-free duplex/floor counters — how often each
    /// full-duplex mechanism fired, for dongle.diagnostics tuning.
    pub fn duplex_counters(&self) -> serde_json::Value {
        let s = &self.status;
        json!({
            "earlySttHits": s.early_stt_hits.load(Ordering::Relaxed),
            "probesSent": s.probes_sent.load(Ordering::Relaxed),
            "probeCommands": s.probe_commands.load(Ordering::Relaxed),
            "boundaryYields": s.boundary_yields.load(Ordering::Relaxed),
            "gapYields": s.gap_yields.load(Ordering::Relaxed),
            "semanticCuts": s.semantic_cuts.load(Ordering::Relaxed),
            "bargeCuts": s.barge_cuts.load(Ordering::Relaxed),
        })
    }

    /// AOK-BT-001: seconds left in the pairing window, 0 when closed or the radio
    /// isn't up yet. Lock-free read of the shared window (reflects timeout + a
    /// successful-bond auto-close without polling the radio thread).
    pub fn pairing_window_remaining_secs(&self) -> u64 {
        self.status
            .pairing_window
            .lock()
            .ok()
            .and_then(|w| w.as_ref().map(|w| w.remaining_secs()))
            .unwrap_or(0)
    }

    /// AOK-BT-001: open a bounded, discoverable pairing window for `seconds`.
    pub fn start_pairing(&self, seconds: u64) -> Result<(), String> {
        self.send(RadioControl::StartPairing { seconds })
    }

    /// AOK-BT-001: close the pairing window now.
    pub fn stop_pairing(&self) -> Result<(), String> {
        self.send(RadioControl::StopPairing)
    }

    /// AOK-BT-001: forget a bonded device (blocks briefly on the radio thread).
    pub fn remove_paired(&self, address: String) -> Result<bool, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::RemovePaired { address, reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the removePaired request".to_string())?
    }

    /// Bonded (revocable) devices with their captured friendly names
    /// (address, name) — round-trips the radio thread, which owns the store.
    pub fn list_bonded(&self) -> Result<Vec<(String, Option<String>)>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::ListBonded { reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the listPaired request".to_string())
    }

    /// The connected phone's captured friendly name/model, if known yet.
    /// Round-trips the radio thread (the name lives in the radio runtime).
    pub fn connected_name(&self) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.send(RadioControl::ConnectedName { reply: tx }).is_err() {
            return None;
        }
        rx.recv_timeout(std::time::Duration::from_secs(3))
            .ok()
            .flatten()
    }

    /// Disconnect the connected phone but KEEP the bond (remote reconnect/
    /// unstick). Blocks briefly on the radio thread.
    pub fn disconnect(&self, address: String) -> Result<bool, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::Disconnect { address, reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the disconnect request".to_string())?
    }

    /// HARD-001: reconnect a bonded phone from OUR side (page + outbound HFP
    /// setup). Blocks briefly on the radio thread; true = attempt started.
    pub fn connect(&self, address: String) -> Result<bool, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::Connect { address, reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| "the radio did not answer the connect request".to_string())?
    }

    /// PAIR-001: the held SSP numeric comparison awaiting the operator, if
    /// any (lock-free slot read; expired prompts read as None).
    pub fn pending_pairing_confirm(
        &self,
    ) -> Option<aokie_dongle::bluetooth::PendingPairingConfirm> {
        self.status
            .pairing_confirm
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|s| s.get()))
    }

    /// PAIR-001: resolve the held SSP numeric comparison (blocks briefly on
    /// the radio thread, which owns the HCI transport).
    pub fn confirm_pairing(&self, address: String, accept: bool) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::ConfirmPairing {
            address,
            accept,
            reply: tx,
        })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the confirmPairing request".to_string())?
    }
}

/// The radio's outbox reference: the store plus HOW delivery is bookkept
/// (Legacy write-marks-sent vs ack-awaited — audit INT-003). Carried as one
/// value so every emit site stays a single `outbox` argument.
type OutboxRef<'a> = Option<(&'a Outbox, crate::event_bridge::EmitMode)>;

/// Emit one event best-effort: essential events route through the outbox
/// (write-before-emit) when it is open; otherwise fall back to a direct
/// stdout notification so a failed outbox never swallows a live call event.
fn emit(outbox: OutboxRef<'_>, sink: &mut dyn Sink, event: DesktopEvent) {
    match outbox {
        Some((o, mode)) => {
            if let Err(e) = emit_event(sink, o, &event, false, mode) {
                eprintln!("[aokie-plugin] radio emit '{}' failed: {e}", event.name);
            }
        }
        None => {
            let line = crate::rpc::notification_line("event.emit", json!({ "event": event }));
            let _ = sink.send_line(&line);
        }
    }
}

/// Emit an `aokie.call.turn.final` transcript turn, matching the contract the
/// Receptionist pack's app-logic + flow bindings expect: `{callId, turn,
/// speaker, text}` with a per-turn-unique idempotency key (`turn.<n>.final`) so
/// the app-logic dedup doesn't drop turns after the first. `speaker` is
/// "caller" (STT) or "bot" (Aokie's own speech); a flow gates its reply on
/// `speaker === 'caller'` so Aokie never answers itself.
#[cfg(feature = "voice")]
fn emit_turn(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    corr: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
) {
    emit_turn_with_delivery(outbox, sink, corr, turn_index, speaker, text, None, None)
}

/// AOK-CTRL-001: bot turns carry a structured per-turn DELIVERY status —
/// `complete` (every recorded sentence audibly played), `interrupted` (caller
/// barge-in cut it short), `operator_ended` (an operator action stopped it) or
/// `error` (synthesis/stream failure mid-reply). The `text` is already only
/// what actually played (the truthful-transcript rule); `delivery` says WHY it
/// may be shorter than the generation. Additive payload field — existing
/// consumers ignore it. Caller turns have no delivery dimension (`None`).
#[cfg(feature = "voice")]
#[allow(clippy::too_many_arguments)]
fn emit_turn_with_delivery(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    corr: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
    delivery: Option<&str>,
    // Speech-START stamp (None = now): bot turns are emitted when the reply
    // FINISHES, but their place in the conversation is when they began.
    at: Option<&str>,
) {
    emit_turn_full(
        outbox, sink, corr, turn_index, speaker, text, delivery, None, false, at,
    )
}

/// Full turn emitter: `kind: Some("control")` marks a caller turn that was a
/// FLOOR COMMAND ("wait", "stop", "slower") handled deterministically by the
/// duplex coordinator — recorded truthfully in the transcript, but flows and
/// downstream logic can (and should) skip business handling for it. Additive
/// payload field — existing consumers ignore it.
#[cfg(feature = "voice")]
#[allow(clippy::too_many_arguments)]
fn emit_turn_full(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    corr: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
    delivery: Option<&str>,
    kind: Option<&str>,
    overlapped: bool,
    at_override: Option<&str>,
) {
    let mut payload = json!({
        "callId": corr,
        "turn": turn_index,
        "speaker": speaker,
        "text": text,
        // Overlap turns carry their SPEECH-START estimate, not commit time.
        "at": at_override
            .map(str::to_string)
            .unwrap_or_else(aokie_core::events::now_iso8601),
    });
    if let Some(d) = delivery {
        payload["delivery"] = json!(d);
    }
    if let Some(k) = kind {
        payload["kind"] = json!(k);
    }
    // Truthful ordering (live report 2026-07-13): a turn seeded from OVERLAP
    // capture is recorded when it flushes — AFTER the bot line it was spoken
    // over. The flag says "this began while the bot was talking", so readers
    // don't misread the record order as the speech order. Additive field.
    if overlapped {
        payload["overlapped"] = json!(true);
    }
    emit(
        outbox,
        sink,
        aokie_core::events::aokie_turn_event(true, corr, turn_index, payload),
    );
}

/// Emit the buffered `call.incoming` NOW if it hasn't gone out yet (audit
/// AOK-LIF-001). The incoming event is normally held briefly for caller-ID
/// enrichment; every other call-scoped event (ringing/answered/audio/ended)
/// forces it out first, so `incoming` ALWAYS precedes the rest of its call's
/// lifecycle regardless of the hold. `from` is whatever caller id has
/// arrived — empty when the phone hasn't sent one (never a sentinel, §8).
fn flush_incoming_if_pending(
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    if !tracker.current().is_some_and(|s| s.incoming_pending()) {
        return;
    }
    let (corr, from) = {
        let s = tracker.current_mut().unwrap();
        s.mark_incoming_emitted();
        (s.id.clone(), s.caller_id.clone().unwrap_or_default())
    };
    emit(
        outbox,
        sink,
        aokie_core::events::aokie_event(
            crate::contract::events::CALL_INCOMING,
            &corr,
            json!({"callId": corr, "from": from, "at": aokie_core::events::now_iso8601()}),
        ),
    );
}

/// The canonical `call.ended` emission — shared by the phone's real
/// CallTerminated and the synthesized device-loss termination (audit
/// AOK-LIF-003), so both produce ONE identical terminal event shape.
/// The after-call flows key off this payload (contract §events): callId
/// mirrors the envelope correlation, from/callerPhone carry the caller id,
/// durationSeconds/durationMs count from ANSWER. The outcome comes from the
/// session state machine (audit AK-001): answered → "completed" (even a
/// sub-second call), operator-rejected → "rejected", never answered →
/// "missed", radio link gone → reason "device_lost".
fn emit_call_ended(
    ended: &crate::call_session::EndedCall,
    config_version: u64,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    use aokie_core::events::{aokie_event, now_iso8601};
    let from = ended.caller_id.clone().unwrap_or_default();
    emit(
        outbox,
        sink,
        aokie_event(
            crate::contract::events::CALL_ENDED,
            &ended.id,
            json!({
                "at": now_iso8601(),
                "reason": ended.reason,
                "callId": ended.id,
                "from": from,
                "callerPhone": from,
                "durationSeconds": ended.duration_seconds,
                "durationMs": ended.duration_ms as u64,
                "outcome": ended.outcome,
                "configVersion": config_version,
            }),
        ),
    );
}

/// AOK-CTRL-001: the authoritative FAILURE record for an accepted call
/// control. The connector's command result only ever says `accepted/queued`
/// (the enqueue succeeded); when the radio later fails to act on the phone,
/// this emits `aokie.hardware.error` carrying `code: "control_failed"`, the
/// action and the operation id from the accepted result — so a flow/UI can
/// correlate "my command didn't happen" instead of trusting a premature verb.
#[cfg(target_os = "windows")]
fn emit_control_failed(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    tracker: &crate::call_session::SessionTracker,
    action: &str,
    op: Option<&str>,
    error: &str,
) {
    use aokie_core::events::{aokie_event_occurrence, occurrence_id};
    let corr = tracker.call_id().unwrap_or("radio").to_string();
    emit(
        outbox,
        sink,
        aokie_event_occurrence(
            crate::contract::events::HARDWARE_ERROR,
            &corr,
            &occurrence_id(),
            json!({
                "message": format!("{action} failed on the radio: {error}"),
                "code": "control_failed",
                "action": action,
                "operationId": op,
            }),
        ),
    );
}

/// PII gate for conversation content in logs (audit PRIV-001/C-06): stderr is
/// captured by Desktop's log ring, so caller speech and agent replies appear
/// verbatim only when the operator explicitly opts in (`AOKIE_LOG_CONTENT=1`);
/// default logs carry only lengths.
#[cfg(feature = "voice")]
fn content_for_log(text: &str) -> String {
    if std::env::var("AOKIE_LOG_CONTENT").map(|v| v == "1").unwrap_or(false) {
        format!("{text:?}")
    } else {
        format!("[{} chars]", text.chars().count())
    }
}

/// Heuristic self-echo guard for the in-plugin agent: true when `caller` (a fresh
/// transcript) is mostly the same words as Aokie's last spoken reply `bot` â€” i.e.
/// Aokie's own TTS leaked back into the mic and STT transcribed it. Keeps Aokie
/// from answering itself if any audio escapes the half-duplex mute.
#[cfg(feature = "voice")]
fn looks_like_echo(caller: &str, bot: &str) -> bool {
    fn words(s: &str) -> Vec<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_string())
            .collect()
    }
    let c = words(caller);
    if c.len() < 3 {
        return false; // too short to judge (e.g. "yes", "ok")
    }
    let b: std::collections::HashSet<String> = words(bot).into_iter().collect();
    if b.is_empty() {
        return false;
    }
    let overlap = c.iter().filter(|w| b.contains(*w)).count();
    (overlap as f32 / c.len() as f32) >= 0.7
}

/// How long a caller turn stays OPEN after a transcript that looks unfinished
/// (audit AK-008): callers read phone numbers in groups with pauses well past
/// the STT endpoint, and replying into that pause both talks over them and
/// books half a number. Long enough to bridge a between-groups breath, short
/// enough that a genuinely finished number only delays the reply by a beat.
#[cfg(feature = "voice")]
const CONTINUATION_HOLD: Duration = Duration::from_millis(1400);

/// Cap on a merged caller turn — past this, flush regardless (a runaway hold
/// must never buffer the whole call into one turn).
#[cfg(feature = "voice")]
const CONTINUATION_MAX_CHARS: usize = 240;

/// True when a transcript's tail says "the caller isn't done" (audit AK-008):
/// it ends in a digit group, a spoken number word, or a connective that
/// announces one ("my number is …"). Drives the continuation hold above.
#[cfg(feature = "voice")]
fn ends_with_unfinished_number(text: &str) -> bool {
    let Some(last) = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .last()
        .map(str::to_string)
    else {
        return false;
    };
    if last.chars().all(|c| c.is_ascii_digit()) {
        return true; // "…0412", "…345" — a digit group just ended at a pause
    }
    matches!(
        last.as_str(),
        // Spoken digits and digit multipliers…
        "zero" | "one" | "two" | "three" | "four" | "five" | "six" | "seven" | "eight"
            | "nine" | "oh" | "double" | "triple"
            // …and connectives that promise a number/detail is coming.
            | "is" | "its" | "on" | "number" | "and" | "um" | "uh"
    )
}

/// General turn-completion heuristic (plan §6.2, live 2026-07-13: "Um no" +
/// "That's all really" arrived as two turns and each got its own reply): a
/// turn is held open for the continuation window when it still sounds
/// mid-thought — a number tail, a bare hesitation, a trailing connective, or
/// a short fragment that OPENS with a filler.
#[cfg(feature = "voice")]
fn turn_looks_unfinished(text: &str) -> bool {
    if ends_with_unfinished_number(text) {
        return true;
    }
    if crate::duplex::is_hesitation(text) {
        return true;
    }
    let words: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    if matches!(
        words.last().map(String::as_str),
        Some("but" | "because" | "then" | "or" | "well" | "also")
    ) {
        return true;
    }
    // "Um no", "Uh maybe Tuesday" — a short fragment opening with a filler
    // usually continues after the breath.
    if words.len() <= 3
        && matches!(
            words.first().map(String::as_str),
            Some("uh" | "um" | "er" | "ah" | "hmm" | "well" | "erm")
        )
    {
        return true;
    }
    false
}

/// A caller turn being merged across STT utterances (audit AK-008). `corr` is
/// pinned at first fragment so a turn that outlives its call (hangup inside
/// the hold window) still lands against the right call record.
#[cfg(feature = "voice")]
struct PendingTurn {
    corr: String,
    text: String,
    flush_at: Instant,
}

// â”€â”€ Windows: the real radio â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Start the live radio on a background thread. Returns immediately with a
/// handle; initialisation happens asynchronously and is reflected in
/// [`RadioStatus`] (and via an `aokie.dongle.ready` / `aokie.hardware.error`
/// event). `preferred_path` pins a specific WinUSB dongle path, or `None`
/// takes the first enumerated HCI controller.
#[cfg(target_os = "windows")]
pub fn spawn(
    data_dir: std::path::PathBuf,
    preferred_path: Option<String>,
    auto_answer: bool,
    answer_tone: bool,
    reenumerate_hwid: Option<String>,
    greeting: Option<String>,
    ack_mode: bool,
) -> Result<RadioHandle, String> {
    use aokie_dongle::bluetooth::BluetoothManager;
    use std::sync::mpsc;

    let (control_tx, control_rx) = mpsc::channel::<RadioControl>();
    let status = Arc::new(RadioStatus::default());
    let status_thread = status.clone();

    std::thread::Builder::new()
        .name("aokie-plugin-radio".to_string())
        // Match the runtime thread's generous stack â€” the deep ACL â†’ L2CAP â†’
        // RFCOMM â†’ HFP dispatch overflowed the 1 MiB Windows default.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            // Software "virtual replug": on a cold boot the dongle's SCO iso
            // endpoint is dead until the device is re-enumerated (physically
            // unplug/replug). CM_Reenumerate the device before opening it so a
            // headless receptionist works after boot with no manual replug.
            // Best-effort + gated (settings.reenumerateHwid); settle briefly so
            // the device + WinUSB re-bind before we open it.
            if let Some(hwid) = reenumerate_hwid.as_deref() {
                match aokie_dongle::winusb::restart_device(hwid) {
                    Ok(()) => {
                        eprintln!("[aokie-plugin] restarted {hwid} (virtual replug: remove + re-add) â€” settling 3s");
                        std::thread::sleep(std::time::Duration::from_millis(3000));
                    }
                    Err(e) => eprintln!("[aokie-plugin] virtual replug {hwid} failed (continuing): {e}"),
                }
            }
            // Raise this process's timer resolution to 1 ms for the lifetime of
            // the radio (see Cargo.toml note). The SCO iso path services USB
            // frames every 1 ms; at the ~15.6 ms per-process default a bare
            // plugin's read_sco waits + TX pacing are too coarse and every iso
            // transfer fails (empty + Win32 87). The original Tauri app gets
            // this for free via WebView2. timeBeginPeriod is ref-counted and
            // paired with timeEndPeriod below.
            unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
            let mut bt = match BluetoothManager::new_with_preferred_dongle(preferred_path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[aokie-plugin] radio failed to start: {e}");
                    *status_thread.last_error.lock().unwrap() = Some(e);
                    unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
                    return;
                }
            };
            // AOK-BT-001: publish the shared pairing window so phone.status can
            // report pairing state lock-free.
            *status_thread.pairing_window.lock().unwrap() = Some(bt.pairing_window());
            // PAIR-001: publish the shared pending-confirmation slot the same way.
            *status_thread.pairing_confirm.lock().unwrap() = Some(bt.pairing_confirm_slot());
            // Second connection to the same outbox file (see module docs).
            // Fail CLOSED (audit AOK-RUN-001): call/SMS events are the
            // business record — if they can't be durably queued, the radio
            // must not run and LOOK available while silently downgrading to
            // direct stdout. The error lands in last_error, so health reads
            // degraded and the operator sees why.
            let outbox = match Outbox::open(&data_dir.join(crate::connector::OUTBOX_FILE)) {
                Ok(o) => o,
                Err(e) => {
                    let msg = format!(
                        "radio outbox unavailable ({e}) — refusing to start without durable event delivery"
                    );
                    eprintln!("[aokie-plugin] {msg}");
                    *status_thread.last_error.lock().unwrap() = Some(msg);
                    unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
                    return;
                }
            };
            let mode = crate::event_bridge::EmitMode::for_host(ack_mode, crate::event_bridge::legacy_host_allowed());
            let mut sink = crate::event_bridge::StdoutSink::new();
            // Exit supervision (audit AOK-RUN-001): the spawner returned long
            // ago — if the loop panics or returns, readiness must flip
            // IMMEDIATELY, never leaving "initialized/connected" green on a
            // thread that no longer exists.
            let status_exit = status_thread.clone();
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_loop(
                    &mut bt,
                    Some((&outbox, mode)),
                    &mut sink,
                    control_rx,
                    status_thread,
                    auto_answer,
                    answer_tone,
                    greeting,
                );
            }));
            if ran.is_err() {
                let msg = "radio thread panicked — phone service stopped".to_string();
                eprintln!("[aokie-plugin] {msg}");
                *status_exit.last_error.lock().unwrap() = Some(msg);
            }
            status_exit.initialized.store(false, Ordering::Relaxed);
            status_exit.connected.store(false, Ordering::Relaxed);
            status_exit.call_active.store(false, Ordering::Relaxed);
            *status_exit.current_call_id.lock().unwrap() = None;
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
        })
        .map_err(|e| format!("spawn radio thread: {e}"))?;

    Ok(RadioHandle { control_tx, status })
}

/// A short two-note chime (mono i16 at the SCO sample rate) used to verify the
/// OUTBOUND SCO audio path actually reaches the caller on a given dongle â€” real
/// TTS speech replaces it once outbound audio is confirmed. Fades each note in
/// and out to avoid clicks.
#[cfg(target_os = "windows")]
fn greeting_tone(sample_rate: u16) -> Vec<i16> {
    let sr = sample_rate.max(8000) as f32;
    let mut out = Vec::new();
    for &(freq, secs) in &[(660.0f32, 0.35f32), (880.0, 0.5)] {
        let n = (sr * secs) as usize;
        for i in 0..n {
            let t = i as f32 / sr;
            let env = ((i as f32 / n as f32) * std::f32::consts::PI).sin(); // 0â†’1â†’0
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * env * 0.6;
            out.push((s * i16::MAX as f32) as i16);
        }
    }
    out
}

/// Result of speaking a phrase: how long the audio will play out, whether the
/// caller barged in (started speaking) mid-phrase so we cut it short, and —
/// when they did — the echo-cancelled audio of what they said WHILE Aokie was
/// still talking (audit AK-008). Barge detection needs sustained speech before
/// it trips, so without this capture the first words of an interruption (the
/// leading digits of a phone number, classically) were used for detection and
/// then thrown away — the STT only ever saw the part spoken after the trip.
/// Delivery-truth v1 (architecture guide §6.3): the numbers needed to
/// conservatively estimate how much of a CUT span the caller actually heard.
/// The engine knows exactly what was QUEUED to the SCO and how long audio had
/// been flowing when the cut landed; remote playout stays an estimate.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CutEstimate {
    /// Estimated audible playout at the cut: wall time since the first audio
    /// frame, capped by what was queued (audio can't play faster than it was
    /// fed).
    audible_ms: u64,
    /// Everything pushed to the SCO TX queue (includes up to ~PLAYOUT_LEAD of
    /// audio that was flushed unplayed by the cut).
    queued_ms: u64,
    /// The span's TOTAL synthesized duration — known exactly only when
    /// synthesis finished before the cut (queued + discarded pending PCM).
    synthesized_ms: Option<u64>,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
struct SpeakOutcome {
    dur: std::time::Duration,
    barged: bool,
    /// AEC-cleaned caller speech captured during playback, at the SCO rate,
    /// starting a short pre-roll before their first above-threshold frame.
    /// Empty when nothing crossed the speech threshold (or half-duplex mode).
    captured_speech: Vec<i16>,
    /// AOK-CTRL-001: playback was cut short by an urgent control (the probe's
    /// `action` says which) — the caller executes it right after this returns.
    cancelled: bool,
    /// Phase 2 probe lane: the caller spoke a FLOOR COMMAND ("wait"/"stop")
    /// over this speech and the priority STT probe caught it mid-sentence —
    /// playback was cut (even through a protected span; explicit commands
    /// always win) and the dialogue should enter its pause state NOW.
    commanded: Option<crate::duplex::CallerIntent>,
    /// §6.3 delivery truth: set when playback was CUT SHORT (barge / spoken
    /// command / urgent control) after some audio played — the numbers a
    /// consumer needs to estimate the audible prefix. `None` = the span
    /// played to its natural end, or nothing played at all.
    cut_est: Option<CutEstimate>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
struct HttpSpeechFallback {
    endpoint: Option<String>,
    failed_for_call: bool,
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
impl HttpSpeechFallback {
    fn new(endpoint: Option<String>) -> Self {
        Self {
            endpoint: normalize_endpoint(endpoint),
            failed_for_call: false,
        }
    }

    fn from_env(var: &str) -> Self {
        Self::new(std::env::var(var).ok())
    }

    fn configure(&mut self, endpoint: Option<String>) {
        let endpoint = normalize_endpoint(endpoint);
        if endpoint != self.endpoint {
            self.endpoint = endpoint;
            self.failed_for_call = false;
        }
    }

    fn reset_call(&mut self) {
        self.failed_for_call = false;
    }

    fn endpoint_for_call(&self) -> Option<&str> {
        if self.failed_for_call {
            None
        } else {
            self.endpoint.as_deref()
        }
    }

    fn mark_failed_for_call(&mut self) -> bool {
        if self.endpoint.is_some() && !self.failed_for_call {
            self.failed_for_call = true;
            true
        } else {
            false
        }
    }
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
fn normalize_endpoint(endpoint: Option<String>) -> Option<String> {
    endpoint
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(feature = "voice")]
enum SttWork {
    /// One finished caller utterance, stamped with the call it belongs to
    /// (audit C-05): the worker skips jobs whose generation is no longer
    /// current, and the consumer drops results the same way — a slow
    /// transcription from call A can never be attributed to call B.
    Utterance {
        generation: u64,
        utterance: u32,
        samples: Vec<f32>,
    },
    /// Priority control probe (phase 2 + §3.2): a SNAPSHOT of the caller's
    /// overlap speech while Aokie is talking, transcribed so a spoken
    /// "wait"/"stop" can cut playback mid-sentence instead of after the
    /// reply. Local-engine ONLY (a probe must never widen audio egress to an
    /// HTTP endpoint), answered on the dedicated probe channel, and NEVER
    /// consuming/truncating the content buffer — the full utterance still
    /// arrives as a normal `Utterance` at its endpoint.
    Probe {
        generation: u64,
        samples: Vec<f32>,
    },
    Configure {
        endpoint: Option<String>,
    },
    ResetCall,
}

/// A finished transcription, still carrying the identity of the call whose
/// audio produced it.
#[cfg(feature = "voice")]
struct SttResult {
    generation: u64,
    utterance: u32,
    text: String,
}

/// The HTTP-TTS endpoint with its sticky per-call fallback. Owned by the
/// synth WORKER since phase 2 (crate::synth) — the radio thread only forwards
/// configure/reset messages.
#[cfg(feature = "voice")]
pub(crate) struct HttpTtsRuntime {
    fallback: HttpSpeechFallback,
}

#[cfg(feature = "voice")]
impl HttpTtsRuntime {
    pub(crate) fn from_env(var: &str) -> Self {
        Self {
            fallback: HttpSpeechFallback::from_env(var),
        }
    }

    pub(crate) fn configure(&mut self, endpoint: Option<String>) {
        self.fallback.configure(endpoint);
    }

    pub(crate) fn reset_call(&mut self) {
        self.fallback.reset_call();
    }

    pub(crate) fn endpoint_for_call(&self) -> Option<String> {
        self.fallback.endpoint_for_call().map(str::to_string)
    }

    pub(crate) fn mark_failed_for_call(&mut self) -> bool {
        self.fallback.mark_failed_for_call()
    }
}

/// One whole-utterance HTTP TTS request against a hardened per-endpoint
/// client (AOK-ENDPOINT-001). Worker-side entry point (crate::synth).
#[cfg(feature = "voice")]
pub(crate) fn http_tts_synthesize_at(
    endpoint: &str,
    text: &str,
    voice: &str,
) -> Result<crate::speech_wire::WavPcm, String> {
    let client = http_speech_client(endpoint)
        .map_err(|e| format!("TTS endpoint rejected ({e})"))?;
    http_tts_synthesize(&client, endpoint, text, voice)
}

/// Hardened per-endpoint speech client (audit AOK-ENDPOINT-001): redirects disabled,
/// hostname endpoints DNS-validated + pinned, cached per endpoint. Err = the endpoint
/// must not receive caller audio; callers surface it via their existing failure paths
/// (mark_failed_for_call → sticky in-process fallback).
#[cfg(feature = "voice")]
fn http_speech_client(endpoint: &str) -> Result<reqwest::blocking::Client, String> {
    crate::endpoint_http::client_for(endpoint, std::time::Duration::from_secs(30), None)
}

#[cfg(feature = "voice")]
fn http_stt_transcribe(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    samples_16k: &[f32],
) -> Result<String, String> {
    let pcm: Vec<i16> = samples_16k
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let wav = crate::speech_wire::encode_wav_pcm16_mono(&pcm, 16_000);
    let audio = format!(
        "data:audio/wav;base64,{}",
        crate::speech_wire::base64_encode(&wav)
    );
    let resp = client
        .post(endpoint)
        .json(&serde_json::json!({ "audio": audio }))
        .send()
        .map_err(|e| format!("stt http request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("stt http responded {}", resp.status()));
    }
    let v: serde_json::Value = resp
        .json()
        .map_err(|e| format!("stt http response was not JSON: {e}"))?;
    Ok(v.get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "stt http response missing text".to_string())?
        .trim()
        .to_string())
}

#[cfg(feature = "voice")]
fn http_tts_synthesize(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    text: &str,
    voice: &str,
) -> Result<crate::speech_wire::WavPcm, String> {
    let resp = client
        .post(endpoint)
        .json(&serde_json::json!({ "input": text, "voice": voice }))
        .send()
        .map_err(|e| format!("tts http request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("tts http responded {}", resp.status()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bytes = resp
        .bytes()
        .map_err(|e| format!("tts http response read failed: {e}"))?
        .to_vec();
    let looks_json = bytes.iter().find(|b| !b.is_ascii_whitespace()).copied() == Some(b'{');
    let audio_bytes = if content_type.contains("json") || looks_json {
        decode_tts_json_audio(&bytes)?
    } else {
        bytes
    };
    let wav = crate::speech_wire::decode_wav_mono_i16(&audio_bytes)?;
    if wav.samples.is_empty() {
        return Err("tts http response contained no audio samples".to_string());
    }
    Ok(wav)
}

#[cfg(feature = "voice")]
fn decode_tts_json_audio(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let v: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("tts http JSON response parse failed: {e}"))?;
    let audio = v
        .get("b64_json")
        .or_else(|| v.get("audio"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| "tts http JSON response missing b64_json/audio".to_string())?;
    crate::speech_wire::base64_decode_text(audio)
}

/// Feed one captured mic chunk through the echo canceller and update the
/// sustained-speech counter, returning `true` once the caller has spoken over
/// Aokie for `need` frames straight. `armed` gates the counting so the adaptive
/// filter has time to converge at a reply's onset (we still call
/// `process_capture` while un-armed â€” to keep the reference FIFO aligned with
/// capture â€” but never trip). Shared by the synth callback and the playout monitor.
///
/// AK-008: the cleaned audio is APPENDED to `captured` (not discarded) and the
/// first above-threshold frame's offset is recorded in `speech_start`, so the
/// caller's words spoken BEFORE the barge trips can be prepended to their
/// utterance instead of being lost to detection.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
fn detect_barge(
    aec: &mut crate::aec::EchoCanceller,
    mic: &[i16],
    frame: usize,
    capture_thr: f32,
    trip_thr: f32,
    armed: bool,
    speech_frames: &mut u32,
    need: u32,
    captured: &mut Vec<i16>,
    speech_start: &mut Option<usize>,
) -> bool {
    let cleaned = aec.process_capture(mic);
    scan_barge_frames(
        &cleaned,
        frame,
        capture_thr,
        trip_thr,
        armed,
        speech_frames,
        need,
        captured,
        speech_start,
    )
}

/// The scratchpad's own speech gate — the same level as the main VAD's
/// SPEECH_RMS. Live finding 2026-07-13: the echo canceller suppresses
/// near-end speech while the bot plays, so normal-volume overlap often sits
/// BETWEEN the VAD gate and the barge threshold — marking capture at the
/// barge threshold made the bot deaf to it. Capture marks at THIS gate; the
/// acoustic barge still requires the sustained, higher `trip_thr`.
#[cfg(all(target_os = "windows", feature = "voice"))]
const CAPTURE_RMS: f32 = 350.0;

/// Pure scan half of [`detect_barge`] (unit-testable without an AEC): append
/// `cleaned` to the capture buffer, note the first frame above the CAPTURE
/// gate (armed or not — the scratchpad always hears), and report whether
/// sustained speech above the TRIP threshold barged (armed only).
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
fn scan_barge_frames(
    cleaned: &[i16],
    frame: usize,
    capture_thr: f32,
    trip_thr: f32,
    armed: bool,
    speech_frames: &mut u32,
    need: u32,
    captured: &mut Vec<i16>,
    speech_start: &mut Option<usize>,
) -> bool {
    let base = captured.len();
    captured.extend_from_slice(cleaned);
    let mut tripped = false;
    for (i, f) in cleaned.chunks(frame).enumerate() {
        let rms = crate::voice::frame_rms(f);
        if rms > capture_thr && speech_start.is_none() {
            *speech_start = Some(base + i * frame);
        }
        if !armed {
            continue;
        }
        if rms > trip_thr {
            *speech_frames += 1;
            if *speech_frames >= need {
                tripped = true;
                break;
            }
        } else {
            *speech_frames = speech_frames.saturating_sub(1);
        }
    }
    tripped
}

/// §12.3 synthetic-audio seam: the paced playback engine's view of the audio
/// transport. Live, this is the SCO link on the [`BluetoothManager`]; the
/// synthetic rig (mod `synthetic_audio`) drives a scripted link that serves
/// echo-mix mic frames and records everything that "played".
#[cfg(all(target_os = "windows", feature = "voice"))]
trait AudioLink {
    /// Non-blocking mic drain: one captured chunk, if any arrived.
    fn try_recv_audio(&mut self) -> Option<Vec<i16>>;
    /// Queue PCM for playout.
    fn send_audio(&mut self, pcm: &[i16]);
}

#[cfg(all(target_os = "windows", feature = "voice"))]
impl AudioLink for aokie_dongle::bluetooth::BluetoothManager {
    fn try_recv_audio(&mut self) -> Option<Vec<i16>> {
        aokie_dongle::bluetooth::BluetoothManager::try_recv_audio(self).map(|a| a.samples)
    }
    fn send_audio(&mut self, pcm: &[i16]) {
        aokie_dongle::bluetooth::BluetoothManager::send_audio(self, pcm);
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
struct TtsChunkPlayback {
    t0: std::time::Instant,
    t_first: std::time::Instant,
    first: bool,
    samples: usize,
    barged: bool,
    frame: usize,
    speech_frames: u32,
    need: u32,
    grace: std::time::Duration,
    /// AK-008: AEC-cleaned mic audio accumulated during playback (SCO rate).
    captured: Vec<i16>,
    /// Offset in `captured` of the caller's first above-threshold frame.
    speech_start: Option<usize>,
    sample_rate: u16,
    /// AOK-CTRL-001: an urgent control stopped playback (see [`ControlProbe`]).
    cancelled: bool,
    /// Interrupt policy for THIS span: `None` = yield the instant a barge
    /// trips (the classic behaviour); `Some(budget)` = a finish-the-span
    /// span (phone number, `[[important]]` detail) keeps playing for at
    /// most `budget` after the overlap started, then yields. Caller speech
    /// is captured throughout either way, and urgent controls (hangup /
    /// reject) still cut at chunk granularity.
    finish_extra: Option<std::time::Duration>,
    /// When the barge first tripped (starts the finish budget).
    barged_at: Option<std::time::Instant>,
    /// The probe lane caught a spoken floor command ("wait"/"stop") in the
    /// overlap capture: stop NOW — an explicit command beats every policy,
    /// protected spans included.
    semantic: Option<crate::duplex::CallerIntent>,
    /// DUCKING (the "nudge" texture): caller speech was detected while THIS
    /// span was audibly playing — outbound gain ramps down to make room
    /// while the floor decision (finish clause / yield / barge) plays out.
    speech_during_playback: bool,
    /// Current outbound gain (1.0 → DUCK_GAIN over ~100 ms once ducked).
    duck_gain: f32,
    /// When the current duck began — after a short window the gain ramps
    /// BACK up, so a brief backchannel doesn't leave the rest of the
    /// sentence whispering. Sustained loud speech re-triggers it.
    ducked_at: Option<std::time::Instant>,
}

/// Ducked outbound level once the caller talks over a playing span, and the
/// per-chunk (~20 ms) ramp step toward it.
#[cfg(all(target_os = "windows", feature = "voice"))]
const DUCK_GAIN: f32 = 0.35;
#[cfg(all(target_os = "windows", feature = "voice"))]
const DUCK_RAMP_STEP: f32 = 0.15;

#[cfg(all(target_os = "windows", feature = "voice"))]
impl TtsChunkPlayback {
    fn new(sample_rate: u16, finish_extra: Option<std::time::Duration>) -> Self {
        let t0 = std::time::Instant::now();
        Self {
            t0,
            t_first: t0,
            first: true,
            samples: 0,
            barged: false,
            frame: (sample_rate as usize / 100).max(80),
            speech_frames: 0,
            need: 22,
            grace: std::time::Duration::from_millis(350),
            captured: Vec::new(),
            speech_start: None,
            sample_rate,
            cancelled: false,
            finish_extra,
            barged_at: None,
            semantic: None,
            speech_during_playback: false,
            duck_gain: 1.0,
            ducked_at: None,
        }
    }

    /// Should playback stop NOW? Cancelled and spoken floor commands always
    /// stop (protected spans included); a barge stops a yield-policy span
    /// immediately and a finish-policy span once its bounded extension is
    /// spent.
    fn stop_playback_now(&self, now: std::time::Instant) -> bool {
        if self.cancelled || self.semantic.is_some() {
            return true;
        }
        if !self.barged {
            return false;
        }
        match self.finish_extra {
            None => true,
            Some(budget) => self
                .barged_at
                .map(|at| now.duration_since(at) >= budget)
                .unwrap_or(false),
        }
    }

    /// Drain + echo-cancel whatever the mic delivered, watching for the
    /// caller talking over us (keeps the AEC reference FIFO aligned with
    /// capture and the scratchpad fed). Runs every paced-loop iteration —
    /// listening never stops, even while synthesis is still decoding.
    fn poll_mic(
        &mut self,
        link: &mut dyn AudioLink,
        aec: &mut Option<&mut crate::aec::EchoCanceller>,
        barge_rms: Option<f32>,
        now: std::time::Instant,
    ) {
        if let (Some(a), Some(thr)) = (aec.as_deref_mut(), barge_rms) {
            // Arm only once audio is actually PLAYING (+ the AEC-convergence
            // grace from the first frame). The paced loop polls the mic while
            // synthesis is still warming up — first live test 2026-07-13: a
            // caller's "hello?" into that pre-audio silence tripped a false
            // barge and cancelled the greeting before its first sample, so
            // the call answered into dead air. Pre-audio speech is still
            // CAPTURED (scratchpad); it just never cancels what hasn't begun.
            let armed = !self.first && now.duration_since(self.t_first) >= self.grace;
            let had_speech = self.speech_start.is_some();
            while let Some(samples) = link.try_recv_audio() {
                if detect_barge(
                    a,
                    &samples,
                    self.frame,
                    CAPTURE_RMS,
                    thr,
                    armed,
                    &mut self.speech_frames,
                    self.need,
                    &mut self.captured,
                    &mut self.speech_start,
                ) && !self.barged
                {
                    self.barged = true;
                    self.barged_at = Some(now);
                }
            }
            // Speech that STARTED while this span was audibly playing ducks
            // the output (pre-audio speech never does — nothing to duck).
            if !had_speech && self.speech_start.is_some() && !self.first {
                self.speech_during_playback = true;
            }
            self.trim_idle_capture();
        }
    }

    /// Keep the capture bounded while the caller ISN'T speaking: with no
    /// speech detected yet, only a short pre-roll tail can ever matter, so
    /// trim to the last ~2 s. Once speech started, everything from its
    /// pre-roll onward is retained (bounded by the phrase length).
    fn trim_idle_capture(&mut self) {
        if self.speech_start.is_some() {
            return;
        }
        let keep = (self.sample_rate as usize).saturating_mul(2).max(1);
        if self.captured.len() > keep * 2 {
            self.captured.drain(..self.captured.len() - keep);
        }
    }

    fn push(
        &mut self,
        link: &mut dyn AudioLink,
        aec: &mut Option<&mut crate::aec::EchoCanceller>,
        barge_rms: Option<f32>,
        ctl: &mut Option<&mut ControlProbe<'_>>,
        pcm: &[i16],
        now: std::time::Instant,
    ) -> bool {
        // AOK-CTRL-001: an urgent control (hangup/reject) cuts playback at
        // CHUNK granularity (~20 ms) — the old worst case was a whole sentence.
        if let Some(probe) = ctl.as_deref_mut() {
            if probe.poll() {
                self.cancelled = true;
                return false;
            }
        }
        if pcm.is_empty() {
            return !self.stop_playback_now(now);
        }
        if self.first {
            eprintln!(
                "[aokie-plugin] speaking (first audio in {:?})",
                self.t0.elapsed()
            );
            self.first = false;
            self.t_first = now;
        }
        // The nudge: the caller is talking over this span — duck the output
        // (ramped, click-free) so the bot audibly makes room while the floor
        // decision (finish the clause / yield / barge) plays out. The AEC
        // reference gets the SAME scaled samples that actually play.
        if (self.speech_during_playback && self.ducked_at.is_none()) || self.speech_frames >= 3 {
            self.ducked_at = Some(now);
        }
        let duck_target = match self.ducked_at {
            Some(at) if now.duration_since(at) < std::time::Duration::from_millis(1200) => {
                DUCK_GAIN
            }
            _ => 1.0,
        };
        if self.duck_gain > duck_target {
            self.duck_gain = (self.duck_gain - DUCK_RAMP_STEP).max(duck_target);
        } else if self.duck_gain < duck_target {
            self.duck_gain = (self.duck_gain + DUCK_RAMP_STEP).min(duck_target);
        }
        if self.duck_gain < 0.999 {
            let ducked: Vec<i16> = pcm
                .iter()
                .map(|&s| (s as f32 * self.duck_gain) as i16)
                .collect();
            if let Some(a) = aec.as_deref_mut() {
                a.feed_reference(&ducked);
            }
            link.send_audio(&ducked);
        } else {
            if let Some(a) = aec.as_deref_mut() {
                a.feed_reference(pcm);
            }
            link.send_audio(pcm);
        }
        self.samples += pcm.len();

        // Full-duplex: drain + echo-cancel the mic as we feed (keeps the
        // reference FIFO aligned with capture) and watch for the caller talking
        // over us. The batch is drained fully even after a trip — a finish-
        // policy span keeps playing (and keeps capturing) through its budget.
        self.poll_mic(link, aec, barge_rms, now);
        !self.stop_playback_now(now)
    }

    /// True once everything queued has PLAYED OUT (the paced loop owns the
    /// playout monitor since phase 2).
    fn played_out(&self, now: std::time::Instant) -> bool {
        if self.first {
            return true; // nothing was ever queued
        }
        let playout =
            std::time::Duration::from_secs_f32(self.samples as f32 / self.sample_rate.max(1) as f32);
        now.duration_since(self.t_first) >= playout
    }

    /// Consume the playback into its outcome (phase 2: no monitor loop here —
    /// the paced loop in `tts_speak` already ran playout to completion or cut).
    fn into_outcome(mut self, text: &str, sample_rate: u16) -> SpeakOutcome {
        use std::time::Duration;
        // AK-008: hand back what the caller said while we were talking, from a
        // short pre-roll before their first above-threshold frame. The caller
        // (run_loop) prepends it to the STT buffer so the utterance is
        // complete — detection no longer eats the leading words.
        let captured_speech = match self.speech_start {
            Some(start) => {
                let pre_roll = self.frame * 30; // ~300 ms
                self.captured.split_off(start.saturating_sub(pre_roll))
            }
            None => Vec::new(),
        };

        eprintln!(
            "[aokie-plugin] spoke ({} chars -> {} samples @ {}Hz, span {:?}{}{}{})",
            text.chars().count(),
            self.samples,
            sample_rate,
            self.t0.elapsed(),
            if self.barged { ", BARGED-IN" } else { "" },
            if self.semantic.is_some() { ", SPOKEN COMMAND" } else { "" },
            if captured_speech.is_empty() {
                String::new()
            } else {
                format!(
                    ", captured {}ms of overlapped caller speech",
                    captured_speech.len() * 1000 / (sample_rate.max(1) as usize)
                )
            }
        );
        SpeakOutcome {
            dur: Duration::from_secs_f32(self.samples as f32 / sample_rate.max(1) as f32),
            // A spoken command IS the caller taking the floor.
            barged: self.barged || self.semantic.is_some(),
            captured_speech,
            cancelled: self.cancelled,
            commanded: self.semantic,
            // The paced loop (tts_speak) fills this in — only IT knows whether
            // the loop exited early and how much synthesized PCM it discarded.
            cut_est: None,
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
fn http_tts_chunk_samples(sample_rate: u16) -> usize {
    (sample_rate as usize / 50).max(160)
}

/// AOK-VOICE-001: record the outcome of a live speech attempt in the shared
/// status — audible speech clears the TTS failure slot; a zero-audio outcome
/// (engine load / synthesis / endpoint failure) sets it, so health degrades
/// and auto-answer stops the moment the receptionist demonstrably can't speak.
/// A barged outcome with no audio is inconclusive (the caller cut it off) and
/// leaves the slot unchanged.
#[cfg(all(target_os = "windows", feature = "voice"))]
fn note_tts_outcome(status: &RadioStatus, out: &SpeakOutcome) {
    let mut slot = status.tts_error.lock().unwrap();
    if out.dur > std::time::Duration::ZERO {
        *slot = None;
    } else if !out.barged && !out.cancelled {
        *slot = Some(
            "the last speech attempt produced no audio (TTS engine/endpoint failure) — check the voice models and the plugin log"
                .to_string(),
        );
    }
}

/// §6.3 delivery-truth v1 (alignment fallback #4 — conservative duration-
/// weighted estimation): which PREFIX of a cut span's display text did the
/// caller plausibly hear? The local engines expose no word/phoneme alignment,
/// so the estimate is deliberately conservative — UNDERCLAIM, never overclaim:
/// duration-weighted against the span's exact synthesized total when synthesis
/// finished before the cut, intersected with a chars-per-second ceiling
/// (~14 cps at rate 1.0), then FLOORED to a word boundary (never half a word).
/// The fraction maps display chars, not TTS-normalized chars — digit expansion
/// skews seconds-per-display-char, which the floor + min() absorb for v1.
/// Returns the prefix and whether the boundary is uncertain (true for
/// anything short of a full play). Underclaiming costs only a little natural
/// redundancy in the repair; overclaiming loses information the caller never
/// heard.
#[cfg(all(target_os = "windows", feature = "voice"))]
fn estimate_audible_prefix(text: &str, rate: f32, cut: &CutEstimate) -> (String, bool) {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || cut.audible_ms == 0 {
        return (String::new(), true);
    }
    if let Some(total) = cut.synthesized_ms {
        if cut.audible_ms >= total {
            // The cut landed after everything queued had played out — the
            // whole span was plausibly heard.
            return (text.to_string(), false);
        }
    }
    let cps = 14.0f32 * rate.max(0.1);
    let by_cps = (cut.audible_ms as f32 / 1000.0 * cps) as usize;
    let est = match cut.synthesized_ms {
        Some(total) if total > 0 => {
            let frac = (cut.audible_ms as f32 / total as f32).min(1.0);
            ((frac * chars.len() as f32) as usize).min(by_cps)
        }
        _ => by_cps,
    };
    if est >= chars.len() {
        return (text.to_string(), true);
    }
    // Floor to the previous word boundary — never claim half a word.
    let prefix: String = chars[..est].iter().collect();
    let cut_at = prefix.rfind(char::is_whitespace).unwrap_or(0);
    (prefix[..cut_at].trim_end().to_string(), true)
}

/// How far ahead of real playout the SCO TX queue is kept topped up (phase 2).
/// Small enough that a cancel/flush silences the line almost immediately;
/// large enough to ride out pump-iteration jitter without underruns.
#[cfg(all(target_os = "windows", feature = "voice"))]
const PLAYOUT_LEAD: std::time::Duration = std::time::Duration::from_millis(200);

/// Pure pacing decision: may another chunk be queued yet? The first chunk
/// always may (it starts the playout clock); after that the queued total may
/// lead the playout clock by at most [`PLAYOUT_LEAD`].
#[cfg(all(target_os = "windows", feature = "voice"))]
fn may_queue_more(
    first: bool,
    queued_samples: usize,
    sample_rate: u16,
    elapsed_since_first: std::time::Duration,
    lead: std::time::Duration,
) -> bool {
    if first {
        return true;
    }
    let queued =
        std::time::Duration::from_secs_f32(queued_samples as f32 / sample_rate.max(1) as f32);
    queued < elapsed_since_first + lead
}

/// The priority STT control lane (phase 2 + plan §3.2): while Aokie speaks,
/// SNAPSHOT the caller's overlap capture every ~500 ms and have the STT
/// worker transcribe it on the dedicated probe channel. A probe that parses
/// to a hard floor command ("wait"/"stop") cuts playback MID-SENTENCE — even
/// through a protected span. Probes never touch the content buffer; the full
/// utterance still arrives at its normal endpoint.
#[cfg(all(target_os = "windows", feature = "voice"))]
struct SttProbeLane<'a> {
    stt_tx: &'a std::sync::mpsc::Sender<SttWork>,
    results: &'a std::sync::mpsc::Receiver<SttResult>,
    status: &'a RadioStatus,
    generation: u64,
    last_probe_at: Option<std::time::Instant>,
    probed_len: usize,
    in_flight: u32,
    /// The live scratchpad TEXT: the latest content partial heard over the
    /// bot's speech. Read at sentence boundaries so the reply can yield to
    /// substantive caller speech instead of finishing a stale paragraph.
    content: String,
    pending_command: Option<crate::duplex::CallerIntent>,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
impl<'a> SttProbeLane<'a> {
    fn new(
        stt_tx: &'a std::sync::mpsc::Sender<SttWork>,
        results: &'a std::sync::mpsc::Receiver<SttResult>,
        generation: u64,
        status: &'a RadioStatus,
    ) -> Self {
        // Discard results from a PREVIOUS span: a "wait" heard over sentence
        // 3 must not cut sentence 4 seconds later — probes are instant-or-
        // nothing; the turn-level grammar still catches the command.
        while results.try_recv().is_ok() {}
        Self {
            stt_tx,
            results,
            status,
            generation,
            last_probe_at: None,
            probed_len: 0,
            in_flight: 0,
            content: String::new(),
            pending_command: None,
        }
    }

    /// Ship a probe when speech has been running long enough and enough NEW
    /// audio arrived since the last one.
    fn maybe_probe(&mut self, playback: &TtsChunkPlayback) {
        let Some(start) = playback.speech_start else {
            return;
        };
        let sr = playback.sample_rate.max(1) as usize;
        let since_speech = playback.captured.len().saturating_sub(start);
        if since_speech < (sr * 2) / 5 {
            return; // <400 ms of speech so far
        }
        if let Some(at) = self.last_probe_at {
            if at.elapsed() < std::time::Duration::from_millis(700) {
                return;
            }
        }
        // At most ONE probe in flight (live regression 2026-07-13: probes
        // queueing ahead of the caller's REAL utterance on the serial STT
        // worker delayed every reply). A lost result unblocks after 1.5 s
        // via the last_probe_at gate above falling through here.
        if self.in_flight > 0 {
            match self.last_probe_at {
                Some(at) if at.elapsed() >= std::time::Duration::from_millis(1500) => {
                    self.in_flight = 0;
                }
                _ => return,
            }
        }
        if playback.captured.len() <= self.probed_len {
            return; // nothing new since the last probe
        }
        // Last ~1.2 s window, from a little before speech onset.
        let window = sr + sr / 5;
        let from = start
            .saturating_sub(playback.frame * 30)
            .max(playback.captured.len().saturating_sub(window));
        let snapshot = crate::voice::to_f32_16k(
            &playback.captured[from..],
            playback.sample_rate as u32,
        );
        self.last_probe_at = Some(std::time::Instant::now());
        self.probed_len = playback.captured.len();
        self.in_flight += 1;
        self.status.probes_sent.fetch_add(1, Ordering::Relaxed);
        let _ = self.stt_tx.send(SttWork::Probe {
            generation: self.generation,
            samples: snapshot,
        });
    }

    /// Fold newly-arrived probe transcripts into the lane: hard floor
    /// commands park in `pending_command`; everything else becomes the
    /// current scratchpad content (latest, longest partial wins).
    fn drain(&mut self) {
        while let Ok(res) = self.results.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            if res.generation != self.generation {
                continue;
            }
            let intent = crate::duplex::parse_caller_intent(&res.text);
            if matches!(
                intent,
                crate::duplex::CallerIntent::Pause | crate::duplex::CallerIntent::StopSpeaking
            ) {
                eprintln!(
                    "[aokie-plugin] probe caught a spoken floor command ({intent:?}): {}",
                    content_for_log(&res.text)
                );
                self.status.probe_commands.fetch_add(1, Ordering::Relaxed);
                self.pending_command = Some(intent);
            } else if res.text.trim().len() > self.content.trim().len() {
                self.content = res.text;
            }
        }
    }

    /// A probe transcript that parses to a hard floor command.
    fn check(&mut self) -> Option<crate::duplex::CallerIntent> {
        self.drain();
        self.pending_command.take()
    }

    /// Sentence-boundary steering: SUBSTANTIVE caller speech on the
    /// scratchpad (≥3 words, not a backchannel, not the bot's own echo)
    /// means the reply should yield here and respond to it.
    fn substantive_content(&mut self, bot_text_so_far: &str) -> Option<String> {
        self.drain();
        let text = self.content.trim();
        if text.is_empty() {
            return None;
        }
        let words = text.split_whitespace().count();
        if words < 3 || crate::duplex::is_backchannel(text) {
            return None;
        }
        if looks_like_echo(text, bot_text_so_far) {
            return None;
        }
        Some(text.to_string())
    }
}

/// Voice build only: speak `text` through the phase-2 paced engine. Synthesis
/// runs on the synth WORKER (crate::synth); this loop — on the radio thread —
/// paces the produced PCM into the SCO TX queue no more than
/// [`PLAYOUT_LEAD`] ahead of real playout while continuously draining the mic
/// (AEC + barge + scratchpad capture) and, when a probe lane is provided,
/// shipping overlap snapshots to the STT worker so a spoken "wait"/"stop"
/// cuts playback mid-sentence. With `aec`/`barge_rms` `None` it's the plain
/// half-duplex path (caller relies on the mute), still gaining the ~200 ms
/// cancel latency. No-op with no SCO channel (sample_rate 0) or empty text.
///
/// `rate` is the span's speaking-speed multiplier (1.0 = normal; WSOLA on the
/// worker). `finish_extra` is the span's interrupt policy.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
fn tts_speak(
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    synth: &crate::synth::SynthHandle,
    text: &str,
    sample_rate: u16,
    mut aec: Option<&mut crate::aec::EchoCanceller>,
    barge_rms: Option<f32>,
    mut ctl: Option<&mut ControlProbe<'_>>,
    rate: f32,
    finish_extra: Option<std::time::Duration>,
    mut probe: Option<&mut SttProbeLane<'_>>,
) -> SpeakOutcome {
    use std::time::Duration;
    let none = SpeakOutcome {
        dur: Duration::ZERO,
        barged: false,
        captured_speech: Vec::new(),
        cancelled: false,
        commanded: None,
        cut_est: None,
    };
    if sample_rate == 0 || text.trim().is_empty() {
        return none;
    }
    // Speech-normalize ONCE at the chokepoint (greeting, agent sentences and
    // operatorSpeak all funnel through here): "10 a.m.," → "10 AM," — dotted
    // abbreviations against punctuation make the TTS stutter audibly.
    let text = &crate::speech_wire::normalize_speech_text(text);
    // Voice from AOKIE_TTS_VOICE, shared by HTTP and in-process synthesis.
    let voice = std::env::var("AOKIE_TTS_VOICE").unwrap_or_default();
    let epoch = synth.begin(text, &voice, rate, sample_rate as u32);

    let mut playback = TtsChunkPlayback::new(sample_rate, finish_extra);
    let mut pending: std::collections::VecDeque<i16> = std::collections::VecDeque::new();
    let mut synth_done = false;
    let mut synth_err: Option<String> = None;
    let chunk = http_tts_chunk_samples(sample_rate); // ~20 ms
    let mut stopped = false;
    // §6.3 delivery truth: did playback run to completion, or was it cut?
    // Only the CUT case needs an audible-prefix estimate.
    let mut natural_end = false;
    loop {
        let now = std::time::Instant::now();
        // Urgent controls cut even while we're idling between frames.
        if let Some(p) = ctl.as_deref_mut() {
            if p.poll() {
                playback.cancelled = true;
            }
        }
        if playback.cancelled {
            break;
        }
        // Collect whatever the worker produced (non-blocking).
        loop {
            match synth.out_rx.try_recv() {
                Ok(crate::synth::SynthOut::Frames { epoch: e, pcm }) if e == epoch => {
                    pending.extend(pcm);
                }
                Ok(crate::synth::SynthOut::Done { epoch: e }) if e == epoch => {
                    synth_done = true;
                }
                Ok(crate::synth::SynthOut::Failed { epoch: e, error }) if e == epoch => {
                    synth_done = true;
                    synth_err = Some(error);
                }
                Ok(_) => {} // stale epoch — discard
                Err(_) => break,
            }
        }
        // Top up the SCO TX queue, bounded to the playout lead.
        while !pending.is_empty()
            && may_queue_more(
                playback.first,
                playback.samples,
                sample_rate,
                if playback.first {
                    Duration::ZERO
                } else {
                    now.duration_since(playback.t_first)
                },
                PLAYOUT_LEAD,
            )
        {
            let n = chunk.min(pending.len());
            let frame: Vec<i16> = pending.drain(..n).collect();
            if !playback.push(bt, &mut aec, barge_rms, &mut ctl, &frame, now) {
                stopped = true;
                break;
            }
        }
        if stopped {
            break;
        }
        // Listening never stops: drain the mic even with nothing to queue.
        playback.poll_mic(bt, &mut aec, barge_rms, now);
        // Priority control lane: snapshot overlap speech + act on a spoken
        // floor command mid-sentence (beats protected spans by design).
        if let Some(lane) = probe.as_deref_mut() {
            lane.maybe_probe(&playback);
            if let Some(intent) = lane.check() {
                playback.semantic = Some(intent);
            }
        }
        if playback.stop_playback_now(now) {
            break;
        }
        // Natural end: synthesis finished, everything queued, playout done.
        if synth_done && pending.is_empty() && playback.played_out(now) {
            natural_end = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // A cut span leaves the worker mid-stream: invalidate its epoch so it
    // aborts at the next chunk instead of synthesizing into the void.
    if playback.cancelled || playback.barged || playback.semantic.is_some() || !synth_done {
        synth.cancel();
    }
    if let Some(e) = synth_err.as_ref() {
        if playback.samples == 0 {
            eprintln!("[aokie-plugin] TTS synthesis failed: {e}");
        }
    }
    // §6.3 delivery truth: a cut span records the numbers needed to estimate
    // its audible prefix — what was queued, how long audio flowed, and (when
    // synthesis finished first) the exact synthesized total.
    let cut_est = if !natural_end && !playback.first && playback.samples > 0 {
        let sr = sample_rate.max(1) as u64;
        let queued_ms = playback.samples as u64 * 1000 / sr;
        let audible_ms = (playback.t_first.elapsed().as_millis() as u64).min(queued_ms);
        let synthesized_ms = if synth_done && synth_err.is_none() {
            Some(queued_ms + pending.len() as u64 * 1000 / sr)
        } else {
            None
        };
        Some(CutEstimate {
            audible_ms,
            queued_ms,
            synthesized_ms,
        })
    } else {
        None
    };
    let mut out = playback.into_outcome(text, sample_rate);
    out.cut_est = cut_est;
    out
}

/// The outcome of speaking one PLANNED utterance (a sequence of validated
/// [`crate::speech_plan::SpeechSpan`]s): the aggregated playback outcome,
/// the marker-free text of the whole plan, and the marker-free text of the
/// spans that actually PLAYED (what transcripts/history may record).
#[cfg(all(target_os = "windows", feature = "voice"))]
struct PlannedSpeech {
    outcome: SpeakOutcome,
    /// Everything the plan intended to say (markers stripped).
    text: String,
    /// §6.3 estimated-AUDIBLE text (markers stripped): full spans that played
    /// to their natural end, plus a CONSERVATIVE word-floored prefix of the
    /// cut span. What transcripts, history and the nudge may claim was heard.
    played_text: String,
    /// §6.3 text SENT toward the phone (markers stripped): the full text of
    /// every span that produced audio, cut span included — the comparator for
    /// the echo guard and deterministic replay, where the flushed-but-echoed
    /// tail must still match.
    sent_text: String,
}

/// Speak `raw_text` through the span planner: strips/validates any control
/// markup, slows + digit-expands phone-number/code runs, applies per-span
/// interrupt policy, and plays the spans in order. ONE pipeline for every
/// speech origin — the built-in agent, the greeting, flow/operator speech —
/// so pacing and duplex behaviour never depend on where words came from.
///
/// Stops early on a barge (after the barged span finishes its bounded
/// extension, if any) or an urgent control; the remaining spans are never
/// spoken (yield: the caller has the floor).
#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
fn speak_planned(
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    synth: &crate::synth::SynthHandle,
    raw_text: &str,
    sample_rate: u16,
    mut aec: Option<&mut crate::aec::EchoCanceller>,
    barge_rms: Option<f32>,
    mut ctl: Option<&mut ControlProbe<'_>>,
    pace: &crate::speech_plan::PaceState,
    protected_max_ms: u32,
    mut probe: Option<&mut SttProbeLane<'_>>,
) -> PlannedSpeech {
    use std::time::Duration;
    let spans = crate::speech_plan::plan_spans(raw_text, pace, protected_max_ms);
    let text = crate::speech_plan::clean_text(&spans);
    let mut outcome = SpeakOutcome {
        dur: Duration::ZERO,
        barged: false,
        captured_speech: Vec::new(),
        cancelled: false,
        commanded: None,
        cut_est: None,
    };
    let mut played: Vec<String> = Vec::new();
    let mut sent: Vec<String> = Vec::new();
    for span in &spans {
        let finish_extra = match span.policy {
            crate::speech_plan::InterruptPolicy::Yield => None,
            crate::speech_plan::InterruptPolicy::FinishSpan { max_extra_ms } => {
                Some(Duration::from_millis(max_extra_ms as u64))
            }
        };
        let out = tts_speak(
            bt,
            synth,
            &span.tts_text,
            sample_rate,
            aec.as_deref_mut(),
            barge_rms,
            ctl.as_deref_mut(),
            span.rate,
            finish_extra,
            probe.as_deref_mut(),
        );
        outcome.dur += out.dur;
        if out.dur > Duration::ZERO {
            sent.push(span.text.clone());
            match &out.cut_est {
                // Cut mid-span: claim only the conservative audible prefix
                // (§6.3) — overclaiming loses whatever the caller never heard
                // from the transcript, the history AND the nudge tail.
                Some(cut) => {
                    let (prefix, _uncertain) =
                        estimate_audible_prefix(&span.text, span.rate, cut);
                    if !prefix.is_empty() {
                        played.push(prefix);
                    }
                }
                None => played.push(span.text.clone()),
            }
        }
        if !out.captured_speech.is_empty() {
            outcome.captured_speech.extend_from_slice(&out.captured_speech);
        }
        if out.commanded.is_some() {
            outcome.commanded = out.commanded;
        }
        if out.cancelled {
            outcome.cancelled = true;
            break;
        }
        if out.barged {
            // The span itself honoured its policy (yield or bounded finish);
            // everything AFTER it always yields — the caller has the floor.
            outcome.barged = true;
            break;
        }
    }
    PlannedSpeech {
        outcome,
        text,
        played_text: played.join(" "),
        sent_text: sent.join(" "),
    }
}

/// Execute an urgent control the [`ControlProbe`] caught mid-playback: note
/// the termination intent (so `call.ended` reads the right outcome), flush
/// the queued audio tail, and act on the phone. A failed radio action emits
/// the authoritative `control_failed` diagnostic against the operation id.
#[cfg(all(target_os = "windows", feature = "voice"))]
fn perform_cancel_action(
    action: CancelAction,
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    bt.flush_tx_audio();
    match action {
        CancelAction::Hangup { op } => {
            tracker.note_intent(crate::call_session::TerminationIntent::OperatorHangup);
            if let Err(e) = bt.hangup() {
                eprintln!("[aokie-plugin] mid-playback hangup failed: {e}");
                emit_control_failed(outbox, sink, tracker, "call.hangup", op.as_deref(), &e);
            }
        }
        CancelAction::Reject { op } => {
            tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
            if let Err(e) = bt.reject_call() {
                eprintln!("[aokie-plugin] mid-playback reject failed: {e}");
                emit_control_failed(outbox, sink, tracker, "call.reject", op.as_deref(), &e);
            }
        }
    }
}

/// The radio poll loop: drain events â†’ map+emit; buffer the incoming-call
/// emission until the caller id lands (or a short timeout); drain audio
/// (Stage 2 feeds the AI here); service control requests. Runs until the
/// control channel closes or a Shutdown is received.
#[cfg(target_os = "windows")]
// `greeting` is only mutated (via RadioControl::Configure) in the voice build.
#[cfg_attr(not(feature = "voice"), allow(unused_mut))]
fn run_loop(
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: std::sync::mpsc::Receiver<RadioControl>,
    status: Arc<RadioStatus>,
    auto_answer: bool,
    answer_tone: bool,
    mut greeting: Option<String>,
) {
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;
    #[cfg(feature = "voice")]
    use std::time::Instant;

    // Phase 2: synthesis runs on the synth WORKER (in-process engine loaded
    // lazily there on the first thing Aokie says; the HTTP TTS endpoint +
    // its sticky per-call fallback live there too). The radio thread paces
    // the worker's PCM into SCO ~200 ms ahead of playout — see tts_speak.
    #[cfg(feature = "voice")]
    let synth = crate::synth::SynthHandle::spawn();
    let _ = &greeting; // used only in the voice build / greeting block below

    // AOK-VOICE-001: fast asset preflight (presence-only — radio start stays
    // instant) seeds the shared voice-status slots BEFORE any call can arrive:
    // a deleted model / missing ONNX Runtime DLL (with no HTTP endpoint
    // substituting) is a KNOWN failure that degrades plugin.health and blocks
    // auto-answer below, instead of being discovered mid-call by answering a
    // caller into silence. Corruption is caught by the live engine loads,
    // which update the same slots.
    #[cfg(feature = "voice")]
    {
        let pf = crate::voice::preflight_assets();
        if let Some(e) = &pf.stt_error {
            eprintln!("[aokie-plugin] voice preflight: {e}");
        }
        if let Some(e) = &pf.tts_error {
            eprintln!("[aokie-plugin] voice preflight: {e}");
        }
        if pf.stt_error.is_none() && pf.tts_error.is_none() {
            eprintln!("[aokie-plugin] voice preflight OK (ORT + STT/TTS assets or endpoints present)");
        }
        let preflight_failed = pf.stt_error.is_some() || pf.tts_error.is_some();
        *status.stt_error.lock().unwrap() = pf.stt_error;
        *status.tts_error.lock().unwrap() = pf.tts_error;

        // VOICE-001: the measured loopback self-test — EXERCISE the engines
        // (TTS→STT round trip of a known phrase) before auto-answer may arm,
        // so a corrupt model / broken provider / silent synthesis is caught at
        // startup, not by the first caller. Runs on its own thread (the ONNX
        // loads are heavy); auto-answer stays blocked until a report lands.
        // Skip cases write an OK report with the reason so arming isn't held
        // hostage: HTTP endpoints replace the local engines this test covers,
        // and a failed preflight already blocks via its own slots.
        let skip_reason: Option<String> =
            if std::env::var("AOKIE_SKIP_SELF_TEST").as_deref() == Ok("1") {
                Some("skipped (AOKIE_SKIP_SELF_TEST=1)".to_string())
            } else if std::env::var("AOKIE_STT_ENDPOINT").is_ok_and(|v| !v.trim().is_empty())
                || std::env::var("AOKIE_TTS_ENDPOINT").is_ok_and(|v| !v.trim().is_empty())
            {
                Some(
                    "skipped: HTTP speech endpoint(s) configured - the local-engine loopback does not cover them"
                        .to_string(),
                )
            } else if preflight_failed {
                Some("skipped: asset preflight already failed (see stt/tts errors)".to_string())
            } else {
                None
            };
        match skip_reason {
            Some(reason) => {
                eprintln!("[aokie-plugin] voice self-test {reason}");
                *status.self_test.lock().unwrap() = Some(VoiceSelfTest {
                    ok: true,
                    at: aokie_core::events::now_iso8601(),
                    duration_ms: 0,
                    detail: reason,
                });
            }
            None => {
                let status_st = status.clone();
                let spawned = std::thread::Builder::new()
                    .name("aokie-voice-selftest".to_string())
                    .spawn(move || {
                        let started = std::time::Instant::now();
                        // A panic inside the ONNX stack must still produce a
                        // report — an empty slot blocks auto-answer forever.
                        let outcome = std::panic::catch_unwind(
                            crate::voice::run_loopback_self_test,
                        )
                        .unwrap_or_else(|_| {
                            Err("self-test panicked inside the speech stack".to_string())
                        });
                        let report = match outcome {
                            Ok(heard) => VoiceSelfTest {
                                ok: true,
                                at: aokie_core::events::now_iso8601(),
                                duration_ms: started.elapsed().as_millis() as u64,
                                detail: format!("loopback ok - heard {heard:?}"),
                            },
                            Err(e) => VoiceSelfTest {
                                ok: false,
                                at: aokie_core::events::now_iso8601(),
                                duration_ms: started.elapsed().as_millis() as u64,
                                detail: e,
                            },
                        };
                        eprintln!(
                            "[aokie-plugin] voice self-test {} in {}ms — {}",
                            if report.ok { "PASSED" } else { "FAILED (auto-answer blocked)" },
                            report.duration_ms,
                            report.detail
                        );
                        *status_st.self_test.lock().unwrap() = Some(report);
                    });
                if let Err(e) = spawned {
                    // Can't run it — never leave the slot empty (permanent block).
                    eprintln!("[aokie-plugin] voice self-test thread failed to start: {e}");
                    *status.self_test.lock().unwrap() = Some(VoiceSelfTest {
                        ok: false,
                        at: aokie_core::events::now_iso8601(),
                        duration_ms: 0,
                        detail: format!("self-test thread failed to start: {e}"),
                    });
                }
            }
        }
    }
    // Non-voice builds never run the loopback: record the skip so any reader
    // (health) sees a settled state instead of "still running" forever.
    #[cfg(not(feature = "voice"))]
    {
        *status.self_test.lock().unwrap() = Some(VoiceSelfTest {
            ok: true,
            at: aokie_core::events::now_iso8601(),
            duration_ms: 0,
            detail: "skipped: no voice output compiled".to_string(),
        });
    }
    // Warn-once bookkeeping for the auto-answer voice block (per call id).
    let mut voice_block_logged_call: Option<String> = None;

    // â”€â”€ Speech-to-text (voice build) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // The caller's audio is transcribed OFF the radio loop: a worker thread owns
    // the heavy Parakeet engine (lazy-loaded on the first utterance) so a ~300 ms
    // transcription never stalls SCO I/O or control handling. The loop segments
    // utterances with a simple energy VAD and ships each finished one to the
    // worker; finished transcripts come back and become `aokie.call.turn.final`
    // events â€” the hook a flow binds to drive the conversation.
    // The CURRENT call generation, shared with the worker: jobs stamped with
    // any other generation are for a finished call — the worker skips them
    // WITHOUT transcribing (cheap cancellation on hangup; audit AK-002).
    #[cfg(feature = "voice")]
    let stt_current_gen = Arc::new(AtomicU64::new(0));
    #[cfg(feature = "voice")]
    let (stt_tx, stt_result_rx, probe_result_rx) = {
        let (utter_tx, utter_rx) = std::sync::mpsc::channel::<SttWork>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<SttResult>();
        // Dedicated probe-result channel: control probes must never be
        // confused with (or consume) final-utterance results.
        let (probe_tx, probe_rx) = std::sync::mpsc::channel::<SttResult>();
        let initial_stt_endpoint = std::env::var("AOKIE_STT_ENDPOINT").ok();
        let worker_gen = stt_current_gen.clone();
        let worker_status = status.clone();
        std::thread::Builder::new()
            .name("aokie-stt".into())
            .spawn(move || {
                // CONSENT-001: the operator denied the `transcription` scope —
                // NO caller audio may reach any STT engine (in-process or HTTP).
                // Set by the connector before radio start from the consent gate;
                // frames are dropped here at the last hop before an engine.
                let stt_disabled = std::env::var("AOKIE_STT_DISABLED")
                    .map(|v| v == "1")
                    .unwrap_or(false);
                if stt_disabled {
                    eprintln!(
                        "[aokie-plugin] transcription consent DENIED — STT worker will drop all audio"
                    );
                }
                let mut engine: Option<crate::voice::SttEngine> = None;
                let mut http_stt = HttpSpeechFallback::new(initial_stt_endpoint);
                while let Ok(work) = utter_rx.recv() {
                    // Control probes first: local engine only (no HTTP egress),
                    // answered on the probe channel, generation-gated like
                    // everything else. Best-effort — a failed probe is silence,
                    // never an error path.
                    if let SttWork::Probe { generation, samples } = &work {
                        if stt_disabled || *generation != worker_gen.load(Ordering::Relaxed) {
                            continue;
                        }
                        if engine.is_none() {
                            match crate::voice::SttEngine::load() {
                                Ok(e) => {
                                    eprintln!("[aokie-plugin] STT engine loaded");
                                    *worker_status.stt_error.lock().unwrap() = None;
                                    engine = Some(e);
                                }
                                Err(e) => {
                                    eprintln!("[aokie-plugin] STT load failed (probe): {e}");
                                    *worker_status.stt_error.lock().unwrap() =
                                        Some(format!("the STT engine failed to load: {e}"));
                                    continue;
                                }
                            }
                        }
                        if let Some(eng) = engine.as_mut() {
                            match eng.transcribe(samples) {
                                Ok(text) if !text.is_empty() => {
                                    let _ = probe_tx.send(SttResult {
                                        generation: *generation,
                                        utterance: 0,
                                        text,
                                    });
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    eprintln!("[aokie-plugin] STT probe transcribe failed: {e}")
                                }
                            }
                        }
                        continue;
                    }
                    let (generation, utterance, buf) = match work {
                        SttWork::Utterance { .. } if stt_disabled => continue,
                        SttWork::Utterance {
                            generation,
                            utterance,
                            samples,
                        } => (generation, utterance, samples),
                        SttWork::Probe { .. } => continue, // handled above
                        SttWork::Configure { endpoint } => {
                            http_stt.configure(endpoint);
                            continue;
                        }
                        SttWork::ResetCall => {
                            http_stt.reset_call();
                            continue;
                        }
                    };
                    // Stale-job gate: the call this audio belongs to is over
                    // (or a new one replaced it) — do not spend a transcription
                    // on it, and never emit its text.
                    if generation != worker_gen.load(Ordering::Relaxed) {
                        eprintln!(
                            "[aokie-plugin] skipped stale STT job (call gen {generation}, utterance {utterance})"
                        );
                        continue;
                    }
                    let send = |text: String| {
                        let _ = res_tx.send(SttResult {
                            generation,
                            utterance,
                            text,
                        });
                    };
                    if let Some(endpoint) = http_stt.endpoint_for_call().map(str::to_string) {
                        // Hardened per-endpoint client (AOK-ENDPOINT-001); a rejected
                        // endpoint takes the same sticky fallback path as a failed request.
                        match http_speech_client(&endpoint)
                            .and_then(|client| http_stt_transcribe(&client, &endpoint, &buf))
                        {
                            Ok(text) if !text.is_empty() => {
                                send(text);
                                continue;
                            }
                            Ok(_) => continue,
                            Err(e) => {
                                if http_stt.mark_failed_for_call() {
                                    eprintln!(
                                        "[aokie-plugin] HTTP STT failed at {endpoint}: {e}; falling back to in-process STT for this call"
                                    );
                                }
                            }
                        }
                    }
                    if engine.is_none() {
                        match crate::voice::SttEngine::load() {
                            Ok(e) => {
                                eprintln!("[aokie-plugin] STT engine loaded");
                                // AOK-VOICE-001: a working load clears any
                                // preflight/previous failure for this half.
                                *worker_status.stt_error.lock().unwrap() = None;
                                engine = Some(e);
                            }
                            Err(e) => {
                                eprintln!("[aokie-plugin] STT load failed: {e}");
                                // AOK-VOICE-001: a KNOWN hearing failure —
                                // degrade health + block auto-answer.
                                *worker_status.stt_error.lock().unwrap() = Some(format!(
                                    "the STT engine failed to load: {e}"
                                ));
                                continue;
                            }
                        }
                    }
                    if let Some(eng) = engine.as_mut() {
                        match eng.transcribe(&buf) {
                            Ok(text) if !text.is_empty() => send(text),
                            Ok(_) => {}
                            Err(e) => eprintln!("[aokie-plugin] STT transcribe failed: {e}"),
                        }
                    }
                }
            })
            .ok();
        (utter_tx, res_rx, probe_rx)
    };
    // VAD / utterance accumulator (all in 16 kHz mono f32, the STT engine's rate).
    #[cfg(feature = "voice")]
    let mut stt_buf: Vec<f32> = Vec::new();
    // Utterances sent to the STT worker whose results haven't come back yet
    // (audit AOK-LIF-002): the termination drain waits for these — bounded —
    // so the caller's last words land BEFORE call.ended.
    #[cfg(feature = "voice")]
    let mut stt_outstanding: usize = 0;
    // Early-start STT (round 3): the utterance is transcribed SPECULATIVELY
    // after a SHORT silence so its text is ready AT the endpoint instead of
    // an STT-latency after it. Resumed speech invalidates the speculation
    // (the grown utterance re-transcribes whole; the stale result is dropped
    // by id, never merged).
    #[cfg(feature = "voice")]
    let mut spec_utterance: Option<u32> = None;
    #[cfg(feature = "voice")]
    let mut stale_specs: Vec<u32> = Vec::new();
    #[cfg(feature = "voice")]
    let mut stt_had_speech = false;
    #[cfg(feature = "voice")]
    let mut stt_silence = std::time::Duration::ZERO;
    // End-of-utterance silence: how long the caller must pause before we treat
    // their turn as finished and transcribe. Lower = snappier replies but risks
    // cutting off mid-sentence pauses. Tunable via AOKIE_STT_ENDPOINT_MS (set from
    // the `sttEndpointMs` connector setting); default 450 ms.
    // §3.11 bounds reconciliation: honour exactly the range `settings.set`
    // accepts (shared consts) — an accepted value must never silently fall
    // back to the default.
    #[cfg(feature = "voice")]
    let stt_endpoint = std::time::Duration::from_millis(
        std::env::var("AOKIE_STT_ENDPOINT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&m| {
                (crate::connector::STT_ENDPOINT_MS_MIN..=crate::connector::STT_ENDPOINT_MS_MAX)
                    .contains(&m)
            })
            .unwrap_or(450),
    );

    // â”€â”€ In-plugin real-time voice agent â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // When AOKIE_AI_RECEPTIONIST is set (from the `aiReceptionist` setting), the
    // plugin answers the caller ITSELF â€” streaming the local LLM (reused from the
    // desktop's llama.cpp/ollama) and speaking each sentence as it's generated â€”
    // instead of routing through a flow. Far lower latency. The pack's live-reply
    // flow binding must be disabled so the caller isn't answered twice.
    #[cfg(feature = "voice")]
    let agent_enabled = std::env::var_os("AOKIE_AI_RECEPTIONIST").is_some();
    // AOK-CTRL-001: publish the RUNNING radio's responder ownership so the
    // connector can refuse operatorSpeak truthfully (the radio would drop it).
    #[cfg(feature = "voice")]
    status
        .agent_enabled
        .store(agent_enabled, Ordering::Relaxed);
    // Shared with the LLM readiness probe thread (PROC-001): Configure updates
    // land here so the probe always checks the CURRENT endpoint setting.
    #[cfg(feature = "voice")]
    let agent_endpoint: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(
        std::env::var("AOKIE_AI_ENDPOINT")
            .ok()
            .filter(|s| !s.trim().is_empty()),
    ));
    // PROC-001: background LLM readiness probe. Runs ONLY while the in-plugin
    // agent owns replies: re-resolves the endpoint every ~30s and records the
    // outcome in status.llm_error, so a dead/unloaded LLM shows up in
    // plugin.health (and blocks auto-answer below) BEFORE a caller finds out.
    // The keep-alive sender lives in this scope — when the radio loop returns,
    // it drops, the probe's recv_timeout disconnects, and the thread exits.
    #[cfg(feature = "voice")]
    let _llm_probe_stop_tx: Option<std::sync::mpsc::Sender<()>> = if agent_enabled {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let status_probe = status.clone();
        let endpoint_probe = agent_endpoint.clone();
        let spawned = std::thread::Builder::new()
            .name("aokie-llm-probe".to_string())
            .spawn(move || {
                loop {
                    let configured = endpoint_probe.lock().unwrap().clone();
                    let reachable = crate::agent::discover_endpoint(configured.as_deref());
                    let new_error = match reachable {
                        Some(_) => None,
                        None => Some(match &configured {
                            Some(ep) => format!("LLM endpoint {ep} is not answering"),
                            None => "no reachable LLM (tried llama.cpp :8080 and ollama :11434)"
                                .to_string(),
                        }),
                    };
                    {
                        let mut slot = status_probe.llm_error.lock().unwrap();
                        if *slot != new_error {
                            match &new_error {
                                Some(e) => eprintln!(
                                    "[aokie-plugin] LLM readiness: DOWN — {e}; auto-answer is blocked until it recovers"
                                ),
                                None => eprintln!("[aokie-plugin] LLM readiness: ok"),
                            }
                            *slot = new_error;
                        }
                    }
                    match stop_rx.recv_timeout(Duration::from_secs(30)) {
                        // A message or a dropped sender both mean the radio is done.
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            });
        match spawned {
            Ok(_) => Some(stop_tx),
            Err(e) => {
                eprintln!("[aokie-plugin] LLM readiness probe thread failed to start: {e}");
                None
            }
        }
    } else {
        None
    };
    // System prompt / script (AOKIE_AI_PERSONA from the `persona` setting, or a flow
    // push). Editable live via RadioControl::Configure. The default is a real
    // receptionist SCRIPT â€” greet, get the caller's name + reason, capture details,
    // book or take a message â€” not just a chat style, so it actively drives the call.
    #[cfg(feature = "voice")]
    let mut agent_persona = std::env::var("AOKIE_AI_PERSONA")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT_PERSONA.to_string());
    // LLM model for the agent (AOKIE_AI_MODEL from `aiModel`, or a flow push). Empty
    // = auto-detect whatever the desktop's running LLM has loaded.
    #[cfg(feature = "voice")]
    let mut agent_model = std::env::var("AOKIE_AI_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    #[cfg(feature = "voice")]
    let mut agent_client: Option<crate::agent::LlmClient> = None;
    // §9.3 call-scoped agent overlay — see [`CallAgentOverlay`].
    #[cfg(feature = "voice")]
    let mut call_agent_overlay: Option<CallAgentOverlay> = None;
    // Conversation history for the agent (OpenAI chat messages), reset per call.
    #[cfg(feature = "voice")]
    let mut history: Vec<serde_json::Value> = Vec::new();
    // Caller turn held open across STT utterances (audit AK-008 — see
    // PendingTurn): replies wait until the turn stops looking unfinished.
    #[cfg(feature = "voice")]
    let mut pending_turn: Option<PendingTurn> = None;
    // Controls that arrived DURING an agent reply (audit AK-003): the
    // mid-reply poll acts on Hangup/Reject instantly and parks everything
    // else here; the main control loop drains this before its channel.
    let mut pending_controls: std::collections::VecDeque<RadioControl> = Default::default();
    // Aokie's last spoken line (greeting or reply) â€” for the self-echo guard.
    #[cfg(feature = "voice")]
    let mut last_bot_reply = String::new();
    // Half-duplex gate: while Aokie is speaking (+ a short tail) inbound audio is
    // discarded so we never transcribe our own TTS echoing back over the line.
    #[cfg(feature = "voice")]
    let mut mute_stt_until: Option<std::time::Instant> = None;
    // â”€â”€ Full-duplex / barge-in â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    // When AOKIE_BARGE_IN is set (the `bargeIn` setting) Aokie keeps LISTENING
    // while it speaks: outbound TTS is echo-cancelled from the inbound mic so the
    // caller can talk over the receptionist, which stops as soon as they do. Off
    // by default â†’ the proven half-duplex mute path above stays the norm. Only
    // meaningful with the agent on (it drives the interruptible reply loop).
    #[cfg(feature = "voice")]
    let barge_in = agent_enabled && std::env::var_os("AOKIE_BARGE_IN").is_some();
    // When AOKIE_AGENT_HANGUP is set (the `agentHangup` setting) the agent ends
    // the call itself once the caller's request is fully handled: it says a brief
    // goodbye, then hangs up (AT+CHUP) so the caller doesn't have to. The LLM
    // signals completion with an [[END_CALL]] marker, which is stripped before the
    // farewell is spoken/recorded. Only meaningful with the agent on.
    #[cfg(feature = "voice")]
    let agent_hangup = agent_enabled && std::env::var_os("AOKIE_AGENT_HANGUP").is_some();
    // Cleaned-mic RMS above which the caller counts as speaking over Aokie. Set
    // above the AEC's residual echo floor; tune per handset via AOKIE_BARGE_RMS.
    #[cfg(feature = "voice")]
    let barge_rms: f32 = std::env::var("AOKIE_BARGE_RMS")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|&v| v > 0.0)
        .unwrap_or(650.0);
    // The echo canceller, built lazily once we know the negotiated SCO rate.
    #[cfg(feature = "voice")]
    let mut aec: Option<crate::aec::EchoCanceller> = None;
    // Call-local speaking pace (base + detail rates, settings-seeded), mutated
    // live by the caller's "slower"/"faster"/"normal speed" voice commands and
    // reset at every call boundary.
    #[cfg(feature = "voice")]
    let mut pace = crate::speech_plan::PaceState::from_env();
    // Operator cap on how long an [[important]] span may resist an overlap.
    #[cfg(feature = "voice")]
    let protected_max_ms = crate::speech_plan::protected_max_ms_from_env();
    // The duplex floor state: PausedByCaller means "say NOTHING until the
    // caller speaks again or asks to continue" (intentional silence).
    #[cfg(feature = "voice")]
    let mut dialogue = crate::duplex::DialogueState::new();
    // The last bot line as clean SPEAKABLE text (no truncation tags) — what
    // "repeat that (slower)" replays. last_bot_reply keeps the echo-guard role.
    #[cfg(feature = "voice")]
    let mut last_bot_speech = String::new();
    // The NEXT caller turn began as OVERLAP capture (spoken while the bot was
    // talking): recorded on the turn so readers don't misread record order as
    // speech order. Set at every overlap-seed site, cleared when a turn flushes.
    #[cfg(feature = "voice")]
    let mut turn_overlapped = false;
    // Back-dated speech-start estimate for that overlap turn (its audio began
    // roughly its own duration before it was seeded).
    #[cfg(feature = "voice")]
    let mut turn_overlap_at: Option<String> = None;
    // The nudge (round 3): an interrupted reply leaves its UNSPOKEN tail here
    // so the next generation can weave the pending point in naturally instead
    // of restarting the thought. Consumed by exactly one generation.
    #[cfg(feature = "voice")]
    let mut last_cut_context: Option<String> = None;
    // Monotonic transcript turn index (caller + bot share one sequence), reset
    // per call. 1-based to match the simulated-call convention (`turn.1.final`,
    // `turn.2.final`, â€¦) so real + simulated calls dedup + display identically.
    #[cfg(feature = "voice")]
    let mut turn_index: u32 = 1;

    // Per-call state: ONE explicit session state machine (audit AK-001) owns
    // the call id, generation, phase, timing, caller id, termination intent
    // and the once-per-call flags (incoming emitted / auto-answered / toned /
    // greeted) that used to be six scattered Option<String>s here.
    //
    // Auto-answer fires IMMEDIATELY when a call appears â€” NOT after a ring
    // delay â€” because on some dongles the SCO/audio channel that comes up
    // right after the ring blocks the radio's main loop, so a late answer
    // never gets serviced. Answering in the brief pre-SCO window is what
    // gets the AT+ATA out. (Emitting `aokie.call.incoming` still waits for
    // the caller id; only the answer is hurried.)
    let mut tracker = crate::call_session::SessionTracker::new();
    // Which generation the voice pipeline is configured for; a change (new
    // call OR idle) resets per-call voice state and re-stamps the STT gate.
    #[cfg(feature = "voice")]
    let mut voice_call_gen: u64 = 0;
    // AOK-CTRL-001: call-level max-silence watchdog (agent mode). Created when
    // the greeting arms the conversation, dropped at every call boundary.
    #[cfg(feature = "voice")]
    let silence_window = max_silence_window();
    #[cfg(feature = "voice")]
    let mut silence_timer: Option<SilenceTimer> = None;

    loop {
        let mut idle = true;

        while let Some(ev) = bt.try_recv_event() {
            idle = false;
            // Final-transcript drain (audit AOK-LIF-002): the caller's last
            // words must land BEFORE call.ended — summaries and after-call
            // flows key off ended, and the last sentence is often the most
            // important one. On termination of a live call: finalize any
            // buffered audio as the closing utterance, wait (bounded) for
            // in-flight STT, and flush the held turn — THEN let handle_event
            // publish the terminal event. The call is already over, so the
            // short stall cannot delay answering it.
            #[cfg(feature = "voice")]
            if matches!(ev, aokie_dongle::bluetooth::BluetoothEvent::CallTerminated)
                && tracker.current().is_some()
            {
                if stt_had_speech && stt_buf.len() >= 16_000 / 5 {
                    if spec_utterance.take().is_some() {
                        // Already in flight speculatively — the drain below
                        // waits for that result; never send it twice.
                    } else if let Some(s) = tracker.current_mut() {
                        let utterance = s.next_utterance_id();
                        if stt_tx
                            .send(SttWork::Utterance {
                                generation: s.generation,
                                utterance,
                                samples: std::mem::take(&mut stt_buf),
                            })
                            .is_ok()
                        {
                            stt_outstanding += 1;
                        }
                    }
                }
                stt_buf.clear();
                stt_had_speech = false;
                stt_silence = Duration::ZERO;

                let gen_now = tracker.generation();
                let deadline = Instant::now() + Duration::from_millis(1500);
                while stt_outstanding > 0 {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        eprintln!(
                            "[aokie-plugin] final-transcript drain timed out with {stt_outstanding} STT job(s) in flight"
                        );
                        break;
                    }
                    match stt_result_rx.recv_timeout(left) {
                        Ok(SttResult { generation, utterance, text }) => {
                            stt_outstanding = stt_outstanding.saturating_sub(1);
                            if let Some(pos) =
                                stale_specs.iter().position(|&u| u == utterance)
                            {
                                stale_specs.remove(pos);
                                continue;
                            }
                            if generation != gen_now {
                                status.stale_stt_results.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            if agent_enabled && looks_like_echo(&text, &last_bot_reply) {
                                continue;
                            }
                            match pending_turn.as_mut() {
                                Some(p) => {
                                    p.text.push(' ');
                                    p.text.push_str(text.trim());
                                }
                                None => {
                                    pending_turn = Some(PendingTurn {
                                        corr: tracker.call_id().unwrap_or_default().to_string(),
                                        text: text.trim().to_string(),
                                        flush_at: Instant::now(),
                                    })
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                if let Some(p) = pending_turn.take() {
                    if !p.corr.is_empty() && !p.text.is_empty() {
                        emit_turn(outbox, sink, &p.corr, turn_index, "caller", &p.text);
                        turn_index += 1;
                    }
                }
            }
            handle_event(ev, &mut tracker, outbox, sink, &status);
        }

        // Per-call voice isolation (audit AK-002/C-05): the moment the session
        // generation changes — a call ended, or a new one replaced it — stamp
        // the STT gate (queued jobs for the old generation are skipped, their
        // results dropped) and reset EVERY piece of per-call voice state:
        // conversation history, transcript turn index, utterance buffers, echo
        // canceller and mute gate. Doing it on generation change (not on an
        // observed idle tick) means back-to-back calls in one event batch can
        // never leak history or half-built utterances into the next call.
        #[cfg(feature = "voice")]
        if voice_call_gen != tracker.generation() {
            // A turn still held open when its call ends (hangup inside the
            // continuation window) is RECORDED against that call — losing the
            // caller's last fragment (often the tail of a phone number) is
            // worse than a late turn event — but never answered.
            if let Some(p) = pending_turn.take() {
                if !p.corr.is_empty() && !p.text.is_empty() {
                    eprintln!(
                        "[aokie-plugin] flushing held caller turn from ended call: {}",
                        content_for_log(&p.text)
                    );
                    emit_turn(outbox, sink, &p.corr, turn_index, "caller", &p.text);
                }
            }
            voice_call_gen = tracker.generation();
            stt_current_gen.store(voice_call_gen, Ordering::Relaxed);
            // §9.2: no caller turns exist yet in the new call — any reply
            // still naming the previous call's turn number must read stale.
            status.last_caller_turn.store(0, Ordering::Relaxed);
            synth.reset_call();
            while probe_result_rx.try_recv().is_ok() {}
            let _ = stt_tx.send(SttWork::ResetCall);
            // Self-healing (AOK-LIF-002): skipped/empty STT jobs never send a
            // result, so the in-flight counter resets at every call boundary
            // rather than accumulating drift across calls.
            stt_outstanding = 0;
            turn_index = 1;
            history.clear();
            last_bot_reply.clear();
            last_bot_speech.clear();
            stt_buf.clear();
            stt_had_speech = false;
            stt_silence = Duration::ZERO;
            mute_stt_until = None;
            silence_timer = None;
            // Pace + floor state are strictly per-call: the next caller gets
            // the configured defaults, never the last caller's "slower".
            pace = crate::speech_plan::PaceState::from_env();
            dialogue.reset();
            turn_overlapped = false;
            turn_overlap_at = None;
            spec_utterance = None;
            stale_specs.clear();
            last_cut_context = None;
            // §9.3: the call-scoped agent overlay dies WITH its call — the
            // next caller can never inherit the previous caller's persona.
            call_agent_overlay = None;
            // Drop the echo canceller entirely rather than just resetting its
            // FIFOs (review sweep): it was built at the FIRST call's SCO rate
            // and reset() keeps that rate + filter length. Back-to-back calls
            // can negotiate different codecs (mSBC 16 kHz vs CVSD 8 kHz), so a
            // reused AEC would run at the wrong rate and cancel nothing. None
            // makes it rebuild at THIS call's actual sample rate on the first
            // captured frame below.
            aec = None;
        }

        // Flush a buffered incoming call once the caller id is known or the
        // grace window elapses.
        let flush_incoming = tracker.current().is_some_and(|s| {
            s.incoming_pending() && (s.caller_id.is_some() || s.incoming_pending_ms() > 800)
        });
        if flush_incoming {
            flush_incoming_if_pending(&mut tracker, outbox, sink);
        }

        // Auto-answer ASAP: the instant a call is present and not yet answered,
        // send the answer â€” before the audio channel comes up and freezes the
        // loop. Answer exactly once per call (the session's auto_answered flag).
        if auto_answer {
            if let Some(s) = tracker.current_mut() {
                if !s.auto_answered && !s.is_active() {
                    // AOK-VOICE-001: never answer into silence. A KNOWN voice
                    // failure (asset preflight or a live engine/synthesis
                    // failure) means the receptionist can't hear or speak —
                    // leave the call ringing for the operator's phone instead
                    // of answering it into a dead line. Fail-safe direction:
                    // only a DEFINITIVE recorded failure blocks; healthy or
                    // not-yet-exercised pipelines answer as before.
                    #[cfg(feature = "voice")]
                    let voice_block: Option<String> = {
                        let tts = status.tts_error.lock().unwrap().clone();
                        let stt = status.stt_error.lock().unwrap().clone();
                        // PROC-001: when the IN-PLUGIN agent owns replies, a dead
                        // LLM blocks auto-answer too — hearing and speaking without
                        // thinking is still answering the caller into a dead line.
                        // Flow-responder mode (agent off) is not gated here: replies
                        // come from host flows the plugin cannot probe.
                        let llm = if agent_enabled {
                            status.llm_error.lock().unwrap().clone()
                        } else {
                            None
                        };
                        // VOICE-001: never arm on UNPROVEN engines — the loopback
                        // self-test must have landed (skip cases record ok) and
                        // passed before the receptionist may pick up.
                        let self_test = match status.self_test.lock().unwrap().as_ref() {
                            None => Some(
                                "voice self-test still running — arming once it passes"
                                    .to_string(),
                            ),
                            Some(r) if !r.ok => {
                                Some(format!("voice self-test failed: {}", r.detail))
                            }
                            Some(_) => None,
                        };
                        tts.or(stt).or(llm).or(self_test)
                    };
                    #[cfg(not(feature = "voice"))]
                    let voice_block: Option<String> = None;
                    if let Some(reason) = voice_block {
                        if voice_block_logged_call.as_deref() != Some(s.id.as_str()) {
                            eprintln!(
                                "[aokie-plugin] auto-answer BLOCKED — voice pipeline down ({reason}); the call rings through to the operator"
                            );
                            voice_block_logged_call = Some(s.id.clone());
                        }
                    } else {
                        match bt.answer_call() {
                            Ok(()) => {
                                eprintln!("[aokie-plugin] auto-answered incoming call (immediate)")
                            }
                            Err(e) => eprintln!("[aokie-plugin] auto-answer failed: {e}"),
                        }
                        s.auto_answered = true;
                        idle = false;
                    }
                }
            }
        }

        // Stage-2 diagnostic: once the call's audio channel is up (sample rate
        // becomes non-zero), play a short two-note chime to the caller to verify
        // the OUTBOUND SCO path actually reaches the phone on this dongle. Real
        // TTS speech replaces this once outbound audio is confirmed. Gated by
        // settings.answerTone.
        if answer_tone {
            let sr = bt.get_sample_rate();
            if let Some(s) = tracker.current_mut() {
                if !s.toned && sr > 0 {
                    let tone = greeting_tone(sr);
                    eprintln!(
                        "[aokie-plugin] answerTone: sending {} samples @ {}Hz to the caller",
                        tone.len(),
                        sr
                    );
                    bt.send_audio(&tone);
                    s.toned = true;
                    idle = false;
                }
            }
        }

        // Greet the caller with real TTS speech once the SCO audio channel is up
        // (voice build). Plays exactly once per call (the session's `greeted`
        // flag); per-call voice state RESETS live in the generation block above.
        let greet_now = {
            let sr = bt.get_sample_rate();
            match tracker.current_mut() {
                Some(s) if !s.greeted && sr > 0 => {
                    s.greeted = true;
                    Some((s.id.clone(), sr))
                }
                _ => None,
            }
        };
        if let Some((corr, sr)) = greet_now {
            #[cfg(not(feature = "voice"))]
            let _ = (&corr, sr);
            #[cfg(feature = "voice")]
            {
                // Build the echo canceller once we know the negotiated SCO
                // rate (full-duplex only). Reused for every phrase this call.
                if barge_in && aec.is_none() {
                    aec = Some(crate::aec::EchoCanceller::new(sr as u32));
                    eprintln!(
                        "[aokie-plugin] full-duplex barge-in ON (AEC @ {sr}Hz, rms>{barge_rms})"
                    );
                }
                // §9.3: a call-scoped greeting (personalize-caller) wins for
                // ITS call; the configured global greeting is the fallback.
                let overlay_greeting = call_agent_overlay
                    .as_ref()
                    .filter(|o| o.call_id == corr)
                    .and_then(|o| o.greeting.as_deref());
                if let Some(text) = overlay_greeting.or(greeting.as_deref()) {
                    // In barge-in mode the caller can talk over the greeting;
                    // in half-duplex we mute STT for its playout instead.
                    let (aec_ref, brms) = if barge_in {
                        (aec.as_mut(), Some(barge_rms))
                    } else {
                        (None, None)
                    };
                    // AOK-CTRL-001: a hangup/reject arriving DURING the
                    // greeting cuts it at chunk granularity. The greeting runs
                    // through the same span planner as every other speech
                    // origin (pacing + digit handling + the probe lane, so a
                    // spoken "wait" cuts even the greeting).
                    let mut probe = ControlProbe::new(&control_rx, &mut pending_controls);
                    let mut lane =
                        SttProbeLane::new(&stt_tx, &probe_result_rx, voice_call_gen, &status);
                    let lane_ref = if barge_in { Some(&mut lane) } else { None };
                    let speak_started = Instant::now();
                    let planned = speak_planned(
                        bt,
                        &synth,
                        text,
                        sr,
                        aec_ref,
                        brms,
                        Some(&mut probe),
                        &pace,
                        protected_max_ms,
                        lane_ref,
                    );
                    let out = planned.outcome;
                    if !planned.text.trim().is_empty() {
                        note_tts_outcome(&status, &out);
                    }
                    if let Some(action) = probe.action.take() {
                        perform_cancel_action(action, bt, &mut tracker, outbox, sink);
                    }
                    // A spoken floor command over the greeting = the caller
                    // holds the floor from the very first words.
                    if let Some(intent) = out.commanded {
                        dialogue.apply(intent);
                        eprintln!(
                            "[aokie-plugin] caller commanded {intent:?} over the greeting — holding the floor"
                        );
                    }
                    if barge_in {
                        if out.barged {
                            bt.flush_tx_audio();
                        }
                        mute_stt_until = None;
                    } else {
                        mute_stt_until =
                            Some(Instant::now() + out.dur + Duration::from_millis(400));
                    }
                    stt_buf.clear();
                    stt_had_speech = false;
                    stt_silence = Duration::ZERO;
                    // AK-008 + scratchpad: seed the caller's turn with EVERY
                    // piece of echo-cancelled speech captured while we were
                    // talking — barge or not — so words spoken over the
                    // greeting are heard, never discarded.
                    if !out.captured_speech.is_empty() {
                        stt_buf = crate::voice::to_f32_16k(&out.captured_speech, sr as u32);
                        stt_had_speech = true;
                        turn_overlapped = true;
                        turn_overlap_at = Some(aokie_core::events::iso8601_ago_ms(
                            (out.captured_speech.len() * 1000 / (sr as usize).max(1)) as u64,
                        ));
                    }
                    // Truthful transcript (audit AOK-VOICE-001/002): record the
                    // greeting only when synthesis actually produced audio —
                    // and only the spans that PLAYED.
                    if out.dur > Duration::ZERO && !planned.played_text.is_empty() {
                        let delivery = if out.barged {
                            "interrupted"
                        } else if out.cancelled {
                            "operator_ended"
                        } else {
                            "complete"
                        };
                        emit_turn_with_delivery(
                            outbox,
                            sink,
                            &corr,
                            turn_index,
                            "bot",
                            &planned.played_text,
                            Some(delivery),
                            Some(&aokie_core::events::iso8601_ago_ms(speak_started.elapsed().as_millis() as u64)),
                        );
                        turn_index += 1;
                        history.push(
                            serde_json::json!({ "role": "assistant", "content": planned.played_text }),
                        );
                        // Echo guard + replay compare against what was SENT —
                        // the flushed-but-echoed tail must still match (§6.3).
                        last_bot_reply = planned.sent_text.clone();
                        last_bot_speech = planned.sent_text;
                    } else {
                        eprintln!(
                            "[aokie-plugin] greeting produced NO audio (TTS failed) — not recorded as a spoken turn"
                        );
                    }
                }
                // AOK-CTRL-001: the conversation is live from here — start the
                // call-level max-silence watchdog (agent mode only; in flow
                // mode the host owns pacing).
                if agent_enabled {
                    silence_timer = Some(SilenceTimer::new(silence_window, Instant::now()));
                }
            }
            idle = false;
        }

        // Inbound caller audio.
        #[cfg(not(feature = "voice"))]
        while bt.try_recv_audio().is_some() {
            idle = false;
        }
        // Voice build: energy-VAD segment the caller's speech â†’ ship each finished
        // utterance to the STT worker. ~350 RMS (i16 units) gates speech; ~700 ms
        // of trailing silence ends an utterance; sub-350 ms blips are dropped.
        #[cfg(feature = "voice")]
        {
            const SPEECH_RMS: f32 = 350.0;
            let endpoint = stt_endpoint;
            let muted = mute_stt_until.is_some_and(|t| Instant::now() < t);
            while let Some(frame) = bt.try_recv_audio() {
                idle = false;
                if tracker.current().is_none() {
                    continue;
                }
                // Full-duplex: echo-cancel the mic (so Aokie's own voice, even
                // when it's mid-reply, doesn't transcribe as the caller) and keep
                // reference consumption 1:1 with the mic. Half-duplex: the mute
                // window swallows the echo instead, so we just skip while muted.
                let samples: std::borrow::Cow<[i16]> = if barge_in {
                    match aec.as_mut() {
                        Some(a) => {
                            let cleaned = a.process_capture(&frame.samples);
                            if cleaned.is_empty() {
                                continue;
                            }
                            std::borrow::Cow::Owned(cleaned)
                        }
                        None => std::borrow::Cow::Borrowed(&frame.samples[..]),
                    }
                } else {
                    if muted {
                        continue;
                    }
                    std::borrow::Cow::Borrowed(&frame.samples[..])
                };
                let rms = crate::voice::frame_rms(&samples);
                let f16 = crate::voice::to_f32_16k(&samples, frame.sample_rate as u32);
                let frame_dur =
                    Duration::from_secs_f32(samples.len() as f32 / frame.sample_rate.max(1) as f32);
                if rms > SPEECH_RMS {
                    stt_had_speech = true;
                    stt_silence = Duration::ZERO;
                    // Speech resumed after a speculative send: that spec no
                    // longer matches the utterance — drop its result by id.
                    if let Some(id) = spec_utterance.take() {
                        stale_specs.push(id);
                    }
                    stt_buf.extend_from_slice(&f16);
                    // AOK-CTRL-001: live caller audio resets the max-silence window.
                    if let Some(t) = silence_timer.as_mut() {
                        t.note_activity(Instant::now());
                    }
                } else if stt_had_speech {
                    stt_silence += frame_dur;
                    stt_buf.extend_from_slice(&f16); // keep trailing silence for context
                }
                if stt_buf.len() > 16_000 * 15 {
                    stt_silence = endpoint; // force-flush a runaway (~15 s) utterance
                    if let Some(id) = spec_utterance.take() {
                        stale_specs.push(id);
                    }
                }
            }
            // Speculative early transcription: 200 ms into the pause (well
            // before the endpoint), ship the buffer as-is. If the caller
            // stays quiet, the endpoint below has NOTHING left to do — the
            // text is already in flight (or back).
            if stt_had_speech
                && spec_utterance.is_none()
                && stt_silence >= Duration::from_millis(200)
                && stt_silence < endpoint
                && stt_buf.len() >= 16_000 / 5
            {
                if let Some(s) = tracker.current_mut() {
                    let utterance = s.next_utterance_id();
                    if stt_tx
                        .send(SttWork::Utterance {
                            generation: s.generation,
                            utterance,
                            samples: stt_buf.clone(),
                        })
                        .is_ok()
                    {
                        stt_outstanding += 1;
                        spec_utterance = Some(utterance);
                    }
                }
            }
            if stt_had_speech && stt_silence >= endpoint {
                if spec_utterance.take().is_some() {
                    // The speculation IS this utterance (nothing new was said
                    // since it was sent) — never transcribe it twice.
                    status.early_stt_hits.fetch_add(1, Ordering::Relaxed);
                    stt_buf.clear();
                } else if stt_buf.len() >= 16_000 / 5 {
                    // Stamp the job with the call it belongs to (audit C-05).
                    if let Some(s) = tracker.current_mut() {
                        let utterance = s.next_utterance_id();
                        if stt_tx
                            .send(SttWork::Utterance {
                                generation: s.generation,
                                utterance,
                                samples: std::mem::take(&mut stt_buf),
                            })
                            .is_ok()
                        {
                            stt_outstanding += 1;
                        }
                    } else {
                        stt_buf.clear();
                    }
                } else {
                    stt_buf.clear();
                }
                stt_had_speech = false;
                stt_silence = Duration::ZERO;
            }
            // Finished transcripts: accumulate into the OPEN caller turn
            // (audit AK-008). A transcript whose tail looks unfinished — a
            // digit group mid-phone-number, "my number is…" — keeps the turn
            // open for CONTINUATION_HOLD instead of triggering a reply into
            // the caller's pause; the next utterance merges into it. Anything
            // else flushes on the spot (no added latency for normal turns).
            while let Ok(SttResult {
                generation,
                utterance,
                text,
            }) = stt_result_rx.try_recv()
            {
                idle = false;
                stt_outstanding = stt_outstanding.saturating_sub(1);
                // A superseded speculative transcription (its utterance grew
                // after it was sent): the whole utterance re-transcribed —
                // drop this partial result, never merge it.
                if let Some(pos) = stale_specs.iter().position(|&u| u == utterance) {
                    stale_specs.remove(pos);
                    continue;
                }
                // Stale-result gate (audit C-05): only text whose generation IS
                // the current call may be recorded or answered. A slow result
                // from a previous call is dropped and counted — never spoken
                // to, or attributed to, the next caller.
                if tracker.current().map(|s| s.generation) != Some(generation) {
                    let n = status.stale_stt_results.fetch_add(1, Ordering::Relaxed) + 1;
                    eprintln!(
                        "[aokie-plugin] DROPPED stale STT result (call gen {generation}, utterance {utterance}, current gen {}, {n} total): {}",
                        tracker.generation(),
                        content_for_log(&text)
                    );
                    continue;
                }
                // Drop a transcript that's really Aokie's own reply echoing back
                // (belt-and-suspenders over the half-duplex mute) so it never
                // records it as a caller turn or answers itself.
                if agent_enabled && looks_like_echo(&text, &last_bot_reply) {
                    eprintln!("[aokie-plugin] ignored self-echo: {}", content_for_log(&text));
                    continue;
                }
                let corr = tracker.call_id().unwrap_or_default().to_string();
                match pending_turn.as_mut() {
                    Some(p) => {
                        p.text.push(' ');
                        p.text.push_str(text.trim());
                        p.corr = corr;
                    }
                    None => {
                        pending_turn = Some(PendingTurn {
                            corr,
                            text: text.trim().to_string(),
                            flush_at: Instant::now(),
                        })
                    }
                }
                let p = pending_turn.as_mut().expect("just set");
                if p.text.len() < CONTINUATION_MAX_CHARS && turn_looks_unfinished(&p.text) {
                    p.flush_at = Instant::now() + CONTINUATION_HOLD;
                    eprintln!(
                        "[aokie-plugin] holding turn open (looks unfinished): {}",
                        content_for_log(&p.text)
                    );
                } else {
                    p.flush_at = Instant::now();
                }
            }
            // Flush the open turn once its hold expired AND the caller isn't
            // mid-utterance (fresh speech extends the merge window naturally).
            let flushed_turn = match pending_turn.as_ref() {
                Some(p) if Instant::now() >= p.flush_at && !stt_had_speech => {
                    pending_turn.take().map(|p| (p.corr, p.text))
                }
                _ => None,
            };
            if let Some((corr, text)) = flushed_turn {
                idle = false;
                {
                    // Duplex floor coordination: parse the caller's words for a
                    // deterministic FLOOR COMMAND before any model runs. "Wait"
                    // / "stop" / "let me think" produce INTENTIONAL SILENCE
                    // (the most human response is sometimes nothing at all);
                    // pace commands and replays are handled without burning a
                    // generation. Ambiguity always reads as Content.
                    let intent = if agent_enabled {
                        crate::duplex::parse_caller_intent(&text)
                    } else {
                        crate::duplex::CallerIntent::Content
                    };
                    let is_control = intent != crate::duplex::CallerIntent::Content;
                    eprintln!(
                        "[aokie-plugin] heard [turn {turn_index}]{}: {}",
                        if is_control {
                            format!(" (control: {intent:?})")
                        } else {
                            String::new()
                        },
                        content_for_log(&text)
                    );
                    // Control turns are recorded truthfully but tagged, so
                    // flows/business logic can skip them.
                    emit_turn_full(
                        outbox,
                        sink,
                        &corr,
                        turn_index,
                        "caller",
                        &text,
                        None,
                        if is_control { Some("control") } else { None },
                        turn_overlapped,
                        turn_overlap_at.as_deref(),
                    );
                    // §9.2: this is now the newest caller turn — a flow reply
                    // naming an older one is stale (typed refusal upstream).
                    status.last_caller_turn.store(turn_index, Ordering::Relaxed);
                    turn_overlapped = false;
                    turn_overlap_at = None;
                    turn_index += 1;

                    // A bare hesitation ("Uh", "Well...") that outlived the
                    // continuation hold is the caller THINKING: record it,
                    // reply with NOTHING, keep listening. Filler never
                    // reaches the model — as history or as a prompt.
                    let hesitation = !is_control && crate::duplex::is_hesitation(&text);
                    if agent_enabled && hesitation {
                        eprintln!(
                            "[aokie-plugin] caller hesitation — staying quiet while they finish thinking"
                        );
                    }

                    // Whether to fall through to the normal LLM reply path.
                    let mut respond_with_llm = false;
                    if agent_enabled && !hesitation {
                        history.push(serde_json::json!({ "role": "user", "content": text }));
                        if history.len() > 24 {
                            let drop = history.len() - 24;
                            history.drain(..drop);
                        }
                        match dialogue.apply(intent) {
                            crate::duplex::DialogueAction::ReplyNormally => {
                                respond_with_llm = true;
                            }
                            crate::duplex::DialogueAction::StaySilent => {
                                // The caller asked for the floor to stay open
                                // ("wait", "stop", "let me think"): keep
                                // listening, generate nothing, speak nothing.
                                eprintln!(
                                    "[aokie-plugin] caller holds the floor ({intent:?}) — waiting silently"
                                );
                            }
                            crate::duplex::DialogueAction::AdjustPace(cmd) => {
                                let line = match cmd {
                                    crate::duplex::PaceCommand::Slower => {
                                        pace.slower();
                                        "Sure, I'll slow down."
                                    }
                                    crate::duplex::PaceCommand::Faster => {
                                        pace.faster();
                                        "Sure, I'll speed up."
                                    }
                                    crate::duplex::PaceCommand::Normal => {
                                        pace.reset();
                                        "Okay, back to normal speed."
                                    }
                                };
                                eprintln!(
                                    "[aokie-plugin] caller pace command {cmd:?} — base rate now {:.2}",
                                    pace.base()
                                );
                                let sr = bt.get_sample_rate();
                                if sr > 0 {
                                    let mut probe =
                                        ControlProbe::new(&control_rx, &mut pending_controls);
                                    let (aec_ref, brms) = if barge_in {
                                        (aec.as_mut(), Some(barge_rms))
                                    } else {
                                        (None, None)
                                    };
                                    // The ack itself plays at the NEW pace —
                                    // the confirmation demonstrates the change.
                                    let ack_started = Instant::now();
                                    let out = tts_speak(
                                        bt,
                                        &synth,
                                        line,
                                        sr,
                                        aec_ref,
                                        brms,
                                        Some(&mut probe),
                                        pace.base(),
                                        None,
                                        None,
                                    );
                                    note_tts_outcome(&status, &out);
                                    if !barge_in {
                                        mute_stt_until = Some(
                                            Instant::now() + out.dur + Duration::from_millis(400),
                                        );
                                    }
                                    if out.barged {
                                        bt.flush_tx_audio();
                                    }
                                    if !out.captured_speech.is_empty() {
                                        let mut seeded = crate::voice::to_f32_16k(
                                            &out.captured_speech,
                                            sr as u32,
                                        );
                                        seeded.extend_from_slice(&stt_buf);
                                        stt_buf = seeded;
                                        stt_had_speech = true;
                                        stt_silence = Duration::ZERO;
                                        turn_overlapped = true;
                                        turn_overlap_at = Some(aokie_core::events::iso8601_ago_ms(
                                            (out.captured_speech.len() * 1000
                                                / (sr as usize).max(1))
                                                as u64,
                                        ));
                                    }
                                    if out.dur > Duration::ZERO {
                                        let delivery = if out.barged {
                                            "interrupted"
                                        } else if out.cancelled {
                                            "operator_ended"
                                        } else {
                                            "complete"
                                        };
                                        emit_turn_with_delivery(
                                            outbox, sink, &corr, turn_index, "bot", line,
                                            Some(delivery),
                                            Some(&aokie_core::events::iso8601_ago_ms(
                                                ack_started.elapsed().as_millis() as u64,
                                            )),
                                        );
                                        turn_index += 1;
                                        history.push(serde_json::json!({
                                            "role": "assistant",
                                            "content": line,
                                        }));
                                        last_bot_reply = line.to_string();
                                    }
                                    if let Some(action) = probe.action.take() {
                                        perform_cancel_action(
                                            action, bt, &mut tracker, outbox, sink,
                                        );
                                    }
                                }
                            }
                            crate::duplex::DialogueAction::Replay { slower } => {
                                if last_bot_speech.is_empty() {
                                    // Nothing to replay yet — let the model
                                    // answer the request instead.
                                    respond_with_llm = true;
                                } else {
                                    let replay_pace = if slower {
                                        pace.replay_slower()
                                    } else {
                                        pace.clone()
                                    };
                                    eprintln!(
                                        "[aokie-plugin] replaying the last reply{} (deterministic)",
                                        if slower { " slower" } else { "" }
                                    );
                                    let sr = bt.get_sample_rate();
                                    if sr > 0 {
                                        let replay_text = last_bot_speech.clone();
                                        let mut probe =
                                            ControlProbe::new(&control_rx, &mut pending_controls);
                                        let (aec_ref, brms) = if barge_in {
                                            (aec.as_mut(), Some(barge_rms))
                                        } else {
                                            (None, None)
                                        };
                                        let mut lane = SttProbeLane::new(
                                            &stt_tx,
                                            &probe_result_rx,
                                            voice_call_gen,
                                            &status,
                                        );
                                        let lane_ref =
                                            if barge_in { Some(&mut lane) } else { None };
                                        let replay_started = Instant::now();
                                        let planned = speak_planned(
                                            bt,
                                            &synth,
                                            &replay_text,
                                            sr,
                                            aec_ref,
                                            brms,
                                            Some(&mut probe),
                                            &replay_pace,
                                            protected_max_ms,
                                            lane_ref,
                                        );
                                        let out = planned.outcome;
                                        note_tts_outcome(&status, &out);
                                        if !barge_in {
                                            mute_stt_until = Some(
                                                Instant::now()
                                                    + out.dur
                                                    + Duration::from_millis(400),
                                            );
                                        }
                                        if out.barged {
                                            bt.flush_tx_audio();
                                        }
                                        if !out.captured_speech.is_empty() {
                                            let mut seeded = crate::voice::to_f32_16k(
                                                &out.captured_speech,
                                                sr as u32,
                                            );
                                            seeded.extend_from_slice(&stt_buf);
                                            stt_buf = seeded;
                                            stt_had_speech = true;
                                            stt_silence = Duration::ZERO;
                                            turn_overlapped = true;
                                            turn_overlap_at =
                                                Some(aokie_core::events::iso8601_ago_ms(
                                                    (out.captured_speech.len() * 1000
                                                        / (sr as usize).max(1))
                                                        as u64,
                                                ));
                                        }
                                        if out.dur > Duration::ZERO
                                            && !planned.played_text.is_empty()
                                        {
                                            let delivery = if out.barged {
                                                "interrupted"
                                            } else if out.cancelled {
                                                "operator_ended"
                                            } else {
                                                "complete"
                                            };
                                            emit_turn_with_delivery(
                                                outbox,
                                                sink,
                                                &corr,
                                                turn_index,
                                                "bot",
                                                &planned.played_text,
                                                Some(delivery),
                                                Some(&aokie_core::events::iso8601_ago_ms(
                                                    replay_started.elapsed().as_millis() as u64,
                                                )),
                                            );
                                            turn_index += 1;
                                            history.push(serde_json::json!({
                                                "role": "assistant",
                                                "content": planned.played_text,
                                            }));
                                            last_bot_reply = planned.sent_text;
                                        }
                                        if let Some(action) = probe.action.take() {
                                            perform_cancel_action(
                                                action, bt, &mut tracker, outbox, sink,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        // The silence watchdog follows the floor state: a
                        // DELIBERATE pause stretches the window (never nag a
                        // caller who asked for quiet); everything else runs
                        // the normal window from now.
                        if !silence_window.is_zero()
                            && tracker.current().is_some_and(|s| s.is_active())
                        {
                            let w = if dialogue.is_paused() {
                                silence_window * 3
                            } else {
                                silence_window
                            };
                            silence_timer = Some(SilenceTimer::new(w, Instant::now()));
                        }
                    }

                    if agent_enabled && respond_with_llm {
                        // Lazily connect to the local LLM on the first caller turn.
                        if agent_client.is_none() {
                            let configured = agent_endpoint.lock().unwrap().clone();
                            match crate::agent::discover_endpoint(configured.as_deref()) {
                                Some(ep) => {
                                    let c = crate::agent::LlmClient::new(ep, agent_model.clone());
                                    eprintln!(
                                        "[aokie-plugin] voice agent LLM: {} (model {:?})",
                                        c.endpoint(),
                                        c.model()
                                    );
                                    agent_client = Some(c);
                                    // PROC-001: a live connect is fresher than the probe.
                                    *status.llm_error.lock().unwrap() = None;
                                }
                                None => {
                                    eprintln!(
                                        "[aokie-plugin] voice agent: no local LLM reachable (:8080/:11434)"
                                    );
                                    *status.llm_error.lock().unwrap() = Some(
                                        "no reachable LLM at reply time (tried llama.cpp :8080 and ollama :11434)"
                                            .to_string(),
                                    );
                                }
                            }
                        }
                        if let Some(client) = agent_client.as_ref() {
                            let sr = bt.get_sample_rate();
                            // Add the standing instructions at reply time (not by
                            // mutating agent_persona, which a live Configure could
                            // replace): spoken-delivery/markers always, the
                            // end-call marker only when agentHangup is on.
                            // §9.3 call-scoped overlay: a caller-specific
                            // persona (personalize-caller) applies to THIS
                            // call only — wiped at the call boundary, it can
                            // never leak into the next caller's conversation.
                            let persona_now: &str = call_agent_overlay
                                .as_ref()
                                .and_then(|o| o.persona.as_deref())
                                .unwrap_or(&agent_persona);
                            let mut system_prompt = if agent_hangup {
                                format!("{persona_now}{SPEECH_STYLE_INSTRUCTION}{END_CALL_INSTRUCTION}")
                            } else {
                                format!("{persona_now}{SPEECH_STYLE_INSTRUCTION}")
                            };
                            // The nudge: the caller interrupted the previous
                            // reply — hand the model its unspoken tail so the
                            // join sounds like one flowing thought.
                            if let Some(tail) = last_cut_context.take() {
                                system_prompt.push_str(&format!(
                                    "\n\nThe caller interrupted your previous reply. You were about to say: \"{tail}\". Respond to what they just said, weaving that pending point in ONLY if it is still relevant. Never repeat what you already said and never restart the reply."
                                ));
                            }
                            let mut messages = vec![
                                serde_json::json!({ "role": "system", "content": system_prompt }),
                            ];
                            messages.extend(history.iter().cloned());
                            // Half-duplex: mute STT for the WHOLE reply as it streams.
                            // Sentences synthesize faster than they play, so the audio
                            // keeps playing (queued) after synthesis finishes; muting
                            // only the last sentence let the tail echo back and Aokie
                            // answered itself. Track cumulative playback from t0.
                            // Full-duplex (barge_in): no mute â€” the AEC keeps the mic
                            // clean AND watches for the caller talking over the reply,
                            // cutting it short (flush the queued tail) the moment they do.
                            let t0 = Instant::now();
                            let mut reply_dur = Duration::ZERO;
                            let mut barged = false;
                            // AK-008 + scratchpad: what the caller said WHILE
                            // Aokie spoke — EVERYTHING above the speech gate is
                            // captured (barge or not) and prepended to their
                            // turn after the reply, so no overlapped words are
                            // ever lost. Listening never stops.
                            let mut overlap_capture: Vec<i16> = Vec::new();
                            // Phase 2: the gaps BETWEEN sentences (LLM still
                            // streaming) are listened to as well — sustained
                            // speech there barges exactly like speech over a
                            // sentence. State for the gap-scan.
                            let mut gap_frames: u32 = 0;
                            let mut gap_start: Option<usize> = None;
                            let mut overlap_has_speech = false;
                            // ONE probe lane for the whole reply (the live
                            // scratchpad): partials accumulate across
                            // sentences so boundary decisions see everything
                            // said so far, and a command heard at the tail of
                            // one sentence still cuts the next.
                            let mut reply_lane = SttProbeLane::new(
                                &stt_tx,
                                &probe_result_rx,
                                voice_call_gen,
                                &status,
                            );
                            // Distinguish a CALLER barge-in from an OPERATOR
                            // hangup/reject mid-reply (review sweep): both stop
                            // the reply, but the transcript must not label an
                            // operator action as "caller interrupted".
                            let mut operator_ended = false;
                            // AOK-CTRL-001 follow-up: the call's audio channel
                            // died mid-reply (link loss / SCO teardown). Nobody
                            // can hear the rest — and speaking it anyway queued
                            // stale audio that played into the NEXT call
                            // (observed live 2026-07-13). Stops the pump; also
                            // suppresses the dead-air fail-safe and the agent
                            // hangup (both would act on a dead link).
                            let mut line_dead = false;
                            // Set when the reply carried the [[END_CALL]] marker: the
                            // agent finalized the call and should hang up after the
                            // goodbye plays (unless the caller barged in over it).
                            let mut hangup_requested = false;
                            // Set when the reply carried the [[WAIT]] marker: the
                            // model chose INTENTIONAL SILENCE (the caller asked for
                            // a moment / is thinking). An empty waited reply is NOT
                            // dead air, and the floor stays with the caller.
                            let mut wait_requested = false;
                            // What the caller actually HEARD: sentences that
                            // reached the speaker (audit AK-008 + sweep). The
                            // history/turn record uses this, never the full
                            // generation — populated in BOTH duplex modes so a
                            // mid-reply failure/hangup records what played.
                            let mut spoken: Vec<String> = Vec::new();
                            // §6.3: the SENT twin of `spoken` — full span text
                            // that produced audio (echo guard / replay), while
                            // `spoken` holds the conservative audible estimate
                            // (transcript / history / nudge).
                            let mut sent_spans: Vec<String> = Vec::new();
                            eprintln!("[aokie-plugin] agent replying (streaming)â€¦");
                            // AOK-CTRL-001: the LLM stream runs on a DETACHED
                            // worker; this thread pumps sentences + controls, so
                            // a hangup/reject acts within ~25 ms even against a
                            // stalled or punctuation-free stream (the old poll
                            // only ran per SENTENCE, and a stream that never
                            // yields one blocked cancellation entirely). The
                            // bounded channel is the backpressure — synthesis
                            // paces the worker, a runaway generation blocks the
                            // WORKER, never grows a queue. The shared activity
                            // stamp feeds the idle-deadline watchdog below. An
                            // abandoned worker aborts at its next stream line
                            // (cancel flag) or, if stuck mid-read, at the
                            // client's whole-request timeout.
                            let (reply_tx, reply_rx) =
                                std::sync::mpsc::sync_channel::<ReplyMsg>(REPLY_CHANNEL_BOUND);
                            let reply_cancel = Arc::new(AtomicBool::new(false));
                            let reply_activity: Arc<Mutex<Option<Instant>>> =
                                Arc::new(Mutex::new(None));
                            {
                                let client = client.clone();
                                let cancel = reply_cancel.clone();
                                let activity = reply_activity.clone();
                                let messages = serde_json::json!(messages);
                                // A failed spawn drops reply_tx → the pump sees
                                // Disconnected and reports a reply failure.
                                let _ = std::thread::Builder::new()
                                    .name("aokie-agent-reply".to_string())
                                    .spawn(move || {
                                        let res = client.stream_reply(
                                            messages,
                                            &cancel,
                                            || {
                                                *activity.lock().unwrap() = Some(Instant::now());
                                            },
                                            |sentence| {
                                                reply_tx
                                                    .send(ReplyMsg::Sentence(sentence.to_string()))
                                                    .is_ok()
                                            },
                                        );
                                        let _ = reply_tx.send(ReplyMsg::Done(res));
                                    })
                                    .map_err(|e| {
                                        eprintln!(
                                            "[aokie-plugin] reply worker failed to start: {e}"
                                        )
                                    });
                            }
                            let started = Instant::now();
                            let mut stream_outcome: Option<Result<String, String>> = None;
                            'pump: loop {
                                // The audio channel is gone (SCO teardown /
                                // link loss): abandon the reply NOW — the
                                // outer loop's event drain will run the real
                                // call teardown. `sr > 0` guards the (never
                                // legitimate) reply-started-without-audio
                                // case, which the dead-air fail-safe owns.
                                if sr > 0 && bt.get_sample_rate() == 0 {
                                    eprintln!(
                                        "[aokie-plugin] call audio channel gone mid-reply — abandoning the rest of the reply"
                                    );
                                    reply_cancel.store(true, Ordering::Relaxed);
                                    line_dead = true;
                                    break 'pump;
                                }
                                // Urgent controls act immediately — no stream
                                // progress required (audit AK-003 + AOK-CTRL-001);
                                // everything else parks for the main control loop.
                                while let Ok(ctl) = control_rx.try_recv() {
                                    match ctl {
                                        RadioControl::Hangup { op } => {
                                            tracker.note_intent(
                                                crate::call_session::TerminationIntent::OperatorHangup,
                                            );
                                            bt.flush_tx_audio();
                                            if let Err(e) = bt.hangup() {
                                                eprintln!("[aokie-plugin] mid-reply hangup failed: {e}");
                                                emit_control_failed(
                                                    outbox, sink, &tracker, "call.hangup",
                                                    op.as_deref(), &e,
                                                );
                                            }
                                            operator_ended = true; // record only what played, not "caller interrupted"
                                        }
                                        RadioControl::Reject { op } => {
                                            tracker.note_intent(
                                                crate::call_session::TerminationIntent::OperatorReject,
                                            );
                                            bt.flush_tx_audio();
                                            if let Err(e) = bt.reject_call() {
                                                eprintln!("[aokie-plugin] mid-reply reject failed: {e}");
                                                emit_control_failed(
                                                    outbox, sink, &tracker, "call.reject",
                                                    op.as_deref(), &e,
                                                );
                                            }
                                            operator_ended = true;
                                        }
                                        other => pending_controls.push_back(other),
                                    }
                                }
                                if operator_ended {
                                    reply_cancel.store(true, Ordering::Relaxed);
                                    break 'pump;
                                }
                                // Phase 2: listening never stops — drain the mic
                                // even BETWEEN sentences (while the LLM is still
                                // thinking/streaming). Sustained caller speech in
                                // a gap barges the reply exactly like speech over
                                // a playing sentence, and everything heard rides
                                // the scratchpad into their next turn.
                                if barge_in {
                                    if let Some(a) = aec.as_mut() {
                                        let gap_frame = (sr as usize / 100).max(80);
                                        while let Some(rxa) = bt.try_recv_audio() {
                                            let cleaned = a.process_capture(&rxa.samples);
                                            if cleaned.is_empty() {
                                                continue;
                                            }
                                            if scan_barge_frames(
                                                &cleaned,
                                                gap_frame,
                                                CAPTURE_RMS,
                                                barge_rms,
                                                true,
                                                &mut gap_frames,
                                                22,
                                                &mut overlap_capture,
                                                &mut gap_start,
                                            ) {
                                                barged = true;
                                            }
                                        }
                                        // Idle-trim: while nothing in the buffer is
                                        // speech, only a short pre-roll tail matters.
                                        if !overlap_has_speech && gap_start.is_none() {
                                            let keep = (sr as usize).saturating_mul(2).max(1);
                                            if overlap_capture.len() > keep * 2 {
                                                overlap_capture
                                                    .drain(..overlap_capture.len() - keep);
                                            }
                                        }
                                        if barged {
                                            eprintln!(
                                                "[aokie-plugin] caller spoke between sentences — reply cut"
                                            );
                                            status.gap_yields.fetch_add(1, Ordering::Relaxed);
                                            bt.flush_tx_audio();
                                            reply_cancel.store(true, Ordering::Relaxed);
                                            break 'pump;
                                        }
                                    }
                                }
                                match reply_rx.recv_timeout(Duration::from_millis(25)) {
                                    Ok(ReplyMsg::Sentence(sentence)) => {
                                        // Strip any [[END_CALL]] marker BEFORE synthesis so the
                                        // caller never hears it and it never lands in the
                                        // transcript; its presence arms the post-reply hangup
                                        // REQUEST (validated by agent_hangup_verdict below).
                                        let (spoken_text, had_marker) =
                                            strip_end_call_marker(&sentence);
                                        if had_marker {
                                            hangup_requested = true;
                                        }
                                        // [[WAIT]] = the model chose intentional silence.
                                        if crate::speech_plan::has_wait_marker(&spoken_text) {
                                            wait_requested = true;
                                        }
                                        eprintln!(
                                            "[aokie-plugin] agent sentence (+{:?}): {}",
                                            t0.elapsed(),
                                            content_for_log(&spoken_text)
                                        );
                                        let mut probe =
                                            ControlProbe::new(&control_rx, &mut pending_controls);
                                        let (aec_ref, brms) = if barge_in {
                                            (aec.as_mut(), Some(barge_rms))
                                        } else {
                                            (None, None)
                                        };
                                        // The caller started talking while the LLM was
                                        // still composing this sentence (gap capture,
                                        // ≥250 ms above the speech gate): yield instead
                                        // of speaking over them — their words are
                                        // already on the scratchpad.
                                        if barge_in
                                            && gap_start.map_or(false, |s| {
                                                overlap_capture.len().saturating_sub(s)
                                                    >= (sr as usize) / 4
                                            })
                                        {
                                            eprintln!(
                                                "[aokie-plugin] caller speaking as the next sentence arrived — yielding to them"
                                            );
                                            status.gap_yields.fetch_add(1, Ordering::Relaxed);
                                            barged = true;
                                            reply_cancel.store(true, Ordering::Relaxed);
                                            break 'pump;
                                        }
                                        // The span planner validates/strips any model
                                        // markers, slows + digit-expands details, and
                                        // applies per-span interrupt policy. The probe
                                        // lane rides along: a spoken "wait"/"stop" cuts
                                        // the sentence mid-playback.
                                        let lane_ref =
                                            if barge_in { Some(&mut reply_lane) } else { None };
                                        let planned = speak_planned(
                                            bt,
                                            &synth,
                                            &spoken_text,
                                            sr,
                                            aec_ref,
                                            brms,
                                            Some(&mut probe),
                                            &pace,
                                            protected_max_ms,
                                            lane_ref,
                                        );
                                        let out = planned.outcome;
                                        // A spoken floor command mid-sentence pauses the
                                        // dialogue IMMEDIATELY (the final transcript will
                                        // re-apply it — idempotent).
                                        if let Some(intent) = out.commanded {
                                            dialogue.apply(intent);
                                            if !silence_window.is_zero() {
                                                silence_timer = Some(SilenceTimer::new(
                                                    silence_window * 3,
                                                    Instant::now(),
                                                ));
                                            }
                                        }
                                        if !planned.text.trim().is_empty() {
                                            note_tts_outcome(&status, &out);
                                        }
                                        reply_dur += out.dur;
                                        // Truthful transcript (AOK-VOICE-001): record
                                        // only the spans that audibly PLAYED.
                                        if out.dur > Duration::ZERO {
                                            if !planned.played_text.is_empty() {
                                                spoken.push(planned.played_text.clone());
                                            }
                                            if !planned.sent_text.is_empty() {
                                                sent_spans.push(planned.sent_text.clone());
                                            }
                                        }
                                        if !barge_in {
                                            let plays_until = (t0 + reply_dur).max(Instant::now());
                                            mute_stt_until =
                                                Some(plays_until + Duration::from_millis(600));
                                        }
                                        // Scratchpad: keep whatever the caller said over
                                        // this sentence, even when it didn't barge.
                                        if !out.captured_speech.is_empty() {
                                            overlap_capture
                                                .extend_from_slice(&out.captured_speech);
                                            overlap_has_speech = true;
                                        }
                                        if let Some(action) = probe.action.take() {
                                            // Operator hangup/reject landed mid-SENTENCE
                                            // (chunk-granular, AOK-CTRL-001).
                                            perform_cancel_action(
                                                action, bt, &mut tracker, outbox, sink,
                                            );
                                            operator_ended = true;
                                            reply_cancel.store(true, Ordering::Relaxed);
                                            break 'pump;
                                        }
                                        if out.barged {
                                            if out.commanded.is_some() {
                                                status
                                                    .semantic_cuts
                                                    .fetch_add(1, Ordering::Relaxed);
                                            } else {
                                                status.barge_cuts.fetch_add(1, Ordering::Relaxed);
                                            }
                                            bt.flush_tx_audio(); // stop the queued tail now
                                            barged = true;
                                            reply_cancel.store(true, Ordering::Relaxed);
                                            break 'pump; // stop pulling from the LLM
                                        }
                                        // SENTENCE-BOUNDARY STEERING (the live
                                        // scratchpad): the caller said something
                                        // SUBSTANTIVE over that sentence — even below
                                        // the acoustic barge threshold. Yield here, at
                                        // a natural pause, so the next thing spoken
                                        // answers THEM instead of finishing a stale
                                        // paragraph. Backchannels and echo never steer.
                                        let bot_so_far = {
                                            // Echo comparison wants the SENT
                                            // text (§6.3) — echo returns from
                                            // audio that left us, estimated
                                            // heard or not.
                                            let mut b = sent_spans.join(" ");
                                            b.push(' ');
                                            b.push_str(&planned.text);
                                            b
                                        };
                                        if let Some(said) =
                                            reply_lane.substantive_content(&bot_so_far)
                                        {
                                            eprintln!(
                                                "[aokie-plugin] scratchpad steering: yielding at the sentence boundary to {}",
                                                content_for_log(&said)
                                            );
                                            status
                                                .boundary_yields
                                                .fetch_add(1, Ordering::Relaxed);
                                            bt.flush_tx_audio();
                                            barged = true;
                                            reply_cancel.store(true, Ordering::Relaxed);
                                            break 'pump;
                                        }
                                    }
                                    Ok(ReplyMsg::Done(res)) => {
                                        stream_outcome = Some(res);
                                        break 'pump;
                                    }
                                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                        // Named deadlines (the VOICE-001-deferred
                                        // per-read idle deadline lives here): a
                                        // reply that stops making progress is
                                        // abandoned and takes the dead-air path.
                                        let last = *reply_activity.lock().unwrap();
                                        if let Some(reason) = reply_deadline_exceeded(
                                            &REPLY_DEADLINES,
                                            started,
                                            last,
                                            Instant::now(),
                                        ) {
                                            reply_cancel.store(true, Ordering::Relaxed);
                                            stream_outcome = Some(Err(reason));
                                            break 'pump;
                                        }
                                    }
                                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                        stream_outcome = Some(Err(
                                            "the reply worker exited without a result".to_string(),
                                        ));
                                        break 'pump;
                                    }
                                }
                            }
                            // Early exits (barge / operator) have no stream result;
                            // their transcript comes from `spoken` via the cut path.
                            let outcome = stream_outcome.unwrap_or_else(|| Ok(String::new()));
                            if !barge_in {
                                // Cover audio still queued after the last chunk synthesized.
                                let plays_until = (t0 + reply_dur).max(Instant::now());
                                mute_stt_until = Some(plays_until + Duration::from_millis(800));
                            }
                            // VOICE-001: set below when this reply attempt left the
                            // caller in DEAD AIR — triggers the fail-safe after the match.
                            let mut dead_air_cause: Option<String> = None;
                            match outcome {
                                Ok(full) => {
                                    // Truthful transcript (audit AK-008 + sweep): a
                                    // reply cut short records what actually PLAYED,
                                    // annotated with WHY — the full generation
                                    // includes sentences the caller never heard, and
                                    // an operator hangup is not a caller interruption.
                                    let cut = if line_dead {
                                        Some(" [call dropped mid-reply]")
                                    } else if barged {
                                        Some(" [caller interrupted]")
                                    } else if operator_ended {
                                        Some(" [ended by the operator]")
                                    } else {
                                        None
                                    };
                                    // Marker fallbacks in the FULL generation, in case
                                    // a stream split hid one from the per-sentence
                                    // detection above.
                                    let (_, had) = strip_end_call_marker(&full);
                                    if had {
                                        hangup_requested = true;
                                    }
                                    if crate::speech_plan::has_wait_marker(&full) {
                                        wait_requested = true;
                                    }
                                    // The transcript records what audibly PLAYED
                                    // (span-planned, marker-free) — never the raw
                                    // generation, which may carry control markup
                                    // and sentences the caller never heard.
                                    let played = spoken.join(" ").trim().to_string();
                                    let heard = match cut {
                                        Some(tag) if !played.is_empty() => {
                                            format!("{played}{tag}")
                                        }
                                        _ => played.clone(),
                                    };
                                    // The nudge: keep the interrupted reply's
                                    // unspoken tail for the NEXT generation.
                                    if barged && !full.trim().is_empty() {
                                        let (clean0, _) = strip_end_call_marker(&full);
                                        let clean_full = crate::speech_plan::clean_text(
                                            &crate::speech_plan::plan_spans(
                                                &clean0,
                                                &pace,
                                                protected_max_ms,
                                            ),
                                        );
                                        let played_words = played.split_whitespace().count();
                                        let tail: Vec<&str> = clean_full
                                            .split_whitespace()
                                            .skip(played_words)
                                            .collect();
                                        if !tail.is_empty() {
                                            let mut t = tail.join(" ");
                                            t.truncate(240);
                                            last_cut_context = Some(t);
                                        }
                                    }
                                    // VOICE-001: nothing audible + no barge/operator
                                    // context = the caller is in DEAD AIR — an empty
                                    // generation or fully-silent synthesis both count.
                                    // A dead LINE is not dead air (no channel left to
                                    // apologise on) — and neither is INTENTIONAL
                                    // silence: a [[WAIT]] reply means the model chose
                                    // to leave the caller their thinking room.
                                    if !line_dead
                                        && !wait_requested
                                        && reply_left_dead_air(
                                            reply_dur > Duration::ZERO,
                                            barged,
                                            operator_ended,
                                        )
                                    {
                                        dead_air_cause = Some(if heard.is_empty() {
                                            "the assistant produced an empty reply".to_string()
                                        } else {
                                            "speech synthesis produced no audio for the whole reply"
                                                .to_string()
                                        });
                                    }
                                    // Truthful transcript (AOK-VOICE-001): an
                                    // un-cut reply whose synthesis produced no
                                    // audio AT ALL was never heard — record
                                    // nothing instead of the full generation.
                                    if !heard.is_empty() && reply_dur > Duration::ZERO {
                                        // AOK-CTRL-001: structured per-turn delivery.
                                        let delivery = if line_dead {
                                            "error"
                                        } else if barged {
                                            "interrupted"
                                        } else if operator_ended {
                                            "operator_ended"
                                        } else {
                                            "complete"
                                        };
                                        history.push(
                                            serde_json::json!({ "role": "assistant", "content": heard }),
                                        );
                                        emit_turn_with_delivery(
                                            outbox,
                                            sink,
                                            &corr,
                                            turn_index,
                                            "bot",
                                            &heard,
                                            Some(delivery),
                                            Some(&aokie_core::events::iso8601_ago_ms(
                                                t0.elapsed().as_millis() as u64,
                                            )),
                                        );
                                        turn_index += 1;
                                        // Echo guard + replay compare/replay what
                                        // was SENT (§6.3) — the flushed-but-echoed
                                        // tail must still match; the transcript
                                        // above stays the conservative estimate.
                                        let sent_full =
                                            sent_spans.join(" ").trim().to_string();
                                        last_bot_reply = if sent_full.is_empty() {
                                            heard
                                        } else {
                                            match cut {
                                                Some(tag) => format!("{sent_full}{tag}"),
                                                None => sent_full.clone(),
                                            }
                                        };
                                        last_bot_speech = if sent_full.is_empty() {
                                            played
                                        } else {
                                            sent_full
                                        };
                                    } else if !heard.is_empty() {
                                        eprintln!(
                                            "[aokie-plugin] agent reply produced NO audio (TTS failed) — not recorded as a spoken turn"
                                        );
                                    }
                                    if !barged && overlap_capture.is_empty() {
                                        // Nothing was said over us: discard the
                                        // residue captured while we replied. With
                                        // overlap captured (barge or scratchpad),
                                        // KEEP the buffer — it's the caller's turn
                                        // in progress and is seeded below.
                                        stt_buf.clear();
                                        stt_had_speech = false;
                                        stt_silence = Duration::ZERO;
                                    } else if barged {
                                        eprintln!(
                                            "[aokie-plugin] caller barged in â€” reply cut short"
                                        );
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[aokie-plugin] agent reply failed: {e}");
                                    // Sentences that already PLAYED before the
                                    // failure are part of the call — record
                                    // them (audit AOK-VOICE-002/AOK-LLM-001).
                                    let heard = spoken.join(" ").trim().to_string();
                                    if !heard.is_empty() {
                                        let heard = format!("{heard} [reply cut short by an error]");
                                        history.push(
                                            serde_json::json!({ "role": "assistant", "content": heard }),
                                        );
                                        emit_turn_with_delivery(
                                            outbox,
                                            sink,
                                            &corr,
                                            turn_index,
                                            "bot",
                                            &heard,
                                            Some("error"),
                                            Some(&aokie_core::events::iso8601_ago_ms(
                                                t0.elapsed().as_millis() as u64,
                                            )),
                                        );
                                        turn_index += 1;
                                        last_bot_speech = heard
                                            .trim_end_matches(" [reply cut short by an error]")
                                            .to_string();
                                        last_bot_reply = heard;
                                    } else if !line_dead
                                        && reply_left_dead_air(false, barged, operator_ended)
                                    {
                                        // VOICE-001: total failure — the caller heard
                                        // nothing at all. Record it as a definitive
                                        // live LLM failure (health degrades; the
                                        // PROC-001 probe re-clears on recovery) and
                                        // take the fail-safe below. A PARTIAL reply
                                        // is transient: the caller heard something,
                                        // the next turn may still work.
                                        *status.llm_error.lock().unwrap() = Some(format!(
                                            "agent reply failed during a live call: {e}"
                                        ));
                                        dead_air_cause =
                                            Some(format!("the assistant failed to reply ({e})"));
                                    }
                                }
                            }
                            // AK-008 + scratchpad: EVERYTHING the caller said over
                            // the reply was captured (echo-cancelled) — barge or
                            // not. Prepend it to the utterance buffer so the STT
                            // hears the WHOLE turn ("zero four two one…", a quick
                            // "wait" that never tripped the barge, a "yeah" spoken
                            // over a sentence). Overlapped speech is never lost.
                            if !overlap_capture.is_empty() {
                                if !barged {
                                    eprintln!(
                                        "[aokie-plugin] scratchpad: captured {}ms of overlapped caller speech (no barge) — transcribing",
                                        overlap_capture.len() * 1000 / (sr.max(1) as usize)
                                    );
                                }
                                let mut seeded =
                                    crate::voice::to_f32_16k(&overlap_capture, sr as u32);
                                seeded.extend_from_slice(&stt_buf);
                                stt_buf = seeded;
                                stt_had_speech = true;
                                stt_silence = Duration::ZERO;
                                turn_overlapped = true;
                                turn_overlap_at = Some(aokie_core::events::iso8601_ago_ms(
                                    (overlap_capture.len() * 1000 / (sr as usize).max(1)) as u64,
                                ));
                            }
                            // VOICE-001 fail-safe: the caller asked something and heard
                            // NOTHING — the responder is broken mid-call. Never leave
                            // them in dead air: apologise with the canned line
                            // (best-effort — TTS may be the broken half) and end the
                            // call cleanly. The hangup happens EVEN IF the fallback
                            // itself is silent: ending the call IS the safe outcome.
                            let mut ended_by_failsafe = false;
                            if let Some(cause) = dead_air_cause {
                                eprintln!(
                                    "[aokie-plugin] responder failed mid-call ({cause}) — speaking the fallback line and ending the call (VOICE-001)"
                                );
                                let fb_t0 = Instant::now();
                                // The fail-safe stays maximally simple: plain rate,
                                // no planning, no barge monitoring.
                                let out = tts_speak(
                                    bt,
                                    &synth,
                                    FALLBACK_LINE,
                                    sr,
                                    None,
                                    None,
                                    None,
                                    1.0,
                                    None,
                                    None,
                                );
                                note_tts_outcome(&status, &out);
                                if out.dur > Duration::ZERO {
                                    // Truthful transcript: the apology WAS heard.
                                    emit_turn_with_delivery(
                                        outbox,
                                        sink,
                                        &corr,
                                        turn_index,
                                        "bot",
                                        FALLBACK_LINE,
                                        Some("complete"),
                                        Some(&aokie_core::events::iso8601_ago_ms(
                                            fb_t0.elapsed().as_millis() as u64,
                                        )),
                                    );
                                    turn_index += 1;
                                    // AOK-CTRL-001: drain the QUEUED apology before
                                    // CHUP — computed from what was actually queued
                                    // (the blind 900 ms cut a long apology short).
                                    let wait =
                                        playout_drain_wait(fb_t0, out.dur, Instant::now());
                                    if !wait.is_zero() {
                                        std::thread::sleep(wait);
                                    }
                                } else {
                                    eprintln!(
                                        "[aokie-plugin] fallback line also produced no audio — hanging up without it"
                                    );
                                }
                                tracker.note_intent(
                                    crate::call_session::TerminationIntent::AgentHangup,
                                );
                                match bt.hangup() {
                                    Ok(()) => eprintln!(
                                        "[aokie-plugin] fail-safe hangup complete (AT+CHUP)"
                                    ),
                                    Err(e) => {
                                        eprintln!("[aokie-plugin] fail-safe hangup failed: {e}")
                                    }
                                }
                                ended_by_failsafe = true;
                            }
                            // Agent-initiated hangup (AOK-CTRL-001): the end-call
                            // marker is only a REQUEST — the pure policy validates
                            // it against what actually happened (barge, operator
                            // action, fail-safe, farewell audibility) and computes
                            // the farewell's remaining playout drain, replacing the
                            // old fixed-delay hangup that could cut a goodbye short
                            // or fire before one was proven audible.
                            let verdict = if line_dead {
                                // No link left to hang up on — the outer loop's
                                // event drain runs the real teardown.
                                HangupVerdict::Skip("the call's audio link is gone")
                            } else {
                                agent_hangup_verdict(
                                    hangup_requested,
                                    barged,
                                    operator_ended,
                                    ended_by_failsafe,
                                    reply_dur > Duration::ZERO,
                                    // Any question in what actually PLAYED means
                                    // the model expects an answer — never hang up
                                    // on the caller mid-question.
                                    spoken.iter().any(|s| s.contains('?')),
                                    t0,
                                    reply_dur,
                                    Instant::now(),
                                )
                            };
                            match verdict {
                                HangupVerdict::Proceed { wait } => {
                                    if !wait.is_zero() {
                                        std::thread::sleep(wait);
                                    }
                                    tracker.note_intent(
                                        crate::call_session::TerminationIntent::AgentHangup,
                                    );
                                    match bt.hangup() {
                                        Ok(()) => eprintln!(
                                            "[aokie-plugin] agent finalized the call — hung up (AT+CHUP)"
                                        ),
                                        Err(e) => {
                                            eprintln!("[aokie-plugin] agent hangup failed: {e}");
                                            emit_control_failed(
                                                outbox,
                                                sink,
                                                &tracker,
                                                "agent.hangup",
                                                None,
                                                &e,
                                            );
                                        }
                                    }
                                }
                                HangupVerdict::Skip(reason) => {
                                    if hangup_requested {
                                        eprintln!(
                                            "[aokie-plugin] agent hangup request skipped: {reason}"
                                        );
                                    }
                                }
                            }
                            // AOK-CTRL-001: a finished reply attempt (audible or
                            // not) is conversational activity — the max-silence
                            // window measures from here.
                            if let Some(t) = silence_timer.as_mut() {
                                t.note_activity(Instant::now());
                            }
                            // The model chose intentional silence ([[WAIT]]): the
                            // floor stays with the caller — hold the pause state
                            // and stretch the silence watchdog so a deliberately
                            // quiet caller isn't nagged mid-thought.
                            if wait_requested && !barged && !operator_ended && !line_dead {
                                eprintln!(
                                    "[aokie-plugin] agent chose to wait silently ([[WAIT]]) — the caller has the floor"
                                );
                                dialogue.apply(crate::duplex::CallerIntent::Pause);
                                if !silence_window.is_zero() {
                                    silence_timer =
                                        Some(SilenceTimer::new(silence_window * 3, Instant::now()));
                                }
                            }
                        }
                    }
                }
            }

            // AOK-CTRL-001: call-level max-silence watchdog (agent mode). A
            // live, answered call where NEITHER side has produced audio for a
            // whole window gets a check-in prompt; a second silent window gets
            // a polite goodbye and a clean hangup — a dead line never holds
            // the phone open indefinitely.
            if agent_enabled && tracker.current().is_some_and(|s| s.is_active()) {
                let sr = bt.get_sample_rate();
                let action = if sr > 0 {
                    silence_timer.as_mut().and_then(|t| t.check(Instant::now()))
                } else {
                    None
                };
                match action {
                    Some(SilenceAction::Prompt) => {
                        idle = false;
                        eprintln!(
                            "[aokie-plugin] max-silence: no activity for {}s — checking in with the caller",
                            silence_window.as_secs()
                        );
                        let mut probe = ControlProbe::new(&control_rx, &mut pending_controls);
                        let (aec_ref, brms) = if barge_in {
                            (aec.as_mut(), Some(barge_rms))
                        } else {
                            (None, None)
                        };
                        let check_started = Instant::now();
                        let out = tts_speak(
                            bt,
                            &synth,
                            SILENCE_CHECK_LINE,
                            sr,
                            aec_ref,
                            brms,
                            Some(&mut probe),
                            pace.base(),
                            None,
                            None,
                        );
                        note_tts_outcome(&status, &out);
                        if barge_in {
                            if out.barged {
                                bt.flush_tx_audio();
                                // The caller spoke over the prompt — that IS activity.
                                if let Some(t) = silence_timer.as_mut() {
                                    t.note_activity(Instant::now());
                                }
                            }
                            // Scratchpad: anything said over the prompt (barge or
                            // not) seeds the caller's next turn.
                            if !out.captured_speech.is_empty() {
                                stt_buf =
                                    crate::voice::to_f32_16k(&out.captured_speech, sr as u32);
                                stt_had_speech = true;
                                stt_silence = Duration::ZERO;
                                turn_overlapped = true;
                                turn_overlap_at = Some(aokie_core::events::iso8601_ago_ms(
                                    (out.captured_speech.len() * 1000 / (sr as usize).max(1))
                                        as u64,
                                ));
                            }
                        } else {
                            mute_stt_until =
                                Some(Instant::now() + out.dur + Duration::from_millis(400));
                        }
                        if out.dur > Duration::ZERO {
                            if let Some(corr) = tracker.call_id().map(str::to_string) {
                                let delivery = if out.barged {
                                    "interrupted"
                                } else if out.cancelled {
                                    "operator_ended"
                                } else {
                                    "complete"
                                };
                                emit_turn_with_delivery(
                                    outbox,
                                    sink,
                                    &corr,
                                    turn_index,
                                    "bot",
                                    SILENCE_CHECK_LINE,
                                    Some(delivery),
                                    Some(&aokie_core::events::iso8601_ago_ms(
                                        check_started.elapsed().as_millis() as u64,
                                    )),
                                );
                                turn_index += 1;
                            }
                            history.push(serde_json::json!({
                                "role": "assistant",
                                "content": SILENCE_CHECK_LINE,
                            }));
                            last_bot_reply = SILENCE_CHECK_LINE.to_string();
                        }
                        if let Some(action) = probe.action.take() {
                            perform_cancel_action(action, bt, &mut tracker, outbox, sink);
                        }
                    }
                    Some(SilenceAction::HangUp) => {
                        idle = false;
                        eprintln!(
                            "[aokie-plugin] max-silence: still nothing after the check-in — saying goodbye and ending the call"
                        );
                        let mut probe = ControlProbe::new(&control_rx, &mut pending_controls);
                        let gb_t0 = Instant::now();
                        let out = tts_speak(
                            bt,
                            &synth,
                            SILENCE_GOODBYE_LINE,
                            sr,
                            None,
                            None,
                            Some(&mut probe),
                            pace.base(),
                            None,
                            None,
                        );
                        note_tts_outcome(&status, &out);
                        if out.barged {
                            // The caller came back at the last moment — keep the call.
                            bt.flush_tx_audio();
                            if let Some(t) = silence_timer.as_mut() {
                                t.note_activity(Instant::now());
                            }
                        } else if let Some(action) = probe.action.take() {
                            // An operator action owns the ending instead.
                            perform_cancel_action(action, bt, &mut tracker, outbox, sink);
                        } else {
                            if out.dur > Duration::ZERO {
                                if let Some(corr) = tracker.call_id().map(str::to_string) {
                                    emit_turn_with_delivery(
                                        outbox,
                                        sink,
                                        &corr,
                                        turn_index,
                                        "bot",
                                        SILENCE_GOODBYE_LINE,
                                        Some("complete"),
                                        Some(&aokie_core::events::iso8601_ago_ms(
                                            gb_t0.elapsed().as_millis() as u64,
                                        )),
                                    );
                                    turn_index += 1;
                                }
                                let wait = playout_drain_wait(gb_t0, out.dur, Instant::now());
                                if !wait.is_zero() {
                                    std::thread::sleep(wait);
                                }
                            }
                            tracker.note_intent(
                                crate::call_session::TerminationIntent::AgentHangup,
                            );
                            match bt.hangup() {
                                Ok(()) => eprintln!(
                                    "[aokie-plugin] max-silence hangup complete (AT+CHUP)"
                                ),
                                Err(e) => {
                                    eprintln!("[aokie-plugin] max-silence hangup failed: {e}");
                                    emit_control_failed(
                                        outbox,
                                        sink,
                                        &tracker,
                                        "agent.hangup",
                                        None,
                                        &e,
                                    );
                                }
                            }
                            silence_timer = None;
                        }
                    }
                    None => {}
                }
            }
        }

        loop {
            // Controls deferred by the mid-reply poll (audit AK-003) run first,
            // in arrival order, before anything newly queued.
            let next = match pending_controls.pop_front() {
                Some(c) => Ok(c),
                None => control_rx.try_recv(),
            };
            match next {
                Ok(RadioControl::Answer { op }) => {
                    if let Err(e) = bt.answer_call() {
                        eprintln!("[aokie-plugin] radio answer failed: {e}");
                        // AOK-CTRL-001: the command result only said "accepted" —
                        // this is the authoritative failure record for it.
                        emit_control_failed(outbox, sink, &tracker, "call.answer", op.as_deref(), &e);
                    }
                }
                Ok(RadioControl::Reject { op }) => {
                    // Record WHY before the phone acts, so the eventual
                    // CallTerminated reads outcome "rejected", never "missed"
                    // (audit AK-001/AK-01).
                    tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
                    if let Err(e) = bt.reject_call() {
                        eprintln!("[aokie-plugin] radio reject failed: {e}");
                        emit_control_failed(outbox, sink, &tracker, "call.reject", op.as_deref(), &e);
                    }
                }
                Ok(RadioControl::Hangup { op }) => {
                    tracker.note_intent(crate::call_session::TerminationIntent::OperatorHangup);
                    if let Err(e) = bt.hangup() {
                        eprintln!("[aokie-plugin] radio hangup failed: {e}");
                        emit_control_failed(outbox, sink, &tracker, "call.hangup", op.as_deref(), &e);
                    }
                }
                Ok(RadioControl::SendSms { to, body }) => {
                    if let Err(e) = bt.send_sms(to, body, None) {
                        emit(
                            outbox,
                            sink,
                            aokie_core::events::aokie_event_occurrence(
                                crate::contract::events::HARDWARE_ERROR,
                                "radio",
                                &aokie_core::events::occurrence_id(),
                                json!({"message": format!("send_sms failed: {e}")}),
                            ),
                        );
                    }
                }
                Ok(RadioControl::Speak { text, op }) => {
                    #[cfg(not(feature = "voice"))]
                    let _ = &op;
                    #[cfg(feature = "voice")]
                    if agent_enabled {
                        // Belt-and-suspenders: the connector now REFUSES
                        // operatorSpeak while the agent owns replies
                        // (AOK-CTRL-001), so this only catches a request that
                        // raced a radio restart. Never spoken (the caller must
                        // not be answered twice) — and never silently either:
                        // the accepted command gets its authoritative failure.
                        eprintln!(
                            "[aokie-plugin] dropping operatorSpeak (agent owns replies): {}",
                            content_for_log(&text)
                        );
                        emit(
                            outbox,
                            sink,
                            aokie_core::events::aokie_event_occurrence(
                                crate::contract::events::HARDWARE_ERROR,
                                tracker.call_id().unwrap_or("radio"),
                                &aokie_core::events::occurrence_id(),
                                json!({
                                    "message": "call.operatorSpeak was dropped: the in-plugin AI receptionist owns replies on this install",
                                    "code": "speak_failed",
                                    "action": "call.operatorSpeak",
                                    "operationId": op,
                                }),
                            ),
                        );
                    } else {
                        let sr = bt.get_sample_rate();
                        // AOK-CTRL-001: a hangup/reject queued behind this speak
                        // cuts it at chunk granularity. Flow/operator speech runs
                        // through the SAME span planner as the built-in agent —
                        // markers ([[slow]]/[[rate=…]]/[[important]]) are
                        // validated + clamped identically, digit runs slow down
                        // identically: the coordinator doesn't care where the
                        // words came from.
                        let mut probe = ControlProbe::new(&control_rx, &mut pending_controls);
                        let op_started = Instant::now();
                        let planned = speak_planned(
                            bt,
                            &synth,
                            &text,
                            sr,
                            None,
                            None,
                            Some(&mut probe),
                            &pace,
                            protected_max_ms,
                            None,
                        );
                        let out = planned.outcome;
                        if sr > 0 && !planned.text.trim().is_empty() {
                            note_tts_outcome(&status, &out);
                        }
                        mute_stt_until =
                            Some(Instant::now() + out.dur + Duration::from_millis(400));
                        stt_buf.clear();
                        stt_had_speech = false;
                        stt_silence = Duration::ZERO;
                        // Truthful transcript (audit AOK-VOICE-002): record a
                        // bot turn ONLY when synthesis actually produced audio
                        // for the caller — and only the spans that played.
                        if out.dur > Duration::ZERO && !planned.played_text.is_empty() {
                            if let Some(corr) = tracker.call_id().map(str::to_string) {
                                let delivery = if out.cancelled {
                                    "operator_ended"
                                } else {
                                    "complete"
                                };
                                emit_turn_with_delivery(
                                    outbox,
                                    sink,
                                    &corr,
                                    turn_index,
                                    "bot",
                                    &planned.played_text,
                                    Some(delivery),
                                    Some(&aokie_core::events::iso8601_ago_ms(
                                        op_started.elapsed().as_millis() as u64,
                                    )),
                                );
                                turn_index += 1;
                            }
                            // Spoken audio is conversational activity.
                            if let Some(t) = silence_timer.as_mut() {
                                t.note_activity(Instant::now());
                            }
                        } else if !out.cancelled && sr > 0 && !planned.text.trim().is_empty() {
                            eprintln!(
                                "[aokie-plugin] operatorSpeak produced NO audio (TTS failed) — not recorded as a spoken turn: {}",
                                content_for_log(&text)
                            );
                            // AOK-CTRL-001: the accepted command's authoritative
                            // failure — the text was NOT spoken to the caller.
                            emit(
                                outbox,
                                sink,
                                aokie_core::events::aokie_event_occurrence(
                                    crate::contract::events::HARDWARE_ERROR,
                                    tracker.call_id().unwrap_or("radio"),
                                    &aokie_core::events::occurrence_id(),
                                    json!({
                                        "message": "call.operatorSpeak produced no audio (TTS failure) — the text was NOT spoken to the caller",
                                        "code": "speak_failed",
                                        "action": "call.operatorSpeak",
                                        "operationId": op,
                                    }),
                                ),
                            );
                        }
                        if let Some(action) = probe.action.take() {
                            perform_cancel_action(action, bt, &mut tracker, outbox, sink);
                        }
                    }
                    #[cfg(not(feature = "voice"))]
                    eprintln!(
                        "[aokie-plugin] operatorSpeak ({} chars) â€” voice feature not built",
                        text.chars().count()
                    );
                }
                Ok(RadioControl::ConfigureCallAgent {
                    call_id,
                    persona,
                    greeting,
                }) => {
                    #[cfg(feature = "voice")]
                    {
                        // Apply only to the CURRENT call — a push that raced
                        // the call's end dies here instead of configuring the
                        // next caller (the point of call-scoped config).
                        if tracker.current().is_some_and(|s| s.id == call_id) {
                            let persona = persona.filter(|p| !p.trim().is_empty());
                            let greeting = greeting.filter(|s| !s.trim().is_empty());
                            eprintln!(
                                "[aokie-plugin] call-scoped agent config for {call_id} (persona {}, greeting {})",
                                persona.is_some(),
                                greeting.is_some()
                            );
                            call_agent_overlay = Some(CallAgentOverlay {
                                call_id,
                                persona,
                                greeting,
                            });
                        } else {
                            eprintln!(
                                "[aokie-plugin] call-scoped agent config DROPPED — {call_id} is not the current call"
                            );
                        }
                    }
                    #[cfg(not(feature = "voice"))]
                    let _ = (call_id, persona, greeting);
                }
                Ok(RadioControl::Configure {
                    persona,
                    greeting: g,
                    voice,
                    model,
                    endpoint,
                    stt_endpoint,
                    tts_endpoint,
                }) => {
                    // Live-reconfigure the agent from a flow / settings.set push. Each
                    // field is Some only when it changed. Greeting applies to the NEXT
                    // call; persona/voice/model take effect on the next caller turn.
                    #[cfg(feature = "voice")]
                    {
                        if let Some(g) = g {
                            // Blank = default, never silence (see DEFAULT_GREETING).
                            greeting = if g.trim().is_empty() {
                                Some(DEFAULT_GREETING.to_string())
                            } else {
                                Some(g)
                            };
                        }
                        if let Some(p) = persona {
                            if !p.trim().is_empty() {
                                agent_persona = p;
                            }
                        }
                        if let Some(v) = voice {
                            std::env::set_var("AOKIE_TTS_VOICE", v.trim());
                        }
                        let mut client_stale = false;
                        if let Some(m) = model {
                            let m = m.trim().to_string();
                            let new = if m.is_empty() { None } else { Some(m) };
                            if new != agent_model {
                                agent_model = new;
                                client_stale = true;
                            }
                        }
                        if let Some(e) = endpoint {
                            let e = e.trim().to_string();
                            let new = if e.is_empty() { None } else { Some(e) };
                            let mut current = agent_endpoint.lock().unwrap();
                            if new != *current {
                                *current = new;
                                client_stale = true;
                            }
                        }
                        if client_stale {
                            // Force a reconnect with the new endpoint/model next turn.
                            agent_client = None;
                        }
                        if let Some(e) = stt_endpoint {
                            let _ = stt_tx.send(SttWork::Configure { endpoint: Some(e) });
                        }
                        if let Some(e) = tts_endpoint {
                            synth.configure(Some(e));
                        }
                        eprintln!("[aokie-plugin] agent reconfigured (persona/greeting/voice/model/endpoints)");
                    }
                    #[cfg(not(feature = "voice"))]
                    {
                        let _ = (
                            persona,
                            g,
                            voice,
                            model,
                            endpoint,
                            stt_endpoint,
                            tts_endpoint,
                        );
                    }
                }
                Ok(RadioControl::StartPairing { seconds }) => {
                    // AOK-BT-001: make the radio discoverable for a bounded window.
                    bt.open_pairing_window(seconds);
                    eprintln!("[aokie-plugin] pairing window opened for {seconds}s");
                }
                Ok(RadioControl::StopPairing) => {
                    bt.close_pairing_window();
                    eprintln!("[aokie-plugin] pairing window closed");
                }
                Ok(RadioControl::RemovePaired { address, reply }) => {
                    let _ = reply.send(bt.remove_paired(&address));
                }
                Ok(RadioControl::Disconnect { address, reply }) => {
                    let _ = reply.send(bt.disconnect(&address));
                }
                Ok(RadioControl::Connect { address, reply }) => {
                    let _ = reply.send(bt.connect(&address));
                }
                Ok(RadioControl::ConfirmPairing {
                    address,
                    accept,
                    reply,
                }) => {
                    let _ = reply.send(bt.confirm_pairing(&address, accept));
                }
                Ok(RadioControl::ListBonded { reply }) => {
                    let _ = reply.send(bt.bonded_devices());
                }
                Ok(RadioControl::ConnectedName { reply }) => {
                    let _ = reply.send(bt.connected_name());
                }
                Ok(RadioControl::Shutdown) => return,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
            idle = false;
        }

        if idle {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

/// Map one `BluetoothEvent` to the `aokie.*` contract: drive the call-session
/// state machine (audit AK-001) + mirror shared status. Available on all
/// targets (it only touches `BluetoothEvent`, which is not Windows-gated) so
/// the whole lifecycle stays unit-testable.
fn handle_event(
    ev: aokie_dongle::bluetooth::BluetoothEvent,
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
) {
    use aokie_core::events::{aokie_event, aokie_event_occurrence, now_iso8601, occurrence_id};
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    // Radio-lifecycle incidents share the `radio` correlation but are DISTINCT
    // occurrences (replug → a second dongle.ready, reconnect → a second
    // phone.connected, every hardware error is its own incident): each mints a
    // fresh occurrence id HERE — once, at detection — so repeats aren't
    // silently dropped by key collision while replays of one occurrence keep
    // one key (audit AOK-EVENT-001).
    match ev {
        E::Initialized(addr) => {
            status.initialized.store(true, Ordering::Relaxed);
            *status.local_address.lock().unwrap() = Some(addr.clone());
            *status.last_error.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::DONGLE_READY,
                    "radio",
                    &occurrence_id(),
                    json!({"address": addr, "source": "radio"}),
                ),
            );
        }
        E::DeviceConnected(addr) => {
            status.connected.store(true, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = Some(addr.clone());
            // A working phone link supersedes whatever transient error last
            // landed (a stalled reconnect attempt, a keepalive hiccup): the
            // slot otherwise held health "degraded — radio error: …" FOREVER
            // with no way to clear it (live report 2026-07-13). The full
            // history stays in the Hardware Events records + desktop log;
            // this slot means "why the line is not working RIGHT NOW".
            *status.last_error.lock().unwrap() = None;
            {
                let mut paired = status.paired.lock().unwrap();
                if !paired.iter().any(|d| d.address == addr) {
                    paired.push(PairedDevice {
                        address: addr.clone(),
                        name: "Paired phone".to_string(),
                    });
                }
            }
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::PHONE_CONNECTED,
                    "radio",
                    &occurrence_id(),
                    json!({"address": addr}),
                ),
            );
        }
        E::DeviceDisconnected(addr) => {
            // A live session cannot outlive its radio link (audit
            // AOK-LIF-003): synthesize the terminal outcome NOW — otherwise
            // call.current and the operator UI stay "live" on hardware that
            // is gone. A late real CallTerminated lands on an idle tracker
            // and is a no-op (no duplicate terminal event).
            if tracker.current().is_some() {
                flush_incoming_if_pending(tracker, outbox, sink);
                status.call_active.store(false, Ordering::Relaxed);
                tracker.note_intent(crate::call_session::TerminationIntent::DeviceLost);
                if let Some(ended) = tracker.terminate() {
                    eprintln!(
                        "[aokie-plugin] phone link lost during call {} — synthesized termination (outcome {})",
                        ended.id, ended.outcome
                    );
                    emit_call_ended(&ended, status.config_version.load(Ordering::Relaxed), outbox, sink);
                }
                *status.current_caller.lock().unwrap() = None;
                *status.current_call_id.lock().unwrap() = None;
                *status.call_started_at.lock().unwrap() = None;
            }
            status.connected.store(false, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::PHONE_DISCONNECTED,
                    "radio",
                    &occurrence_id(),
                    json!({"address": addr}),
                ),
            );
        }
        E::CallIncoming => {
            // Some phones emit the ring / callsetup indicator more than once
            // for a single call — the tracker starts a NEW session only when
            // idle, so incoming + greeting fire exactly once per call.
            let id = format!("call_{}", uuid::Uuid::new_v4().simple());
            if let Some(s) = tracker.ring(id, now_iso8601()) {
                // Shared call identity: `call.current` recovers a live call
                // from these after a browser refresh (audit C-02), and call
                // controls verify their `callId` against it (audit C-01).
                *status.current_caller.lock().unwrap() = None;
                *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                *status.call_started_at.lock().unwrap() = Some(s.started_at_iso.clone());
            }
        }
        E::CallerId(num) => {
            // The FIRST time this call learns a (non-empty) number, announce
            // it: with instant auto-answer the ringing-phase +CLIP usually
            // loses the race, so `call.incoming` often went out with an empty
            // `from` — this event (fed by +CLIP or the AT+CLCC rescue) is
            // what lets flows personalize the LIVE call (greet a matched
            // customer by name). Emitted at most once per call: +CLIP repeats
            // per ring and +CLCC answers too, and the idempotency key
            // (corr + `caller_id` step) must be minted exactly once.
            let newly_known = !num.trim().is_empty()
                && tracker
                    .current()
                    .is_some_and(|s| s.caller_id.as_deref().unwrap_or("").is_empty());
            tracker.caller_id(num.clone());
            if newly_known {
                // Lifecycle order (AOK-LIF-001): the number is known now, so
                // the held `incoming` can flush WITH it — and must go first.
                flush_incoming_if_pending(tracker, outbox, sink);
                if let Some(corr) = tracker.call_id() {
                    emit(
                        outbox,
                        sink,
                        aokie_event(
                            crate::contract::events::CALL_CALLER_ID,
                            corr,
                            json!({"callId": corr, "from": num.clone(), "at": now_iso8601()}),
                        ),
                    );
                }
            }
            *status.current_caller.lock().unwrap() = Some(num);
        }
        E::CallRinging => {
            flush_incoming_if_pending(tracker, outbox, sink);
            if let Some(corr) = tracker.call_id() {
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        crate::contract::events::CALL_RINGING,
                        corr,
                        json!({"at": now_iso8601()}),
                    ),
                );
            }
        }
        E::CallAnswered => {
            // AOK-CTRL-001 recovery: an answer with NO tracked session means
            // an earlier indicator was misread as a terminate (or events were
            // lost) while the phone call is genuinely up — without a session
            // every audio frame is dropped and the receptionist goes deaf on
            // a LIVE call (observed 2026-07-13; the HFP held-verdict fix
            // prevents the known ordering, this catches any other). Rebuild a
            // session so the call is heard; the greeting replays, which also
            // tells the caller the line reset. Gated on the phone still being
            // CONNECTED: a stale answered queued behind a device-loss
            // termination must never build a phantom session on a dead link
            // (AOK-LIF-003).
            if tracker.current().is_none() && status.connected.load(Ordering::Relaxed) {
                let id = format!("call_{}", uuid::Uuid::new_v4().simple());
                eprintln!(
                    "[aokie-plugin] call ANSWERED with no tracked session — recovering as {id} (a terminate was misread or events were lost)"
                );
                if let Some(s) = tracker.ring(id, now_iso8601()) {
                    *status.current_caller.lock().unwrap() = None;
                    *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                    *status.call_started_at.lock().unwrap() = Some(s.started_at_iso.clone());
                }
            }
            // Lifecycle order (audit AOK-LIF-001): incoming ALWAYS precedes
            // answered — even when the caller-ID hold hasn't elapsed yet.
            flush_incoming_if_pending(tracker, outbox, sink);
            tracker.answered();
            if let Some(corr) = tracker.call_id() {
                // Only a TRACKED call may read as active — an orphaned
                // answer that wasn't recovered (dead link) must not leave
                // `call_active` true with no session behind it.
                status.call_active.store(true, Ordering::Relaxed);
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        crate::contract::events::CALL_ANSWERED,
                        corr,
                        json!({"at": now_iso8601()}),
                    ),
                );
            }
        }
        E::CallTerminated => {
            // Even an instantly-abandoned ring gets its incoming record
            // before the terminal event (audit AOK-LIF-001).
            flush_incoming_if_pending(tracker, outbox, sink);
            status.call_active.store(false, Ordering::Relaxed);
            if let Some(ended) = tracker.terminate() {
                emit_call_ended(&ended, status.config_version.load(Ordering::Relaxed), outbox, sink);
            }
            *status.current_caller.lock().unwrap() = None;
            *status.current_call_id.lock().unwrap() = None;
            *status.call_started_at.lock().unwrap() = None;
        }
        E::AudioConnected { codec, sample_rate, armed } => {
            flush_incoming_if_pending(tracker, outbox, sink);
            // SCO can drop and re-arm repeatedly within ONE call, so even with
            // a call correlation these are per-incident occurrences.
            let corr = tracker.call_id().unwrap_or("radio").to_string();
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::CALL_AUDIO_CONNECTED,
                    &corr,
                    &occurrence_id(),
                    json!({"codec": codec, "sampleRate": sample_rate, "armed": armed}),
                ),
            );
            // Silent SCO must never LOOK healthy (audit AOK-HW-001): the
            // link is up but the iso pipes didn't arm — record it where the
            // Device Setup console shows it, with the concrete recovery.
            if !armed {
                emit(
                    outbox,
                    sink,
                    aokie_event_occurrence(
                        crate::contract::events::HARDWARE_ERROR,
                        &corr,
                        &occurrence_id(),
                        json!({
                            "message": "Call audio failed to arm (SCO alternate setting) — this call will be SILENT both ways. Hang up, unplug and replug the dongle, then take the next call.",
                            "code": "sco_unarmed",
                        }),
                    ),
                );
            }
        }
        E::AudioDisconnected => {
            flush_incoming_if_pending(tracker, outbox, sink);
            let corr = tracker.call_id().unwrap_or("radio").to_string();
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::CALL_AUDIO_DISCONNECTED,
                    &corr,
                    &occurrence_id(),
                    json!({}),
                ),
            );
        }
        E::SmsReceived(p) => {
            // Stable inbound identity (audit AOK-EVENT-001): the MAP handle is
            // the AG's own stable id for this message on this phone, so a
            // re-fetch of the SAME message (MNS re-notification, reconnect
            // replay) dedupes instead of duplicating the record. Scope it by
            // device address — handles are only unique per phone. A phone
            // that sends no handle falls back to a fresh occurrence id (no
            // dedupe possible, matching the old behaviour).
            let corr = if p.handle.is_empty() {
                format!("sms_{}", occurrence_id())
            } else {
                let device = status
                    .connected_address
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default()
                    .replace(':', "");
                format!("sms_{device}_{}", p.handle)
            };
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_RECEIVED,
                    &corr,
                    json!({
                        "from": p.sender_phone,
                        "name": p.sender_name,
                        "body": p.body,
                        "handle": p.handle,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::SmsSent { recipient_phone } => {
            let message_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_SENT,
                    &message_id,
                    json!({"messageId": message_id, "to": recipient_phone, "at": now_iso8601()}),
                ),
            );
        }
        E::SmsSendFailed {
            recipient_phone,
            reason,
        } => {
            // The radio abandoned an outbound SMS (MAS PUT failed / aged out
            // across recovery cycles). Surface it truthfully — a queued send
            // that quietly evaporates is audit C-16's exact failure mode.
            let message_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_FAILED,
                    &message_id,
                    json!({
                        "messageId": message_id,
                        "to": recipient_phone,
                        "reason": reason,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::PairingConfirmRequired {
            address,
            numeric_value,
        } => {
            // PAIR-001: surface the held SSP numeric comparison so the Desktop
            // pairing UI can prompt the operator (phone.status carries the same
            // data for pollers). Not essential/outboxed — the prompt expires in
            // seconds, so replaying it after a host restart would be wrong.
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::PHONE_PAIRING_CONFIRM_REQUIRED,
                    "radio",
                    &occurrence_id(),
                    json!({
                        "address": address,
                        "numericValue": numeric_value,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::ContactsFetched(_) | E::MapNotificationsSubscribed => {
            // Phonebook / MNS-subscribe are diagnostic; nothing to surface yet.
        }
        E::Error(e) => {
            *status.last_error.lock().unwrap() = Some(e.clone());
            // Every hardware error is its own incident — without an occurrence
            // id the SECOND distinct error would collide with the first's key
            // and be silently dropped (essential/outboxed event!).
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::HARDWARE_ERROR,
                    "radio",
                    &occurrence_id(),
                    json!({"message": e}),
                ),
            );
        }
    }
}

// â”€â”€ Non-Windows: no radio â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[cfg(not(target_os = "windows"))]
pub fn spawn(
    _data_dir: std::path::PathBuf,
    _preferred_path: Option<String>,
    _auto_answer: bool,
    _answer_tone: bool,
    _reenumerate_hwid: Option<String>,
    _greeting: Option<String>,
    _ack_mode: bool,
) -> Result<RadioHandle, String> {
    Err("the Aokie radio is only supported on Windows (WinUSB)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AOK-CTRL-001: fake-clock deadline / silence / hangup-policy tests ──
    // Every decision function takes `now` (the clock seam): tests fabricate
    // instants by offsetting one base Instant — fully deterministic.

    #[cfg(feature = "voice")]
    use std::time::{Duration as D, Instant};

    /// The reply watchdog names WHICH deadline expired: first-activity (the
    /// endpoint accepted but never produced stream data), idle (mid-stream
    /// stall — the per-read deadline VOICE-001 deferred), or total.
    #[cfg(feature = "voice")]
    #[test]
    fn reply_deadlines_fire_by_phase_and_stay_quiet_on_progress() {
        let cfg = ReplyDeadlines {
            first_activity: D::from_secs(10),
            idle: D::from_secs(8),
            total: D::from_secs(60),
        };
        let t0 = Instant::now();

        // Healthy: fresh activity, inside every window.
        assert_eq!(
            reply_deadline_exceeded(&cfg, t0, Some(t0 + D::from_secs(29)), t0 + D::from_secs(30)),
            None
        );
        // No first token yet, but still inside the first-activity window.
        assert_eq!(reply_deadline_exceeded(&cfg, t0, None, t0 + D::from_secs(9)), None);
        // First-activity deadline.
        let msg = reply_deadline_exceeded(&cfg, t0, None, t0 + D::from_secs(10)).unwrap();
        assert!(msg.contains("first-activity"), "{msg}");
        // Idle (per-read) deadline: activity happened, then the stream stalled.
        let msg = reply_deadline_exceeded(
            &cfg,
            t0,
            Some(t0 + D::from_secs(5)),
            t0 + D::from_secs(13),
        )
        .unwrap();
        assert!(msg.contains("idle deadline"), "{msg}");
        // Total deadline wins even with fresh activity (a stream that trickles
        // forever must still end).
        let msg = reply_deadline_exceeded(
            &cfg,
            t0,
            Some(t0 + D::from_secs(59)),
            t0 + D::from_secs(60),
        )
        .unwrap();
        assert!(msg.contains("total deadline"), "{msg}");
    }

    /// The max-silence timer: first expiry prompts, a second silent window
    /// hangs up, any activity resets BOTH the window and the prompt state,
    /// and a zero window disables the timer entirely.
    #[cfg(feature = "voice")]
    #[test]
    fn silence_timer_prompts_then_hangs_up_and_activity_resets() {
        let t0 = Instant::now();
        let mut timer = SilenceTimer::new(D::from_secs(30), t0);

        assert_eq!(timer.check(t0 + D::from_secs(29)), None);
        assert_eq!(timer.check(t0 + D::from_secs(30)), Some(SilenceAction::Prompt));
        // The prompt restarted the window — not an instant hangup.
        assert_eq!(timer.check(t0 + D::from_secs(31)), None);
        assert_eq!(
            timer.check(t0 + D::from_secs(60)),
            Some(SilenceAction::HangUp)
        );

        // Activity after a prompt forgives it: the next expiry prompts again.
        let mut timer = SilenceTimer::new(D::from_secs(30), t0);
        assert_eq!(timer.check(t0 + D::from_secs(30)), Some(SilenceAction::Prompt));
        timer.note_activity(t0 + D::from_secs(40));
        assert_eq!(timer.check(t0 + D::from_secs(69)), None);
        assert_eq!(timer.check(t0 + D::from_secs(70)), Some(SilenceAction::Prompt));

        // Zero window = disabled.
        let mut off = SilenceTimer::new(D::ZERO, t0);
        assert_eq!(off.check(t0 + D::from_secs(3600)), None);
    }

    /// The agent-hangup POLICY: the LLM's marker is only a request — barge,
    /// operator ownership, the fail-safe, an unproven farewell and a farewell
    /// that ASKS A QUESTION all veto it; a valid request waits out the
    /// farewell's computed playout drain.
    #[cfg(feature = "voice")]
    #[test]
    fn agent_hangup_policy_vetoes_and_computes_the_drain() {
        let t0 = Instant::now();
        let dur = D::from_secs(2);

        // No request → skip.
        assert!(matches!(
            agent_hangup_verdict(false, false, false, false, true, false, t0, dur, t0),
            HangupVerdict::Skip(_)
        ));
        // Barge / operator / fail-safe veto.
        for (barged, operator, failsafe) in
            [(true, false, false), (false, true, false), (false, false, true)]
        {
            assert!(matches!(
                agent_hangup_verdict(true, barged, operator, failsafe, true, false, t0, dur, t0),
                HangupVerdict::Skip(_)
            ));
        }
        // Farewell never played → the dead-air fail-safe owns the ending.
        assert!(matches!(
            agent_hangup_verdict(true, false, false, false, false, false, t0, D::ZERO, t0),
            HangupVerdict::Skip(_)
        ));
        // Live report 2026-07-13: "Is there anything else I can help you
        // with? [[END_CALL]]" hung up on its own question — a farewell that
        // asks anything must WAIT for the answer instead.
        let verdict =
            agent_hangup_verdict(true, false, false, false, true, true, t0, dur, t0);
        let HangupVerdict::Skip(reason) = verdict else {
            panic!("a questioning farewell must not hang up");
        };
        assert!(reason.contains("question"), "reason names the cause: {reason}");
        // Valid: the wait is the REMAINING playout + margin (queued 2s, 1s
        // already elapsed → ~1.4s), bounded.
        let HangupVerdict::Proceed { wait } = agent_hangup_verdict(
            true,
            false,
            false,
            false,
            true,
            false,
            t0,
            dur,
            t0 + D::from_secs(1),
        ) else {
            panic!("expected Proceed");
        };
        assert_eq!(wait, D::from_millis(1400));
    }

    /// Drain math: remaining playout + margin, zero once already drained,
    /// capped for pathological durations.
    #[cfg(feature = "voice")]
    #[test]
    fn playout_drain_wait_is_remaining_playout_bounded() {
        let t0 = Instant::now();
        // 3s queued, 1s elapsed → 2s remaining + 400ms margin.
        assert_eq!(
            playout_drain_wait(t0, D::from_secs(3), t0 + D::from_secs(1)),
            D::from_millis(2400)
        );
        // Fully drained long ago → zero (no blind sleep).
        assert_eq!(
            playout_drain_wait(t0, D::from_secs(1), t0 + D::from_secs(10)),
            D::ZERO
        );
        // Pathological queue → capped.
        assert_eq!(
            playout_drain_wait(t0, D::from_secs(120), t0),
            D::from_secs(8)
        );
    }

    /// The control probe: hangup/reject stop playback and are recorded (sticky);
    /// every other control parks in arrival order for the main loop.
    #[cfg(feature = "voice")]
    #[test]
    fn control_probe_catches_urgent_actions_and_parks_the_rest() {
        let (tx, rx) = std::sync::mpsc::channel::<RadioControl>();
        let mut parked = std::collections::VecDeque::new();
        let mut probe = ControlProbe::new(&rx, &mut parked);

        assert!(!probe.poll(), "no controls yet");

        tx.send(RadioControl::StopPairing).unwrap();
        tx.send(RadioControl::Hangup {
            op: Some("op_1".into()),
        })
        .unwrap();
        tx.send(RadioControl::StartPairing { seconds: 30 }).unwrap();

        assert!(probe.poll(), "hangup must stop playback");
        assert_eq!(
            probe.action,
            Some(CancelAction::Hangup {
                op: Some("op_1".into())
            })
        );
        // Sticky once set, and the pre-hangup control was parked in order.
        assert!(probe.poll());
        drop(probe);
        assert!(matches!(parked.front(), Some(RadioControl::StopPairing)));

        // Reject is urgent too.
        let mut parked = std::collections::VecDeque::new();
        let mut probe = ControlProbe::new(&rx, &mut parked);
        // The StartPairing sent above is still queued — it parks first.
        tx.send(RadioControl::Reject { op: None }).unwrap();
        assert!(probe.poll());
        assert_eq!(probe.action, Some(CancelAction::Reject { op: None }));
        drop(probe);
        assert!(matches!(
            parked.front(),
            Some(RadioControl::StartPairing { seconds: 30 })
        ));
    }

    /// VOICE-001: the dead-air decision — the fail-safe (apologise + hang up)
    /// fires ONLY when nothing audibly played and nothing else explains the
    /// silence. A barge means the caller is talking; an operator action means
    /// a human owns the call; any audible sentence = transient, keep going.
    #[cfg(feature = "voice")]
    #[test]
    fn dead_air_fires_only_on_unexplained_total_silence() {
        assert!(reply_left_dead_air(false, false, false), "total silence = dead air");
        assert!(!reply_left_dead_air(true, false, false), "partial reply is transient");
        assert!(!reply_left_dead_air(false, true, false), "barge = caller talking");
        assert!(!reply_left_dead_air(false, false, true), "operator owns the call");
        assert!(!reply_left_dead_air(true, true, true));
        // The canned apology must be non-trivial speech, not a stub.
        assert!(FALLBACK_LINE.len() > 40 && FALLBACK_LINE.contains("sorry"));
    }

    /// The agent-hangup end-call marker must be stripped from spoken/recorded
    /// text (tolerant to small-model bracket/case variants) and its presence
    /// detected so the plugin knows to hang up after the goodbye.
    #[cfg(feature = "voice")]
    #[test]
    fn end_call_marker_stripping() {
        let (t, f) = strip_end_call_marker("Thanks, goodbye! [[END_CALL]]");
        assert_eq!(t, "Thanks, goodbye!");
        assert!(f);
        // bare + lowercase variant
        let (t, f) = strip_end_call_marker("See you soon. end_call");
        assert_eq!(t, "See you soon.");
        assert!(f);
        // single brackets, mixed case
        let (t, f) = strip_end_call_marker("Bye now [End_Call]");
        assert_eq!(t, "Bye now");
        assert!(f);
        // a marker-only sentence collapses to empty (nothing is spoken)
        let (t, f) = strip_end_call_marker("[[END_CALL]]");
        assert_eq!(t, "");
        assert!(f);
        // no marker: text is unchanged and the flag stays false
        let (t, f) = strip_end_call_marker("How else can I help?");
        assert_eq!(t, "How else can I help?");
        assert!(!f);
    }

    /// AK-008: the barge scan must CAPTURE the audio it inspects and remember
    /// where speech started, so the caller's words spoken over Aokie are
    /// prepended to their turn instead of being consumed by detection.
    #[cfg(all(target_os = "windows", feature = "voice"))]
    #[test]
    fn barge_scan_captures_audio_and_marks_speech_start() {
        let frame = 80usize; // 10 ms @ 8 kHz
        let mut speech_frames = 0u32;
        let mut captured: Vec<i16> = Vec::new();
        let mut speech_start: Option<usize> = None;

        // 1) Silence first: captured grows, no speech start, no trip.
        let silence = vec![0i16; frame * 5];
        let tripped = scan_barge_frames(
            &silence, frame, 350.0, 500.0, true, &mut speech_frames, 3, &mut captured,
            &mut speech_start,
        );
        assert!(!tripped);
        assert_eq!(captured.len(), frame * 5);
        assert_eq!(speech_start, None);
        assert_eq!(speech_frames, 0);

        // 2) Loud speech: start is marked at ITS offset (after the silence),
        //    and 3 sustained frames trip the barge.
        let loud = vec![8000i16; frame * 3];
        let tripped = scan_barge_frames(
            &loud, frame, 350.0, 500.0, true, &mut speech_frames, 3, &mut captured,
            &mut speech_start,
        );
        assert!(tripped);
        assert_eq!(speech_start, Some(frame * 5), "speech starts where the loud audio began");
        // The loud chunk was captured too — nothing was consumed by detection.
        assert_eq!(captured.len(), frame * 8);

        // 3) QUIET speech (above the capture gate, below the trip threshold —
        //    the AEC-suppressed-overlap band): marked + captured for the
        //    scratchpad, but never barges.
        let mut frames2 = 0u32;
        let mut cap2: Vec<i16> = Vec::new();
        let mut start2: Option<usize> = None;
        let quiet = vec![420i16; frame * 6];
        let tripped = scan_barge_frames(
            &quiet, frame, 350.0, 500.0, true, &mut frames2, 3, &mut cap2, &mut start2,
        );
        assert!(!tripped, "sub-trip speech never barges");
        assert_eq!(start2, Some(0), "but the scratchpad hears it");
    }

    /// Span interrupt policy: a Yield span stops the instant the barge trips;
    /// a FinishSpan span (phone number, [[important]] detail) keeps playing
    /// through its bounded extension and then stops; an urgent control
    /// (hangup/reject) stops everything regardless of policy.
    #[cfg(all(target_os = "windows", feature = "voice"))]
    #[test]
    fn playback_policy_yield_vs_finish_span() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        // Yield: barge = stop now.
        let mut p = TtsChunkPlayback::new(8000, None);
        assert!(!p.stop_playback_now(now), "nothing happened yet");
        p.barged = true;
        p.barged_at = Some(now);
        assert!(p.stop_playback_now(now), "yield stops on the trip");

        // FinishSpan: barge = keep going until the budget is spent.
        let mut p = TtsChunkPlayback::new(8000, Some(Duration::from_millis(1500)));
        p.barged = true;
        p.barged_at = Some(now);
        assert!(!p.stop_playback_now(now), "inside the finish budget");
        assert!(
            p.stop_playback_now(now + Duration::from_millis(1600)),
            "budget spent — yield"
        );

        // Cancelled (urgent control) always stops, policy notwithstanding.
        let mut p = TtsChunkPlayback::new(8000, Some(Duration::from_secs(5)));
        p.cancelled = true;
        assert!(p.stop_playback_now(now), "hangup/reject beats protection");

        // A spoken floor command (probe lane) also beats protection — the
        // caller's explicit "stop" cuts even an [[important]] span instantly.
        let mut p = TtsChunkPlayback::new(8000, Some(Duration::from_secs(5)));
        p.barged = true;
        p.barged_at = Some(now);
        assert!(!p.stop_playback_now(now), "ordinary overlap rides the budget");
        p.semantic = Some(crate::duplex::CallerIntent::StopSpeaking);
        assert!(p.stop_playback_now(now), "spoken command beats protection");
    }

    /// §6.3 delivery-truth v1: the audible-prefix estimator UNDERCLAIMS —
    /// duration-weighted against exact totals when synthesis finished,
    /// capped by a chars-per-second ceiling, floored to a word boundary.
    #[cfg(all(target_os = "windows", feature = "voice"))]
    #[test]
    fn audible_prefix_estimate_is_conservative() {
        let text = "Your appointment is on Thursday at ten in the morning";
        // Cut ~40% through a 4s span whose synthesis finished: claims a
        // word-floored prefix, never the whole sentence, flagged uncertain.
        let (prefix, uncertain) = estimate_audible_prefix(
            text,
            1.0,
            &CutEstimate { audible_ms: 1600, queued_ms: 1800, synthesized_ms: Some(4000) },
        );
        assert!(uncertain);
        assert!(text.starts_with(&prefix), "estimate must be a prefix");
        assert!(prefix.len() < text.len(), "a mid-span cut must not claim everything");
        assert!(!prefix.is_empty(), "1.6s of audio heard something");
        assert!(!prefix.ends_with(char::is_whitespace));
        // The prefix always ends on a WORD boundary of the original text.
        assert!(text[prefix.len()..].starts_with(' '), "must cut at a word boundary");

        // The cut landed after everything played out: the whole span was
        // plausibly heard — full text, certain.
        let (all, uncertain) = estimate_audible_prefix(
            text,
            1.0,
            &CutEstimate { audible_ms: 4000, queued_ms: 4000, synthesized_ms: Some(4000) },
        );
        assert_eq!(all, text);
        assert!(!uncertain);

        // Synthesis NOT finished (total unknown): the chars-per-second
        // ceiling alone drives it — 300ms at rate 1.0 is a couple of words
        // at most, never half the sentence.
        let (short, uncertain) = estimate_audible_prefix(
            text,
            1.0,
            &CutEstimate { audible_ms: 300, queued_ms: 500, synthesized_ms: None },
        );
        assert!(uncertain);
        assert!(short.len() <= 5, "300ms cannot claim more than ~4 chars, got {short:?}");

        // Nothing audible = nothing claimed.
        let (none, _) = estimate_audible_prefix(
            text,
            1.0,
            &CutEstimate { audible_ms: 0, queued_ms: 0, synthesized_ms: None },
        );
        assert!(none.is_empty());

        // A slower span rate lowers the ceiling proportionally.
        let (slow, _) = estimate_audible_prefix(
            text,
            0.5,
            &CutEstimate { audible_ms: 1000, queued_ms: 1200, synthesized_ms: None },
        );
        let (fast, _) = estimate_audible_prefix(
            text,
            1.0,
            &CutEstimate { audible_ms: 1000, queued_ms: 1200, synthesized_ms: None },
        );
        assert!(slow.len() <= fast.len());
    }

    /// Phase 2 pacing: the first chunk always goes (it starts the playout
    /// clock); after that the queued total may lead real playout by at most
    /// the configured lead — cancel latency is bounded by that lead.
    #[cfg(all(target_os = "windows", feature = "voice"))]
    #[test]
    fn playout_pacing_bounds_the_queue_lead() {
        use std::time::Duration;
        let lead = Duration::from_millis(200);
        // First chunk: always allowed.
        assert!(may_queue_more(true, 0, 8000, Duration::ZERO, lead));
        // 1s queued, 900ms played: 100ms ahead — under the lead, may queue.
        assert!(may_queue_more(false, 8000, 8000, Duration::from_millis(900), lead));
        // 1s queued, 700ms played: 300ms ahead — over the lead, must wait.
        assert!(!may_queue_more(false, 8000, 8000, Duration::from_millis(700), lead));
        // Exactly at the boundary: not strictly under — wait.
        assert!(!may_queue_more(false, 8000, 8000, Duration::from_millis(800), lead));
    }

    /// AK-008: un-armed frames (AEC convergence grace / pre-first-audio) are
    /// still captured AND marked for the scratchpad — they may hold the
    /// caller's first words — but never trip the barge.
    #[cfg(all(target_os = "windows", feature = "voice"))]
    #[test]
    fn barge_scan_unarmed_captures_but_never_trips() {
        let frame = 80usize;
        let mut speech_frames = 0u32;
        let mut captured: Vec<i16> = Vec::new();
        let mut speech_start: Option<usize> = None;
        let loud = vec![8000i16; frame * 10];
        let tripped = scan_barge_frames(
            &loud, frame, 350.0, 500.0, false, &mut speech_frames, 3, &mut captured,
            &mut speech_start,
        );
        assert!(!tripped);
        assert_eq!(captured.len(), frame * 10);
        assert_eq!(speech_start, Some(0), "the scratchpad hears pre-audio speech");
        assert_eq!(speech_frames, 0, "but the barge counter never arms");
    }

    /// Audit AK-008: the continuation heuristic must hold a turn open exactly
    /// when the caller sounds mid-number — digit groups, spoken digits, and
    /// the connectives that announce one — and never for a finished sentence.
    #[cfg(feature = "voice")]
    #[test]
    fn unfinished_number_heuristic() {
        // The live failure: a phone number read in groups with pauses.
        assert!(ends_with_unfinished_number("my number is 0412"));
        assert!(ends_with_unfinished_number("it's 0412 345"));
        assert!(ends_with_unfinished_number("zero four one two"));
        assert!(ends_with_unfinished_number("you can reach me on 0412, 345"));
        assert!(ends_with_unfinished_number("double four"));
        assert!(ends_with_unfinished_number("my number is"));
        assert!(ends_with_unfinished_number("you can call me on"));
        // Finished turns must flush immediately — no added latency.
        assert!(!ends_with_unfinished_number("I'd like to book a haircut"));
        assert!(!ends_with_unfinished_number("yes that's right"));
        assert!(!ends_with_unfinished_number("my name is Lance"));
        assert!(!ends_with_unfinished_number(""));
        assert!(!ends_with_unfinished_number("   "));
        // A word after the digits releases the hold.
        assert!(!ends_with_unfinished_number("nine thirty tomorrow"));
        assert!(!ends_with_unfinished_number("0412 345 678 thanks"));
    }

    /// Plan §6.2 (live 2026-07-13): the general turn-completion hold — number
    /// tails (as before), bare hesitations, trailing connectives and short
    /// filler-opened fragments all hold for a continuation; complete answers
    /// still flush instantly.
    #[cfg(feature = "voice")]
    #[test]
    fn general_turn_completion_hold() {
        // The live failure: "Um no" + "That's all really" split into two turns.
        assert!(turn_looks_unfinished("Um no"));
        assert!(turn_looks_unfinished("Uh"));
        assert!(turn_looks_unfinished("Well"));
        assert!(turn_looks_unfinished("I want to change it but"));
        assert!(turn_looks_unfinished("we could do Tuesday or"));
        // Number tails still hold (the original AK-008 behaviour).
        assert!(turn_looks_unfinished("my number is 0412"));
        // Complete answers flush immediately.
        assert!(!turn_looks_unfinished("That's good."));
        assert!(!turn_looks_unfinished("No, that's all really"));
        assert!(!turn_looks_unfinished("I'd like to book a haircut for Tuesday"));
        assert!(!turn_looks_unfinished("yes"));
    }

    /// Bare hesitations are the caller THINKING: recorded, never answered.
    #[cfg(feature = "voice")]
    #[test]
    fn hesitations_are_silence_not_turns() {
        for s in ["Uh", "um", "Well...", "hmm", "uh um"] {
            assert!(crate::duplex::is_hesitation(s), "{s:?}");
        }
        for s in ["Um no", "well yes", "no", "that's all", ""] {
            assert!(!crate::duplex::is_hesitation(s), "{s:?}");
        }
    }

    /// Audit C-01/C-02/AK-001: the radio publishes the current call's identity
    /// (`callId` + `startedAt`) into the shared status the moment it rings
    /// and clears it on termination — `call.current` and the call-control
    /// `callId` guard read exactly these fields — and the session state
    /// machine decides the ended outcome.
    #[test]
    fn handle_event_tracks_shared_call_identity() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        macro_rules! apply {
            ($ev:expr) => {
                handle_event($ev, &mut tracker, None, &mut sink, &status)
            };
        }

        assert!(status.current_call_id.lock().unwrap().is_none());

        apply!(E::CallIncoming);
        let call_id = status.current_call_id.lock().unwrap().clone();
        assert_eq!(
            call_id.as_deref(),
            tracker.call_id(),
            "shared id mirrors the session"
        );
        assert!(call_id.as_deref().unwrap().starts_with("call_"));
        assert!(status.call_started_at.lock().unwrap().is_some());
        assert!(
            !status.call_active.load(Ordering::Relaxed),
            "ringing, not active"
        );
        let first_gen = tracker.generation();
        assert_eq!(first_gen, 1);

        // Phones re-emit the ring indicator — the id must not change mid-call.
        apply!(E::CallIncoming);
        assert_eq!(*status.current_call_id.lock().unwrap(), call_id);
        assert_eq!(tracker.generation(), first_gen);

        apply!(E::CallerId("+61400000001".to_string()));
        assert_eq!(
            status.current_caller.lock().unwrap().as_deref(),
            Some("+61400000001")
        );

        apply!(E::CallAnswered);
        assert!(status.call_active.load(Ordering::Relaxed));
        assert_eq!(*status.current_call_id.lock().unwrap(), call_id);

        sink.lines.clear();
        apply!(E::CallTerminated);
        assert!(status.current_call_id.lock().unwrap().is_none());
        assert!(status.call_started_at.lock().unwrap().is_none());
        assert!(status.current_caller.lock().unwrap().is_none());
        assert!(!status.call_active.load(Ordering::Relaxed));
        assert_eq!(tracker.generation(), 0, "idle after termination");
        // The ended event carried the SAME call id the whole call used, and
        // an ANSWERED call is completed even when it lasted under a second.
        let v: serde_json::Value = serde_json::from_str(&sink.lines[0]).unwrap();
        assert_eq!(
            v["params"]["event"]["name"],
            json!(crate::contract::events::CALL_ENDED)
        );
        assert_eq!(v["params"]["event"]["data"]["callId"], json!(call_id));
        assert_eq!(v["params"]["event"]["data"]["outcome"], json!("completed"));
        assert_eq!(v["params"]["event"]["data"]["from"], json!("+61400000001"));

        // The next call gets a FRESH id and a FRESH generation.
        apply!(E::CallIncoming);
        let second = status.current_call_id.lock().unwrap().clone();
        assert!(second.is_some());
        assert_ne!(second, call_id);
        assert_eq!(tracker.generation(), 2);
    }

    /// Audit AK-001/AK-01: an operator-rejected ring ends "rejected", a
    /// remote-abandoned ring ends "missed" — the two are no longer conflated.
    #[test]
    fn handle_event_rejected_is_not_missed() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        // Operator rejects the ringing call (RadioControl::Reject notes the
        // intent, then the phone reports termination).
        // CallTerminated force-flushes the pending incoming first (AOK-LIF-001),
        // so locate the terminal event by NAME, not position.
        let ended_event = |sink: &VecSink| -> serde_json::Value {
            sink.lines
                .iter()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .find(|v| v["params"]["event"]["name"] == json!(crate::contract::events::CALL_ENDED))
                .expect("a call.ended event")
        };
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        let v = ended_event(&sink);
        assert_eq!(v["params"]["event"]["data"]["outcome"], json!("rejected"));
        assert_eq!(
            v["params"]["event"]["data"]["reason"],
            json!("operator_reject")
        );

        // Remote abandons the next ring: genuinely missed.
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        let v = ended_event(&sink);
        assert_eq!(v["params"]["event"]["data"]["outcome"], json!("missed"));
    }

    /// Audit AOK-LIF-001: `incoming` always precedes the rest of its call's
    /// lifecycle. The caller-ID enrichment hold must not let an instant
    /// answer (or termination) overtake the canonical start-of-call event.
    #[test]
    fn handle_event_incoming_always_precedes_answered() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        // The phone answers within the enrichment hold — no run_loop flush
        // tick has happened between the two events.
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);

        let names: Vec<String> = sink
            .lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["params"]["event"]["name"].as_str().unwrap_or("").to_string()
            })
            .collect();
        let incoming_at = names.iter().position(|n| n == crate::contract::events::CALL_INCOMING);
        let answered_at = names.iter().position(|n| n == crate::contract::events::CALL_ANSWERED);
        assert!(incoming_at.is_some(), "incoming must be emitted (forced flush)");
        assert!(
            incoming_at < answered_at,
            "incoming must precede answered, got order {names:?}"
        );

        // An instantly-abandoned ring still gets incoming before ended.
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        let names: Vec<String> = sink
            .lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["params"]["event"]["name"].as_str().unwrap_or("").to_string()
            })
            .collect();
        let incoming_at = names.iter().position(|n| n == crate::contract::events::CALL_INCOMING);
        let ended_at = names.iter().position(|n| n == crate::contract::events::CALL_ENDED);
        assert!(incoming_at.is_some() && incoming_at < ended_at, "incoming precedes ended, got {names:?}");
    }

    /// `aokie.call.caller_id` announces the number the FIRST time this call
    /// learns it (fed by +CLIP or the AT+CLCC rescue) — exactly once per call
    /// (+CLIP repeats per ring; the idempotency key is corr-scoped), never for
    /// an empty number, and always AFTER the call's `incoming`.
    #[test]
    fn caller_id_event_fires_once_per_call_with_the_number() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
        sink.lines.clear();

        // The number lands (CLCC rescue) → announced once, with the number.
        handle_event(
            E::CallerId("0491570156".to_string()),
            &mut tracker,
            None,
            &mut sink,
            &status,
        );
        let events: Vec<serde_json::Value> = sink
            .lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let caller_id: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| {
                v["params"]["event"]["name"] == json!(crate::contract::events::CALL_CALLER_ID)
            })
            .collect();
        assert_eq!(caller_id.len(), 1, "announced exactly once: {events:?}");
        assert_eq!(
            caller_id[0]["params"]["event"]["data"]["from"],
            json!("0491570156")
        );
        assert_eq!(
            caller_id[0]["params"]["event"]["data"]["callId"].as_str(),
            tracker.call_id()
        );

        // A repeated +CLIP for the same call must NOT re-announce (the
        // corr-scoped idempotency key may only be minted once).
        sink.lines.clear();
        handle_event(
            E::CallerId("0491570156".to_string()),
            &mut tracker,
            None,
            &mut sink,
            &status,
        );
        assert!(sink.lines.is_empty(), "no re-announce: {:?}", sink.lines);

        // An empty caller id (withheld) never announces.
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        sink.lines.clear();
        handle_event(E::CallerId(String::new()), &mut tracker, None, &mut sink, &status);
        assert!(
            !sink.lines.iter().any(|l| l.contains("caller_id")),
            "withheld id stays silent: {:?}",
            sink.lines
        );
    }

    /// AOK-CTRL-001: an ANSWER landing on an idle tracker while the phone is
    /// still connected rebuilds a session (the live-call deafness bug: a
    /// misread terminate killed the session, the late answer was a no-op and
    /// every frame of a real call was dropped). After a device loss the same
    /// stale answer must NOT build a phantom session (AOK-LIF-003).
    #[test]
    fn orphaned_answer_recovers_a_session_only_while_connected() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        // Connected phone, no session (a terminate was misread earlier).
        status.connected.store(true, Ordering::Relaxed);
        handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
        assert!(tracker.current().is_some(), "session rebuilt");
        assert!(tracker.current().unwrap().is_active(), "and answered");
        assert!(status.call_active.load(Ordering::Relaxed));
        assert_eq!(
            status.current_call_id.lock().unwrap().as_deref(),
            tracker.call_id()
        );
        let names: Vec<String> = sink
            .lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["params"]["event"]["name"].as_str().unwrap_or("").to_string()
            })
            .collect();
        let incoming_at = names.iter().position(|n| n == crate::contract::events::CALL_INCOMING);
        let answered_at = names.iter().position(|n| n == crate::contract::events::CALL_ANSWERED);
        assert!(
            incoming_at.is_some() && incoming_at < answered_at,
            "recovered session still emits incoming before answered: {names:?}"
        );
        // Clean up: terminate the recovered call normally.
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);

        // Disconnected: the stale answer is dropped — no phantom session.
        status.connected.store(false, Ordering::Relaxed);
        sink.lines.clear();
        handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
        assert!(tracker.current().is_none(), "no phantom session on a dead link");
        assert!(sink.lines.is_empty(), "and no events");
        assert!(
            !status.call_active.load(Ordering::Relaxed),
            "an unrecovered orphan answer never reads as an active call"
        );
    }

    /// Audit AOK-LIF-003: losing the phone/radio link under a live call
    /// synthesizes exactly ONE terminal `call.ended` (reason device_lost),
    /// clears the shared call identity, and a late real CallTerminated is a
    /// no-op — the UI can never stay "live" on hardware that is gone.
    #[test]
    fn handle_event_device_loss_terminates_the_active_call_once() {
        use crate::event_bridge::VecSink;
        use aokie_dongle::bluetooth::BluetoothEvent as E;

        let status = Arc::new(RadioStatus::default());
        let mut sink = VecSink::default();
        let mut tracker = crate::call_session::SessionTracker::new();

        handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
        handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
        assert!(status.call_active.load(Ordering::Relaxed));
        sink.lines.clear();

        handle_event(
            E::DeviceDisconnected("AA:BB:CC:DD:EE:FF".into()),
            &mut tracker,
            None,
            &mut sink,
            &status,
        );

        let events: Vec<serde_json::Value> = sink
            .lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let ended: Vec<&serde_json::Value> = events
            .iter()
            .filter(|v| v["params"]["event"]["name"] == json!(crate::contract::events::CALL_ENDED))
            .collect();
        assert_eq!(ended.len(), 1, "exactly one terminal event");
        assert_eq!(ended[0]["params"]["event"]["data"]["reason"], json!("device_lost"));
        assert_eq!(ended[0]["params"]["event"]["data"]["outcome"], json!("completed"));
        assert!(!status.call_active.load(Ordering::Relaxed));
        assert!(status.current_call_id.lock().unwrap().is_none(), "call identity cleared");
        assert!(tracker.current().is_none(), "session consumed");

        // The phone reports the (now stale) termination later: no duplicate.
        sink.lines.clear();
        handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
        assert!(
            sink.lines.iter().all(|l| !l.contains(crate::contract::events::CALL_ENDED)),
            "late real termination after synthesized one is a no-op"
        );
    }

    #[test]
    fn http_speech_fallback_is_sticky_per_call() {
        let mut state = HttpSpeechFallback::new(Some(
            "  http://127.0.0.1:17920/v1/audio/speech  ".to_string(),
        ));
        assert_eq!(
            state.endpoint_for_call(),
            Some("http://127.0.0.1:17920/v1/audio/speech")
        );

        assert!(state.mark_failed_for_call());
        assert_eq!(state.endpoint_for_call(), None);
        assert!(!state.mark_failed_for_call());

        state.reset_call();
        assert_eq!(
            state.endpoint_for_call(),
            Some("http://127.0.0.1:17920/v1/audio/speech")
        );

        assert!(state.mark_failed_for_call());
        state.configure(Some("http://127.0.0.1:17920/v1/audio/speech2".to_string()));
        assert_eq!(
            state.endpoint_for_call(),
            Some("http://127.0.0.1:17920/v1/audio/speech2")
        );

        state.configure(Some("   ".to_string()));
        assert_eq!(state.endpoint_for_call(), None);
    }
}

/// §12.3 synthetic audio rig: drives the REAL paced-playback machinery
/// ([`TtsChunkPlayback`] + the real speexdsp [`crate::aec::EchoCanceller`])
/// with scripted audio on a virtual clock — a known "bot voice" waveform
/// whose echo returns through a synthetic echo path, plus an optional caller
/// signal at a chosen onset/level/duration. No radio, no models: this tests
/// the duplex DECISIONS (echo rejection, scratchpad capture, barge timing,
/// interrupt policy, spoken-command cuts, ducking) end-to-end at the sample
/// level, deterministically.
#[cfg(all(test, target_os = "windows", feature = "voice"))]
mod synthetic_audio {
    use super::*;
    use std::time::{Duration, Instant};

    const SR: u16 = 8000; // CVSD-like; every gate in the engine is rate-relative
    const CHUNK: usize = 160; // 20 ms @ 8 kHz — the paced loop's chunk size
    const ECHO_DELAY: usize = CHUNK; // one-chunk round trip, well inside the AEC tail
    const TRIP_RMS: f32 = 500.0; // the live default barge threshold

    /// Scripted transport: the timeline stages one mic frame per step (echo
    /// mix + caller script); everything the engine sends is recorded — it IS
    /// the played audio, ducking included.
    #[derive(Default)]
    struct FakeLink {
        sent: Vec<i16>,
        mic: std::collections::VecDeque<Vec<i16>>,
    }

    impl AudioLink for FakeLink {
        fn try_recv_audio(&mut self) -> Option<Vec<i16>> {
            self.mic.pop_front()
        }
        fn send_audio(&mut self, pcm: &[i16]) {
            self.sent.extend_from_slice(pcm);
        }
    }

    /// A voice-like synthetic signal: two incommensurate tones, optionally
    /// with a syllable-rate (4 Hz) amplitude envelope, scaled so the whole
    /// buffer's RMS hits `rms`. Not a pure sine (the AEC preprocessor
    /// special-cases those) and, with `am`, not stationary either (the
    /// denoiser eats steady tones).
    fn voice_signal(n: usize, phase0: usize, rms: f32, f1: f32, f2: f32, am: bool) -> Vec<i16> {
        let sr = SR as f32;
        let mut raw = Vec::with_capacity(n);
        let mut acc = 0.0f64;
        for i in 0..n {
            let t = (phase0 + i) as f32 / sr;
            let mut v = (2.0 * std::f32::consts::PI * f1 * t).sin()
                + 0.6 * (2.0 * std::f32::consts::PI * f2 * t).sin();
            if am {
                v *= 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
            }
            raw.push(v);
            acc += (v as f64) * (v as f64);
        }
        let cur = ((acc / n.max(1) as f64).sqrt()) as f32;
        let k = if cur > 0.0 { rms / cur } else { 0.0 };
        raw.iter()
            .map(|v| (v * k).clamp(-32000.0, 32000.0) as i16)
            .collect()
    }

    /// The phone-side echo of our playout: an attenuated copy of the sent
    /// buffer, `ECHO_DELAY` samples behind real time. Returns the `n` mic
    /// samples for the next step (leading zeros while history is short).
    fn echo_chunk(sent: &[i16], n: usize, gain: f32) -> Vec<i16> {
        let end = sent.len().saturating_sub(ECHO_DELAY);
        let start = end.saturating_sub(n);
        let src = &sent[start..end];
        let mut out = vec![0i16; n];
        let off = n - src.len();
        for (i, &v) in src.iter().enumerate() {
            out[off + i] = (v as f32 * gain) as i16;
        }
        out
    }

    struct Timeline {
        /// How much bot audio the "synth" produces.
        bot_ms: u64,
        /// Steps before the first push — the paced loop polls the mic while
        /// synthesis warms up (greeting scenario).
        synth_warmup_ms: u64,
        /// Echo-path attenuation (0.0 = no echo returns).
        echo_gain: f32,
        /// Pre-converge the canceller on the echo path first (mid-call
        /// reality, and determinism — adaption transients stay out of the
        /// assertions).
        warm_aec: bool,
        /// Span interrupt policy (None = Yield).
        finish_extra: Option<Duration>,
        caller_onset_ms: Option<u64>,
        caller_dur_ms: u64,
        caller_rms: f32,
        caller_am: bool,
        /// Inject a probe-lane verdict ("stop") at this playout time.
        semantic_at_ms: Option<u64>,
        /// Inject an urgent control (hangup/reject) at this playout time.
        cancel_at_ms: Option<u64>,
    }

    impl Default for Timeline {
        fn default() -> Self {
            Self {
                bot_ms: 3000,
                synth_warmup_ms: 0,
                echo_gain: 0.3,
                warm_aec: true,
                finish_extra: None,
                caller_onset_ms: None,
                caller_dur_ms: 1000,
                caller_rms: 2500.0,
                caller_am: true,
                semantic_at_ms: None,
                cancel_at_ms: None,
            }
        }
    }

    struct RunResult {
        outcome: SpeakOutcome,
        stopped_at_ms: u64,
        sent: Vec<i16>,
    }

    /// Drive one bot utterance through the engine exactly like the paced
    /// loop: 20 ms virtual steps, mic staged before each step from the echo
    /// of what already played plus the caller script, `now` advanced on the
    /// fake clock.
    fn run(t: &Timeline) -> RunResult {
        let mut aec_engine = crate::aec::EchoCanceller::new(SR as u32);
        let mut link = FakeLink::default();
        // The bot signal is generated PHASE-CONTINUOUS with the AEC warm-up
        // preamble (first rig run proved why, twice: a sent-history reset
        // reads as an echo-path change, and even a carrier-phase jump sprays
        // broadband energy through frequency bins the converged filter never
        // learned — either way the leak transient marks phantom caller
        // speech and cascades into ducking the whole run).
        let warm_len = if t.warm_aec && t.echo_gain > 0.0 {
            SR as usize * 2
        } else {
            0
        };
        let bot_len = (SR as u64 * t.bot_ms / 1000) as usize;
        let full = voice_signal(warm_len + bot_len, 0, 6000.0, 330.0, 730.0, true);
        if warm_len > 0 {
            // Teach the filter the echo path first (mid-call reality), feeding
            // reference + capture in the same order as the run loop, and keep
            // the warm-up audio as the link's sent-history.
            for chunk in full[..warm_len].chunks(CHUNK) {
                let mic = echo_chunk(&link.sent, CHUNK, t.echo_gain);
                aec_engine.feed_reference(chunk);
                let _ = aec_engine.process_capture(&mic);
                link.sent.extend_from_slice(chunk);
            }
        }
        let run_start = link.sent.len();
        let bot = &full[warm_len..];
        let mut playback = TtsChunkPlayback::new(SR, t.finish_extra);
        let base = playback.t0;
        let mut no_ctl: Option<&mut ControlProbe<'_>> = None;
        let warm_steps = t.synth_warmup_ms / 20;
        let mut caller_phase = 0usize;
        let mut pos = 0usize;
        let mut step: u64 = 0;
        loop {
            let now = base + Duration::from_millis(step * 20);
            let ms = step * 20;

            // Stage this step's mic frame: echo of what already played + the
            // caller script.
            let mut mic = if t.echo_gain > 0.0 {
                echo_chunk(&link.sent, CHUNK, t.echo_gain)
            } else {
                vec![0i16; CHUNK]
            };
            if let Some(on) = t.caller_onset_ms {
                if ms >= on && ms < on + t.caller_dur_ms {
                    let c = voice_signal(
                        CHUNK,
                        caller_phase,
                        t.caller_rms,
                        210.0,
                        520.0,
                        t.caller_am,
                    );
                    caller_phase += CHUNK;
                    for (m, v) in mic.iter_mut().zip(c) {
                        *m = m.saturating_add(v);
                    }
                }
            }
            link.mic.push_back(mic);

            // Injected verdicts (probe lane / urgent control), by playout time.
            if let Some(at) = t.semantic_at_ms {
                if ms >= at && playback.semantic.is_none() {
                    playback.semantic = Some(crate::duplex::CallerIntent::StopSpeaking);
                }
            }
            if let Some(at) = t.cancel_at_ms {
                if ms >= at {
                    playback.cancelled = true;
                }
            }

            if step < warm_steps || pos >= bot.len() {
                // Synthesis warm-up / post-audio tail: the paced loop still
                // drains the mic every iteration.
                playback.poll_mic(
                    &mut link,
                    &mut Some(&mut aec_engine),
                    Some(TRIP_RMS),
                    now,
                );
                if playback.stop_playback_now(now) {
                    break;
                }
                if pos >= bot.len() && playback.played_out(now) {
                    break;
                }
            } else {
                let n = CHUNK.min(bot.len() - pos);
                let ok = playback.push(
                    &mut link,
                    &mut Some(&mut aec_engine),
                    Some(TRIP_RMS),
                    &mut no_ctl,
                    &bot[pos..pos + n],
                    now,
                );
                pos += n;
                if !ok {
                    break;
                }
            }
            step += 1;
            assert!(step < 4000, "synthetic timeline ran away");
        }
        let stopped_at_ms = step * 20;
        RunResult {
            outcome: playback.into_outcome("synthetic", SR),
            stopped_at_ms,
            sent: link.sent.split_off(run_start),
        }
    }

    fn sent_rms(sent: &[i16], from_ms: u64, to_ms: u64) -> f32 {
        let a = (from_ms as usize * SR as usize / 1000).min(sent.len());
        let b = (to_ms as usize * SR as usize / 1000).min(sent.len());
        crate::voice::frame_rms(&sent[a..b])
    }

    /// §12.3 "echo not being transcribed as caller speech": with the real
    /// canceller converged on the echo path, three seconds of playout whose
    /// echo returns at 30 % never marks caller speech, never barges, and
    /// hands back no captured audio.
    #[test]
    fn echo_alone_is_cancelled_never_captured() {
        let r = run(&Timeline::default());
        assert!(!r.outcome.barged, "echo residual tripped the barge");
        assert!(!r.outcome.cancelled);
        assert!(
            r.outcome.captured_speech.is_empty(),
            "echo residual crossed the capture gate ({} samples captured)",
            r.outcome.captured_speech.len()
        );
        assert!(r.stopped_at_ms >= 3000, "playback must run to its natural end");
    }

    /// §12.3 caller onset at 100 / 300 / 1000 ms into bot output: sustained
    /// loud caller speech trips the acoustic barge shortly after onset —
    /// never before the arming grace — and the overlap is captured for STT.
    #[test]
    fn caller_onset_trips_barge_at_each_offset() {
        for (onset, lo, hi) in [(100u64, 400u64, 1100u64), (300, 450, 1200), (1000, 1100, 1950)] {
            let r = run(&Timeline {
                caller_onset_ms: Some(onset),
                caller_dur_ms: 2500,
                ..Timeline::default()
            });
            assert!(r.outcome.barged, "onset {onset} ms: barge never tripped");
            assert!(
                r.stopped_at_ms >= lo && r.stopped_at_ms <= hi,
                "onset {onset} ms: stopped at {} ms (expected {lo}..{hi})",
                r.stopped_at_ms
            );
            assert!(
                !r.outcome.captured_speech.is_empty(),
                "onset {onset} ms: overlap not captured"
            );
        }
    }

    /// §12.3 a ~200 ms interjection is REMEMBERED even without an acoustic
    /// barge: quiet overlap (above the capture gate, below the trip
    /// threshold) never stops playback but rides out in captured_speech.
    #[test]
    fn brief_quiet_interjection_captured_without_barge() {
        let r = run(&Timeline {
            caller_onset_ms: Some(600),
            caller_dur_ms: 200,
            caller_rms: 460.0,
            caller_am: false,
            ..Timeline::default()
        });
        assert!(!r.outcome.barged, "sub-trip speech must never barge");
        assert!(r.stopped_at_ms >= 3000, "playback must run to its natural end");
        assert!(
            !r.outcome.captured_speech.is_empty(),
            "the scratchpad must hear the interjection"
        );
    }

    /// §12.3 "wait" inside the first 200 ms of a greeting: speech BEFORE any
    /// audio has played is captured for the scratchpad but never cancels the
    /// greeting (the paced loop polls the mic during synthesis warm-up; live
    /// finding 2026-07-13 — a false barge here answered a call into dead air).
    #[test]
    fn pre_audio_speech_never_cancels_playback() {
        let r = run(&Timeline {
            synth_warmup_ms: 400,
            caller_onset_ms: Some(40),
            caller_dur_ms: 240,
            ..Timeline::default()
        });
        assert!(!r.outcome.barged, "pre-audio speech must not barge");
        assert!(
            r.stopped_at_ms >= 3400,
            "the greeting must play out fully (stopped at {} ms)",
            r.stopped_at_ms
        );
        assert!(
            !r.outcome.captured_speech.is_empty(),
            "but the scratchpad hears the pre-audio speech"
        );
    }

    /// §12.3 protected output with overlapping content: a FinishSpan span
    /// (digits, [[important]]) rides through the barge for its bounded
    /// extension, then yields; the same overlap stops a Yield span at once.
    #[test]
    fn finish_span_rides_bounded_extension_then_yields() {
        let overlap = Timeline {
            caller_onset_ms: Some(500),
            caller_dur_ms: 2500,
            ..Timeline::default()
        };
        let yielded = run(&overlap);
        let protected = run(&Timeline {
            finish_extra: Some(Duration::from_millis(700)),
            ..overlap
        });
        assert!(yielded.outcome.barged && protected.outcome.barged);
        assert!(
            protected.stopped_at_ms >= yielded.stopped_at_ms + 600,
            "the finish budget must audibly extend playback (yield {} ms vs finish {} ms)",
            yielded.stopped_at_ms,
            protected.stopped_at_ms
        );
        assert!(
            protected.stopped_at_ms <= yielded.stopped_at_ms + 1000,
            "but the budget is bounded (yield {} ms vs finish {} ms)",
            yielded.stopped_at_ms,
            protected.stopped_at_ms
        );
    }

    /// §12.3 explicit stop during protected output: a spoken floor command
    /// (probe-lane verdict) cuts even a protected span immediately — explicit
    /// commands always beat policy.
    #[test]
    fn spoken_stop_cuts_protected_span() {
        let r = run(&Timeline {
            finish_extra: Some(Duration::from_millis(5000)),
            caller_onset_ms: Some(500),
            caller_dur_ms: 2500,
            semantic_at_ms: Some(900),
            ..Timeline::default()
        });
        assert!(
            r.stopped_at_ms <= 1000,
            "spoken command must cut instantly (stopped at {} ms with a 5 s budget)",
            r.stopped_at_ms
        );
        assert!(matches!(
            r.outcome.commanded,
            Some(crate::duplex::CallerIntent::StopSpeaking)
        ));
        assert!(
            r.outcome.barged,
            "a spoken command IS the caller taking the floor"
        );
    }

    /// §12.3 call end during queued playout: an urgent control (hangup /
    /// reject) cancels mid-span regardless of policy.
    #[test]
    fn urgent_control_cancels_mid_playout() {
        let r = run(&Timeline {
            finish_extra: Some(Duration::from_millis(5000)),
            cancel_at_ms: Some(700),
            ..Timeline::default()
        });
        assert!(r.outcome.cancelled);
        assert!(
            r.stopped_at_ms <= 800,
            "urgent control must cut at chunk granularity (stopped at {} ms)",
            r.stopped_at_ms
        );
    }

    /// §2.2 the nudge: caller overlap DUCKS the playing span (~35 %) and the
    /// gain ramps back up once they stop — a brief interjection doesn't leave
    /// the rest of the sentence whispering. Measured on the actually-sent
    /// samples, ducking applied.
    #[test]
    fn overlap_ducks_output_then_ramps_back() {
        let r = run(&Timeline {
            bot_ms: 4000,
            // A large finish budget holds the floor so ducking is observable
            // in isolation from the yield decision.
            finish_extra: Some(Duration::from_millis(10_000)),
            caller_onset_ms: Some(800),
            caller_dur_ms: 400,
            ..Timeline::default()
        });
        let full = sent_rms(&r.sent, 200, 700);
        let ducked = sent_rms(&r.sent, 950, 1200);
        let recovered = sent_rms(&r.sent, 3000, 3800);
        assert!(
            ducked < full * 0.6,
            "output must duck under overlap (ducked {ducked} vs full {full})"
        );
        assert!(
            recovered > full * 0.8,
            "gain must ramp back after the overlap ends (recovered {recovered} vs full {full})"
        );
    }

    /// The probe lane end-to-end minus audio: a result that parses to a floor
    /// command parks as the pending verdict; content partials feed
    /// sentence-boundary steering; one probe in flight at a time; stale
    /// generations are dropped.
    #[test]
    fn probe_lane_commands_content_and_backpressure() {
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let status = RadioStatus::default();
        let mut lane = SttProbeLane::new(&stt_tx, &res_rx, 7, &status);

        // A playback with ≥400 ms of marked speech ships exactly ONE probe.
        let mut p = TtsChunkPlayback::new(SR, None);
        p.captured = vec![100i16; SR as usize];
        p.speech_start = Some(0);
        lane.maybe_probe(&p);
        assert!(matches!(
            stt_rx.try_recv(),
            Ok(SttWork::Probe { generation: 7, .. })
        ));
        lane.maybe_probe(&p);
        assert!(stt_rx.try_recv().is_err(), "at most one probe in flight");

        // Stale-generation results are dropped; a matching "wait" parks.
        res_tx
            .send(SttResult { generation: 6, utterance: 0, text: "stop".into() })
            .unwrap();
        assert!(lane.check().is_none(), "stale generation must be dropped");
        res_tx
            .send(SttResult { generation: 7, utterance: 0, text: "wait".into() })
            .unwrap();
        assert!(matches!(
            lane.check(),
            Some(crate::duplex::CallerIntent::Pause)
        ));

        // Substantive content steers at sentence boundaries; backchannels don't.
        res_tx
            .send(SttResult { generation: 7, utterance: 0, text: "yeah".into() })
            .unwrap();
        assert!(lane.substantive_content("the weather is lovely today").is_none());
        res_tx
            .send(SttResult {
                generation: 7,
                utterance: 0,
                text: "actually I need to change my order".into(),
            })
            .unwrap();
        assert_eq!(
            lane.substantive_content("the weather is lovely today").as_deref(),
            Some("actually I need to change my order")
        );
    }
}
