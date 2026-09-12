use super::*;

#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn realtime_prompt_never_inherits_legacy_action_markers() {
    let prompt = realtime_safe_instructions(
        "Friendly shop. [[BOOK: table]] [[LOOKUP: records]] [[END_CALL]]",
        true,
    );
    assert!(!prompt.contains("[["));
    assert!(!prompt.contains("]]"));
    for marker in ["BOOK:", "LOOKUP:", "MANAGER:", "ASSISTANCE:", "TRANSFER:"] {
        assert!(
            !prompt.contains(&format!("[[{marker}")),
            "legacy tool marker escaped into direct-speech prompt"
        );
    }
    assert!(prompt.contains("use lookup_business_data"));
    assert!(prompt.contains("call request_appointment WITHOUT speaking first"));
    assert!(prompt.contains("do not ask a redundant second confirmation"));
    assert!(prompt.contains("booking REQUEST"));
    assert!(prompt.contains("call finish_call without speaking first"));
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn realtime_appointment_request_is_minimal_essential_and_durable() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = crate::outbox::Outbox::open(&dir.path().join("outbox.sqlite")).unwrap();
    let mut sink = crate::event_bridge::VecSink::default();
    let request = crate::realtime_appointment::ValidatedAppointmentRequest {
        request_id: "appt_0123456789abcdef0123456789abcdef".into(),
        caller_name: "Lance".into(),
        service: "Lawn mowing".into(),
        date: "2026-07-22".into(),
        time: "10:00".into(),
        agreement_turn: 6,
    };

    emit_realtime_appointment_request(
        Some((&outbox, crate::event_bridge::EmitMode::AckExpected)),
        &mut sink,
        "call_5685374790b241a1a48dd8549c0c6a4c",
        "+61400000000",
        &request,
    )
    .unwrap();
    assert_eq!(sink.lines.len(), 1);
    let line: serde_json::Value = serde_json::from_str(&sink.lines[0]).unwrap();
    let event = &line["params"]["event"];
    assert_eq!(
        event["name"],
        crate::contract::events::APPOINTMENT_REQUESTED
    );
    assert_eq!(event["data"]["requestId"], request.request_id);
    assert_eq!(event["data"]["agreementTurn"], 6);
    assert!(event["data"].get("agreementPhrase").is_none());
    assert!(event["idempotencyKey"]
        .as_str()
        .is_some_and(|key| key.contains(&request.request_id)));
    assert_eq!(
        outbox
            .status_of(event["idempotencyKey"].as_str().unwrap())
            .unwrap(),
        Some(crate::outbox::OutboxStatus::Pending)
    );
    assert!(crate::event_bridge::is_essential(
        crate::contract::events::APPOINTMENT_REQUESTED
    ));

    // A stable key can never make different appointment content look
    // durable merely because the older row is still pending.
    let mut collision = request.clone();
    collision.service = "Tree trimming".into();
    assert!(emit_realtime_appointment_request(
        Some((&outbox, crate::event_bridge::EmitMode::AckExpected)),
        &mut sink,
        "call_5685374790b241a1a48dd8549c0c6a4c",
        "+61400000000",
        &collision,
    )
    .unwrap_err()
    .contains("collided"));

    // Once the exact event is in the outbox, a broken host write is a
    // truthful queued result: replay owns delivery under the same key.
    let failed_dir = tempfile::tempdir().unwrap();
    let failed_outbox =
        crate::outbox::Outbox::open(&failed_dir.path().join("outbox.sqlite")).unwrap();
    let mut failed_sink = crate::event_bridge::VecSink {
        fail: true,
        ..Default::default()
    };
    let mut retryable = request.clone();
    retryable.request_id = "appt_11111111111111111111111111111111".into();
    emit_realtime_appointment_request(
        Some((&failed_outbox, crate::event_bridge::EmitMode::AckExpected)),
        &mut failed_sink,
        "call_retryable",
        "+61400000001",
        &retryable,
    )
    .unwrap();
    let retry_key = aokie_core::events::aokie_idempotency_key(
        "call_retryable",
        &format!("appointment.requested.{}", retryable.request_id),
    );
    assert_eq!(
        failed_outbox.status_of(&retry_key).unwrap(),
        Some(crate::outbox::OutboxStatus::Failed)
    );

    let held_dir = tempfile::tempdir().unwrap();
    let held_outbox =
        crate::outbox::Outbox::open(&held_dir.path().join("outbox.sqlite")).unwrap();
    let mut held_sink = crate::event_bridge::VecSink::default();
    let mut held = request.clone();
    held.request_id = "appt_22222222222222222222222222222222".into();
    emit_realtime_appointment_request(
        Some((&held_outbox, crate::event_bridge::EmitMode::RequireAck)),
        &mut held_sink,
        "call_held",
        "+61400000002",
        &held,
    )
    .unwrap();
    assert!(held_sink.lines.is_empty());
    let held_key = aokie_core::events::aokie_idempotency_key(
        "call_held",
        &format!("appointment.requested.{}", held.request_id),
    );
    assert_eq!(
        held_outbox.status_of(&held_key).unwrap(),
        Some(crate::outbox::OutboxStatus::Pending)
    );

    let mut no_outbox = crate::event_bridge::VecSink::default();
    assert!(emit_realtime_appointment_request(
        None,
        &mut no_outbox,
        "call_missing",
        "",
        &request,
    )
    .is_err());
    assert!(no_outbox.lines.is_empty());
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn realtime_business_lookup_poll_is_nonblocking_and_delivers_result() {
    let host = crate::host_rpc::HostRpc::new();
    let (id, _line, rx) = host.begin("flow.run", serde_json::json!({}));
    let mut pending = PendingBusinessLookup {
        host: Arc::clone(&host),
        id: Some(id),
        rx,
        deadline: Instant::now() + Duration::from_secs(5),
    };

    // The old recv_timeout paused the radio loop for the whole host
    // request. Repeated pending polls must remain immediate.
    let started = Instant::now();
    for _ in 0..512 {
        assert!(poll_business_lookup(&mut pending, Instant::now()).is_none());
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "pending host lookup blocked the simulated SCO ingress loop"
    );

    assert!(host.try_route_response(&serde_json::json!({
        "id": id,
        "result": {
            "status": "done",
            "result": { "digest": "OPEN 10:00", "spoken": "Ten is open." }
        }
    })));
    let (digest, spoken) = poll_business_lookup(&mut pending, Instant::now())
        .expect("completed host response becomes ready on the next poll");
    assert_eq!(digest, "OPEN 10:00");
    assert_eq!(spoken.as_deref(), Some("Ten is open."));
    assert_eq!(host.pending_count(), 0);
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn abandoned_realtime_business_lookup_forgets_its_host_request() {
    let host = crate::host_rpc::HostRpc::new();
    let (id, _line, rx) = host.begin("flow.run", serde_json::json!({}));
    let mut pending = PendingBusinessLookup {
        host: Arc::clone(&host),
        id: Some(id),
        rx,
        deadline: Instant::now(),
    };
    let (digest, spoken) = poll_business_lookup(&mut pending, Instant::now())
        .expect("expired lookup resolves without blocking");
    assert_eq!(digest, "LOOKUP UNAVAILABLE (timed out)");
    assert!(spoken.is_none());
    assert_eq!(host.pending_count(), 1);
    drop(pending);
    assert_eq!(host.pending_count(), 0);
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn realtime_tool_wait_pcm_is_bounded_coalesced_and_ordered() {
    const RATE: u32 = 16_000;
    // Reproduce the live one-second host lookup: ~133 narrow SCO frames
    // arrive while the function result is outstanding. None is forwarded
    // to auto-response VAD until the tool fence clears.
    let mut deferred = DeferredRealtimeInput::default();
    let mut expected = Vec::new();
    for frame in 0..133i16 {
        let samples = vec![frame; 120];
        expected.extend_from_slice(&samples);
        deferred.push(&samples, RATE);
    }

    // Release old audio before new audio and coalesce the tiny 7.5 ms SCO
    // packets into <=100 ms Realtime commands. The live one-second catch-
    // up therefore uses ten commands, not 133 entries in a depth-32 queue.
    let newest = vec![i16::MAX; 120];
    expected.extend_from_slice(&newest);
    deferred.push(&newest, RATE);
    let mut actual = Vec::new();
    let mut commands = 0;
    while !deferred.is_empty() {
        let chunk = deferred.take_flush_chunk(RATE);
        assert!(chunk.len() <= 1_600);
        actual.extend(chunk);
        commands += 1;
    }
    assert_eq!(actual, expected);
    assert_eq!(commands, 11);

    // Overflow keeps the NEWEST audio and never fails the session: a long
    // tool wait drops the oldest deferred samples instead of killing the
    // call (the pre-fix behavior returned a session-fatal Err here).
    let mut bounded = DeferredRealtimeInput::default();
    bounded.push(&vec![0; RATE as usize * 15], RATE);
    bounded.push(&[7; 4], RATE);
    let max_samples = RATE as usize * 15;
    assert_eq!(bounded.samples.len(), max_samples);
    let tail: Vec<i16> = bounded.samples.iter().rev().take(4).copied().collect();
    assert_eq!(tail, vec![7; 4]);
}

#[cfg(feature = "voice")]
#[test]
fn realtime_finish_requires_unchanged_caller_floor_and_an_audible_non_question_goodbye() {
    assert!(!realtime_tool_invalidated_by_caller(
        "request_appointment",
        4,
        4
    ));
    assert!(realtime_tool_invalidated_by_caller(
        "request_appointment",
        4,
        5
    ));
    // Read-only lookups are never voided by caller speech — discarding
    // them left the caller in dead air while the data was already fetched.
    assert!(!realtime_tool_invalidated_by_caller(
        "lookup_business_data",
        4,
        5
    ));
    assert!(!realtime_tool_invalidated_by_caller("finish_call", 4, 5));
    assert!(realtime_finish_call_allowed(true, true, 4, 4));
    assert!(!realtime_finish_call_allowed(false, true, 4, 4));
    assert!(!realtime_finish_call_allowed(true, false, 4, 4));
    assert!(!realtime_finish_call_allowed(true, true, 4, 5));

    assert!(realtime_finish_attempt_allowed(true, true, false, true, 0));
    assert!(!realtime_finish_attempt_allowed(true, true, false, true, 3));
    assert!(!realtime_finish_attempt_allowed(true, true, true, true, 0));
    assert!(!realtime_finish_attempt_allowed(false, true, false, true, 0));

    // Courtesy over the farewell proceeds; a resumed turn cancels.
    for courtesy in [
        "Hi.",
        "Bye",
        "Goodbye!",
        "Thank you so much",
        "thanks",
        "Take care",
        "you too",
        "Um",
    ] {
        assert!(
            !realtime_caller_turn_resumes_conversation(courtesy),
            "courtesy must not cancel the hangup: {courtesy}"
        );
    }
    for resumed in [
        "Wait",
        "No, hang on",
        "Actually one more thing",
        "Can you also book Monday",
        "What time was that?",
        "Hold on a second please",
        "Sorry, quick question",
    ] {
        assert!(
            realtime_caller_turn_resumes_conversation(resumed),
            "a resumed turn must cancel the hangup: {resumed}"
        );
    }

    let oversized = format!("{}{}", "\"é".repeat(8_000), "👋".repeat(8_000));
    let output = realtime_lookup_tool_output(true, &oversized, Some(&oversized));
    assert!(
        serde_json::to_vec(&output).unwrap().len()
            <= crate::realtime_voice::MAX_TOOL_OUTPUT_BYTES
    );

    assert!(realtime_farewell_can_arm(
        "item_1",
        "item_1",
        "Thanks for calling. Goodbye.",
        16_000,
        16_000,
        Duration::from_secs(1),
    ));
    assert!(!realtime_farewell_can_arm(
        "item_1",
        "item_2",
        "Goodbye.",
        16_000,
        16_000,
        Duration::from_secs(1),
    ));
    assert!(!realtime_farewell_can_arm(
        "item_1",
        "item_1",
        "Is there anything else?",
        16_000,
        16_000,
        Duration::from_secs(1),
    ));
    assert!(!realtime_farewell_can_arm(
        "item_1",
        "item_1",
        "Goodbye.",
        0,
        16_000,
        Duration::from_secs(1),
    ));
    assert!(!realtime_farewell_can_arm(
        "item_1",
        "item_1",
        "Goodbye.",
        1_000,
        16_000,
        Duration::from_secs(1),
    ));
}

#[cfg(feature = "voice")]
#[test]
fn realtime_responder_ownership_is_exact_and_never_implied_by_mode_alone() {
    assert!(!realtime_owns_call(false, None, Some("call_a")));
    assert!(realtime_owns_call(true, None, Some("call_a")));
    assert!(!realtime_owns_call(true, Some("call_a"), Some("call_a")));
    assert!(realtime_owns_call(
        true,
        Some("call_manager"),
        Some("call_customer")
    ));
    assert!(!realtime_owns_call(true, None, None));
}

#[cfg(feature = "voice")]
#[test]
fn realtime_waits_for_late_identity_before_beginning_or_reclassifying() {
    assert!(!realtime_identity_settled(
        false,
        ANSWER_ID_WAIT - std::time::Duration::from_millis(1)
    ));
    assert!(realtime_identity_settled(false, ANSWER_ID_WAIT));
    assert!(realtime_identity_settled(true, std::time::Duration::ZERO));
}

#[cfg(feature = "voice")]
#[test]
fn realtime_failure_policy_never_hangs_up_under_human_ownership() {
    assert_eq!(
        realtime_failure_disposition(false, false, false),
        RealtimeFailureDisposition::RingThrough
    );
    assert_eq!(
        realtime_failure_disposition(true, true, false),
        RealtimeFailureDisposition::FailSafe
    );
    assert_eq!(
        realtime_failure_disposition(true, false, true),
        RealtimeFailureDisposition::ResumeAfterHuman
    );
    assert_eq!(
        realtime_failure_disposition(true, false, false),
        RealtimeFailureDisposition::ResumeAfterHuman
    );
}

#[cfg(feature = "voice")]
#[test]
fn realtime_failsafe_never_waits_on_unproven_local_tts() {
    let proven = VoiceSelfTest {
        ok: true,
        at: "now".into(),
        duration_ms: 1,
        detail: "loopback ok - heard test".into(),
    };
    let skipped = VoiceSelfTest {
        detail: "skipped: Desktop realtime selected".into(),
        ..proven.clone()
    };
    assert!(realtime_failsafe_can_speak(None, Some(&proven)));
    assert!(!realtime_failsafe_can_speak(
        Some("TTS down"),
        Some(&proven)
    ));
    assert!(!realtime_failsafe_can_speak(None, Some(&skipped)));
    assert!(!realtime_failsafe_can_speak(None, None));
    assert!(!realtime_failsafe_answer_settled(None));
    assert!(!realtime_failsafe_answer_settled(Some(
        std::time::Duration::from_millis(899)
    )));
    assert!(realtime_failsafe_answer_settled(Some(
        std::time::Duration::from_millis(900)
    )));
}

#[cfg(feature = "voice")]
#[test]
fn realtime_keeps_local_audio_and_unsupported_backends_out_of_normal_lane() {
    assert!(!should_prepare_local_speech(true, false));
    assert!(should_prepare_local_speech(true, true));
    assert!(realtime_backend_error(true, false, "native").is_some());
    assert!(realtime_backend_error(true, true, "dongle").is_none());
    assert!(!should_send_answer_tone(true, true));
    assert!(should_send_answer_tone(true, false));
    assert!(!should_speak_legacy_resume(true, true));
    assert!(should_speak_legacy_resume(true, false));
    assert!(!screened_call_needs_tts("  "));
    assert!(screened_call_needs_tts("This number is blocked."));
    assert!(!realtime_error_is_terminal(false));
    assert!(realtime_error_is_terminal(true));
    assert!(should_resume_realtime_after_owner_loss(true));
    assert!(!should_resume_realtime_after_owner_loss(false));
    assert!(exact_failure_call(
        Some("call_a"),
        [None, Some("call_a"), None]
    ));
    assert!(!exact_failure_call(
        Some("call_b"),
        [None, Some("call_a"), None]
    ));
}

#[cfg(feature = "voice")]
#[test]
fn late_cancelled_output_is_tombstoned_until_exact_done() {
    let now = Instant::now();
    let mut pacer = crate::realtime_voice::OutputPacer::new(16_000);
    pacer.start_item("item_1", now).unwrap();
    pacer.push("item_1", &[1; 320]).unwrap();
    pacer.clear();
    let mut cancelled = realtime_retain_abandoned_output(None, Some("item_1".to_string()));

    // This is the integration event sequence after speech_started: late
    // PCM/transcript for the exact cancelled item are dropped, not pushed
    // into the now-cleared pacer (which would otherwise be a stale-item
    // terminal error).
    assert!(realtime_output_is_cancelled(cancelled.as_deref(), "item_1"));
    assert!(pacer.take_ready(now, usize::MAX).is_empty());
    assert!(!realtime_output_is_cancelled(
        cancelled.as_deref(),
        "item_2"
    ));
    // A second recoverable response error without an active provider item
    // must not erase the existing exact cancellation tombstone.
    cancelled = realtime_retain_abandoned_output(cancelled, None);
    assert_eq!(cancelled.as_deref(), Some("item_1"));
    if realtime_output_is_cancelled(cancelled.as_deref(), "item_1") {
        cancelled = None;
    }
    assert!(cancelled.is_none());
}

#[cfg(feature = "voice")]
#[test]
fn late_manager_requires_every_legacy_dependency() {
    assert!(legacy_manager_readiness_error(None, None, None).is_none());
    assert_eq!(
        legacy_manager_readiness_error(None, None, Some("LLM probe pending".into())),
        Some("LLM probe pending".into())
    );
    assert_eq!(
        legacy_manager_readiness_error(Some("TTS missing".into()), None, None),
        Some("TTS missing".into())
    );
}

#[test]
fn manager_terminal_flag_is_number_classification_not_outbound_authority() {
    assert!(manager_classification_flag(false, true));
    assert!(!manager_classification_flag(false, false));
    assert!(!manager_classification_flag(true, true));
}

#[test]
fn transcript_settlement_waits_for_every_correction() {
    let t0 = std::time::Instant::now();
    let mut tracker = TranscriptSettleTracker::default();
    tracker.correction_started("call_a");
    tracker.correction_started("call_a");
    tracker.call_ended("call_a", json!({"callId": "call_a"}), t0);

    assert!(tracker.take_ready(t0).is_empty());
    tracker.correction_finished("call_a");
    assert!(tracker.take_ready(t0).is_empty());
    tracker.correction_finished("call_a");

    let ready = tracker.take_ready(t0);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].call_id, "call_a");
    assert!(!ready[0].correction_timed_out);
    assert!(tracker.take_ready(t0).is_empty(), "settlement is one-shot");
}

#[test]
fn transcript_settlement_is_immediate_without_corrections_and_bounded_when_stuck() {
    let t0 = std::time::Instant::now();
    let mut tracker = TranscriptSettleTracker::default();
    tracker.call_ended("call_none", json!({"callId": "call_none"}), t0);
    let ready = tracker.take_ready(t0);
    assert_eq!(ready.len(), 1);
    assert!(!ready[0].correction_timed_out);

    tracker.correction_started("call_stuck");
    tracker.call_ended("call_stuck", json!({"callId": "call_stuck"}), t0);
    assert!(tracker
        .take_ready(t0 + TRANSCRIPT_SETTLE_TIMEOUT - std::time::Duration::from_millis(1))
        .is_empty());
    let timed_out = tracker.take_ready(t0 + TRANSCRIPT_SETTLE_TIMEOUT);
    assert_eq!(timed_out.len(), 1);
    assert!(timed_out[0].correction_timed_out);

    // A worker can still return and emit its corrected-turn event later,
    // but it cannot produce a second settlement.
    tracker.correction_finished("call_stuck");
    assert!(tracker
        .take_ready(t0 + TRANSCRIPT_SETTLE_TIMEOUT)
        .is_empty());
}

#[test]
fn transcript_shutdown_drain_emits_one_timed_out_barrier() {
    use crate::event_bridge::VecSink;

    TRANSCRIPT_SETTLEMENTS.with(|tracker| {
        let mut tracker = tracker.borrow_mut();
        *tracker = TranscriptSettleTracker::default();
        tracker.correction_started("call_shutdown");
        tracker.call_ended(
            "call_shutdown",
            json!({"callId": "call_shutdown", "outcome": "completed"}),
            std::time::Instant::now(),
        );
    });

    let mut sink = VecSink {
        fail: true,
        ..Default::default()
    };
    assert!(!emit_forced_transcript_settlements(None, &mut sink));
    assert!(sink.lines.is_empty(), "failed write emitted nothing");

    // A failed durable/write attempt retains the exact settlement; a
    // later graceful-shutdown pass can complete it before acknowledging.
    sink.fail = false;
    assert!(emit_forced_transcript_settlements(None, &mut sink));
    assert_eq!(sink.lines.len(), 1);
    let event: serde_json::Value = serde_json::from_str(&sink.lines[0]).unwrap();
    assert_eq!(
        event["params"]["event"]["name"],
        json!(crate::contract::events::CALL_TRANSCRIPT_SETTLED)
    );
    assert_eq!(
        event["params"]["event"]["data"]["transcriptCorrectionTimedOut"],
        json!(true)
    );

    assert!(emit_forced_transcript_settlements(None, &mut sink));
    assert_eq!(sink.lines.len(), 1, "shutdown drain is one-shot");
}

#[test]
fn transcript_settlement_outbox_quarantine_is_not_durable_success() {
    use crate::event_bridge::{EmitMode, VecSink};
    use crate::outbox::{OutboxStatus, PayloadProtection};

    let outbox = Outbox::open_in_memory_with_protection(PayloadProtection::Unavailable)
        .expect("in-memory outbox");
    let event = aokie_core::events::aokie_event(
        crate::contract::events::CALL_TRANSCRIPT_SETTLED,
        "call_quarantined",
        json!({"callId": "call_quarantined"}),
    );
    let mut sink = VecSink::default();

    assert!(!emit_transcript_settlement_durably(
        Some((&outbox, EmitMode::Legacy)),
        &mut sink,
        &event,
    ));
    assert!(sink.lines.is_empty(), "a quarantined payload must not emit");
    assert_eq!(
        outbox.status_of(&event.idempotency_key).unwrap(),
        Some(OutboxStatus::Dead)
    );
}

#[test]
fn transcript_settlement_outbox_payload_collision_is_not_durable_success() {
    use crate::event_bridge::{EmitMode, VecSink};
    use crate::outbox::{InsertOutcome, TARGET_DESKTOP};

    let outbox = Outbox::open_in_memory().expect("in-memory outbox");
    let first = aokie_core::events::aokie_event(
        crate::contract::events::CALL_TRANSCRIPT_SETTLED,
        "call_collision",
        json!({"callId": "call_collision", "revision": 1}),
    );
    assert_eq!(
        outbox.insert_pending(&first, TARGET_DESKTOP).unwrap(),
        InsertOutcome::Inserted
    );
    let conflicting = aokie_core::events::aokie_event(
        crate::contract::events::CALL_TRANSCRIPT_SETTLED,
        "call_collision",
        json!({"callId": "call_collision", "revision": 2}),
    );
    assert_eq!(first.idempotency_key, conflicting.idempotency_key);
    let mut sink = VecSink::default();

    assert!(!emit_transcript_settlement_durably(
        Some((&outbox, EmitMode::Legacy)),
        &mut sink,
        &conflicting,
    ));
    assert!(
        sink.lines.is_empty(),
        "the conflicting payload must not emit"
    );
    assert_eq!(outbox.collision_count().unwrap(), 1);
}

#[test]
fn transcript_settlement_is_durable_when_delivery_fails_after_insert() {
    use crate::event_bridge::{EmitMode, VecSink};
    use crate::outbox::OutboxStatus;

    let outbox = Outbox::open_in_memory().expect("in-memory outbox");
    let event = aokie_core::events::aokie_event(
        crate::contract::events::CALL_TRANSCRIPT_SETTLED,
        "call_delivery_pending",
        json!({"callId": "call_delivery_pending"}),
    );
    let mut sink = VecSink {
        fail: true,
        ..Default::default()
    };

    assert!(emit_transcript_settlement_durably(
        Some((&outbox, EmitMode::AckExpected)),
        &mut sink,
        &event,
    ));
    assert!(sink.lines.is_empty());
    assert_eq!(
        outbox.status_of(&event.idempotency_key).unwrap(),
        Some(OutboxStatus::Failed),
        "the durable row remains owned by outbox replay"
    );
}

#[cfg(target_os = "windows")]
struct CompanionEndCallerFixture {
    tracker: crate::call_session::SessionTracker,
    status: Arc<RadioStatus>,
    media: crate::remote_media::RemoteMediaHandle,
    request: CompanionEndCallerRequest,
}

#[cfg(target_os = "windows")]
fn companion_end_caller_fixture() -> CompanionEndCallerFixture {
    use aokie_media::{MediaMode, SessionBinding};

    let media = crate::remote_media::RemoteMediaHandle::spawn().unwrap();
    media.set_remote_consent(crate::remote_media::RemoteConsentGate {
        policy_id: crate::remote_media::REMOTE_CONSENT_POLICY_ID.into(),
        policy_version: crate::consent::CURRENT_CONSENT_VERSION,
        enabled: true,
        acknowledged: true,
        acknowledged_at: Some("2026-07-19T00:00:00Z".into()),
        expires_at: Some("2999-01-01T00:00:00Z".into()),
        captions_enabled: true,
        assistance_enabled: true,
        monitor_enabled: true,
        consult_enabled: true,
        takeover_enabled: true,
    });
    media.observe_physical_call(Some("call_end_a"), true);

    let prepared = SessionBinding {
        rtc_session_id: "rtc_end_a".into(),
        call_id: "call_end_a".into(),
        call_epoch: 1,
        owner_epoch: 0,
        device_id: "device_end_a".into(),
        mode: MediaMode::PreparedTalk,
        lease_id: Some("lease_end_a".into()),
        fence: 17,
    };
    media
        .install_test_prepared_peer(prepared.clone(), 20_000)
        .unwrap();
    media.ack_prepare_human(&prepared).unwrap();
    media
        .close_peer(&prepared.rtc_session_id, "active_rebind")
        .unwrap();
    let active = SessionBinding {
        owner_epoch: 1,
        mode: MediaMode::Talk,
        ..prepared
    };
    media
        .install_test_active_talk_peer(active.clone(), 20_000)
        .unwrap();
    let remote = media.snapshot();
    assert_eq!(
        remote.service_mode,
        crate::remote_media::ServiceMode::HumanActive
    );

    let mut tracker = crate::call_session::SessionTracker::new();
    tracker.ring(active.call_id.clone(), aokie_core::events::now_iso8601());
    tracker.answered();
    let status = Arc::new(RadioStatus::default());
    status.connected.store(true, Ordering::Release);
    status.call_active.store(true, Ordering::Release);
    *status.current_call_id.lock().unwrap() = Some(active.call_id.clone());

    let request = CompanionEndCallerRequest {
        call_id: active.call_id,
        call_epoch: active.call_epoch,
        owner_epoch: active.owner_epoch,
        switchboard_revision: status.switchboard_revision.load(Ordering::Acquire),
        remote_revision: remote.remote_revision,
        device_id: active.device_id,
        lease_id: active.lease_id.unwrap(),
        fence: active.fence,
    };
    CompanionEndCallerFixture {
        tracker,
        status,
        media,
        request,
    }
}

#[cfg(target_os = "windows")]
#[test]
fn companion_end_caller_enqueue_is_not_completion_and_timeout_fails_back() {
    let mut fixture = companion_end_caller_fixture();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let mut pending = None;
    let now = Instant::now();
    assert!(start_companion_end_caller(
        fixture.request.clone(),
        reply_tx,
        &mut fixture.tracker,
        fixture.status.as_ref(),
        &fixture.media,
        &mut pending,
        now,
        || Ok(()),
    ));
    assert!(matches!(
        reply_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    poll_companion_end_caller(
        &mut pending,
        &fixture.tracker,
        fixture.status.as_ref(),
        &fixture.media,
        now + COMPANION_END_CALL_CONFIRM_TIMEOUT - Duration::from_millis(1),
    );
    assert!(matches!(
        reply_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    poll_companion_end_caller(
        &mut pending,
        &fixture.tracker,
        fixture.status.as_ref(),
        &fixture.media,
        now + COMPANION_END_CALL_CONFIRM_TIMEOUT,
    );
    let failure = reply_rx.recv().unwrap().unwrap_err();
    assert_eq!(failure.code, "physical_hangup_timeout");
    assert_eq!(
        fixture.media.snapshot().service_mode,
        crate::remote_media::ServiceMode::ReturningToAokie
    );
    assert!(pending.is_none());
}

#[cfg(target_os = "windows")]
#[test]
fn companion_end_caller_exact_termination_completes_once() {
    let mut fixture = companion_end_caller_fixture();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let mut pending = None;
    assert!(start_companion_end_caller(
        fixture.request.clone(),
        reply_tx,
        &mut fixture.tracker,
        fixture.status.as_ref(),
        &fixture.media,
        &mut pending,
        Instant::now(),
        || Ok(()),
    ));
    resolve_companion_end_caller_termination(
        &mut pending,
        &fixture.media,
        "call_end_other",
        true,
    );
    assert!(matches!(
        reply_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    resolve_companion_end_caller_termination(
        &mut pending,
        &fixture.media,
        &fixture.request.call_id,
        true,
    );
    assert_eq!(reply_rx.recv().unwrap(), Ok(()));
    complete_companion_end_caller(&mut pending, &fixture.request.call_id);
    assert!(matches!(
        reply_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    ));
}

#[cfg(target_os = "windows")]
#[test]
fn companion_end_caller_link_loss_never_counts_as_completion() {
    let mut fixture = companion_end_caller_fixture();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let mut pending = None;
    assert!(start_companion_end_caller(
        fixture.request.clone(),
        reply_tx,
        &mut fixture.tracker,
        fixture.status.as_ref(),
        &fixture.media,
        &mut pending,
        Instant::now(),
        || Ok(()),
    ));
    resolve_companion_end_caller_termination(
        &mut pending,
        &fixture.media,
        &fixture.request.call_id,
        false,
    );
    let failure = reply_rx.recv().unwrap().unwrap_err();
    assert_eq!(failure.code, "physical_proof_lost");
    assert_eq!(
        fixture.media.snapshot().service_mode,
        crate::remote_media::ServiceMode::ReturningToAokie
    );
}

#[cfg(target_os = "windows")]
#[test]
fn companion_end_caller_write_failure_and_state_change_fail_back() {
    let mut write_fixture = companion_end_caller_fixture();
    let (write_tx, write_rx) = std::sync::mpsc::channel();
    let mut write_pending = None;
    assert!(start_companion_end_caller(
        write_fixture.request.clone(),
        write_tx,
        &mut write_fixture.tracker,
        write_fixture.status.as_ref(),
        &write_fixture.media,
        &mut write_pending,
        Instant::now(),
        || Ok(()),
    ));
    fail_companion_end_caller(
        &mut write_pending,
        &write_fixture.media,
        "radio_hangup_failed",
        "hangup: ACL write failed",
    );
    assert_eq!(
        write_rx.recv().unwrap().unwrap_err().code,
        "radio_hangup_failed"
    );
    assert_eq!(
        write_fixture.media.snapshot().service_mode,
        crate::remote_media::ServiceMode::ReturningToAokie
    );

    let mut changed_fixture = companion_end_caller_fixture();
    let (changed_tx, changed_rx) = std::sync::mpsc::channel();
    let mut changed_pending = None;
    assert!(start_companion_end_caller(
        changed_fixture.request.clone(),
        changed_tx,
        &mut changed_fixture.tracker,
        changed_fixture.status.as_ref(),
        &changed_fixture.media,
        &mut changed_pending,
        Instant::now(),
        || Ok(()),
    ));
    *changed_fixture.status.current_call_id.lock().unwrap() = Some("call_end_b".into());
    poll_companion_end_caller(
        &mut changed_pending,
        &changed_fixture.tracker,
        changed_fixture.status.as_ref(),
        &changed_fixture.media,
        Instant::now(),
    );
    assert_eq!(
        changed_rx.recv().unwrap().unwrap_err().code,
        "physical_call_changed"
    );
    assert_eq!(
        changed_fixture.media.snapshot().service_mode,
        crate::remote_media::ServiceMode::ReturningToAokie
    );
}

#[cfg(target_os = "windows")]
#[test]
fn companion_end_caller_duplicate_is_refused_without_disturbing_first_reply() {
    let mut fixture = companion_end_caller_fixture();
    let (first_tx, first_rx) = std::sync::mpsc::channel();
    let (second_tx, second_rx) = std::sync::mpsc::channel();
    let mut pending = None;
    assert!(start_companion_end_caller(
        fixture.request.clone(),
        first_tx,
        &mut fixture.tracker,
        fixture.status.as_ref(),
        &fixture.media,
        &mut pending,
        Instant::now(),
        || Ok(()),
    ));
    assert!(!start_companion_end_caller(
        fixture.request.clone(),
        second_tx,
        &mut fixture.tracker,
        fixture.status.as_ref(),
        &fixture.media,
        &mut pending,
        Instant::now(),
        || panic!("a duplicate must never enqueue another physical hangup"),
    ));
    assert_eq!(
        second_rx.recv().unwrap().unwrap_err().code,
        "end_caller_pending"
    );
    assert!(matches!(
        first_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    complete_companion_end_caller(&mut pending, &fixture.request.call_id);
    assert_eq!(first_rx.recv().unwrap(), Ok(()));
}

// ── AOK-CTRL-001: fake-clock deadline / silence / hangup-policy tests ──
// Every decision function takes `now` (the clock seam): tests fabricate
// instants by offsetting one base Instant — fully deterministic.

#[cfg(feature = "voice")]
use std::time::{Duration as D, Instant};

#[cfg(feature = "voice")]
#[test]
fn reply_side_effect_fence_rejects_an_actual_call_transition() {
    let remote = crate::remote_media::RemoteMediaHandle::spawn().unwrap();
    remote.observe_physical_call(Some("call_reply_a"), true);
    let owner = remote
        .aokie_owner_fence()
        .expect("the active Aokie call has an owner fence");
    assert!(aokie_owner_for_call(&remote, "call_reply_a").is_some());
    assert!(
        aokie_owner_for_call(&remote, "call_reply_b").is_none(),
        "a reply can never borrow authority from a different foreground leg"
    );
    assert!(reply_owner_is_current(&remote, Some(&owner)));

    // Drive the same public physical transition used by the radio loop.
    remote.observe_physical_call(Some("call_reply_b"), true);
    assert!(!reply_owner_is_current(&remote, Some(&owner)));
    let next = remote
        .aokie_owner_fence()
        .expect("the replacement Aokie call has its own fence");
    assert!(reply_owner_is_current(&remote, Some(&next)));
    assert!(!reply_owner_is_current(&remote, None));
}

#[cfg(feature = "voice")]
#[test]
fn no_sco_watchdog_times_the_continuous_outage_not_total_call_age() {
    let t0 = Instant::now();
    let mut watchdog = NoScoWatchdog::default();

    assert_eq!(
        watchdog.check(Some("call_a"), false, false, true, false, t0),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_millis(2_499),
        ),
        None
    );
    // The AT+BCC self-heal fires once per outage, well before the hangup.
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_millis(2_500),
        ),
        Some(NoScoAction::NudgeCodecConnection)
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_millis(7_999),
        ),
        None
    );

    // Recovery erases the old outage. A later drop gets a full grace
    // period even though the call itself is now much older than eight seconds.
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            true,
            false,
            true,
            false,
            t0 + D::from_secs(100),
        ),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(200),
        ),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_millis(202_600),
        ),
        Some(NoScoAction::NudgeCodecConnection),
        "the fresh outage re-arms the nudge"
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(208),
        ),
        Some(NoScoAction::HangUp)
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(220),
        ),
        None,
        "one outage emits at most one CHUP request"
    );
}

#[cfg(feature = "voice")]
#[test]
fn no_sco_owner_race_starts_a_fresh_aokie_grace_period() {
    let t0 = Instant::now();
    let mut watchdog = NoScoWatchdog::default();

    assert_eq!(
        watchdog.check(Some("call_a"), false, false, true, false, t0),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(3),
        ),
        Some(NoScoAction::NudgeCodecConnection)
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(8),
        ),
        Some(NoScoAction::HangUp)
    );

    // The physical action lost its exact-owner race because a complete
    // Companion claim-and-return crossed it. The replacement Aokie owner
    // must receive a whole new eight-second SCO recovery window.
    let returned_at = t0 + D::from_millis(8_250);
    watchdog.rearm_after_owner_race(returned_at);
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            returned_at + D::from_millis(2_600),
        ),
        Some(NoScoAction::NudgeCodecConnection),
        "the fresh owner window re-arms the audio self-heal too"
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            returned_at + D::from_millis(7_999),
        ),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            returned_at + D::from_secs(8),
        ),
        Some(NoScoAction::HangUp)
    );
}

#[cfg(feature = "voice")]
#[test]
fn no_sco_watchdog_returns_remote_owner_then_grants_fresh_aokie_grace() {
    let t0 = Instant::now();
    let mut watchdog = NoScoWatchdog::default();

    assert_eq!(
        watchdog.check(Some("call_a"), false, false, false, true, t0),
        Some(NoScoAction::RequestRemoteReturn)
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            false,
            true,
            t0 + D::from_secs(30),
        ),
        None,
        "remote ownership is never ended by the hardware watchdog"
    );

    // The authoritative return completed after a long outage. Its first
    // Aokie-owned tick starts a new grace period instead of hanging up.
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(31),
        ),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(34),
        ),
        Some(NoScoAction::NudgeCodecConnection),
        "the fresh grace window includes a fresh audio self-heal"
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(38),
        ),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(39),
        ),
        Some(NoScoAction::HangUp)
    );

    // An expected CHLD bounce resets rather than merely pauses the clock.
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            true,
            true,
            false,
            t0 + D::from_secs(40),
        ),
        None
    );
    assert_eq!(
        watchdog.check(
            Some("call_a"),
            false,
            false,
            true,
            false,
            t0 + D::from_secs(100),
        ),
        None
    );
}

/// sendAudio and audioTranscript are INDEPENDENT (2026-07-17): either
/// alone must arm the shared per-turn audio capture; the attach gate
/// stays sendAudio-only and the correction gate audioTranscript-only.
#[cfg(feature = "voice")]
#[test]
fn audio_lanes_are_independently_selectable() {
    // (agent, sendAudio env, audioTranscript env) → (attach, correct, capture)
    assert_eq!(audio_lane_gates(true, false, false), (false, false, false));
    // Direct audio only: attach + capture, NO side-run corrections.
    assert_eq!(audio_lane_gates(true, true, false), (true, false, true));
    // Corrections only (text-only reply model): capture still runs so
    // the correction lane has PCM to hear — no reply attach.
    assert_eq!(audio_lane_gates(true, false, true), (false, true, true));
    assert_eq!(audio_lane_gates(true, true, true), (true, true, true));
    // Both are agent-mode features: nothing arms without the agent.
    assert_eq!(audio_lane_gates(false, true, true), (false, false, false));
}

/// The reply watchdog names WHICH deadline expired: first-activity (the
/// endpoint accepted but never produced stream data), idle (mid-stream
/// stall — the per-read deadline VOICE-001 deferred), or total.
#[cfg(feature = "voice")]
#[test]
fn reply_deadlines_fire_by_phase_and_stay_quiet_on_progress() {
    let cfg = ReplyDeadlines {
        first_activity: D::from_secs(10),
        idle: D::from_secs(8),
        total: D::from_secs(60),
    };
    let t0 = Instant::now();

    // Healthy: fresh activity, inside every window.
    assert_eq!(
        reply_deadline_exceeded(&cfg, t0, Some(t0 + D::from_secs(29)), t0 + D::from_secs(30)),
        None
    );
    // No first token yet, but still inside the first-activity window.
    assert_eq!(
        reply_deadline_exceeded(&cfg, t0, None, t0 + D::from_secs(9)),
        None
    );
    // First-activity deadline.
    let msg = reply_deadline_exceeded(&cfg, t0, None, t0 + D::from_secs(10)).unwrap();
    assert!(msg.contains("first-activity"), "{msg}");
    // Idle (per-read) deadline: activity happened, then the stream stalled.
    let msg =
        reply_deadline_exceeded(&cfg, t0, Some(t0 + D::from_secs(5)), t0 + D::from_secs(13))
            .unwrap();
    assert!(msg.contains("idle deadline"), "{msg}");
    // Total deadline wins even with fresh activity (a stream that trickles
    // forever must still end).
    let msg =
        reply_deadline_exceeded(&cfg, t0, Some(t0 + D::from_secs(59)), t0 + D::from_secs(60))
            .unwrap();
    assert!(msg.contains("total deadline"), "{msg}");
}

/// The max-silence timer: first expiry prompts, a second silent window
/// hangs up, any activity resets BOTH the window and the prompt state,
/// and a zero window disables the timer entirely.
#[cfg(feature = "voice")]
#[test]
fn silence_timer_prompts_then_hangs_up_and_activity_resets() {
    let t0 = Instant::now();
    let mut timer = SilenceTimer::new(D::from_secs(30), t0);

    assert_eq!(timer.check(t0 + D::from_secs(29)), None);
    assert_eq!(
        timer.check(t0 + D::from_secs(30)),
        Some(SilenceAction::Prompt)
    );
    // The prompt restarted the window — not an instant hangup.
    assert_eq!(timer.check(t0 + D::from_secs(31)), None);
    assert_eq!(
        timer.check(t0 + D::from_secs(60)),
        Some(SilenceAction::HangUp)
    );

    // Activity after a prompt forgives it: the next expiry prompts again.
    let mut timer = SilenceTimer::new(D::from_secs(30), t0);
    assert_eq!(
        timer.check(t0 + D::from_secs(30)),
        Some(SilenceAction::Prompt)
    );
    timer.note_activity(t0 + D::from_secs(40));
    assert_eq!(timer.check(t0 + D::from_secs(69)), None);
    assert_eq!(
        timer.check(t0 + D::from_secs(70)),
        Some(SilenceAction::Prompt)
    );

    // Zero window = disabled.
    let mut off = SilenceTimer::new(D::ZERO, t0);
    assert_eq!(off.check(t0 + D::from_secs(3600)), None);
}

/// A human/consult route owns the conversational clock. Repeated reserved
/// passes reset both the elapsed window and an already-issued check-in, so
/// returning the caller to Aokie can never inherit a stale second-window
/// hangup.
#[cfg(feature = "voice")]
#[test]
fn silence_timer_gets_a_full_new_window_after_remote_ownership() {
    let t0 = Instant::now();
    let mut timer = SilenceTimer::new(D::from_secs(30), t0);
    assert_eq!(
        timer.check(t0 + D::from_secs(30)),
        Some(SilenceAction::Prompt)
    );

    // These note_activity calls model each main-loop pass while the radio
    // is reserved. The final one is immediately before Return to Aokie.
    timer.note_activity(t0 + D::from_secs(45));
    timer.note_activity(t0 + D::from_secs(90));

    assert_eq!(timer.check(t0 + D::from_secs(119)), None);
    assert_eq!(
        timer.check(t0 + D::from_secs(120)),
        Some(SilenceAction::Prompt),
        "return starts with a prompt window, never the stale hangup phase"
    );
}

/// The agent-hangup POLICY: the LLM's marker is only a request — barge,
/// operator ownership, the fail-safe, an unproven farewell and a farewell
/// that ASKS A QUESTION all veto it; a valid request waits out the
/// farewell's computed playout drain.
#[cfg(feature = "voice")]
#[test]
fn agent_hangup_policy_vetoes_and_computes_the_drain() {
    let t0 = Instant::now();
    let dur = D::from_secs(2);

    // No request → skip.
    assert!(matches!(
        agent_hangup_verdict(false, true, false, false, false, true, false, t0, dur, t0),
        HangupVerdict::Skip(_)
    ));
    // A stale/remote-owned route vetoes an otherwise valid request.
    let HangupVerdict::Skip(reason) =
        agent_hangup_verdict(true, false, false, false, false, true, false, t0, dur, t0)
    else {
        panic!("a stale Aokie owner fence must veto autonomous hangup");
    };
    assert!(reason.contains("owner fence"), "{reason}");
    // Barge / operator / fail-safe veto.
    for (barged, operator, failsafe) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        assert!(matches!(
            agent_hangup_verdict(
                true, true, barged, operator, failsafe, true, false, t0, dur, t0
            ),
            HangupVerdict::Skip(_)
        ));
    }
    // Farewell never played → the dead-air fail-safe owns the ending.
    assert!(matches!(
        agent_hangup_verdict(
            true,
            true,
            false,
            false,
            false,
            false,
            false,
            t0,
            D::ZERO,
            t0
        ),
        HangupVerdict::Skip(_)
    ));
    // Live report 2026-07-13: "Is there anything else I can help you
    // with? [[END_CALL]]" hung up on its own question — a farewell that
    // asks anything must WAIT for the answer instead.
    let verdict =
        agent_hangup_verdict(true, true, false, false, false, true, true, t0, dur, t0);
    let HangupVerdict::Skip(reason) = verdict else {
        panic!("a questioning farewell must not hang up");
    };
    assert!(
        reason.contains("question"),
        "reason names the cause: {reason}"
    );
    // Valid: the wait is the REMAINING playout + margin (queued 2s, 1s
    // already elapsed → ~1.4s), bounded.
    let HangupVerdict::Proceed { wait } = agent_hangup_verdict(
        true,
        true,
        false,
        false,
        false,
        true,
        false,
        t0,
        dur,
        t0 + D::from_secs(1),
    ) else {
        panic!("expected Proceed");
    };
    assert_eq!(wait, D::from_millis(1400));
}

/// Drain math: remaining playout + margin, zero once already drained,
/// capped for pathological durations.
#[cfg(feature = "voice")]
#[test]
fn playout_drain_wait_is_remaining_playout_bounded() {
    let t0 = Instant::now();
    // 3s queued, 1s elapsed → 2s remaining + 400ms margin.
    assert_eq!(
        playout_drain_wait(t0, D::from_secs(3), t0 + D::from_secs(1)),
        D::from_millis(2400)
    );
    // Fully drained long ago → zero (no blind sleep).
    assert_eq!(
        playout_drain_wait(t0, D::from_secs(1), t0 + D::from_secs(10)),
        D::ZERO
    );
    // Pathological queue → capped.
    assert_eq!(
        playout_drain_wait(t0, D::from_secs(120), t0),
        D::from_secs(8)
    );
}

/// The control probe: hangup/reject stop playback and are recorded (sticky);
/// every other control parks in arrival order for the main loop.
#[cfg(feature = "voice")]
#[test]
fn control_probe_catches_urgent_actions_and_parks_the_rest() {
    let (tx, rx) = std::sync::mpsc::channel::<RadioControl>();
    let mut parked = std::collections::VecDeque::new();
    let mut probe = ControlProbe::new(&rx, &mut parked);

    assert!(!probe.poll(), "no controls yet");

    tx.send(RadioControl::StopPairing).unwrap();
    tx.send(RadioControl::Hangup {
        op: Some("op_1".into()),
    })
    .unwrap();
    tx.send(RadioControl::StartPairing { seconds: 30 }).unwrap();

    assert!(probe.poll(), "hangup must stop playback");
    assert_eq!(
        probe.action,
        Some(CancelAction::Hangup {
            op: Some("op_1".into())
        })
    );
    // Sticky once set, and the pre-hangup control was parked in order.
    assert!(probe.poll());
    drop(probe);
    assert!(matches!(parked.front(), Some(RadioControl::StopPairing)));

    // Reject is urgent too.
    let mut parked = std::collections::VecDeque::new();
    let mut probe = ControlProbe::new(&rx, &mut parked);
    // The StartPairing sent above is still queued — it parks first.
    tx.send(RadioControl::Reject { op: None }).unwrap();
    assert!(probe.poll());
    assert_eq!(probe.action, Some(CancelAction::Reject { op: None }));
    drop(probe);
    assert!(matches!(
        parked.front(),
        Some(RadioControl::StartPairing { seconds: 30 })
    ));
}

/// VOICE-001: the dead-air decision — the fail-safe (apologise + hang up)
/// fires ONLY when nothing audibly played and nothing else explains the
/// silence. A barge means the caller is talking; an operator action means
/// a human owns the call; any audible sentence = transient, keep going.
#[cfg(feature = "voice")]
#[test]
fn dead_air_fires_only_on_unexplained_total_silence() {
    assert!(
        reply_left_dead_air(false, false, false),
        "total silence = dead air"
    );
    assert!(
        !reply_left_dead_air(true, false, false),
        "partial reply is transient"
    );
    assert!(
        !reply_left_dead_air(false, true, false),
        "barge = caller talking"
    );
    assert!(
        !reply_left_dead_air(false, false, true),
        "operator owns the call"
    );
    assert!(!reply_left_dead_air(true, true, true));
    // The canned apology must be non-trivial speech, not a stub.
    assert!(FALLBACK_LINE.len() > 40 && FALLBACK_LINE.contains("sorry"));
}

#[test]
fn spoofable_ani_never_grants_manager_access_without_pin() {
    assert!(!manager_access_allowed(false, true));
    assert!(!manager_access_allowed(true, false));
    assert!(!manager_access_allowed(false, false));
    assert!(manager_access_allowed(true, true));
}

/// Phase 1 abuse handling: the notice is fixed ASCII speech (straight to
/// TTS), the standing instruction teaches EXACTLY the marker the pump
/// detects, and it explicitly protects ordinary frustration from being
/// flagged. The prompt composer must always carry the rule.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn abuse_notice_and_instruction_are_wired() {
    assert!(ABUSE_LINE.len() > 30 && ABUSE_LINE.contains("abusive"));
    assert!(ABUSE_LINE.is_ascii(), "the notice goes straight to TTS");
    assert!(ABUSE_INSTRUCTION.contains("[[ABUSE]]"));
    assert!(ABUSE_INSTRUCTION.contains("NOT abuse"));
    let p = compose_agent_system_prompt("persona", false, None, false);
    assert!(p.contains("[[ABUSE]]"), "prompt must teach the marker");
    let p2 = compose_agent_system_prompt("persona", true, None, false);
    assert!(p2.contains("[[ABUSE]]"));
    // The MANAGER marker is taught ONLY on manager-number calls: an
    // ordinary caller's prompt must never mention it (live calls
    // 8c689f13 + 0576e7eb: own-bookings questions kept getting marked
    // manager-only despite ever-sharper challenge wording).
    assert!(
        !p.contains("[[MANAGER"),
        "ordinary calls must not be taught the manager marker"
    );
    let mgr = compose_agent_system_prompt("persona", false, None, true);
    assert!(
        mgr.contains("[[MANAGER:"),
        "manager-number calls still get the marker instruction"
    );
    assert!(is_exact_abuse_marker(" [[ABUSE]] \n"));
    for incomplete in ["[[ABUSE", "hello [[ABUSE]]", "[[ABUSE]] more", "[[abuse]]"] {
        assert!(!is_exact_abuse_marker(incomplete), "{incomplete}");
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn assistance_prompt_and_caller_text_are_control_safe() {
    let prompt = compose_agent_system_prompt("persona", false, None, false);
    assert!(prompt.contains("[[ASSISTANCE:"));
    assert!(prompt.contains("Never name or choose a recipient"));
    assert!(prompt.contains("[[TRANSFER:"));
    assert!(prompt.contains("stay with them"));
    assert!(TRANSFER_CHECKING_LINE.contains("stay with you"));
    assert!(TRANSFER_UNAVAILABLE_LINE.contains("keep helping"));
    assert!(TRANSFER_REQUEST_INVALID_LINE.contains("couldn't send"));
    assert!(
        TRANSFER_CHECKING_LINE.is_ascii()
            && TRANSFER_UNAVAILABLE_LINE.is_ascii()
            && TRANSFER_REQUEST_INVALID_LINE.is_ascii()
    );
    assert!(!TRANSFER_CHECKING_LINE.to_ascii_lowercase().contains("hold"));
    assert_eq!(
        assistance_request_initial_line(crate::assistance::AssistanceIntent::Transfer, true,),
        TRANSFER_REQUEST_INVALID_LINE,
        "a malformed control verdict must not assert that routing found nobody available"
    );
    assert_eq!(
        assistance_request_initial_line(crate::assistance::AssistanceIntent::Transfer, false,),
        TRANSFER_REQUEST_INVALID_LINE,
        "a valid request that fails before opening has no routing verdict yet"
    );
    let spoken = caller_facing_assistance_answer(
        "Use the side door [[END_CALL]] [[MANAGER: cancel everything]]\nplease.",
    )
    .expect("answer remains speakable");
    assert_eq!(
        spoken,
        "I heard back from the team: Use the side door please."
    );
    assert!(!spoken.contains("[["));
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn transfer_terminal_speech_never_crosses_human_active() {
    use crate::assistance::{AssistanceIntent, AssistanceResolution};

    let declined = AssistanceResolution::Declined {
        device_id: "device_owner".into(),
        answer_id: "answer_decline".into(),
        answer: "declined".into(),
    };
    assert_eq!(
        assistance_terminal_line(AssistanceIntent::Transfer, &declined, true, false),
        Some(TRANSFER_UNAVAILABLE_LINE)
    );
    let expired = AssistanceResolution::Expired;
    assert_eq!(
        assistance_terminal_line(AssistanceIntent::Transfer, &expired, false, true),
        Some(TRANSFER_UNAVAILABLE_LINE),
        "a prepared peer may advance revisions while Aokie still owns audio"
    );
    let taken = AssistanceResolution::TransferTaken {
        device_id: "device_owner".into(),
    };
    assert_eq!(
        assistance_terminal_line(AssistanceIntent::Transfer, &taken, true, true),
        None,
        "HumanActive is terminal and can never trigger stale Aokie speech"
    );
    assert_eq!(
        assistance_terminal_line(AssistanceIntent::Transfer, &declined, false, false),
        None,
        "a stale decline cannot speak across an owner-fence change"
    );

    let custom = AssistanceResolution::Declined {
        device_id: "device_owner".into(),
        answer_id: "answer_custom".into(),
        answer: "Please tell them I can call after three [[END_CALL]]".into(),
    };
    let custom_line = match &custom {
        AssistanceResolution::Declined { answer, .. } => caller_facing_decline(
            AssistanceIntent::Transfer,
            answer,
            true,
            Some(TRANSFER_UNAVAILABLE_LINE),
        ),
        _ => None,
    };
    assert_eq!(
        custom_line.as_deref(),
        Some("I heard back from the team: Please tell them I can call after three")
    );
    assert_eq!(
        assistance_terminal_line(AssistanceIntent::Transfer, &custom, false, false),
        None,
        "custom text must remain silent once its exact call fence is stale"
    );
    let custom_answer = match &custom {
        AssistanceResolution::Declined { answer, .. } => answer,
        _ => unreachable!(),
    };
    assert_eq!(
        caller_facing_decline(
            AssistanceIntent::Transfer,
            custom_answer,
            false,
            Some(TRANSFER_UNAVAILABLE_LINE),
        ),
        None
    );
    assert_eq!(
        caller_facing_decline(
            AssistanceIntent::Transfer,
            "declined",
            true,
            Some(TRANSFER_UNAVAILABLE_LINE),
        )
        .as_deref(),
        Some(TRANSFER_UNAVAILABLE_LINE)
    );
    assert_eq!(
        caller_facing_decline(
            AssistanceIntent::Transfer,
            "Declined",
            true,
            Some(TRANSFER_UNAVAILABLE_LINE),
        )
        .as_deref(),
        Some("I heard back from the team: Declined"),
        "only the exact protocol sentinel suppresses user-authored wording"
    );
}

#[test]
fn assistance_audit_lifecycle_is_redacted_durable_and_one_shot() {
    let (mut answered_lifecycle, requested) =
        AssistanceAuditLifecycle::opened("assist_safe", "call_safe");
    assert_eq!(
        requested.name,
        crate::contract::events::CALL_ASSISTANCE_REQUESTED
    );
    assert_eq!(requested.correlation_id, "call_safe");
    assert_eq!(
        requested.idempotency_key,
        "aokie:call_safe:assistance.requested.assist_safe:v1"
    );
    assert_eq!(requested.data["requestId"], json!("assist_safe"));
    assert_eq!(requested.data["callId"], json!("call_safe"));
    assert_eq!(requested.data["outcome"], json!("requested"));
    assert_eq!(requested.data["urgency"], json!("normal"));
    assert!(requested.data["at"].is_string());
    assert_eq!(requested.data.as_object().expect("object").len(), 5);

    let answered = answered_lifecycle
        .resolve(AssistanceAuditResolution::Answered("device_staff_1"))
        .expect("first resolution emits");
    assert_eq!(
        answered.name,
        crate::contract::events::CALL_ASSISTANCE_RESOLVED
    );
    assert_eq!(
        answered.idempotency_key,
        "aokie:call_safe:assistance.resolved.assist_safe:v1"
    );
    assert_eq!(answered.data["outcome"], json!("answered"));
    assert_eq!(answered.data["responderDeviceId"], json!("device_staff_1"));
    assert!(answered_lifecycle
        .resolve(AssistanceAuditResolution::Answered("device_staff_1"))
        .is_none());

    let (mut expired_lifecycle, _) =
        AssistanceAuditLifecycle::opened("assist_expired", "call_safe");
    let expired = expired_lifecycle
        .resolve(AssistanceAuditResolution::Expired)
        .expect("expiry emits");
    assert_eq!(expired.data["outcome"], json!("expired"));
    assert!(expired.data.get("responderDeviceId").is_none());
    assert!(expired_lifecycle
        .resolve(AssistanceAuditResolution::Expired)
        .is_none());

    let (mut transferred_lifecycle, _) =
        AssistanceAuditLifecycle::opened("assist_transfer", "call_safe");
    let transferred = transferred_lifecycle
        .resolve(AssistanceAuditResolution::Transferred("device_owner"))
        .expect("transfer emits once");
    assert_eq!(transferred.data["outcome"], json!("transferred"));
    assert_eq!(transferred.data["responderDeviceId"], json!("device_owner"));

    let (mut declined_lifecycle, _) =
        AssistanceAuditLifecycle::opened("assist_declined", "call_safe");
    let declined = declined_lifecycle
        .resolve(AssistanceAuditResolution::Declined("device_owner"))
        .expect("decline emits once");
    assert_eq!(declined.data["outcome"], json!("declined"));
    assert_eq!(declined.data["responderDeviceId"], json!("device_owner"));

    let (mut unavailable_lifecycle, _) =
        AssistanceAuditLifecycle::opened("assist_unavailable", "call_safe");
    let unavailable = unavailable_lifecycle
        .resolve(AssistanceAuditResolution::Unavailable)
        .expect("unavailable emits once");
    assert_eq!(unavailable.data["outcome"], json!("unavailable"));
    assert!(unavailable.data.get("responderDeviceId").is_none());

    // The audit constructor has no sensitive-text input and its exact
    // payload allow-list excludes every real-time assistance content key.
    for event in [
        &requested,
        &answered,
        &expired,
        &transferred,
        &declined,
        &unavailable,
    ] {
        let data = event.data.as_object().expect("object");
        for forbidden in ["question", "context", "answer", "transcript", "text"] {
            assert!(
                !data.contains_key(forbidden),
                "{forbidden} leaked into {}: {data:?}",
                event.name
            );
        }
    }
}

/// The agent-hangup end-call marker must be stripped from spoken/recorded
/// text (tolerant to small-model bracket/case variants) and its presence
/// detected so the plugin knows to hang up after the goodbye.
#[cfg(feature = "voice")]
#[test]
fn end_call_marker_stripping() {
    let (t, f) = strip_end_call_marker("Thanks, goodbye! [[END_CALL]]");
    assert_eq!(t, "Thanks, goodbye!");
    assert!(f);
    // bare + lowercase variant
    let (t, f) = strip_end_call_marker("See you soon. end_call");
    assert_eq!(t, "See you soon.");
    assert!(f);
    // single brackets, mixed case
    let (t, f) = strip_end_call_marker("Bye now [End_Call]");
    assert_eq!(t, "Bye now");
    assert!(f);
    // a marker-only sentence collapses to empty (nothing is spoken)
    let (t, f) = strip_end_call_marker("[[END_CALL]]");
    assert_eq!(t, "");
    assert!(f);
    // no marker: text is unchanged and the flag stays false
    let (t, f) = strip_end_call_marker("How else can I help?");
    assert_eq!(t, "How else can I help?");
    assert!(!f);
}

/// The bare tokens are a SUBSTRING of ordinary English, and an unanchored
/// search hung callers up mid-sentence while mangling the words on the way out:
/// "I'd recommend calling back" was spoken as "I'd recomming back" and then the
/// line dropped. Every phrase below is something a receptionist says.
#[cfg(feature = "voice")]
#[test]
fn an_ordinary_sentence_is_never_mistaken_for_the_end_call_marker() {
    for phrase in [
        "I'd recommend calling back tomorrow.",
        "We had a weekend call about that.",
        "A friend called earlier.",
        "I can attend calls until five.",
        "Please recommend calling the office.",
    ] {
        let (text, hangup) = strip_end_call_marker(phrase);
        assert_eq!(text, phrase, "the words must reach the caller unaltered");
        assert!(!hangup, "{phrase:?} must not hang up the call");
    }

    // The marker still works when it stands on its own, whatever punctuation
    // or bracketing surrounds it.
    for (input, want) in [
        ("Thanks, goodbye. END_CALL", "Thanks, goodbye."),
        ("Thanks, goodbye. end call", "Thanks, goodbye."),
        ("Bye! (END_CALL)", "Bye! ()"),
        ("Bye now [[end_call]]", "Bye now"),
    ] {
        let (text, hangup) = strip_end_call_marker(input);
        assert_eq!(text, want, "input {input:?}");
        assert!(hangup, "{input:?} must still end the call");
    }
}

/// AK-008: the barge scan must CAPTURE the audio it inspects and remember
/// where speech started, so the caller's words spoken over Aokie are
/// prepended to their turn instead of being consumed by detection.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn barge_scan_captures_audio_and_marks_speech_start() {
    let frame = 80usize; // 10 ms @ 8 kHz
    let mut speech_frames = 0u32;
    let mut captured: Vec<i16> = Vec::new();
    let mut speech_start: Option<usize> = None;

    // 1) Silence first: captured grows, no speech start, no trip.
    let silence = vec![0i16; frame * 5];
    let tripped = scan_barge_frames(
        &silence,
        frame,
        350.0,
        500.0,
        true,
        &mut speech_frames,
        3,
        &mut captured,
        &mut speech_start,
    );
    assert!(!tripped);
    assert_eq!(captured.len(), frame * 5);
    assert_eq!(speech_start, None);
    assert_eq!(speech_frames, 0);

    // 2) Loud speech: start is marked at ITS offset (after the silence),
    //    and 3 sustained frames trip the barge.
    let loud = vec![8000i16; frame * 3];
    let tripped = scan_barge_frames(
        &loud,
        frame,
        350.0,
        500.0,
        true,
        &mut speech_frames,
        3,
        &mut captured,
        &mut speech_start,
    );
    assert!(tripped);
    assert_eq!(
        speech_start,
        Some(frame * 5),
        "speech starts where the loud audio began"
    );
    // The loud chunk was captured too — nothing was consumed by detection.
    assert_eq!(captured.len(), frame * 8);

    // 3) QUIET speech (above the capture gate, below the trip threshold —
    //    the AEC-suppressed-overlap band): marked + captured for the
    //    scratchpad, but never barges.
    let mut frames2 = 0u32;
    let mut cap2: Vec<i16> = Vec::new();
    let mut start2: Option<usize> = None;
    let quiet = vec![420i16; frame * 6];
    let tripped = scan_barge_frames(
        &quiet,
        frame,
        350.0,
        500.0,
        true,
        &mut frames2,
        3,
        &mut cap2,
        &mut start2,
    );
    assert!(!tripped, "sub-trip speech never barges");
    assert_eq!(start2, Some(0), "but the scratchpad hears it");
}

/// Span interrupt policy: a Yield span stops the instant the barge trips;
/// a FinishSpan span (phone number, [[important]] detail) keeps playing
/// through its bounded extension and then stops; an urgent control
/// (hangup/reject) stops everything regardless of policy.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn playback_policy_yield_vs_finish_span() {
    use std::time::{Duration, Instant};
    let now = Instant::now();
    // Yield: barge = stop now.
    let mut p = TtsChunkPlayback::new(8000, None);
    assert!(!p.stop_playback_now(now), "nothing happened yet");
    p.barged = true;
    p.barged_at = Some(now);
    assert!(p.stop_playback_now(now), "yield stops on the trip");

    // FinishSpan: barge = keep going until the budget is spent.
    let mut p = TtsChunkPlayback::new(8000, Some(Duration::from_millis(1500)));
    p.barged = true;
    p.barged_at = Some(now);
    assert!(!p.stop_playback_now(now), "inside the finish budget");
    assert!(
        p.stop_playback_now(now + Duration::from_millis(1600)),
        "budget spent — yield"
    );

    // Cancelled (urgent control) always stops, policy notwithstanding.
    let mut p = TtsChunkPlayback::new(8000, Some(Duration::from_secs(5)));
    p.cancelled = true;
    assert!(p.stop_playback_now(now), "hangup/reject beats protection");

    // A spoken floor command (probe lane) also beats protection — the
    // caller's explicit "stop" cuts even an [[important]] span instantly.
    let mut p = TtsChunkPlayback::new(8000, Some(Duration::from_secs(5)));
    p.barged = true;
    p.barged_at = Some(now);
    assert!(
        !p.stop_playback_now(now),
        "ordinary overlap rides the budget"
    );
    p.semantic = Some(crate::duplex::CallerIntent::StopSpeaking);
    assert!(p.stop_playback_now(now), "spoken command beats protection");
}

/// Guide phase 5: hypothesis stability + adoption rules for speculative
/// reply generation — stable = the previous partial stayed a (near-)prefix
/// and carries intent; covers = the final turn matched what the model was
/// answering, with at most a short unseen tail.
#[cfg(feature = "voice")]
#[test]
fn speculative_hypothesis_rules() {
    // Stability: prefix held across two partials, >=4 words.
    assert!(hypothesis_stable(
        "do you have any tables",
        "do you have any tables free on Friday"
    ));
    assert!(
        !hypothesis_stable("do you", "do you have any tables"),
        "too short to speculate"
    );
    assert!(
        !hypothesis_stable(
            "can I change my booking",
            "can I cancel the whole thing and"
        ),
        "a rewritten head is NOT stable"
    );
    // One mid-prefix STT wobble is tolerated.
    assert!(hypothesis_stable(
        "do you have any tables",
        "do you have eny tables free"
    ));
    // Head-focused (2026-07-14 tune): a 3-word stable head speculates,
    // and a long prev whose TAIL flickers still counts — only the head
    // must hold.
    assert!(hypothesis_stable("book a table", "book a table for two"));
    assert!(hypothesis_stable(
        "do you have any tables free maybe Friday",
        "do you have any tables free on Friday night"
    ));

    // Adoption: the final said what the hypothesis said (+ short tail).
    assert!(hypothesis_covers(
        "do you have any tables free on Friday",
        "Do you have any tables free on Friday night?"
    ));
    assert!(
        !hypothesis_covers(
            "do you have any tables free on Friday",
            "do you have any tables free on Friday actually make that Saturday around six"
        ),
        "a long unseen tail is a material revision"
    );
    assert!(
        !hypothesis_covers("book me for Friday", "cancel my booking for Friday"),
        "a diverged head must regenerate"
    );
    // Case/punctuation never break the match.
    assert!(hypothesis_covers(
        "what time do you open tomorrow",
        "What time do you open tomorrow?"
    ));
}

/// Only the exact FormLogic Codex live-call routes disable interim-STT
/// generation. Equivalent loopback spellings and one-pass encoded route
/// ids reach the same handler; unrelated providers keep speculation even
/// when their hostname, path or query merely contains "codex".
#[cfg(feature = "voice")]
#[test]
fn speculative_llm_route_gate_is_exact() {
    for endpoint in [
        crate::connector::CODEX_LIVE_CALL_ENDPOINT_NONE,
        crate::connector::CODEX_LIVE_CALL_ENDPOINT_LOW,
        crate::connector::CODEX_LIVE_CALL_ENDPOINT_LUNA_LOW,
        crate::connector::CODEX_LIVE_CALL_ENDPOINT_LUNA_LOW_FAST,
        "http://localhost:17872/api/ai/providers/openai-codex-agent-none/v1/chat/completions?request=1#ignored",
        "https://[::1]:17872/api/ai/providers/openai-codex-agent-low/v1/chat/completions",
        "http://127.0.0.2:17872/api/ai/providers/openai%2Dcodex-agent-none/v1/chat/completions",
        "http://[::ffff:127.0.0.1]:17872/api/ai/providers/%6fpenai-codex-agent-luna-low%2Dfast/v1/chat/completions",
    ] {
        assert!(
            !llm_endpoint_allows_speculative_reply(endpoint),
            "reserved route must not speculate: {endpoint}"
        );
    }

    for endpoint in [
        "http://127.0.0.1:8080/v1/chat/completions?model=codex",
        "https://codex.example.com/v1/chat/completions",
        "http://127.0.0.1:17872/api/ai/providers/my-codex-model/v1/chat/completions",
        "http://127.0.0.1:17872/api/ai/providers/openai-codex-agent-none/v1/chat/completions/",
        "http://127.0.0.1:17873/api/ai/providers/openai-codex-agent-low/v1/chat/completions",
        "http://127.0.0.1:17872/api/ai/providers/openai%252Dcodex-agent-none/v1/chat/completions",
    ] {
        assert!(
            llm_endpoint_allows_speculative_reply(endpoint),
            "ordinary/near-miss provider must keep speculation: {endpoint}"
        );
    }
}

/// Ring-time personalization window: auto-answer waits for +CLIP (short)
/// and then the overlay (bounded), and the overlay's arrival always wins.
#[cfg(feature = "voice")]
#[test]
fn auto_answer_waits_one_ring_for_personalization() {
    use std::time::Duration as D3;
    // No caller id yet: wait, but only inside the short id window.
    assert!(hold_auto_answer(false, false, D3::from_millis(300)));
    assert!(
        !hold_auto_answer(false, false, D3::from_millis(1300)),
        "withheld numbers answer after ~one ring"
    );
    // Id known, flow still running: wait up to the overlay budget.
    assert!(hold_auto_answer(true, false, D3::from_millis(1800)));
    assert!(
        !hold_auto_answer(true, false, D3::from_millis(2600)),
        "the budget is hard"
    );
    // Overlay ready: answer NOW.
    assert!(!hold_auto_answer(true, true, D3::from_millis(100)));
}

/// §9.3: the greeting hold waits for the overlay only while the caller
/// id is known, the overlay is missing, and the bounded cap has time left.
/// Live call 085ce239: a PIN said digit by digit split across STT turns
/// and each fragment was judged (and failed) alone. Partial fragments must
/// COLLECT; a full-length total (or a no-digit turn) judges.
#[cfg(feature = "voice")]
#[test]
fn pin_gate_collects_split_digit_fragments_until_full_length() {
    let mut acc = String::new();
    // "one two" … "three" … "four" against a 4-digit PIN.
    assert!(matches!(pin_gate_step(&mut acc, "12", 4), PinStep::Collect));
    assert!(matches!(pin_gate_step(&mut acc, "3", 4), PinStep::Collect));
    match pin_gate_step(&mut acc, "4", 4) {
        PinStep::Judge(given) => assert_eq!(given, "1234"),
        PinStep::Collect => panic!("full-length attempt must judge"),
    }
    assert!(acc.is_empty(), "judged attempt consumes the accumulator");

    // A single full-length utterance judges immediately (no regression).
    match pin_gate_step(&mut acc, "1234", 4) {
        PinStep::Judge(given) => assert_eq!(given, "1234"),
        PinStep::Collect => panic!("exact-length attempt must judge"),
    }

    // A digit-free turn judges whatever accumulated (an "I don't know"
    // still consumes the attempt — same as before).
    assert!(matches!(pin_gate_step(&mut acc, "12", 4), PinStep::Collect));
    match pin_gate_step(&mut acc, "", 4) {
        PinStep::Judge(given) => assert_eq!(given, "12"),
        PinStep::Collect => panic!("no-digit turn must judge"),
    }

    // Over-long stream judges (and fails downstream) instead of growing forever.
    match pin_gate_step(&mut acc, "123456", 4) {
        PinStep::Judge(given) => assert_eq!(given, "123456"),
        PinStep::Collect => panic!("overshoot must judge"),
    }

    // A blank configured PIN never collects (the gate refuses separately).
    assert!(matches!(
        pin_gate_step(&mut acc, "12", 0),
        PinStep::Judge(_)
    ));
}

/// The bare-PIN fast path must accept a turn that is ONLY the PIN (with
/// harmless filler) and reject ordinary sentences whose incidental digits
/// happen to add up to the right count.
#[cfg(feature = "voice")]
#[test]
fn bare_pin_detector_accepts_pin_only_turns_and_rejects_sentences() {
    assert!(looks_like_bare_pin("One, two, three, four.", 4));
    assert!(looks_like_bare_pin(
        "my manager pin is one two three four",
        4
    ));
    assert!(looks_like_bare_pin("1234", 4));
    // Real content words reject — this has digits "4322" but is a booking.
    assert!(!looks_like_bare_pin("yes 4 people at 3 pm on the 22nd", 4));
    // Wrong length rejects (judged only via the prompted gate).
    assert!(!looks_like_bare_pin("one two three", 4));
    // No PIN configured never matches.
    assert!(!looks_like_bare_pin("1234", 0));
    // A plain sentence with no digits never matches.
    assert!(!looks_like_bare_pin("what appointments are booked", 4));
}

/// Live call 8576ba9e (2026-07-18): an overlap estimate that overshoots
/// to (or past) the interrupted bot line's own speech start flips the
/// visible transcript order — the clamp keeps the caller turn strictly
/// after the line it was spoken over.
#[cfg(feature = "voice")]
#[test]
fn overlap_backdate_never_reaches_the_interrupted_lines_start() {
    // Estimate overshoots the whole playback window: clamped to margin.
    assert_eq!(
        overlap_backdate_ms(5000, 5000),
        5000 - OVERLAP_ORDER_MARGIN_MS
    );
    // Estimate deeper than the window (pre-roll/noise pollution): clamped.
    assert_eq!(
        overlap_backdate_ms(9000, 4000),
        4000 - OVERLAP_ORDER_MARGIN_MS
    );
    // A genuine mid-line interruption keeps its honest estimate.
    assert_eq!(overlap_backdate_ms(1200, 5000), 1200);
    // Degenerate tiny window never underflows.
    assert_eq!(overlap_backdate_ms(500, 30), 0);
}

/// AOKIE_GREETING_SETTLE_MS parse: default 700, 0 disables, capped 3000.
#[cfg(feature = "voice")]
#[test]
fn greeting_settle_env_parses_with_default_and_cap() {
    assert_eq!(parse_greeting_settle_ms(None), 700);
    assert_eq!(parse_greeting_settle_ms(Some("garbage")), 700);
    assert_eq!(parse_greeting_settle_ms(Some("")), 700);
    assert_eq!(parse_greeting_settle_ms(Some("0")), 0);
    assert_eq!(parse_greeting_settle_ms(Some(" 500 ")), 500);
    assert_eq!(parse_greeting_settle_ms(Some("99999")), 3000);
}

#[cfg(feature = "voice")]
#[test]
fn greeting_holds_briefly_for_the_personalization_overlay() {
    use std::time::Duration as D2;
    let cap = D2::from_millis(1500);
    // Known caller, no overlay yet, inside the cap: hold.
    assert!(hold_greeting_for_overlay(
        false,
        true,
        D2::from_millis(200),
        cap
    ));
    // Overlay arrived: speak NOW (personalized).
    assert!(!hold_greeting_for_overlay(
        true,
        true,
        D2::from_millis(200),
        cap
    ));
    // Cap spent: speak the default — a slow flow never buys dead air.
    assert!(!hold_greeting_for_overlay(
        false,
        true,
        D2::from_millis(1600),
        cap
    ));
    // Caller id withheld: no push is coming — never hold.
    assert!(!hold_greeting_for_overlay(
        false,
        false,
        D2::from_millis(200),
        cap
    ));
}

/// §6.3 delivery-truth v1: the audible-prefix estimator UNDERCLAIMS —
/// duration-weighted against exact totals when synthesis finished,
/// capped by a chars-per-second ceiling, floored to a word boundary.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn audible_prefix_estimate_is_conservative() {
    let text = "Your appointment is on Thursday at ten in the morning";
    // Cut ~40% through a 4s span whose synthesis finished: claims a
    // word-floored prefix, never the whole sentence, flagged uncertain.
    let (prefix, uncertain) = estimate_audible_prefix(
        text,
        1.0,
        &CutEstimate {
            audible_ms: 1600,
            queued_ms: 1800,
            synthesized_ms: Some(4000),
        },
    );
    assert!(uncertain);
    assert!(text.starts_with(&prefix), "estimate must be a prefix");
    assert!(
        prefix.len() < text.len(),
        "a mid-span cut must not claim everything"
    );
    assert!(!prefix.is_empty(), "1.6s of audio heard something");
    assert!(!prefix.ends_with(char::is_whitespace));
    // The prefix always ends on a WORD boundary of the original text.
    assert!(
        text[prefix.len()..].starts_with(' '),
        "must cut at a word boundary"
    );

    // The cut landed after everything played out: the whole span was
    // plausibly heard — full text, certain.
    let (all, uncertain) = estimate_audible_prefix(
        text,
        1.0,
        &CutEstimate {
            audible_ms: 4000,
            queued_ms: 4000,
            synthesized_ms: Some(4000),
        },
    );
    assert_eq!(all, text);
    assert!(!uncertain);

    // Synthesis NOT finished (total unknown): the chars-per-second
    // ceiling alone drives it — 300ms at rate 1.0 is a couple of words
    // at most, never half the sentence.
    let (short, uncertain) = estimate_audible_prefix(
        text,
        1.0,
        &CutEstimate {
            audible_ms: 300,
            queued_ms: 500,
            synthesized_ms: None,
        },
    );
    assert!(uncertain);
    assert!(
        short.len() <= 5,
        "300ms cannot claim more than ~4 chars, got {short:?}"
    );

    // Nothing audible = nothing claimed.
    let (none, _) = estimate_audible_prefix(
        text,
        1.0,
        &CutEstimate {
            audible_ms: 0,
            queued_ms: 0,
            synthesized_ms: None,
        },
    );
    assert!(none.is_empty());

    // A slower span rate lowers the ceiling proportionally.
    let (slow, _) = estimate_audible_prefix(
        text,
        0.5,
        &CutEstimate {
            audible_ms: 1000,
            queued_ms: 1200,
            synthesized_ms: None,
        },
    );
    let (fast, _) = estimate_audible_prefix(
        text,
        1.0,
        &CutEstimate {
            audible_ms: 1000,
            queued_ms: 1200,
            synthesized_ms: None,
        },
    );
    assert!(slow.len() <= fast.len());
}

/// Phase 2 pacing: the first chunk always goes (it starts the playout
/// clock); after that the queued total may lead real playout by at most
/// the configured lead — cancel latency is bounded by that lead.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn playout_pacing_bounds_the_queue_lead() {
    use std::time::Duration;
    let lead = Duration::from_millis(200);
    // First chunk: always allowed.
    assert!(may_queue_more(true, 0, 8000, Duration::ZERO, lead));
    // 1s queued, 900ms played: 100ms ahead — under the lead, may queue.
    assert!(may_queue_more(
        false,
        8000,
        8000,
        Duration::from_millis(900),
        lead
    ));
    // 1s queued, 700ms played: 300ms ahead — over the lead, must wait.
    assert!(!may_queue_more(
        false,
        8000,
        8000,
        Duration::from_millis(700),
        lead
    ));
    // Exactly at the boundary: not strictly under — wait.
    assert!(!may_queue_more(
        false,
        8000,
        8000,
        Duration::from_millis(800),
        lead
    ));
}

/// AK-008: un-armed frames (AEC convergence grace / pre-first-audio) are
/// still captured AND marked for the scratchpad — they may hold the
/// caller's first words — but never trip the barge.
#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn barge_scan_unarmed_captures_but_never_trips() {
    let frame = 80usize;
    let mut speech_frames = 0u32;
    let mut captured: Vec<i16> = Vec::new();
    let mut speech_start: Option<usize> = None;
    let loud = vec![8000i16; frame * 10];
    let tripped = scan_barge_frames(
        &loud,
        frame,
        350.0,
        500.0,
        false,
        &mut speech_frames,
        3,
        &mut captured,
        &mut speech_start,
    );
    assert!(!tripped);
    assert_eq!(captured.len(), frame * 10);
    assert_eq!(
        speech_start,
        Some(0),
        "the scratchpad hears pre-audio speech"
    );
    assert_eq!(speech_frames, 0, "but the barge counter never arms");
}

/// Audit AK-008: the continuation heuristic must hold a turn open exactly
/// when the caller sounds mid-number — digit groups, spoken digits, and
/// the connectives that announce one — and never for a finished sentence.
#[cfg(feature = "voice")]
#[test]
fn unfinished_number_heuristic() {
    // The live failure: a phone number read in groups with pauses.
    assert!(ends_with_unfinished_number("my number is 0412"));
    assert!(ends_with_unfinished_number("it's 0412 345"));
    assert!(ends_with_unfinished_number("zero four one two"));
    assert!(ends_with_unfinished_number("you can reach me on 0412, 345"));
    assert!(ends_with_unfinished_number("double four"));
    assert!(ends_with_unfinished_number("my number is"));
    assert!(ends_with_unfinished_number("you can call me on"));
    // Finished turns must flush immediately — no added latency.
    assert!(!ends_with_unfinished_number("I'd like to book a haircut"));
    assert!(!ends_with_unfinished_number("yes that's right"));
    assert!(!ends_with_unfinished_number("my name is Lance"));
    assert!(!ends_with_unfinished_number(""));
    assert!(!ends_with_unfinished_number("   "));
    // A word after the digits releases the hold.
    assert!(!ends_with_unfinished_number("nine thirty tomorrow"));
    assert!(!ends_with_unfinished_number("0412 345 678 thanks"));
}

/// Plan §6.2 (live 2026-07-13): the general turn-completion hold — number
/// tails (as before), bare hesitations, trailing connectives and short
/// filler-opened fragments all hold for a continuation; complete answers
/// still flush instantly.
#[cfg(feature = "voice")]
#[test]
fn general_turn_completion_hold() {
    // The live failure: "Um no" + "That's all really" split into two turns.
    assert!(turn_looks_unfinished("Um no"));
    assert!(turn_looks_unfinished("Uh"));
    assert!(turn_looks_unfinished("Well"));
    assert!(turn_looks_unfinished("I want to change it but"));
    assert!(turn_looks_unfinished("we could do Tuesday or"));
    // Number tails still hold (the original AK-008 behaviour).
    assert!(turn_looks_unfinished("my number is 0412"));
    // Complete answers flush immediately.
    assert!(!turn_looks_unfinished("That's good."));
    assert!(!turn_looks_unfinished("No, that's all really"));
    assert!(!turn_looks_unfinished(
        "I'd like to book a haircut for Tuesday"
    ));
    assert!(!turn_looks_unfinished("yes"));
}

/// Bare hesitations are the caller THINKING: recorded, never answered.
#[cfg(feature = "voice")]
#[test]
fn hesitations_are_silence_not_turns() {
    for s in ["Uh", "um", "Well...", "hmm", "uh um"] {
        assert!(crate::duplex::is_hesitation(s), "{s:?}");
    }
    for s in ["Um no", "well yes", "no", "that's all", ""] {
        assert!(!crate::duplex::is_hesitation(s), "{s:?}");
    }
}

/// Phase 2, the 2026-07-14 silent-callback incident replayed: the
/// previous ring's STALE CallTerminated (held verdict discharging after
/// our ATD) must not kill the fresh dial session, the setup indicator
/// attaches to the REAL dial (agent-owned, ORIGINAL call id) when the
/// session was lost anyway, and a terminate after ANSWER is real again.
#[test]
fn stale_terminate_never_kills_a_fresh_dial_and_setup_reattaches() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    // The Dial arm's work: agent-owned session + in-flight context.
    tracker.dial(
        "call_dial1".into(),
        Some("0491570156".into()),
        "x".into(),
        true,
    );
    *status.pending_dial.lock().unwrap() = Some(PendingDial {
        call_id: "call_dial1".into(),
        number: "0491570156".into(),
        at: std::time::Instant::now(),
    });

    // The stale terminate (100ms after ATD in the incident): IGNORED —
    // the session survives and no call.ended is emitted.
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    assert_eq!(
        tracker.call_id(),
        Some("call_dial1"),
        "dial session survives"
    );
    assert!(
        !sink
            .lines
            .iter()
            .any(|l| l.contains(crate::contract::events::CALL_ENDED)),
        "no spurious call.ended: {:?}",
        sink.lines
    );

    // Even if the session HAD been lost, the setup indicator re-attaches
    // to the pending dial instead of minting an observed session.
    let mut lost = crate::call_session::SessionTracker::new();
    handle_event(E::OutgoingDialing, &mut lost, None, &mut sink, &status);
    let s = lost.current().expect("session re-created");
    assert_eq!(
        s.id, "call_dial1",
        "ORIGINAL call id (overlay + event correlation)"
    );
    assert!(s.agent_owned, "agent owns the re-attached call");
    assert_eq!(s.caller_id.as_deref(), Some("0491570156"));
    assert_eq!(
        status.outbound_call_id.lock().unwrap().as_deref(),
        Some("call_dial1")
    );

    // Answer clears the in-flight context — a terminate is REAL now.
    handle_event(E::CallAnswered, &mut lost, None, &mut sink, &status);
    assert!(
        status.pending_dial.lock().unwrap().is_none(),
        "context consumed"
    );
    assert_eq!(
        status.outbound_call_id.lock().unwrap().as_deref(),
        Some("call_dial1")
    );
    handle_event(E::CallTerminated, &mut lost, None, &mut sink, &status);
    assert!(
        lost.current().is_none(),
        "post-answer terminate ends the call"
    );
}

/// Audit C-01/C-02/AK-001: the radio publishes the current call's identity
/// (`callId` + `startedAt`) into the shared status the moment it rings
/// and clears it on termination — `call.current` and the call-control
/// `callId` guard read exactly these fields — and the session state
/// machine decides the ended outcome.
#[test]
fn handle_event_tracks_shared_call_identity() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    macro_rules! apply {
        ($ev:expr) => {
            handle_event($ev, &mut tracker, None, &mut sink, &status)
        };
    }

    assert!(status.current_call_id.lock().unwrap().is_none());

    apply!(E::CallIncoming);
    let call_id = status.current_call_id.lock().unwrap().clone();
    assert_eq!(
        call_id.as_deref(),
        tracker.call_id(),
        "shared id mirrors the session"
    );
    assert!(call_id.as_deref().unwrap().starts_with("call_"));
    assert!(status.call_started_at.lock().unwrap().is_some());
    assert!(
        !status.call_active.load(Ordering::Relaxed),
        "ringing, not active"
    );
    let first_gen = tracker.generation();
    assert_eq!(first_gen, 1);

    // Phones re-emit the ring indicator — the id must not change mid-call.
    apply!(E::CallIncoming);
    assert_eq!(*status.current_call_id.lock().unwrap(), call_id);
    assert_eq!(tracker.generation(), first_gen);

    apply!(E::CallerId("+61400000001".to_string()));
    assert_eq!(
        status.current_caller.lock().unwrap().as_deref(),
        Some("+61400000001")
    );

    apply!(E::CallAnswered);
    assert!(status.call_active.load(Ordering::Relaxed));
    assert_eq!(*status.current_call_id.lock().unwrap(), call_id);

    sink.lines.clear();
    apply!(E::CallTerminated);
    // Production emits the transcript barrier at the loop tail, after
    // any generation-change boundary turn has been flushed.
    emit_ready_transcript_settlements(None, &mut sink);
    assert!(status.current_call_id.lock().unwrap().is_none());
    assert!(status.call_started_at.lock().unwrap().is_none());
    assert!(status.current_caller.lock().unwrap().is_none());
    assert!(!status.call_active.load(Ordering::Relaxed));
    assert_eq!(tracker.generation(), 0, "idle after termination");
    // The ended event carried the SAME call id the whole call used, and
    // an ANSWERED call is completed even when it lasted under a second.
    let v: serde_json::Value = serde_json::from_str(&sink.lines[0]).unwrap();
    assert_eq!(
        v["params"]["event"]["name"],
        json!(crate::contract::events::CALL_ENDED)
    );
    assert_eq!(v["params"]["event"]["data"]["callId"], json!(call_id));
    assert_eq!(v["params"]["event"]["data"]["outcome"], json!("completed"));
    assert_eq!(v["params"]["event"]["data"]["from"], json!("+61400000001"));
    let settled: serde_json::Value = serde_json::from_str(&sink.lines[1]).unwrap();
    assert_eq!(
        settled["params"]["event"]["name"],
        json!(crate::contract::events::CALL_TRANSCRIPT_SETTLED)
    );
    assert_eq!(settled["params"]["event"]["data"]["callId"], json!(call_id));
    assert_eq!(
        settled["params"]["event"]["data"]["transcriptCorrectionTimedOut"],
        json!(false)
    );

    // The next call gets a FRESH id and a FRESH generation.
    apply!(E::CallIncoming);
    let second = status.current_call_id.lock().unwrap().clone();
    assert!(second.is_some());
    assert_ne!(second, call_id);
    assert_eq!(tracker.generation(), 2);
}

#[cfg(feature = "voice")]
#[test]
fn second_caller_hold_line_names_the_queue_position() {
    // Nobody waiting ahead = the very next caller.
    assert!(super::second_caller_hold_line(0).ends_with("You're next in the queue."));
    // One ahead (a parked caller) = number 2, and so on.
    assert!(super::second_caller_hold_line(1).ends_with("You're number 2 in the queue."));
    assert!(super::second_caller_hold_line(2).ends_with("You're number 3 in the queue."));
    // Every line starts with the ask and is ASCII (TTS-safe).
    let line = super::second_caller_hold_line(0);
    assert!(line.starts_with("Thank you for calling!"));
    assert!(
        line.is_ascii(),
        "hold line must be ASCII for the synthesizer"
    );
}

/// Phase 4 observe lane: a waiting knock emits ONE aokie.call.waiting
/// per episode (correlated to the ACTIVE call), never disturbs the
/// session, and a fresh episode later in the same call re-announces.
#[test]
fn handle_event_call_waiting_episode_emits_once_and_preserves_the_call() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
    let call_id = tracker.call_id().unwrap().to_string();
    sink.lines.clear();

    // Anonymous knock (callsetup-first ordering), then the +CCWA that
    // names the caller: still exactly ONE durable event.
    handle_event(
        E::CallWaiting { number: None },
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    handle_event(
        E::CallWaiting {
            number: Some("0491570157".to_string()),
        },
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    let events: Vec<serde_json::Value> = sink
        .lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let waiting: Vec<_> = events
        .iter()
        .filter(|v| {
            v["params"]["event"]["name"] == json!(crate::contract::events::CALL_WAITING)
        })
        .collect();
    assert_eq!(
        waiting.len(),
        1,
        "one durable event per episode: {events:?}"
    );
    assert_eq!(
        waiting[0]["params"]["event"]["data"]["callId"],
        json!(call_id)
    );
    assert_eq!(waiting[0]["params"]["event"]["data"]["from"], json!(""));
    // The active session is untouched by the whole episode.
    assert_eq!(tracker.call_id(), Some(call_id.as_str()));
    assert!(status.call_active.load(Ordering::Relaxed));
    assert_eq!(status.call_waiting_episodes.load(Ordering::Relaxed), 1);

    // Episode ends; a SECOND knock later in the same call re-announces —
    // number known from the start this time, so it rides the event.
    handle_event(E::CallWaitingEnded, &mut tracker, None, &mut sink, &status);
    // The ended episode is STASHED for give-up classification: if no
    // session claims the knock (accept mints under its id; promotion
    // under a fresh id with the same number), the run_loop records an
    // honest MISSED call so follow-ups ring the caller back.
    {
        let stash = status.gave_up_knock.lock().unwrap().clone();
        assert_eq!(stash.len(), 1, "ended episode queued for give-up check");
        let (leg, _at, gen_at_end) = &stash[0];
        assert!(leg.call_id.starts_with("call_"));
        assert_eq!(*gen_at_end, tracker.generation());
        status.gave_up_knock.lock().unwrap().clear();
    }
    sink.lines.clear();
    handle_event(
        E::CallWaiting {
            number: Some("0491570157".to_string()),
        },
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    let v: serde_json::Value = serde_json::from_str(&sink.lines[0]).unwrap();
    assert_eq!(
        v["params"]["event"]["name"],
        json!(crate::contract::events::CALL_WAITING)
    );
    assert_eq!(v["params"]["event"]["data"]["from"], json!("0491570157"));
    assert_eq!(status.call_waiting_episodes.load(Ordering::Relaxed), 2);

    // callheld transitions are diagnostics-only.
    handle_event(
        E::CallHeld { state: 1 },
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    assert_eq!(status.call_held_state.load(Ordering::Relaxed), 1);
    assert!(tracker.current().is_some(), "session survives callheld");

    // A knock with NO tracked call is ignored (no panic, no event).
    let mut idle = crate::call_session::SessionTracker::new();
    sink.lines.clear();
    handle_event(
        E::CallWaiting {
            number: Some("0491570157".to_string()),
        },
        &mut idle,
        None,
        &mut sink,
        &status,
    );
    assert!(sink.lines.is_empty(), "no event without a tracked call");

    // CLCC entries fold into ONE snapshot burst (diagnostics-visible),
    // and the waiting leg renders with its status name + number.
    handle_event(
        E::CallListEntry {
            index: 1,
            direction: 1,
            status: 0,
            multiparty: false,
            number: Some("0491570156".to_string()),
        },
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    handle_event(
        E::CallListEntry {
            index: 2,
            direction: 1,
            status: 5,
            multiparty: false,
            number: Some("0491570157".to_string()),
        },
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    let snapshot = status.clcc_snapshot.lock().unwrap().clone();
    let (_, lines) = snapshot.expect("snapshot recorded");
    assert_eq!(
        lines,
        vec![
            "idx=1 dir=1 status=active number=0491570156".to_string(),
            "idx=2 dir=1 status=waiting number=0491570157".to_string(),
        ]
    );
    // The STRUCTURED twin folds the same burst — the verified-swap
    // machinery judges numbers/status off exactly these.
    let calls = status.clcc_calls.lock().unwrap().clone();
    let (_, legs) = calls.expect("structured snapshot recorded");
    assert_eq!(
        legs,
        vec![
            ClccLeg {
                index: 1,
                status: 0,
                number: Some("0491570156".to_string())
            },
            ClccLeg {
                index: 2,
                status: 5,
                number: Some("0491570157".to_string())
            },
        ]
    );
}

/// Phase 4 verified swaps: the pure accept judge — every observed shape
/// maps to exactly one convergence path.
#[cfg(feature = "voice")]
#[test]
fn judge_accept_covers_every_topology() {
    use super::{judge_accept, AcceptVerdict, SwapSnapshot};
    let snap = |callheld: u64, waiting: Option<&str>, alive: bool| SwapSnapshot {
        callheld,
        waiting_id: waiting.map(str::to_string),
        current_alive: alive,
        clcc: None,
    };
    // Knock resolved + held/active pair = the newcomer has the line.
    assert_eq!(
        judge_accept(&snap(1, None, true), "w1"),
        AcceptVerdict::Accepted
    );
    // The primary died mid-accept beats everything else.
    assert_eq!(
        judge_accept(&snap(1, None, false), "w1"),
        AcceptVerdict::PrimaryGone
    );
    // Held with NOBODY active — retrieve territory (even if the knock
    // is somehow still showing).
    assert_eq!(
        judge_accept(&snap(2, Some("w1"), true), "w1"),
        AcceptVerdict::HeldAlone
    );
    assert_eq!(
        judge_accept(&snap(2, None, true), "w1"),
        AcceptVerdict::HeldAlone
    );
    // Knock still up, nothing held: the phone ignored the CHLD.
    assert_eq!(
        judge_accept(&snap(0, Some("w1"), true), "w1"),
        AcceptVerdict::NothingChanged
    );
    // A DIFFERENT knock id is a new episode, not this accept resolving.
    assert_eq!(
        judge_accept(&snap(0, Some("w2"), true), "w1"),
        AcceptVerdict::KnockGone
    );
    // Knock vanished without a pair forming: the waiting caller gave up.
    assert_eq!(
        judge_accept(&snap(0, None, true), "w1"),
        AcceptVerdict::KnockGone
    );
}

/// Phase 4 verified swaps: the pure swap-back judge. CLCC numbers name
/// WHO is active (the indicator cannot); the indicator is the fallback.
#[cfg(feature = "voice")]
#[test]
fn judge_swap_back_names_the_active_leg() {
    use super::{judge_swap_back, ClccLeg, SwapBackVerdict, SwapSnapshot};
    let leg = |status: u8, number: Option<&str>| ClccLeg {
        index: 1,
        status,
        number: number.map(str::to_string),
    };
    let snap = |callheld: u64, alive: bool, clcc: Option<Vec<ClccLeg>>| SwapSnapshot {
        callheld,
        waiting_id: None,
        current_alive: alive,
        clcc,
    };
    let a = Some("0491570156");
    let b = Some("0491570157");
    // CLCC: primary active + newcomer held = the swap took.
    assert_eq!(
        judge_swap_back(&snap(1, true, Some(vec![leg(0, a), leg(1, b)])), a, b),
        SwapBackVerdict::Swapped
    );
    // CLCC: newcomer still active = the toggle was ignored.
    assert_eq!(
        judge_swap_back(&snap(1, true, Some(vec![leg(0, b), leg(1, a)])), a, b),
        SwapBackVerdict::StayedOnNewcomer
    );
    // CLCC: primary active ALONE = swap took, newcomer's leg vanished.
    assert_eq!(
        judge_swap_back(&snap(0, true, Some(vec![leg(0, a)])), a, b),
        SwapBackVerdict::SwappedNewcomerGone
    );
    // CLCC: newcomer active ALONE = the primary's held leg vanished.
    assert_eq!(
        judge_swap_back(&snap(0, true, Some(vec![leg(0, b)])), a, b),
        SwapBackVerdict::NewcomerAlone
    );
    // CLCC: nobody active but someone held = retrieve — ONLY when the
    // callheld indicator agrees (2 = held-only).
    assert_eq!(
        judge_swap_back(&snap(2, true, Some(vec![leg(1, a)])), a, b),
        SwapBackVerdict::ActiveDied
    );
    // The same list with callheld=1 is a MID-TRANSITION capture (the
    // live round-2 incident: judging it killed the wrong session).
    assert_eq!(
        judge_swap_back(&snap(1, true, Some(vec![leg(1, a)])), a, b),
        SwapBackVerdict::Inconclusive
    );
    // CLCC: empty list = the z49 signature, everything gone — only when
    // the indicator agrees nothing is held.
    assert_eq!(
        judge_swap_back(&snap(0, false, Some(vec![])), a, b),
        SwapBackVerdict::AllGone
    );
    assert_eq!(
        judge_swap_back(&snap(1, true, Some(vec![])), a, b),
        SwapBackVerdict::Inconclusive
    );
    // A lone active leg while the indicator still reports a held call is
    // transitional too — even with a matching number.
    assert_eq!(
        judge_swap_back(&snap(1, true, Some(vec![leg(0, a)])), a, b),
        SwapBackVerdict::Inconclusive
    );
    // A KNOWN number matching NEITHER party = the CHLD collided with a
    // fresh knock and answered a STRANGER (live round 6: assuming
    // "Swapped" here bound the primary's session to the stranger's leg).
    let stranger = Some("0491570158");
    assert_eq!(
        judge_swap_back(
            &snap(1, true, Some(vec![leg(0, stranger), leg(1, b)])),
            a,
            b
        ),
        SwapBackVerdict::StrangerActive
    );
    assert_eq!(
        judge_swap_back(&snap(0, true, Some(vec![leg(0, stranger)])), a, b),
        SwapBackVerdict::StrangerActive
    );
    // With one of OUR numbers unknown the stranger read is unprovable —
    // never fires (falls through to the safer reads).
    assert_ne!(
        judge_swap_back(
            &snap(1, true, Some(vec![leg(0, stranger), leg(1, b)])),
            None,
            b
        ),
        SwapBackVerdict::StrangerActive
    );
    // Withheld numbers + one lone active leg: keep the tracker's
    // session (no further CHLD needed) — NewcomerAlone.
    assert_eq!(
        judge_swap_back(&snap(0, true, Some(vec![leg(0, None)])), a, None),
        SwapBackVerdict::NewcomerAlone
    );
    // Dark numbers with a full pair fall through to the indicator.
    assert_eq!(
        judge_swap_back(
            &snap(1, true, Some(vec![leg(0, None), leg(1, None)])),
            None,
            None
        ),
        SwapBackVerdict::Swapped
    );
    // Indicator-only fallbacks (no fresh CLCC at all).
    assert_eq!(
        judge_swap_back(&snap(1, true, None), a, b),
        SwapBackVerdict::Swapped
    );
    assert_eq!(
        judge_swap_back(&snap(2, true, None), a, b),
        SwapBackVerdict::ActiveDied
    );
    assert_eq!(
        judge_swap_back(&snap(0, true, None), a, b),
        SwapBackVerdict::NewcomerAlone
    );
    assert_eq!(
        judge_swap_back(&snap(0, false, None), a, b),
        SwapBackVerdict::AllGone
    );
}

/// Audit AK-001/AK-01: an operator-rejected ring ends "rejected", a
/// remote-abandoned ring ends "missed" — the two are no longer conflated.
#[test]
fn handle_event_rejected_is_not_missed() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    // Operator rejects the ringing call (RadioControl::Reject notes the
    // intent, then the phone reports termination).
    // CallTerminated force-flushes the pending incoming first (AOK-LIF-001),
    // so locate the terminal event by NAME, not position.
    let ended_event = |sink: &VecSink| -> serde_json::Value {
        sink.lines
            .iter()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|v| {
                v["params"]["event"]["name"] == json!(crate::contract::events::CALL_ENDED)
            })
            .expect("a call.ended event")
    };
    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
    sink.lines.clear();
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    let v = ended_event(&sink);
    assert_eq!(v["params"]["event"]["data"]["outcome"], json!("rejected"));
    assert_eq!(
        v["params"]["event"]["data"]["reason"],
        json!("operator_reject")
    );

    // Remote abandons the next ring: genuinely missed.
    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    sink.lines.clear();
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    let v = ended_event(&sink);
    assert_eq!(v["params"]["event"]["data"]["outcome"], json!("missed"));
}

/// Audit AOK-LIF-001: `incoming` always precedes the rest of its call's
/// lifecycle. The caller-ID enrichment hold must not let an instant
/// answer (or termination) overtake the canonical start-of-call event.
#[test]
fn handle_event_incoming_always_precedes_answered() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    // The phone answers within the enrichment hold — no run_loop flush
    // tick has happened between the two events.
    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);

    let names: Vec<String> = sink
        .lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["params"]["event"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let incoming_at = names
        .iter()
        .position(|n| n == crate::contract::events::CALL_INCOMING);
    let answered_at = names
        .iter()
        .position(|n| n == crate::contract::events::CALL_ANSWERED);
    assert!(
        incoming_at.is_some(),
        "incoming must be emitted (forced flush)"
    );
    assert!(
        incoming_at < answered_at,
        "incoming must precede answered, got order {names:?}"
    );

    // An instantly-abandoned ring still gets incoming before ended.
    sink.lines.clear();
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    sink.lines.clear();
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    let names: Vec<String> = sink
        .lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["params"]["event"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let incoming_at = names
        .iter()
        .position(|n| n == crate::contract::events::CALL_INCOMING);
    let ended_at = names
        .iter()
        .position(|n| n == crate::contract::events::CALL_ENDED);
    assert!(
        incoming_at.is_some() && incoming_at < ended_at,
        "incoming precedes ended, got {names:?}"
    );
}

/// `aokie.call.caller_id` announces the number the FIRST time this call
/// learns it (fed by +CLIP or the AT+CLCC rescue) — exactly once per call
/// (+CLIP repeats per ring; the idempotency key is corr-scoped), never for
/// an empty number, and always AFTER the call's `incoming`.
#[test]
fn caller_id_event_fires_once_per_call_with_the_number() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
    sink.lines.clear();

    // The number lands (CLCC rescue) → announced once, with the number.
    handle_event(
        E::CallerId("0491570156".to_string()),
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    let events: Vec<serde_json::Value> = sink
        .lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let caller_id: Vec<&serde_json::Value> = events
        .iter()
        .filter(|v| {
            v["params"]["event"]["name"] == json!(crate::contract::events::CALL_CALLER_ID)
        })
        .collect();
    assert_eq!(caller_id.len(), 1, "announced exactly once: {events:?}");
    assert_eq!(
        caller_id[0]["params"]["event"]["data"]["from"],
        json!("0491570156")
    );
    assert_eq!(
        caller_id[0]["params"]["event"]["data"]["callId"].as_str(),
        tracker.call_id()
    );

    // A repeated +CLIP for the same call must NOT re-announce (the
    // corr-scoped idempotency key may only be minted once).
    sink.lines.clear();
    handle_event(
        E::CallerId("0491570156".to_string()),
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    assert!(sink.lines.is_empty(), "no re-announce: {:?}", sink.lines);

    // An empty caller id (withheld) never announces.
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    sink.lines.clear();
    handle_event(
        E::CallerId(String::new()),
        &mut tracker,
        None,
        &mut sink,
        &status,
    );
    assert!(
        !sink.lines.iter().any(|l| l.contains("caller_id")),
        "withheld id stays silent: {:?}",
        sink.lines
    );
}

/// AOK-CTRL-001: an ANSWER landing on an idle tracker while the phone is
/// still connected rebuilds a session (the live-call deafness bug: a
/// misread terminate killed the session, the late answer was a no-op and
/// every frame of a real call was dropped). After a device loss the same
/// stale answer must NOT build a phantom session (AOK-LIF-003).
#[test]
fn orphaned_answer_recovers_a_session_only_while_connected() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    // Connected phone, no session (a terminate was misread earlier).
    status.connected.store(true, Ordering::Relaxed);
    handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
    assert!(tracker.current().is_some(), "session rebuilt");
    assert!(tracker.current().unwrap().is_active(), "and answered");
    assert!(status.call_active.load(Ordering::Relaxed));
    assert_eq!(
        status.current_call_id.lock().unwrap().as_deref(),
        tracker.call_id()
    );
    let names: Vec<String> = sink
        .lines
        .iter()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["params"]["event"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let incoming_at = names
        .iter()
        .position(|n| n == crate::contract::events::CALL_INCOMING);
    let answered_at = names
        .iter()
        .position(|n| n == crate::contract::events::CALL_ANSWERED);
    assert!(
        incoming_at.is_some() && incoming_at < answered_at,
        "recovered session still emits incoming before answered: {names:?}"
    );
    // Clean up: terminate the recovered call normally.
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);

    // Disconnected: the stale answer is dropped — no phantom session.
    status.connected.store(false, Ordering::Relaxed);
    sink.lines.clear();
    handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
    assert!(
        tracker.current().is_none(),
        "no phantom session on a dead link"
    );
    assert!(sink.lines.is_empty(), "and no events");
    assert!(
        !status.call_active.load(Ordering::Relaxed),
        "an unrecovered orphan answer never reads as an active call"
    );
}

/// Audit AOK-LIF-003: losing the phone/radio link under a live call
/// synthesizes exactly ONE terminal `call.ended` (reason device_lost),
/// clears the shared call identity, and a late real CallTerminated is a
/// no-op — the UI can never stay "live" on hardware that is gone.
#[test]
fn handle_event_device_loss_terminates_the_active_call_once() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();

    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
    assert!(status.call_active.load(Ordering::Relaxed));
    sink.lines.clear();

    handle_event(
        E::DeviceDisconnected("AA:BB:CC:DD:EE:FF".into()),
        &mut tracker,
        None,
        &mut sink,
        &status,
    );

    let events: Vec<serde_json::Value> = sink
        .lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let ended: Vec<&serde_json::Value> = events
        .iter()
        .filter(|v| v["params"]["event"]["name"] == json!(crate::contract::events::CALL_ENDED))
        .collect();
    assert_eq!(ended.len(), 1, "exactly one terminal event");
    assert_eq!(
        ended[0]["params"]["event"]["data"]["reason"],
        json!("device_lost")
    );
    assert_eq!(
        ended[0]["params"]["event"]["data"]["outcome"],
        json!("completed")
    );
    assert!(!status.call_active.load(Ordering::Relaxed));
    assert!(
        status.current_call_id.lock().unwrap().is_none(),
        "call identity cleared"
    );
    assert!(tracker.current().is_none(), "session consumed");

    // The phone reports the (now stale) termination later: no duplicate.
    sink.lines.clear();
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    assert!(
        sink.lines
            .iter()
            .all(|l| !l.contains(crate::contract::events::CALL_ENDED)),
        "late real termination after synthesized one is a no-op"
    );
}

#[test]
fn http_speech_fallback_is_sticky_per_call() {
    let mut state = HttpSpeechFallback::new(Some(
        "  http://127.0.0.1:17920/v1/audio/speech  ".to_string(),
    ));
    assert_eq!(
        state.endpoint_for_call(),
        Some("http://127.0.0.1:17920/v1/audio/speech")
    );

    assert!(state.mark_failed_for_call());
    assert_eq!(state.endpoint_for_call(), None);
    assert!(!state.mark_failed_for_call());

    state.reset_call();
    assert_eq!(
        state.endpoint_for_call(),
        Some("http://127.0.0.1:17920/v1/audio/speech")
    );

    assert!(state.mark_failed_for_call());
    state.configure(Some("http://127.0.0.1:17920/v1/audio/speech2".to_string()));
    assert_eq!(
        state.endpoint_for_call(),
        Some("http://127.0.0.1:17920/v1/audio/speech2")
    );

    state.configure(Some("   ".to_string()));
    assert_eq!(state.endpoint_for_call(), None);
}
#[cfg(all(target_os = "windows", feature = "voice"))]
#[test]
fn overlap_capture_initializes_without_a_greeting_and_survives_phrases() {
    let mut aec = None;
    assert!(!ensure_overlap_capture(&mut aec, true, 0));
    assert!(!ensure_overlap_capture(&mut aec, false, 8000));
    assert!(aec.is_none());
    // The audio channel becomes ready without ever invoking the greeting.
    assert!(ensure_overlap_capture(&mut aec, true, 8000));
    assert!(aec.is_some());
    assert!(!ensure_overlap_capture(&mut aec, true, 8000));
    // Teardown/takeover clears the session; the next channel reinitializes.
    aec = None;
    assert!(ensure_overlap_capture(&mut aec, true, 16000));
}

#[test]
fn observed_outgoing_dial_publishes_direction_for_current_call() {
    use crate::event_bridge::VecSink;
    use aokie_dongle::bluetooth::BluetoothEvent as E;
    let status = Arc::new(RadioStatus::default());
    let mut sink = VecSink::default();
    let mut tracker = crate::call_session::SessionTracker::new();
    handle_event(E::OutgoingDialing, &mut tracker, None, &mut sink, &status);
    let outbound = tracker.current().unwrap();
    assert!(outbound.outbound);
    assert!(
        !outbound.agent_owned,
        "observing the dial cannot give the AI ownership"
    );
    assert_eq!(
        status.outbound_call_id.lock().unwrap().as_deref(),
        Some(outbound.id.as_str())
    );
    handle_event(E::CallAnswered, &mut tracker, None, &mut sink, &status);
    assert_eq!(
        status.outbound_call_id.lock().unwrap().as_deref(),
        tracker.call_id()
    );
    handle_event(E::CallTerminated, &mut tracker, None, &mut sink, &status);
    handle_event(E::CallIncoming, &mut tracker, None, &mut sink, &status);
    assert_ne!(
        status.outbound_call_id.lock().unwrap().as_deref(),
        tracker.call_id()
    );
}
