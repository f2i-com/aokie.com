//! Text normalization for speech synthesis, shared by the Aokie plugin's
//! in-call TTS and the aokie-voice-server HTTP service.

/// Normalize text for speech synthesis. Two real-world TTS failure modes drive
/// this (both heard on live receptionist calls with pocket-tts):
///   1. dotted abbreviations butting into punctuation — "10 p.m., and…" gets
///      voiced as a mangled "pee-em-comma" stutter;
///   2. the uppercase token "AM"/"PM" itself — the model reads it as a WORD
///      (with a distinctly French flavour), not as the meridiem letters.
///
/// So `a.m` / `p.m` (any case, dotted or not, with or without a space after the
/// hour: "10 a.m.", "2 P.M.!", "9pm", "10:30 AM") become the PHONETIC "ay em" /
/// "pee em", which every voice pronounces correctly. Boundary-aware: "Sam.",
/// "spam." and a shouted "I AM HERE" are untouched (bare am/pm/AM/PM only
/// converts right after a digit — i.e. a time). Punctuation clusters left by
/// the rewrite (".," "..", ".!") collapse to their terminal mark, and markdown
/// residue (`*`, `` ` ``) is dropped.
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
        // Digits are a valid left boundary ("9p.m." → "9 pee em"); letters are not ("Sam." stays).
        let left_is_letter = i > 0 && chars[i - 1].is_ascii_alphabetic();
        // The nearest non-space/colon character to the left — a digit there means
        // this am/pm follows a time ("10 AM", "10:30pm").
        let after_digit = chars[..i]
            .iter()
            .rev()
            .find(|ch| !ch.is_whitespace() && **ch != ':')
            .is_some_and(|ch| ch.is_ascii_digit());

        if !left_is_letter && (lower == 'a' || lower == 'p') {
            let phonetic = if lower == 'a' { "ay em" } else { "pee em" };
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
    // Cleanup pass: collapse the punctuation clusters left by "a.m." → "ay em.".
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
    fn speech_normalizer_fixes_am_pm_stutter() {
        // The reported bug: "10 a.m., and" was voiced as a "pee-em-comma" stutter.
        assert_eq!(
            normalize_speech_text("I have you booked for 10 a.m., and we'll call you."),
            "I have you booked for 10 ay em, and we'll call you."
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
            "Booked for 10 ay em. Anything else?"
        );
    }

    #[test]
    fn speech_normalizer_converts_bare_am_pm_only_after_a_time() {
        // The LLM often writes the meridiem without dots — still phonetic.
        assert_eq!(normalize_speech_text("see you at 10 AM."), "see you at 10 ay em.");
        assert_eq!(normalize_speech_text("booked for 10am sharp"), "booked for 10 ay em sharp");
        assert_eq!(normalize_speech_text("at 10:30 PM tonight"), "at 10:30 pee em tonight");
        // NOT times — never rewritten.
        assert_eq!(normalize_speech_text("I AM HERE"), "I AM HERE");
        assert_eq!(normalize_speech_text("am I early?"), "am I early?");
        assert_eq!(normalize_speech_text("the PM will visit"), "the PM will visit");
    }

    #[test]
    fn speech_normalizer_leaves_words_and_strips_markdown() {
        // Names and words containing am/pm are untouched.
        assert_eq!(normalize_speech_text("Sam. said hi"), "Sam. said hi");
        assert_eq!(normalize_speech_text("the spam. filter"), "the spam. filter");
        // Markdown residue is dropped.
        assert_eq!(normalize_speech_text("**Great** — see you `then`"), "Great — see you then");
    }
}
