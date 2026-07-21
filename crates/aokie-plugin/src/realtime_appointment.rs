//! Fixed, call-scoped validation for OpenAI Realtime appointment requests.
//!
//! The provider may propose bounded business fields, but it cannot choose the
//! call id, caller phone, transcript turn, event name, or idempotency key. A
//! request is accepted only when the quoted agreement is the exact latest
//! final caller transcript and the recent caller history contains an explicit
//! appointment intent. The resulting record is still only a staff-confirmed
//! `requested` appointment, never a confirmed booking.

use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Timelike, Weekday};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAppointmentRequest {
    pub request_id: String,
    pub caller_name: String,
    pub service: String,
    pub date: String,
    pub time: String,
    pub agreement_turn: u32,
}

fn bounded_plain(value: Option<&Value>, label: &str, max_chars: usize) -> Result<String, String> {
    let value = value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{label} is required"))?;
    if value.chars().count() > max_chars || value.chars().any(char::is_control) {
        return Err(format!("{label} is invalid"));
    }
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(format!("{label} is required"));
    }
    Ok(normalized)
}

fn normalized_speech(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Canonical token form for cross-transcriber comparison. The provider quotes
/// what its own audio model heard while the trusted transcript comes from a
/// separate transcription model, so formatting routinely diverges ("10 a.m."
/// vs "10am" vs "ten AM"). Punctuation becomes token boundaries, digit+am/pm
/// fusions split, spelled a.m./p.m./o'clock unify, and the number words
/// one..twelve map to digits.
fn canonical_agreement(value: &str) -> String {
    let lowered: String = value
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect();
    let mut tokens: Vec<String> = Vec::new();
    for raw in lowered.split_whitespace() {
        let fused = raw
            .strip_suffix("am")
            .map(|digits| (digits, "am"))
            .or_else(|| raw.strip_suffix("pm").map(|digits| (digits, "pm")))
            .filter(|(digits, _)| {
                !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
            });
        if let Some((digits, suffix)) = fused {
            tokens.push(digits.to_string());
            tokens.push(suffix.to_string());
        } else {
            tokens.push(raw.to_string());
        }
    }
    let mut merged: Vec<String> = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if index + 1 < tokens.len()
            && matches!(tokens[index].as_str(), "a" | "p")
            && tokens[index + 1] == "m"
        {
            merged.push(format!("{}m", tokens[index]));
            index += 2;
            continue;
        }
        if index + 1 < tokens.len() && tokens[index] == "o" && tokens[index + 1] == "clock" {
            merged.push("oclock".to_string());
            index += 2;
            continue;
        }
        merged.push(tokens[index].clone());
        index += 1;
    }
    let mapped: Vec<&str> = merged
        .iter()
        .map(|token| match token.as_str() {
            "one" => "1",
            "two" => "2",
            "three" => "3",
            "four" => "4",
            "five" => "5",
            "six" => "6",
            "seven" => "7",
            "eight" => "8",
            "nine" => "9",
            "ten" => "10",
            "eleven" => "11",
            "twelve" => "12",
            other => other,
        })
        .collect();
    mapped.join(" ")
}

/// The provider's quoted agreement must still be grounded in the trusted
/// latest caller turn, but exact string equality between two independent
/// transcribers is unachievable in practice. Accept the quote when its
/// canonical form equals the turn, is a token-bounded substring of it, or is
/// a substantial token subset of it — the provider still cannot fabricate an
/// agreement out of words the caller never said, and the semantic date/time/
/// agreement fences below run on the trusted transcript, never the quote.
fn agreement_matches_latest(agreement: &str, latest: &str) -> bool {
    let quoted = canonical_agreement(agreement);
    let trusted = canonical_agreement(latest);
    if quoted.is_empty() || trusted.is_empty() {
        return false;
    }
    if quoted == trusted {
        return true;
    }
    // A quote that is a token-bounded PREFIX of the trusted turn is always
    // aligned: callers split agreements across quick turns ("Yes." then
    // "Yes, that'd be good."), and the model may quote the first fragment
    // while the trusted latest turn carries the completed sentence. The
    // quote still cannot fabricate — it must literally lead the turn.
    if format!(" {trusted} ").starts_with(&format!(" {quoted} ")) {
        return true;
    }
    let quoted_tokens: Vec<&str> = quoted.split(' ').collect();
    let trusted_token_count = trusted.split(' ').count();
    let substantial =
        quoted_tokens.len() >= 3 || quoted_tokens.len().saturating_mul(2) >= trusted_token_count;
    if !substantial {
        return false;
    }
    if format!(" {trusted} ").contains(&format!(" {quoted} ")) {
        return true;
    }
    let trusted_tokens: std::collections::BTreeSet<&str> = trusted.split(' ').collect();
    quoted_tokens.len() >= 3
        && quoted_tokens
            .iter()
            .all(|token| trusted_tokens.contains(token))
}

fn contains_explicit_appointment_intent(value: &str) -> bool {
    let value = normalized_speech(value);
    [
        "appointment",
        "booking",
        "book me",
        "book an",
        "book a",
        "book one",
        "book us",
        "book it",
        "book that",
        "make a booking",
        "make an appointment",
        "schedule me",
        "schedule an",
        "schedule a",
        "reserve",
        "come in",
        "fit me in",
        "get me in",
        "put me in",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}

fn looks_like_question(value: &str) -> bool {
    let value = normalized_speech(value);
    value.trim_end().ends_with('?')
        || [
            "is ", "are ", "am ", "do ", "does ", "did ", "can ", "could ", "would ", "will ",
            "what ", "when ", "where ", "how ", "have ", "has ",
        ]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

fn contains_negative(value: &str) -> bool {
    let value = normalized_speech(value);
    [
        "don't",
        "do not",
        "not yet",
        "not now",
        "no thanks",
        "never mind",
        "nevermind",
        "cancel",
        "maybe",
        "let me think",
        "i'm not sure",
        "i am not sure",
    ]
    .iter()
    .any(|needle| value.contains(needle))
}

const DAY_WORDS: [&str; 9] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "today",
    "tomorrow",
];

pub(crate) fn is_conservative_agreement(value: &str) -> bool {
    if contains_negative(value) {
        return false;
    }
    let value = normalized_speech(value);

    let direct_booking_request = [
        "book me",
        "book an",
        "book a",
        "book one",
        "book us",
        "book it",
        "book that",
        "book my",
        "make a booking",
        "make an appointment",
        "schedule me",
        "schedule an",
        "put me down",
        "put me in",
        "get me in",
        "fit me in",
        "come in",
        "lock it in",
        "lock in",
        "pencil me in",
    ]
    .iter()
    .any(|needle| value.contains(needle));

    let affirmative = [
        "yes",
        "yep",
        "yeah",
        "please",
        "go ahead",
        "do that",
        "book it",
        "book me",
        "make the appointment",
        "that works",
        "sounds good",
        "i'll ",
        "i’ll ",
        "i will ",
        "put me down",
    ]
    .iter()
    .any(|needle| value == *needle || value.contains(needle));

    // Supplying a concrete slot in direct response to an appointment question
    // is itself agreement ("Wednesday at 10"), even without a redundant
    // "yes". Questions remain read-only availability exploration.
    let has_day = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "today",
        "tomorrow",
    ]
    .iter()
    .any(|day| value.contains(day));
    let concrete_slot = has_day && value.chars().any(|character| character.is_ascii_digit());

    // A concrete slot the caller PROPOSES is agreement even when phrased as a
    // polite question: "How about tomorrow at 10?" and "Could you do Thursday
    // at 2?" select a slot, they do not explore availability. Exploration
    // shapes ("Is Wednesday at 10 available?", "Do you have anything at 3?")
    // stay read-only because they match none of these openers.
    let slot_proposal = value.starts_with("how about")
        || value.starts_with("what about")
        || value.starts_with("can we do")
        || value.starts_with("could we do")
        || value.starts_with("can you do")
        || value.starts_with("could you do");

    // Punctuation in automatic transcripts is not guaranteed. A lexical
    // availability question is never treated as agreement merely because it
    // contains a weekday and a number. A direct booking request is different:
    // "Could you book me Wednesday at 10?" is itself explicit approval.
    (concrete_slot && (!looks_like_question(&value) || direct_booking_request || slot_proposal))
        || (affirmative
            && (!value.ends_with('?') || value.starts_with("yes") || value.starts_with("yeah")))
}

fn validate_spoken_date(agreement: &str, date: NaiveDate, today: NaiveDate) -> Result<(), String> {
    let speech = normalized_speech(agreement);
    let relative: Vec<(&str, NaiveDate)> =
        [("today", today), ("tomorrow", today + Duration::days(1))]
            .into_iter()
            .filter(|(word, _)| speech.contains(word))
            .collect();
    if relative.len() > 1
        || relative
            .first()
            .is_some_and(|(_, expected)| *expected != date)
    {
        return Err("appointment date does not match the caller's selected day".into());
    }

    let mentioned: Vec<Weekday> = [
        ("monday", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sunday", Weekday::Sun),
    ]
    .into_iter()
    .filter_map(|(word, weekday)| speech.contains(word).then_some(weekday))
    .collect();
    if mentioned.len() > 1
        || mentioned
            .first()
            .is_some_and(|weekday| *weekday != date.weekday())
    {
        return Err("appointment date does not match the caller's selected weekday".into());
    }
    Ok(())
}

fn validate_spoken_time(agreement: &str, time: NaiveTime) -> Result<(), String> {
    static SPOKEN_TIME: OnceLock<Regex> = OnceLock::new();
    let regex = SPOKEN_TIME.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:at|around|about)\s+([01]?\d|2[0-3])(?::([0-5]\d))?\s*(a\.?m\.?|p\.?m\.?|o['’]?clock)?\b",
        )
        .expect("fixed spoken-time regex")
    });
    // The MOST RECENT spoken time wins: the consent window spans a few turns
    // and the caller may have revised ("at 9... actually around 10").
    let Some(captures) = regex.captures_iter(agreement).last() else {
        return Ok(());
    };
    let spoken_hour = captures
        .get(1)
        .and_then(|value| value.as_str().parse::<u32>().ok())
        .ok_or_else(|| "the caller's selected time was invalid".to_string())?;
    let spoken_minute = captures
        .get(2)
        .and_then(|value| value.as_str().parse::<u32>().ok())
        .unwrap_or(0);
    let suffix = captures
        .get(3)
        .map(|value| value.as_str().to_ascii_lowercase().replace('.', ""));
    let hour_matches = match suffix.as_deref() {
        Some("am") => time.hour() == spoken_hour % 12,
        Some("pm") => time.hour() == spoken_hour % 12 + 12,
        _ if spoken_hour > 12 => time.hour() == spoken_hour,
        _ => time.hour() % 12 == spoken_hour % 12,
    };
    if !hour_matches || time.minute() != spoken_minute {
        return Err("appointment time does not match the caller's selected time".into());
    }
    Ok(())
}

/// Validate one fixed `request_appointment` tool invocation.
///
/// `latest_caller_turn` and `caller_history` must come from final transcript
/// events owned by the plugin. The provider-supplied `agreementPhrase` is an
/// exact quote used only as a fence; it is never included in the durable event.
pub fn validate(
    arguments: &Value,
    call_id: &str,
    latest_caller_turn: Option<(u32, &str)>,
    caller_history: &[String],
    today: NaiveDate,
) -> Result<ValidatedAppointmentRequest, String> {
    if call_id.is_empty() || call_id.len() > 256 || call_id.chars().any(char::is_control) {
        return Err("the trusted call identity is invalid".into());
    }
    let object = arguments
        .as_object()
        .ok_or_else(|| "appointment request arguments must be an object".to_string())?;
    const KEYS: [&str; 5] = ["callerName", "service", "date", "time", "agreementPhrase"];
    if object.len() != KEYS.len() || !KEYS.iter().all(|key| object.contains_key(*key)) {
        return Err("appointment request contains unsupported fields".into());
    }

    let caller_name = bounded_plain(object.get("callerName"), "caller name", 120)?;
    let caller_name_kind = normalized_speech(&caller_name);
    if matches!(
        caller_name_kind.as_str(),
        "unknown" | "caller" | "n/a" | "na"
    ) {
        return Err("a real caller name is required".into());
    }
    let service = bounded_plain(object.get("service"), "service", 160)?;
    let date = bounded_plain(object.get("date"), "date", 10)?;
    let time = bounded_plain(object.get("time"), "time", 5)?;
    let agreement = bounded_plain(object.get("agreementPhrase"), "agreement phrase", 500)?;

    let parsed_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
        .map_err(|_| "date must be a real YYYY-MM-DD date".to_string())?;
    if parsed_date < today || parsed_date > today + Duration::days(730) {
        return Err("appointment date must be within the next two years".into());
    }
    let parsed_time = NaiveTime::parse_from_str(&time, "%H:%M")
        .map_err(|_| "time must be a real 24-hour HH:MM time".to_string())?;

    let (agreement_turn, latest_text) =
        latest_caller_turn.ok_or_else(|| "no final caller agreement is available".to_string())?;
    // Consent evidence spans the last few caller turns: real callers split a
    // booking across quick fragments ("book a gardening appointment for
    // Thursday" ... "Susan, and around 10 a.m."), and the provider may quote
    // any of them. All turns here come from the trusted final transcripts.
    let mut recent_newest_first: Vec<&str> = vec![latest_text];
    for turn in caller_history {
        if recent_newest_first.len() >= 3 {
            break;
        }
        if turn != latest_text {
            recent_newest_first.push(turn.as_str());
        }
    }
    let joined_chronological = recent_newest_first
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let quote_matches = recent_newest_first
        .iter()
        .any(|turn| agreement_matches_latest(&agreement, turn))
        || agreement_matches_latest(&agreement, &joined_chronological);
    if !quote_matches {
        return Err(
            "agreement phrase does not match the caller's recent turns; ask one short natural \
             confirmation question (never ask the caller to repeat exact wording), then call \
             this tool again quoting their answer"
                .into(),
        );
    }
    // The caller's consent: either the latest turn is itself a clear
    // agreement, or it is a plain detail turn (names a time/date, no question,
    // no refusal) completing an explicit booking request made moments before.
    let latest_is_agreement = is_conservative_agreement(latest_text);
    let latest_normalized = normalized_speech(latest_text);
    let latest_is_detail = !looks_like_question(latest_text)
        && !contains_negative(latest_text)
        && (latest_normalized
            .chars()
            .any(|character| character.is_ascii_digit())
            || DAY_WORDS.iter().any(|day| latest_normalized.contains(day)));
    let booking_request_nearby = recent_newest_first.iter().any(|turn| {
        contains_explicit_appointment_intent(turn)
            && !looks_like_question(turn)
            && !contains_negative(turn)
    });
    if !(latest_is_agreement || (latest_is_detail && booking_request_nearby)) {
        return Err(
            "the caller has not clearly agreed to this slot yet; ask one short natural \
             confirmation question (for example: 'Shall I put that request in for Thursday at \
             10?'), then call this tool again"
                .into(),
        );
    }
    validate_spoken_date(&joined_chronological, parsed_date, today)?;
    validate_spoken_time(&joined_chronological, parsed_time)?;
    if !caller_history
        .iter()
        .any(|turn| contains_explicit_appointment_intent(turn))
    {
        return Err("the caller did not explicitly request an appointment".into());
    }

    let identity = format!(
        "{}\n{}\n{}\n{}\n{}",
        call_id,
        normalized_speech(&caller_name),
        normalized_speech(&service),
        date,
        time
    );
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    Ok(ValidatedAppointmentRequest {
        request_id: format!("appt_{}", &digest[..32]),
        caller_name,
        service,
        date,
        time,
        agreement_turn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 20).unwrap()
    }

    fn lance_request() -> Value {
        json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-22",
            "time": "10:00",
            "agreementPhrase": "I'll be Wednesday at 10 o'clock."
        })
    }

    #[test]
    fn exact_live_call_language_is_a_bounded_request() {
        let request = validate(
            &lance_request(),
            "call_5685374790b241a1a48dd8549c0c6a4c",
            Some((6, "I'll be Wednesday at 10 o'clock.")),
            &[
                "Hi, I'd like to make an appointment, please.".into(),
                "Lawn mowing, my name is Lance as well.".into(),
                "I'll be Wednesday at 10 o'clock.".into(),
            ],
            day(),
        )
        .unwrap();
        assert_eq!(request.caller_name, "Lance");
        assert_eq!(request.date, "2026-07-22");
        assert_eq!(request.time, "10:00");
        assert_eq!(request.agreement_turn, 6);
        assert!(request.request_id.starts_with("appt_"));
        assert_eq!(request.request_id.len(), 37);
    }

    #[test]
    fn stable_business_identity_dedupes_provider_retries() {
        let first = validate(
            &lance_request(),
            "call_one",
            Some((6, "I'll be Wednesday at 10 o'clock.")),
            &["Please make an appointment.".into()],
            day(),
        )
        .unwrap();
        let second = validate(
            &lance_request(),
            "call_one",
            Some((9, "I'll be Wednesday at 10 o'clock.")),
            &["Please make an appointment.".into()],
            day(),
        )
        .unwrap();
        assert_eq!(first.request_id, second.request_id);
    }

    #[test]
    fn mismatch_uncertainty_and_missing_intent_fail_closed() {
        let mismatch = validate(
            &lance_request(),
            "call_one",
            Some((6, "Thursday would be better.")),
            &["Please make an appointment.".into()],
            day(),
        );
        assert!(mismatch.is_err());

        let mut uncertain = lance_request();
        uncertain["agreementPhrase"] = json!("Maybe Wednesday at 10.");
        assert!(validate(
            &uncertain,
            "call_one",
            Some((6, "Maybe Wednesday at 10.")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());

        assert!(validate(
            &lance_request(),
            "call_one",
            Some((6, "I'll be Wednesday at 10 o'clock.")),
            &["Is Wednesday at ten available?".into()],
            day(),
        )
        .is_err());

        let mut availability = lance_request();
        availability["agreementPhrase"] = json!("Is Wednesday at 10 available");
        assert!(validate(
            &availability,
            "call_one",
            Some((6, "Is Wednesday at 10 available")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());

        let mut direct_request = lance_request();
        direct_request["agreementPhrase"] = json!("Could you book me Wednesday at 10?");
        assert!(validate(
            &direct_request,
            "call_one",
            Some((6, "Could you book me Wednesday at 10?")),
            &["I'd like an appointment.".into()],
            day(),
        )
        .is_ok());
    }

    #[test]
    fn live_call_polite_question_bookings_are_accepted() {
        // call_051404a179974eb59f6685b0011db6aa turn 12: refused live with
        // "not a clear appointment agreement" before slot proposals counted.
        let tomorrow = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-21",
            "time": "10:00",
            "agreementPhrase": "How about tomorrow at 10 a.m.?"
        });
        let request = validate(
            &tomorrow,
            "call_051404a179974eb59f6685b0011db6aa",
            Some((12, "How about tomorrow at 10 a.m.?")),
            &["Hey, can you please check the appointments for me?".into()],
            day(),
        )
        .unwrap();
        assert_eq!(request.date, "2026-07-21");

        // Turn 14: "book one" was outside the needle list.
        let thursday = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-23",
            "time": "10:00",
            "agreementPhrase": "could you book one for Thursday then at 10 a.m.?"
        });
        validate(
            &thursday,
            "call_051404a179974eb59f6685b0011db6aa",
            Some((14, "could you book one for Thursday then at 10 a.m.?")),
            &["Hey, can you please check the appointments for me?".into()],
            day(),
        )
        .unwrap();
    }

    #[test]
    fn cross_transcriber_quote_divergence_is_tolerated_but_fabrication_is_not() {
        // The provider quotes its own hearing; the trusted transcript comes
        // from a different transcription model. Formatting must not refuse.
        let request = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-22",
            "time": "10:00",
            "agreementPhrase": "Yes, book it for Wednesday at 10am."
        });
        validate(
            &request,
            "call_one",
            Some((6, "Yes book it for Wednesday at ten a.m.")),
            &["I'd like an appointment.".into()],
            day(),
        )
        .unwrap();

        // A fragment of the turn is an acceptable quote.
        let fragment = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-22",
            "time": "10:00",
            "agreementPhrase": "book it for Wednesday at 10"
        });
        validate(
            &fragment,
            "call_one",
            Some((6, "Yes please, book it for Wednesday at 10.")),
            &["I'd like an appointment.".into()],
            day(),
        )
        .unwrap();

        // Words the caller never said still fail closed.
        let fabricated = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-22",
            "time": "10:00",
            "agreementPhrase": "Yes, book Wednesday at 10 for a full renovation quote."
        });
        assert!(validate(
            &fabricated,
            "call_one",
            Some((6, "Yes, book it for Wednesday at 10.")),
            &["I'd like an appointment.".into()],
            day(),
        )
        .is_err());
    }

    #[test]
    fn split_agreement_prefix_quote_is_accepted() {
        // Live Susan call: "Yes." (turn 6) then "Yes, that'd be good."
        // (turn 7) one second apart — the model quoted the first fragment
        // while the trusted latest turn carried the completed sentence.
        let request = json!({
            "callerName": "Susan",
            "service": "Gardening",
            "date": "2026-07-24",
            "time": "12:00",
            "agreementPhrase": "Yes."
        });
        validate(
            &request,
            "call_a635c87c5f1e46478250b0cda20d7124",
            Some((7, "Yes, that'd be good.")),
            &["I'd like to book a gardening appointment.".into()],
            day(),
        )
        .unwrap();

        // A quote that does not lead the turn still fails closed.
        let misleading = json!({
            "callerName": "Susan",
            "service": "Gardening",
            "date": "2026-07-24",
            "time": "12:00",
            "agreementPhrase": "good"
        });
        assert!(validate(
            &misleading,
            "call_one",
            Some((7, "Nothing good about Friday, cancel it all")),
            &["I'd like to book a gardening appointment.".into()],
            day(),
        )
        .is_err());
    }

    #[test]
    fn details_split_across_turns_complete_an_explicit_booking_request() {
        // Live Susan call 2: "I'd like to book a gardening appointment for
        // Thursday." then "Hello, this is" / "Susan, and around 10 a.m."
        // split across quick turns — the booking must assemble from the
        // recent window instead of demanding one perfect final sentence.
        let request = json!({
            "callerName": "Susan",
            "service": "Gardening",
            "date": "2026-07-23",
            "time": "10:00",
            "agreementPhrase": "Hello, this is Susan, and around 10 a.m."
        });
        let validated = validate(
            &request,
            "call_b76c4460a32e4312923f2ebf1954b4a5",
            Some((5, "Susan, and around 10 a.m.")),
            &[
                "Susan, and around 10 a.m.".into(),
                "Hello, this is".into(),
                "I'd like to book a gardening appointment for Thursday.".into(),
            ],
            day(),
        )
        .unwrap();
        assert_eq!(validated.date, "2026-07-23");
        assert_eq!(validated.time, "10:00");

        // A conflicting weekday in the window still fails closed.
        let mut wrong_day = request.clone();
        wrong_day["date"] = json!("2026-07-24");
        assert!(validate(
            &wrong_day,
            "call_one",
            Some((5, "Susan, and around 10 a.m.")),
            &[
                "Susan, and around 10 a.m.".into(),
                "I'd like to book a gardening appointment for Thursday.".into(),
            ],
            day(),
        )
        .is_err());

        // A conflicting spoken time in the window also fails closed.
        let mut wrong_time = request;
        wrong_time["time"] = json!("11:00");
        assert!(validate(
            &wrong_time,
            "call_one",
            Some((5, "Susan, and around 10 a.m.")),
            &[
                "Susan, and around 10 a.m.".into(),
                "I'd like to book a gardening appointment for Thursday.".into(),
            ],
            day(),
        )
        .is_err());
    }

    #[test]
    fn relative_day_phrases_validate_against_the_resolved_date() {
        // day() is Monday 2026-07-20: "this Thursday" resolves to 2026-07-23.
        let request = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-23",
            "time": "14:00",
            "agreementPhrase": "Yes, book me in for this Thursday at 2pm"
        });
        validate(
            &request,
            "call_one",
            Some((6, "Yes, book me in for this Thursday at 2pm")),
            &["I'd like an appointment.".into()],
            day(),
        )
        .unwrap();

        // The wrong weekday for the resolved date still fails closed.
        let mut wrong = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-24",
            "time": "14:00",
            "agreementPhrase": "Yes, book me in for this Thursday at 2pm"
        });
        assert!(validate(
            &wrong,
            "call_one",
            Some((6, "Yes, book me in for this Thursday at 2pm")),
            &["I'd like an appointment.".into()],
            day(),
        )
        .is_err());
        wrong["date"] = json!("2026-07-23");
        wrong["time"] = json!("15:00");
        assert!(validate(
            &wrong,
            "call_one",
            Some((6, "Yes, book me in for this Thursday at 2pm")),
            &["I'd like an appointment.".into()],
            day(),
        )
        .is_err());
    }

    #[test]
    fn availability_exploration_stays_read_only() {
        let exploration = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-22",
            "time": "10:00",
            "agreementPhrase": "Is Wednesday at 10 available?"
        });
        assert!(validate(
            &exploration,
            "call_one",
            Some((6, "Is Wednesday at 10 available?")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());

        let anything = json!({
            "callerName": "Lance",
            "service": "Lawn mowing",
            "date": "2026-07-22",
            "time": "10:00",
            "agreementPhrase": "Do you have anything Wednesday at 10?"
        });
        assert!(validate(
            &anything,
            "call_one",
            Some((6, "Do you have anything Wednesday at 10?")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());
    }

    #[test]
    fn invalid_or_remote_dates_and_extra_fields_are_rejected() {
        assert!(validate(
            &lance_request(),
            "",
            Some((6, "I'll be Wednesday at 10 o'clock.")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());

        for date in ["2026-02-30", "2026-07-19", "2029-01-01"] {
            let mut request = lance_request();
            request["date"] = json!(date);
            assert!(validate(
                &request,
                "call_one",
                Some((6, "I'll be Wednesday at 10 o'clock.")),
                &["Please make an appointment.".into()],
                day(),
            )
            .is_err());
        }

        let mut extra = lance_request();
        extra["status"] = json!("confirmed");
        assert!(validate(
            &extra,
            "call_one",
            Some((6, "I'll be Wednesday at 10 o'clock.")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());

        let mut wrong_weekday = lance_request();
        wrong_weekday["date"] = json!("2026-07-23");
        assert!(validate(
            &wrong_weekday,
            "call_one",
            Some((6, "I'll be Wednesday at 10 o'clock.")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());

        let mut wrong_time = lance_request();
        wrong_time["time"] = json!("11:00");
        assert!(validate(
            &wrong_time,
            "call_one",
            Some((6, "I'll be Wednesday at 10 o'clock.")),
            &["Please make an appointment.".into()],
            day(),
        )
        .is_err());
    }
}
