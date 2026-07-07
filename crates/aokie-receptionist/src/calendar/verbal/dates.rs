//! Resolve relative date phrases ("tomorrow", "next Friday", "May 8th",
//! "the 12th") into a `NaiveDate` against a fixed `today` anchor.
//!
//! Semantics:
//!  - "today" / "tomorrow" / "yesterday" — obvious.
//!  - bare weekday ("Friday", "this Friday") — the next occurrence,
//!    including today; on a Friday, "Friday" means today.
//!  - "next <weekday>" — the next occurrence EXCLUDING today; on a
//!    Friday, "next Friday" means today + 7.
//!  - month + day ("May 8", "8 May", "May 8th") — current year if the
//!    date is today or in the future, next year otherwise.
//!  - "the Nth" — current month if N is today's day-of-month or later,
//!    next month otherwise.
//!  - "in N days" / "in a week" — N days from today.
//!
//! Anything ambiguous returns `None` — the bot then falls back to
//! asking the caller. We'd rather miss a date than fire a tool call
//! against the wrong day.

use chrono::{Datelike, Duration as ChronoDuration, NaiveDate, Weekday};

/// Parse a relative date phrase. The phrase can be embedded inside a
/// longer string ("I'd like to book for next Friday afternoon") — we
/// scan for the first recognised pattern. `today` should be the
/// caller's local date (use `today_in_config_tz`).
pub fn parse_relative_date(phrase: &str, today: NaiveDate) -> Option<NaiveDate> {
    let lower = phrase.to_lowercase();
    let lower = lower.as_str();

    // Exact one-word phrases first — cheapest, and they win over
    // weekday names that could otherwise fire on "today is Friday".
    // "day after tomorrow" must be checked BEFORE "tomorrow" because
    // it's a strict superset — bare "tomorrow" would otherwise match
    // first and resolve to today+1.
    if has_phrase(lower, "day after tomorrow") {
        return Some(today + ChronoDuration::days(2));
    }
    if has_word(lower, "today") || has_word(lower, "tonight") {
        return Some(today);
    }
    if has_word(lower, "tomorrow") {
        return Some(today + ChronoDuration::days(1));
    }
    if has_word(lower, "yesterday") {
        return Some(today - ChronoDuration::days(1));
    }

    // "in N days" / "in a week"
    if let Some(d) = parse_in_n_days(lower) {
        return Some(today + ChronoDuration::days(d));
    }
    if has_phrase(lower, "in a week")
        || has_phrase(lower, "in one week")
        || has_phrase(lower, "next week")
    {
        return Some(today + ChronoDuration::days(7));
    }

    // "next <weekday>" — must be checked before bare weekday so the
    // "next" prefix is honoured.
    if let Some(target) = find_weekday_after_marker(lower, "next") {
        return Some(next_weekday_strict_after(today, target));
    }
    // "this <weekday>" — same as bare weekday, included so the
    // "this" prefix doesn't fall through into pattern soup.
    if let Some(target) = find_weekday_after_marker(lower, "this") {
        return Some(next_weekday_inclusive(today, target));
    }

    // "May 8", "May 8th", "8 May" — month + day in either order, with
    // optional ordinal suffix.
    if let Some(d) = parse_month_day(lower, today) {
        return Some(d);
    }

    // "the 12th" / "the 12" / "12th of the month"
    if let Some(d) = parse_day_of_month(lower, today) {
        return Some(d);
    }

    // Bare weekday name. Last because anything more specific should
    // have matched already; e.g. "next Friday" reaches here only if
    // `find_weekday_after_marker` somehow missed it (won't happen).
    if let Some(target) = find_first_weekday(lower) {
        return Some(next_weekday_inclusive(today, target));
    }

    None
}

/// True iff `needle` appears as a whitespace-bounded word in `hay`.
/// Avoids "nottoday" matching "today". `hay` is expected lowercased.
fn has_word(hay: &str, needle: &str) -> bool {
    for (idx, _) in hay.match_indices(needle) {
        let before = idx == 0 || !hay.as_bytes()[idx - 1].is_ascii_alphanumeric();
        let after_idx = idx + needle.len();
        let after = after_idx >= hay.len() || !hay.as_bytes()[after_idx].is_ascii_alphanumeric();
        if before && after {
            return true;
        }
    }
    false
}

/// Substring match — for multi-word phrases where word-boundary
/// checks are awkward and unnecessary.
fn has_phrase(hay: &str, needle: &str) -> bool {
    hay.contains(needle)
}

fn weekday_from_str(s: &str) -> Option<Weekday> {
    match s {
        "monday" | "mon" => Some(Weekday::Mon),
        "tuesday" | "tues" | "tue" => Some(Weekday::Tue),
        "wednesday" | "weds" | "wed" => Some(Weekday::Wed),
        "thursday" | "thurs" | "thu" => Some(Weekday::Thu),
        "friday" | "fri" => Some(Weekday::Fri),
        "saturday" | "sat" => Some(Weekday::Sat),
        "sunday" | "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

const WEEKDAY_TOKENS: &[&str] = &[
    "monday",
    "mon",
    "tuesday",
    "tues",
    "tue",
    "wednesday",
    "weds",
    "wed",
    "thursday",
    "thurs",
    "thu",
    "friday",
    "fri",
    "saturday",
    "sat",
    "sunday",
    "sun",
];

/// Find the first weekday token in the lowercased haystack.
fn find_first_weekday(hay: &str) -> Option<Weekday> {
    let mut earliest: Option<(usize, Weekday)> = None;
    for tok in WEEKDAY_TOKENS {
        if let Some(idx) = find_word(hay, tok) {
            let wd = weekday_from_str(tok)?;
            match earliest {
                None => earliest = Some((idx, wd)),
                Some((cur_idx, _)) if idx < cur_idx => earliest = Some((idx, wd)),
                _ => {}
            }
        }
    }
    earliest.map(|(_, wd)| wd)
}

/// Find a weekday token that is preceded (with at least one whitespace
/// in between) by a marker word like "next" or "this". Avoids picking
/// up a marker like "nextFridayisfine".
fn find_weekday_after_marker(hay: &str, marker: &str) -> Option<Weekday> {
    let marker_idx = find_word(hay, marker)?;
    let after = marker_idx + marker.len();
    if after >= hay.len() {
        return None;
    }
    let tail = &hay[after..];
    // Allow a single space (or several) between marker and weekday.
    let tail = tail.trim_start();
    for tok in WEEKDAY_TOKENS {
        if tail.starts_with(tok) {
            // Confirm word boundary.
            let next_byte = tail.as_bytes().get(tok.len()).copied();
            if next_byte
                .map(|b| !b.is_ascii_alphanumeric())
                .unwrap_or(true)
            {
                return weekday_from_str(tok);
            }
        }
    }
    None
}

fn find_word(hay: &str, needle: &str) -> Option<usize> {
    for (idx, _) in hay.match_indices(needle) {
        let before = idx == 0 || !hay.as_bytes()[idx - 1].is_ascii_alphanumeric();
        let after_idx = idx + needle.len();
        let after = after_idx >= hay.len() || !hay.as_bytes()[after_idx].is_ascii_alphanumeric();
        if before && after {
            return Some(idx);
        }
    }
    None
}

/// "Friday" on a Friday → today; "Friday" on a Tuesday → +3 days.
fn next_weekday_inclusive(today: NaiveDate, target: Weekday) -> NaiveDate {
    let cur = today.weekday().num_days_from_monday() as i64;
    let want = target.num_days_from_monday() as i64;
    let delta = ((want - cur) % 7 + 7) % 7;
    today + ChronoDuration::days(delta)
}

/// "next Friday" on a Friday → +7 days; on a Tuesday → +3 days.
fn next_weekday_strict_after(today: NaiveDate, target: Weekday) -> NaiveDate {
    let cur = today.weekday().num_days_from_monday() as i64;
    let want = target.num_days_from_monday() as i64;
    let delta = ((want - cur) % 7 + 7) % 7;
    let delta = if delta == 0 { 7 } else { delta };
    today + ChronoDuration::days(delta)
}

const MONTH_NAMES: &[(&str, u32)] = &[
    ("january", 1),
    ("jan", 1),
    ("february", 2),
    ("feb", 2),
    ("march", 3),
    ("mar", 3),
    ("april", 4),
    ("apr", 4),
    ("may", 5),
    ("june", 6),
    ("jun", 6),
    ("july", 7),
    ("jul", 7),
    ("august", 8),
    ("aug", 8),
    ("september", 9),
    ("sept", 9),
    ("sep", 9),
    ("october", 10),
    ("oct", 10),
    ("november", 11),
    ("nov", 11),
    ("december", 12),
    ("dec", 12),
];

/// Try "May 8", "May 8th", "8 May", "8th of May". Returns a date in
/// the same year as `today` if the date is today or in the future,
/// otherwise next year.
fn parse_month_day(hay: &str, today: NaiveDate) -> Option<NaiveDate> {
    let (month_idx, month_num, month_len) = MONTH_NAMES
        .iter()
        .filter_map(|(name, num)| find_word(hay, name).map(|idx| (idx, *num, name.len())))
        .min_by_key(|&(idx, _, _)| idx)?;

    // Look for a day number within ~12 chars on either side of the
    // month token. That window covers "May 8th", "May the 8th",
    // "the 8th of May", "8 May" etc. without dragging in distant
    // numbers from elsewhere in the sentence.
    let window_start = month_idx.saturating_sub(12);
    let window_end = (month_idx + month_len + 12).min(hay.len());
    let window = &hay[window_start..window_end];
    let day = first_day_number_in(window)?;
    NaiveDate::from_ymd_opt(today.year(), month_num, day).map(|d| {
        if d < today {
            NaiveDate::from_ymd_opt(today.year() + 1, month_num, day).unwrap_or(d)
        } else {
            d
        }
    })
}

/// "the 12th", "the 8" — day of month. Resolves to current month if
/// the day is >= today's day-of-month, else next month.
fn parse_day_of_month(hay: &str, today: NaiveDate) -> Option<NaiveDate> {
    // Require either "the <N>" or "<N>th of the month" phrasing —
    // bare numbers like "I want 2 of those" must not match.
    let after_the = find_word(hay, "the").map(|i| i + "the".len())?;
    let tail = hay[after_the..].trim_start();
    let day = first_day_number_in(tail)?;
    let in_current_month = NaiveDate::from_ymd_opt(today.year(), today.month(), day)?;
    if in_current_month >= today {
        Some(in_current_month)
    } else {
        let (year, month) = if today.month() == 12 {
            (today.year() + 1, 1)
        } else {
            (today.year(), today.month() + 1)
        };
        NaiveDate::from_ymd_opt(year, month, day)
    }
}

/// "in 3 days" → 3.
fn parse_in_n_days(hay: &str) -> Option<i64> {
    let idx = find_word(hay, "in")?;
    let tail = hay[idx + 2..].trim_start();
    let n = first_number_in(tail)?;
    // Confirm the next non-digit word is "day" or "days".
    let tail_after_num = tail.trim_start_matches(|c: char| c.is_ascii_digit() || c.is_whitespace());
    if tail_after_num.starts_with("day") {
        Some(n as i64)
    } else {
        None
    }
}

/// First number 1–31 found in `s`, optionally with an ordinal suffix
/// ("8th"). Larger numbers (years, phone digits) are rejected so we
/// don't pull "8" out of "12 Park Street" thinking it's a day.
fn first_day_number_in(s: &str) -> Option<u32> {
    let mut chars = s.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_ascii_digit() {
            let mut end = start;
            for (i, ch) in s[start..].char_indices() {
                if ch.is_ascii_digit() {
                    end = start + i + ch.len_utf8();
                } else {
                    break;
                }
            }
            let num: u32 = s[start..end].parse().ok()?;
            if (1..=31).contains(&num) {
                return Some(num);
            }
            return None;
        }
        chars.next();
    }
    None
}

fn first_number_in(s: &str) -> Option<u32> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let end = s[start..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| start + i)
        .unwrap_or(s.len());
    s[start..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn today_tomorrow_yesterday() {
        let today = d(2026, 5, 6);
        assert_eq!(parse_relative_date("today", today), Some(d(2026, 5, 6)));
        assert_eq!(
            parse_relative_date("can you fit me in today?", today),
            Some(d(2026, 5, 6))
        );
        assert_eq!(parse_relative_date("tomorrow", today), Some(d(2026, 5, 7)));
        assert_eq!(parse_relative_date("yesterday", today), Some(d(2026, 5, 5)));
        assert_eq!(
            parse_relative_date("day after tomorrow", today),
            Some(d(2026, 5, 8))
        );
    }

    #[test]
    fn weekday_inclusive_for_bare_name() {
        let wed = d(2026, 5, 6); // Wednesday
        assert_eq!(parse_relative_date("Friday", wed), Some(d(2026, 5, 8)));
        assert_eq!(parse_relative_date("Wednesday", wed), Some(d(2026, 5, 6)));
        assert_eq!(parse_relative_date("Sunday", wed), Some(d(2026, 5, 10)));
        // Mid-sentence
        assert_eq!(
            parse_relative_date("I'd like Friday please", wed),
            Some(d(2026, 5, 8))
        );
    }

    #[test]
    fn next_friday_on_wednesday_is_two_days_away() {
        let wed = d(2026, 5, 6);
        assert_eq!(parse_relative_date("next Friday", wed), Some(d(2026, 5, 8)));
    }

    #[test]
    fn next_friday_on_a_friday_is_a_week_later() {
        let fri = d(2026, 5, 8);
        assert_eq!(
            parse_relative_date("next Friday", fri),
            Some(d(2026, 5, 15))
        );
    }

    #[test]
    fn this_friday_thursday_is_tomorrow() {
        let thu = d(2026, 5, 7);
        assert_eq!(parse_relative_date("this Friday", thu), Some(d(2026, 5, 8)));
    }

    #[test]
    fn month_day_in_either_order() {
        let today = d(2026, 5, 1);
        assert_eq!(parse_relative_date("May 8", today), Some(d(2026, 5, 8)));
        assert_eq!(parse_relative_date("8 May", today), Some(d(2026, 5, 8)));
        assert_eq!(parse_relative_date("May 8th", today), Some(d(2026, 5, 8)));
        assert_eq!(
            parse_relative_date("the 8th of May", today),
            Some(d(2026, 5, 8))
        );
    }

    #[test]
    fn month_day_in_the_past_rolls_to_next_year() {
        let today = d(2026, 6, 15);
        // March is past for June; should roll to 2027
        assert_eq!(parse_relative_date("March 5", today), Some(d(2027, 3, 5)));
    }

    #[test]
    fn day_of_month_with_the_prefix() {
        let today = d(2026, 5, 6);
        assert_eq!(parse_relative_date("the 12th", today), Some(d(2026, 5, 12)));
        // Past day-of-month rolls to next month
        assert_eq!(parse_relative_date("the 2nd", today), Some(d(2026, 6, 2)));
    }

    #[test]
    fn in_n_days() {
        let today = d(2026, 5, 6);
        assert_eq!(parse_relative_date("in 3 days", today), Some(d(2026, 5, 9)));
        assert_eq!(
            parse_relative_date("in 10 days", today),
            Some(d(2026, 5, 16))
        );
        assert_eq!(
            parse_relative_date("in a week", today),
            Some(d(2026, 5, 13))
        );
        assert_eq!(
            parse_relative_date("next week", today),
            Some(d(2026, 5, 13))
        );
    }

    #[test]
    fn no_match_returns_none() {
        let today = d(2026, 5, 6);
        assert_eq!(parse_relative_date("just chatting", today), None);
        assert_eq!(parse_relative_date("hello there", today), None);
        // Bare numbers without "the" or month context — must not match.
        assert_eq!(parse_relative_date("12 park street", today), None);
    }

    #[test]
    fn case_insensitivity() {
        let today = d(2026, 5, 6);
        assert_eq!(parse_relative_date("FRIDAY", today), Some(d(2026, 5, 8)));
        assert_eq!(parse_relative_date("Tomorrow", today), Some(d(2026, 5, 7)));
    }

    #[test]
    fn nottoday_does_not_match_today() {
        let today = d(2026, 5, 6);
        // Defensive: word-boundary check should keep "nottoday" /
        // "todayish" out of the today branch.
        assert_eq!(parse_relative_date("nottoday", today), None);
    }
}
