//! Validated speech plans: per-span speaking rate + interruption policy.
//!
//! Everything Aokie says is broken into SPANS before synthesis. A span
//! carries its own speaking rate (a phone number reads slower than the
//! sentence around it) and its own interruption policy (a short critical
//! detail finishes its bounded clause under ordinary overlap instead of
//! dying mid-digits — while explicit controls always win).
//!
//! Three inputs shape a plan, in trust order:
//!
//! 1. **Deterministic detail segmentation** — phone-number/code digit runs
//!    are detected here, slowed to the call's detail rate, expanded to
//!    spoken digit groups for the synthesizer, and given a bounded
//!    finish-the-phrase policy. Correct pacing never depends on the model.
//! 2. **Call-local pace state** — the caller's own "slower / faster /
//!    normal speed" commands move the base rate between bounded steps.
//! 3. **Model markers** — the LLM may wrap a detail in `[[slow]]…[[/slow]]`
//!    / `[[rate=0.8]]…[[/rate]]` or a single vital sentence in
//!    `[[important]]…[[/important]]`. Marker input is UNTRUSTED: rates are
//!    clamped, protection duration is capped by the operator setting, and
//!    every bracketed token — recognized or not — is stripped so markup can
//!    never be spoken or recorded.
//!
//! Span `text` is what transcripts record (plain, marker-free); `tts_text`
//! is what the synthesizer speaks (digit runs expanded to words so "0412"
//! is read as digits, never "four hundred and twelve").

/// Percent defaults for the two pace knobs (`defaultSpeechRate` /
/// `detailSpeechRate` settings; env AOKIE_SPEECH_RATE_PCT /
/// AOKIE_DETAIL_RATE_PCT at radio spawn).
pub const DEFAULT_BASE_RATE_PCT: u64 = 100;
pub const DEFAULT_DETAIL_RATE_PCT: u64 = 75;

/// Base-rate band for live "slower"/"faster" voice commands.
const BASE_RATE_MIN: f32 = 0.6;
const BASE_RATE_MAX: f32 = 1.4;
/// Detail spans may only ever be slow-to-normal.
const DETAIL_RATE_MIN: f32 = 0.5;
const DETAIL_RATE_MAX: f32 = 1.0;
/// One "slower"/"faster" voice-command step.
const RATE_STEP: f32 = 0.15;
/// Span rates the model may request are clamped into this band.
const SPAN_RATE_MIN: f32 = 0.5;
const SPAN_RATE_MAX: f32 = 1.5;

/// How long a digit-run span keeps playing after a barge before yielding —
/// long enough to finish a digit group, short enough that the caller is
/// never fighting the bot.
pub const FINISH_PHRASE_MS: u32 = 1500;

/// `protectedSpeechMaxMs` bounds (default 2500): the cap on how long an
/// `[[important]]` span may keep playing once the caller has started
/// talking over it. Explicit controls (hangup/reject) still cut instantly.
pub const PROTECTED_MAX_MS_DEFAULT: u32 = 2500;
pub const PROTECTED_MAX_MS_MIN: u32 = 500;
pub const PROTECTED_MAX_MS_MAX: u32 = 5000;

/// Read the operator's protected-span cap (set by the connector from the
/// `protectedSpeechMaxMs` setting at radio spawn).
pub fn protected_max_ms_from_env() -> u32 {
    std::env::var("AOKIE_PROTECTED_MAX_MS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|v| v.clamp(PROTECTED_MAX_MS_MIN, PROTECTED_MAX_MS_MAX))
        .unwrap_or(PROTECTED_MAX_MS_DEFAULT)
}

/// What happens when the caller starts speaking over a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptPolicy {
    /// Stop now (the default): the caller has the floor.
    Yield,
    /// Keep playing this bounded span for at most `max_extra_ms` after the
    /// overlap started, then yield. Everything the caller says is still
    /// captured throughout, and urgent controls still cut instantly.
    FinishSpan { max_extra_ms: u32 },
}

/// One semantic segment of an utterance with its own pacing + policy.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechSpan {
    /// Plain text for transcripts/history (markers stripped, digits as
    /// written).
    pub text: String,
    /// What the synthesizer speaks (digit runs expanded to spoken words).
    pub tts_text: String,
    /// Speaking-rate multiplier (1.0 = normal), already clamped.
    pub rate: f32,
    pub policy: InterruptPolicy,
}

/// Call-local speaking pace, mutated by the caller's voice commands
/// ("slower", "faster", "normal speed") and reset at every call boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct PaceState {
    base: f32,
    detail: f32,
    default_base: f32,
    default_detail: f32,
}

impl PaceState {
    pub fn new(base_pct: u64, detail_pct: u64) -> Self {
        let base = (base_pct as f32 / 100.0).clamp(BASE_RATE_MIN, BASE_RATE_MAX);
        let detail = (detail_pct as f32 / 100.0).clamp(DETAIL_RATE_MIN, DETAIL_RATE_MAX);
        Self {
            base,
            detail,
            default_base: base,
            default_detail: detail,
        }
    }

    /// Settings-fed defaults (AOKIE_SPEECH_RATE_PCT / AOKIE_DETAIL_RATE_PCT,
    /// set by the connector at radio spawn).
    pub fn from_env() -> Self {
        let pct = |var: &str, default: u64| {
            std::env::var(var)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(default)
        };
        Self::new(
            pct("AOKIE_SPEECH_RATE_PCT", DEFAULT_BASE_RATE_PCT),
            pct("AOKIE_DETAIL_RATE_PCT", DEFAULT_DETAIL_RATE_PCT),
        )
    }

    pub fn base(&self) -> f32 {
        self.base
    }

    /// The rate detail spans (numbers, codes) are spoken at — never faster
    /// than the base rate.
    pub fn detail(&self) -> f32 {
        self.detail.min(self.base)
    }

    /// One caller-requested step slower. Returns the new base rate.
    pub fn slower(&mut self) -> f32 {
        self.base = (self.base - RATE_STEP).max(BASE_RATE_MIN);
        self.base
    }

    /// One caller-requested step faster. Returns the new base rate.
    pub fn faster(&mut self) -> f32 {
        self.base = (self.base + RATE_STEP).min(BASE_RATE_MAX);
        self.base
    }

    /// Back to the configured defaults ("normal speed").
    pub fn reset(&mut self) {
        self.base = self.default_base;
        self.detail = self.default_detail;
    }

    /// A ONE-OFF pace for "say that again slower": a step slower overall,
    /// with detail spans (the phone number they're writing down) slowed
    /// further still. Does not touch the call's persistent pace.
    pub fn replay_slower(&self) -> PaceState {
        let mut p = self.clone();
        p.base = (p.base - RATE_STEP).max(BASE_RATE_MIN);
        p.detail = (p.detail - 0.1).max(DETAIL_RATE_MIN);
        p
    }
}

impl Default for PaceState {
    fn default() -> Self {
        Self::new(DEFAULT_BASE_RATE_PCT, DEFAULT_DETAIL_RATE_PCT)
    }
}

// ── Marker parsing ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Base,
    Slow(f32),
    Important,
}

/// A recognized marker token (the text between the brackets).
fn recognize_marker(inner: &str) -> Option<MarkerToken> {
    let t = inner.trim().to_ascii_lowercase();
    match t.as_str() {
        "slow" => Some(MarkerToken::OpenSlow),
        "important" | "protect" | "protected" => Some(MarkerToken::OpenImportant),
        "/slow" | "/rate" | "/important" | "/protect" | "/protected" => Some(MarkerToken::Close),
        _ => t
            .strip_prefix("rate=")
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|r| r.is_finite() && *r > 0.0)
            .map(|r| MarkerToken::OpenRate(r.clamp(SPAN_RATE_MIN, SPAN_RATE_MAX))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MarkerToken {
    OpenSlow,
    OpenRate(f32),
    OpenImportant,
    Close,
}

/// Find the next bracketed token starting at or after `from`: returns
/// (start, end_exclusive, inner). Handles `[[x]]` always; `[x]` only when
/// the inner is a recognized control word, so ordinary bracketed prose is
/// left alone. Tokens are bounded (≤ 40 chars inner, single line).
fn next_bracket_token(s: &str, from: usize) -> Option<(usize, usize, String)> {
    let bytes = s.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let double = i + 1 < bytes.len() && bytes[i + 1] == b'[';
        let inner_start = if double { i + 2 } else { i + 1 };
        let close_pat: &[u8] = if double { b"]]" } else { b"]" };
        // Find the close within the bounded window.
        let window_end = (inner_start + 40).min(bytes.len());
        let mut j = inner_start;
        let mut found: Option<usize> = None;
        while j <= window_end && j < bytes.len() {
            if bytes[j] == b'\n' || bytes[j] == b'[' {
                break;
            }
            if bytes[j..].starts_with(close_pat) {
                found = Some(j);
                break;
            }
            j += 1;
        }
        if let Some(inner_end) = found {
            let inner = &s[inner_start..inner_end];
            let end = inner_end + close_pat.len();
            if double {
                // Any double-bracket token is control markup: strip it even
                // when unrecognized (markup must never be spoken).
                return Some((i, end, inner.to_string()));
            }
            if recognize_marker(inner).is_some()
                || inner.trim().eq_ignore_ascii_case("wait")
                || inner.trim().eq_ignore_ascii_case("end_call")
                || inner.trim().eq_ignore_ascii_case("end call")
            {
                return Some((i, end, inner.to_string()));
            }
        }
        i += 1;
    }
    None
}

/// Live business lookup (guide P1-16): the model may reply with EXACTLY
/// `[[LOOKUP: <question>]]` to have the host run the read-only lookup flow
/// and hand the result back for a second generation. Like every bracketed
/// token it is ALWAYS stripped from speech/transcripts by [`plan_spans`], so
/// even a leaked marker is never spoken. Tolerant scan: the marker is found
/// anywhere in the text (models sometimes wrap it in whitespace/punctuation).
pub fn parse_lookup_marker(text: &str) -> Option<String> {
    let start = text.find("[[LOOKUP:")?;
    let rest = &text[start + "[[LOOKUP:".len()..];
    let end = rest.find("]]")?;
    let q = rest[..end].trim();
    if q.is_empty() {
        None
    } else {
        Some(q.to_string())
    }
}

/// Typed Companion assistance: extract the bounded question from an exact
/// `[[ASSISTANCE: ...]]` model verdict. The marker is control-plane data and
/// is stripped by [`plan_spans`] before any text can reach TTS.
pub fn parse_assistance_marker(text: &str) -> Option<String> {
    let start = text.find("[[ASSISTANCE:")?;
    let rest = &text[start + "[[ASSISTANCE:".len()..];
    let end = rest.find("]]")?;
    let question = rest[..end].trim();
    if question.is_empty() {
        None
    } else {
        Some(question.to_string())
    }
}

/// An explicit caller-to-owner handoff request. Unlike the older lookup and
/// assistance parsers this is deliberately strict: the complete model reply
/// must be one exact `[[TRANSFER: short reason]]` verdict. That keeps ordinary
/// prose (including someone merely discussing a transfer) out of the control
/// plane. The reason contains no nested markup/control characters and is never
/// itself spoken; overlong model metadata is canonicalised to a fixed safe
/// reason before it can enter history or the assistance mailbox.
pub const MAX_TRANSFER_REASON_BYTES: usize = 30;
const MAX_TRANSFER_UNTRUSTED_REASON_BYTES: usize = 160;
const SAFE_TRANSFER_REASON: &str = "Caller requested the owner";

pub fn parse_transfer_marker(text: &str) -> Option<String> {
    let verdict = text.trim();
    let reason = verdict
        .strip_prefix("[[TRANSFER:")?
        .strip_suffix("]]")?
        .trim();
    if reason.is_empty()
        || reason.len() > MAX_TRANSFER_UNTRUSTED_REASON_BYTES
        || reason.chars().any(char::is_control)
        || reason.contains('[')
        || reason.contains(']')
    {
        return None;
    }
    // The wrapper is the control verdict; the model-authored reason is only
    // untrusted metadata and request_transfer replaces it with the same safe
    // generic text before fan-out.  Small models can miss the prompt's byte
    // limit by one word (or one byte).  Treat an otherwise exact verdict as a
    // transfer intent, but canonicalise overlong metadata instead of falsely
    // telling the caller that nobody is available without consulting routing.
    Some(if reason.len() > MAX_TRANSFER_REASON_BYTES {
        SAFE_TRANSFER_REASON.to_owned()
    } else {
        reason.to_owned()
    })
}

/// Phase 3: extract the manager-action request from a `[[MANAGER: ...]]`
/// marker (same shape as the lookup marker). The request is the manager's
/// change in the model's words — a downstream flow structures and validates
/// it; the PIN gate stands between this marker and any write.
pub fn parse_manager_marker(text: &str) -> Option<String> {
    let start = text.find("[[MANAGER:")?;
    let rest = &text[start + "[[MANAGER:".len()..];
    let end = rest.find("]]")?;
    let q = rest[..end].trim();
    if q.is_empty() {
        None
    } else {
        Some(q.to_string())
    }
}

/// Phase 3 PIN capture: the digits in a spoken utterance, tolerant to STT
/// writing them as words ("one two three four"), digits ("1234"), or a mix.
/// Everything that is not a digit is ignored, so "uh, it's 1 2 3 4 thanks"
/// verifies cleanly. "oh"/"o" count as zero (phone-speak).
pub fn spoken_digits(s: &str) -> String {
    let mut out = String::new();
    for token in s.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            out.push_str(token);
            continue;
        }
        let word = token.to_ascii_lowercase();
        let d = match word.as_str() {
            "zero" | "oh" | "o" => Some('0'),
            "one" => Some('1'),
            "two" => Some('2'),
            "three" => Some('3'),
            "four" => Some('4'),
            "five" => Some('5'),
            "six" => Some('6'),
            "seven" => Some('7'),
            "eight" => Some('8'),
            "nine" => Some('9'),
            _ => None,
        };
        if let Some(d) = d {
            out.push(d);
        }
    }
    out
}

#[cfg(test)]
mod manager_marker_tests {
    use super::{parse_manager_marker, spoken_digits};

    #[test]
    fn parse_manager_marker_shapes() {
        assert_eq!(
            parse_manager_marker("[[MANAGER: cancel the 2pm booking on the 22nd]]"),
            Some("cancel the 2pm booking on the 22nd".into())
        );
        assert_eq!(
            parse_manager_marker("Sure. [[MANAGER: block 0400 111 222]]"),
            Some("block 0400 111 222".into())
        );
        assert_eq!(parse_manager_marker("[[MANAGER:]]"), None);
        assert_eq!(parse_manager_marker("[[MANAGER: unclosed"), None);
        assert_eq!(parse_manager_marker("no marker here"), None);
    }

    #[test]
    fn spoken_digits_reads_words_digits_and_mixes() {
        assert_eq!(spoken_digits("one two three four"), "1234");
        assert_eq!(spoken_digits("1234"), "1234");
        assert_eq!(spoken_digits("uh, it's 12 three 4 thanks"), "1234");
        assert_eq!(spoken_digits("Oh seven oh nine"), "0709");
        assert_eq!(spoken_digits("nothing here"), "");
        assert_eq!(spoken_digits("One, Two... THREE? four!"), "1234");
    }
}

#[cfg(test)]
mod lookup_marker_tests {
    use super::{
        parse_assistance_marker, parse_transfer_marker, MAX_TRANSFER_REASON_BYTES,
        SAFE_TRANSFER_REASON,
    };

    #[test]
    fn parse_lookup_marker_shapes() {
        use super::parse_lookup_marker as p;
        assert_eq!(
            p("[[LOOKUP: any tables Friday?]]"),
            Some("any tables Friday?".into())
        );
        assert_eq!(
            p("Sure. [[LOOKUP: bookings on the 28th]] thanks"),
            Some("bookings on the 28th".into())
        );
        assert_eq!(p("[[LOOKUP:]]"), None, "empty question is not a lookup");
        assert_eq!(
            p("[[LOOKUP: unclosed marker"),
            None,
            "unclosed never parses"
        );
        assert_eq!(p("no marker here"), None);
    }

    #[test]
    fn parse_assistance_marker_shapes() {
        assert_eq!(
            parse_assistance_marker("[[ASSISTANCE: Can we accept a late arrival?]]"),
            Some("Can we accept a late arrival?".into())
        );
        assert_eq!(
            parse_assistance_marker("Please wait. [[ASSISTANCE: confirm the exception]]"),
            Some("confirm the exception".into())
        );
        assert_eq!(parse_assistance_marker("[[ASSISTANCE:]]"), None);
        assert_eq!(parse_assistance_marker("[[ASSISTANCE: unclosed"), None);
    }

    #[test]
    fn transfer_marker_is_exact_and_bounded() {
        assert_eq!(
            parse_transfer_marker("  [[TRANSFER: caller asked for owner]]\n"),
            Some("caller asked for owner".into())
        );
        for invalid in [
            "Please wait. [[TRANSFER: caller asked for owner]]",
            "[[transfer: caller asked for owner]]",
            "[[TRANSFER:]]",
            "[[TRANSFER: caller asked for owner]",
            "[[TRANSFER: caller [asked] for owner]]",
            "[[TRANSFER: caller asked for owner]] trailing",
        ] {
            assert_eq!(parse_transfer_marker(invalid), None, "{invalid}");
        }
        let overlong = format!(
            "[[TRANSFER: {}]]",
            "x".repeat(MAX_TRANSFER_REASON_BYTES + 1)
        );
        assert_eq!(
            parse_transfer_marker(&overlong).as_deref(),
            Some(SAFE_TRANSFER_REASON),
            "an exact marker canonicalises untrusted overlong metadata"
        );
        assert_eq!(
            parse_transfer_marker("[[TRANSFER: Caller wants someone in charge.]]").as_deref(),
            Some(SAFE_TRANSFER_REASON),
            "the live 31-byte reason remains an exact transfer intent"
        );
        let unbounded = format!(
            "[[TRANSFER: {}]]",
            "x".repeat(super::MAX_TRANSFER_UNTRUSTED_REASON_BYTES + 1)
        );
        assert_eq!(
            parse_transfer_marker(&unbounded),
            None,
            "untrusted marker metadata remains hard-bounded"
        );
        let largest = format!("[[TRANSFER: {}]]", "x".repeat(MAX_TRANSFER_REASON_BYTES));
        assert_eq!(parse_transfer_marker(&largest).unwrap().len(), 30);
        assert!(super::clean_text(&super::plan_spans(
            &largest,
            &super::PaceState::default(),
            2_500,
        ))
        .is_empty());
    }
}

/// True when the text carries a `[[WAIT]]` / `[WAIT]` marker — the model's
/// way of choosing INTENTIONAL SILENCE ("the caller is thinking; say
/// nothing"). The marker itself is always stripped by [`plan_spans`].
pub fn has_wait_marker(text: &str) -> bool {
    let mut at = 0;
    while let Some((_, end, inner)) = next_bracket_token(text, at) {
        if inner.trim().eq_ignore_ascii_case("wait") {
            return true;
        }
        at = end;
    }
    false
}

// ── Digit-run detection + expansion ─────────────────────────────────────────

fn digit_word(token: &str) -> bool {
    matches!(
        token,
        "zero"
            | "one"
            | "two"
            | "three"
            | "four"
            | "five"
            | "six"
            | "seven"
            | "eight"
            | "nine"
            | "oh"
            | "double"
            | "triple"
    )
}

fn digits_in(token: &str) -> usize {
    token.chars().filter(|c| c.is_ascii_digit()).count()
}

/// Numeral-ish token: contains a digit and nothing but digits/separators —
/// "0412", "345-678", "(07)", "+61". Times ("10:30") are excluded (the
/// clock-time normalizer owns those) and so is anything with letters.
fn is_numeralish(token: &str) -> bool {
    if aokie_core::speech::is_calendar_date_token(token) {
        return false;
    }
    let core = token.trim_matches(|c: char| matches!(c, '.' | ',' | '!' | '?' | ';'));
    !core.is_empty()
        && core.chars().any(|c| c.is_ascii_digit())
        && core
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '(' | ')' | '+' | '-' | '.' | ','))
}

/// Money-style veto: "$1,250", "15,000", "1,250.50" — thousands-grouped or
/// currency-marked numbers are AMOUNTS, not codes; they must be read as
/// numbers at normal pace.
fn is_money_style(token: &str) -> bool {
    let core = token.trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ';'));
    if core.starts_with('$') || core.starts_with('€') || core.starts_with('£') {
        return true;
    }
    // ^\d{1,3}(,\d{3})+([.]\d+)?$ — thousands grouping.
    let mut parts = core.split(',');
    let Some(head) = parts.next() else {
        return false;
    };
    if head.is_empty() || head.len() > 3 || !head.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let mut any_group = false;
    for p in parts {
        let (grp, dec) = match p.split_once('.') {
            Some((g, d)) => (g, Some(d)),
            None => (p, None),
        };
        if grp.len() != 3 || !grp.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        if let Some(d) = dec {
            if !d.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
        }
        any_group = true;
    }
    any_group
}

fn money_word(token: &str) -> bool {
    matches!(
        token
            .trim_matches(|c: char| !c.is_ascii_alphabetic())
            .to_ascii_lowercase()
            .as_str(),
        "dollar" | "dollars" | "cent" | "cents" | "euro" | "euros" | "pound" | "pounds" | "bucks"
    )
}

/// Expand one numeral-ish token to spoken digit words: "0412" → "zero four
/// one two"; separators inside the token become a short group pause (", ").
/// A leading '+' reads as "plus". Trailing sentence punctuation survives.
fn expand_numeral_token(token: &str) -> String {
    const WORDS: [&str; 10] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    ];
    let trailing: String = token
        .chars()
        .rev()
        .take_while(|c| matches!(c, '.' | ',' | '!' | '?' | ';'))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let core = &token[..token.len() - trailing.len()];
    let mut out = String::new();
    let mut pending_sep = false;
    for c in core.chars() {
        if let Some(d) = c.to_digit(10) {
            if pending_sep && !out.is_empty() {
                out.push_str(", ");
            } else if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(WORDS[d as usize]);
            pending_sep = false;
        } else if c == '+' && out.is_empty() {
            out.push_str("plus");
        } else if matches!(c, '-' | '.' | ',' | '/') {
            pending_sep = true;
        }
        // '(' / ')' and anything else: dropped.
    }
    out.push_str(&trailing);
    out
}

/// One run of tokens classified as a slow-read detail (phone number / code).
struct DetailRun {
    start: usize,
    end: usize, // exclusive
    expand: bool,
}

/// Find detail runs in a token list: numeral runs totalling ≥ 5 digits
/// (expanded to digit words) and spoken-digit-word runs of ≥ 4 words
/// (already words — no expansion). Money-style amounts never match.
fn find_detail_runs(tokens: &[&str]) -> Vec<DetailRun> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let lower = tokens[i]
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        if is_numeralish(tokens[i]) && !is_money_style(tokens[i]) {
            let mut j = i;
            let mut digits = 0;
            let mut money = false;
            while j < tokens.len() && is_numeralish(tokens[j]) {
                money = money || is_money_style(tokens[j]);
                digits += digits_in(tokens[j]);
                j += 1;
            }
            let followed_by_money = j < tokens.len() && money_word(tokens[j]);
            let preceded_by_money = i > 0 && tokens[i - 1].ends_with('$');
            if digits >= 5 && !money && !followed_by_money && !preceded_by_money {
                runs.push(DetailRun {
                    start: i,
                    end: j,
                    expand: true,
                });
            }
            i = j;
            continue;
        }
        if digit_word(&lower) {
            let mut j = i;
            while j < tokens.len()
                && digit_word(
                    &tokens[j]
                        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
                        .to_ascii_lowercase(),
                )
            {
                j += 1;
            }
            if j - i >= 4 {
                runs.push(DetailRun {
                    start: i,
                    end: j,
                    expand: false,
                });
            }
            i = j;
            continue;
        }
        i += 1;
    }
    runs
}

// ── Planning ────────────────────────────────────────────────────────────────

/// Plan one sentence into validated speech spans.
///
/// - strips ALL bracketed control markup (recognized or not) from both
///   `text` and `tts_text`;
/// - applies model-requested slow/rate/important segments with clamped
///   rates and the operator-capped protection budget;
/// - detects phone/code digit runs in unmarked text, slows them to the
///   detail rate, expands them for the synthesizer, and gives them a
///   bounded finish-the-phrase policy;
/// - returns an empty vec when nothing speakable remains.
pub fn plan_spans(sentence: &str, pace: &PaceState, protected_max_ms: u32) -> Vec<SpeechSpan> {
    let protected_max_ms = protected_max_ms.clamp(PROTECTED_MAX_MS_MIN, PROTECTED_MAX_MS_MAX);
    // 1) Split on markers into (text, mode) segments.
    let mut segments: Vec<(String, Mode)> = Vec::new();
    let mut mode = Mode::Base;
    let mut at = 0usize;
    while let Some((start, end, inner)) = next_bracket_token(sentence, at) {
        if start > at {
            segments.push((sentence[at..start].to_string(), mode));
        }
        match recognize_marker(&inner) {
            Some(MarkerToken::OpenSlow) => mode = Mode::Slow(pace.detail()),
            Some(MarkerToken::OpenRate(r)) => mode = Mode::Slow(r),
            Some(MarkerToken::OpenImportant) => mode = Mode::Important,
            Some(MarkerToken::Close) => mode = Mode::Base,
            None => {} // unknown/WAIT/END_CALL token: stripped, mode unchanged
        }
        at = end;
    }
    if at < sentence.len() {
        segments.push((sentence[at..].to_string(), mode));
    }

    // 2) Segments → spans (digit segmentation inside Base segments).
    let mut spans: Vec<SpeechSpan> = Vec::new();
    for (seg, seg_mode) in segments {
        let tokens: Vec<&str> = seg.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        match seg_mode {
            Mode::Slow(rate) => {
                push_span(
                    &mut spans,
                    &tokens,
                    0,
                    tokens.len(),
                    rate,
                    InterruptPolicy::FinishSpan {
                        max_extra_ms: FINISH_PHRASE_MS,
                    },
                    true,
                );
            }
            Mode::Important => {
                push_span(
                    &mut spans,
                    &tokens,
                    0,
                    tokens.len(),
                    pace.base(),
                    InterruptPolicy::FinishSpan {
                        max_extra_ms: protected_max_ms,
                    },
                    true,
                );
            }
            Mode::Base => {
                let runs = find_detail_runs(&tokens);
                let mut cursor = 0usize;
                for run in &runs {
                    if run.start > cursor {
                        push_span(
                            &mut spans,
                            &tokens,
                            cursor,
                            run.start,
                            pace.base(),
                            InterruptPolicy::Yield,
                            false,
                        );
                    }
                    push_detail_span(&mut spans, &tokens, run, pace);
                    cursor = run.end;
                }
                if cursor < tokens.len() {
                    push_span(
                        &mut spans,
                        &tokens,
                        cursor,
                        tokens.len(),
                        pace.base(),
                        InterruptPolicy::Yield,
                        false,
                    );
                }
            }
        }
    }
    spans
}

fn push_span(
    spans: &mut Vec<SpeechSpan>,
    tokens: &[&str],
    start: usize,
    end: usize,
    rate: f32,
    policy: InterruptPolicy,
    expand_digits: bool,
) {
    let text = tokens[start..end].join(" ");
    if text.trim().is_empty() {
        return;
    }
    let tts_text = if expand_digits {
        let runs = find_detail_runs(&tokens[start..end]);
        if runs.iter().any(|r| r.expand) {
            expand_tokens(&tokens[start..end], &runs)
        } else {
            text.clone()
        }
    } else {
        text.clone()
    };
    spans.push(SpeechSpan {
        text,
        tts_text: aokie_core::speech::normalize_calendar_dates(&tts_text),
        rate,
        policy,
    });
}

fn push_detail_span(
    spans: &mut Vec<SpeechSpan>,
    tokens: &[&str],
    run: &DetailRun,
    pace: &PaceState,
) {
    let text = tokens[run.start..run.end].join(" ");
    let tts_text = if run.expand {
        tokens[run.start..run.end]
            .iter()
            .map(|t| expand_numeral_token(t))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        text.clone()
    };
    spans.push(SpeechSpan {
        text,
        tts_text,
        rate: pace.detail(),
        policy: InterruptPolicy::FinishSpan {
            max_extra_ms: FINISH_PHRASE_MS,
        },
    });
}

/// Expand the marked runs inside a token window (used for slow/important
/// segments where the whole segment stays ONE span but its digit runs must
/// still be read digit-by-digit).
fn expand_tokens(tokens: &[&str], runs: &[DetailRun]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0usize;
    while i < tokens.len() {
        if let Some(run) = runs.iter().find(|r| r.expand && r.start == i) {
            out.push(
                tokens[run.start..run.end]
                    .iter()
                    .map(|t| expand_numeral_token(t))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            i = run.end;
        } else {
            out.push(tokens[i].to_string());
            i += 1;
        }
    }
    out.join(" ")
}

/// Marker-free plain text of a plan — what transcripts and model history
/// record. (Digit runs stay as written; expansion is synthesis-only.)
pub fn clean_text(spans: &[SpeechSpan]) -> String {
    spans
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pace() -> PaceState {
        PaceState::new(100, 75)
    }

    #[test]
    fn plain_sentence_is_one_yield_span_at_base_rate() {
        let spans = plan_spans("How can I help you today?", &pace(), 2500);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "How can I help you today?");
        assert_eq!(spans[0].tts_text, spans[0].text);
        assert_eq!(spans[0].rate, 1.0);
        assert_eq!(spans[0].policy, InterruptPolicy::Yield);
    }

    #[test]
    fn dates_are_spoken_naturally_before_digit_expansion_in_all_modes() {
        for raw in ["Your appointment is 2026-09-14.",
            "[[slow]]Your appointment is 2026-09-14.[[/slow]]",
            "[[important]]Your appointment is 2026-09-14.[[/important]]"] {
            let spans = plan_spans(raw, &pace(), 2500);
            assert_eq!(spans.len(), 1, "{spans:?}");
            assert!(spans[0].tts_text.contains("Monday the fourteenth of September"));
            assert!(spans[0].text.contains("2026-09-14"), "keep source date in records");
            assert!(!spans[0].tts_text.contains("two zero two six"));
        }
        let spans = plan_spans("On 2026-09-14 call 0412 345 678.", &pace(), 2500);
        assert!(spans[0].tts_text.contains("Monday the fourteenth"));
        assert!(spans.iter().any(|s| s.tts_text.contains("zero four one two")));
        assert_eq!(spans[0].policy, InterruptPolicy::Yield);
    }

    #[test]
    fn phone_number_becomes_a_slow_expanded_finish_span() {
        let spans = plan_spans("The number is 0412 345 678. Anything else?", &pace(), 2500);
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!(spans[0].text, "The number is");
        assert_eq!(spans[0].rate, 1.0);
        assert_eq!(spans[1].text, "0412 345 678.");
        assert_eq!(
            spans[1].tts_text,
            "zero four one two, three four five, six seven eight."
        );
        assert_eq!(spans[1].rate, 0.75);
        assert_eq!(
            spans[1].policy,
            InterruptPolicy::FinishSpan {
                max_extra_ms: FINISH_PHRASE_MS
            }
        );
        assert_eq!(spans[2].text, "Anything else?");
        // Transcript text reconstructs cleanly.
        assert_eq!(
            clean_text(&spans),
            "The number is 0412 345 678. Anything else?"
        );
    }

    #[test]
    fn single_long_numeral_and_plus_prefix_expand() {
        let spans = plan_spans("Call 0491570156 now", &pace(), 2500);
        assert_eq!(
            spans[1].tts_text,
            "zero four nine one five seven zero one five six"
        );
        let spans = plan_spans("It's +61 491 570 156.", &pace(), 2500);
        let detail = spans.iter().find(|s| s.rate < 1.0).unwrap();
        assert!(detail.tts_text.starts_with("plus six one, "), "{detail:?}");
        assert!(detail.tts_text.ends_with("one five six."), "{detail:?}");
    }

    #[test]
    fn money_years_times_and_short_numbers_stay_normal() {
        for text in [
            "That will be $1,250 in total.",
            "The deposit is 15,000 dollars.",
            "We open in 2026.",
            "See you at 10:30 tomorrow.",
            "Table for 4 people at 7.",
            "That's 5.50 each.",
        ] {
            let spans = plan_spans(text, &pace(), 2500);
            assert!(
                spans
                    .iter()
                    .all(|s| s.rate == 1.0 && s.policy == InterruptPolicy::Yield),
                "{text:?} produced a detail span: {spans:?}"
            );
            assert_eq!(clean_text(&spans), text, "text must be untouched");
        }
    }

    #[test]
    fn spoken_digit_word_runs_slow_down_without_expansion() {
        let spans = plan_spans("It's zero four one two, okay?", &pace(), 2500);
        let detail = spans.iter().find(|s| s.rate < 1.0).expect("detail span");
        assert_eq!(detail.text, "zero four one two,");
        assert_eq!(detail.tts_text, detail.text);
        // Three digit words is not a phone number.
        let spans = plan_spans("Room two oh nine is ready", &pace(), 2500);
        assert!(spans.iter().all(|s| s.rate == 1.0), "{spans:?}");
    }

    #[test]
    fn slow_marker_sets_detail_rate_and_is_stripped() {
        let spans = plan_spans("Your code is [[slow]]X 4 B 9[[/slow]] okay?", &pace(), 2500);
        assert_eq!(spans.len(), 3, "{spans:?}");
        assert_eq!(spans[1].text, "X 4 B 9");
        assert_eq!(spans[1].rate, 0.75);
        assert!(matches!(
            spans[1].policy,
            InterruptPolicy::FinishSpan { .. }
        ));
        assert!(!clean_text(&spans).contains("[["), "markers stripped");
    }

    #[test]
    fn rate_marker_is_clamped_and_malformed_markers_strip_to_plain_text() {
        let spans = plan_spans("[[rate=0.8]]slow bit[[/rate]] normal bit", &pace(), 2500);
        assert_eq!(spans[0].rate, 0.8);
        assert_eq!(spans[1].rate, 1.0);
        // Out-of-band rate clamps.
        let spans = plan_spans("[[rate=0.1]]very slow[[/rate]]", &pace(), 2500);
        assert_eq!(spans[0].rate, 0.5);
        let spans = plan_spans("[[rate=9]]fast[[/rate]]", &pace(), 2500);
        assert_eq!(spans[0].rate, 1.5);
        // Malformed rate: token stripped, text spoken normally.
        let spans = plan_spans("[[rate=abc]]hello there friend", &pace(), 2500);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello there friend");
        assert_eq!(spans[0].rate, 1.0);
    }

    #[test]
    fn important_marker_gets_the_capped_protection_budget() {
        let spans = plan_spans(
            "[[important]]Please do not share this code with anyone.[[/important]]",
            &pace(),
            9999, // over the cap → clamped
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].policy,
            InterruptPolicy::FinishSpan {
                max_extra_ms: PROTECTED_MAX_MS_MAX
            }
        );
        assert_eq!(spans[0].rate, 1.0);
        let spans = plan_spans("[[important]]Short warning[[/important]]", &pace(), 2500);
        assert_eq!(
            spans[0].policy,
            InterruptPolicy::FinishSpan { max_extra_ms: 2500 }
        );
    }

    #[test]
    fn unclosed_marker_runs_to_sentence_end_and_dangling_close_is_ignored() {
        let spans = plan_spans("Here it is [[slow]]zero four one two", &pace(), 2500);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].rate, 0.75, "unclosed slow applies to the rest");
        let spans = plan_spans("All good.[[/slow]] Thanks!", &pace(), 2500);
        assert_eq!(clean_text(&spans), "All good. Thanks!");
        assert!(spans.iter().all(|s| s.rate == 1.0));
    }

    #[test]
    fn unknown_and_wait_markers_are_stripped_never_spoken() {
        let spans = plan_spans("Sure thing. [[SOMETHING_ODD]] Right away.", &pace(), 2500);
        assert_eq!(clean_text(&spans), "Sure thing. Right away.");
        // A marker-only reply plans to NOTHING (the intentional-silence path).
        assert!(plan_spans("[[WAIT]]", &pace(), 2500).is_empty());
        assert!(plan_spans("[WAIT]", &pace(), 2500).is_empty());
        assert!(has_wait_marker("[[WAIT]]"));
        assert!(has_wait_marker("Okay. [wait]"));
        assert!(!has_wait_marker("please wait a moment"));
        // Ordinary single-bracket prose is NOT treated as markup.
        let spans = plan_spans("The sign says [closed] on Sundays", &pace(), 2500);
        assert_eq!(clean_text(&spans), "The sign says [closed] on Sundays");
    }

    #[test]
    fn digit_runs_inside_slow_or_important_segments_still_expand() {
        let spans = plan_spans(
            "[[important]]Your code is 48291 today[[/important]]",
            &pace(),
            2500,
        );
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0].tts_text.contains("four eight two nine one"),
            "{spans:?}"
        );
        assert!(spans[0].text.contains("48291"), "transcript keeps digits");
    }

    #[test]
    fn pace_state_steps_clamp_and_reset() {
        let mut p = PaceState::new(100, 75);
        assert_eq!(p.base(), 1.0);
        assert_eq!(p.detail(), 0.75);
        assert!((p.slower() - 0.85).abs() < 1e-6);
        for _ in 0..10 {
            p.slower();
        }
        assert_eq!(p.base(), 0.6, "floor");
        assert_eq!(p.detail(), 0.6, "detail never faster than base");
        for _ in 0..10 {
            p.faster();
        }
        assert_eq!(p.base(), 1.4, "ceiling");
        p.reset();
        assert_eq!(p.base(), 1.0);
        assert_eq!(p.detail(), 0.75);
        // Constructor clamps percent inputs.
        let p = PaceState::new(500, 10);
        assert_eq!(p.base(), BASE_RATE_MAX);
        assert_eq!(p.detail(), DETAIL_RATE_MIN);
    }

    #[test]
    fn empty_and_whitespace_inputs_plan_to_nothing() {
        assert!(plan_spans("", &pace(), 2500).is_empty());
        assert!(plan_spans("   ", &pace(), 2500).is_empty());
        assert!(plan_spans("[[slow]][[/slow]]", &pace(), 2500).is_empty());
    }
}
