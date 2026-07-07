//! `TtsProvider` — the swappable text-to-speech surface.
//!
//! Adapters: Pocket-TTS-ONNX (existing), sherpa-onnx (Phase 3,
//! covers Piper / Kokoro / VITS), OpenAI / ElevenLabs HTTP, system
//! TTS. All emit mono f32 PCM via the streaming chunk callback.

use async_trait::async_trait;

#[derive(Debug, Clone, Default)]
pub struct TtsCapabilities {
    /// Native sample rate of the synthesizer's output. Callers
    /// resample to whatever the audio sink wants (mSBC handoff is
    /// 16 kHz, the desktop player runs at 48 kHz, etc.).
    pub sample_rate: u32,
    /// True when the provider actually emits chunks as they're
    /// produced. Providers without streaming buffer the whole
    /// utterance and emit it as a single chunk.
    pub streaming: bool,
    /// Voices the provider exposes. Empty when voices are
    /// configured per-provider out-of-band (e.g. ElevenLabs voice id
    /// in settings, sherpa speaker_id).
    pub voices: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TtsRequest {
    pub text: String,
    /// Voice name / id understood by the chosen provider.
    /// Empty string = "use whatever default the provider has."
    pub voice: String,
}

/// One piece of synthesized audio, with its native sample rate so
/// callers can decide whether to resample. Providers stream these in
/// roughly utterance-length chunks (sentence or sub-sentence).
#[derive(Debug, Clone)]
pub struct TtsChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

#[derive(Debug)]
pub enum TtsError {
    NotReady(String),
    InvalidInput(String),
    Synthesize(String),
    Interrupted,
}

impl std::fmt::Display for TtsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TtsError::NotReady(s) => write!(f, "TTS not ready: {}", s),
            TtsError::InvalidInput(s) => write!(f, "invalid TTS input: {}", s),
            TtsError::Synthesize(s) => write!(f, "TTS synthesis failed: {}", s),
            TtsError::Interrupted => write!(f, "TTS interrupted"),
        }
    }
}

impl std::error::Error for TtsError {}

/// Audio chunk sink. Same `false-to-stop` contract as
/// `LlmProvider::TokenSink` — used to interrupt mid-utterance when
/// the caller starts speaking and barge-in fires.
pub type ChunkSink = Box<dyn FnMut(TtsChunk) -> bool + Send>;

/// Rewrite clock-style time strings into a TTS-friendlier shape before
/// synthesis. Pure text transform with no audio side effects.
///
/// Why: Pocket-TTS and Sherpa-ONNX both choke on a literal colon —
/// "11:00 am" comes out as "eleven colon zero zero a m" or similar
/// digit-by-digit garbage. Two small rewrites tame the worst of it:
///
/// - `HH:MM` → `HH MM` so the synth treats hour and minutes as two
///   ordinary numeric tokens instead of stumbling on the colon.
/// - When `MM` is `00`, drop it entirely so "11:00 am" reads as
///   "11 AM" instead of "eleven zero zero ay em".
/// - Uppercase any `am`/`pm` suffix and fold out the dots in
///   `a.m.` / `p.m.` so it gets pronounced as two letters rather than
///   blended into a syllable.
///
/// Applied **only** at the TTS boundary. The transcript persisted to
/// history and emitted to the live UI keeps the operator-readable
/// "11:15 am" — what the caller would have read on screen.
///
/// Conservative on edge cases: `HH:MM:SS` timestamps, ratios like
/// `5:30 ratio`, version strings like `v1:30 release`, and times
/// embedded inside word characters all pass through untouched. The
/// LLM doesn't emit those in spoken replies in practice; if one slips
/// through we'd rather under-transform than mangle.
pub fn humanize_for_speech(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut last_copied = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        // Only attempt a match at a left boundary. The previous byte
        // must be neither a word char (avoid mid-identifier matches
        // like "v1:30") nor another colon (so "11:30:45" doesn't get
        // re-entered at the second segment and turned into "11:30 45").
        let at_left = i == 0 || (!is_word_byte(bytes[i - 1]) && bytes[i - 1] != b':');
        if at_left {
            if let Some((consumed, replacement)) = try_match_time(&bytes[i..]) {
                out.push_str(&text[last_copied..i]);
                out.push_str(&replacement);
                i += consumed;
                last_copied = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[last_copied..]);
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Try to consume an `HH:MM` (optionally `am`/`pm`) at the start of
/// `b`. Returns `(bytes_consumed, replacement)` on match.
fn try_match_time(b: &[u8]) -> Option<(usize, String)> {
    // Hour: 1-2 ASCII digits.
    let mut p = 0usize;
    while p < b.len() && p < 2 && b[p].is_ascii_digit() {
        p += 1;
    }
    if p == 0 {
        return None;
    }
    let hour_raw = &b[..p];
    // Colon must follow the hour digits.
    if p >= b.len() || b[p] != b':' {
        return None;
    }
    p += 1;
    // Minute: exactly 2 ASCII digits.
    if p + 2 > b.len() || !b[p].is_ascii_digit() || !b[p + 1].is_ascii_digit() {
        return None;
    }
    let minute = [b[p], b[p + 1]];
    p += 2;
    // Reject if the minute is followed by another colon or another
    // digit — that means an HH:MM:SS timestamp or a longer digit run
    // we shouldn't be touching.
    if p < b.len() && (b[p] == b':' || b[p].is_ascii_digit()) {
        return None;
    }
    // Optional am/pm marker, with optional whitespace and trailing dots.
    let after_minute = p;
    let mut q = p;
    while q < b.len() && b[q] == b' ' {
        q += 1;
    }
    let suffix = match try_match_am_pm(&b[q..]) {
        Some((len, kind)) => {
            let end = q + len;
            // Right-side word boundary so "9:00amazing" doesn't bind
            // "am" into the meridiem.
            if end >= b.len() || !is_word_byte(b[end]) {
                p = end;
                Some(kind)
            } else {
                p = after_minute;
                None
            }
        }
        None => {
            p = after_minute;
            None
        }
    };
    let hour_str = std::str::from_utf8(hour_raw).ok()?;
    // Strip a single leading zero ("08" → "8") so the synth doesn't
    // say "zero eight". A bare "0" (zero hour) is rare enough to leave
    // alone.
    let hour_pretty = if hour_str.len() == 2 && hour_str.starts_with('0') {
        &hour_str[1..]
    } else {
        hour_str
    };
    let suffix_str = match suffix {
        Some(true) => " PM",
        Some(false) => " AM",
        None => "",
    };
    let replacement = if minute == *b"00" {
        format!("{}{}", hour_pretty, suffix_str)
    } else {
        let minute_str = std::str::from_utf8(&minute).ok()?;
        format!("{} {}{}", hour_pretty, minute_str, suffix_str)
    };
    Some((p, replacement))
}

/// Match an am/pm marker. Accepts `am` / `AM` / `a.m` / `a.m.` /
/// `pm` / `PM` / `p.m` / `p.m.` (case-insensitive). Returns
/// `(bytes_consumed, is_pm)` on match.
///
/// A trailing dot is only consumed when an internal dot was already
/// present (the `a.m.` shape). Without the internal dot, a following
/// `.` is a sentence terminator — consuming it would eat the
/// punctuation off the end of "...at 2:30pm." and silently strip
/// punctuation from spoken sentences.
fn try_match_am_pm(b: &[u8]) -> Option<(usize, bool)> {
    if b.is_empty() {
        return None;
    }
    let is_pm = match b[0].to_ascii_lowercase() {
        b'a' => false,
        b'p' => true,
        _ => return None,
    };
    let mut p = 1usize;
    let saw_internal_dot = p < b.len() && b[p] == b'.';
    if saw_internal_dot {
        p += 1;
    }
    if p >= b.len() || b[p].to_ascii_lowercase() != b'm' {
        return None;
    }
    p += 1;
    if saw_internal_dot && p < b.len() && b[p] == b'.' {
        p += 1;
    }
    Some((p, is_pm))
}

#[cfg(test)]
mod humanize_for_speech_tests {
    use super::humanize_for_speech;

    #[test]
    fn drops_double_zero_minutes_with_meridiem() {
        assert_eq!(humanize_for_speech("11:00 am"), "11 AM");
        assert_eq!(humanize_for_speech("11:00am"), "11 AM");
        assert_eq!(humanize_for_speech("11:00 PM"), "11 PM");
        assert_eq!(humanize_for_speech("9:00 a.m."), "9 AM");
        assert_eq!(humanize_for_speech("9:00 p.m."), "9 PM");
    }

    #[test]
    fn preserves_minutes_when_nonzero() {
        assert_eq!(humanize_for_speech("11:15 am"), "11 15 AM");
        assert_eq!(humanize_for_speech("2:30 pm"), "2 30 PM");
        assert_eq!(humanize_for_speech("9:45am"), "9 45 AM");
    }

    #[test]
    fn handles_no_meridiem_times() {
        assert_eq!(humanize_for_speech("14:30"), "14 30");
        assert_eq!(humanize_for_speech("9:00"), "9");
    }

    #[test]
    fn strips_leading_zero_on_two_digit_hour() {
        assert_eq!(humanize_for_speech("08:30"), "8 30");
        assert_eq!(humanize_for_speech("08:00 AM"), "8 AM");
    }

    #[test]
    fn rewrites_inside_full_sentence() {
        assert_eq!(
            humanize_for_speech("I have you booked for Tuesday May 15 at 2:30pm."),
            "I have you booked for Tuesday May 15 at 2 30 PM."
        );
        assert_eq!(
            humanize_for_speech("The window is 10:00 to 11:30."),
            "The window is 10 to 11 30."
        );
    }

    #[test]
    fn leaves_text_without_clock_times_alone() {
        let s = "Aokie picks up calls in your voice.";
        assert_eq!(humanize_for_speech(s), s);
        // "9 a.m." (no colon) is already pronounced fine by the local
        // synths, so we don't touch it.
        assert_eq!(
            humanize_for_speech("Meeting at 9 a.m. tomorrow."),
            "Meeting at 9 a.m. tomorrow."
        );
    }

    #[test]
    fn passes_hh_mm_ss_timestamps_through() {
        // First-pass match at "11:30" sees the trailing colon and
        // backs off; subsequent positions are blocked by the colon
        // boundary check, so the whole timestamp passes through.
        assert_eq!(
            humanize_for_speech("Logged at 11:30:45 PM"),
            "Logged at 11:30:45 PM"
        );
    }

    #[test]
    fn does_not_eat_letters_after_minute() {
        // "9:00amazing" must not bind "am" into the meridiem.
        assert_eq!(humanize_for_speech("9:00amazing"), "9amazing");
    }

    #[test]
    fn does_not_match_inside_identifiers() {
        // "v1:30" — previous byte "1" is a word char, so don't fire.
        assert_eq!(
            humanize_for_speech("see v1:30 release"),
            "see v1:30 release"
        );
    }

    #[test]
    fn idempotent_on_already_normalized_output() {
        let once = humanize_for_speech("11:15 am");
        assert_eq!(once, "11 15 AM");
        assert_eq!(humanize_for_speech(&once), "11 15 AM");
    }

    #[test]
    fn preserves_unicode_around_match() {
        // Multi-byte UTF-8 around the match must round-trip intact.
        assert_eq!(
            humanize_for_speech("café at 11:00 am — see you"),
            "café at 11 AM — see you"
        );
    }
}

#[async_trait]
pub trait TtsProvider: Send + Sync {
    fn id(&self) -> &str;

    fn capabilities(&self) -> TtsCapabilities;

    /// Stream synthesized audio chunks through `on_chunk`. Returns
    /// when the utterance is complete or `on_chunk` returns false.
    async fn synthesize_stream(&self, req: TtsRequest, on_chunk: ChunkSink)
        -> Result<(), TtsError>;
}
