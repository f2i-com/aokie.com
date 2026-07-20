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

fn normalized_quote(value: &str) -> String {
    normalized_speech(value)
        .replace('‘', "'")
        .replace('’', "'")
        .trim_end_matches(|character: char| matches!(character, '.' | '!' | '?'))
        .trim()
        .to_string()
}

fn contains_explicit_appointment_intent(value: &str) -> bool {
    let value = normalized_speech(value);
    [
        "appointment",
        "book me",
        "book an",
        "book a",
        "make a booking",
        "make an appointment",
        "schedule me",
        "schedule an",
        "reserve a",
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

fn is_conservative_agreement(value: &str) -> bool {
    let value = normalized_speech(value);
    let negative = [
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
    .any(|needle| value.contains(needle));
    if negative {
        return false;
    }

    let direct_booking_request = [
        "book me",
        "book an",
        "book a",
        "make a booking",
        "make an appointment",
        "schedule me",
        "schedule an",
        "put me down",
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

    // Punctuation in automatic transcripts is not guaranteed. A lexical
    // availability question is never treated as agreement merely because it
    // contains a weekday and a number. A direct booking request is different:
    // "Could you book me Wednesday at 10?" is itself explicit approval.
    (concrete_slot && (!looks_like_question(&value) || direct_booking_request))
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
            r"(?i)\bat\s+([01]?\d|2[0-3])(?::([0-5]\d))?\s*(a\.?m\.?|p\.?m\.?|o['’]?clock)?\b",
        )
        .expect("fixed spoken-time regex")
    });
    let Some(captures) = regex.captures(agreement) else {
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
    if normalized_quote(&agreement) != normalized_quote(latest_text) {
        return Err("agreement phrase does not match the latest caller turn".into());
    }
    if !is_conservative_agreement(latest_text) {
        return Err("the latest caller turn is not a clear appointment agreement".into());
    }
    validate_spoken_date(latest_text, parsed_date, today)?;
    validate_spoken_time(latest_text, parsed_time)?;
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
