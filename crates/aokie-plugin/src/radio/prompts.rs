//! Standing agent instructions, spoken lines and small text-policy helpers.

#[allow(unused_imports)]
use super::*;

/// Spoken on answer when no greeting is configured. A BLANK greeting setting means
/// "use the default", never "answer silently" — a desktop settings-form save (which
/// writes the full settings bag, greeting included) or a flow push with an empty form
/// field must not silence the receptionist. Shared by the spawn path (connector.rs)
/// and the live `RadioControl::Configure` path below.
pub const DEFAULT_GREETING: &str = "Hello, thanks for calling. How can I help you today?";

/// Appended to the system prompt when `agentHangup` is on. The agent asks the LLM
/// to emit an [[END_CALL]] marker at the very end of its farewell so the plugin
/// knows the conversation is complete and can hang up after the goodbye plays.
#[cfg(feature = "voice")]
pub(super) const END_CALL_INSTRUCTION: &str = "\n\nEnding the call: ordinary replies just answer - do NOT tack \"anything else?\" onto them (live transcripts show it seven times in one call; it reads as rushing the caller off the line). Only when the conversation itself sounds finished - the caller's requests are handled and they have nothing more - ask ONCE whether they need anything else, with NO marker. After the caller confirms they are done (\"no, that's all\", \"that's everything, thanks\", \"no thank you\") or says goodbye themselves, reply with a brief, warm goodbye and append the exact marker [[END_CALL]] at the very end of that goodbye. Never ask the anything-else question twice in a row. The goodbye carrying the marker must never contain a question - the system refuses to hang up while a question is waiting for an answer. Never write the marker mid-conversation.";

/// Appended to the system prompt in agent mode: how the model participates in
/// spoken delivery (validated pacing markers, the [[WAIT]] intentional-silence
/// marker, and continuation over backchannels). All markers are UNTRUSTED
/// input — the speech planner clamps rates, caps protection and strips every
/// bracketed token before anything is spoken or recorded. Plain ASCII (the
/// text is model-facing but lives next to caller-spoken constants).
#[cfg(feature = "voice")]
/// Standing booking rule (live report 2026-07-14, call be80cacb: an unknown
/// caller booked a table for two and was never asked their NAME - they even
/// pointed it out on the call). Appended at reply time like the other
/// standing instructions; the KNOWN-CALLER overlay's "don't re-ask" rule
/// composes cleanly with the "unless you already know it" clause.
pub(super) const BOOKING_INSTRUCTION: &str = "

Booking rule: first respect the configured capabilities for this call. If no booking backend is connected, say you cannot create or confirm an appointment; do not act as if collecting a name completes a booking. When the configured workflow supports appointment requests, collect the caller's name (unless already known), preferred date and time. Read these back as a REQUEST awaiting confirmation, not a confirmed appointment. A caller's agreement, a readback, and your own earlier messages are never proof of a successful booking. Only a successful backend result explicitly confirming the exact appointment can justify saying it is confirmed.";

pub(super) const CONVERSATION_GROUNDING_INSTRUCTION: &str = "\n\nBefore speaking: respect the operator's capability limits above. Never turn a requested appointment into a confirmed booking without a successful backend result; correct any unsupported confirmation in your earlier replies. Treat a clipped or nonsensical transcript as recognition uncertainty: ask one short question such as 'Could you repeat that last part?' rather than explaining the stray word or changing topics. For example, after asking which day, an unrelated fragment such as act is not a day: reply only Could you repeat that last part? Do not add a name, date or other intake question. If the caller trails off mid-sentence, give them time to finish. Use their latest correction (for example Friday replacing Thursday), and do not ask again for a choice they already made. A correction that starts with Wait, I meant is a complete request: acknowledge the corrected detail instead of emitting [[WAIT]]. Never offer to send a message or contact the team if the operator says those capabilities are unavailable. When the caller spells a name or word letter by letter, use those letters to resolve a conflicting speech transcription (for example Alex, A-L-I-C-E means Alice); read the spelling back once if uncertain, never call a clear spelling a cut-off message. Ask only one question and do not repeat a question the caller already answered. Speak plain text without Markdown formatting.";

/// Live business lookups (guide P1-16): tells the model about the
/// [[LOOKUP:]] tool. Appended by the shared composer, so speculative and
/// real generations stay identically primed. A missing/failed lookup flow
/// degrades gracefully — the injected result says UNAVAILABLE and the model
/// answers from its notes.
pub(super) const TOOL_INSTRUCTION: &str = "\n\nLive lookups: when the caller asks for business DATA you genuinely do NOT have in your notes (availability beyond the listed days, records), reply with EXACTLY [[LOOKUP: one clear data question]] and nothing else - the SYSTEM runs the lookup and hands you the result to answer from. The lookup question is addressed to a DATABASE, never to the caller: if you need to ask the CALLER something (to clarify a date, for example), just ask them normally WITHOUT the marker. At most one lookup per caller turn; never for things already in your notes. Dates beyond your calendar window are EXACTLY what lookups are for - run one instead of deferring to the team. When the question involves specific dates, work each one out from today's date and write it in plain YYYY-MM-DD form inside the lookup question (for example [[LOOKUP: availability 2026-08-01]]) - the system answers exact dates directly. If the caller names a WEEKDAY within some week ('the Wednesday in the second week of August'), resolve THAT weekday to its exact date and ask for it. For a whole week or span, write a range: [[LOOKUP: availability 2026-08-11 to 2026-08-17]]. NEVER tell the caller you will check or look something up without putting the [[LOOKUP: ...]] marker in that SAME reply - announcing a check without the marker strands the caller in silence waiting for an answer that never comes. NEVER defer a date or availability question to the team without running the lookup FIRST - the calendar is right there; if the caller has not named a date yet, ask them for the date instead of deferring.";

/// Typed help is a separate, consent-gated Companion channel. The model may
/// request help, but it cannot choose recipients, grant access, or turn the
/// responder's text into a control command. The accepted answer is relayed to
/// the caller as attributed text and is never reinterpreted as a tool marker.
pub(super) const ASSISTANCE_INSTRUCTION: &str = "\n\nTeam assistance: only when a caller needs a business decision or exception that is not in your notes or available through a live lookup, reply with EXACTLY [[ASSISTANCE: one short question for the authorised team]] and nothing else. The SYSTEM selects eligible responders according to server policy. Never name or choose a recipient, never include secrets or a full transcript, and never use this marker for ordinary data lookups. At most one assistance request may be pending. The system will tell the caller you asked and will relay the authorised team's answer.";

pub(super) const TRANSFER_INSTRUCTION: &str = "\n\nOwner transfer: when the caller EXPLICITLY asks to speak with the owner, a person or a human, or a standing safety/policy rule requires a live human escalation, reply with EXACTLY [[TRANSFER: short generic reason]] and nothing else. The reason must be at most six short words (30 bytes), with no name, phone number, secret or transcript. Use [[ASSISTANCE:]] instead when you only need the team's answer while continuing to help the caller. Never claim the caller is on hold or transferred: the SYSTEM checks availability while you stay with them, and only proven live media completes a transfer.";

pub(super) const ASSISTANCE_FILLER_LINE: &str = "One moment - I'm checking that with the team for you.";
pub(super) const ASSISTANCE_PENDING_LINE: &str =
    "I've already asked the team and I'll let you know as soon as they reply.";
pub(super) const ASSISTANCE_UNAVAILABLE_LINE: &str =
    "I can't contact the team from this call. No message or callback has been arranged.";
pub(super) const TRANSFER_CHECKING_LINE: &str =
    "I'll check whether the owner is available to take your call. I'll stay with you while we wait.";
pub(super) const TRANSFER_UNAVAILABLE_LINE: &str =
    "They aren't available to take the call just now. I can keep helping with the information I have.";
pub(super) const TRANSFER_REQUEST_INVALID_LINE: &str =
    "I couldn't send that request just now. I can keep helping with the information I have.";
pub(super) const ASSISTANCE_REQUEST_TTL_SECONDS: u64 = 60;
pub(super) const TRANSFER_REQUEST_TTL_SECONDS: u64 = 30;

/// Spoken while the lookup flow runs (1-4 s): silence there reads as a dead
/// line. Persona-neutral on purpose.
pub(super) const LOOKUP_FILLER_LINE: &str = "One moment - let me check that for you.";

/// Spoken when the model asks for ANOTHER lookup after already receiving one
/// (or the line can't take a lookup): an honest handoff beats silence — the
/// silent path ended in the technical-difficulties fail-safe on a live call
/// (73325204).
pub(super) const LOOKUP_HANDOFF_LINE: &str =
    "I couldn't verify that information right now. No booking or callback has been arranged.";

pub(super) const SPEECH_STYLE_INSTRUCTION: &str = "\n\nSpoken delivery: your words are read aloud to the caller by a voice synthesizer.\n- This is a LIVE phone conversation: keep every reply to ONE or TWO short sentences, then let the caller speak. Long replies get talked over and feel rude. Ask at most one question per reply. Only go longer when reading back details the caller asked for.\n- When reading back dates, times or booking details from your notes, copy them EXACTLY as written - never approximate, merge or reorder them. If a detail is not in your notes, say you will have the team confirm it rather than guessing.\n- Phone numbers and codes are automatically read slowly, digit by digit; you do not need to do anything special for them.\n- You may wrap a short critical detail in [[slow]]...[[/slow]] to have it spoken more slowly.\n- Rarely, you may wrap ONE short vital sentence in [[important]]...[[/important]] so a brief overlap does not cut it off. The caller can always stop you by saying stop or wait.\n- If the caller ONLY asks for time to think or pause, reply with exactly [[WAIT]] and nothing else. Read the ENTIRE utterance first: wait followed by a correction or question is NOT a request for silence; respond to that correction or question. Never fill their pause with chatter; when they speak again, continue naturally.\n- If the caller's words were only a brief acknowledgement (yeah, okay, mm-hm) while you were talking, continue where you left off instead of starting over - or reply with [[WAIT]] if nothing needs saying.\n- NEVER reply [[WAIT]] twice in a row: if you already waited once and the caller speaks again, greets you, or checks you are there, ANSWER them.\nThe double-bracket markers are never spoken and never shown to anyone.";

/// VOICE-001 fail-safe: what the caller hears when the responder breaks
/// MID-call (LLM died / synthesis went silent) — a plain apology, then a
/// clean hangup. Local + fixed so it needs nothing but TTS; when TTS itself
/// is the broken half, the hangup still happens (silence must END, never
/// stretch on).
#[cfg(feature = "voice")]
pub(super) const FALLBACK_LINE: &str = "I'm sorry, I'm having technical trouble taking your call right now. \
Please call back shortly. Goodbye.";

// ── Phase 4 call-waiting: the deterministic spoken switch flow ──────────
// Only the ACTIVE call can hear Aokie (HFP exposes one audio path), so the
// receptionist juggles: it tells the primary caller it will be a moment,
// swaps to the new caller to ask them to hold, swaps back and resumes. All
// lines are fixed/records-composed (never model prose) and ASCII (TTS-safe).
/// Spoken to the PRIMARY caller, who is active, right before they are put on
/// hold to deal with a second caller knocking.
#[cfg(feature = "voice")]
pub(super) const HOLD_PRIMARY_ASK_LINE: &str =
    "Sorry, I've just had another call come in. Let me put you on hold for one moment - I'll be right back with you.";
/// Spoken to the PRIMARY caller when Aokie returns to them after the swap.
#[cfg(feature = "voice")]
pub(super) const HOLD_PRIMARY_RESUME_LINE: &str = "Thanks so much for holding. Now, where were we?";
/// Spoken to the SECOND caller during the brief window they are active, to
/// ask them to hold. `heard_hold_line` appends their queue position.
#[cfg(feature = "voice")]
pub(super) const HOLD_SECOND_ASK_LINE: &str =
    "Thank you for calling! I'm just with another caller at the moment. Please hold and I'll be with you as soon as I can.";
/// Spoken to a held caller when Aokie finally gives them its full attention
/// (the previous call ended and they were promoted from hold).
#[cfg(feature = "voice")]
pub(super) const HOLD_PROMOTED_GREET_LINE: &str = "Thank you so much for holding. How can I help you today?";
/// Spoken to the PRIMARY caller when a juggle is ABANDONED before they were
/// ever really held (the knock vanished, or the phone ignored the swap) —
/// they heard "let me put you on hold" and then nothing happened.
#[cfg(feature = "voice")]
pub(super) const HOLD_JUGGLE_ABORT_LINE: &str = "Sorry about that - I'm back with you. Where were we?";

/// The hold ask spoken to a just-accepted caller, with their queue position.
/// `ahead_in_queue` counts the callers WAITING ahead of them (excluding
/// whoever is being actively served): 0 = they are next; 1 = number 2; and
/// so on. The numbers stay honest as the queue moves — someone hanging up
/// ahead simply means the next accept speaks a smaller number. Kept pure
/// for testing.
#[cfg(feature = "voice")]
pub(super) fn second_caller_hold_line(ahead_in_queue: u32) -> String {
    let tail = match ahead_in_queue {
        0 => " You're next in the queue.".to_string(),
        n => format!(" You're number {} in the queue.", n + 1),
    };
    format!("{HOLD_SECOND_ASK_LINE}{tail}")
}

/// Phase 1 abuse handling (call-policy spec): the standing prompt rule. The
/// model only FLAGS ([[ABUSE]]); deterministic code speaks the notice, ends
/// the call and writes the block — the LLM is never in the block/unblock
/// path. Worded to keep a small model from overfiring on ordinary frustration.
pub(super) const ABUSE_INSTRUCTION: &str = "\n\nAbusive callers: reply with EXACTLY [[ABUSE]] and nothing else ONLY when the caller is SWEARING AT you or the staff (profanity aimed at a person), or YELLING abuse at you (sustained angry shouting), or both. The system then speaks a standard notice and ends the call for you. This is a LAST RESORT with a very high bar: rudeness, sarcasm, insults without swearing, threats to leave a bad review, frustration, venting, complaining, or swearing about their own situation (not at a person) are NOT abuse - stay warm, patient and helpful through ALL of those, every time. One heated word is not abuse either; it must be unmistakable, directed and sustained. Never argue with or lecture an abusive caller yourself, and never threaten them with the marker.";

/// Phase 1: the deterministic notice spoken to a flagged caller before the
/// hangup — never model prose. ASCII only (straight to TTS).
#[cfg(feature = "voice")]
pub(super) const ABUSE_LINE: &str = "We do not tolerate abusive calls, so this call will now end. Goodbye.";

#[cfg(feature = "voice")]
pub(super) fn is_exact_abuse_marker(text: &str) -> bool {
    text.trim() == "[[ABUSE]]"
}

/// Phase 2: composes the OUTBOUND CALL persona block for a plugin-dialed
/// call. The local voice engine listens first; the introduction belongs in its
/// first reply, never in an unsolicited greeting over the recipient's hello.
#[cfg(feature = "voice")]
pub(super) fn outbound_call_block(number: &str, purpose: Option<&str>) -> String {
    let purpose_line = match purpose {
        Some(p) if !p.trim().is_empty() => format!("\nPurpose of this call: {}", p.trim()),
        _ => String::new(),
    };
    format!(
        "\n\nOUTBOUND CALL: YOU placed this call to {number} - the person answering is NOT a caller, you rang THEM. Wait for the recipient to speak first. If you have not spoken yet, respond naturally to their greeting, identify yourself as Aokie, the AI assistant, and briefly explain why you called. If your introduction is already in the conversation history, do not repeat it.{purpose_line}\nBe brief and polite, one short question at a time. Once they say that is all, all good, or otherwise close the conversation, thank them and end with [[END_CALL]]; do not ask another anything-else question. On an outbound call, NEVER use an anything-else or how-can-I-help follow-up after a closing acknowledgement. Examples: Recipient: All good, thank you so much. Assistant: You are welcome. Have a lovely day! [[END_CALL]] Recipient: That is all, thanks. Assistant: Thanks for your time. Goodbye! [[END_CALL]] Accomplish the purpose, answer their questions honestly from your notes, then say a short goodbye ending with [[END_CALL]]. If they are busy, annoyed, or say it is a bad time, apologise briefly and end the call politely with [[END_CALL]]. Do not promise to call back later: a future callback must be explicitly scheduled and acknowledged first. For a busy recipient say: Sorry to interrupt. Thanks for your time. Goodbye! [[END_CALL]] If you reach VOICEMAIL or an answering machine (a recorded greeting, a beep, no live person), leave ONE short message covering the purpose and end with [[END_CALL]] - never hold a conversation with a recording."
    )
}

/// VOICE-001, pure for tests: after a reply attempt, is the caller sitting in
/// DEAD AIR? True only when nothing audibly played AND nothing else explains
/// the silence — a barge-in means the caller is talking (their turn is already
/// accumulating), and an operator action means a human has the call.
#[cfg(feature = "voice")]
pub(super) fn reply_left_dead_air(audible: bool, barged: bool, operator_ended: bool) -> bool {
    !audible && !barged && !operator_ended
}

/// Remove any end-of-call marker the LLM emitted (tolerant to small-model
/// variants: bracketed or bare, any case) and report whether one was present.
/// Returns the cleaned, trimmed text so the marker is never spoken or recorded.
#[cfg(feature = "voice")]
pub(super) fn strip_end_call_marker(s: &str) -> (String, bool) {
    // Longest / most-bracketed variants first so the bare token never leaves a
    // stray bracket behind.
    const VARIANTS: [&str; 6] = [
        "[[END_CALL]]",
        "[[END CALL]]",
        "[END_CALL]",
        "[END CALL]",
        "END_CALL",
        "END CALL",
    ];
    let mut out = s.to_string();
    let mut found = false;
    for v in VARIANTS {
        let vl = v.to_lowercase();
        // A BRACKETED variant is unambiguous anywhere. The bare tokens are not:
        // "end call" is a substring of ordinary English — "recommend calling",
        // "weekend call", "a friend called", "attend calls" — and an unanchored
        // search both cut the word apart before it was spoken ("I'd recommend
        // calling back" became "I'd recomming back") and hung the caller up
        // mid-sentence. Only accept a bare token standing on its own.
        let bare = !v.starts_with('[');
        let mut from = 0usize;
        loop {
            let lower = out.to_lowercase();
            let Some(rel) = lower[from..].find(&vl) else { break };
            let pos = from + rel;
            let end = pos + v.len();
            if bare && !stands_alone(&lower, pos, end) {
                // Part of a longer word — leave it alone and keep looking.
                from = pos + 1;
                continue;
            }
            out.replace_range(pos..end, "");
            found = true;
            from = pos;
        }
    }
    (out.trim().to_string(), found)
}

/// Is the slice `[start, end)` a whole token — not spliced out of a longer word?
///
/// Boundaries are alphanumeric-only: surrounding punctuation, brackets and
/// whitespace all still count as standing alone, so "…goodbye. END_CALL" and
/// "(end call)" are honoured while "recommend calling" is not.
#[cfg(feature = "voice")]
fn stands_alone(text: &str, start: usize, end: usize) -> bool {
    let before_ok = text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric());
    let after_ok = text[end..]
        .chars()
        .next()
        .is_none_or(|c| !c.is_alphanumeric());
    before_ok && after_ok
}
