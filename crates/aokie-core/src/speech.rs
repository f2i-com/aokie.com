//! Text normalization for speech synthesis, shared by the Aokie plugin's
//! in-call TTS and the aokie-voice-server HTTP service.

/// Normalize text for speech synthesis. Real-world TTS failure modes drive
/// this (all heard on live receptionist calls with pocket-tts):
///   1. dotted abbreviations butting into punctuation — "10 p.m., and…" gets
///      voiced as a mangled "pee-em-comma" stutter;
///   2. the token "AM"/"PM" read as a WORD (with a French flavour);
///   3. the spelling "ay em" read as "eye em".
///
/// So meridiems become the user-tuned phonetic spellings "a em" / "pee em" —
/// for the dotted forms ("10 a.m.", "2 P.M.!") and for bare am/pm straight
/// after a digit ("10 AM", "10am", "10:30 PM"). Boundary-aware: "Sam.",
/// "spam.", the word "am" and a shouted "I AM HERE" are never rewritten.
/// Punctuation clusters left by the rewrite (".," "..", ".!") collapse to
/// their terminal mark, and markdown residue (`*`, `` ` ``) is dropped.
///
/// This runs ONLY on the text handed to the synthesizer — transcripts, history
/// and records keep the original wording.
pub fn normalize_speech_text(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let lower = c.to_ascii_lowercase();
        // Digits are a valid left boundary ("9p.m." works); letters are not ("Sam." stays).
        let left_is_letter = i > 0 && chars[i - 1].is_ascii_alphabetic();
        // Is the nearest non-space character to the left a digit (i.e. a time)?
        let after_digit = chars[..i]
            .iter()
            .rev()
            .find(|ch| !ch.is_whitespace())
            .is_some_and(|ch| ch.is_ascii_digit());

        if !left_is_letter && (lower == 'a' || lower == 'p') {
            let phonetic = if lower == 'a' { "a em" } else { "pee em" };
            // Dotted form `a.m` / `p.m` (optionally `a.m.`) — always a meridiem.
            if i + 2 < chars.len()
                && chars[i + 1] == '.'
                && chars[i + 2].to_ascii_lowercase() == 'm'
                && chars.get(i + 3).is_none_or(|r| !r.is_ascii_alphanumeric())
            {
                push_phonetic(&mut out, phonetic);
                i += 3;
                continue;
            }
            // Bare `am` / `pm` / `AM` / `PM` — only right after a digit (a time),
            // so "I AM HERE" and the word "am" are never rewritten.
            if after_digit
                && i + 1 < chars.len()
                && chars[i + 1].to_ascii_lowercase() == 'm'
                && chars.get(i + 2).is_none_or(|r| !r.is_ascii_alphanumeric())
            {
                push_phonetic(&mut out, phonetic);
                i += 2;
                continue;
            }
        }
        if c == '*' || c == '`' {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Cleanup pass: collapse the punctuation clusters left by "a.m." → "a em.".
    let mut cleaned = out;
    loop {
        let next = cleaned
            .replace("..", ".")
            .replace(".,", ",")
            .replace(".!", "!")
            .replace(".?", "?")
            .replace(".;", ";")
            .replace(".:", ":");
        if next == cleaned {
            break;
        }
        cleaned = next;
    }
    cleaned
}

/// Append a phonetic meridiem, inserting a space when the text ran straight off
/// a digit ("9pm" → "9 pee em", but "10 pm" keeps its single space).
fn push_phonetic(out: &mut String, phonetic: &str) {
    if out.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        out.push(' ');
    }
    out.push_str(phonetic);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meridiems_become_user_tuned_phonetics() {
        // The reported bug chain: "10 a.m.," stuttered; "AM" read as a word;
        // "ay em" read as "eye em" — the user picked "a em".
        assert_eq!(
            normalize_speech_text("I have you booked for 10 a.m., and we'll call you."),
            "I have you booked for 10 a em, and we'll call you."
        );
        assert_eq!(normalize_speech_text("See you at 2 p.m."), "See you at 2 pee em.");
        assert_eq!(normalize_speech_text("See you at 2 P.M.!"), "See you at 2 pee em!");
        assert_eq!(
            normalize_speech_text("Open until 9p.m. tomorrow"),
            "Open until 9 pee em. tomorrow"
        );
        // Sentence boundary after the abbreviation survives as ONE period.
        assert_eq!(
            normalize_speech_text("Booked for 10 a.m.. Anything else?"),
            "Booked for 10 a em. Anything else?"
        );
    }

    #[test]
    fn bare_am_pm_only_converts_after_a_time() {
        assert_eq!(normalize_speech_text("see you at 10 AM."), "see you at 10 a em.");
        assert_eq!(normalize_speech_text("booked for 10am sharp"), "booked for 10 a em sharp");
        assert_eq!(normalize_speech_text("at 10:30 PM tonight"), "at 10:30 pee em tonight");
        // NOT times — never rewritten.
        assert_eq!(normalize_speech_text("I AM HERE"), "I AM HERE");
        assert_eq!(normalize_speech_text("am I early?"), "am I early?");
        assert_eq!(normalize_speech_text("the PM will visit"), "the PM will visit");
    }

    #[test]
    fn words_survive_and_markdown_is_stripped() {
        assert_eq!(normalize_speech_text("Sam. said hi"), "Sam. said hi");
        assert_eq!(normalize_speech_text("the spam. filter"), "the spam. filter");
        assert_eq!(normalize_speech_text("**Great** — see you `then`"), "Great — see you then");
    }
}
