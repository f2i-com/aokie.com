//! Duplex/floor coordination: WHO has the conversational floor, and what a
//! caller's words mean for it.
//!
//! Two pieces live here, both deterministic and pure (the LLM must never
//! sit in the stop path):
//!
//! 1. [`parse_caller_intent`] — a conservative grammar over final STT text
//!    that recognizes floor-control commands: *wait / hold on / let me
//!    think* (pause — INTENTIONAL SILENCE, the bot says nothing at all),
//!    *stop / be quiet* (yield), *continue / go ahead* (resume),
//!    *slower / faster / normal speed* (pace), *repeat (slower)* (replay).
//!    Ambiguity always resolves to [`CallerIntent::Content`]: answering
//!    like a receptionist is recoverable, wrongly going silent is not.
//! 2. [`DialogueState`] — the pause state machine. `PausedByCaller` means
//!    the caller asked for the floor to stay open: the agent generates
//!    nothing, speaks nothing, and simply keeps listening until the caller
//!    says something substantive (which exits the pause) or asks it to
//!    continue. "Take your time!" chatter is exactly what this exists to
//!    prevent — sometimes the most human response is nothing at all.
//!
//! Evolution note: today the floor decision fuses a handful of hard
//! thresholds (energy VAD, sustained-frame barge counting, this grammar).
//! The intended next step is a single *floor-confidence* estimate combining
//! VAD, STT confidence, sentence-completion shape, pause duration,
//! interruption history and dialogue state — the reason this module keeps
//! every floor decision in one place instead of scattering ifs through the
//! radio loop.

/// What the caller's utterance asks of the floor coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerIntent {
    /// Ordinary conversational content — answer it.
    Content,
    /// "Wait" / "hold on" / "let me think": stay silent, keep listening,
    /// do not auto-resume.
    Pause,
    /// "Stop" / "be quiet": yield the floor now and wait.
    StopSpeaking,
    /// "Continue" / "go ahead": the caller hands the floor back.
    Resume,
    /// "Slower" / "slow down" — lower the call-local base rate.
    Slower,
    /// "Faster" / "speed up" — raise the call-local base rate.
    Faster,
    /// "Normal speed" — reset the call-local pace.
    NormalSpeed,
    /// "Repeat that" / "say that again" — replay the last reply verbatim.
    Repeat,
    /// "Say the number slower" / "repeat that slower" — replay slowed down.
    RepeatSlower,
}

/// Words that neutralize a command reading ("don't stop", "not so fast").
/// Conservative: their presence anywhere makes the utterance Content.
const NEGATIONS: &[&str] = &[
    "don't", "dont", "not", "never", "won't", "wont", "wouldn't", "wouldnt", "didn't", "didnt",
    "doesn't", "doesnt", "isn't", "isnt",
];

/// A leading question word makes the utterance a question, not a command
/// ("should I stop?").
const QUESTION_LEADS: &[&str] = &[
    "should", "shall", "do", "does", "did", "will", "would", "are", "is", "was", "were", "why",
    "when", "where", "who", "how",
];

/// Words ignored when counting how much NON-command content an utterance
/// carries. "Can you slow down please" is pure command; "can you stop by
/// the office" is not — "by the office" survives filtering.
const FILLERS: &[&str] = &[
    "please", "just", "um", "uh", "erm", "ah", "okay", "ok", "oh", "hey", "now", "a", "bit",
    "little", "sorry", "yeah", "yes", "thanks", "thank", "you", "can", "could", "would", "aokie",
    "that", "for", "me", "it", "the", "i", "i'm", "im", "of", "to", "and", "so", "well", "there",
    "sec", "second", "moment", "minute", "actually", "maybe", "up",
];

const PAUSE_PHRASES: &[&[&str]] = &[
    &["wait"],
    &["hold", "on"],
    &["hang", "on"],
    &["one", "moment"],
    &["one", "second"],
    &["one", "sec"],
    &["one", "minute"],
    &["give", "me", "a", "moment"],
    &["give", "me", "a", "second"],
    &["give", "me", "a", "minute"],
    &["bear", "with", "me"],
    // Intentional silence: the caller is thinking — the right response is
    // to wait quietly, not to fill their pause with chatter.
    &["let", "me", "think"],
    &["let", "me", "check"],
    &["let", "me", "look"],
    &["let", "me", "see"],
    &["let", "me", "find"],
    &["thinking"],
    &["need", "a", "moment"],
    &["need", "a", "minute"],
    &["need", "a", "second"],
];

const STOP_PHRASES: &[&[&str]] = &[
    &["stop"],
    &["stop", "talking"],
    &["be", "quiet"],
    &["quiet"],
    &["shush"],
    &["hush"],
    &["shut", "up"],
    &["let", "me", "speak"],
    &["let", "me", "talk"],
    &["let", "me", "finish"],
];

const RESUME_PHRASES: &[&[&str]] = &[
    &["continue"],
    &["resume"],
    &["go", "ahead"],
    &["go", "on"],
    &["keep", "going"],
    &["carry", "on"],
    &["i'm", "back"],
    &["im", "back"],
    &["back", "now"],
];

const REPEAT_PHRASES: &[&[&str]] = &[
    &["repeat"],
    &["say", "that", "again"],
    &["say", "it", "again"],
    &["what", "was", "that"],
    &["come", "again"],
    &["pardon"],
    &["read", "that", "back"],
];

const NORMAL_SPEED_PHRASES: &[&[&str]] = &[
    &["normal", "speed"],
    &["regular", "speed"],
    &["normal", "pace"],
    &["usual", "speed"],
    &["back", "to", "normal"],
];

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace('’', "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_matches('\'').to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

fn contains_seq(tokens: &[String], phrase: &[&str]) -> bool {
    if phrase.is_empty() || tokens.len() < phrase.len() {
        return false;
    }
    tokens
        .windows(phrase.len())
        .any(|w| w.iter().zip(phrase.iter()).all(|(a, b)| a == b))
}

/// Non-filler tokens that are neither in the matched phrase's word set nor
/// fillers — the "how much else did they say" measure.
fn leftover_count(tokens: &[String], phrase_words: &[&str]) -> usize {
    tokens
        .iter()
        .filter(|t| !phrase_words.contains(&t.as_str()))
        .filter(|t| !FILLERS.contains(&t.as_str()))
        .count()
}

/// Match `tokens` against a phrase table: some phrase appears AND at most
/// `max_leftover` other substantive words ride along. Leftover counting
/// excludes the WHOLE table's vocabulary, so stacked same-type commands
/// ("hold on, let me check") read as one command, while genuinely
/// substantive content still disqualifies.
fn phrase_command(tokens: &[String], phrases: &[&[&str]], max_leftover: usize) -> bool {
    if !phrases.iter().any(|phrase| contains_seq(tokens, phrase)) {
        return false;
    }
    let table_words: Vec<&str> = phrases.iter().flat_map(|p| p.iter().copied()).collect();
    leftover_count(tokens, &table_words) <= max_leftover
}

/// True when an utterance is a PURE backchannel — a brief acknowledgement
/// ("yeah", "oh cool", "that's awesome") that keeps the floor with the
/// current speaker. Used by the live scratchpad to decide whether overlap
/// speech should steer the reply (substantive) or ride along (backchannel).
pub fn is_backchannel(text: &str) -> bool {
    let tokens = words(text);
    if tokens.is_empty() || tokens.len() > 4 {
        return false;
    }
    tokens.iter().all(|t| {
        matches!(
            t.as_str(),
            "yeah" | "yep" | "yes" | "ok" | "okay" | "mm" | "mhm" | "hmm" | "uh" | "huh" | "right"
                | "sure" | "cool" | "awesome" | "great" | "nice" | "good" | "oh" | "ah" | "wow"
                | "alright" | "thanks" | "thank" | "you" | "got" | "it" | "totally" | "exactly"
                | "perfect" | "fantastic" | "lovely" | "brilliant" | "that's" | "thats" | "so"
        )
    })
}

/// True when an utterance is a BARE HESITATION — a thinking sound ("Uh",
/// "Um", "Well...") with no content. The right response is silence: the
/// caller is composing, not asking. (Live 2026-07-13: "Uh" fragments were
/// each answered with chatter, derailing the goodbye.)
pub fn is_hesitation(text: &str) -> bool {
    let tokens = words(text);
    !tokens.is_empty()
        && tokens.len() <= 2
        && tokens.iter().all(|t| {
            matches!(
                t.as_str(),
                "uh" | "um" | "er" | "ah" | "hmm" | "mm" | "well" | "so" | "like" | "erm"
            )
        })
}

/// Parse one final caller utterance into a floor intent.
///
/// Conservative by construction: negations and question leads disqualify,
/// phrase matches must leave (almost) no unexplained content, and anything
/// ambiguous is Content. A missed command costs one awkward reply; a false
/// positive makes the receptionist go mute — the asymmetry drives every
/// threshold here.
pub fn parse_caller_intent(text: &str) -> CallerIntent {
    let tokens = words(text);
    if tokens.is_empty() {
        return CallerIntent::Content;
    }
    if tokens.iter().any(|t| NEGATIONS.contains(&t.as_str())) {
        return CallerIntent::Content;
    }
    if QUESTION_LEADS.contains(&tokens[0].as_str()) {
        return CallerIntent::Content;
    }
    // A caller reaching for a HUMAN ("let me speak to a person") must always
    // land in the normal reply path, never in a floor command.
    if tokens.iter().any(|t| {
        matches!(
            t.as_str(),
            "person" | "human" | "someone" | "somebody" | "operator" | "staff" | "manager"
                | "owner" | "representative" | "agent"
        )
    }) {
        return CallerIntent::Content;
    }

    let has = |w: &str| tokens.iter().any(|t| t == w);
    let slow_word = has("slower")
        || has("slowly")
        || contains_seq(&tokens, &["slow", "down"])
        || contains_seq(&tokens, &["too", "fast"])
        || contains_seq(&tokens, &["more", "slowly"]);
    let repeat_word = REPEAT_PHRASES.iter().any(|p| contains_seq(&tokens, p))
        || (has("say") && (has("again") || has("number")));

    // "Say the number slower" / "repeat that slower": replay beats a plain
    // pace change, checked first while both flags are visible.
    if repeat_word && slow_word && tokens.len() <= 12 {
        return CallerIntent::RepeatSlower;
    }
    if phrase_command(&tokens, STOP_PHRASES, 1) {
        return CallerIntent::StopSpeaking;
    }
    if phrase_command(&tokens, PAUSE_PHRASES, 1) {
        return CallerIntent::Pause;
    }
    if phrase_command(&tokens, RESUME_PHRASES, 1) {
        return CallerIntent::Resume;
    }
    if phrase_command(&tokens, NORMAL_SPEED_PHRASES, 1) {
        return CallerIntent::NormalSpeed;
    }
    if slow_word {
        // Keyword rule: everything else must be filler/rate words.
        let rate_words = [
            "slow", "slower", "slowly", "down", "too", "fast", "speak", "talk", "say", "more",
            "speed",
        ];
        if leftover_count(&tokens, &rate_words) == 0 {
            return CallerIntent::Slower;
        }
    }
    {
        let fast_word = has("faster")
            || has("quicker")
            || contains_seq(&tokens, &["speed", "up"])
            || contains_seq(&tokens, &["too", "slow"]);
        if fast_word {
            let rate_words = [
                "fast", "faster", "quicker", "speed", "up", "too", "slow", "speak", "talk", "go",
            ];
            if leftover_count(&tokens, &rate_words) == 0 {
                return CallerIntent::Faster;
            }
        }
    }
    if phrase_command(&tokens, REPEAT_PHRASES, 1) {
        return CallerIntent::Repeat;
    }
    CallerIntent::Content
}

/// What the radio loop should DO with an intent, given the dialogue state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogueAction {
    /// Answer through the normal reply path (LLM in agent mode).
    ReplyNormally,
    /// Say nothing at all — the caller asked for the floor to stay open.
    StaySilent,
    /// Apply a pace change and give a one-line spoken acknowledgement.
    AdjustPace(PaceCommand),
    /// Replay the last spoken reply verbatim (optionally slowed).
    Replay { slower: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceCommand {
    Slower,
    Faster,
    Normal,
}

/// The pause state machine. Pure; the radio loop owns one per call.
#[derive(Debug, Default)]
pub struct DialogueState {
    paused: bool,
}

impl DialogueState {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while the caller has asked Aokie to hold the floor open
    /// ("wait", "let me think", "stop"). Silence-watchdog windows stretch
    /// while paused so a deliberately quiet caller isn't nagged.
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Reset at every call boundary.
    pub fn reset(&mut self) {
        self.paused = false;
    }

    /// Fold one caller intent into the state and say what to do.
    pub fn apply(&mut self, intent: CallerIntent) -> DialogueAction {
        match intent {
            CallerIntent::Pause | CallerIntent::StopSpeaking => {
                self.paused = true;
                DialogueAction::StaySilent
            }
            CallerIntent::Resume | CallerIntent::Content => {
                self.paused = false;
                DialogueAction::ReplyNormally
            }
            CallerIntent::Slower => DialogueAction::AdjustPace(PaceCommand::Slower),
            CallerIntent::Faster => DialogueAction::AdjustPace(PaceCommand::Faster),
            CallerIntent::NormalSpeed => DialogueAction::AdjustPace(PaceCommand::Normal),
            CallerIntent::Repeat => {
                self.paused = false;
                DialogueAction::Replay { slower: false }
            }
            CallerIntent::RepeatSlower => {
                self.paused = false;
                DialogueAction::Replay { slower: true }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(s: &str) -> CallerIntent {
        parse_caller_intent(s)
    }

    #[test]
    fn pause_commands_and_thinking_phrases() {
        for s in [
            "wait",
            "Wait!",
            "wait wait wait",
            "hold on",
            "hold on a second",
            "hang on",
            "one moment please",
            "give me a moment",
            "bear with me",
            "let me think",
            "I'm just thinking",
            "hold on, let me check",
            "let me see...",
            "just need a minute",
        ] {
            assert_eq!(intent(s), CallerIntent::Pause, "{s:?}");
        }
    }

    #[test]
    fn stop_commands() {
        for s in [
            "stop",
            "Stop!",
            "stop talking",
            "please stop",
            "stop please",
            "be quiet",
            "shush",
            "let me speak",
            "let me finish",
            "okay stop",
        ] {
            assert_eq!(intent(s), CallerIntent::StopSpeaking, "{s:?}");
        }
    }

    #[test]
    fn stop_and_wait_inside_real_content_are_content() {
        for s in [
            "can I stop by the office tomorrow",
            "the bus stop is nearby",
            "don't stop",
            "do not stop",
            "I need to stop my subscription",
            "should I stop?",
            "wait, I want to change the booking to Friday",
            "we waited for an hour last time",
            "stop the appointment reminder emails",
            "I can't wait for the weekend",
        ] {
            assert_eq!(intent(s), CallerIntent::Content, "{s:?}");
        }
    }

    #[test]
    fn resume_commands_and_lookalikes() {
        for s in ["continue", "go ahead", "okay go ahead", "keep going", "carry on", "I'm back"] {
            assert_eq!(intent(s), CallerIntent::Resume, "{s:?}");
        }
        for s in [
            "go ahead and book it for Tuesday",
            "continue with the order I placed yesterday",
        ] {
            assert_eq!(intent(s), CallerIntent::Content, "{s:?}");
        }
    }

    #[test]
    fn pace_commands() {
        for s in [
            "slower",
            "slow down",
            "can you slow down please",
            "a bit slower",
            "too fast",
            "speak more slowly",
        ] {
            assert_eq!(intent(s), CallerIntent::Slower, "{s:?}");
        }
        for s in ["faster", "speed up", "a bit quicker", "you can talk faster"] {
            assert_eq!(intent(s), CallerIntent::Faster, "{s:?}");
        }
        for s in ["normal speed", "back to normal speed", "regular speed please"] {
            assert_eq!(intent(s), CallerIntent::NormalSpeed, "{s:?}");
        }
        // Pace words inside substantive content stay content.
        for s in [
            "the delivery was too fast last time",
            "my internet is too slow at home",
            "not so fast",
        ] {
            assert_eq!(intent(s), CallerIntent::Content, "{s:?}");
        }
    }

    #[test]
    fn repeat_and_repeat_slower() {
        for s in ["repeat that", "say that again", "can you repeat that", "pardon", "what was that"] {
            assert_eq!(intent(s), CallerIntent::Repeat, "{s:?}");
        }
        for s in [
            "say that again slower",
            "repeat that slower",
            "can you say the number slower",
            "say the number again more slowly",
            "repeat that more slowly please",
        ] {
            assert_eq!(intent(s), CallerIntent::RepeatSlower, "{s:?}");
        }
        assert_eq!(
            intent("can you repeat the opening hours you mentioned for the city branch"),
            CallerIntent::Content,
            "repeat buried in a long specific request stays content"
        );
    }

    #[test]
    fn ordinary_receptionist_traffic_is_content() {
        for s in [
            "I'd like to book a haircut for Tuesday",
            "my number is 0412 345 678",
            "yes that works",
            "no",
            "yeah",
            "do you open on Saturdays",
            "it's Lance",
            "",
            "   ",
        ] {
            assert_eq!(intent(s), CallerIntent::Content, "{s:?}");
        }
    }

    #[test]
    fn dialogue_state_pause_resume_flow() {
        let mut d = DialogueState::new();
        assert!(!d.is_paused());

        // "hold on" → silence, paused.
        assert_eq!(d.apply(CallerIntent::Pause), DialogueAction::StaySilent);
        assert!(d.is_paused());
        // More pause words while paused: still silent, still paused.
        assert_eq!(d.apply(CallerIntent::StopSpeaking), DialogueAction::StaySilent);
        assert!(d.is_paused());
        // A pace command while paused acts but does not end the pause.
        assert_eq!(
            d.apply(CallerIntent::Slower),
            DialogueAction::AdjustPace(PaceCommand::Slower)
        );
        assert!(d.is_paused(), "rate tweak does not steal the floor");
        // Substantive content ends the pause and replies.
        assert_eq!(d.apply(CallerIntent::Content), DialogueAction::ReplyNormally);
        assert!(!d.is_paused());

        // Pause → explicit resume.
        d.apply(CallerIntent::Pause);
        assert_eq!(d.apply(CallerIntent::Resume), DialogueAction::ReplyNormally);
        assert!(!d.is_paused());

        // Replay exits the pause (the caller is engaging again).
        d.apply(CallerIntent::Pause);
        assert_eq!(
            d.apply(CallerIntent::RepeatSlower),
            DialogueAction::Replay { slower: true }
        );
        assert!(!d.is_paused());

        d.apply(CallerIntent::Pause);
        d.reset();
        assert!(!d.is_paused(), "call boundary clears the pause");
    }
}
