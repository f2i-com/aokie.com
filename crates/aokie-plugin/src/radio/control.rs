//! Control-plane types: `RadioControl`, `EndpointUpdate` and the per-call agent overlay.

#[allow(unused_imports)]
use super::*;

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
pub(super) struct CallAgentOverlay {
    pub(super) call_id: String,
    pub(super) persona: Option<String>,
    pub(super) greeting: Option<String>,
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
    /// Protocol-v2 caller ending. Unlike the public connector hangup, this
    /// command carries the complete takeover/physical fence and is checked
    /// again on the radio thread immediately before AT+CHUP. A queued command
    /// can therefore never land after Return to Aokie or on a later call.
    EndCallerFromCompanion {
        request: CompanionEndCallerRequest,
        reply: Sender<Result<(), CompanionEndCallerFailure>>,
    },
    SendSms {
        message_id: String,
        to: String,
        body: String,
    },
    /// Phase 2: place an OUTBOUND call (`call.dial`). The connector minted
    /// `call_id` (returned to the caller) and enforced every guardrail;
    /// the radio owns the wire: ATD, the outbound session, the
    /// `aokie.call.outbound.dialing` event, and the agent context — the
    /// `opening_line` is spoken VERBATIM when the remote party answers
    /// (via the greeting slot) and `purpose` grounds the conversation.
    Dial {
        call_id: String,
        number: String,
        purpose: Option<String>,
        opening_line: String,
        op: Option<String>,
    },
    /// Speak text to the caller. The connector result is `accepted/queued`;
    /// the bot `call.turn.final` event is the authoritative confirmation the
    /// text actually played (a silent synthesis emits `speak_failed` instead).
    Speak {
        text: String,
        op: Option<String>,
    },
    /// Phase 4 (switchboard): make `call_id` the FOREGROUND call. The
    /// connector validated the target against the switchboard mirrors
    /// (waiting or parked) and revision; the radio owns the wire: it parks
    /// the current foreground context+session, sends exactly one AT+CHLD=2
    /// (toggle — never blind-retried), and installs/restores the target.
    /// Outcome confirmation is the indicator stream + the follow-up CLCC.
    Activate {
        call_id: String,
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
        endpoint: EndpointUpdate,
        stt_endpoint: EndpointUpdate,
        tts_endpoint: EndpointUpdate,
        /// The `ttsEngine`/`ttsModelDir` selection changed: the connector
        /// re-stamped AOKIE_TTS_ENGINE / AOKIE_TTS_MODEL_DIR before sending
        /// this, so the radio just tells the synth worker to reload its
        /// in-process engine from the new env.
        reload_tts_engine: bool,
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
    /// Live-reload the call-screening policy (spec Phase 0) from the current
    /// environment — sent by settings.set when a screening key changes so a
    /// block/unblock takes effect on the NEXT call without a reconnect. The
    /// env vars are set by the connector before this is sent.
    ReloadScreening,
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
    /// Graceful stop. The optional completion is sent only after terminal
    /// transcript settlements have been durably outboxed/emitted, so the
    /// plugin process can acknowledge shutdown without losing background
    /// after-call work that was waiting on a detached correction.
    Shutdown {
        completion: Option<Sender<()>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointUpdate {
    Unchanged,
    Clear,
    Set(String),
}
