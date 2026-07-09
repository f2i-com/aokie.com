//! Text normalization for speech synthesis, shared by the Aokie plugin's
//! in-call TTS and the aokie-voice-server HTTP service.

/// Normalize text for speech synthesis. TTS models stumble over dotted
/// abbreviations butting into punctuation — "10 p.m., and…" gets voiced as a
/// mangled "pee-em-comma" stutter. Rewrites, boundary-aware:
///   - `a.m` / `p.m` (any case, optional trailing dot handled by the cleanup
///     pass) → `AM` / `PM`, only when not inside a word ("Sam. said" is safe);
///   - punctuation clusters the rewrite can leave behind (".," "..", ".!", ".?")
///     collapse to their terminal mark;
///   - markdown residue a small LLM sometimes emits (`*`, `` ` ``) is dropped.
pub fn normalize_speech_text(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let lower = c.to_ascii_lowercase();
        // Digits are a valid left boundary ("9p.m." → "9PM"); letters are not ("Sam." stays).
        let left_ok = i == 0 || !chars[i - 1].is_ascii_alphabetic();
        // Match `a.m` / `p.m` with a non-letter (or end) on the right of the `m`.
        if left_ok
            && (lower == 'a' || lower == 'p')
            && i + 2 < chars.len()
            && chars[i + 1] == '.'
            && chars[i + 2].to_ascii_lowercase() == 'm'
            && chars.get(i + 3).is_none_or(|r| !r.is_ascii_alphanumeric())
        {
            out.push_str(if lower == 'a' { "AM" } else { "PM" });
            i += 3;
            continue;
        }
        if c == '*' || c == '`' {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // Cleanup pass: collapse the punctuation clusters left by "a.m." → "AM.".
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_normalizer_fixes_am_pm_stutter() {
        // The reported bug: "10 a.m., and" was voiced as a "pee-em-comma" stutter.
        assert_eq!(
            normalize_speech_text("I have you booked for 10 a.m., and we'll call you."),
            "I have you booked for 10 AM, and we'll call you."
        );
        assert_eq!(normalize_speech_text("See you at 2 p.m."), "See you at 2 PM.");
        assert_eq!(normalize_speech_text("See you at 2 P.M.!"), "See you at 2 PM!");
        assert_eq!(normalize_speech_text("Open until 9p.m. tomorrow"), "Open until 9PM. tomorrow");
        // Sentence boundary after the abbreviation survives as ONE period.
        assert_eq!(
            normalize_speech_text("Booked for 10 a.m.. Anything else?"),
            "Booked for 10 AM. Anything else?"
        );
        // Names and words containing am/pm are untouched.
        assert_eq!(normalize_speech_text("Sam. said hi"), "Sam. said hi");
        assert_eq!(normalize_speech_text("the spam. filter"), "the spam. filter");
        // Markdown residue is dropped.
        assert_eq!(normalize_speech_text("**Great** — see you `then`"), "Great — see you then");
    }
}
