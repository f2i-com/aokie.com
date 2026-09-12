//! Caller turn assembly: continuation holds, per-utterance audio pairing.

#[allow(unused_imports)]
use super::*;

/// How long a caller turn stays OPEN after a transcript that looks unfinished
/// (audit AK-008): callers read phone numbers in groups with pauses well past
/// the STT endpoint, and replying into that pause both talks over them and
/// books half a number. Long enough to bridge a between-groups breath, short
/// enough that a genuinely finished number only delays the reply by a beat.
#[cfg(feature = "voice")]
pub(super) const CONTINUATION_HOLD: Duration = Duration::from_millis(1400);

/// Cap on a merged caller turn — past this, flush regardless (a runaway hold
/// must never buffer the whole call into one turn).
#[cfg(feature = "voice")]
pub(super) const CONTINUATION_MAX_CHARS: usize = 240;

/// A complete phrase captured over the bot only needs a short chance to
/// continue. Numbers and unfinished thoughts retain their full breathing room.
#[cfg(feature = "voice")]
pub(super) fn continuation_delay(text: &str, from_overlap: bool) -> Duration {
    if text.len() >= CONTINUATION_MAX_CHARS { Duration::ZERO }
    else if turn_looks_unfinished(text) { CONTINUATION_HOLD }
    else if from_overlap { Duration::from_millis(450) }
    else { Duration::ZERO }
}

#[cfg(all(test, feature = "voice"))]
mod continuation_timing_tests {
    use super::*;
    #[test]
    fn completed_answers_resume_quickly_without_rushing_number_groups() {
        assert_eq!(continuation_delay("Yes, I can hear you clearly.", true), Duration::from_millis(450));
        assert_eq!(continuation_delay("Hello", false), Duration::ZERO);
        assert_eq!(continuation_delay("My number is 0412", true), CONTINUATION_HOLD);
        assert_eq!(continuation_delay("I wanted to ask because", false), CONTINUATION_HOLD);
    }
}

/// True when a transcript's tail says "the caller isn't done" (audit AK-008):
/// it ends in a digit group, a spoken number word, or a connective that
/// announces one ("my number is …"). Drives the continuation hold above.
#[cfg(feature = "voice")]
pub(super) fn ends_with_unfinished_number(text: &str) -> bool {
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
pub(super) fn turn_looks_unfinished(text: &str) -> bool {
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
pub(super) struct PendingTurn {
    pub(super) corr: String,
    pub(super) text: String,
    /// This turn was SEEDED from barge/overlap capture — the caller cut in
    /// while Aokie was speaking, so they are almost certainly mid-sentence.
    /// Gets ONE continuation-hold grace so the rest of the interruption
    /// merges into it instead of splitting (live call e150a269: "Yeah, do
    /// you have any" and "appointments next week?" landed as two turns).
    pub(super) from_overlap: bool,
    /// sendAudio: the utterance PCM(s) whose STT produced `text`, paired by
    /// utterance id and merged across continuation holds (tail-capped 30 s).
    /// A single shared "last audio" slot raced the turn flush (live call
    /// 94b9c792: corrections carried the NEIGHBOURING utterance's words) —
    /// the audio now travels WITH its turn.
    pub(super) audio: Vec<i16>,
    pub(super) flush_at: Instant,
}

/// sendAudio: stash a just-sent utterance's PCM keyed by its STT utterance
/// id — the result drain pairs it back into the pending turn. Bounded: a
/// result that never returns (worker death) just ages out.
#[cfg(feature = "voice")]
pub(super) fn stash_utt_audio(map: &mut std::collections::VecDeque<(u32, Vec<i16>)>, utt: u32, buf: &[f32]) {
    let start = buf.len().saturating_sub(16_000 * 30);
    let pcm: Vec<i16> = buf[start..]
        .iter()
        .map(|&x| (x.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    map.push_back((utt, pcm));
    while map.len() > 6 {
        map.pop_front();
    }
}

/// Take the stashed PCM for `utt` out of the map (None when aged out /
/// never stashed — e.g. sendAudio off).
#[cfg(feature = "voice")]
pub(super) fn take_utt_audio(
    map: &mut std::collections::VecDeque<(u32, Vec<i16>)>,
    utt: u32,
) -> Option<Vec<i16>> {
    let pos = map.iter().position(|(u, _)| *u == utt)?;
    map.remove(pos).map(|(_, pcm)| pcm)
}

/// Audio-understanding lane gates (2026-07-17): `sendAudio` (attach the
/// caller turn's PCM to the reply request) and `audioTranscript` (side-run
/// a detached transcript-correction request) are INDEPENDENT settings —
/// either may be on without the other. Both ride the same per-turn AUDIO
/// CAPTURE machinery (utterance-id → PCM stash paired into the flushed
/// turn), so capture runs when EITHER is on. Both are agent-mode features.
/// Returns `(send_audio, audio_transcript, audio_capture)`.
#[cfg(feature = "voice")]
pub(super) fn audio_lane_gates(
    agent_enabled: bool,
    send_audio_env: bool,
    audio_transcript_env: bool,
) -> (bool, bool, bool) {
    let send_audio = agent_enabled && send_audio_env;
    let audio_transcript = agent_enabled && audio_transcript_env;
    (send_audio, audio_transcript, send_audio || audio_transcript)
}

/// Merge one utterance's PCM into its turn's accumulated audio, keeping the
/// most recent 30 s (the WAV cap the LLM request also uses).
#[cfg(feature = "voice")]
pub(super) fn append_turn_audio(dst: &mut Vec<i16>, src: Vec<i16>) {
    dst.extend(src);
    let cap = 16_000 * 30;
    if dst.len() > cap {
        let drop = dst.len() - cap;
        dst.drain(..drop);
    }
}
