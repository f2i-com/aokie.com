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
    let input = &normalize_calendar_dates(input);
    // Currency first (live report 2026-07-14: "$9" in a menu read-back was
    // voiced wrong): "$18" -> "18 dollars", "$1" -> "1 dollar",
    // "$18.50" -> "18 dollars and 50 cents" — the sign becomes a spoken word
    // AFTER the amount, the way a person says a price.
    let input = &normalize_currency(input);
    // Date abbreviations next (live report 2026-07-14: "Tue Jul 14 2026" —
    // a toDateString-style label echoed by the model — was voiced as garbled
    // numbers): "Tue" -> "Tuesday", "Jul" -> "July" when they sit in date
    // context, so the synthesizer reads dates like a person would.
    let input = &expand_date_abbreviations(input);
    // Clock times become spoken words before the remaining bare-hour
    // meridiem pass: "10:05 AM" -> "ten oh five a em".
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

/// ISO calendar dates are dates, never slow digit-by-digit codes. Only the
/// synthesizer receives this rendering; stored booking values stay unchanged.
pub fn normalize_calendar_dates(input: &str) -> String {
    use chrono::Datelike;
    calendar_dates_for_year(input, chrono::Local::now().year())
}

fn iso_date(input: &str) -> Option<chrono::NaiveDate> {
    let b = input.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-'
        || !b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()) {
        return None;
    }
    chrono::NaiveDate::parse_from_str(input, "%Y-%m-%d").ok()
}

/// Used before speech-plan digit segmentation, including punctuation-wrapped
/// dates. Invalid dates are not guessed or silently repaired.
pub fn is_calendar_date_token(token: &str) -> bool {
    iso_date(token.trim_matches(|c: char| matches!(c, '(' | ')' | '.' | ',' | '!' | '?' | ';' | ':' | '*' | '`'))).is_some()
}

fn calendar_dates_for_year(input: &str, current_year: i32) -> String {
    use chrono::Datelike;
    const ORDINALS: [&str; 31] = ["first", "second", "third", "fourth", "fifth",
        "sixth", "seventh", "eighth", "ninth", "tenth", "eleventh", "twelfth",
        "thirteenth", "fourteenth", "fifteenth", "sixteenth", "seventeenth",
        "eighteenth", "nineteenth", "twentieth", "twenty-first", "twenty-second",
        "twenty-third", "twenty-fourth", "twenty-fifth", "twenty-sixth",
        "twenty-seventh", "twenty-eighth", "twenty-ninth", "thirtieth", "thirty-first"];
    const WEEKDAYS: [&str; 14] = ["Monday", "Tuesday", "Wednesday", "Thursday",
        "Friday", "Saturday", "Sunday", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let part_of_identifier = |c: char| c.is_alphanumeric() || matches!(c, '_' | '/' | '-' | '+');
    let mut out = String::with_capacity(input.len());
    let mut at = 0;
    while at < input.len() {
        let candidate = input.get(at..at.saturating_add(10));
        let left_ok = input[..at].chars().next_back().is_none_or(|c| !part_of_identifier(c));
        let date = candidate.and_then(iso_date).filter(|_| {
            left_ok && input[at + 10..].chars().next().is_none_or(|c| !part_of_identifier(c))
        });
        if let Some(date) = date {
            // A model may have supplied a weekday already. Derive it from the
            // real date instead of saying it twice or repeating a wrong day.
            let prefix = out.trim_end_matches(|c: char| c.is_whitespace() || c == ',');
            let word_start = prefix.rfind(char::is_whitespace).map_or(0, |i| i + 1);
            if WEEKDAYS.iter().any(|day| prefix[word_start..].eq_ignore_ascii_case(day)) {
                out.truncate(word_start);
            }
            out.push_str(&format!("{} the {} of {}", date.format("%A"),
                ORDINALS[date.day() as usize - 1], date.format("%B")));
            if date.year() != current_year {
                out.push_str(&format!(", {}", date.year()));
            }
            at += 10;
        } else {
            let c = input[at..].chars().next().expect("character boundary");
            out.push(c);
            at += c.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod calendar_date_tests {
    use super::*;
    #[test]
    fn iso_dates_use_real_weekdays_and_ordinals() {
        assert_eq!(calendar_dates_for_year("On 2026-09-14 at 10:30.", 2026),
            "On Monday the fourteenth of September at 10:30.");
        assert_eq!(calendar_dates_for_year("Wednesday, 2026-09-14.", 2026),
            "Monday the fourteenth of September.");
        assert_eq!(calendar_dates_for_year("2028-02-29", 2026),
            "Tuesday the twenty-ninth of February, 2028");
        assert_eq!(calendar_dates_for_year("2026-09-21 and 2026-09-23", 2026),
            "Monday the twenty-first of September and Wednesday the twenty-third of September");
    }
    #[test]
    fn dates_do_not_rewrite_codes_urls_or_ambiguous_values() {
        for text in ["2026-02-29", "2026-13-01", "14/09/2026", "09/10/2026",
            "ref-2026-09-14", "https://example.test/2026-09-14", "2026-09-14T10:30:00Z",
            "0412 345 678", "x2026-09-14", "2026-09-140"] {
            assert_eq!(calendar_dates_for_year(text, 2026), text);
        }
        assert_eq!(calendar_dates_for_year("Café (2026-09-14).", 2026),
            "Café (Monday the fourteenth of September).");
    }
    #[test]
    fn date_speech_is_idempotent() {
        let text = normalize_speech_text("2026-09-14 at 10:30 AM.");
        assert!(text.contains("Monday the fourteenth of September"));
        assert!(!text.contains("2026-09-14"));
        assert_eq!(normalize_speech_text(&text), text);
    }
}

/// Expand abbreviated weekday/month names when they sit in DATE CONTEXT, so
/// "Tue Jul 14 2026 at 6 PM" is voiced "Tuesday July 14 2026…". Conservative
/// by design: only exactly-capitalized abbreviations ("Tue", "Sept") expand,
/// and only next to a date neighbour — a day abbreviation needs a following
/// month name or day-of-month number; a month abbreviation needs a
/// day-of-month number on either side. "he sat 14 exams", the word "sun" and
/// the name "Jan" (no digit neighbour) are never rewritten.
fn expand_date_abbreviations(input: &str) -> String {
    const DAYS: [(&str, &str); 10] = [
        ("Mon", "Monday"),
        ("Tue", "Tuesday"),
        ("Tues", "Tuesday"),
        ("Wed", "Wednesday"),
        ("Thu", "Thursday"),
        ("Thur", "Thursday"),
        ("Thurs", "Thursday"),
        ("Fri", "Friday"),
        ("Sat", "Saturday"),
        ("Sun", "Sunday"),
    ];
    const MONTHS: [(&str, &str); 12] = [
        ("Jan", "January"),
        ("Feb", "February"),
        ("Mar", "March"),
        ("Apr", "April"),
        ("Jun", "June"),
        ("Jul", "July"),
        ("Aug", "August"),
        ("Sep", "September"),
        ("Sept", "September"),
        ("Oct", "October"),
        ("Nov", "November"),
        ("Dec", "December"),
    ];
    const MONTH_FULL: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    fn is_day_number(w: &str) -> bool {
        let w = w.trim_end_matches(|c: char| !c.is_ascii_digit());
        !w.is_empty() && w.len() <= 2 && w.chars().all(|c| c.is_ascii_digit())
    }
    fn is_month_word(w: &str) -> bool {
        let w = w.trim_end_matches('.');
        MONTHS.iter().any(|(a, f)| *a == w || *f == w) || MONTH_FULL.contains(&w)
    }

    // Word-wise pass: split on whitespace, keep the original separators by
    // rebuilding with single spaces only when the input used them — simpler:
    // operate on whitespace-delimited words and rejoin with the ORIGINAL
    // separator slices.
    let mut words: Vec<&str> = Vec::new();
    let mut seps: Vec<&str> = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (w, r) = rest.split_at(word_end);
        words.push(w);
        let sep_end = r.find(|c: char| !c.is_whitespace()).unwrap_or(r.len());
        let (s, r2) = r.split_at(sep_end);
        seps.push(s);
        rest = r2;
    }
    let mut out = String::with_capacity(input.len() + 16);
    for i in 0..words.len() {
        let w = words[i];
        let bare = w.trim_end_matches(['.', ',']);
        let next = words.get(i + 1).copied().unwrap_or("");
        let prev = if i > 0 { words[i - 1] } else { "" };
        let day_hit = DAYS
            .iter()
            .find(|(a, _)| *a == bare)
            .filter(|_| is_month_word(next) || is_day_number(next));
        let month_hit = MONTHS
            .iter()
            .find(|(a, _)| *a == bare)
            .filter(|_| is_day_number(next) || is_day_number(prev));
        if let Some((_, full)) = day_hit.or(month_hit) {
            out.push_str(full);
            // Keep any trailing punctuation the abbreviation carried ("Tue,").
            out.push_str(&w[bare.len()..]);
        } else {
            out.push_str(w);
        }
        out.push_str(seps[i]);
    }
    out
}

/// `$<amount>` → spoken price: "$18" -> "18 dollars", "$1" -> "1 dollar",
/// "$1,200" -> "1200 dollars", "$18.50" -> "18 dollars and 50 cents",
/// "$0.50" -> "50 cents", "$9.5" -> "9 dollars and 50 cents". A `$` not
/// directly followed by a digit is left alone. Synthesizer-input only.
fn normalize_currency(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 16);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            let mut j = i + 1;
            let mut whole = String::new();
            while j < chars.len() {
                if chars[j].is_ascii_digit() {
                    whole.push(chars[j]);
                    j += 1;
                } else if chars[j] == ',' && chars.get(j + 1).is_some_and(|c| c.is_ascii_digit()) {
                    // A thousands separator ONLY when digits follow — the
                    // comma in "$18, steak" is prose punctuation and stays.
                    j += 1;
                } else {
                    break;
                }
            }
            // Optional cents: '.' + 1-2 digits (3+ digits after the dot is
            // not money — "$3.14159" keeps its dot untouched).
            let mut cents: Option<u32> = None;
            if chars.get(j) == Some(&'.') && chars.get(j + 1).is_some_and(|c| c.is_ascii_digit()) {
                let mut k = j + 1;
                let mut frac = String::new();
                while k < chars.len() && chars[k].is_ascii_digit() {
                    frac.push(chars[k]);
                    k += 1;
                }
                if frac.len() <= 2 {
                    let padded = if frac.len() == 1 {
                        format!("{frac}0")
                    } else {
                        frac
                    };
                    cents = padded.parse::<u32>().ok();
                    j = k;
                }
            }
            let dollars: u64 = whole.parse().unwrap_or(0);
            let mut spoken = String::new();
            if dollars > 0 || cents.unwrap_or(0) == 0 {
                spoken.push_str(&format!(
                    "{dollars} dollar{}",
                    if dollars == 1 { "" } else { "s" }
                ));
            }
            if let Some(c) = cents.filter(|c| *c > 0) {
                if !spoken.is_empty() {
                    spoken.push_str(" and ");
                }
                spoken.push_str(&format!("{c} cent{}", if c == 1 { "" } else { "s" }));
            }
            out.push_str(&spoken);
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Speak H:MM[:SS] as words, convert unambiguous 24-hour times, and preserve
/// explicit AM/PM. Boundaries prevent rewriting identifiers and timestamps.
/// Only synthesizer text changes; booking data retains its original value.
fn normalize_clock_times(input: &str) -> String {
    fn number(n: u32) -> String {
        const SMALL: [&str; 20] = ["zero", "one", "two", "three", "four", "five",
            "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen",
            "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen"];
        if n < 20 { return SMALL[n as usize].to_string(); }
        let tens = ["", "", "twenty", "thirty", "forty", "fifty"][(n / 10) as usize];
        if n % 10 == 0 { tens.to_string() } else { format!("{tens}-{}", SMALL[(n % 10) as usize]) }
    }
    let identifier = |c: char| c.is_alphanumeric() || matches!(c, '_' | '/' | ':');
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        // Do not turn timestamp/URL/id fragments into appointment times.
        if chars[i].is_ascii_digit() && (i == 0 || !identifier(chars[i - 1])) {
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
                let mut end = j + 3;
                let mut seconds = 0;
                let mut valid_seconds = true;
                if chars.get(end) == Some(&':') {
                    if chars.get(end + 1).is_some_and(|c| c.is_ascii_digit())
                        && chars.get(end + 2).is_some_and(|c| c.is_ascii_digit()) {
                        seconds = chars[end + 1].to_digit(10).unwrap() * 10
                            + chars[end + 2].to_digit(10).unwrap();
                        end += 3;
                        valid_seconds = seconds <= 59;
                    } else { valid_seconds = false; }
                }
                let mut suffix_at = end;
                while chars.get(suffix_at).is_some_and(|c| c.is_whitespace()) { suffix_at += 1; }
                let mut meridiem = None;
                for (token, pm) in [("a.m", false), ("p.m", true), ("am", false), ("pm", true)] {
                    let n = token.len();
                    if chars.get(suffix_at..suffix_at + n).is_some_and(|s|
                        s.iter().collect::<String>().eq_ignore_ascii_case(token))
                        && chars.get(suffix_at + n).is_none_or(|c| !identifier(*c)) {
                        meridiem = Some(pm);
                        end = suffix_at + n;
                        break;
                    }
                }
                let boundary_ok = chars.get(end).is_none_or(|c| !identifier(*c));
                if hour <= 23 && minutes <= 59 && valid_seconds && boundary_ok
                    && (meridiem.is_none() || (1..=12).contains(&hour)) {
                    if minutes == 0 && seconds == 0 && (hour == 0 || hour == 12 && meridiem.is_some()) {
                        out.push_str(if hour == 0 || meridiem == Some(false) { "midnight" } else { "noon" });
                    } else {
                        out.push_str(&number(if hour == 0 { 12 } else if hour > 12 { hour - 12 } else { hour }));
                        if minutes > 0 {
                            out.push_str(if minutes < 10 { " oh " } else { " " });
                            out.push_str(&number(minutes));
                        }
                        if seconds > 0 {
                            out.push_str(&format!(" and {} second{}", number(seconds), if seconds == 1 { "" } else { "s" }));
                        }
                        let period = meridiem.or(if hour == 0 { Some(false) } else if hour > 12 { Some(true) } else { None });
                        if let Some(pm) = period { out.push_str(if pm { " pee em" } else { " a em" }); }
                    }
                    i = end;
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
    #[test]
    fn date_abbreviations_expand_in_date_context_only() {
        use super::expand_date_abbreviations as x;
        // The live failure: a toDateString-style label.
        assert_eq!(x("Tue Jul 14 2026 at 6 PM"), "Tuesday July 14 2026 at 6 PM");
        assert_eq!(x("booked for Sun Sept 6"), "booked for Sunday September 6");
        // Month next to a day number, either side.
        assert_eq!(x("on 14 Jul"), "on 14 July");
        assert_eq!(x("Jul 14."), "July 14.");
        // Trailing punctuation on the abbreviation is kept.
        assert_eq!(x("Tue, Jul 14"), "Tuesday, July 14");
        assert_eq!(x("Tue. Jul 14"), "Tuesday. July 14");
        // NEVER in ordinary prose: verbs, names, the actual sun.
        assert_eq!(x("he sat 14 exams"), "he sat 14 exams");
        assert_eq!(x("the Sun is bright"), "the Sun is bright");
        assert_eq!(x("ask Jan about it"), "ask Jan about it");
        assert_eq!(x("Mar was here"), "Mar was here");
    }

    use super::*;

    #[test]
    fn currency_spoken_as_dollars_after_the_number() {
        use super::normalize_currency as c;
        // The live failure: a menu read-back — "$9" voiced wrong.
        assert_eq!(
            c("fish & chips for $18, steak for $32, and rum pudding for $9."),
            "fish & chips for 18 dollars, steak for 32 dollars, and rum pudding for 9 dollars."
        );
        assert_eq!(c("$1 coin"), "1 dollar coin");
        assert_eq!(c("$1,200 deposit"), "1200 dollars deposit");
        assert_eq!(
            c("that's $18.50 all up"),
            "that's 18 dollars and 50 cents all up"
        );
        assert_eq!(c("$9.5 special"), "9 dollars and 50 cents special");
        assert_eq!(c("just $0.50"), "just 50 cents");
        assert_eq!(c("$18.00 even"), "18 dollars even");
        assert_eq!(c("$1.01 exactly"), "1 dollar and 1 cent exactly");
        // 3+ decimals is not money; a bare $ is left alone.
        assert_eq!(c("pi costs $3.14159"), "pi costs 3 dollars.14159");
        assert_eq!(c("the $ sign"), "the $ sign");
        // Through the full pipeline too.
        assert_eq!(
            normalize_speech_text("Steak is $32 at 6 PM"),
            "Steak is 32 dollars at 6 pee em"
        );
    }

    #[test]
    fn meridiems_become_user_tuned_phonetics() {
        // The reported bug chain: "10 a.m.," stuttered; "AM" read as a word;
        // "ay em" read as "eye em" — the user picked "a em".
        assert_eq!(
            normalize_speech_text("I have you booked for 10 a.m., and we'll call you."),
            "I have you booked for 10 a em, and we'll call you."
        );
        assert_eq!(
            normalize_speech_text("See you at 2 p.m."),
            "See you at 2 pee em."
        );
        assert_eq!(
            normalize_speech_text("See you at 2 P.M.!"),
            "See you at 2 pee em!"
        );
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
        assert_eq!(
            normalize_speech_text("see you at 10 AM."),
            "see you at 10 a em."
        );
        assert_eq!(
            normalize_speech_text("booked for 10am sharp"),
            "booked for 10 a em sharp"
        );
        assert_eq!(
            normalize_speech_text("at 10:30 PM tonight"),
            "at ten thirty pee em tonight"
        );
        // NOT times — never rewritten.
        assert_eq!(normalize_speech_text("I AM HERE"), "I AM HERE");
        assert_eq!(normalize_speech_text("am I early?"), "am I early?");
        assert_eq!(
            normalize_speech_text("the PM will visit"),
            "the PM will visit"
        );
    }

    #[test]
    fn clock_times_read_naturally() {
        // The reported bug: "10:00" spoken as "ten zero zero" / colon noise.
        assert_eq!(
            normalize_speech_text("booked for 10:00 AM."),
            "booked for ten a em."
        );
        assert_eq!(
            normalize_speech_text("see you at 10:15."),
            "see you at ten fifteen."
        );
        assert_eq!(normalize_speech_text("at 10:05 pm"), "at ten oh five pee em");
        assert_eq!(
            normalize_speech_text("open 9:00 to 17:30"),
            "open nine to five thirty pee em"
        );
        // NOT clock times — untouched.
        assert_eq!(normalize_speech_text("a 3:1 ratio"), "a 3:1 ratio");
        assert_eq!(
            normalize_speech_text("code 10:154 please"),
            "code 10:154 please"
        );
        assert_eq!(
            normalize_speech_text("item 100:30 stays"),
            "item 100:30 stays"
        );
        assert_eq!(
            normalize_speech_text("at 75:00 minutes?"),
            "at 75:00 minutes?"
        );
    }

    #[test]
    fn clock_times_cover_midnight_noon_seconds_ranges_and_boundaries() {
        for (raw, expected) in [
            ("00:00", "midnight"), ("00:05", "twelve oh five a em"),
            ("12:00 AM", "midnight"), ("12:00 PM", "noon"),
            ("12:05 AM", "twelve oh five a em"),
            ("09:05 a.m.", "nine oh five a em."),
            ("23:45", "eleven forty-five pee em"),
            ("14:30:00", "two thirty pee em"),
            ("14:30:05", "two thirty and five seconds pee em"),
            ("9:00–17:30", "nine–five thirty pee em"),
            ("10:30AM", "ten thirty a em"),
        ] {
            let actual = normalize_speech_text(raw);
            assert_eq!(actual, expected, "{raw}");
            assert_eq!(normalize_speech_text(&actual), actual, "must be safe through two speech layers");
        }
        for raw in ["24:00", "09:60", "10:30:99", "10:30:001", "10:30:5",
            "abc10:30", "2026-09-14T10:30:00Z", "https://host:10/path"] {
            assert_eq!(normalize_speech_text(raw), raw, "{raw}");
        }
    }

    #[test]
    fn words_survive_and_markdown_is_stripped() {
        assert_eq!(normalize_speech_text("Sam. said hi"), "Sam. said hi");
        assert_eq!(
            normalize_speech_text("the spam. filter"),
            "the spam. filter"
        );
        assert_eq!(
            normalize_speech_text("**Great** — see you `then`"),
            "Great — see you then"
        );
    }
}
