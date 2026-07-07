//! Per-call slot-filling state. Replaces the old "everything lives in
//! conversation history" model where the bot had to remember whether
//! it'd already asked for a name, which slot was offered, etc.
//!
//! A `CallSession` is created on `CallAnswered`, mutated turn by turn
//! (user-utterance pre-detector fills the draft; bot-reply detector
//! reads it to gate tool calls), and dropped on `CallTerminated`.
//! Concurrent-call safety isn't a concern — Aokie handles one HFP call
//! at a time and the session is owned by the per-call event loop.
//!
//! The session is the source of truth for "what does the bot need
//! next?" — the slim system-prompt builder reads `mode` to decide which
//! micro-instruction to inject ("you have date+service, tell the
//! caller you're checking", "all details captured, read them back",
//! etc). That's the Phase 3 piece; the data here is shape-only.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

use super::{Service, ToolCall};

/// In-memory state for one in-progress phone call. Lives inside the
/// bluetooth event-loop spawn task and is dropped on `CallTerminated`.
#[derive(Debug, Clone, Default)]
pub struct CallSession {
    pub call_id: String,

    /// Caller-ID phone (E.164). Empty string means caller-ID was
    /// withheld — privacy clamps still apply but `MyAppointments` is
    /// effectively a no-op.
    pub caller_phone: String,

    /// Resolved from the contact store, if known.
    pub caller_name: Option<String>,

    /// In-progress booking. `None` until the caller gives us either a
    /// service or a date — that's the trigger for "we are now in a
    /// booking flow". Cleared on explicit reset or after a successful
    /// `Book` execution.
    pub draft_booking: Option<BookingDraft>,

    /// In-progress order. Same lifecycle as `draft_booking`.
    pub draft_order: Option<OrderDraft>,

    /// Slots returned by the most recent `check_availability`. Used to
    /// resolve "yes 2pm works" / "the second one" against a real
    /// `NaiveTime`. `None` once the caller picks a slot or the topic
    /// changes.
    pub last_offered_slots: Option<OfferedSlots>,

    /// Drives the slim system-prompt builder. See `SessionMode`.
    pub mode: SessionMode,

    /// Number of bookings successfully minted during this call. Surfaced
    /// up through `TurnResult.in_call_booked` so `spawn_post_call_booking_extraction`
    /// skips re-booking after the call ends.
    pub bookings_made: usize,

    /// Number of orders successfully placed during this call. Same purpose.
    pub orders_placed: usize,
}

/// What information we've collected so far about a booking the caller
/// wants to make. Each field is `Option` because callers give them in
/// different orders — some lead with the service ("I want a lawn
/// mow"), some with the day ("are you free Friday?"), some with both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookingDraft {
    /// Resolved against the active `Service` list — stored as the
    /// canonical name from the catalogue, not whatever the caller
    /// said. Avoids "Lawn Mow" vs "lawn mow" mismatches at book time.
    pub service: Option<String>,

    /// Resolved against TODAY in the configured timezone.
    pub date: Option<NaiveDate>,

    /// 24-hour. Set by the user-utterance pre-detector when the caller
    /// picks a slot from `last_offered_slots`.
    pub start_time: Option<NaiveTime>,

    pub customer_name: Option<String>,

    /// Only required when the matched service has `requires_address=true`.
    /// `BookingDraft::is_complete` enforces this.
    pub address: Option<String>,

    pub notes: Option<String>,
}

/// What information we've collected so far about an order the caller
/// wants to place. Mirrors the `OrderToolCall::AddItem`/`PlaceOrder`
/// shape but treats item-add as cumulative (multiple items can be in
/// flight before placement).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderDraft {
    /// Items the caller has named so far. Each entry is whatever the
    /// caller said — the executor resolves against the product
    /// catalogue when `place_order` runs.
    pub items: Vec<DraftOrderItem>,
    pub customer_name: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftOrderItem {
    pub product: String,
    pub quantity: u32,
    pub extras: Vec<String>,
    pub notes: Option<String>,
}

/// Slots offered by the most recent `check_availability` result.
/// `bot_intent.rs` reads this to gate "let me check again" patterns
/// (don't re-fire if we already have an offer on the table); user_intent.rs
/// reads it to resolve "yes 2pm" → `NaiveTime`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedSlots {
    pub date: NaiveDate,
    pub service: String,
    pub slots: Vec<NaiveTime>,
}

/// Coarse-grained state used to slim the system prompt. Each variant
/// gets a small mode-specific fragment in `prompt::build_voice_system_prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    /// No active draft. Generic receptionist mode.
    #[default]
    Idle,

    /// Caller is mid-booking. The draft has at least a service or a
    /// date but is missing other required fields. Prompt nudges the
    /// bot toward asking for what's missing without re-asking known
    /// fields.
    Gathering,

    /// `last_offered_slots` is populated. Prompt reminds the bot of
    /// the offered times so it doesn't list ten slots back at the
    /// caller.
    SlotsOffered,

    /// All booking fields are present. Prompt nudges the bot toward
    /// reading the details back rather than emitting a final "all
    /// set!" before the caller has confirmed.
    Confirming,

    /// Booking minted within this call. Prompt is conversational
    /// close-out — confirm once, wrap up.
    Booked,

    /// Order draft is in flight. Same idea as `Gathering` for orders.
    OrderGathering,

    /// Order placed within this call.
    OrderPlaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingField {
    Service,
    Date,
    Time,
    CustomerName,
    Address,
}

impl CallSession {
    pub fn new(call_id: String, caller_phone: String, caller_name: Option<String>) -> Self {
        Self {
            call_id,
            caller_phone,
            caller_name,
            ..Default::default()
        }
    }

    /// Reset the booking draft + offered slots. Used when the caller
    /// says "never mind" or otherwise abandons a booking flow. Keeps
    /// caller-ID and call-level counters intact — those persist across
    /// the whole call.
    pub fn reset_booking(&mut self) {
        self.draft_booking = None;
        self.last_offered_slots = None;
        self.mode = SessionMode::Idle;
    }

    pub fn reset_order(&mut self) {
        self.draft_order = None;
        if matches!(
            self.mode,
            SessionMode::OrderGathering | SessionMode::OrderPlaced
        ) {
            self.mode = SessionMode::Idle;
        }
    }

    /// Recompute `mode` from the current draft state. Called after every
    /// detector pass (user pre-detector and bot-reply detector both),
    /// so the slim prompt builder always sees a consistent mode.
    pub fn recompute_mode(&mut self, services: &[Service]) {
        // Booked / OrderPlaced are sticky AS LONG AS no new draft has
        // appeared. The booking-completion path clears `draft_booking`
        // / `draft_order`, so a caller saying "now book me a haircut
        // too" after a confirmed booking will populate a NEW draft
        // via pre-detect and we want mode to flow out of Booked into
        // Gathering for the next round. If no new draft is forming
        // (just chit-chat after the booking), Booked stays.
        if matches!(self.mode, SessionMode::Booked) && self.draft_booking.is_none() {
            return;
        }
        if matches!(self.mode, SessionMode::OrderPlaced) && self.draft_order.is_none() {
            return;
        }
        if let Some(draft) = self.draft_booking.as_ref() {
            let svc = draft
                .service
                .as_deref()
                .and_then(|n| match_service(services, n));
            self.mode = if let Some(svc) = svc {
                if draft.is_complete(svc) {
                    SessionMode::Confirming
                } else if self.last_offered_slots.is_some() {
                    SessionMode::SlotsOffered
                } else {
                    SessionMode::Gathering
                }
            } else if self.last_offered_slots.is_some() {
                SessionMode::SlotsOffered
            } else {
                SessionMode::Gathering
            };
            return;
        }
        if self.draft_order.is_some() {
            self.mode = SessionMode::OrderGathering;
            return;
        }
        // Defensive: orphan offered slots without a draft (runtime
        // path always pairs them, but a manual reset / partial update
        // could land us here). Treat as SlotsOffered so the slim
        // prompt still shows the times to the bot.
        if self.last_offered_slots.is_some() {
            self.mode = SessionMode::SlotsOffered;
            return;
        }
        self.mode = SessionMode::Idle;
    }
}

impl BookingDraft {
    /// True when every field the executor needs is present. `service`
    /// drives whether `address` is required — without the service we
    /// can't decide, so missing-service alone disqualifies.
    pub fn is_complete(&self, service: &Service) -> bool {
        self.missing_fields(service).is_empty()
    }

    pub fn missing_fields(&self, service: &Service) -> Vec<MissingField> {
        let mut out = Vec::new();
        if self.service.is_none() {
            out.push(MissingField::Service);
        }
        if self.date.is_none() {
            out.push(MissingField::Date);
        }
        if self.start_time.is_none() {
            out.push(MissingField::Time);
        }
        if self.customer_name.is_none() {
            out.push(MissingField::CustomerName);
        }
        if service.requires_address && self.address.is_none() {
            out.push(MissingField::Address);
        }
        out
    }

    /// Mint a `Book` tool call from a complete draft. Returns `None`
    /// if any required field is missing — caller is expected to gate
    /// on `is_complete` first; this is a defence-in-depth check.
    ///
    /// Phone always comes from caller-ID (privacy clamp); the model's
    /// XML path also enforces this in `run_voice_inline_tools_and_pass2`.
    pub fn to_book_call(&self, service: &Service, caller_phone: &str) -> Option<ToolCall> {
        let svc_name = self.service.clone()?;
        let date = self.date?;
        let time = self.start_time?;
        let start = NaiveDateTime::new(date, time);
        let _ = self.is_complete(service).then_some(())?;
        Some(ToolCall::Book {
            service: svc_name,
            start,
            customer_name: self.customer_name.clone(),
            customer_phone: if caller_phone.is_empty() {
                None
            } else {
                Some(caller_phone.to_string())
            },
            notes: self.notes.clone(),
            address: self.address.clone(),
        })
    }
}

/// Case-insensitive match against the active services. Substring on
/// either side — "lawn mow" matches "Lawn mow + edges", and "lawn mow
/// + edges" matches "Lawn mow" (the catalogue trumps the caller's
/// shorthand). `verbal::services` has the more permissive matcher
/// used at detection time; this one is strict-ish for the recompute_mode
/// path.
pub fn match_service<'a>(services: &'a [Service], needle: &str) -> Option<&'a Service> {
    let n = needle.trim().to_lowercase();
    if n.is_empty() {
        return None;
    }
    services.iter().filter(|s| s.active).find(|s| {
        let h = s.name.to_lowercase();
        h == n || h.contains(&n) || n.contains(&h)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{Service, ToolCall};
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

    #[test]
    fn missing_fields_empty_draft_lists_everything_except_address_for_in_shop_service() {
        let d = BookingDraft::default();
        let svc = svc("Haircut", false);
        let m = d.missing_fields(&svc);
        assert!(m.contains(&MissingField::Service));
        assert!(m.contains(&MissingField::Date));
        assert!(m.contains(&MissingField::Time));
        assert!(m.contains(&MissingField::CustomerName));
        assert!(!m.contains(&MissingField::Address));
    }

    #[test]
    fn missing_fields_includes_address_only_when_service_requires_it() {
        let svc = svc("Lawn mow", true);
        let mut d = BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            start_time: Some(NaiveTime::from_hms_opt(14, 0, 0).unwrap()),
            customer_name: Some("Lance".into()),
            address: None,
            notes: None,
        };
        assert_eq!(d.missing_fields(&svc), vec![MissingField::Address]);
        d.address = Some("12 Park Street".into());
        assert!(d.is_complete(&svc));
    }

    #[test]
    fn to_book_call_clamps_phone_from_caller_id() {
        let svc = svc("Lawn mow", true);
        let draft = BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            start_time: Some(NaiveTime::from_hms_opt(14, 0, 0).unwrap()),
            customer_name: Some("Lance".into()),
            address: Some("12 Park Street".into()),
            notes: None,
        };
        let call = draft.to_book_call(&svc, "+61432000111").unwrap();
        match call {
            ToolCall::Book {
                customer_phone,
                address,
                ..
            } => {
                assert_eq!(customer_phone.as_deref(), Some("+61432000111"));
                assert_eq!(address.as_deref(), Some("12 Park Street"));
            }
            _ => panic!("expected Book"),
        }
    }

    #[test]
    fn to_book_call_returns_none_when_address_missing_for_required_service() {
        let svc = svc("Lawn mow", true);
        let draft = BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            start_time: Some(NaiveTime::from_hms_opt(14, 0, 0).unwrap()),
            customer_name: Some("Lance".into()),
            address: None,
            notes: None,
        };
        assert!(draft.to_book_call(&svc, "+61432000111").is_none());
    }

    #[test]
    fn match_service_handles_case_and_partial_overlap() {
        let services = vec![svc("Lawn mow + edges", false), svc("Hedge trim", false)];
        assert_eq!(
            match_service(&services, "lawn mow").unwrap().name,
            "Lawn mow + edges"
        );
        assert_eq!(
            match_service(&services, "LAWN MOW + EDGES").unwrap().name,
            "Lawn mow + edges"
        );
        assert_eq!(
            match_service(&services, "hedge").unwrap().name,
            "Hedge trim"
        );
        assert!(match_service(&services, "haircut").is_none());
    }

    #[test]
    fn recompute_mode_progresses_through_states() {
        let services = vec![svc("Lawn mow", true)];
        let mut s = CallSession::new("c1".into(), "+61432".into(), None);
        s.recompute_mode(&services);
        assert_eq!(s.mode, SessionMode::Idle);

        s.draft_booking = Some(BookingDraft {
            service: Some("Lawn mow".into()),
            ..Default::default()
        });
        s.recompute_mode(&services);
        assert_eq!(s.mode, SessionMode::Gathering);

        s.last_offered_slots = Some(OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![NaiveTime::from_hms_opt(14, 0, 0).unwrap()],
        });
        s.recompute_mode(&services);
        assert_eq!(s.mode, SessionMode::SlotsOffered);

        let draft = s.draft_booking.as_mut().unwrap();
        draft.date = Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap());
        draft.start_time = Some(NaiveTime::from_hms_opt(14, 0, 0).unwrap());
        draft.customer_name = Some("Lance".into());
        draft.address = Some("12 Park St".into());
        s.recompute_mode(&services);
        assert_eq!(s.mode, SessionMode::Confirming);
    }

    #[test]
    fn recompute_mode_keeps_booked_sticky_only_with_no_new_draft() {
        let services = vec![svc("Lawn mow", false)];
        let mut s = CallSession::new("c1".into(), "+61432".into(), None);
        s.mode = SessionMode::Booked;
        // No new draft (just close-out chit-chat) — mode stays Booked.
        s.recompute_mode(&services);
        assert_eq!(s.mode, SessionMode::Booked);
        // A new draft (caller wants another booking) — mode transitions
        // out of Booked so the slim prompt switches to Gathering.
        s.draft_booking = Some(BookingDraft {
            service: Some("Lawn mow".into()),
            ..Default::default()
        });
        s.recompute_mode(&services);
        assert_eq!(s.mode, SessionMode::Gathering);
    }

    #[test]
    fn reset_booking_clears_draft_and_offered_slots() {
        let mut s = CallSession::new("c1".into(), String::new(), None);
        s.draft_booking = Some(BookingDraft {
            service: Some("Lawn mow".into()),
            ..Default::default()
        });
        s.last_offered_slots = Some(OfferedSlots {
            date: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![NaiveTime::from_hms_opt(14, 0, 0).unwrap()],
        });
        s.mode = SessionMode::SlotsOffered;
        s.reset_booking();
        assert!(s.draft_booking.is_none());
        assert!(s.last_offered_slots.is_none());
        assert_eq!(s.mode, SessionMode::Idle);
    }
}
