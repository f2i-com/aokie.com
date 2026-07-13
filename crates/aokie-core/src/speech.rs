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
    // Clock times first, so the meridiem pass sees the cleaned form:
    // "10:00 AM" -> "10 AM" -> "10 a em".
    let input = &normalize_clock_times(input);
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

/// Rewrite `H:MM` clock times into forms the TTS reads naturally — the colon
/// made it spell the minutes out ("10:00" read as "ten zero zero" / colon
/// noises, live report 2026-07-13):
///   "10:00" → "10"        (on-the-hour: just the hour)
///   "10:15" → "10 15"     ("ten fifteen")
///   "10:05" → "10 oh 5"   ("ten oh five")
/// Strictly shaped: 1-2 digit hour (0-23), exactly 2-digit minutes (00-59),
/// no digit on either side — "3:1" (a ratio), "10:154" and "100:30" are
/// untouched. Runs only on synthesizer text, never on transcripts/records.
fn normalize_clock_times(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        // Candidate start: a digit with no digit immediately before it.
        if chars[i].is_ascii_digit() && (i == 0 || !chars[i - 1].is_ascii_digit()) {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let hour_len = j - i;
            let is_time = hour_len <= 2
                && j < chars.len()
                && chars[j] == ':'
                && j + 2 < chars.len() + 1
                && chars.get(j + 1).is_some_and(|c| c.is_ascii_digit())
                && chars.get(j + 2).is_some_and(|c| c.is_ascii_digit())
                && chars.get(j + 3).is_none_or(|c| !c.is_ascii_digit());
            if is_time {
                let hour: u32 = chars[i..j].iter().collect::<String>().parse().unwrap_or(99);
                let m1 = chars[j + 1];
                let m2 = chars[j + 2];
                let minutes: u32 = format!("{m1}{m2}").parse().unwrap_or(99);
                if hour <= 23 && minutes <= 59 {
                    for k in i..j {
                        out.push(chars[k]);
                    }
                    if minutes == 0 {
                        // on the hour: drop ":00" entirely
                    } else if minutes < 10 {
                        out.push_str(" oh ");
                        out.push(m2);
                    } else {
                        out.push(' ');
                        out.push(m1);
                        out.push(m2);
                    }
                    i = j + 3;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
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
        assert_eq!(normalize_speech_text("at 10:30 PM tonight"), "at 10 30 pee em tonight");
        // NOT times — never rewritten.
        assert_eq!(normalize_speech_text("I AM HERE"), "I AM HERE");
        assert_eq!(normalize_speech_text("am I early?"), "am I early?");
        assert_eq!(normalize_speech_text("the PM will visit"), "the PM will visit");
    }

    #[test]
    fn clock_times_read_naturally() {
        // The reported bug: "10:00" spoken as "ten zero zero" / colon noise.
        assert_eq!(normalize_speech_text("booked for 10:00 AM."), "booked for 10 a em.");
        assert_eq!(normalize_speech_text("see you at 10:15."), "see you at 10 15.");
        assert_eq!(normalize_speech_text("at 10:05 pm"), "at 10 oh 5 pee em");
        assert_eq!(normalize_speech_text("open 9:00 to 17:30"), "open 9 to 17 30");
        // NOT clock times — untouched.
        assert_eq!(normalize_speech_text("a 3:1 ratio"), "a 3:1 ratio");
        assert_eq!(normalize_speech_text("code 10:154 please"), "code 10:154 please");
        assert_eq!(normalize_speech_text("item 100:30 stays"), "item 100:30 stays");
        assert_eq!(normalize_speech_text("at 75:00 minutes?"), "at 75:00 minutes?");
    }

    #[test]
    fn words_survive_and_markdown_is_stripped() {
        assert_eq!(normalize_speech_text("Sam. said hi"), "Sam. said hi");
        assert_eq!(normalize_speech_text("the spam. filter"), "the spam. filter");
        assert_eq!(normalize_speech_text("**Great** — see you `then`"), "Great — see you then");
    }
}
