//! Durable `aokie.*` event emission: call.ended, transcript settlements, control failures.

#[allow(unused_imports)]
use super::*;

/// The radio's outbox reference: the store plus HOW delivery is bookkept
/// (Legacy write-marks-sent vs ack-awaited — audit INT-003). Carried as one
/// value so every emit site stays a single `outbox` argument.
pub(super) type OutboxRef<'a> = Option<(&'a Outbox, crate::event_bridge::EmitMode)>;

/// Emit one event best-effort: essential events route through the outbox
/// (write-before-emit) when it is open; otherwise fall back to a direct
/// stdout notification so a failed outbox never swallows a live call event.
pub(super) fn emit(outbox: OutboxRef<'_>, sink: &mut dyn Sink, event: DesktopEvent) {
    match outbox {
        Some((o, mode)) => {
            if let Err(e) = emit_event(sink, o, &event, false, mode) {
                eprintln!("[aokie-plugin] radio emit '{}' failed: {e}", event.name);
            }
        }
        None => {
            let line = crate::rpc::notification_line("event.emit", json!({ "event": event }));
            let _ = sink.send_line(&line);
        }
    }
}

/// Emit an `aokie.call.turn.final` transcript turn, matching the contract the
/// Receptionist pack's app-logic + flow bindings expect: `{callId, turn,
/// speaker, text}` with a per-turn-unique idempotency key (`turn.<n>.final`) so
/// the app-logic dedup doesn't drop turns after the first. `speaker` is
/// "caller" (STT) or "bot" (Aokie's own speech); a flow gates its reply on
/// `speaker === 'caller'` so Aokie never answers itself.
#[cfg(feature = "voice")]
pub(super) fn emit_turn(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    corr: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
) {
    emit_turn_with_delivery(outbox, sink, corr, turn_index, speaker, text, None, None)
}

/// AOK-CTRL-001: bot turns carry a structured per-turn DELIVERY status —
/// `complete` (every recorded sentence audibly played), `interrupted` (caller
/// barge-in cut it short), `operator_ended` (an operator action stopped it) or
/// `error` (synthesis/stream failure mid-reply). The `text` is already only
/// what actually played (the truthful-transcript rule); `delivery` says WHY it
/// may be shorter than the generation. Additive payload field — existing
/// consumers ignore it. Caller turns have no delivery dimension (`None`).
#[cfg(feature = "voice")]
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_turn_with_delivery(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    corr: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
    delivery: Option<&str>,
    // Speech-START stamp (None = now): bot turns are emitted when the reply
    // FINISHES, but their place in the conversation is when they began.
    at: Option<&str>,
) {
    emit_turn_full(
        outbox, sink, corr, turn_index, speaker, text, delivery, None, false, at,
    )
}

/// Full turn emitter: `kind: Some("control")` marks a caller turn that was a
/// FLOOR COMMAND ("wait", "stop", "slower") handled deterministically by the
/// duplex coordinator — recorded truthfully in the transcript, but flows and
/// downstream logic can (and should) skip business handling for it. Additive
/// payload field — existing consumers ignore it.
#[cfg(feature = "voice")]
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_turn_full(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    corr: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
    delivery: Option<&str>,
    kind: Option<&str>,
    overlapped: bool,
    at_override: Option<&str>,
) {
    let occurred_at = at_override
        .map(str::to_string)
        .unwrap_or_else(aokie_core::events::now_iso8601);
    // The v2 lane consumes the same finalized STT/TTS truth as the durable
    // turn event. It is bounded/volatile and remains permission filtered by
    // the native consent gate plus gateway admission grants.
    crate::remote_media::publish_caption_globally(corr, turn_index, speaker, text, &occurred_at);
    let mut payload = json!({
        "callId": corr,
        "turn": turn_index,
        "speaker": speaker,
        "text": text,
        // Overlap turns carry their SPEECH-START estimate, not commit time.
        "at": occurred_at,
    });
    if let Some(d) = delivery {
        payload["delivery"] = json!(d);
    }
    if let Some(k) = kind {
        payload["kind"] = json!(k);
    }
    // Truthful ordering (live report 2026-07-13): a turn seeded from OVERLAP
    // capture is recorded when it flushes — AFTER the bot line it was spoken
    // over. The flag says "this began while the bot was talking", so readers
    // don't misread the record order as the speech order. Additive field.
    if overlapped {
        payload["overlapped"] = json!(true);
    }
    emit(
        outbox,
        sink,
        aokie_core::events::aokie_turn_event(true, corr, turn_index, payload),
    );
}

/// Ordering guard for overlap back-dating: the estimated speech start of a
/// caller turn seeded from overlap capture may never land at (or before) the
/// speech-start stamp of the bot line it was spoken OVER — the transcript
/// sorts by these stamps, and an estimate that overshoots flips the visible
/// order. Live call 8576ba9e (2026-07-18): line noise during the carrier's
/// answer transition tripped the capture threshold at the greeting's very
/// first frames, pinning the caller's estimate to the answer instant - 6ms
/// BEFORE the greeting's own stamp - so the transcript opened with the
/// caller's line. The margin keeps the caller turn safely after the bot line
/// even across the microseconds between the two stamp computations.
#[cfg(feature = "voice")]
pub(super) const OVERLAP_ORDER_MARGIN_MS: u64 = 60;

/// Clamped back-date for an overlap-seeded caller turn: at most
/// `since_speech_start - margin` ago, so it always sorts AFTER the bot line
/// it interrupted. Pure math half, unit-tested.
#[cfg(feature = "voice")]
pub(super) fn overlap_backdate_ms(captured_ms: u64, since_speech_start_ms: u64) -> u64 {
    captured_ms.min(since_speech_start_ms.saturating_sub(OVERLAP_ORDER_MARGIN_MS))
}

/// Format an overlap turn's back-dated `at` from the captured sample count,
/// clamped against the elapsed time since the interrupted line began speaking.
/// NOT used for the outbound pre-line hello (the callee's pickup genuinely
/// precedes the agent's opening line — sorting it first is correct there).
#[cfg(feature = "voice")]
pub(super) fn overlap_backdate(captured_samples: usize, sr_hz: usize, since_speech_start: Duration) -> String {
    let captured_ms = (captured_samples * 1000 / sr_hz.max(1)) as u64;
    aokie_core::events::iso8601_ago_ms(overlap_backdate_ms(
        captured_ms,
        since_speech_start.as_millis() as u64,
    ))
}

/// Emit the buffered `call.incoming` NOW if it hasn't gone out yet (audit
/// AOK-LIF-001). The incoming event is normally held briefly for caller-ID
/// enrichment; every other call-scoped event (ringing/answered/audio/ended)
/// forces it out first, so `incoming` ALWAYS precedes the rest of its call's
/// lifecycle regardless of the hold. `from` is whatever caller id has
/// arrived — empty when the phone hasn't sent one (never a sentinel, §8).
pub(super) fn flush_incoming_if_pending(
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    if !tracker.current().is_some_and(|s| s.incoming_pending()) {
        return;
    }
    let (corr, from) = {
        let s = tracker.current_mut().unwrap();
        s.mark_incoming_emitted();
        (s.id.clone(), s.caller_id.clone().unwrap_or_default())
    };
    emit(
        outbox,
        sink,
        aokie_core::events::aokie_event(
            crate::contract::events::CALL_INCOMING,
            &corr,
            json!({"callId": corr, "from": from, "at": aokie_core::events::now_iso8601()}),
        ),
    );
}

/// The canonical `call.ended` emission — shared by the phone's real
/// CallTerminated and the synthesized device-loss termination (audit
/// AOK-LIF-003), so both produce ONE identical terminal event shape.
/// The after-call flows key off this payload (contract §events): callId
/// mirrors the envelope correlation, from/callerPhone carry the caller id,
/// durationSeconds/durationMs count from ANSWER. The outcome comes from the
/// session state machine (audit AK-001): answered → "completed" (even a
/// sub-second call), operator-rejected → "rejected", never answered →
/// "missed", radio link gone → reason "device_lost".
/// Phase 4 (hold queue): how a PARKED caller's disappearance reads. A caller
/// who had a real conversation before being held gave up ON HOLD (follow-up
/// flows apologise by SMS — never a callback that would ring someone who
/// chose to leave); one who only ever heard the "please hold" line gave up
/// IN THE QUEUE (follow-ups call back like a missed call, with a hold
/// apology in the opening line).
pub(super) fn parked_end_intent(
    sess: &crate::call_session::CallSession,
) -> crate::call_session::TerminationIntent {
    if sess.greeted {
        crate::call_session::TerminationIntent::AbandonedOnHold
    } else {
        crate::call_session::TerminationIntent::AbandonedInQueue
    }
}

pub(super) fn manager_number_classification(outbound: bool, caller_id: Option<&str>) -> bool {
    // Classification only. This bit lets ordinary post-call automation avoid
    // treating the owner's line as a new customer/booking; it is never proof
    // of identity and must never grant manager actions. Those still require
    // the separate per-call PIN challenge and its exact authorization state.
    manager_classification_flag(
        outbound,
        crate::screen::ScreenPolicy::from_env().is_manager(caller_id),
    )
}

pub(super) fn manager_classification_flag(outbound: bool, configured_manager_number: bool) -> bool {
    !outbound && configured_manager_number
}

pub(super) fn emit_call_ended(
    ended: &crate::call_session::EndedCall,
    config_version: u64,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) {
    use aokie_core::events::{aokie_event, now_iso8601};
    let from = ended.caller_id.clone().unwrap_or_default();
    // Phase 3 (additive): a manager-number caller. The after-call booking
    // extractor must never run on the manager's own line — live call
    // a5c3f900: a manager-line move was ALSO read as a new booking, minting
    // a duplicate appointment + an active SMS loop + a kickoff text AT THE
    // MANAGER. Env is the same truth the running ScreenPolicy is built from
    // (managerNumbers applies live through apply_screening_env).
    let manager = manager_number_classification(ended.outbound, ended.caller_id.as_deref());
    let ended_data = json!({
        "at": now_iso8601(),
        "reason": ended.reason,
        "callId": ended.id,
        "from": from,
        "callerPhone": from,
        "durationSeconds": ended.duration_seconds,
        "durationMs": ended.duration_ms as u64,
        "outcome": ended.outcome,
        // Phase 2 (additive): which way the call went. Outbound
        // callers' `from` is the DIALED number.
        "direction": if ended.outbound { "outbound" } else { "inbound" },
        "manager": manager,
        "configVersion": config_version,
    });
    emit(
        outbox,
        sink,
        aokie_event(
            crate::contract::events::CALL_ENDED,
            &ended.id,
            ended_data.clone(),
        ),
    );
    TRANSCRIPT_SETTLEMENTS.with(|tracker| {
        tracker
            .borrow_mut()
            .call_ended(&ended.id, ended_data, std::time::Instant::now())
    });
}

pub(super) fn emit_ready_transcript_settlements(outbox: OutboxRef<'_>, sink: &mut dyn Sink) -> bool {
    let ready =
        TRANSCRIPT_SETTLEMENTS.with(|tracker| tracker.borrow().ready(std::time::Instant::now()));
    emit_transcript_settlements(ready, outbox, sink)
}

pub(super) fn emit_forced_transcript_settlements(outbox: OutboxRef<'_>, sink: &mut dyn Sink) -> bool {
    let ready = TRANSCRIPT_SETTLEMENTS.with(|tracker| tracker.borrow().all_ended_as_timed_out());
    emit_transcript_settlements(ready, outbox, sink)
}

/// Settlement has a stronger success boundary than ordinary best-effort
/// radio events because graceful shutdown waits on it. Inspect the structured
/// insert outcome directly: only a fresh insert or a content-identical
/// duplicate proves the exact barrier is durably replayable. A quarantined
/// payload or same-key/different-content collision must never masquerade as
/// success merely because a `dead` row exists under that key.
pub(super) fn emit_transcript_settlement_durably(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    event: &DesktopEvent,
) -> bool {
    let Some((outbox, mode)) = outbox else {
        let line = crate::rpc::notification_line("event.emit", json!({ "event": event }));
        return sink.send_line(&line).is_ok();
    };

    match outbox.insert_pending(event, crate::outbox::TARGET_DESKTOP) {
        Ok(crate::outbox::InsertOutcome::Inserted | crate::outbox::InsertOutcome::Duplicate) => {
            // The exact payload is durable now. Delivery may still fail (or
            // be intentionally held for eventAck), but the normal outbox
            // replay loop owns it, so settlement can leave the in-memory
            // tracker and graceful shutdown may acknowledge.
            if let Err(error) = emit_event(sink, outbox, event, false, mode) {
                eprintln!(
                    "[aokie-plugin] transcript settlement {} is durable but delivery is pending: {error}",
                    event.correlation_id
                );
            }
            true
        }
        Ok(crate::outbox::InsertOutcome::PayloadCollision) => {
            eprintln!(
                "[aokie-plugin] transcript settlement {} refused: idempotency-key payload collision",
                event.correlation_id
            );
            false
        }
        Ok(crate::outbox::InsertOutcome::QuarantinedProtectFailed) => {
            eprintln!(
                "[aokie-plugin] transcript settlement {} was quarantined because its payload could not be protected",
                event.correlation_id
            );
            false
        }
        Err(error) => {
            eprintln!(
                "[aokie-plugin] transcript settlement {} outbox insert failed: {error}",
                event.correlation_id
            );
            false
        }
    }
}

pub(super) fn emit_transcript_settlements(
    ready: Vec<ReadyTranscriptSettlement>,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
) -> bool {
    let mut all_durable = true;
    for mut settled in ready {
        if settled.correction_timed_out {
            eprintln!(
                "[aokie-plugin] transcript correction settle timed out for {} after {}s",
                settled.call_id,
                TRANSCRIPT_SETTLE_TIMEOUT.as_secs()
            );
        }
        if let Some(data) = settled.data.as_object_mut() {
            data.insert(
                "transcriptSettledAt".to_string(),
                json!(aokie_core::events::now_iso8601()),
            );
            data.insert(
                "transcriptCorrectionTimedOut".to_string(),
                json!(settled.correction_timed_out),
            );
        }
        let event = aokie_core::events::aokie_event(
            crate::contract::events::CALL_TRANSCRIPT_SETTLED,
            &settled.call_id,
            settled.data,
        );
        let durable = emit_transcript_settlement_durably(outbox, sink, &event);
        if durable {
            TRANSCRIPT_SETTLEMENTS
                .with(|tracker| tracker.borrow_mut().mark_settled(&settled.call_id));
        } else {
            all_durable = false;
            eprintln!(
                "[aokie-plugin] transcript settlement for {} was not durably written — retaining it for retry",
                settled.call_id
            );
        }
    }
    all_durable
}

/// AOK-CTRL-001: the authoritative FAILURE record for an accepted call
/// control. The connector's command result only ever says `accepted/queued`
/// (the enqueue succeeded); when the radio later fails to act on the phone,
/// this emits `aokie.hardware.error` carrying `code: "control_failed"`, the
/// action and the operation id from the accepted result — so a flow/UI can
/// correlate "my command didn't happen" instead of trusting a premature verb.
#[cfg(target_os = "windows")]
pub(super) fn emit_control_failed(
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    tracker: &crate::call_session::SessionTracker,
    action: &str,
    op: Option<&str>,
    error: &str,
) {
    use aokie_core::events::{aokie_event_occurrence, occurrence_id};
    let corr = tracker.call_id().unwrap_or("radio").to_string();
    emit(
        outbox,
        sink,
        aokie_event_occurrence(
            crate::contract::events::HARDWARE_ERROR,
            &corr,
            &occurrence_id(),
            json!({
                "message": format!("{action} failed on the radio: {error}"),
                "code": "control_failed",
                "action": action,
                "operationId": op,
            }),
        ),
    );
}
