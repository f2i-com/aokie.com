//! Transcript settlement tracking and the audio-model transcript correction lane.

#[allow(unused_imports)]
use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Detached audio-transcript corrections deliberately yield to the live reply
/// lane. Keep `call.ended` immediate for lifecycle consumers, then emit one
/// separate terminal transcript event after every correction for that call has
/// reported back. A hard deadline prevents a failed/panicked model worker from
/// suppressing after-call automation forever.
pub(super) const TRANSCRIPT_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40);

#[derive(Debug, Clone)]
pub(super) struct PendingTranscriptSettlement {
    pub(super) data: serde_json::Value,
    pub(super) deadline: std::time::Instant,
}

#[derive(Debug, Clone)]
pub(super) struct ReadyTranscriptSettlement {
    pub(super) call_id: String,
    pub(super) data: serde_json::Value,
    pub(super) correction_timed_out: bool,
}

#[derive(Debug, Default)]
pub(super) struct TranscriptSettleTracker {
    pub(super) corrections_in_flight: std::collections::HashMap<String, usize>,
    pub(super) ended: std::collections::HashMap<String, PendingTranscriptSettlement>,
}

impl TranscriptSettleTracker {
    #[cfg(any(feature = "voice", test))]
    pub(super) fn correction_started(&mut self, call_id: &str) {
        *self
            .corrections_in_flight
            .entry(call_id.to_string())
            .or_default() += 1;
    }

    #[cfg(any(feature = "voice", test))]
    pub(super) fn correction_finished(&mut self, call_id: &str) {
        let Some(count) = self.corrections_in_flight.get_mut(call_id) else {
            // A late completion after the bounded timeout is still allowed to
            // emit its corrected-turn event, but cannot mint a second settled
            // event for the same call.
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.corrections_in_flight.remove(call_id);
        }
    }

    pub(super) fn call_ended(&mut self, call_id: &str, data: serde_json::Value, now: std::time::Instant) {
        self.ended
            .entry(call_id.to_string())
            .or_insert(PendingTranscriptSettlement {
                data,
                deadline: now + TRANSCRIPT_SETTLE_TIMEOUT,
            });
    }

    pub(super) fn ready(&self, now: std::time::Instant) -> Vec<ReadyTranscriptSettlement> {
        self.ended
            .iter()
            .filter_map(|(call_id, pending)| {
                let correction_pending = self
                    .corrections_in_flight
                    .get(call_id)
                    .copied()
                    .unwrap_or_default()
                    > 0;
                if !correction_pending {
                    Some(ReadyTranscriptSettlement {
                        call_id: call_id.clone(),
                        data: pending.data.clone(),
                        correction_timed_out: false,
                    })
                } else if now >= pending.deadline {
                    Some(ReadyTranscriptSettlement {
                        call_id: call_id.clone(),
                        data: pending.data.clone(),
                        correction_timed_out: true,
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    pub(super) fn all_ended_as_timed_out(&self) -> Vec<ReadyTranscriptSettlement> {
        self.ended
            .iter()
            .map(|(call_id, pending)| ReadyTranscriptSettlement {
                call_id: call_id.clone(),
                data: pending.data.clone(),
                correction_timed_out: true,
            })
            .collect()
    }

    pub(super) fn mark_settled(&mut self, call_id: &str) {
        self.ended.remove(call_id);
        self.corrections_in_flight.remove(call_id);
    }

    #[cfg(test)]
    pub(super) fn take_ready(&mut self, now: std::time::Instant) -> Vec<ReadyTranscriptSettlement> {
        let ready = self.ready(now);
        for settlement in &ready {
            self.mark_settled(&settlement.call_id);
        }
        ready
    }
}

#[cfg(feature = "voice")]
pub(super) struct TranscriptCorrectionResult {
    pub(super) call_id: String,
    pub(super) turn: u32,
    pub(super) stt: String,
    pub(super) result: Result<String, String>,
}

// The radio loop is deliberately single-threaded. Keep its bounded settlement
// ledger thread-local so lifecycle helpers (including device-loss paths) share
// one ledger without exposing it through RadioStatus or crossing an authority
// boundary. Detached model workers report completion over `heard_rx`; they
// never touch this state directly.
std::thread_local! {
    pub(super) static TRANSCRIPT_SETTLEMENTS: std::cell::RefCell<TranscriptSettleTracker> =
        std::cell::RefCell::new(TranscriptSettleTracker::default());
}

#[cfg(feature = "voice")]
pub(super) fn transcript_correction_started(call_id: &str) {
    TRANSCRIPT_SETTLEMENTS.with(|tracker| tracker.borrow_mut().correction_started(call_id));
}

#[cfg(feature = "voice")]
pub(super) fn transcript_correction_finished(call_id: &str) {
    TRANSCRIPT_SETTLEMENTS.with(|tracker| tracker.borrow_mut().correction_finished(call_id));
}

/// Concurrency ceiling for correction workers (audit AK-07): the live reply
/// lane always wins the GPU — two background corrections in flight is plenty,
/// anything beyond it is dropped (never queued: a queued correction of a
/// minutes-old turn has no value and still pins its PCM copy).
pub(super) const MAX_CONCURRENT_CORRECTIONS: usize = 2;
static ACTIVE_CORRECTIONS: AtomicUsize = AtomicUsize::new(0);
static DROPPED_CORRECTIONS: AtomicU64 = AtomicU64::new(0);

/// RAII permit against [`ACTIVE_CORRECTIONS`] — released on every worker exit
/// path (audit AK-07), so a panicking worker can never leak the slot.
pub(super) struct CorrectionPermit;

impl CorrectionPermit {
    pub(super) fn try_acquire() -> Option<Self> {
        let acquired = ACTIVE_CORRECTIONS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                (active < MAX_CONCURRENT_CORRECTIONS).then_some(active + 1)
            })
            .is_ok();
        acquired.then_some(CorrectionPermit)
    }
}

impl Drop for CorrectionPermit {
    fn drop(&mut self) {
        ACTIVE_CORRECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Total corrections dropped at the cap — visible in logs for tuning.
pub(super) fn dropped_corrections() -> u64 {
    DROPPED_CORRECTIONS.load(Ordering::Relaxed)
}

/// Origin + path only (audit AK-10): configured endpoints may carry API
/// tokens or tenant ids in the query string — strip query and fragment
/// before anything reaches a log line. The request itself still uses the
/// full configured URL.
pub(super) fn redact_endpoint_for_log(endpoint: &str) -> String {
    let end = endpoint
        .find(['?', '#'])
        .unwrap_or(endpoint.len());
    endpoint[..end].to_string()
}

/// PII gate for conversation content in logs (audit PRIV-001/C-06): stderr is
/// captured by Desktop's log ring, so caller speech and agent replies appear
/// verbatim only when the operator explicitly opts in (`AOKIE_LOG_CONTENT=1`);
/// default logs carry only lengths.
#[cfg(feature = "voice")]
pub(super) fn content_for_log(text: &str) -> String {
    if std::env::var("AOKIE_LOG_CONTENT")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        format!("{text:?}")
    } else {
        format!("[{} chars]", text.chars().count())
    }
}

/// audioTranscript: tidy the audio model's corrected transcription before it
/// replaces a transcript line. Markers the model may have imitated from its
/// reply training are stripped, whitespace collapsed, surrounding quotes
/// dropped; an empty, oversized, or unchanged result means "no correction"
/// (None) — the STT text stays, and no corrected event is emitted.
#[cfg(feature = "voice")]
pub(super) fn sanitize_heard(raw: &str, stt: &str) -> Option<String> {
    let mut s = raw.trim().to_string();
    while let (Some(a), Some(rel)) = (s.find("[["), s.find("[[").and_then(|a| s[a..].find("]]"))) {
        s.replace_range(a..a + rel + 2, " ");
    }
    let s = s.trim().trim_matches('"').trim_matches('\'').trim();
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.chars().count() > 800 {
        return None;
    }
    if collapsed == stt.trim() {
        return None;
    }
    // Length-ratio guard: a short utterance cannot honestly become a long
    // sentence — that shape is the model transcribing the conversation
    // CONTEXT instead of the audio (live call de1834ef: 'Uh' → a ten-word
    // sentence copied from an earlier turn). Generous bound: real
    // corrections change words, they don't multiply them.
    let stt_words = stt.split_whitespace().count();
    let heard_words = collapsed.split_whitespace().count();
    if heard_words > stt_words * 3 + 4 {
        return None;
    }
    Some(collapsed)
}

/// audioTranscript: the last few conversation turns as a compact text block
/// for the correction request — dialogue context resolves ambiguous audio
/// toward the right domain words ("ointments" → "appointments" at a dental
/// receptionist). Text only, clipped per turn, markers stripped (assistant
/// history entries keep their [[markers]] by design — they must never reach
/// another prompt as instructions).
#[cfg(feature = "voice")]
pub(super) fn heard_context(history: &[serde_json::Value]) -> String {
    let mut turns: Vec<String> = history
        .iter()
        .rev()
        .filter_map(|m| {
            let role = m.get("role").and_then(serde_json::Value::as_str)?;
            let content = m.get("content").and_then(serde_json::Value::as_str)?;
            let who = match role {
                "assistant" => "Receptionist",
                "user" => "Caller",
                _ => return None,
            };
            let mut c = content.replace('\n', " ");
            while let (Some(a), Some(rel)) =
                (c.find("[["), c.find("[[").and_then(|a| c[a..].find("]]")))
            {
                c.replace_range(a..a + rel + 2, " ");
            }
            let c = c.split_whitespace().collect::<Vec<_>>().join(" ");
            if c.is_empty() {
                return None;
            }
            let clipped: String = c.chars().take(160).collect();
            Some(format!("{who}: {clipped}"))
        })
        .take(6)
        .collect();
    turns.reverse();
    turns.join("\n")
}

/// Start one detached audio-transcript correction, including for a caller
/// turn that was flushed by a call boundary. Registration happens on the
/// radio thread before `call.ended` can settle; the worker only reports its
/// typed completion back over `heard_tx`.
#[cfg(feature = "voice")]
#[allow(clippy::too_many_arguments)]
pub(super) fn maybe_spawn_transcript_correction(
    audio_transcript: bool,
    call_id: &str,
    turn: u32,
    stt: &str,
    turn_audio: &[i16],
    prev_heard: Option<&(Vec<i16>, String, Instant)>,
    history: &[serde_json::Value],
    setting: &str,
    heard_client: Option<crate::agent::LlmClient>,
    transcript_client_cache: &Arc<std::sync::OnceLock<Option<crate::agent::LlmClient>>>,
    heard_tx: &std::sync::mpsc::Sender<TranscriptCorrectionResult>,
) -> bool {
    if !audio_transcript {
        return false;
    }
    let worthwhile = !crate::duplex::is_hesitation(stt) && stt.split_whitespace().count() > 2;
    if !worthwhile {
        eprintln!(
            "[aokie-plugin] audio transcript check skipped [turn {turn}]: hesitation/too short"
        );
        return false;
    }
    if turn_audio.is_empty() {
        eprintln!(
            "[aokie-plugin] audio transcript check skipped [turn {turn}]: no paired audio for this turn"
        );
        return false;
    }
    let Some(heard_client) = heard_client else {
        eprintln!(
            "[aokie-plugin] audio transcript check skipped [turn {turn}]: no connected LLM client yet"
        );
        return false;
    };

    // Split-utterance continuity: a recent previous turn's audio lets the
    // model hear one sentence across a mid-thought pause; the prompt and
    // length guard still constrain output to this final STT draft.
    let (pcm, prev_draft) = match prev_heard {
        Some((previous_pcm, previous_text, at)) if at.elapsed() < Duration::from_secs(8) => {
            let mut combined = previous_pcm.clone();
            append_turn_audio(&mut combined, turn_audio.to_vec());
            (combined, Some(previous_text.clone()))
        }
        _ => (turn_audio.to_vec(), None),
    };
    let setting: String = setting
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .take(160)
        .collect();
    let mut correction_context = format!("Setting: {}", setting.trim());
    let dialogue = heard_context(history);
    if !dialogue.is_empty() {
        correction_context.push('\n');
        correction_context.push_str(&dialogue);
    }

    // Bounded executor (audit AK-07): each worker holds a PCM copy, an OS
    // thread, and eventually a GPU/HTTP inference slot. A long or rapid call
    // must not accumulate an unbounded pile of those — at the cap this turn's
    // correction is DROPPED (the original STT stands; corrections are
    // best-effort polish), counted, and the radio thread never blocks.
    let Some(permit) = CorrectionPermit::try_acquire() else {
        let dropped = DROPPED_CORRECTIONS.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!(
            "[aokie-plugin] audio transcript check dropped [turn {turn}]: {MAX_CONCURRENT_CORRECTIONS} corrections already in flight ({dropped} dropped total)"
        );
        return false;
    };

    let call_id = call_id.to_string();
    let stt = stt.to_string();
    let tx = heard_tx.clone();
    let client_cache = transcript_client_cache.clone();
    let samples = pcm.len();
    let includes_previous = prev_draft.is_some();
    let worker_call_id = call_id.clone();
    let worker_stt = stt.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("aokie-transcript-{turn}"))
        .spawn(move || {
            // The permit lives for the WHOLE worker (sleep + inference) and is
            // released on every exit path, including panics.
            let _permit = permit;
            // Yield the GPU to the live reply first. Corrections update the
            // transcript in place, so a short delay is preferable to making
            // the caller wait for an answer.
            std::thread::sleep(Duration::from_millis(2500));
            let heard_client = {
                let override_client = client_cache.get_or_init(|| {
                    let endpoint =
                        std::env::var("AOKIE_AUDIO_TRANSCRIPT_ENDPOINT").unwrap_or_default();
                    let endpoint = endpoint.trim().to_string();
                    if endpoint.is_empty() {
                        return None;
                    }
                    let model = std::env::var("AOKIE_AUDIO_TRANSCRIPT_MODEL")
                        .ok()
                        .map(|model| model.trim().to_string())
                        .filter(|model| !model.is_empty());
                    // Audit AK-10: log only the redacted origin+path — a
                    // query-string API token or tenant id in the configured
                    // endpoint must never enter the plugin/Desktop logs.
                    eprintln!(
                        "[aokie-plugin] transcript corrections → separate endpoint {}",
                        redact_endpoint_for_log(&endpoint)
                    );
                    Some(crate::agent::LlmClient::new(endpoint, model))
                });
                match override_client {
                    Some(client) => client.clone(),
                    None => match std::env::var("AOKIE_AUDIO_TRANSCRIPT_MODEL") {
                        Ok(model) if !model.trim().is_empty() => {
                            heard_client.with_model(model.trim().to_string())
                        }
                        _ => heard_client,
                    },
                }
            };
            let pcm = crate::agent::trim_silence_for_llm(&pcm, 16_000);
            let wav = crate::agent::LlmClient::wav_base64(&pcm, 16_000);
            let result = heard_client.transcribe_turn(
                &wav,
                &worker_stt,
                &correction_context,
                prev_draft.as_deref(),
            );
            let _ = tx.send(TranscriptCorrectionResult {
                call_id: worker_call_id,
                turn,
                stt: worker_stt,
                result,
            });
        });

    match spawned {
        Ok(_) => {
            transcript_correction_started(&call_id);
            eprintln!(
                "[aokie-plugin] audio transcript check spawned [turn {turn}] ({samples} samples{})",
                if includes_previous {
                    ", incl. previous-turn audio"
                } else {
                    ""
                }
            );
            true
        }
        Err(error) => {
            eprintln!(
                "[aokie-plugin] transcript correction worker failed to start [turn {turn}]: {error}"
            );
            false
        }
    }
}

/// Heuristic self-echo guard for the in-plugin agent: true when `caller` (a fresh
/// transcript) is mostly the same words as Aokie's last spoken reply `bot` â€” i.e.
/// Aokie's own TTS leaked back into the mic and STT transcribed it. Keeps Aokie
/// from answering itself if any audio escapes the half-duplex mute.
#[cfg(feature = "voice")]
pub(super) fn looks_like_echo(caller: &str, bot: &str) -> bool {
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

#[cfg(all(test, feature = "voice"))]
pub(super) mod heard_context_tests {
    use super::heard_context;

    #[test]
    fn recent_turns_compact_with_markers_stripped() {
        let history = vec![
            serde_json::json!({"role": "user", "content": "oldest turn that falls off"}),
            serde_json::json!({"role": "assistant", "content": "second oldest, also dropped"}),
            serde_json::json!({"role": "user", "content": "kept one"}),
            serde_json::json!({"role": "assistant", "content": "kept two"}),
            serde_json::json!({"role": "user", "content": "Do I have any appointments?"}),
            serde_json::json!({"role": "assistant", "content": "You have a checkup Wednesday. [[LOOKUP: availability 2026-07-22]]"}),
            serde_json::json!({"role": "user", "content": "What\ntime is\nit at?"}),
            serde_json::json!({"role": "assistant", "content": "It's at 3 PM."}),
        ];
        let ctx = heard_context(&history);
        assert_eq!(
            ctx,
            "Caller: kept one\nReceptionist: kept two\nCaller: Do I have any appointments?\nReceptionist: You have a checkup Wednesday.\nCaller: What time is it at?\nReceptionist: It's at 3 PM."
        );
        assert!(!ctx.contains("oldest turn"), "capped at the last 6 turns");
        assert!(!ctx.contains("[["), "markers never reach another prompt");
    }

    #[test]
    fn empty_history_means_empty_context() {
        assert_eq!(heard_context(&[]), "");
    }
}

#[cfg(all(test, feature = "voice"))]
pub(super) mod sanitize_heard_tests {
    use super::{redact_endpoint_for_log, sanitize_heard, CorrectionPermit};

    #[test]
    fn plain_correction_passes_collapsed() {
        assert_eq!(
            sanitize_heard(
                "  I'd like to book a  table\nfor two ",
                "I'd like to look a table for two"
            ),
            Some("I'd like to book a table for two".to_string())
        );
    }

    #[test]
    fn unchanged_or_empty_means_no_correction() {
        assert_eq!(sanitize_heard("hello there", "hello there"), None);
        assert_eq!(sanitize_heard("   ", "anything"), None);
        assert_eq!(sanitize_heard("\"hello there\"", "hello there"), None); // quotes stripped, then unchanged
    }

    #[test]
    fn imitated_markers_and_quotes_are_stripped() {
        assert_eq!(
            sanitize_heard(
                "[[WAIT]] \"Can I move my booking?\"",
                "can I moo my booking"
            ),
            Some("Can I move my booking?".to_string())
        );
        // An unclosed marker never loops forever.
        assert_eq!(
            sanitize_heard("[[HEARD hello", "x"),
            Some("[[HEARD hello".to_string())
        );
    }

    #[test]
    fn oversized_output_is_rejected() {
        assert_eq!(sanitize_heard(&"word ".repeat(300), "short"), None);
    }

    #[test]
    fn context_hallucination_shape_is_rejected() {
        // Live call de1834ef: 'Uh' came back as a full sentence copied from
        // the conversation context — a short utterance can never honestly
        // become a long one.
        assert_eq!(
            sanitize_heard(
                "Hi, I just want to check my appointments for next week",
                "Uh"
            ),
            None
        );
        // Real corrections of comparable length still pass.
        assert_eq!(
            sanitize_heard(
                "We already have an appointment booked, don't I?",
                "already have a ploint book don't I?"
            ),
            Some("We already have an appointment booked, don't I?".to_string())
        );
        // A fast mumble legitimately expanding a little passes too.
        assert_eq!(
            sanitize_heard("next Tuesday please", "nex"),
            Some("next Tuesday please".to_string())
        );
    }

    /// Audit AK-07: at most MAX_CONCURRENT_CORRECTIONS workers hold permits,
    /// and a dropped permit frees its slot (RAII — panics included).
    #[test]
    fn correction_permits_are_bounded_and_raii() {
        let a = CorrectionPermit::try_acquire().expect("first permit");
        let b = CorrectionPermit::try_acquire().expect("second permit");
        assert!(
            CorrectionPermit::try_acquire().is_none(),
            "the third concurrent correction must be refused"
        );
        drop(a);
        let c = CorrectionPermit::try_acquire().expect("released slot is reusable");
        drop(b);
        drop(c);
    }

    /// Audit AK-10: neither the query string (tokens) nor the fragment may
    /// survive into a log line.
    #[test]
    fn endpoint_log_redaction_strips_query_and_fragment() {
        assert_eq!(
            redact_endpoint_for_log("https://host/v1/chat?token=secret#x"),
            "https://host/v1/chat"
        );
        assert_eq!(
            redact_endpoint_for_log("https://host/v1/chat#frag"),
            "https://host/v1/chat"
        );
        assert_eq!(
            redact_endpoint_for_log("http://127.0.0.1:8080/v1/audio"),
            "http://127.0.0.1:8080/v1/audio"
        );
    }
}
