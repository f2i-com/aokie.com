//! Build the calendar-aware system-prompt suffix and the conversational
//! framing for tool-call interactions. The bot's persona prompt lives
//! in user settings; this module's only job is to add the
//! "you can also book appointments" instructions and any current
//! business-hours / service context the model needs to answer
//! availability questions.
//!
//! Kept narrow on purpose: the suffix is appended once per LLM pass,
//! and the conversation between passes (tool result re-injection) is
//! framed by `tool_result_user_turn`. Larger pipelines would do this
//! via a dedicated chat template; we don't have one, so we build the
//! prompt strings by hand and tell Gemma 4 about the protocol in
//! plain English.

use chrono::Datelike;

use super::session::{CallSession, SessionMode};
use super::{BusinessHours, CalendarConfig, Service};

/// Return the relevance-classifier instruction that must precede every
/// LLM pass for inbound SMS. We make this a separate string because
/// the call-flow doesn't need it (a phone call by definition is a
/// business contact — only SMS gets random spam).
pub fn relevance_classifier_instruction() -> &'static str {
    "BEFORE anything else, decide whether the inbound message is about \
     this business (a question, a booking, a reply to a previous \
     conversation, or anything a real customer or prospect might send). \
     Output a single line:\n\
     \n\
     <RELEVANT>yes</RELEVANT>   — looks like a customer message; continue normally.\n\
     <RELEVANT>no</RELEVANT>    — spam, marketing, sales pitch, scam, wrong-number, \
     or anything unrelated to the business. STOP after this line — do not write a reply.\n\
     \n\
     If unsure, prefer 'yes' so a real customer is never ignored."
}

/// Append the calendar-aware suffix to whatever persona system prompt
/// is configured. Disabled-calendar deployments get nothing extra so
/// the bot's behaviour is unchanged.
///
/// `mode` controls whether the tool-protocol block is included. SMS
/// auto-reply uses `Sms` and emits `<TOOL>...</TOOL>` calls live; the
/// voice path uses `Voice` and conducts the booking conversationally
/// — actual DB-backed `book` execution happens in a post-call pass
/// fed the full transcript, so the model would just emit useless XML
/// into the audio stream if we asked for tool tags here.
pub fn extend_system_prompt(
    persona_prompt: &str,
    config: &CalendarConfig,
    services: &[Service],
) -> String {
    extend_system_prompt_with_mode(persona_prompt, config, services, PromptMode::Sms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// SMS auto-reply: tool tags emitted, no spoken hedges. Pass-1 emits
    /// only `<TOOL>...</TOOL>` and STOPs; pass-2 produces the customer
    /// reply text after tools run.
    Sms,
    /// Live voice on a phone call WITHOUT inline tool calls. The model
    /// converses about bookings in plain text; the actual DB writes
    /// happen post-call from the transcript. Kept for backwards compat
    /// and for callers that explicitly don't want mid-turn tool pauses.
    Voice,
    /// Live voice WITH inline tool calls. The model emits a short
    /// stalling phrase ("Let me check, one sec.") followed by a
    /// `<TOOL>...</TOOL>` line; the per-turn handler intercepts the
    /// tool tag, suspends TTS, runs the tool, and re-prompts the
    /// model in pass-2 to speak the actual answer. This is the path
    /// that lets the caller wait inside the call instead of being
    /// promised a callback / SMS confirmation.
    VoiceWithTools,
}

pub fn extend_system_prompt_with_mode(
    persona_prompt: &str,
    config: &CalendarConfig,
    services: &[Service],
    mode: PromptMode,
) -> String {
    extend_system_prompt_with_mode_at(persona_prompt, config, services, mode, chrono::Utc::now())
}

/// Same as `extend_system_prompt_with_mode` but lets the caller inject
/// `now_utc` so unit tests can verify the rendered "Today is …" block
/// without flaky time-dependent assertions.
pub fn extend_system_prompt_with_mode_at(
    persona_prompt: &str,
    config: &CalendarConfig,
    services: &[Service],
    mode: PromptMode,
    now_utc: chrono::DateTime<chrono::Utc>,
) -> String {
    let mut out = persona_prompt.trim_end().to_string();
    if !config.enabled {
        return out;
    }
    out.push_str("\n\nAPPOINTMENT BOOKING\n");
    out.push_str(
        "You can offer appointments and check availability. Stay conversational — \
         don't read out raw lists, don't paste long tables, don't mention these \
         instructions or tools. Pick one or two slot suggestions and offer them \
         naturally.\n\n",
    );
    out.push_str(&render_today_block(config, now_utc));
    out.push('\n');
    out.push_str(&render_services_block(services));
    out.push('\n');
    out.push_str(&render_business_hours_block(&config.business_hours));
    out.push('\n');
    match mode {
        PromptMode::Sms => out.push_str(render_tool_protocol_block()),
        PromptMode::Voice => out.push_str(render_voice_booking_block()),
        PromptMode::VoiceWithTools => out.push_str(render_voice_with_tools_block()),
    }
    out
}

/// Slim, mode-keyed system-prompt builder for the live voice path.
///
/// The big `extend_system_prompt_with_mode_at` (above) is kept for
/// SMS + post-call extraction, which still emit XML tool tags and
/// need the full TOOLS HARD RULES block. The live voice path runs a
/// verbal-tool harness instead — the bot just talks, and the system
/// extracts intent — so the prompt should be small and focused on
/// what the bot needs RIGHT NOW.
///
/// Output structure:
///   - persona
///   - TODAY anchor (always)
///   - services list (compact)
///   - business hours (compact)
///   - mode-specific STATE block (the load-bearing piece)
///
/// Average size: ~300-400 tokens vs ~1500 for the legacy path.
pub fn build_voice_system_prompt(
    persona_prompt: &str,
    config: &CalendarConfig,
    services: &[Service],
    session: &CallSession,
    now_utc: chrono::DateTime<chrono::Utc>,
) -> String {
    let mut out = persona_prompt.trim_end().to_string();
    if !config.enabled {
        return out;
    }
    out.push_str("\n\nAPPOINTMENT BOOKING\n");
    out.push_str(
        "You can offer appointments. Stay conversational and natural — \
         don't read out lists or paste tables. Use plain spoken English; \
         times in 12-hour am/pm format. Don't promise to text or call \
         the caller back later — this call is the only contact.\n\n",
    );
    out.push_str(&render_today_block(config, now_utc));
    out.push('\n');
    out.push_str(&render_services_block(services));
    out.push('\n');
    out.push_str(&render_business_hours_block(&config.business_hours));
    out.push('\n');
    out.push_str(&render_state_block(session));
    out
}

fn render_state_block(session: &CallSession) -> String {
    match session.mode {
        SessionMode::Idle => {
            "STATE: no active booking. If the caller wants to book, ask what day works \
             for them and what service they need. If they name a day, you don't need \
             to ask again — just say you're checking and the system handles the rest.\n"
                .to_string()
        }
        SessionMode::Gathering => render_gathering_state(session),
        SessionMode::SlotsOffered => render_slots_offered_state(session),
        SessionMode::Confirming => render_confirming_state(session),
        SessionMode::Booked => {
            "STATE: booking is confirmed and saved. Confirm the details once briefly \
             (\"you're booked for ...\"), then it's fine to wrap up the call. Don't \
             offer to text or email — this call is the only contact.\n"
                .to_string()
        }
        SessionMode::OrderGathering => render_order_gathering_state(session),
        SessionMode::OrderPlaced => {
            "STATE: order is placed. Confirm the total and pickup/delivery details \
             briefly, then wrap up.\n"
                .to_string()
        }
    }
}

fn render_gathering_state(session: &CallSession) -> String {
    let draft = match session.draft_booking.as_ref() {
        Some(d) => d,
        None => return String::new(),
    };
    let has_service = draft.service.is_some();
    let has_date = draft.date.is_some();
    let has_time = draft.start_time.is_some();
    let has_name = draft.customer_name.is_some();
    let has_address = draft.address.is_some();

    let mut s = String::from("STATE: caller is mid-booking. So far you have:\n");
    match draft.service.as_deref() {
        Some(svc) => s.push_str(&format!("- service: {}\n", svc)),
        None => s.push_str("- service: (not yet given)\n"),
    }
    match draft.date {
        Some(date) => s.push_str(&format!("- date: {}\n", date.format("%A %-d %B %Y"))),
        None => s.push_str("- date: (not yet given)\n"),
    }
    if let Some(time) = draft.start_time {
        s.push_str(&format!("- time: {}\n", format_time_12h(time)));
    }
    if let Some(name) = draft.customer_name.as_deref() {
        s.push_str(&format!("- name: {}\n", name));
    }
    if let Some(addr) = draft.address.as_deref() {
        s.push_str(&format!("- address: {}\n", addr));
    }
    s.push('\n');

    // The "next step" branches by what we still need. Order matters —
    // service first (anchors the conversation), then date (drives
    // availability), then time (slot pick), then name + address.
    if !has_service && !has_date {
        s.push_str(
            "Next step: ask the caller what they'd like to book and what day works. \
             One short question covering both.\n",
        );
    } else if !has_service {
        s.push_str(
            "Next step: ask the caller what service they need. Don't restate the \
             date — they've already named one. One short question.\n",
        );
    } else if !has_date {
        s.push_str(
            "Next step: ask the caller what day works for them. Don't list services \
             — they've already named one. One short question.\n",
        );
    } else if !has_time {
        s.push_str(
            "Next step: tell the caller you're checking availability for the date \
             and service above. Don't restate the date back to them. As soon as \
             you say you're checking, the system runs availability and re-prompts \
             you with the open slots.\n",
        );
    } else if !has_name {
        s.push_str(
            "Next step: ask the caller for their name. Don't restate the slot — \
             they just picked it. One short question.\n",
        );
    } else if !has_address {
        s.push_str(
            "Next step: ask the caller for their street address — the service \
             above happens at their place. Read back the slot details only if it \
             helps the conversation.\n",
        );
    } else {
        // All fields populated but is_complete() false (shouldn't
        // happen — recompute_mode would have set Confirming). Fall
        // through to a generic "read it back".
        s.push_str("Next step: read the booking details back to confirm.\n");
    }
    s
}

fn render_slots_offered_state(session: &CallSession) -> String {
    let offered = match session.last_offered_slots.as_ref() {
        Some(o) => o,
        None => return String::new(),
    };
    let mut s = String::from("STATE: open slots for ");
    if !offered.service.is_empty() {
        s.push_str(&offered.service);
        s.push_str(" on ");
    }
    s.push_str(&offered.date.format("%A %-d %B").to_string());
    s.push_str(":\n");
    for slot in offered.slots.iter().take(6) {
        s.push_str(&format!("- {}\n", format_time_12h(*slot)));
    }
    if offered.slots.len() > 6 {
        s.push_str(&format!("(plus {} more)\n", offered.slots.len() - 6));
    }
    s.push_str(
        "\nNext step: offer one or two of these times naturally. Don't list more \
         than two. If the caller picks a time, the system records it and then \
         asks for their name (and street address if the service needs it).\n",
    );
    s
}

fn render_confirming_state(session: &CallSession) -> String {
    let draft = match session.draft_booking.as_ref() {
        Some(d) => d,
        None => return String::new(),
    };
    let mut s = String::from("STATE: ready to book. Details:\n");
    if let Some(svc) = draft.service.as_deref() {
        s.push_str(&format!("- service: {}\n", svc));
    }
    if let (Some(date), Some(time)) = (draft.date, draft.start_time) {
        s.push_str(&format!(
            "- when: {} at {}\n",
            date.format("%A %-d %B %Y"),
            format_time_12h(time),
        ));
    }
    if let Some(name) = draft.customer_name.as_deref() {
        s.push_str(&format!("- name: {}\n", name));
    }
    if let Some(addr) = draft.address.as_deref() {
        s.push_str(&format!("- address: {}\n", addr));
    }
    s.push_str(
        "\nNext step: read these details back to the caller and ask if they're \
         right. When the caller confirms, say something like \"locking that in\" \
         and the system books it. DON'T say \"booked\" or \"all set\" until \
         after the caller confirms AND the system reports the booking succeeded.\n",
    );
    s
}

fn render_order_gathering_state(session: &CallSession) -> String {
    let draft = match session.draft_order.as_ref() {
        Some(d) => d,
        None => return String::new(),
    };
    let mut s = String::from("STATE: caller is placing an order. So far:\n");
    if draft.items.is_empty() {
        s.push_str("- (no items yet)\n");
    } else {
        for item in &draft.items {
            s.push_str(&format!("- {}× {}\n", item.quantity, item.product));
        }
    }
    if let Some(name) = draft.customer_name.as_deref() {
        s.push_str(&format!("- customer name: {}\n", name));
    } else {
        s.push_str("- customer name: (not yet given)\n");
    }
    s.push_str(
        "\nNext step: ask for any missing info (items, name) and read the order \
         back before placing.\n",
    );
    s
}

fn format_time_12h(t: chrono::NaiveTime) -> String {
    use chrono::Timelike;
    let hour = t.hour();
    let minute = t.minute();
    let (h12, am) = if hour == 0 {
        (12, true)
    } else if hour < 12 {
        (hour, true)
    } else if hour == 12 {
        (12, false)
    } else {
        (hour - 12, false)
    };
    let suffix = if am { "am" } else { "pm" };
    if minute == 0 {
        format!("{}{}", h12, suffix)
    } else {
        format!("{}:{:02}{}", h12, minute, suffix)
    }
}

/// Render the "Today is …" anchor that lets the model resolve relative
/// dates the caller mentions ("tomorrow", "Friday", "next week"). The
/// time is included so the bot can answer "are you open right now?"
/// against the opening-hours block without guessing.
///
/// The timezone comes from `config.timezone` (e.g. "Australia/Sydney")
/// and is canonicalised through chrono-tz so DST is correct on edge
/// dates. An invalid timezone string falls back to UTC with a note —
/// better to ground the bot in *some* concrete time than to skip the
/// block and let it confabulate.
fn render_today_block(config: &CalendarConfig, now_utc: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Datelike as _;
    let (now, tz_label): (chrono::DateTime<chrono_tz::Tz>, String) =
        match config.timezone.parse::<chrono_tz::Tz>() {
            Ok(tz) => (now_utc.with_timezone(&tz), config.timezone.clone()),
            Err(_) => (
                now_utc.with_timezone(&chrono_tz::UTC),
                format!(
                    "UTC (configured timezone {:?} is not a recognised IANA zone)",
                    config.timezone
                ),
            ),
        };
    format!(
        "TODAY: {}, {} {} {} (local time {}, {}). \
         Use this when the caller mentions \"today\", \"tomorrow\", \
         \"this Friday\", \"next week\", etc. — convert relative phrases \
         to absolute YYYY-MM-DD dates against this anchor before emitting \
         a tool call.\n",
        now.format("%A"),
        now.day(),
        now.format("%B"),
        now.year(),
        now.format("%H:%M"),
        tz_label,
    )
}

fn render_voice_booking_block() -> &'static str {
    // R9/7: explicit "no tool round-trip in this mode → don't promise
    // success" rule. The in-call tool path (VoiceWithTools) already
    // gates "booked" / "all set" on a TOOL_RESULTS line, so the bot
    // can't claim a booking succeeded until the DB write has actually
    // happened. This mode (no tools) records the booking via post-call
    // extraction — that pass can fail (the slot got taken between the
    // call ending and the extraction running, an opening-hours edit
    // landed mid-call, the LLM mis-parsed the timing). Saying "you're
    // booked in" before that pass succeeds is a trust footgun the
    // reviewer (R9/7) flagged: caller hangs up believing they have a
    // confirmed slot, the DB rejects the slot, the operator finds out
    // via `post-call-booking-failed` and has to follow up. The phrasing
    // rules below stop the bot from over-promising in the first place.
    "BOOKINGS DURING THIS CALL\n\
     Collect bookings conversationally — do NOT emit XML or tool tags. \
     If the caller wants to book, gather: service, day + time (within opening hours), \
     their name, and confirm their phone number (you already have it from caller-ID). \
     For services prefixed with [NEEDS ADDRESS] in the Services block, also \
     ask for and confirm the caller's street address — that's where the work \
     happens. Skip the address question for services without that prefix \
     (in-shop work). \
     Read the details back to confirm. The actual booking is recorded after the \
     call ends.\n\
     \n\
     IMPORTANT — DON'T OVER-PROMISE\n\
     Because the booking happens after the call, you DON'T know yet if \
     the slot will land in the calendar (another booking might land \
     first, opening hours could change, etc.). FORBIDDEN phrases:\n\
     - \"You're booked in\" / \"You're booked\" / \"All booked\"\n\
     - \"Confirmed\" / \"All set\" / \"Locked in\"\n\
     - \"I've got you down\" / \"You're in the calendar\"\n\
     INSTEAD say something like:\n\
     - \"I'll pass that booking request through and we'll send you a \
       confirmation if anything changes.\"\n\
     - \"I've got that down for [day] at [time]. We'll be in touch if \
       there's any clash.\"\n\
     - \"That's noted for [day at time], thanks.\"\n\
     The caller should hang up understanding the booking has been \
     RECEIVED, not that it's been GUARANTEED. The operator confirms the \
     slot post-call (and reaches out to the caller manually if it didn't \
     land)."
}

fn render_voice_with_tools_block() -> &'static str {
    "TOOLS — HARD RULES\n\
     This is the single most important section. Read every rule.\n\
     \n\
     RULE 0 — NEVER promise a future message, callback, or text.\n\
     This call is the ONLY contact. There is no SMS, no email, no \
     callback queue. The bot has no way to reach the caller after they \
     hang up. So these phrases are FORBIDDEN — never say them:\n\
     - \"I'll get back to you\" / \"I'll get back to ya\"\n\
     - \"I'll text you the details\" / \"I'll send you a text\"\n\
     - \"I'll call you back\" / \"I'll ring you back\"\n\
     - \"I'll send a confirmation\" / \"I'll let you know\"\n\
     - \"I'll check and reach out later\"\n\
     If you need information you don't have, run a tool RIGHT NOW (see \
     RULE 1 + 2). The answer must come this turn — there is no later.\n\
     \n\
     RULE 1 — A stalling phrase REQUIRES a tool tag, same turn.\n\
     If you say \"let me check\", \"one sec\", \"let me look that up\", \
     \"give me a moment\", or anything similar, the VERY NEXT THING you \
     output MUST be a <TOOL>...</TOOL> tag, on a new line, then STOP. \
     Skipping the tool tag is a BUG — the caller hears your stall and \
     then dead air, and the call ends with nothing booked. Don't ask \
     another question, don't acknowledge, don't pause. Stalling phrase, \
     newline, tool tag, end. If you're not ready to emit a tool tag, \
     don't stall — answer with what you know, or ask the caller a \
     specific question.\n\
     \n\
     RULE 2 — A weekday name OR relative date COUNTS as a date. Emit immediately.\n\
     If the caller names ANY of these, you have a date — call \
     check_availability NOW, do not ask \"what day?\" again:\n\
     - a weekday: \"Friday\", \"Monday\", \"Tues\", \"this Saturday\"\n\
     - a relative day: \"today\", \"tomorrow\", \"next week\", \"the day after\"\n\
     - a calendar date: \"the 12th\", \"May 8\", \"8 May\"\n\
     Resolve the phrase to YYYY-MM-DD against TODAY (above) and emit the \
     tool tag. Don't ask for a time first — check_availability shows you \
     what's free; you offer a slot AFTER the result comes back.\n\
     \n\
     RULE 2B — Only ASK when timing is completely missing.\n\
     If the caller said something like \"I'd like to make an \
     appointment\" or \"can I book something?\" with NO weekday, no \
     relative day, no date — just a bare request — your reply is one \
     short question: \"Sure, what day works for you?\" — no tool call, \
     no stalling phrase. The moment they name a day, RULE 2 takes over.\n\
     \n\
     RULE 3 — One tool per turn, then stop.\n\
     After </TOOL> the system runs the tool and prompts you again with \
     the result. Don't keep talking after </TOOL>.\n\
     \n\
     WHEN EACH TOOL FIRES\n\
     - Caller named a date or relative day → check_availability\n\
     - You have a slot + caller's name + (if [NEEDS ADDRESS]) address \
       → book\n\
     - Caller wants to view / cancel / reschedule → my_appointments\n\
     - Caller asks about price / hours / services → answer from the \
       blocks above, no tool needed.\n\
     \n\
     EXAMPLES (these are the only allowed shapes for tool turns)\n\
     Caller: \"Are you free Friday at 2 for a lawn mow?\"\n\
     You: One sec, let me check.\n\
     <TOOL>check_availability date=2026-04-28 service=\"Lawn mow\"</TOOL>\n\
     \n\
     Caller: \"I'd like to book a lawn mow for Friday.\"  (just the day, no time)\n\
     You: Let me check Friday.\n\
     <TOOL>check_availability date=2026-04-28 service=\"Lawn mow\"</TOOL>\n\
     \n\
     Caller: \"Can you do tomorrow?\"\n\
     You: Let me check tomorrow.\n\
     <TOOL>check_availability date=2026-05-07 service=\"Lawn mow\"</TOOL>\n\
     \n\
     Caller: \"Friday 2pm works. I'm Lance, 12 Park Street.\"\n\
     You: Locking that in.\n\
     <TOOL>book service=\"Lawn mow\" start=2026-04-28T14:00 customer_name=\"Lance\" address=\"12 Park Street\"</TOOL>\n\
     \n\
     Caller: \"What appointments do I have?\"\n\
     You: Let me pull that up.\n\
     <TOOL>my_appointments customer_phone=\"+61432000111\"</TOOL>\n\
     \n\
     COUNTER-EXAMPLES (these are WRONG — never produce a turn like this):\n\
     Caller: \"I'd like to book a lawn mow for Friday.\"\n\
     You: \"Sure, what day works for you?\" ← BUG. They said Friday — \
     that's the day. Emit check_availability instead.\n\
     \n\
     You: \"Got it. Let me check the availability for today. One second.\" \
     [no tool tag follows] ← BUG. Either emit the tool tag or don't \
     promise to check.\n\
     \n\
     OUTPUT-LEVEL CONSTRAINTS\n\
     - Don't say \"booked\", \"confirmed\", \"all set\", \"locked in\", or \
       \"I've got you down\" unless the previous TOOL_RESULTS line says \
       \"1 (or more) appointment(s) booked\".\n\
     - Don't claim a day is full / busy / free unless you just got a \
       fresh check_availability result for THAT date.\n\
     - Don't invent the caller's name — ask if it's missing.\n\
     - Phone is from caller-ID; don't ask, don't include customer_phone= \
       in `book`.\n\
     - Skip the address question for services without [NEEDS ADDRESS].\n\
     \n\
     ALL TOOL SHAPES\n\
     <TOOL>check_availability date=YYYY-MM-DD service=\"Service Name\"</TOOL>\n\
     <TOOL>book service=\"Service Name\" start=YYYY-MM-DDTHH:MM customer_name=\"Full Name\" address=\"street (only when [NEEDS ADDRESS])\" notes=\"optional\"</TOOL>\n\
     <TOOL>list_services</TOOL>\n\
     <TOOL>my_appointments customer_phone=\"+61432...\"</TOOL>"
}

fn render_services_block(services: &[Service]) -> String {
    let active: Vec<&Service> = services.iter().filter(|s| s.active).collect();
    if active.is_empty() {
        return "Services: none configured yet — if a customer asks for a booking \
                tell them you'll get someone to call back to confirm details.\n"
            .to_string();
    }
    let mut s = String::from("Services on offer:\n");
    let mut any_requires_address = false;
    let mut address_required_names: Vec<&str> = Vec::new();
    for svc in active {
        // [NEEDS ADDRESS] goes at the FRONT of the line so the model
        // sees the requirement before the service name. Gemma 4 was
        // ignoring a trailing tag and skipping the address question
        // mid-conversation — moving it leftward makes the requirement
        // load-bearing on every read of the services block.
        if svc.requires_address {
            s.push_str("- [NEEDS ADDRESS] ");
            any_requires_address = true;
            address_required_names.push(&svc.name);
        } else {
            s.push_str("- ");
        }
        s.push_str(&format!("{} ({} min)", svc.name, svc.duration_minutes));
        if let Some(desc) = svc.description.as_deref() {
            if !desc.trim().is_empty() {
                s.push_str(&format!(" — {}", desc.trim()));
            }
        }
        s.push('\n');
    }
    if any_requires_address {
        s.push_str(&format!(
            "ADDRESS REQUIRED for: {}. These are off-site services — you MUST \
             ask the caller for their street address aloud during the call AND \
             pass it as address=\"...\" in the book tool call. Booking without an \
             address bounces with an error and the operator has to chase the \
             customer afterwards.\n",
            address_required_names.join(", "),
        ));
    }
    s
}

fn render_business_hours_block(hours: &BusinessHours) -> String {
    let mut s = String::from("Opening hours:\n");
    let days = [
        ("Monday", &hours.monday),
        ("Tuesday", &hours.tuesday),
        ("Wednesday", &hours.wednesday),
        ("Thursday", &hours.thursday),
        ("Friday", &hours.friday),
        ("Saturday", &hours.saturday),
        ("Sunday", &hours.sunday),
    ];
    for (label, dh) in days {
        if !dh.open {
            s.push_str(&format!("- {}: closed\n", label));
            continue;
        }
        match &dh.lunch {
            Some(br) => s.push_str(&format!(
                "- {}: {}–{} (lunch {}–{})\n",
                label, dh.start, dh.end, br.start, br.end
            )),
            None => s.push_str(&format!("- {}: {}–{}\n", label, dh.start, dh.end)),
        }
    }
    s
}

fn render_tool_protocol_block() -> &'static str {
    "TOOLS\n\
     The system runs tool calls for you. In pass-1, emit ONE tool tag and \
     nothing else — no \"let me check\" hedge, no chatter. The system runs \
     the tool and re-prompts you in pass-2 to write the SMS reply.\n\
     \n\
     WHEN TO CALL EACH TOOL\n\
     - Customer asks about availability or names a date → check_availability\n\
     - You have a chosen slot AND the customer's name AND (if [NEEDS ADDRESS] \
       service) their street address → book\n\
     - Customer asks about THEIR appointments, wants to cancel, or wants \
       to reschedule → my_appointments\n\
     - Customer chats about price, hours, or services → reply normally in \
       plain text, no tool call.\n\
     \n\
     EXAMPLES\n\
     Customer: \"Can I book a lawn mow for Friday at 2?\"\n\
     Pass-1: <TOOL>check_availability date=2026-04-29 service=\"Lawn mow\"</TOOL>\n\
     Pass-2 (after results): \"Friday 2pm works! Could I grab your name \
     and street address to lock it in?\"\n\
     \n\
     Customer: \"Lance Smith, 12 Park Street.\"\n\
     Pass-1: <TOOL>book service=\"Lawn mow\" start=2026-04-29T14:00 customer_name=\"Lance Smith\" address=\"12 Park Street\"</TOOL>\n\
     Pass-2 (after a successful booking): \"All booked, Lance — see you \
     Friday at 2pm.\"\n\
     \n\
     IMPORTANT\n\
     - Don't say \"booked\", \"confirmed\", \"all set\", \"sorted\", \"locked \
       in\", or \"I've got you down\" UNLESS the previous TOOL_RESULTS line \
       says \"1 (or more) appointment(s) booked\".\n\
     - Don't claim a day is full / busy / no openings unless you just got \
       a fresh check_availability for THAT date. New date → new tool call.\n\
     - Don't make up the customer's name. If you didn't catch it, ASK in \
       pass-2: \"Sorry, what name should I put it under?\"\n\
     - The customer's phone is already known (SMS sender) — don't ask, and \
       don't include customer_phone= in `book`. The system fills it in.\n\
     - Skip the address question for services NOT tagged [NEEDS ADDRESS].\n\
     \n\
     ALL TOOLS\n\
     <TOOL>check_availability date=YYYY-MM-DD service=\"Service Name\"</TOOL>\n\
     <TOOL>book service=\"Service Name\" start=YYYY-MM-DDTHH:MM customer_name=\"Full Name\" address=\"street address (only when [NEEDS ADDRESS])\" notes=\"optional\"</TOOL>\n\
     <TOOL>list_services</TOOL>\n\
     <TOOL>my_appointments customer_phone=\"+61432...\"</TOOL>\n\
     \n\
     Use double-quotes for values with spaces. Dates YYYY-MM-DD, times \
     HH:MM 24-hour, date+time pairs YYYY-MM-DDTHH:MM."
}

/// Frame a tool-execution result as the next user-role turn, so the
/// model sees it as system-provided context rather than as something
/// the customer typed. Using "user" role rather than a system role
/// simplifies the chat-template handling — Gemma's template only
/// alternates user/model after the leading system prompt.
///
/// `rendered_results` is expected to start with an `ACTIONS THIS TURN: ...`
/// summary line built by the caller (see `spawn_sms_auto_reply` in
/// `commands::bluetooth_commands`). The summary is the model's source
/// of truth for what actually happened — Gemma 4 confabulates
/// "I've got you down for Tuesday 2pm" off a check_availability
/// result that didn't book anything, so the prompt below makes the
/// summary load-bearing rather than relying on the model to read
/// bracketed tags out of the rendered tool output.
///
/// The prompt also forbids hedges like "let me check" / "I'll get
/// back to you" — Gemma 4 will otherwise stay consistent with any
/// such phrasing the model emitted alongside the tool call in pass-1
/// (see `extract_tool_blocks` in calendar::tools for the other half
/// of the fix). The architecture is one-shot: pass-2's reply IS the
/// SMS, there's no follow-up message pipeline.
pub fn tool_result_user_turn(rendered_results: &str, today_local: chrono::NaiveDate) -> String {
    format!(
        "TOOL_RESULTS (don't quote this raw to the customer):\n\
         {}\n\
         \n\
         Today is {} ({}). Now write the SMS reply in plain conversational \
         English — short, friendly, no brackets / JSON / tool tags.\n\
         \n\
         The ACTIONS THIS TURN line at the top is the source of truth. \
         Match its summary:\n\
         - \"0 appointment(s) booked\" → nothing is booked yet. Offer the \
           slot, ask for the name. Don't say \"booked\" / \"confirmed\".\n\
         - \"1 (or more) appointment(s) booked\" → the booking IS done. \
           Confirm name, service, day, and time.\n\
         - \"booking attempt(s) failed\" non-zero → apologise and offer an \
           alternative slot from the availability list.\n\
         - \"booking(s) waiting on customer's address\" non-zero → find the \
           [book PAUSED ...] line and ask the customer for their street \
           address. Don't apologise, don't offer a different slot.\n\
         - \"[tool error]\" line OR \"0 availability check(s)\" with no \
           slots → tell the customer briefly what went wrong (e.g. \"I \
           couldn't find that service\") and use the configured service \
           names verbatim from the Services block. Don't invent slots.\n\
         \n\
         Time format: convert tool times like \"14:00\" to \"2pm\" (12-hour, \
         am/pm). Never write \"14:00\" in the SMS.\n\
         \n\
         If the customer named a specific time AND it's in the availability \
         list, confirm just that slot and ask for the name — don't dump \
         every slot back at them.",
        rendered_results,
        today_local.format("%A %-d %B %Y"),
        today_local.weekday(),
    )
}

/// Voice-flavored sibling of `tool_result_user_turn`. Same intent — feed
/// the model the tool results as the next user turn — but the reply is
/// going through TTS to a live caller mid-call, so the language must
/// be spoken-style: short sentences, no list-formatting, no \"see SMS\"
/// references. The ACTIONS-THIS-TURN guard against confabulated
/// bookings is preserved.
pub fn tool_result_user_turn_voice(
    rendered_results: &str,
    today_local: chrono::NaiveDate,
) -> String {
    format!(
        "TOOL_RESULTS (don't read the raw brackets aloud):\n\
         {}\n\
         \n\
         Today is {} ({}). Now speak the reply in one or two short \
         conversational sentences — no list formatting, no brackets, no \
         tool tags. You already said \"let me check\" before the tool ran; \
         don't repeat it.\n\
         \n\
         The ACTIONS THIS TURN line is the source of truth. Match it:\n\
         - \"0 appointment(s) booked\" → not booked yet. Offer the slot \
           and ask for the name. Don't say \"booked\" / \"confirmed\".\n\
         - \"1 (or more) appointment(s) booked\" → it's done. Confirm name, \
           service, day, and time briefly.\n\
         - \"booking attempt(s) failed\" non-zero → apologise and offer a \
           different slot from the availability list.\n\
         - \"booking(s) waiting on caller's address\" non-zero → find the \
           [book PAUSED ...] line and ask the caller for their street \
           address. Don't apologise, don't offer a different time.\n\
         - \"[tool error]\" line OR \"0 availability check(s)\" with no \
           slots → tell the caller briefly what went wrong, use the \
           configured service names verbatim. Don't invent slots.\n\
         \n\
         Speak times as 12-hour am/pm (\"2pm\", \"9am\", \"noon\"). Never \
         say \"14:00\" or \"thirteen hundred\".\n\
         \n\
         If the caller named a specific time AND it's available, confirm \
         that slot and ask for the name — don't list every slot back.",
        rendered_results,
        today_local.format("%A %-d %B %Y"),
        today_local.weekday(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{DayHours, TimeRange};

    fn cfg(enabled: bool) -> CalendarConfig {
        CalendarConfig {
            enabled,
            timezone: "Australia/Sydney".to_string(),
            buffer_minutes: 15,
            business_hours: BusinessHours {
                monday: DayHours {
                    open: true,
                    start: "09:00".into(),
                    end: "17:00".into(),
                    lunch: Some(TimeRange {
                        start: "12:00".into(),
                        end: "13:00".into(),
                    }),
                },
                tuesday: DayHours {
                    open: true,
                    start: "09:00".into(),
                    end: "17:00".into(),
                    lunch: None,
                },
                wednesday: DayHours {
                    open: false,
                    start: "09:00".into(),
                    end: "17:00".into(),
                    lunch: None,
                },
                thursday: DayHours {
                    open: true,
                    start: "09:00".into(),
                    end: "17:00".into(),
                    lunch: None,
                },
                friday: DayHours {
                    open: true,
                    start: "09:00".into(),
                    end: "17:00".into(),
                    lunch: None,
                },
                saturday: DayHours {
                    open: false,
                    start: "09:00".into(),
                    end: "17:00".into(),
                    lunch: None,
                },
                sunday: DayHours {
                    open: false,
                    start: "09:00".into(),
                    end: "17:00".into(),
                    lunch: None,
                },
            },
        }
    }

    #[test]
    fn disabled_calendar_returns_persona_prompt_unchanged() {
        let result = extend_system_prompt("You are a receptionist.", &cfg(false), &[]);
        assert_eq!(result, "You are a receptionist.");
    }

    #[test]
    fn enabled_calendar_appends_services_hours_and_protocol() {
        let svc = Service {
            id: 1,
            name: "Lawn mow".into(),
            duration_minutes: 30,
            description: Some("standard mow + edges".into()),
            active: true,
            sort_order: 0,
            created_at: 0,
            requires_address: false,
        };
        let result = extend_system_prompt("You are a receptionist.", &cfg(true), &[svc]);
        assert!(result.contains("APPOINTMENT BOOKING"));
        assert!(result.contains("Lawn mow (30 min)"));
        assert!(result.contains("standard mow + edges"));
        assert!(result.contains("Monday: 09:00–17:00 (lunch 12:00–13:00)"));
        assert!(result.contains("Wednesday: closed"));
        assert!(result.contains("<TOOL>check_availability"));
        assert!(result.contains("<TOOL>book"));
    }

    #[test]
    fn no_services_configured_falls_back_to_callback_phrase() {
        let result = extend_system_prompt("Hi.", &cfg(true), &[]);
        assert!(result.contains("none configured yet"));
        assert!(result.contains("call back to confirm"));
    }

    #[test]
    fn relevance_classifier_text_includes_both_tag_options() {
        let txt = relevance_classifier_instruction();
        assert!(txt.contains("<RELEVANT>yes</RELEVANT>"));
        assert!(txt.contains("<RELEVANT>no</RELEVANT>"));
    }

    #[test]
    fn voice_with_tools_mode_emits_tool_grammar_and_stalling_phrase() {
        let svc = Service {
            id: 1,
            name: "Lawn mow".into(),
            duration_minutes: 30,
            description: None,
            active: true,
            sort_order: 0,
            created_at: 0,
            requires_address: false,
        };
        let result = extend_system_prompt_with_mode(
            "You are a receptionist.",
            &cfg(true),
            &[svc],
            PromptMode::VoiceWithTools,
        );
        // Must keep the shared APPOINTMENT BOOKING preamble + services + hours.
        assert!(result.contains("APPOINTMENT BOOKING"));
        assert!(result.contains("Lawn mow (30 min)"));
        // Voice-specific tool block sub-pieces.
        assert!(result.contains("TOOLS — HARD RULES"));
        assert!(result.contains("stalling phrase"));
        assert!(result.contains("<TOOL>check_availability"));
        assert!(result.contains("<TOOL>book"));
        // RULE 2 must affirm that a weekday name counts as a date —
        // earlier wording ("Don't assume today") was making Qwen3.5 4B
        // ask "what day works?" even after the caller said "Friday".
        assert!(result.contains("weekday name OR relative date COUNTS as a date"));
        // RULE 0 forbids promising a future message/callback — the
        // voice flow has no post-call text channel and "I'll get back
        // to you" creates dead air at the end of the call.
        assert!(result.contains("\"I'll get back to you\""));
        assert!(result.contains("This call is the ONLY contact"));
        // Must NOT contain SMS-specific wording about hedges leaking
        // into pass-2 — that prompt is for SMS only.
        assert!(!result.contains("Drafting a hedge now leaks"));
    }

    #[test]
    fn voice_no_tools_mode_keeps_legacy_block() {
        let result = extend_system_prompt_with_mode(
            "You are a receptionist.",
            &cfg(true),
            &[],
            PromptMode::Voice,
        );
        assert!(result.contains("BOOKINGS DURING THIS CALL"));
        assert!(result.contains("do NOT emit XML or tool tags"));
        assert!(!result.contains("<TOOL>check_availability"));
    }

    #[test]
    fn today_block_renders_date_day_and_time_in_local_tz() {
        // 2026-05-06 12:34:56 UTC = 2026-05-06 22:34:56 +10 in
        // Australia/Sydney (AEST in May, no DST). The system prompt
        // must surface the local weekday + date so the model can
        // resolve "tomorrow" / "Friday" / "next week" without guessing.
        let fixed: chrono::DateTime<chrono::Utc> =
            chrono::DateTime::parse_from_rfc3339("2026-05-06T12:34:56Z")
                .unwrap()
                .with_timezone(&chrono::Utc);
        let result = extend_system_prompt_with_mode_at(
            "You are a receptionist.",
            &cfg(true),
            &[],
            PromptMode::Sms,
            fixed,
        );
        assert!(
            result.contains("TODAY: Wednesday"),
            "missing local weekday: {}",
            result,
        );
        assert!(result.contains("6 May 2026"), "missing date: {}", result);
        assert!(
            result.contains("22:34"),
            "missing local time (Sydney is UTC+10 in May): {}",
            result,
        );
        assert!(
            result.contains("Australia/Sydney"),
            "missing tz label: {}",
            result,
        );
        // The UTC instant string must NOT leak into the rendered prompt
        // — operator timezones are everywhere from Sydney to Tijuana
        // and the bot needs the *operator's* local clock, not UTC.
        assert!(!result.contains("12:34"));
    }

    #[test]
    fn today_block_falls_back_to_utc_for_invalid_timezone() {
        // Defensive: if `config.timezone` is garbage, the bot still
        // gets a concrete time anchor — UTC — instead of a missing
        // block that would let it confabulate dates.
        let mut bad_cfg = cfg(true);
        bad_cfg.timezone = "Mars/Olympus_Mons".to_string();
        let fixed: chrono::DateTime<chrono::Utc> =
            chrono::DateTime::parse_from_rfc3339("2026-05-06T12:34:56Z")
                .unwrap()
                .with_timezone(&chrono::Utc);
        let result =
            extend_system_prompt_with_mode_at("Hi.", &bad_cfg, &[], PromptMode::Sms, fixed);
        assert!(result.contains("TODAY: Wednesday"));
        assert!(result.contains("6 May 2026"));
        // UTC time is the fallback, not the bogus timezone's "local".
        assert!(result.contains("12:34"));
        assert!(result.contains("UTC"));
    }

    #[test]
    fn voice_tool_result_user_turn_speaks_aloud() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 4, 28).unwrap();
        let txt = tool_result_user_turn_voice(
            "ACTIONS THIS TURN: 0 appointment(s) booked, 0 booking attempt(s) failed, \
             1 availability check(s), 0 order(s) placed, 0 order(s) cancelled.\n\n\
             [availability for Lawn mow on 2026-04-28]\n- 14:00\n- 14:30",
            date,
        );
        // Voice-specific phrasing — must direct the model to speak, not
        // write an SMS.
        assert!(txt.contains("speak the reply"));
        assert!(!txt.contains("SMS we send"));
        assert!(!txt.contains("SMS reply"));
        // Confabulation guard preserved (zero-bookings → don't say
        // "booked" / "confirmed").
        assert!(txt.contains("0 appointment(s) booked"));
        assert!(txt.contains("not booked yet"));
        assert!(txt.contains("Don't say \"booked\""));
    }

    fn fixed_now_utc() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-05-06T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn lawn_svc() -> Service {
        Service {
            id: 1,
            name: "Lawn mow".into(),
            duration_minutes: 30,
            description: None,
            active: true,
            sort_order: 0,
            created_at: 0,
            requires_address: true,
        }
    }

    #[test]
    fn slim_voice_prompt_idle_mode_has_no_tool_grammar() {
        use crate::calendar::CallSession;
        let session = CallSession::new("c1".into(), "+61432".into(), None);
        let prompt = build_voice_system_prompt(
            "You are a receptionist.",
            &cfg(true),
            &[lawn_svc()],
            &session,
            fixed_now_utc(),
        );
        // Slim prompt MUST NOT include the legacy "<TOOL>...</TOOL>"
        // grammar block — that's the whole point of the redesign.
        assert!(!prompt.contains("<TOOL>"));
        assert!(!prompt.contains("HARD RULES"));
        // Must still ground the bot in TODAY + services + hours.
        assert!(prompt.contains("TODAY:"));
        assert!(prompt.contains("Lawn mow"));
        // Idle-mode STATE block.
        assert!(prompt.contains("STATE: no active booking"));
    }

    #[test]
    fn slim_voice_prompt_gathering_mode_with_full_referent_says_check() {
        use crate::calendar::session::BookingDraft;
        use crate::calendar::CallSession;
        let mut session = CallSession::new("c1".into(), "+61432".into(), None);
        session.draft_booking = Some(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(chrono::NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            ..Default::default()
        });
        session.recompute_mode(&[lawn_svc()]);
        let prompt = build_voice_system_prompt(
            "You are a receptionist.",
            &cfg(true),
            &[lawn_svc()],
            &session,
            fixed_now_utc(),
        );
        // Caller has already named both — prompt should nudge the bot
        // toward saying it's checking, not re-asking.
        assert!(prompt.contains("you're checking availability"));
        assert!(!prompt.contains("ask the caller what day"));
    }

    #[test]
    fn slim_voice_prompt_gathering_with_partial_draft_asks_for_missing() {
        use crate::calendar::session::BookingDraft;
        use crate::calendar::CallSession;
        let mut session = CallSession::new("c1".into(), "+61432".into(), None);
        session.draft_booking = Some(BookingDraft {
            service: Some("Lawn mow".into()),
            date: None,
            ..Default::default()
        });
        session.recompute_mode(&[lawn_svc()]);
        let prompt = build_voice_system_prompt(
            "You are a receptionist.",
            &cfg(true),
            &[lawn_svc()],
            &session,
            fixed_now_utc(),
        );
        // Service known but no date — bot should ask for the day.
        assert!(prompt.contains("ask the caller what day works"));
        assert!(!prompt.contains("you're checking availability"));
    }

    #[test]
    fn slim_voice_prompt_slots_offered_lists_times_in_12h() {
        use crate::calendar::session::{BookingDraft, OfferedSlots};
        use crate::calendar::CallSession;
        let mut session = CallSession::new("c1".into(), "+61432".into(), None);
        // The runtime path always populates a draft alongside slots
        // (see `run_voice_inline_tools_and_pass2`'s session update).
        // Mirror that here so recompute_mode reaches SlotsOffered.
        session.draft_booking = Some(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(chrono::NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            ..Default::default()
        });
        session.last_offered_slots = Some(OfferedSlots {
            date: chrono::NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            service: "Lawn mow".into(),
            slots: vec![
                chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                chrono::NaiveTime::from_hms_opt(14, 0, 0).unwrap(),
                chrono::NaiveTime::from_hms_opt(16, 30, 0).unwrap(),
            ],
        });
        session.recompute_mode(&[lawn_svc()]);
        let prompt = build_voice_system_prompt(
            "You are a receptionist.",
            &cfg(true),
            &[lawn_svc()],
            &session,
            fixed_now_utc(),
        );
        // Slots rendered in 12-hour spoken-style format.
        assert!(prompt.contains("9am"));
        assert!(prompt.contains("2pm"));
        assert!(prompt.contains("4:30pm"));
        // Bot is told to pick one or two — not all.
        assert!(prompt.contains("offer one or two"));
    }

    #[test]
    fn slim_voice_prompt_disabled_calendar_returns_persona_unchanged() {
        use crate::calendar::CallSession;
        let session = CallSession::new("c1".into(), "+61432".into(), None);
        let prompt = build_voice_system_prompt(
            "You are a receptionist.",
            &cfg(false),
            &[],
            &session,
            fixed_now_utc(),
        );
        assert_eq!(prompt, "You are a receptionist.");
    }

    /// R3-#7: regression-protect the "don't say booked before the DB
    /// confirms" instructions so a future prompt refactor can't
    /// silently lose the guard rail. The reviewer flagged a real risk
    /// — the bot occasionally confirmed bookings the DB later
    /// rejected (slot conflict, deactivated service) — and the only
    /// thing standing between an LLM hallucination and a customer
    /// expecting a slot they don't have is exactly this prompt
    /// language.
    #[test]
    fn confirming_state_forbids_premature_confirmation_language() {
        use crate::calendar::session::BookingDraft;
        use crate::calendar::CallSession;
        let mut session = CallSession::new("c1".into(), "+61432".into(), None);
        // Fully-fleshed draft drives the session into Confirming.
        session.draft_booking = Some(BookingDraft {
            service: Some("Lawn mow".into()),
            date: Some(chrono::NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            start_time: Some(chrono::NaiveTime::from_hms_opt(14, 0, 0).unwrap()),
            customer_name: Some("Alice".into()),
            address: Some("12 Test Lane".into()),
            ..Default::default()
        });
        session.recompute_mode(&[lawn_svc()]);
        let prompt = build_voice_system_prompt(
            "You are a receptionist.",
            &cfg(true),
            &[lawn_svc()],
            &session,
            fixed_now_utc(),
        );
        // The DON'T list must be present — without it the model
        // happily says "all booked" before book_appointment commits.
        assert!(
            prompt.contains("DON'T say"),
            "confirming-state prompt missing the explicit don't-say guard rail"
        );
        assert!(
            prompt.contains("booked"),
            "confirming-state prompt should mention 'booked' as forbidden pre-commit"
        );
        assert!(
            prompt.contains("after the caller confirms AND the system reports"),
            "confirming-state prompt should require BOTH caller confirmation AND DB success"
        );
    }
}
