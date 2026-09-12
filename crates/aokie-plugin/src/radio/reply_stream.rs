//! Detached LLM reply streaming, speculative-reply hypothesis gating and reply deadlines.

#[allow(unused_imports)]
use super::*;

/// One message from the detached reply worker to the radio thread. The
/// bounded channel (see `REPLY_CHANNEL_BOUND`) is the backpressure: synthesis
/// paces consumption, so a runaway generation blocks the WORKER, never grows
/// a queue.
#[cfg(feature = "voice")]
pub(super) enum ReplyMsg {
    Sentence(String),
    /// The stream finished (full text) or failed (reason). Always the last
    /// message the worker sends.
    Done(Result<String, String>),
}

#[cfg(feature = "voice")]
pub(super) const REPLY_CHANNEL_BOUND: usize = 8;

/// One in-flight agent generation: the detached worker streaming sentences
/// into a bounded channel (see [`ReplyMsg`]), plus the caller text it is
/// answering. Speculative generation (guide phase 5) makes this a first-class
/// value: a stream started from a STABLE live-STT hypothesis mid-utterance is
/// ADOPTED by the reply path when the final turn says the same thing — the
/// first sentence is then already waiting in the channel.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) struct ReplyStream {
    pub(super) rx: std::sync::mpsc::Receiver<ReplyMsg>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) activity: Arc<Mutex<Option<Instant>>>,
    /// The caller text this generation answers (final turn text, or the
    /// live-STT hypothesis it speculated from).
    pub(super) answering: String,
    pub(super) started: Instant,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn spawn_reply_stream(
    client: &crate::agent::LlmClient,
    messages: serde_json::Value,
    answering: String,
) -> ReplyStream {
    let (reply_tx, rx) = std::sync::mpsc::sync_channel::<ReplyMsg>(REPLY_CHANNEL_BOUND);
    let cancel = Arc::new(AtomicBool::new(false));
    let activity: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    {
        let client = client.clone();
        let c = cancel.clone();
        let a = activity.clone();
        // A failed spawn drops reply_tx → the pump sees Disconnected and
        // reports a reply failure.
        let _ = std::thread::Builder::new()
            .name("aokie-agent-reply".to_string())
            .spawn(move || {
                let res = client.stream_reply(
                    messages,
                    &c,
                    || {
                        *a.lock().unwrap() = Some(Instant::now());
                    },
                    |sentence| {
                        reply_tx
                            .send(ReplyMsg::Sentence(sentence.to_string()))
                            .is_ok()
                    },
                );
                let _ = reply_tx.send(ReplyMsg::Done(res));
            })
            .map_err(|e| eprintln!("[aokie-plugin] reply worker failed to start: {e}"));
    }
    ReplyStream {
        rx,
        cancel,
        activity,
        answering,
        started: Instant::now(),
    }
}

/// The agent's system prompt at reply time: persona (call-scoped overlay wins
/// over the global one, §9.3) + the standing spoken-delivery instructions +
/// the end-call marker when agent hangup is on + the nudge tail when the
/// previous reply was interrupted. ONE composer for the real reply AND the
/// speculative start, so the adopted generation was primed identically.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn compose_agent_system_prompt(
    persona: &str,
    agent_hangup: bool,
    cut_context: Option<&str>,
    manager_call: bool,
) -> String {
    // An explicit "today" anchor: the model has to resolve relative dates
    // ("first Saturday of August", "next Tuesday") both when speaking and
    // when composing lookup questions, and without this line it had nothing
    // to resolve them against but conversational vibes.
    let today = aokie_core::events::today_spoken_local();
    // The [[MANAGER:]] marker is taught ONLY on manager-number calls (where
    // the marker legitimately routes into the PIN flow). Ordinary calls used
    // to carry the challenge instruction as a deterministic-denial funnel,
    // but small models kept marking ordinary own-bookings questions as
    // manager requests despite ever-sharper wording (live calls 8c689f13 and
    // 0576e7eb: "what do I have booked next week?" → "that's manager-only").
    // A caller who never hears about the marker can't be mis-routed by it;
    // the non-manager [[MANAGER]] refusal handler stays as a safety net.
    let manager = if manager_call {
        MANAGER_CHALLENGE_INSTRUCTION
    } else {
        ""
    };
    let mut p = if agent_hangup {
        format!("{persona}\n\nToday is {today}.{SPEECH_STYLE_INSTRUCTION}{BOOKING_INSTRUCTION}{TOOL_INSTRUCTION}{ASSISTANCE_INSTRUCTION}{TRANSFER_INSTRUCTION}{manager}{ABUSE_INSTRUCTION}{END_CALL_INSTRUCTION}")
    } else {
        format!("{persona}\n\nToday is {today}.{SPEECH_STYLE_INSTRUCTION}{BOOKING_INSTRUCTION}{TOOL_INSTRUCTION}{ASSISTANCE_INSTRUCTION}{TRANSFER_INSTRUCTION}{manager}{ABUSE_INSTRUCTION}")
    };
    if let Some(tail) = cut_context {
        p.push_str(&format!(
            "\n\nThe caller interrupted your previous reply. You were about to say: \"{tail}\". Respond to what they just said, weaving that pending point in ONLY if it is still relevant. Never repeat what you already said and never restart the reply."
        ));
    }
    p.push_str(CONVERSATION_GROUNDING_INSTRUCTION);
    p
}

/// Case/punctuation-insensitive word for hypothesis comparison.
#[cfg(feature = "voice")]
pub(super) fn norm_word(w: &str) -> String {
    w.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// Guide phase 5: is the live hypothesis STABLE enough to speculate on? The
/// previous partial's words must still be the (near-)prefix of the current
/// one — the head of the utterance stopped changing — and long enough to
/// carry an intent (≥4 words). One mid-prefix wobble is tolerated (STT
/// partials flicker on homophones).
#[cfg(feature = "voice")]
pub(super) fn hypothesis_stable(prev: &str, cur: &str) -> bool {
    let p: Vec<String> = prev.split_whitespace().map(norm_word).collect();
    let c: Vec<String> = cur.split_whitespace().map(norm_word).collect();
    if p.len() < 3 || c.len() < p.len() {
        return false;
    }
    // The HEAD is what must have stopped changing — the tail of a partial is
    // the unstable zone by definition, so requiring the WHOLE previous
    // partial to hold never fired on real calls (specLlmStarted stayed 0).
    // Compare the first min(4, len) words, one STT wobble tolerated.
    let head = p.len().min(4);
    let matches = p[..head]
        .iter()
        .zip(&c[..head])
        .filter(|(a, b)| a == b)
        .count();
    matches + 1 >= head
}

/// FormLogic's reserved Codex live-call adapters serialize requests behind a
/// single in-flight turn gate. A discarded interim-STT generation still owns
/// that gate until its exact cancellation terminal arrives, so immediately
/// starting the final generation can otherwise race into `codex_busy`.
/// Every other provider keeps the latency benefit of speculative generation.
#[cfg(feature = "voice")]
pub(super) fn llm_endpoint_allows_speculative_reply(endpoint: &str) -> bool {
    !crate::connector::is_codex_live_call_endpoint(endpoint)
}

/// Guide phase 5: may the speculative generation answer the FINAL turn? The
/// hypothesis must be a (near-)prefix of the final text — one wobble
/// tolerated — with at most a short tail the model never saw ("...please").
/// Anything else is a material revision: cancel and regenerate; speaking a
/// reply to something the caller revised is worse than the ~1 s regen cost.
#[cfg(feature = "voice")]
pub(super) fn hypothesis_covers(hyp: &str, fin: &str) -> bool {
    let h: Vec<String> = hyp.split_whitespace().map(norm_word).collect();
    let f: Vec<String> = fin.split_whitespace().map(norm_word).collect();
    if h.is_empty() || f.is_empty() || f.len() < h.len() {
        return false;
    }
    let matches = h.iter().zip(&f).filter(|(a, b)| a == b).count();
    matches + 1 >= h.len() && f.len() - h.len() <= 3
}

/// Deadlines for one agent reply, enforced by the radio thread's pump (the
/// worker may be stuck in a blocking read — reqwest 0.11 has no per-read
/// timeout — so the PUMP owns the deadline and abandons the worker, whose
/// whole-request timeout is the eventual backstop).
#[cfg(feature = "voice")]
pub(super) struct ReplyDeadlines {
    /// The endpoint accepted the request but produced NO stream data yet.
    pub(super) first_activity: std::time::Duration,
    /// Mid-stream: no data for this long (the per-read idle deadline).
    pub(super) idle: std::time::Duration,
    /// Whole-reply cap, regardless of progress.
    pub(super) total: std::time::Duration,
}

#[cfg(feature = "voice")]
pub(super) const REPLY_DEADLINES: ReplyDeadlines = ReplyDeadlines {
    first_activity: std::time::Duration::from_secs(10),
    idle: std::time::Duration::from_secs(8),
    total: std::time::Duration::from_secs(60),
};

/// Pure deadline verdict: `Some(reason)` when the reply must be abandoned.
/// `last_activity` is `None` until the stream's first line arrives.
#[cfg(feature = "voice")]
pub(super) fn reply_deadline_exceeded(
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
