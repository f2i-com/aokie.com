//! `RadioStatus` shared state plus the switchboard/diagnostic snapshot types.

#[allow(unused_imports)]
use super::*;

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
    /// Desktop-brokered Realtime voice diagnostics. `ready` is call-scoped:
    /// it becomes true only after Desktop has proven the configured upstream
    /// origin for the current ringing call, and returns false at its boundary.
    pub realtime_selected: AtomicBool,
    pub realtime_ready: AtomicBool,
    pub realtime_destination: Mutex<Option<String>>,
    pub realtime_error: Mutex<Option<String>>,
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
    /// Clause-level replanning (guide §7): substantive overlap content cut a
    /// span MID-SENTENCE (policy-aware soft barge) instead of waiting for the
    /// sentence boundary.
    pub mid_span_yields: AtomicU64,
    /// FloorManager SHADOW (guide §7.1/P1-8): what the fused evidence-driven
    /// decision-maker WOULD have done, logged only — the live threshold paths
    /// keep the floor. Transition counts per severity + span-end divergences
    /// (shadow said cut/yield but the span played out, or vice versa) are the
    /// promote-or-tune signal.
    pub floor_shadow_cuts: AtomicU64,
    pub floor_shadow_yields: AtomicU64,
    pub floor_shadow_ducks: AtomicU64,
    pub floor_shadow_divergences: AtomicU64,
    /// Speculative reply generation (guide phase 5): starts from a STABLE
    /// live-STT hypothesis while the caller is still speaking; kept when the
    /// final turn matched, cancelled (wasted) when it diverged.
    pub spec_llm_started: AtomicU64,
    pub spec_llm_kept: AtomicU64,
    pub spec_llm_wasted: AtomicU64,
    /// Phase-0 observability: run_loop liveness breadcrumbs — the iteration
    /// counter and the coarse phase the loop last entered (see
    /// [`loop_phase`]). The watchdog thread reports a stalled loop WITH its
    /// phase, so a live hang names its own location instead of needing a
    /// debugger on a production box.
    pub loop_beat: AtomicU64,
    pub loop_phase: std::sync::atomic::AtomicU8,
    /// When the STT worker started its CURRENT transcription (None = idle) +
    /// the job's sample count — a wedged engine reports itself the same way.
    pub stt_busy: Mutex<Option<(std::time::Instant, usize)>>,
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
    /// Phase 1 abuse auto-block hand-off: numbers the radio thread already
    /// blocked LIVE (policy + env) that still need persisting into the
    /// `blockedNumbers` setting. The connector drains this on every command
    /// dispatch (the desktop's health poll bounds the lag) — the radio never
    /// touches the settings store itself.
    pub pending_blocked_numbers: Mutex<Vec<String>>,
    /// Phase 2: the outbound dial in flight (set at ATD, cleared when its
    /// call goes ACTIVE). Lets the event mapper attach a late
    /// OutgoingDialing to the REAL dial (agent-owned, original call id) and
    /// refuse a stale CallTerminated that would otherwise kill the fresh
    /// attempt (live incident 2026-07-14: a held verdict from the
    /// just-missed inbound ring discharged 100ms after ATD — the callee
    /// answered a silent observed session while a spurious failed
    /// call.ended fired the apology-SMS flow).
    pub pending_dial: Mutex<Option<PendingDial>>,
    /// Phase 4 (observe-only): waiting episodes seen this radio session —
    /// dongle.diagnostics visibility for the live capability soak.
    pub call_waiting_episodes: AtomicU64,
    /// The call id whose CURRENT waiting episode already emitted
    /// `aokie.call.waiting` (one durable event per episode; cleared when
    /// the episode ends so a later second knock on the same call
    /// re-announces).
    pub call_waiting_announced: Mutex<Option<String>>,
    /// Last `callheld` indicator state (0 none / 1 held+active / 2 held
    /// only) — diagnostics only, nothing consumes it yet.
    pub call_held_state: AtomicU64,
    /// Phase 4 observe topology: the most recent AT+CLCC snapshot (one
    /// rendered line per current call) + when its burst started. Entries
    /// arriving within a short window belong to one response burst; a
    /// later entry starts a fresh snapshot. Bounded (a phone has at most a
    /// handful of concurrent legs). Exposed via dongle.diagnostics so a
    /// knock's topology is verifiable AFTER the log ring wraps.
    pub clcc_snapshot: Mutex<Option<(std::time::Instant, Vec<String>)>>,
    /// Structured twin of `clcc_snapshot` (same burst rules): the
    /// verified-swap machinery matches leg status/numbers programmatically
    /// against exactly the burst a post-CHLD `AT+CLCC` produced, instead of
    /// assuming a toggle took.
    pub clcc_calls: Mutex<Option<(std::time::Instant, Vec<ClccLeg>)>>,
    /// Phase 4 switchboard mirrors (connector-readable): the WAITING caller
    /// (identity minted at the knock — `call.activate`'s target before any
    /// session exists) and the PARKED caller (their real session + voice
    /// context live as radio-loop locals; this is the reporting/validation
    /// view). Max one of each in v1 — plain CHLD=2 is ambiguous with more.
    pub waiting_call: Mutex<Option<SwitchboardLeg>>,
    pub parked_call: Mutex<Option<SwitchboardLeg>>,
    /// Bumped on every topology change (knock start/end, park, restore,
    /// promotion) — `call.activate`'s optimistic-concurrency token.
    pub switchboard_revision: AtomicU64,
    /// A CHLD switch we sent that hasn't settled (label + when sent). One
    /// at a time; doubles as the suppression window for callheld-edge
    /// attribution (our own swap produces expected transitions).
    pub switch_in_flight: Mutex<Option<(String, std::time::Instant)>>,
    /// Waiting episodes that just ended, pending give-up classification
    /// (leg, when it ended, tracker generation at that moment). A QUEUE
    /// (bounded 8): during one long call several different callers can
    /// knock and give up in turn — EVERY unclaimed one becomes an honest
    /// MISSED `call.ended` under its waitingCallId so the missed-call
    /// flows queue a ring-back for each (user request 2026-07-15: multiple
    /// missed calls must all be called back, one after the other). Claimed
    /// entries (accepted under their minted id, or promoted under a fresh
    /// id with the same number) are dropped silently.
    pub gave_up_knock: Mutex<Vec<(SwitchboardLeg, std::time::Instant, u64)>>,
}

/// One non-foreground switchboard leg (waiting or parked) — see
/// [`RadioStatus::waiting_call`].
#[derive(Debug, Clone)]
pub struct SwitchboardLeg {
    pub call_id: String,
    /// "" when the network withheld the number.
    pub from: String,
    pub since_iso: String,
}

/// One structured `+CLCC:` leg — see [`RadioStatus::clcc_calls`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClccLeg {
    pub index: u8,
    /// 0 active / 1 held / 2 dialing / 3 alerting / 4 incoming / 5 waiting.
    pub status: u8,
    pub number: Option<String>,
}

/// See [`RadioStatus::pending_dial`].
pub struct PendingDial {
    pub call_id: String,
    pub number: String,
    pub at: std::time::Instant,
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
