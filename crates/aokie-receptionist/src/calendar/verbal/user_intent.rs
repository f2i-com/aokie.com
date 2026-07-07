//! Fill a `BookingDraft` from a single user utterance. Runs *before*
//! pass-1 generation so the slim system prompt the bot sees reflects
//! the latest known state.
//!
//! All extractors are best-effort. None should panic, none should
//! falsely fill a field — when in doubt the field stays `None` and
//! the bot will follow up with a question. False positives here
//! cause far worse customer experiences than false negatives.

use chrono::{NaiveDate, NaiveTime};

use super::{dates, services};
use crate::calendar::session::{BookingDraft, OfferedSlots};
use crate::calendar::Service;

/// Update `draft` in place from an inbound user utterance. Returns a
/// summary of what changed (used for logging + recompute_mode
/// triggers). The draft is created lazily — if it was `None` and we
/// extract anything, the caller should `Some` it (caller pattern
/// keeps `BookingDraft` allocation off the path until needed).
pub fn apply_user_utterance(
    text: &str,
    draft: &mut BookingDraft,
    offered: Option<&OfferedSlots>,
    services_list: &[Service],
    today: NaiveDate,
) -> UserUpdate {
    let mut update = UserUpdate::default();
    let lower = text.to_lowercase();

    // Reset patterns must short-circuit so we don't fill new fields
    // on a "never mind" turn.
    if is_reset_phrase(&lower) {
        update.reset_requested = true;
        return update;
    }

    if draft.service.is_none() {
        if let Some(svc) = services::match_service(services_list, text) {
            draft.service = Some(svc.name.clone());
            update.service_set = true;
        }
    }

    if draft.date.is_none() {
        if let Some(date) = dates::parse_relative_date(text, today) {
            draft.date = Some(date);
            update.date_set = true;
        }
    }

    // Time picking — only attempts to resolve against offered slots.
    // We don't accept a bare time ("2pm") without an offer because
    // the bot should be checking availability first; honoring "2pm"
    // before a check would skip the slot validation.
    if draft.start_time.is_none() {
        if let Some(offered) = offered {
            if let Some(time) = pick_offered_slot(&lower, offered) {
                draft.start_time = Some(time);
                update.time_set = true;
            }
        }
    }

    if draft.customer_name.is_none() {
        if let Some(name) = extract_customer_name(text) {
            draft.customer_name = Some(name);
            update.name_set = true;
        }
    }

    if draft.address.is_none() {
        if let Some(addr) = extract_street_address(text) {
            draft.address = Some(addr);
            update.address_set = true;
        }
    }

    update
}

/// Summary of what `apply_user_utterance` changed. The bluetooth
/// event loop logs this so we can audit detector recall after the
/// fact.
#[derive(Debug, Clone, Copy, Default)]
pub struct UserUpdate {
    pub service_set: bool,
    pub date_set: bool,
    pub time_set: bool,
    pub name_set: bool,
    pub address_set: bool,
    /// Caller asked us to forget the booking — caller is responsible
    /// for invoking `CallSession::reset_booking`.
    pub reset_requested: bool,
}

impl UserUpdate {
    pub fn any_change(&self) -> bool {
        self.service_set
            || self.date_set
            || self.time_set
            || self.name_set
            || self.address_set
            || self.reset_requested
    }
}

fn is_reset_phrase(lower: &str) -> bool {
    [
        "never mind",
        "nevermind",
        "scratch that",
        "forget it",
        "forget that",
        "actually no",
        "no don't",
        "i'll call back",
        "call back later",
        "leave it",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// Look up which offered slot (if any) the caller picked.
///
/// Order of resolution:
///  1. Explicit time match — "2pm", "2 pm", "2 o'clock", "14:00".
///     The time string is parsed and matched against the offered
///     slots; if it matches one, that's the pick.
///  2. Ordinal — "the first one", "the second option", "the third".
///  3. Plain "yes" / "sure" / "that works" — only when there is
///     exactly ONE offered slot, since otherwise it's ambiguous.
fn pick_offered_slot(lower: &str, offered: &OfferedSlots) -> Option<NaiveTime> {
    if offered.slots.is_empty() {
        return None;
    }

    if let Some(time) = parse_clock_phrase(lower) {
        if offered.slots.contains(&time) {
            return Some(time);
        }
    }

    if let Some(idx) = parse_ordinal(lower) {
        return offered.slots.get(idx).copied();
    }

    if offered.slots.len() == 1 && is_affirmative(lower) {
        return Some(offered.slots[0]);
    }

    None
}

fn parse_clock_phrase(lower: &str) -> Option<NaiveTime> {
    // 24h "14:00" / "9:30"
    if let Some(t) = parse_24h_clock(lower) {
        return Some(t);
    }
    // 12h "2pm", "2 pm", "2:30pm", "2 p.m.", "2 o'clock"
    parse_12h_clock(lower)
}

fn parse_24h_clock(lower: &str) -> Option<NaiveTime> {
    let bytes = lower.as_bytes();
    for i in 0..bytes.len() {
        if !bytes[i].is_ascii_digit() {
            continue;
        }
        // word boundary on the left
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            continue;
        }
        let hour: u32 = lower[i..j].parse().ok()?;
        if hour > 23 {
            continue;
        }
        let mut k = j + 1;
        while k < bytes.len() && bytes[k].is_ascii_digit() {
            k += 1;
        }
        let minute: u32 = lower[j + 1..k].parse().ok()?;
        if minute > 59 {
            continue;
        }
        return NaiveTime::from_hms_opt(hour, minute, 0);
    }
    None
}

fn parse_12h_clock(lower: &str) -> Option<NaiveTime> {
    // Walk for digit runs followed (after optional separator + minutes)
    // by "am"/"pm"/"a.m."/"p.m."/"o'clock".
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        let hour: u32 = match lower[i..j].parse() {
            Ok(h) => h,
            _ => {
                i = j;
                continue;
            }
        };
        if !(1..=12).contains(&hour) {
            i = j;
            continue;
        }
        let mut k = j;
        let mut minute: u32 = 0;
        if k < bytes.len() && bytes[k] == b':' {
            k += 1;
            let m_start = k;
            while k < bytes.len() && bytes[k].is_ascii_digit() {
                k += 1;
            }
            if let Ok(m) = lower[m_start..k].parse::<u32>() {
                if m < 60 {
                    minute = m;
                }
            }
        }
        // Optional whitespace before am/pm/o'clock
        let tail_start = k;
        let tail = lower[tail_start..].trim_start();
        let pm = tail.starts_with("pm")
            || tail.starts_with("p.m.")
            || tail.starts_with("p m")
            || tail.starts_with("p. m.");
        let am = tail.starts_with("am")
            || tail.starts_with("a.m.")
            || tail.starts_with("a m")
            || tail.starts_with("a. m.");
        let oclock = tail.starts_with("o'clock")
            || tail.starts_with("oclock")
            || tail.starts_with("o clock");
        if !pm && !am && !oclock {
            i = k;
            continue;
        }
        let h24 = if pm {
            if hour == 12 {
                12
            } else {
                hour + 12
            }
        } else if am {
            if hour == 12 {
                0
            } else {
                hour
            }
        } else {
            // "o'clock" without am/pm — assume 12-hour daytime sense
            // (1 o'clock = 13:00 if it'd otherwise be in the past
            // relative to typical business hours, else 1 o'clock as
            // 01:00 makes no business sense — go with 13:00).
            // For the slot-picker this is fine: we'll compare against
            // the offered list; a wrong AM/PM just won't match.
            if hour < 8 {
                hour + 12
            } else {
                hour
            }
        };
        return NaiveTime::from_hms_opt(h24, minute, 0);
    }
    None
}

fn parse_ordinal(lower: &str) -> Option<usize> {
    let patterns = [
        ("first", 0usize),
        ("1st", 0),
        ("number one", 0),
        ("option one", 0),
        ("first one", 0),
        ("first option", 0),
        ("second", 1),
        ("2nd", 1),
        ("number two", 1),
        ("option two", 1),
        ("second one", 1),
        ("second option", 1),
        ("third", 2),
        ("3rd", 2),
        ("number three", 2),
        ("third one", 2),
        ("third option", 2),
        ("fourth", 3),
        ("4th", 3),
    ];
    let mut earliest: Option<(usize, usize)> = None;
    for (phrase, idx) in patterns {
        if let Some(at) = lower.find(phrase) {
            match earliest {
                None => earliest = Some((at, idx)),
                Some((cur, _)) if at < cur => earliest = Some((at, idx)),
                _ => {}
            }
        }
    }
    earliest.map(|(_, idx)| idx)
}

fn is_affirmative(lower: &str) -> bool {
    let t = lower.trim();
    [
        "yes",
        "yeah",
        "yep",
        "yup",
        "sure",
        "ok",
        "okay",
        "sounds good",
        "that works",
        "works for me",
        "perfect",
        "please",
        "yes please",
    ]
    .iter()
    .any(|p| t == *p || t.contains(p))
}

/// Extract a customer name from common self-introduction phrasings.
/// Returns the extracted name in title-case form (how the operator
/// will see it in the calendar UI).
fn extract_customer_name(text: &str) -> Option<String> {
    // Patterns are ordered by specificity — "my name is Lance" wins
    // over "I'm Lance" if both could match.
    let lower = text.to_lowercase();
    let candidates = [
        "my name is ",
        "my name's ",
        "this is ",
        "i'm ",
        "i am ",
        "it's ",
        "name's ",
        "name is ",
        "call me ",
    ];
    for marker in candidates {
        if let Some(idx) = lower.find(marker) {
            let after = idx + marker.len();
            let tail = &text[after..];
            // Take up to ~4 words, stopping at sentence punctuation
            // or a stop-token ("here", "good", "fine" etc.).
            if let Some(name) = take_name(tail) {
                return Some(name);
            }
        }
    }
    None
}

/// Walk the start of `tail` and pull a 1-4 word name out, stopping
/// at punctuation, common stop words, or numeric runs (a phone number
/// or street number must NOT slip through here).
fn take_name(tail: &str) -> Option<String> {
    let tail = tail.trim_start();
    if tail.is_empty() {
        return None;
    }
    let mut words: Vec<&str> = Vec::new();
    let mut chars = tail.char_indices();
    let mut word_start: Option<usize> = None;
    while let Some((i, c)) = chars.next() {
        if c.is_alphabetic() || c == '\'' || c == '-' {
            if word_start.is_none() {
                word_start = Some(i);
            }
        } else if c.is_whitespace() || c == ',' || c == '.' || c == '!' || c == '?' {
            if let Some(start) = word_start.take() {
                let w = &tail[start..i];
                if is_name_stop_word(w) {
                    break;
                }
                words.push(w);
                if words.len() >= 4 {
                    break;
                }
            }
            // Bail on hard punctuation after grabbing the word.
            if c == ',' || c == '.' || c == '!' || c == '?' {
                break;
            }
        } else {
            // Anything else (digit, punctuation) terminates.
            if let Some(start) = word_start.take() {
                let w = &tail[start..i];
                if is_name_stop_word(w) {
                    break;
                }
                words.push(w);
            }
            break;
        }
    }
    if let Some(start) = word_start {
        let w = &tail[start..];
        if !is_name_stop_word(w) {
            words.push(w);
        }
    }

    if words.is_empty() {
        return None;
    }
    // Trim trailing connectives — "Lance and that's it" → "Lance".
    while words.last().map(|w| is_name_stop_word(w)).unwrap_or(false) {
        words.pop();
    }
    if words.is_empty() {
        return None;
    }
    let name = words
        .iter()
        .map(|w| title_case_word(w))
        .collect::<Vec<_>>()
        .join(" ");
    Some(name)
}

fn is_name_stop_word(w: &str) -> bool {
    let lc = w.to_lowercase();
    matches!(
        lc.as_str(),
        "here"
            | "there"
            | "good"
            | "fine"
            | "ok"
            | "okay"
            | "sorry"
            | "yes"
            | "no"
            | "yeah"
            | "the"
            | "a"
            | "an"
            | "and"
            | "but"
            | "so"
            | "well"
            | "it"
            | "is"
            | "calling"
            | "speaking"
            | "from"
            | "of"
            | "actually"
            | "just"
            | "you"
            | "your"
            // Prepositions that typically introduce an address /
            // location after a name. "I'm Lance at 12 Park Street" →
            // stop at "at" so the address parser gets the rest.
            | "at"
            | "on"
            | "in"
            | "near"
            | "by"
    )
}

fn title_case_word(w: &str) -> String {
    let mut out = String::with_capacity(w.len());
    let mut first = true;
    for c in w.chars() {
        if first {
            out.extend(c.to_uppercase());
            first = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Extract a street address. Must look like "<digits> <one or more
/// words> <street type>" — protects against "12 Park" (no street
/// type) and "Park Street" (no number) which are ambiguous.
fn extract_street_address(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // Word boundary on the left.
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let num_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // Optional unit suffix like "12a"
        if i < bytes.len()
            && bytes[i].is_ascii_alphabetic()
            && (i + 1 == bytes.len() || !bytes[i + 1].is_ascii_alphabetic())
        {
            i += 1;
        }
        let after_num = i;
        // Need a space.
        if after_num >= bytes.len() || !bytes[after_num].is_ascii_whitespace() {
            continue;
        }
        // Find the next street-type word within ~6 words.
        let tail = &text[after_num..];
        if let Some(end_offset) = find_street_type_end(tail) {
            let raw = &text[num_start..after_num + end_offset];
            return Some(title_case_address(raw));
        }
    }
    None
}

const STREET_TYPES: &[&str] = &[
    "street",
    "st",
    "road",
    "rd",
    "avenue",
    "ave",
    "lane",
    "ln",
    "drive",
    "dr",
    "court",
    "ct",
    "place",
    "pl",
    "way",
    "boulevard",
    "blvd",
    "highway",
    "hwy",
    "terrace",
    "tce",
    "circuit",
    "cct",
    "crescent",
    "cres",
    "parade",
    "pde",
    "close",
    "cl",
];

fn find_street_type_end(tail: &str) -> Option<usize> {
    let lower = tail.to_lowercase();
    let mut word_count = 0usize;
    let mut i = 0usize;
    while i < lower.len() {
        // Skip whitespace
        while i < lower.len() && lower.as_bytes()[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= lower.len() {
            break;
        }
        // Read a word
        let word_start = i;
        while i < lower.len()
            && !lower.as_bytes()[i].is_ascii_whitespace()
            && lower.as_bytes()[i] != b','
            && lower.as_bytes()[i] != b'.'
        {
            i += 1;
        }
        let word = &lower[word_start..i];
        // Strip a trailing apostrophe-s or punctuation that may have leaked.
        let word_clean = word.trim_end_matches(|c: char| !c.is_ascii_alphabetic());
        if STREET_TYPES.iter().any(|t| *t == word_clean) {
            return Some(i);
        }
        word_count += 1;
        if word_count > 6 {
            return None;
        }
        // Stop if we hit a hard punctuation.
        if i < lower.len() && (lower.as_bytes()[i] == b',' || lower.as_bytes()[i] == b'.') {
            return None;
        }
    }
    None
}

fn title_case_address(raw: &str) -> String {
    raw.split_whitespace()
        .map(|w| {
            // Keep digits as-is, title-case alpha.
            if w.chars().all(|c| c.is_ascii_digit() || c.is_alphabetic())
                && w.chars().any(|c| c.is_alphabetic())
            {
                title_case_word(w)
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::session::OfferedSlots;

    fn svc(name: &str, requires_address: bool) -> Service {
        Service {
            id: 1,
            name: name.into(),
            duration_minutes: 30,
            description: None,
            active: true,
            sort_order: 0,
            created_at: 0,
            requires_address,
        }
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn extracts_name_from_im_phrase() {
        assert_eq!(
            extract_customer_name("I'm Lance Smith."),
            Some("Lance Smith".into())
        );
        assert_eq!(
            extract_customer_name("My name is Lance"),
            Some("Lance".into())
        );
        assert_eq!(
            extract_customer_name("Hi this is Lance Smith here"),
            Some("Lance Smith".into())
        );
        assert_eq!(extract_customer_name("Call me Lance"), Some("Lance".into()));
    }

    #[test]
    fn ignores_im_phrases_without_a_real_name() {
        // "I'm here" shouldn't extract "here" as the name.
        assert_eq!(extract_customer_name("I'm here for the appointment"), None);
        assert_eq!(extract_customer_name("I'm calling about my booking"), None);
    }

    #[test]
    fn extracts_address_from_natural_phrasing() {
        assert_eq!(
            extract_street_address("I'm at 12 Park Street"),
            Some("12 Park Street".into())
        );
        assert_eq!(
            extract_street_address("the address is 245 high road"),
            Some("245 High Road".into())
        );
        assert_eq!(
            extract_street_address("12a Park St"),
            Some("12a Park St".into())
        );
    }

    #[test]
    fn rejects_ambiguous_addresses() {
        // Number alone — could be a phone digit, a quantity, etc.
        assert_eq!(extract_street_address("there are 12 of us"), None);
        // No street number.
        assert_eq!(extract_street_address("Park Street"), None);
    }

    #[test]
    fn picks_offered_slot_by_explicit_time() {
        let offered = OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![t(9, 0), t(14, 0), t(16, 0)],
        };
        assert_eq!(
            pick_offered_slot("yes the 2pm works", &offered),
            Some(t(14, 0))
        );
        assert_eq!(pick_offered_slot("9am please", &offered), Some(t(9, 0)));
        assert_eq!(
            pick_offered_slot("4pm sounds good", &offered),
            Some(t(16, 0))
        );
    }

    #[test]
    fn picks_offered_slot_by_ordinal() {
        let offered = OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![t(9, 0), t(14, 0), t(16, 0)],
        };
        assert_eq!(pick_offered_slot("the first one", &offered), Some(t(9, 0)));
        assert_eq!(
            pick_offered_slot("the second option works", &offered),
            Some(t(14, 0))
        );
        assert_eq!(
            pick_offered_slot("third one please", &offered),
            Some(t(16, 0))
        );
    }

    #[test]
    fn affirmative_picks_only_when_single_slot() {
        let single = OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![t(14, 0)],
        };
        assert_eq!(pick_offered_slot("yes please", &single), Some(t(14, 0)));

        let multi = OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![t(9, 0), t(14, 0)],
        };
        assert_eq!(pick_offered_slot("yes please", &multi), None);
    }

    #[test]
    fn explicit_time_not_in_offers_returns_none() {
        let offered = OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![t(9, 0), t(14, 0)],
        };
        // Caller asked for 11am but it wasn't on offer.
        assert_eq!(pick_offered_slot("11am please", &offered), None);
    }

    #[test]
    fn apply_user_utterance_fills_service_and_date() {
        let services = vec![svc("Lawn mow", true)];
        let mut draft = BookingDraft::default();
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let u = apply_user_utterance(
            "I'd like a lawn mow for next Friday please",
            &mut draft,
            None,
            &services,
            today,
        );
        assert!(u.service_set);
        assert!(u.date_set);
        assert_eq!(draft.service.as_deref(), Some("Lawn mow"));
        assert_eq!(
            draft.date,
            Some(NaiveDate::from_ymd_opt(2026, 5, 8).unwrap())
        );
    }

    #[test]
    fn apply_user_utterance_fills_name_and_address_in_one_turn() {
        let services = vec![svc("Lawn mow", true)];
        let mut draft = BookingDraft::default();
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let u = apply_user_utterance(
            "I'm Lance Smith at 12 Park Street",
            &mut draft,
            None,
            &services,
            today,
        );
        assert!(u.name_set);
        assert!(u.address_set);
        assert_eq!(draft.customer_name.as_deref(), Some("Lance Smith"));
        assert_eq!(draft.address.as_deref(), Some("12 Park Street"));
    }

    #[test]
    fn reset_phrase_short_circuits() {
        let services = vec![svc("Lawn mow", true)];
        let mut draft = BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 8).unwrap()),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let u = apply_user_utterance(
            "actually never mind I'll call back",
            &mut draft,
            None,
            &services,
            today,
        );
        assert!(u.reset_requested);
        assert!(!u.service_set);
        assert!(!u.date_set);
    }

    #[test]
    fn does_not_overwrite_already_set_fields() {
        let services = vec![svc("Lawn mow", true), svc("Hedge trim", false)];
        let mut draft = BookingDraft {
            service: Some("Lawn mow".into()),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let _ = apply_user_utterance(
            "I want hedge trim instead actually",
            &mut draft,
            None,
            &services,
            today,
        );
        // Service already set — we keep it. Caller is expected to
        // explicitly reset to switch services. Avoids accidental
        // re-fill from chatter.
        assert_eq!(draft.service.as_deref(), Some("Lawn mow"));
    }
}
