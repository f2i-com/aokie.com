//! Detect tool intent in the bot's natural-language reply.
//!
//! The model says "let me check availability for Friday May 15" — the
//! detector parses out `CheckAvailability { date=2026-05-15,
//! service="Lawn mow" }` and the runtime executes it. The XML
//! `<TOOL>...</TOOL>` path is still honoured (model occasionally
//! emits one), but it's a fallback now, not the primary trigger.
//!
//! State-gated on purpose. "Let me check" with no `draft.service` /
//! `draft.date` shouldn't fire `CheckAvailability` against today —
//! the bot is bullshitting and the recovery is to fall through so
//! the next turn re-prompts with a question.

use chrono::NaiveDate;

use super::{dates, services};
use crate::calendar::session::{CallSession, SessionMode};
use crate::calendar::{Service, ToolCall};

/// One detected tool intention. `phrase` is the literal substring that
/// triggered it — surfaced in logs so we can audit what's working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerbalToolDetection {
    pub tool: ToolCall,
    pub source: DetectionSource,
    pub confidence: Confidence,
    pub phrase: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionSource {
    /// Pulled out of `<TOOL>...</TOOL>` XML (legacy path, kept as fallback).
    XmlTag,
    /// Pulled out of natural-language phrasing in the bot's reply.
    Verbal,
    /// Last-ditch corrective re-prompt fired after pass-1 stalled.
    /// (Existing `run_voice_stall_recovery` path tags its output with
    /// this. Kept here so log analysis can distinguish the three
    /// pathways.)
    StallRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// Scan a bot reply for verbal tool intentions, gated on session
/// state. Returns up to one tool call — bot turns rarely justify
/// more than one mid-conversation tool fire (the legacy `<TOOL>`
/// path could chain, but verbal detection is one-shot per turn).
pub fn detect_verbal_tools(
    bot_reply: &str,
    session: &CallSession,
    services_list: &[Service],
    today: NaiveDate,
) -> Option<VerbalToolDetection> {
    let lower = bot_reply.to_lowercase();

    // 1. Book — highest priority because the prompt only emits a
    //    "locking that in" phrase when it's about to commit.
    if let Some(d) = detect_book(&lower, bot_reply, session, services_list) {
        return Some(d);
    }

    // 2. MyAppointments — caller asked about their existing bookings.
    if let Some(d) = detect_my_appointments(&lower, bot_reply, session) {
        return Some(d);
    }

    // 3. CheckAvailability — the most common case; runs only if no
    //    higher-priority tool fired.
    if let Some(d) = detect_check_availability(&lower, bot_reply, session, services_list, today) {
        return Some(d);
    }

    // 4. ListServices — caller asked what's on offer and the bot
    //    forgot to read them out.
    if let Some(d) = detect_list_services(&lower, bot_reply) {
        return Some(d);
    }

    None
}

// ============================================================================
// Pattern detectors
// ============================================================================

// Phrase lists are intentionally short and broad. `first_match`
// does substring matching, so "let me check" already covers
// "let me check it" / "let me check now" / "let me check the
// calendar". Specific noun-phrase variants ("checking your
// calendar", "checking the schedule") collapse into the bare
// gerund forms ("checking", "looking up") because the
// downstream `detect_check_availability` guard already requires
// either a date in the reply or a Gathering-mode session with
// both date + service drafted — that grounding gate is what
// keeps a generic "checking" from firing on unrelated content.
const CHECK_PHRASES: &[&str] = &[
    // "let me ..." family (prompt's exemplar phrasing)
    "let me check",
    "let me look",
    "let me see",
    "let me pull up",
    "let me pull that up",
    "let me have a look",
    // "i'll ..." family (future-tense first person)
    "i'll check",
    "i'll look",
    "i'll have a look",
    "i'll have to check",
    "i'll pull up",
    "i'll see",
    "i'll go ahead and check",
    // "i'm ..." / "i am ..." family — present continuous, the form
    // that triggered R6-#1: the live LLM said "I'm checking the
    // availability" which matched none of the previous phrase set.
    "i'm checking",
    "i am checking",
    "i'm looking",
    "i am looking",
    "i'm pulling up",
    "i'm having a look",
    // Bare gerunds. The grounding gate downstream means these
    // can only fire when there's a real date / service to check.
    "checking",
    "looking up",
    "looking that up",
    "looking it up",
    "looking into",
    "pulling up",
    // Time-buying phrases that pair "moment / second" with "check".
    "moment to check",
    "moment while i check",
    "second to check",
    "second while i check",
];

fn detect_check_availability(
    lower: &str,
    raw: &str,
    session: &CallSession,
    services_list: &[Service],
    today: NaiveDate,
) -> Option<VerbalToolDetection> {
    // Exclusion: don't re-fire when we just got slots.
    if matches!(
        session.mode,
        SessionMode::SlotsOffered | SessionMode::Confirming | SessionMode::Booked
    ) {
        return None;
    }

    let phrase = first_match(lower, CHECK_PHRASES)?;

    // Date — try the reply itself first, fall back to draft.
    let date = dates::parse_relative_date(raw, today)
        .or_else(|| session.draft_booking.as_ref().and_then(|d| d.date))?;

    // Service — try the reply, fall back to draft. Service is optional
    // for `CheckAvailability` (the executor lists everything when
    // unset), but we'd rather not fire a too-broad check during a
    // call about a specific service.
    let service_from_reply = services::match_service(services_list, raw).map(|s| s.name.clone());
    let service_from_draft = session
        .draft_booking
        .as_ref()
        .and_then(|d| d.service.clone());
    // Capture the bool before moving service_from_reply into `.or()`.
    let from_reply_service = service_from_reply.is_some();
    let service = service_from_reply.or(service_from_draft);

    // Decide confidence + whether we fire based on what the bot's
    // reply itself referenced. Two cases qualify:
    //
    // (a) The reply names a date and/or service explicitly. Fire with
    //     High (both) or Medium (one) confidence — the bot grounded
    //     its stall in a real referent.
    //
    // (b) The reply uses a generic "let me check that for you" but the
    //     session is already in Gathering mode with BOTH date + service
    //     in the draft. The caller named these on a prior turn; the
    //     bot is simply saying it'll check what was given. Fire with
    //     Medium confidence. Relaxation here trades a small risk of
    //     false-positive (the bot was actually checking something
    //     unrelated like opening hours) for the much larger fix of
    //     killing the dead-air bug when the bot's stall doesn't
    //     restate the date / service.
    let from_reply_date = dates::parse_relative_date(raw, today).is_some();
    let in_gathering_with_full_draft = matches!(session.mode, SessionMode::Gathering)
        && session
            .draft_booking
            .as_ref()
            .map(|d| d.date.is_some() && d.service.is_some())
            .unwrap_or(false);
    if !from_reply_date && !from_reply_service && !in_gathering_with_full_draft {
        return None;
    }

    Some(VerbalToolDetection {
        tool: ToolCall::CheckAvailability { date, service },
        source: DetectionSource::Verbal,
        confidence: if from_reply_date && from_reply_service {
            Confidence::High
        } else {
            Confidence::Medium
        },
        phrase: phrase.to_string(),
    })
}

// BOOK is gated by `to_book_call` which requires every draft
// field set (date, time, service, customer name, address-if-
// required); a phrase match against an incomplete draft returns
// None and falls through to CHECK. That makes it safe to be very
// loose with the surface phrases here — a bare "booking your"
// can't fire Book unless the rest of the draft is already in
// place, so over-matching just means slightly more work parsing,
// not false-positive bookings.
const BOOK_PHRASES: &[&str] = &[
    // "let me ..." (prompt's exemplar phrasing)
    "let me book",
    "let me lock",
    "let me get that booked",
    "let me get that locked in",
    "let me put that in",
    "let me set that up",
    // "i'll ..." (future tense)
    "i'll book",
    "i'll lock that in",
    "i'll get that booked",
    "i'll get you booked",
    "i'll get you down",
    "i'll set that up",
    "i'll go ahead and book",
    "i'll go ahead and lock",
    // "i'm ..." / "i am ..." — present continuous
    "i'm booking",
    "i am booking",
    "i'm getting you booked",
    "i'm locking",
    "i'm setting that up",
    "i'm setting up your booking",
    "i'm putting",
    // Bare gerunds — "Booking ...", "Locking ..." etc. as
    // standalone commitment phrases. The to_book_call guard
    // protects against firing on partial drafts.
    "booking that",
    "booking you",
    "booking your",
    "locking that in",
    "locking it in",
    "putting that in",
    "putting you down",
    "setting up your booking",
    "going to book",
    "going to lock",
];

fn detect_book(
    lower: &str,
    _raw: &str,
    session: &CallSession,
    services_list: &[Service],
) -> Option<VerbalToolDetection> {
    let phrase = first_match(lower, BOOK_PHRASES)?;
    let draft = session.draft_booking.as_ref()?;
    let service_name = draft.service.as_deref()?;
    let svc = services::match_service(services_list, service_name)?;

    let tool = draft.to_book_call(svc, &session.caller_phone)?;

    Some(VerbalToolDetection {
        tool,
        source: DetectionSource::Verbal,
        confidence: Confidence::High,
        phrase: phrase.to_string(),
    })
}

// MY_APPTS gates only on caller_phone, so phrases must clearly
// mean "look up existing bookings" — not "I'm about to write
// a new one". The verbs (pull up / look up / check / pulling
// up) and possessive "your appointments / your bookings" are
// what separate this from BOOK ("book / lock / set up") and
// CHECK ("check the availability"). Substring-matched against
// the lowercased reply, so "let me pull up your" naturally
// covers "let me pull up your appointments" / "...your
// bookings" / "...your schedule".
const MY_APPTS_PHRASES: &[&str] = &[
    // "let me ..." family
    "let me pull up your",
    "let me look up your",
    "let me check your",
    "let me see what you have",
    "let me see what's booked",
    // "i'll ..." family (future tense)
    "i'll pull up your",
    "i'll look up your",
    "i'll check your",
    // "i'm ..." family (present continuous)
    "i'm pulling up your",
    "i'm looking up your",
    "i'm checking your",
    // Bare gerunds with possessive grounding
    "checking your appointments",
    "checking your bookings",
    "looking up your appointments",
    "looking up your bookings",
    "pulling up your appointments",
    "pulling up your bookings",
    // Possessive nouns implying existing state
    "your upcoming appointment",
    "your upcoming booking",
    "your existing booking",
    "what you have booked",
    "what you've got booked",
];

fn detect_my_appointments(
    lower: &str,
    _raw: &str,
    session: &CallSession,
) -> Option<VerbalToolDetection> {
    let phrase = first_match(lower, MY_APPTS_PHRASES)?;
    if session.caller_phone.is_empty() {
        // No caller-ID → MyAppointments has no useful answer.
        return None;
    }
    Some(VerbalToolDetection {
        tool: ToolCall::MyAppointments {
            customer_phone: session.caller_phone.clone(),
        },
        source: DetectionSource::Verbal,
        confidence: Confidence::High,
        phrase: phrase.to_string(),
    })
}

const LIST_SERVICES_PHRASES: &[&str] = &[
    "let me read out our services",
    "let me list our services",
    "let me read you the menu",
    "let me run through what we offer",
    "i'll read out the services",
];

fn detect_list_services(lower: &str, _raw: &str) -> Option<VerbalToolDetection> {
    let phrase = first_match(lower, LIST_SERVICES_PHRASES)?;
    Some(VerbalToolDetection {
        tool: ToolCall::ListServices,
        source: DetectionSource::Verbal,
        confidence: Confidence::High,
        phrase: phrase.to_string(),
    })
}

fn first_match<'a>(hay: &'a str, needles: &[&'a str]) -> Option<&'a str> {
    let mut earliest: Option<(usize, &str)> = None;
    for n in needles {
        if let Some(idx) = hay.find(n) {
            match earliest {
                None => earliest = Some((idx, n)),
                Some((cur, _)) if idx < cur => earliest = Some((idx, n)),
                _ => {}
            }
        }
    }
    earliest.map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::session::{BookingDraft, OfferedSlots};
    use chrono::{NaiveDate, NaiveTime};

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

    fn session_with_draft(draft: BookingDraft) -> CallSession {
        let mut s = CallSession::new("c1".into(), "+61432000111".into(), None);
        s.draft_booking = Some(draft);
        s
    }

    #[test]
    fn check_availability_fires_when_date_in_reply() {
        let session = session_with_draft(BookingDraft {
            service: Some("Lawn mow".into()),
            ..Default::default()
        });
        let services = vec![svc("Lawn mow", true)];
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let detected = detect_verbal_tools(
            "Let me check availability for next Friday.",
            &session,
            &services,
            today,
        )
        .unwrap();
        match &detected.tool {
            ToolCall::CheckAvailability { date, service } => {
                assert_eq!(*date, NaiveDate::from_ymd_opt(2026, 5, 8).unwrap());
                assert_eq!(service.as_deref(), Some("Lawn mow"));
            }
            _ => panic!("expected CheckAvailability, got {:?}", detected.tool),
        }
        assert_eq!(detected.source, DetectionSource::Verbal);
    }

    #[test]
    fn check_availability_uses_draft_service_when_reply_doesnt_name_it() {
        let session = session_with_draft(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            ..Default::default()
        });
        let services = vec![svc("Lawn mow", true)];
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let detected = detect_verbal_tools(
            "Let me check availability for next Friday.",
            &session,
            &services,
            today,
        )
        .unwrap();
        if let ToolCall::CheckAvailability { service, .. } = &detected.tool {
            assert_eq!(service.as_deref(), Some("Lawn mow"));
        } else {
            panic!("expected CheckAvailability");
        }
    }

    #[test]
    fn does_not_re_fire_check_when_slots_already_offered() {
        let mut session = session_with_draft(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            ..Default::default()
        });
        session.last_offered_slots = Some(OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![NaiveTime::from_hms_opt(14, 0, 0).unwrap()],
        });
        session.mode = SessionMode::SlotsOffered;
        let services = vec![svc("Lawn mow", true)];
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        // Bot hedge in slots-offered state shouldn't re-trigger.
        assert!(detect_verbal_tools(
            "Let me check that one more time.",
            &session,
            &services,
            today,
        )
        .is_none());
    }

    #[test]
    fn check_fires_in_gathering_mode_when_draft_has_full_referent() {
        // Pre-detect set draft.date + draft.service from a prior user
        // turn ("Friday for a lawn mow"). Bot says "let me check that
        // for you" without restating either — this is exactly the
        // dead-air case the relaxation was added for. Fire with the
        // values from the draft.
        let mut session = session_with_draft(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            ..Default::default()
        });
        session.mode = SessionMode::Gathering;
        let services = vec![svc("Lawn mow", true)];
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let detected =
            detect_verbal_tools("Let me check that for you.", &session, &services, today)
                .expect("should fire from draft state alone in Gathering mode");
        match &detected.tool {
            ToolCall::CheckAvailability { date, service } => {
                assert_eq!(*date, NaiveDate::from_ymd_opt(2026, 5, 15).unwrap());
                assert_eq!(service.as_deref(), Some("Lawn mow"));
            }
            _ => panic!("expected CheckAvailability"),
        }
        assert_eq!(detected.confidence, Confidence::Medium);
    }

    #[test]
    fn check_does_not_fire_in_idle_mode_without_reply_signal() {
        // If the caller hasn't said anything yet, mode is Idle. A bare
        // "let me check" should NOT fire — there's nothing to check.
        let session = session_with_draft(BookingDraft::default());
        let services = vec![svc("Lawn mow", true)];
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        assert!(detect_verbal_tools("Let me check.", &session, &services, today).is_none());
    }

    #[test]
    fn book_fires_when_draft_complete_and_phrase_present() {
        let svc1 = svc("Lawn mow", true);
        let mut session = session_with_draft(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            start_time: Some(NaiveTime::from_hms_opt(14, 0, 0).unwrap()),
            customer_name: Some("Lance".into()),
            address: Some("12 Park St".into()),
            notes: None,
        });
        session.mode = SessionMode::Confirming;
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let detected =
            detect_verbal_tools("Locking that in for you.", &session, &[svc1], today).unwrap();
        assert!(matches!(detected.tool, ToolCall::Book { .. }));
        assert_eq!(detected.source, DetectionSource::Verbal);
    }

    #[test]
    fn book_does_not_fire_when_address_missing_for_required_service() {
        let svc1 = svc("Lawn mow", true);
        let session = session_with_draft(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            start_time: Some(NaiveTime::from_hms_opt(14, 0, 0).unwrap()),
            customer_name: Some("Lance".into()),
            address: None,
            notes: None,
        });
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        assert!(
            detect_verbal_tools("Locking that in for you.", &session, &[svc1], today,).is_none()
        );
    }

    #[test]
    fn my_appointments_requires_caller_phone() {
        let mut session = session_with_draft(BookingDraft::default());
        session.caller_phone = String::new();
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        assert!(
            detect_verbal_tools("Let me pull up your appointments.", &session, &[], today,)
                .is_none()
        );

        session.caller_phone = "+61432000111".into();
        let detected =
            detect_verbal_tools("Let me pull up your appointments.", &session, &[], today).unwrap();
        assert!(matches!(detected.tool, ToolCall::MyAppointments { .. }));
    }

    #[test]
    fn book_priority_above_check() {
        // Bot reply has both "checking" and "locking that in" phrasings.
        // Book should win because it's the more specific commitment.
        let svc1 = svc("Lawn mow", false);
        let mut session = session_with_draft(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            start_time: Some(NaiveTime::from_hms_opt(14, 0, 0).unwrap()),
            customer_name: Some("Lance".into()),
            ..Default::default()
        });
        session.mode = SessionMode::Confirming;
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        let detected = detect_verbal_tools(
            "Checking the calendar — locking that in for you for Friday.",
            &session,
            &[svc1],
            today,
        )
        .unwrap();
        assert!(matches!(detected.tool, ToolCall::Book { .. }));
    }

    #[test]
    fn no_match_returns_none() {
        let session = session_with_draft(BookingDraft::default());
        let today = NaiveDate::from_ymd_opt(2026, 5, 6).unwrap();
        assert!(detect_verbal_tools("How can I help you today?", &session, &[], today,).is_none());
    }

    /// R6-#1 regression: the live LLM said "I'm checking the
    /// availability for next Monday, 11 May 2026, for Lawn Mowing at
    /// 10 Wendy Drive, Point Clair. Just a moment please." The previous
    /// CHECK_PHRASES list had "let me check" / "checking the calendar"
    /// / "checking the schedule" but missed "i'm checking" /
    /// "checking the availability" / "checking availability" — the
    /// detector returned None and the bot sat in dead-air after
    /// announcing it would check.
    #[test]
    fn check_fires_on_im_checking_the_availability_user_phrasing() {
        let mut session = session_with_draft(BookingDraft {
            service: Some("Lawn Mowing".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap()),
            address: Some("10 Wendy Drive, Point Clair".into()),
            ..Default::default()
        });
        session.mode = SessionMode::Gathering;
        let services = vec![svc("Lawn Mowing", true)];
        let today = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let detected = detect_verbal_tools(
            "I'm checking the availability for next Monday, 11 May 2026, for Lawn Mowing at 10 Wendy Drive, Point Clair. Just a moment please.",
            &session,
            &services,
            today,
        )
        .expect("user's exact bot phrasing must trigger CheckAvailability");
        match &detected.tool {
            ToolCall::CheckAvailability { date, service } => {
                assert_eq!(*date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
                assert_eq!(service.as_deref(), Some("Lawn Mowing"));
            }
            other => panic!("expected CheckAvailability, got {:?}", other),
        }
        assert_eq!(detected.confidence, Confidence::High);
    }

    /// Present-continuous and bare-gerund forms across the full
    /// CHECK_PHRASES surface. Each is a phrasing the LLM uses
    /// naturally instead of the prompt's "let me check" exemplar.
    /// Pinning them so a future detector tightening doesn't quietly
    /// regress the broadened recognition.
    #[test]
    fn check_fires_on_present_continuous_paraphrases() {
        let mut session = session_with_draft(BookingDraft {
            service: Some("Lawn Mowing".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap()),
            ..Default::default()
        });
        session.mode = SessionMode::Gathering;
        let services = vec![svc("Lawn Mowing", true)];
        let today = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        for phrase in &[
            "I'm checking availability for Monday.",
            "I am checking the availability for next Monday.",
            "Checking availability for you now.",
            "Checking your availability for next week.",
            "Looking that up for you, one moment.",
            "I'll have to check the calendar — give me a sec.",
            "Pulling up the schedule for next Monday.",
            "One moment while I check.",
        ] {
            assert!(
                detect_verbal_tools(phrase, &session, &services, today).is_some(),
                "phrase {:?} should trigger CheckAvailability",
                phrase
            );
        }
    }

    /// Same paraphrase coverage on the BOOK side. Present-continuous
    /// "I'm booking you in", bare gerund "Booking that in", and
    /// "Going to book" are all natural LLM phrasings that the
    /// prompt's "let me book" exemplar alone wouldn't pre-train.
    #[test]
    fn book_fires_on_present_continuous_paraphrases() {
        let svc1 = svc("Lawn Mowing", true);
        let mut session = session_with_draft(BookingDraft {
            service: Some("Lawn Mowing".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap()),
            start_time: Some(NaiveTime::from_hms_opt(10, 0, 0).unwrap()),
            customer_name: Some("Lance".into()),
            address: Some("10 Wendy Drive".into()),
            notes: None,
        });
        session.mode = SessionMode::Confirming;
        let today = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        for phrase in &[
            "I'm booking you in for Monday at 10am.",
            "Booking that in now.",
            "Going to book the lawn mow for Monday.",
            "I'm setting up your booking now.",
        ] {
            assert!(
                matches!(
                    detect_verbal_tools(phrase, &session, &[svc1.clone()], today).map(|d| d.tool),
                    Some(ToolCall::Book { .. })
                ),
                "phrase {:?} should trigger Book",
                phrase
            );
        }
    }
}
