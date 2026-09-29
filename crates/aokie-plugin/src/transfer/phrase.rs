//! The "caller asked" phrase check.
//!
//! The AI may ask for a transfer with the reason `caller_asked` only when the
//! caller really did ask for a person. The OAIY host runs this check on the
//! last three caller turns when it plans the ring; the plugin runs the same
//! rules first, so a model that claims `caller_asked` after a caller who said
//! nothing of the kind is refused before anything is asked of the host.
//!
//! The rules are the reference implementation of appendix A.9 of the mobile
//! design, and `docs/contracts/transfer/caller-asked.fixtures.json` holds the
//! shared cases. Both repositories test against that one file.
//!
//! What the check does not do, on purpose, is understand negation beyond the
//! block list: "please don't put me through" still matches `put me through`.
//! It is a floor under the host's own policy (initiative setting, limits, quiet
//! hours), never the whole of it.

use regex::Regex;
use std::sync::OnceLock;

/// How many of the caller's most recent turns are looked at.
pub const RECENT_TURNS: usize = 3;

const PERSON: &str = "(?:the |your |a |an )?(?:owner|manager|boss|proprietor|person|human|real person|actual person|someone|somebody|staff|member of staff|representative)";

struct Rules {
    rules: Vec<Regex>,
    blocks: Vec<Regex>,
}

fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        let compile = |pattern: String| Regex::new(&pattern).expect("phrase rule compiles");
        Rules {
            rules: vec![
                compile(format!(r"\b(?:speak|talk|chat)(?:ing)? (?:to|with) {PERSON}\b")),
                compile(r"\b(?:put|patch) me through\b".into()),
                compile(r"\btransfer me\b".into()),
                compile(format!(r"\b(?:get|find|fetch) (?:me )?{PERSON}\b")),
                compile(r"\b(?:is|are) (?:the |your )?(?:owner|manager|boss|anyone|anybody|somebody|someone) (?:there|available|in|around|free)\b".into()),
                compile(r"\b(?:real|actual) (?:person|human)\b".into()),
                compile(r"\b(?:i )?(?:want|need|would like|d like|wanna) (?:to )?(?:speak|talk) (?:to|with)\b".into()),
            ],
            blocks: vec![
                compile(r"\b(?:my|our|his|her|their) (?:owner|manager|boss)\b".into()),
                compile(r"\bowner of\b".into()),
                compile(r"\btalk to you later\b".into()),
                compile(r"\bspeak to you (?:later|soon)\b".into()),
                compile(r"\bdon't (?:want|need) (?:to )?(?:speak|talk)\b".into()),
                compile(r"\bno need to (?:speak|talk|transfer)\b".into()),
            ],
        }
    })
}

/// Characters phones, keyboards and speech engines use for the apostrophe:
/// left and right single quotation marks, modifier letter apostrophe, single
/// high-reversed-9 quotation mark, prime, fullwidth apostrophe, grave accent
/// and acute accent.
const APOSTROPHES: [char; 8] = [
    '\u{2018}', '\u{2019}', '\u{02BC}', '\u{201B}', '\u{2032}', '\u{FF07}', '`', '\u{00B4}',
];

/// Lower-case; the apostrophe look-alikes become `'`; every run of characters
/// outside `[a-z0-9' ]` becomes one space; every run of spaces becomes one
/// space; trim. The last two steps together mean punctuation between words
/// never leaves the double space that would defeat a single-space pattern
/// ("speak, to the owner"), and a curly apostrophe reads as the straight one
/// ("I don\u{2019}t want to speak" is blocked like "I don't want to speak").
pub(crate) fn normalize(turn: &str) -> String {
    let mut out = String::with_capacity(turn.len());
    let mut after_space = true;
    for character in turn.to_lowercase().chars() {
        let character = if APOSTROPHES.contains(&character) {
            '\''
        } else {
            character
        };
        if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '\'' {
            after_space = false;
            out.push(character);
        } else if !after_space {
            after_space = true;
            out.push(' ');
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Whether one of the last three caller turns asks for a person. A turn counts
/// when no block pattern matches it and at least one rule does.
pub fn caller_asked<S: AsRef<str>>(turns: &[S]) -> bool {
    let rules = rules();
    let start = turns.len().saturating_sub(RECENT_TURNS);
    turns[start..].iter().any(|turn| {
        let turn = normalize(turn.as_ref());
        !rules.blocks.iter().any(|block| block.is_match(&turn))
            && rules.rules.iter().any(|rule| rule.is_match(&turn))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSITIVE: [&[&str]; 8] = [
        &["Can I speak to the owner?"],
        &["I'd like to talk to a real person please"],
        &["Could you put me through to the manager"],
        &["Is the owner there?"],
        &["Transfer me to somebody"],
        &["Yeah I want to speak with someone about this"],
        &["um", "get me the boss"],
        &["I need to talk to a human"],
    ];

    const NEGATIVE: [&[&str]; 8] = [
        &["I'd like to book a mow for Tuesday"],
        &["The owner of the house is away that week"],
        &["My manager said it was fine"],
        &["I'll talk to you later"],
        &["Someone is coming on Tuesday to open the gate"],
        &["No need to speak to anyone, just book it"],
        &["Is Tuesday free?"],
        &["I spoke to the owner yesterday"],
    ];

    #[test]
    fn the_design_fixtures_pass() {
        for turns in POSITIVE {
            assert!(caller_asked(turns), "{turns:?}");
        }
        for turns in NEGATIVE {
            assert!(!caller_asked(turns), "{turns:?}");
        }
    }

    #[test]
    fn normalisation_is_the_reference_normalisation() {
        assert_eq!(normalize("Can I speak to the owner?"), "can i speak to the owner");
        // A run of punctuation becomes one space, and so does a run of spaces:
        // punctuation between words never leaves a double space behind.
        assert_eq!(normalize("  Hello,,, WORLD!! "), "hello world");
        assert_eq!(normalize("Hello, WORLD!"), "hello world");
        assert_eq!(normalize("Hello WORLD!"), "hello world");
        assert_eq!(normalize("a  b"), "a b");
        assert_eq!(normalize("a \t\n b"), "a b");
        assert_eq!(normalize("it's fine"), "it's fine");
        // Non-ASCII letters are punctuation to this check.
        assert_eq!(normalize("caf\u{e9}ok"), "caf ok");
        assert_eq!(normalize("caf\u{e9} ok"), "caf ok");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize(" ,, "), "");
    }

    #[test]
    fn every_apostrophe_look_alike_is_the_ascii_apostrophe() {
        for apostrophe in APOSTROPHES {
            assert_eq!(
                normalize(&format!("I don{apostrophe}t want to speak")),
                "i don't want to speak",
                "{apostrophe:?}"
            );
        }
        // A curly apostrophe blocks exactly what the straight one does.
        for apostrophe in ['\'', '\u{2018}', '\u{2019}', '\u{02BC}'] {
            assert!(
                !caller_asked(&[format!("I don{apostrophe}t want to speak to anyone")]),
                "{apostrophe:?}"
            );
            assert!(
                !caller_asked(&[format!("no, I don{apostrophe}t need to talk to a person")]),
                "{apostrophe:?}"
            );
            assert!(
                caller_asked(&[format!("I{apostrophe}d like to talk to a real person please")]),
                "{apostrophe:?}"
            );
        }
        // A quote that is not an apostrophe stays punctuation.
        assert_eq!(normalize("\u{201C}hello\u{201D}"), "hello");
    }

    #[test]
    fn only_the_last_three_turns_count() {
        let turns = [
            "Can I speak to the owner?",
            "no",
            "sorry",
            "the gate is blue",
        ];
        assert!(!caller_asked(&turns), "an ask four turns back is not current");
        assert!(caller_asked(&turns[..3]), "three turns back still is");
        assert!(!caller_asked::<&str>(&[]));
        assert!(!caller_asked(&[""]));
    }

    #[test]
    fn a_blocked_turn_does_not_count_but_another_turn_can() {
        assert!(!caller_asked(&["My manager said transfer me"]));
        assert!(caller_asked(&["My manager said no", "transfer me please"]));
    }

    #[test]
    fn ordinary_business_talk_is_not_a_request() {
        assert!(!caller_asked(&["How much does a mow cost?", "and for a hedge?"]));
        assert!(!caller_asked(&["I'd like to move my appointment to Friday"]));
        assert!(!caller_asked(&["Is the price for the whole lawn?"]));
    }

    /// Known weaknesses of the reference rules, kept here so nobody mistakes
    /// this check for more than a floor. They are reported to the design owner;
    /// changing them means changing the shared fixture file on both sides.
    #[test]
    fn known_gaps_of_the_reference_rules() {
        // Negation of the request itself is not understood.
        assert!(caller_asked(&["please don't transfer me"]));
        assert!(caller_asked(&["don't put me through to the manager"]));
        // A caller repeating what someone else said still asks, as far as the
        // rules can tell.
        assert!(caller_asked(&["they said they would get the owner to call"]));
    }

    /// Review finding 7: punctuation inside a request no longer defeats the
    /// single-space patterns (this used to be a known gap).
    #[test]
    fn punctuation_between_the_words_of_a_request_does_not_defeat_it() {
        assert!(caller_asked(&["speak, to the owner"]));
        assert!(caller_asked(&["Can I speak -- to the owner?"]));
        assert!(caller_asked(&["Can I please talk,   with a person"]));
    }
}
