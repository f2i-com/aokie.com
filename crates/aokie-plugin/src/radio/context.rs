//! `CallVoiceContext`: the per-call conversational state bundle.

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "windows")]
pub(super) struct CallVoiceContext {
    /// Volatile realtime captions lane (guide §9.2) — one per call epoch;
    /// `None` until the call id is known.
    pub(super) rt_lane: Option<crate::realtime::RealtimeLane>,
    /// [[WAIT]] streak breaker (live call 5824f759): at most ONE
    /// consecutive accepted WAIT.
    pub(super) consecutive_waits: u32,
    /// Post-hangup ghost-turn latch (live call 2f668ad4): once the agent
    /// finalizes the call, late turns are recorded but never answered.
    pub(super) agent_hung_up: bool,
    /// The previous caller turn's text: an explicit 'look it up' usually
    /// names its subject one turn earlier.
    pub(super) prev_caller_text: String,
    /// Conversation history for the agent (OpenAI chat messages).
    #[cfg(feature = "voice")]
    pub(super) history: Vec<serde_json::Value>,
    /// Monotonic transcript turn index (caller + bot share one sequence),
    /// 1-based to match the simulated-call convention.
    #[cfg(feature = "voice")]
    pub(super) turn_index: u32,
    /// Caller turn held open across STT utterances (audit AK-008).
    #[cfg(feature = "voice")]
    pub(super) pending_turn: Option<PendingTurn>,
    /// Phase 3: the per-call manager PIN gate.
    #[cfg(feature = "voice")]
    pub(super) manager_gate: ManagerGate,
    /// §9.3 call-scoped agent overlay (persona/greeting bound to ONE call).
    #[cfg(feature = "voice")]
    pub(super) call_agent_overlay: Option<CallAgentOverlay>,
    /// Call-local speaking pace, mutated live by "slower"/"faster".
    #[cfg(feature = "voice")]
    pub(super) pace: crate::speech_plan::PaceState,
    /// Duplex floor state (PausedByCaller = intentional silence).
    #[cfg(feature = "voice")]
    pub(super) dialogue: crate::duplex::DialogueState,
    /// AOK-CTRL-001 max-silence watchdog (armed by the greeting).
    #[cfg(feature = "voice")]
    pub(super) silence_timer: Option<SilenceTimer>,
    /// Aokie's last spoken line — the self-echo guard's reference.
    #[cfg(feature = "voice")]
    pub(super) last_bot_reply: String,
    /// The last bot line as clean SPEAKABLE text ("repeat that" replays it).
    #[cfg(feature = "voice")]
    pub(super) last_bot_speech: String,
    /// The nudge: an interrupted reply's UNSPOKEN tail, consumed by exactly
    /// one next generation.
    #[cfg(feature = "voice")]
    pub(super) last_cut_context: Option<String>,
    /// Split-utterance continuity: previous turn's audio + draft + flush
    /// instant (audioTranscript correction requests prepend it).
    #[cfg(feature = "voice")]
    pub(super) prev_heard: Option<(Vec<i16>, String, std::time::Instant)>,
    /// The current turn's OWN paired audio (sendAudio reply attach +
    /// audioTranscript correction source).
    #[cfg(feature = "voice")]
    pub(super) last_turn_audio: Vec<i16>,
    /// One volatile typed-help request for this exact caller/fence. It is
    /// parked with the caller context and destroyed at the call boundary.
    #[cfg(feature = "voice")]
    pub(super) pending_assistance: Option<PendingAssistanceCall>,
    /// The caller was assigned the Desktop Realtime responder. This travels
    /// with a parked caller even though its WebSocket never does: a restore
    /// therefore creates a fresh, call-fenced session instead of silently
    /// falling into the local legacy pipeline.
    #[cfg(feature = "voice")]
    pub(super) desktop_realtime_responder: bool,
}

#[cfg(target_os = "windows")]
impl CallVoiceContext {
    /// A brand-new caller's context — the same values every field was
    /// individually reset to at the old per-call boundary.
    pub(super) fn fresh(rt_lane: Option<crate::realtime::RealtimeLane>) -> Self {
        Self {
            rt_lane,
            consecutive_waits: 0,
            agent_hung_up: false,
            prev_caller_text: String::new(),
            #[cfg(feature = "voice")]
            history: Vec::new(),
            #[cfg(feature = "voice")]
            turn_index: 1,
            #[cfg(feature = "voice")]
            pending_turn: None,
            #[cfg(feature = "voice")]
            manager_gate: ManagerGate::default(),
            #[cfg(feature = "voice")]
            call_agent_overlay: None,
            // Pace + floor are strictly per-call: the next caller gets the
            // configured defaults, never the last caller's "slower".
            #[cfg(feature = "voice")]
            pace: crate::speech_plan::PaceState::from_env(),
            #[cfg(feature = "voice")]
            dialogue: crate::duplex::DialogueState::new(),
            #[cfg(feature = "voice")]
            silence_timer: None,
            #[cfg(feature = "voice")]
            last_bot_reply: String::new(),
            #[cfg(feature = "voice")]
            last_bot_speech: String::new(),
            #[cfg(feature = "voice")]
            last_cut_context: None,
            #[cfg(feature = "voice")]
            prev_heard: None,
            #[cfg(feature = "voice")]
            last_turn_audio: Vec::new(),
            #[cfg(feature = "voice")]
            pending_assistance: None,
            #[cfg(feature = "voice")]
            desktop_realtime_responder: false,
        }
    }
}
