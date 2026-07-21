//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `service_realtime_lane`.

#[allow(unused_imports)]
use super::*;

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn service_realtime_lane(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
    greeting: &Option<String>,
    host_rpc: &Arc<crate::host_rpc::HostRpc>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    realtime_selected: bool,
    realtime_config: &Option<RealtimeRuntimeConfig>,
    synth: &crate::synth::SynthHandle,
    stt_tx: &std::sync::mpsc::Sender<SttWork>,
    agent_persona: &String,
    answer_hold_started: &mut Option<std::time::Instant>,
    screen_policy: &crate::screen::ScreenPolicy,
    agent_hangup: bool,
    promote_greet_for: &mut Option<String>,
    aec: &mut Option<crate::aec::EchoCanceller>,
    tracker: &mut crate::call_session::SessionTracker,
    voice_call_gen: u64,
    realtime_lane: &mut Option<RealtimeCallLane>,
    realtime_legacy_call: &mut Option<String>,
    realtime_failed_call: &mut Option<(String, String)>,
    realtime_resume_call: &mut Option<String>,
    realtime_answered_at: &mut Option<(String, std::time::Instant)>,
    realtime_midcall_failure: &mut Option<(String, crate::remote_media::AokieOwnerFence, String)>,
    realtime_terminal_call: &mut Option<(String, std::time::Instant, u8)>,
    realtime_deferred_policy_failure: &mut Option<(String, String)>,
    ctx: &mut CallVoiceContext,
) {
    #[cfg(feature = "voice")]
    if realtime_selected {
        // Companion takeover cancels the old upstream generation and
        // flushes every queued byte. Once the exact same physical caller
        // is returned to Aokie, establish a fresh provider session; never
        // silently demote to a stopped local LLM/STT/TTS stack.
        if realtime_lane.is_none() {
            if let Some(resume_id) = realtime_resume_call.clone() {
                let resumable = tracker.current().is_some_and(|call| {
                    call.is_active() && call.id == resume_id && (!call.outbound || call.agent_owned)
                }) && !remote_media.radio_reserved()
                    && bt.get_sample_rate() > 0;
                if resumable {
                    if let (Some(config), Some(owner)) = (
                        realtime_config.as_ref(),
                        aokie_owner_for_call(&remote_media, &resume_id),
                    ) {
                        let persona = ctx
                            .call_agent_overlay
                            .as_ref()
                            .filter(|overlay| overlay.call_id == resume_id)
                            .and_then(|overlay| overlay.persona.as_deref())
                            .unwrap_or(&agent_persona);
                        match crate::realtime_voice::RealtimeVoiceSession::spawn(
                            crate::realtime_voice::SessionConfig {
                                endpoint: config.endpoint.clone(),
                                call_id: resume_id.clone(),
                                generation: voice_call_gen,
                                expected_destination: config.destination.clone(),
                                instructions: realtime_safe_instructions(persona, agent_hangup),
                                greeting: "Thanks for waiting. How can I continue helping?"
                                    .to_string(),
                                voice: Some(config.voice.clone()),
                                model: None,
                                turn_detection: config.turn_detection,
                                max_output_tokens: config.max_output_tokens,
                                allow_business_lookup: true,
                                allow_request_appointment: true,
                                allow_finish_call: agent_hangup,
                            },
                        ) {
                            Ok(session) => {
                                *realtime_lane = Some(RealtimeCallLane::new(session));
                                *realtime_resume_call = None;
                                *status.realtime_error.lock().unwrap() = None;
                            }
                            Err(error) => {
                                *realtime_resume_call = None;
                                *realtime_midcall_failure =
                                    Some((resume_id, owner, error.clone()));
                                *status.realtime_error.lock().unwrap() = Some(error);
                            }
                        }
                    }
                }
            }
        }
        // Decide the call's responder only after the same bounded caller-id
        // and personalization window used by auto-answer. Manager,
        // screened, switched, already-active and handset-observed calls
        // stay on the proven legacy path; Realtime serves fresh normal
        // inbound callers AND agent-placed outbound dials (`call.dial` —
        // the opening line rides the overlay greeting into the Realtime
        // session exactly like a personalized inbound greeting).
        if realtime_lane.is_none()
            && realtime_legacy_call.is_none()
            && realtime_failed_call.is_none()
            && realtime_resume_call.is_none()
            && realtime_terminal_call.is_none()
        {
            if let Some(call) = tracker.current() {
                let call_id = call.id.clone();
                if (call.outbound && !call.agent_owned)
                    || call.is_active()
                    || promote_greet_for.is_some()
                {
                    *realtime_legacy_call = Some(call_id);
                    ctx.desktop_realtime_responder = false;
                    if should_prepare_local_speech(realtime_selected, true) {
                        let _ = stt_tx.send(SttWork::Warm);
                        synth.warm();
                    }
                } else {
                    let overlay_ready = ctx
                        .call_agent_overlay
                        .as_ref()
                        .is_some_and(|overlay| overlay.call_id == call.id);
                    let started = *answer_hold_started.get_or_insert_with(Instant::now);
                    let wait_for_identity = hold_auto_answer(
                        call.caller_id.as_deref().is_some_and(|id| !id.is_empty()),
                        overlay_ready,
                        started.elapsed(),
                    );
                    if !wait_for_identity {
                        let legacy = screen_policy.is_manager(call.caller_id.as_deref())
                            || screen_policy.verdict(call.caller_id.as_deref()).is_some();
                        ctx.desktop_realtime_responder = !legacy;
                        if legacy {
                            *realtime_legacy_call = Some(call_id);
                            ctx.desktop_realtime_responder = false;
                            if should_prepare_local_speech(realtime_selected, true) {
                                let _ = stt_tx.send(SttWork::Warm);
                                synth.warm();
                            }
                        } else if let Some(config) = realtime_config.as_ref() {
                            ctx.desktop_realtime_responder = true;
                            let persona = ctx
                                .call_agent_overlay
                                .as_ref()
                                .filter(|overlay| overlay.call_id == call.id)
                                .and_then(|overlay| overlay.persona.as_deref())
                                .unwrap_or(&agent_persona);
                            let greeting_text = ctx
                                .call_agent_overlay
                                .as_ref()
                                .filter(|overlay| overlay.call_id == call.id)
                                .and_then(|overlay| overlay.greeting.as_deref())
                                .or(greeting.as_deref())
                                .filter(|text| !text.trim().is_empty())
                                .unwrap_or(DEFAULT_GREETING);
                            let session = crate::realtime_voice::RealtimeVoiceSession::spawn(
                                crate::realtime_voice::SessionConfig {
                                    endpoint: config.endpoint.clone(),
                                    call_id: call.id.clone(),
                                    generation: voice_call_gen,
                                    expected_destination: config.destination.clone(),
                                    instructions: realtime_safe_instructions(
                                        persona,
                                        agent_hangup,
                                    ),
                                    greeting: greeting_text.to_string(),
                                    voice: Some(config.voice.clone()),
                                    // Provider profiles own the concrete
                                    // Realtime model. `aiModel` may be a
                                    // text/Codex model and is never reused.
                                    model: None,
                                    turn_detection: config.turn_detection,
                                    max_output_tokens: config.max_output_tokens,
                                    allow_business_lookup: true,
                                    allow_request_appointment: true,
                                    allow_finish_call: agent_hangup,
                                },
                            );
                            match session {
                                Ok(session) => {
                                    eprintln!(
                                        "[aokie-plugin] preparing Desktop realtime voice for {} call {}",
                                        if call.outbound { "outbound" } else { "ringing" },
                                        call.id
                                    );
                                    *realtime_lane = Some(RealtimeCallLane::new(session));
                                    *status.realtime_error.lock().unwrap() = None;
                                }
                                Err(error) if call.outbound => {
                                    // An agent-placed dial must never ring
                                    // the callee into a responder-less
                                    // line: fall back to the proven legacy
                                    // lane (engines load lazily).
                                    eprintln!(
                                        "[aokie-plugin] Desktop realtime preconnect failed for outbound dial — using the legacy responder: {error}"
                                    );
                                    *status.realtime_error.lock().unwrap() =
                                        Some(error.clone());
                                    *realtime_legacy_call = Some(call.id.clone());
                                    ctx.desktop_realtime_responder = false;
                                    if should_prepare_local_speech(realtime_selected, true) {
                                        let _ = stt_tx.send(SttWork::Warm);
                                        synth.warm();
                                    }
                                }
                                Err(error) => {
                                    eprintln!(
                                        "[aokie-plugin] Desktop realtime preconnect failed — call rings through: {error}"
                                    );
                                    *status.realtime_error.lock().unwrap() =
                                        Some(error.clone());
                                    *realtime_failed_call = Some((call.id.clone(), error));
                                }
                            }
                        } else if call.outbound {
                            // No realtime configuration: agent dials keep
                            // their proven legacy responder.
                            *realtime_legacy_call = Some(call.id.clone());
                            ctx.desktop_realtime_responder = false;
                            if should_prepare_local_speech(realtime_selected, true) {
                                let _ = stt_tx.send(SttWork::Warm);
                                synth.warm();
                            }
                        } else {
                            let error = status
                                .realtime_error
                                .lock()
                                .unwrap()
                                .clone()
                                .unwrap_or_else(|| {
                                    "Desktop realtime configuration is unavailable".into()
                                });
                            *realtime_failed_call = Some((call.id.clone(), error));
                        }
                    }
                }
            }
        }

        if let Some(call) = tracker.current().filter(|call| call.is_active()) {
            if ctx.desktop_realtime_responder
                && realtime_answered_at
                    .as_ref()
                    .is_none_or(|(call_id, _)| call_id != &call.id)
            {
                *realtime_answered_at = Some((call.id.clone(), Instant::now()));
            }
            let identity_settled = realtime_identity_settled(
                call.caller_id
                    .as_deref()
                    .is_some_and(|caller_id| !caller_id.is_empty()),
                realtime_answered_at
                    .as_ref()
                    .filter(|(call_id, _)| call_id == &call.id)
                    .map(|(_, answered_at)| answered_at.elapsed())
                    .unwrap_or_default(),
            );
            let became_legacy = realtime_lane
                .as_ref()
                .is_some_and(|lane| lane.call_id == call.id)
                && identity_settled
                && (screen_policy.is_manager(call.caller_id.as_deref())
                    || screen_policy.verdict(call.caller_id.as_deref()).is_some());
            if became_legacy {
                let call_id = call.id.clone();
                let late_manager = screen_policy.is_manager(call.caller_id.as_deref());
                let manager_error = late_manager
                    .then(|| {
                        legacy_manager_readiness_error(
                            status.tts_error.lock().unwrap().clone(),
                            status.stt_error.lock().unwrap().clone(),
                            status.llm_error.lock().unwrap().clone(),
                        )
                    })
                    .flatten();
                if let Some(lane) = realtime_lane.take() {
                    if let Some(item_id) = lane.output_pacer.active_item() {
                        let _ = lane.session.cancel_output(
                            item_id,
                            lane.output_pacer.audible_played_ms(Instant::now()),
                        );
                    }
                    lane.session.stop("caller policy requires the legacy lane");
                }
                bt.flush_tx_audio();
                if let Some(policy_aec) = aec.as_mut() {
                    policy_aec.reset();
                }
                ctx.history.clear();
                ctx.pending_turn = None;
                *realtime_legacy_call = Some(call_id.clone());
                ctx.desktop_realtime_responder = false;
                let _ = stt_tx.send(SttWork::Warm);
                synth.warm();
                status.realtime_ready.store(false, Ordering::Relaxed);
                if let Some(current) = tracker.current_mut().filter(|s| s.id == call_id) {
                    current.greeted = manager_error.is_some();
                }
                if let Some(reason) = manager_error {
                    let failure = format!(
                        "late manager classification cannot use the legacy responder: {reason}"
                    );
                    if let Some(owner) = aokie_owner_for_call(&remote_media, &call_id) {
                        *realtime_midcall_failure = Some((call_id.clone(), owner, failure));
                    } else {
                        *realtime_deferred_policy_failure = Some((call_id.clone(), failure));
                    }
                }
                eprintln!(
                    "[aokie-plugin] late caller policy classification moved {call_id} to the legacy screen/manager lane"
                );
            }
        }

        // A preconnect failure normally leaves the handset ringing. If
        // the exact same physical call nevertheless becomes active
        // (manual answer or a late answer acknowledgement), it is no
        // longer a before-answer failure: apply the same owner-fenced
        // fail-safe/Companion-resume policy as an established session.
        if let Some((failed_call_id, reason)) = realtime_failed_call.clone() {
            let exact_call_active = tracker
                .current()
                .is_some_and(|call| call.is_active() && call.id == failed_call_id);
            if exact_call_active {
                if realtime_answered_at
                    .as_ref()
                    .is_none_or(|(call_id, _)| call_id != &failed_call_id)
                {
                    *realtime_answered_at = Some((failed_call_id.clone(), Instant::now()));
                }
                *realtime_failed_call = None;
                let owner = aokie_owner_for_call(&remote_media, &failed_call_id);
                match realtime_failure_disposition(
                    true,
                    owner.is_some(),
                    remote_media.radio_reserved(),
                ) {
                    RealtimeFailureDisposition::FailSafe => {
                        *realtime_midcall_failure = Some((
                            failed_call_id,
                            owner.expect("fail-safe requires an exact Aokie owner"),
                            reason,
                        ));
                    }
                    RealtimeFailureDisposition::ResumeAfterHuman => {
                        *realtime_resume_call = Some(failed_call_id);
                    }
                    RealtimeFailureDisposition::RingThrough => unreachable!(),
                }
            }
        }

        if let Some((call_id, reason)) = realtime_deferred_policy_failure.clone() {
            let exact_call_active = tracker
                .current()
                .is_some_and(|call| call.is_active() && call.id == call_id);
            if !exact_call_active {
                *realtime_deferred_policy_failure = None;
            } else if let Some(owner) = aokie_owner_for_call(&remote_media, &call_id) {
                *realtime_deferred_policy_failure = None;
                *realtime_midcall_failure = Some((call_id, owner, reason));
            }
        }

        // Switchboard park invariant: the exact Realtime session belongs
        // to ONE call. The moment that call stops being the active
        // foreground call (operator switchboard accept, hold cascade),
        // dispose the session so the OTHER caller's audio can never
        // reach it; the parked-caller machinery re-establishes a FRESH
        // session with the resume greeting if the call returns. The
        // WebSocket is deliberately disposable — same policy as the
        // parked-caller resume path.
        if realtime_lane.as_ref().is_some_and(|lane| {
            lane.begun
                && !tracker
                    .current()
                    .is_some_and(|call| call.is_active() && call.id == lane.call_id)
        }) {
            if let Some(lane) = realtime_lane.take() {
                if let Some(item_id) = lane.output_pacer.active_item() {
                    let _ = lane.session.cancel_output(
                        item_id,
                        lane.output_pacer.audible_played_ms(Instant::now()),
                    );
                }
                lane.session
                    .stop("realtime call left the foreground (switchboard)");
                bt.flush_tx_audio();
                *aec = None;
                status.realtime_ready.store(false, Ordering::Relaxed);
                eprintln!(
                    "[aokie-plugin] realtime session for {} disposed after leaving the foreground; a fresh session resumes it if the call returns",
                    lane.call_id
                );
                *realtime_resume_call = Some(lane.call_id.clone());
            }
        }

        let mut realtime_failure: Option<(
            String,
            bool,
            Option<crate::remote_media::AokieOwnerFence>,
            String,
        )> = None;
        if let Some(lane) = realtime_lane.as_mut() {
            for _ in 0..64 {
                let event = match lane.session.try_recv() {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(error) => {
                        realtime_failure =
                            Some((lane.call_id.clone(), lane.begun, lane.owner.clone(), error));
                        break;
                    }
                };
                if event.call_id != lane.call_id || event.generation != lane.generation {
                    realtime_failure = Some((
                        lane.call_id.clone(),
                        lane.begun,
                        lane.owner.clone(),
                        "Desktop realtime returned stale call authority".into(),
                    ));
                    break;
                }
                match event.kind {
                    crate::realtime_voice::RealtimeEventKind::Ready { destination_origin } => {
                        lane.ready = true;
                        status.realtime_ready.store(true, Ordering::Relaxed);
                        *status.realtime_destination.lock().unwrap() = Some(destination_origin);
                    }
                    crate::realtime_voice::RealtimeEventKind::SpeechStarted => {
                        // Caller speech no longer cancels a pending finish
                        // outright — its COMPLETED transcript decides
                        // (courtesy proceeds, a resumed turn cancels).
                        // Onset only DELAYS the first physical CHUP so
                        // that transcript has time to arrive.
                        lane.note_caller_activity();
                        if let Some(pending) = lane.pending_hangup.as_mut() {
                            if pending.attempts == 0 {
                                if let Some(ready_at) = pending.ready_at {
                                    let ceiling =
                                        pending.requested_at + Duration::from_secs(8);
                                    let pushed = (Instant::now()
                                        + Duration::from_millis(1_500))
                                    .min(ceiling);
                                    if pushed > ready_at {
                                        pending.ready_at = Some(pushed);
                                    }
                                }
                            }
                        }
                        if let Some(item_id) =
                            lane.output_pacer.active_item().map(str::to_string)
                        {
                            let heard_ms = lane.output_pacer.audible_played_ms(Instant::now());
                            let total_ms = lane.output_total_samples.saturating_mul(1_000)
                                / lane.sco_rate.max(1) as u64;
                            let transcript = lane
                                .completed_transcript
                                .take()
                                .or_else(|| lane.output_transcript.take())
                                .filter(|(transcript_item, _)| transcript_item == &item_id);
                            if let Some((_, text)) = transcript {
                                let (audible, _) = estimate_audible_prefix(
                                    text.trim(),
                                    1.0,
                                    &CutEstimate {
                                        audible_ms: heard_ms,
                                        queued_ms: lane
                                            .output_pacer
                                            .audible_played_ms(Instant::now())
                                            .saturating_add(
                                                crate::realtime_voice::OUTPUT_LEAD_MS,
                                            ),
                                        synthesized_ms: (total_ms > 0).then_some(total_ms),
                                    },
                                );
                                if !audible.is_empty() {
                                    emit_turn_with_delivery(
                                        outbox,
                                        sink,
                                        &lane.call_id,
                                        ctx.turn_index,
                                        "bot",
                                        &audible,
                                        Some("interrupted"),
                                        None,
                                    );
                                    ctx.history.push(serde_json::json!({
                                        "role": "assistant",
                                        "content": audible,
                                    }));
                                    ctx.turn_index += 1;
                                }
                            }
                            if let Err(error) = lane.session.cancel_output(&item_id, heard_ms) {
                                realtime_failure = Some((
                                    lane.call_id.clone(),
                                    lane.begun,
                                    lane.owner.clone(),
                                    error,
                                ));
                                break;
                            }
                            lane.cancelled_item = Some(item_id);
                        }
                        bt.flush_tx_audio();
                        lane.output_pacer.clear();
                        lane.output_resampler = crate::realtime_voice::StreamingResampler::new(
                            crate::realtime_voice::WIRE_SAMPLE_RATE,
                            lane.sco_rate,
                        );
                        lane.output_transcript = None;
                        lane.completed_transcript = None;
                        lane.last_completed_output = None;
                        lane.output_total_samples = 0;
                        if let Some(cancelled_aec) = aec.as_mut() {
                            cancelled_aec.reset();
                        }
                    }
                    crate::realtime_voice::RealtimeEventKind::InputTranscript {
                        text, ..
                    } => {
                        if lane.begun
                            && tracker.call_id() == Some(lane.call_id.as_str())
                            && reply_owner_is_current(&remote_media, lane.owner.as_ref())
                            && !text.trim().is_empty()
                        {
                            emit_turn(
                                outbox,
                                sink,
                                &lane.call_id,
                                ctx.turn_index,
                                "caller",
                                text.trim(),
                            );
                            ctx.history.push(serde_json::json!({
                                "role": "user",
                                "content": text.trim(),
                            }));
                            status
                                .last_caller_turn
                                .store(ctx.turn_index, Ordering::Relaxed);
                            lane.latest_caller_turn =
                                Some((ctx.turn_index, text.trim().to_string()));
                            // Content-aware finish cancellation: a real
                            // resumed turn cancels the armed hangup; a
                            // courtesy "bye"/"thanks"/"hi" over the
                            // farewell lets it proceed instead of
                            // stranding the line open.
                            if lane.authorized_finish_tool.is_some()
                                || lane.pending_hangup.is_some()
                            {
                                if realtime_caller_turn_resumes_conversation(text.trim()) {
                                    lane.authorized_finish_tool = None;
                                    lane.pending_hangup = None;
                                    eprintln!(
                                        "[aokie-plugin] caller resumed the conversation; realtime hangup cancelled"
                                    );
                                } else {
                                    eprintln!(
                                        "[aokie-plugin] caller courtesy during the farewell; realtime hangup continues"
                                    );
                                }
                            }
                            ctx.turn_index += 1;
                            if let Some(timer) = ctx.silence_timer.as_mut() {
                                timer.note_activity(Instant::now());
                            }
                        }
                    }
                    crate::realtime_voice::RealtimeEventKind::OutputItemStarted { item_id } => {
                        if !lane.begun {
                            realtime_failure = Some((
                                lane.call_id.clone(),
                                false,
                                lane.owner.clone(),
                                "Desktop realtime generated output before the call was begun"
                                    .into(),
                            ));
                            break;
                        }
                        if lane.output_pacer.start_item(&item_id, Instant::now()).is_err() {
                            // The predecessor's TAIL is still playing —
                            // generation runs ahead of real-time playout,
                            // and bridge-initiated responses (refusals,
                            // failure retries) can land while it drains.
                            // Newer speech SUPERSEDES it: record the
                            // audible prefix truthfully, drop the unplayed
                            // tail, and play the new item. Ending the live
                            // call here (the pre-fix behavior) is never
                            // the right outcome for a playout overlap.
                            if let Some(previous) =
                                lane.output_pacer.active_item().map(str::to_string)
                            {
                                let heard_ms =
                                    lane.output_pacer.audible_played_ms(Instant::now());
                                let total_ms = lane
                                    .output_total_samples
                                    .saturating_mul(1_000)
                                    / lane.sco_rate.max(1) as u64;
                                let transcript = lane
                                    .completed_transcript
                                    .take()
                                    .or_else(|| lane.output_transcript.take())
                                    .filter(|(transcript_item, _)| {
                                        transcript_item == &previous
                                    });
                                if let Some((_, text)) = transcript {
                                    let (audible, _) = estimate_audible_prefix(
                                        text.trim(),
                                        1.0,
                                        &CutEstimate {
                                            audible_ms: heard_ms,
                                            queued_ms: heard_ms.saturating_add(
                                                crate::realtime_voice::OUTPUT_LEAD_MS,
                                            ),
                                            synthesized_ms: (total_ms > 0)
                                                .then_some(total_ms),
                                        },
                                    );
                                    if !audible.is_empty() {
                                        emit_turn_with_delivery(
                                            outbox,
                                            sink,
                                            &lane.call_id,
                                            ctx.turn_index,
                                            "bot",
                                            &audible,
                                            Some("interrupted"),
                                            None,
                                        );
                                        ctx.history.push(serde_json::json!({
                                            "role": "assistant",
                                            "content": audible,
                                        }));
                                        ctx.turn_index += 1;
                                    }
                                }
                                eprintln!(
                                    "[aokie-plugin] realtime output item {item_id} superseded the still-playing {previous}; its unplayed tail was dropped"
                                );
                            }
                            bt.flush_tx_audio();
                            lane.output_pacer.clear();
                            lane.output_resampler =
                                crate::realtime_voice::StreamingResampler::new(
                                    crate::realtime_voice::WIRE_SAMPLE_RATE,
                                    lane.sco_rate,
                                );
                            if let Err(error) =
                                lane.output_pacer.start_item(&item_id, Instant::now())
                            {
                                realtime_failure = Some((
                                    lane.call_id.clone(),
                                    lane.begun,
                                    lane.owner.clone(),
                                    error,
                                ));
                                break;
                            }
                        }
                        lane.output_transcript = None;
                        lane.completed_transcript = None;
                        lane.last_completed_output = None;
                        lane.output_total_samples = 0;
                        lane.cancelled_item = None;
                    }
                    crate::realtime_voice::RealtimeEventKind::OutputPcm {
                        item_id,
                        samples,
                    } => {
                        if realtime_output_is_cancelled(
                            lane.cancelled_item.as_deref(),
                            &item_id,
                        ) {
                            continue;
                        }
                        if !lane.begun {
                            realtime_failure = Some((
                                lane.call_id.clone(),
                                false,
                                lane.owner.clone(),
                                "Desktop realtime sent PCM before the call was begun".into(),
                            ));
                            break;
                        }
                        let converted = lane.output_resampler.process(&samples);
                        lane.output_total_samples = lane
                            .output_total_samples
                            .saturating_add(converted.len() as u64);
                        match lane.output_pacer.push(&item_id, &converted) {
                            Ok(()) => {}
                            Err(
                                crate::realtime_voice::OutputPacerPushError::CapacityExceeded,
                            ) => {
                                // A provider is allowed to synthesize much faster than
                                // wall-clock playout. If even the bounded 30-second
                                // reservoir is exhausted, abandon only this exact output
                                // item. The session and cellular call remain healthy; the
                                // caller can immediately start another turn. The radio-side
                                // tombstone and socket parser both discard the ordered late
                                // PCM tail until item_done.
                                let heard_ms =
                                    lane.output_pacer.audible_played_ms(Instant::now());
                                if let Err(error) =
                                    lane.session.cancel_output(&item_id, heard_ms)
                                {
                                    realtime_failure = Some((
                                        lane.call_id.clone(),
                                        lane.begun,
                                        lane.owner.clone(),
                                        error,
                                    ));
                                    break;
                                }
                                eprintln!(
                                    "[aokie-plugin] Desktop realtime response exceeded the bounded playout reservoir; cancelled exact output item while keeping the call active"
                                );
                                lane.cancelled_item = Some(item_id);
                                bt.flush_tx_audio();
                                lane.output_pacer.clear();
                                lane.output_resampler =
                                    crate::realtime_voice::StreamingResampler::new(
                                        crate::realtime_voice::WIRE_SAMPLE_RATE,
                                        lane.sco_rate,
                                    );
                                lane.output_transcript = None;
                                lane.completed_transcript = None;
                                lane.output_total_samples = 0;
                                if let Some(overflow_aec) = aec.as_mut() {
                                    overflow_aec.reset();
                                }
                            }
                            Err(error) => {
                                realtime_failure = Some((
                                    lane.call_id.clone(),
                                    lane.begun,
                                    lane.owner.clone(),
                                    error.to_string(),
                                ));
                                break;
                            }
                        }
                    }
                    crate::realtime_voice::RealtimeEventKind::OutputTranscript {
                        item_id,
                        text,
                    } => {
                        if !realtime_output_is_cancelled(
                            lane.cancelled_item.as_deref(),
                            &item_id,
                        ) {
                            lane.output_transcript = Some((item_id, text));
                        }
                    }
                    crate::realtime_voice::RealtimeEventKind::OutputItemDone { item_id } => {
                        if realtime_output_is_cancelled(
                            lane.cancelled_item.as_deref(),
                            &item_id,
                        ) {
                            lane.cancelled_item = None;
                            lane.output_transcript = None;
                            lane.completed_transcript = None;
                            lane.output_total_samples = 0;
                            continue;
                        }
                        if let Err(error) = lane.output_pacer.finish_item(&item_id) {
                            realtime_failure = Some((
                                lane.call_id.clone(),
                                lane.begun,
                                lane.owner.clone(),
                                error,
                            ));
                            break;
                        }
                        if let Some((transcript_item, text)) = lane.output_transcript.take() {
                            if transcript_item == item_id && !text.trim().is_empty() {
                                lane.completed_transcript = Some((item_id, text));
                            }
                        }
                    }
                    crate::realtime_voice::RealtimeEventKind::ToolCall {
                        tool_call_id,
                        name,
                        arguments,
                    } => {
                        if !lane.begun
                            || lane.pending_tool_call.is_some()
                            || lane.pending_business_lookup.is_some()
                            || lane.completed_tool_calls.len() >= 8
                            || lane
                                .completed_tool_calls
                                .iter()
                                .any(|completed| completed == &tool_call_id)
                        {
                            realtime_failure = Some((
                                lane.call_id.clone(),
                                lane.begun,
                                lane.owner.clone(),
                                "Desktop realtime repeated or overlapped a tool call".into(),
                            ));
                            break;
                        }
                        // If the response spoke a short preamble first,
                        // let its exact PCM drain before beginning the one
                        // tool. Host lookups are then polled asynchronously.
                        lane.pending_tool_call = Some((
                            tool_call_id,
                            name,
                            arguments,
                            lane.caller_activity_revision,
                        ));
                    }
                    crate::realtime_voice::RealtimeEventKind::HangupRequested {
                        tool_call_id,
                        response_id,
                        item_id,
                    } => {
                        let authorized = lane
                            .authorized_finish_tool
                            .as_ref()
                            .is_some_and(|(authorized, _)| authorized == &tool_call_id);
                        if authorized
                            && lane.pending_hangup.is_none()
                            && lane.begun
                            && tracker.current().is_some_and(|call| {
                                call.is_active() && (!call.outbound || call.agent_owned) && call.id == lane.call_id
                            })
                            && reply_owner_is_current(&remote_media, lane.owner.as_ref())
                        {
                            lane.authorized_finish_tool = None;
                            lane.pending_hangup = Some(PendingRealtimeHangup {
                                tool_call_id,
                                response_id,
                                item_id,
                                requested_at: Instant::now(),
                                ready_at: None,
                                attempts: 0,
                            });
                        } else {
                            eprintln!(
                                "[aokie-plugin] ignored stale or unauthorized Desktop realtime hangup request"
                            );
                        }
                    }
                    crate::realtime_voice::RealtimeEventKind::Error {
                        code,
                        message,
                        fatal,
                        abandoned_item_id,
                    } => {
                        let description = format!(
                            "{}{}",
                            code.map(|code| format!("{code}: ")).unwrap_or_default(),
                            message
                        );
                        if realtime_error_is_terminal(fatal) {
                            realtime_failure = Some((
                                lane.call_id.clone(),
                                lane.begun,
                                lane.owner.clone(),
                                description,
                            ));
                            break;
                        }
                        eprintln!(
                            "[aokie-plugin] Desktop realtime response failed non-fatally; session remains active: {description}"
                        );
                        lane.output_pacer.clear();
                        lane.output_resampler = crate::realtime_voice::StreamingResampler::new(
                            crate::realtime_voice::WIRE_SAMPLE_RATE,
                            lane.sco_rate,
                        );
                        lane.output_transcript = None;
                        lane.completed_transcript = None;
                        lane.output_total_samples = 0;
                        lane.authorized_finish_tool = None;
                        lane.pending_hangup = None;
                        // Keep the exact failed/cancelled output fenced
                        // until its ordered item_done. Late frames are
                        // discarded by the parser and this radio-layer
                        // tombstone prevents any already-queued event from
                        // reaching the cleared physical playout lane.
                        lane.cancelled_item = realtime_retain_abandoned_output(
                            lane.cancelled_item.take(),
                            abandoned_item_id,
                        );
                        if lane.begun
                            && reply_owner_is_current(&remote_media, lane.owner.as_ref())
                        {
                            bt.flush_tx_audio();
                            if let Some(nonfatal_aec) = aec.as_mut() {
                                nonfatal_aec.reset();
                            }
                        }
                    }
                    crate::realtime_voice::RealtimeEventKind::Closed { reason } => {
                        realtime_failure = Some((
                            lane.call_id.clone(),
                            lane.begun,
                            lane.owner.clone(),
                            reason,
                        ));
                        break;
                    }
                }
            }
        }

        if realtime_failure.is_none() {
            if let Some(lane) = realtime_lane.as_mut() {
                let ownership_changed = remote_media.radio_reserved()
                    || (lane.begun
                        && !reply_owner_is_current(&remote_media, lane.owner.as_ref()));
                if ownership_changed {
                    // A Companion claim/return changes the exact owner
                    // fence. Close Realtime immediately and prepare a fresh
                    // provider session only after human media is released;
                    // never compete for caller PCM.
                    if let Some(item_id) = lane.output_pacer.active_item() {
                        let _ = lane.session.cancel_output(
                            item_id,
                            lane.output_pacer.audible_played_ms(Instant::now()),
                        );
                    }
                    lane.session.stop("Aokie media ownership changed");
                    bt.flush_tx_audio();
                    lane.output_pacer.clear();
                    if let Some(owner_aec) = aec.as_mut() {
                        owner_aec.reset();
                    }
                    *realtime_resume_call = Some(lane.call_id.clone());
                    status.realtime_ready.store(false, Ordering::Relaxed);
                    *realtime_lane = None;
                }
            }
        }

        if realtime_failure.is_none() {
            if let Some(lane) = realtime_lane.as_mut() {
                if lane.ready && !lane.begun {
                    let identity_settled = tracker.current().is_some_and(|call| {
                        realtime_identity_settled(
                            call.caller_id
                                .as_deref()
                                .is_some_and(|caller_id| !caller_id.is_empty()),
                            realtime_answered_at
                                .as_ref()
                                .filter(|(call_id, _)| call_id == &call.id)
                                .map(|(_, answered_at)| answered_at.elapsed())
                                .unwrap_or_default(),
                        )
                    });
                    let can_begin = tracker.current().is_some_and(|call| {
                        call.is_active()
                            && call.id == lane.call_id
                            && if call.outbound {
                                // Agent-placed dials skip inbound
                                // screening: we chose to place this call
                                // and the opening line must speak.
                                call.agent_owned
                            } else {
                                screen_policy.verdict(call.caller_id.as_deref()).is_none()
                                    && !screen_policy.is_manager(call.caller_id.as_deref())
                            }
                    }) && identity_settled;
                    if can_begin && !bt.realtime_call_audio_supported() {
                        realtime_failure = Some((
                            lane.call_id.clone(),
                            false,
                            lane.owner.clone(),
                            format!(
                                "{} cannot expose phone-call PCM to Desktop realtime voice",
                                bt.backend_name()
                            ),
                        ));
                    } else if can_begin && bt.get_sample_rate() > 0 {
                        if let Some(owner) = aokie_owner_for_call(&remote_media, &lane.call_id)
                        {
                            // The session connected at RING with the
                            // global persona/greeting; the personalize-
                            // caller overlay ("Hi Lance!") usually lands
                            // between ring and answer. Begin carries the
                            // freshest call-scoped values so the spoken
                            // greeting is the personalized one.
                            let begin_instructions = ctx
                                .call_agent_overlay
                                .as_ref()
                                .filter(|overlay| overlay.call_id == lane.call_id)
                                .and_then(|overlay| overlay.persona.as_deref())
                                .map(|persona| {
                                    realtime_safe_instructions(persona, agent_hangup)
                                });
                            let begin_greeting = ctx
                                .call_agent_overlay
                                .as_ref()
                                .filter(|overlay| overlay.call_id == lane.call_id)
                                .and_then(|overlay| overlay.greeting.as_deref())
                                .map(str::trim)
                                .filter(|text| !text.is_empty())
                                .map(str::to_string);
                            match lane.session.begin(begin_instructions, begin_greeting) {
                                Ok(()) => {
                                    let sr = bt.get_sample_rate() as u32;
                                    lane.reset_sco_rate(sr);
                                    lane.owner = Some(owner);
                                    lane.begun = true;
                                    *aec = Some(crate::aec::EchoCanceller::new(sr));
                                    if let Some(call) = tracker.current_mut() {
                                        call.greeted = true;
                                    }
                                    eprintln!(
                                        "[aokie-plugin] Desktop realtime voice begun for active call {} at {sr}Hz SCO",
                                        lane.call_id
                                    );
                                    // Warm the LOCAL engine in the
                                    // background: the hold ceremony and
                                    // switchboard announcements speak
                                    // locally even on realtime calls, and
                                    // a cold engine load must not delay
                                    // the first "please hold" (no-op when
                                    // already loaded).
                                    synth.warm();
                                }
                                Err(error) => {
                                    realtime_failure =
                                        Some((lane.call_id.clone(), false, Some(owner), error));
                                }
                            }
                        }
                    }
                }
            }
        }

        if realtime_failure.is_none() {
            if let Some(lane) = realtime_lane.as_mut().filter(|lane| lane.begun) {
                let sr = bt.get_sample_rate() as u32;
                if sr > 0 {
                    if lane.sco_rate != sr {
                        realtime_failure = Some((
                            lane.call_id.clone(),
                            true,
                            lane.owner.clone(),
                            format!(
                                "SCO sample rate changed from {}Hz to {sr}Hz during realtime voice",
                                lane.sco_rate
                            ),
                        ));
                    }
                    let chunk = if realtime_failure.is_none() {
                        lane.output_pacer
                            .take_ready(Instant::now(), (sr as usize / 50).max(1))
                    } else {
                        Vec::new()
                    };
                    if !chunk.is_empty() {
                        if let Some(expected) = lane.owner.as_ref() {
                            if let Err(reason) = remote_media.with_aokie_owner(expected, || {
                                if let Some(reference_aec) = aec.as_mut() {
                                    reference_aec.feed_reference(&chunk);
                                }
                                AudioLink::send_audio(bt, &chunk);
                            }) {
                                realtime_failure = Some((
                                    lane.call_id.clone(),
                                    true,
                                    lane.owner.clone(),
                                    reason,
                                ));
                            }
                        }
                    }
                    if realtime_failure.is_none() && lane.output_pacer.active_item().is_none() {
                        if let Some((item_id, text)) = lane.completed_transcript.take() {
                            if !text.trim().is_empty() {
                                emit_turn_with_delivery(
                                    outbox,
                                    sink,
                                    &lane.call_id,
                                    ctx.turn_index,
                                    "bot",
                                    text.trim(),
                                    Some("complete"),
                                    None,
                                );
                                ctx.history.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": text.trim(),
                                }));
                                ctx.last_bot_reply = text.trim().to_string();
                                ctx.last_bot_speech = text.trim().to_string();
                                ctx.turn_index += 1;
                                lane.last_completed_output = Some((
                                    item_id,
                                    text.trim().to_string(),
                                    lane.output_total_samples,
                                    Instant::now(),
                                ));
                                lane.output_total_samples = 0;
                            }
                        }
                    }
                }
            }
        }

        // Execute at most one provider-requested tool after any spoken
        // preamble has drained. The pacer plays exactly ONE output item at
        // a time (OutputPacer::start_item hard-fails on overlap), so a
        // tool continuation delivered while a preamble still plays would
        // end the call — the gate below is what serializes them. The
        // provider supplies only the natural language question; caller
        // identity, call id, manager status, and media authority always
        // come from trusted plugin state.
        if realtime_failure.is_none() {
            if let Some(lane) = realtime_lane
                .as_mut()
                .filter(|lane| lane.begun && lane.output_pacer.active_item().is_none())
            {
                let mut completion: Option<(String, String, bool, serde_json::Value, bool)> =
                    None;

                // A host lookup can legitimately take seconds. Poll it;
                // never block this radio loop, which also owns continuous
                // SCO capture, hang-up controls, and Realtime PCM ingress.
                if let Some(pending) = lane.pending_business_lookup.as_mut() {
                    if let Some((digest, spoken)) =
                        poll_business_lookup(&mut pending.lookup, Instant::now())
                    {
                        let pending = lane
                            .pending_business_lookup
                            .take()
                            .expect("polled realtime lookup remains current");
                        // A lookup is read-only: caller speech never voids
                        // its data. Deliver the digest with a continuation;
                        // if a caller-triggered response is already active
                        // the bridge resolves the collision and the model
                        // reads the result on its next turn.
                        let available = !digest.starts_with("LOOKUP UNAVAILABLE");
                        completion = Some((
                            pending.tool_call_id,
                            pending.name,
                            available,
                            realtime_lookup_tool_output(available, &digest, spoken.as_deref()),
                            true,
                        ));
                    }
                } else if let Some((tool_call_id, name, arguments, activity_revision)) =
                    lane.pending_tool_call.take()
                {
                    let exact_call = tracker.current().is_some_and(|call| {
                        call.is_active() && (!call.outbound || call.agent_owned) && call.id == lane.call_id
                    });
                    let exact_owner =
                        reply_owner_is_current(&remote_media, lane.owner.as_ref());
                    if !exact_call || !exact_owner {
                        completion = Some((
                            tool_call_id,
                            name,
                            false,
                            serde_json::json!({
                                "error": "The live call authority changed; no action was performed."
                            }),
                            true,
                        ));
                    } else if realtime_tool_invalidated_by_caller(
                        &name,
                        activity_revision,
                        lane.caller_activity_revision,
                    ) && !lane.latest_caller_turn.as_ref().is_some_and(|(_, text)| {
                        // "Yes." followed a beat later by "Yes, that'd be
                        // good." must not void the booking: when the
                        // caller's NEWEST completed turn is itself a clear
                        // agreement, consent is fresher than the snapshot
                        // and validate() runs against that newest turn.
                        crate::realtime_appointment::is_conservative_agreement(text)
                    }) {
                        eprintln!(
                            "[aokie-plugin] realtime tool {name} refused: caller activity superseded the consent snapshot"
                        );
                        completion = Some((
                            tool_call_id,
                            name,
                            false,
                            serde_json::json!({
                                "available": false,
                                "error": "The caller continued speaking before the action began; answer their latest turn instead."
                            }),
                            false,
                        ));
                    } else if name == "lookup_business_data" {
                        let object = arguments.as_object();
                        let question = object
                            .filter(|object| {
                                object.len() == 1 && object.contains_key("question")
                            })
                            .and_then(|object| object.get("question"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::trim)
                            .filter(|question| {
                                !question.is_empty()
                                    && question.len() <= 500
                                    && !question.contains('\0')
                            });
                        if let Some(question) = question {
                            let from = tracker
                                .current()
                                .and_then(|call| call.caller_id.clone())
                                .unwrap_or_default();
                            let pending = begin_business_lookup(
                                &host_rpc,
                                sink,
                                question,
                                &lane.call_id,
                                &from,
                                false,
                            );
                            if let Some(lookup) = pending {
                                lane.pending_business_lookup =
                                    Some(PendingRealtimeBusinessLookup {
                                        tool_call_id,
                                        name,
                                        lookup,
                                    });
                            } else {
                                completion = Some((
                                    tool_call_id,
                                    name,
                                    false,
                                    realtime_lookup_tool_output(
                                        false,
                                        "LOOKUP UNAVAILABLE (host offline)",
                                        None,
                                    ),
                                    true,
                                ));
                            }
                        } else {
                            completion = Some((
                                tool_call_id,
                                name,
                                false,
                                serde_json::json!({
                                    "error": "The lookup question was missing or invalid."
                                }),
                                true,
                            ));
                        }
                    } else if name == "request_appointment" {
                        let caller_history: Vec<String> = ctx
                            .history
                            .iter()
                            .rev()
                            .filter(|entry| {
                                entry.get("role").and_then(serde_json::Value::as_str)
                                    == Some("user")
                            })
                            .filter_map(|entry| {
                                entry
                                    .get("content")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string)
                            })
                            .take(24)
                            .collect();
                        let latest_caller_turn = lane
                            .latest_caller_turn
                            .as_ref()
                            .map(|(turn, text)| (*turn, text.as_str()));
                        match crate::realtime_appointment::validate(
                            &arguments,
                            &lane.call_id,
                            latest_caller_turn,
                            &caller_history,
                            chrono::Local::now().date_naive(),
                        ) {
                            Err(error) => {
                                // The refusal reason must be diagnosable
                                // post-hoc: live calls needed 2-3 confirm
                                // rounds and only the model's paraphrase
                                // hinted at why (2026-07-21).
                                eprintln!(
                                    "[aokie-plugin] realtime request_appointment refused: {error}"
                                );
                                completion = Some((
                                    tool_call_id,
                                    name,
                                    false,
                                    serde_json::json!({
                                        "recorded": false,
                                        "status": "not_recorded",
                                        "error": error,
                                    }),
                                    true,
                                ));
                            }
                            Ok(request) => {
                                let duplicate = lane
                                    .completed_appointment_requests
                                    .iter()
                                    .any(|completed| completed == &request.request_id);
                                let capacity_available =
                                    lane.completed_appointment_requests.len() < 3;
                                if !capacity_available && !duplicate {
                                    completion = Some((
                                        tool_call_id,
                                        name,
                                        false,
                                        serde_json::json!({
                                            "recorded": false,
                                            "status": "not_recorded",
                                            "error": "This call already reached the safe appointment-request limit. Staff must follow up."
                                        }),
                                        true,
                                    ));
                                } else {
                                    let refreshed_owner = if duplicate {
                                        lane.owner.clone()
                                    } else {
                                        lane.owner.as_ref().and_then(|expected_owner| {
                                            remote_media
                                                .linearize_aokie_action(expected_owner)
                                                .ok()
                                        })
                                    };
                                    if let Some(refreshed_owner) = refreshed_owner {
                                        // `linearize_aokie_action` advances the dedicated
                                        // autonomous-action epoch. Keep the Realtime lane on
                                        // the returned exact fence so its tool result and all
                                        // later replies are not mistaken for stale ownership.
                                        lane.owner = Some(refreshed_owner);
                                        let from = tracker
                                            .current()
                                            .and_then(|call| call.caller_id.clone())
                                            .unwrap_or_default();
                                        let recorded = duplicate
                                            || emit_realtime_appointment_request(
                                                outbox,
                                                sink,
                                                &lane.call_id,
                                                &from,
                                                &request,
                                            )
                                            .is_ok();
                                        if recorded && !duplicate {
                                            lane.completed_appointment_requests
                                                .push(request.request_id.clone());
                                        }
                                        completion = Some((
                                            tool_call_id,
                                            name,
                                            recorded,
                                            if recorded {
                                                serde_json::json!({
                                                    "recorded": true,
                                                    "duplicate": duplicate,
                                                    "status": "requested",
                                                    "requestId": request.request_id,
                                                    "callerName": request.caller_name,
                                                    "service": request.service,
                                                    "date": request.date,
                                                    "time": request.time,
                                                    "instruction": "The request is queued for staff confirmation; it is not a confirmed booking."
                                                })
                                            } else {
                                                serde_json::json!({
                                                    "recorded": false,
                                                    "status": "not_recorded",
                                                    "error": "The appointment request could not be durably recorded. Staff must follow up."
                                                })
                                            },
                                            true,
                                        ));
                                    } else {
                                        completion = Some((
                                            tool_call_id,
                                            name,
                                            false,
                                            serde_json::json!({
                                                "recorded": false,
                                                "status": "not_recorded",
                                                "error": "The live call authority changed; no appointment request was recorded."
                                            }),
                                            true,
                                        ));
                                    }
                                }
                            }
                        }
                    } else if name == "finish_call" {
                        let valid_arguments =
                            arguments.as_object().is_some_and(serde_json::Map::is_empty);
                        let accepted = realtime_finish_call_allowed(
                            agent_hangup,
                            valid_arguments,
                            activity_revision,
                            lane.caller_activity_revision,
                        );
                        if accepted {
                            lane.authorized_finish_tool =
                                Some((tool_call_id.clone(), lane.caller_activity_revision));
                        }
                        completion = Some((
                            tool_call_id,
                            name,
                            accepted,
                            if accepted {
                                serde_json::json!({
                                    "accepted": true,
                                    "instruction": "Say one brief goodbye now. Do not ask a question."
                                })
                            } else {
                                serde_json::json!({
                                    "accepted": false,
                                    "error": "Call finishing is disabled or the request was invalid."
                                })
                            },
                            true,
                        ));
                    } else {
                        completion = Some((
                            tool_call_id,
                            name,
                            false,
                            serde_json::json!({"error": "Unsupported realtime tool."}),
                            true,
                        ));
                    }
                }

                if let Some((tool_call_id, name, ok, output, continue_response)) = completion {
                    let exact_call = tracker.current().is_some_and(|call| {
                        call.is_active() && (!call.outbound || call.agent_owned) && call.id == lane.call_id
                    });
                    if !exact_call
                        || !reply_owner_is_current(&remote_media, lane.owner.as_ref())
                    {
                        realtime_failure = Some((
                            lane.call_id.clone(),
                            true,
                            lane.owner.clone(),
                            "Aokie ownership changed while a realtime tool was running".into(),
                        ));
                    } else if let Err(error) = lane.session.complete_tool(
                        &tool_call_id,
                        &name,
                        ok,
                        output,
                        continue_response,
                    ) {
                        realtime_failure =
                            Some((lane.call_id.clone(), true, lane.owner.clone(), error));
                    } else {
                        lane.completed_tool_calls.push(tool_call_id);
                    }
                }
            }
        }

        // A finish_call is only a request. Desktop generates a separate,
        // tools-disabled goodbye and reports its exact completed item.
        // Wait until that PCM has drained into SCO plus the 80 ms lead,
        // then re-check every physical/call/owner fence before CHUP.
        if realtime_failure.is_none() {
            if let Some(lane) = realtime_lane.as_mut().filter(|lane| lane.begun) {
                let now = Instant::now();
                let mut discard = false;
                if let Some(pending) = lane.pending_hangup.as_mut() {
                    if pending.requested_at.elapsed() > Duration::from_secs(10) {
                        discard = true;
                    } else if pending.ready_at.is_none()
                        && lane.output_pacer.active_item().is_none()
                    {
                        if let Some((item_id, text, samples, completed_at)) =
                            lane.last_completed_output.as_ref()
                        {
                            let valid_farewell = realtime_farewell_can_arm(
                                &pending.item_id,
                                item_id,
                                text,
                                *samples,
                                lane.sco_rate,
                                completed_at.elapsed(),
                            );
                            if valid_farewell {
                                // Leave a short local-SCO veto window after
                                // the final played sample. This is longer
                                // than the provider/network VAD path, so a
                                // caller starting "wait" is observed below
                                // before the first physical CHUP attempt.
                                pending.ready_at = Some(
                                    now + Duration::from_millis(
                                        (crate::realtime_voice::OUTPUT_LEAD_MS + 20).max(350),
                                    ),
                                );
                            } else if completed_at.elapsed() <= Duration::from_secs(5) {
                                discard = true;
                            }
                        }
                    }
                }
                if discard {
                    lane.pending_hangup = None;
                }

                let due = lane
                    .pending_hangup
                    .as_ref()
                    .and_then(|pending| pending.ready_at)
                    .is_some_and(|ready_at| now >= ready_at);
                if due {
                    let mut pending = lane.pending_hangup.take().expect("due hangup request");
                    let exact_call = tracker.current().is_some_and(|call| {
                        call.is_active() && (!call.outbound || call.agent_owned) && call.id == lane.call_id
                    });
                    let human_reserved = remote_media.radio_reserved();
                    let exact_owner =
                        reply_owner_is_current(&remote_media, lane.owner.as_ref());
                    if realtime_finish_attempt_allowed(
                        agent_hangup,
                        exact_call,
                        human_reserved,
                        exact_owner,
                        pending.attempts,
                    ) {
                        let expected = lane
                            .owner
                            .as_ref()
                            .expect("begun realtime lane has an Aokie owner")
                            .clone();
                        eprintln!(
                            "[aokie-plugin] realtime finish_call {} completed farewell response {}; ending exact call (attempt {})",
                            pending.tool_call_id,
                            pending.response_id,
                            pending.attempts + 1,
                        );
                        let result = remote_media.with_aokie_owner(&expected, || bt.hangup());
                        let keep_retrying = match result {
                            Ok(Ok(())) => {
                                tracker.note_intent(
                                    crate::call_session::TerminationIntent::AgentHangup,
                                );
                                ctx.agent_hung_up = true;
                                true
                            }
                            Ok(Err(error)) => {
                                eprintln!(
                                    "[aokie-plugin] realtime finish_call hangup failed: {error}"
                                );
                                true
                            }
                            Err(reason) => {
                                eprintln!(
                                    "[aokie-plugin] realtime finish_call lost ownership: {reason}"
                                );
                                false
                            }
                        };
                        pending.attempts = pending.attempts.saturating_add(1);
                        if keep_retrying && pending.attempts < 3 {
                            // Submission is not physical completion. Keep
                            // the live lane listening, and retain the exact
                            // caller-activity revision across each bounded
                            // retry. Local or provider speech clears this
                            // pending request before the next attempt.
                            pending.ready_at =
                                Some(now + std::time::Duration::from_millis(1500));
                            lane.pending_hangup = Some(pending);
                        }
                    }
                }
            }
        }

        if let Some((call_id, _begun, owner, reason)) = realtime_failure {
            if let Some(lane) = realtime_lane.take() {
                lane.session.stop("realtime voice failed");
            }
            bt.flush_tx_audio();
            if let Some(failed_aec) = aec.as_mut() {
                failed_aec.reset();
            }
            status.realtime_ready.store(false, Ordering::Relaxed);
            *status.realtime_error.lock().unwrap() = Some(reason.clone());
            eprintln!("[aokie-plugin] Desktop realtime voice failed: {reason}");
            let exact_call_active = tracker
                .current()
                .is_some_and(|call| call.is_active() && call.id == call_id);
            let current_owner = aokie_owner_for_call(&remote_media, &call_id);
            let passed_owner_is_current = owner
                .as_ref()
                .is_some_and(|expected| reply_owner_is_current(&remote_media, Some(expected)));
            let exact_owner =
                current_owner.or_else(|| passed_owner_is_current.then_some(owner).flatten());
            match realtime_failure_disposition(
                exact_call_active,
                exact_owner.is_some(),
                remote_media.radio_reserved(),
            ) {
                RealtimeFailureDisposition::RingThrough => {
                    *realtime_failed_call = Some((call_id, reason));
                }
                RealtimeFailureDisposition::FailSafe => {
                    *realtime_midcall_failure = Some((
                        call_id,
                        exact_owner.expect("fail-safe requires an exact Aokie owner"),
                        reason,
                    ));
                }
                RealtimeFailureDisposition::ResumeAfterHuman => {
                    *realtime_resume_call = Some(call_id);
                }
            }
        }
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn service_realtime_failures(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    status: &Arc<RadioStatus>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    synth: &crate::synth::SynthHandle,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    tracker: &mut crate::call_session::SessionTracker,
    realtime_resume_call: &mut Option<String>,
    realtime_answered_at: &mut Option<(String, std::time::Instant)>,
    realtime_midcall_failure: &mut Option<(String, crate::remote_media::AokieOwnerFence, String)>,
    realtime_terminal_call: &mut Option<(String, std::time::Instant, u8)>,
    realtime_deferred_policy_failure: &mut Option<(String, String)>,
    ctx: &mut CallVoiceContext,
) {
    #[cfg(feature = "voice")]
    if let Some((failed_call_id, failed_owner, cause)) = realtime_midcall_failure.clone() {
        let exact_call = tracker
            .current()
            .is_some_and(|call| call.is_active() && call.id == failed_call_id);
        if !exact_call {
            *realtime_midcall_failure = None;
        } else if !reply_owner_is_current(&remote_media, Some(&failed_owner)) {
            // Human ownership always wins. Never speak or end the cellular
            // call underneath a Companion route. Preserve the responder
            // assignment so an eventual return creates a fresh session.
            eprintln!(
                "[aokie-plugin] realtime fail-safe deferred because caller ownership changed"
            );
            *realtime_midcall_failure = None;
            if should_resume_realtime_after_owner_loss(ctx.desktop_realtime_responder) {
                *realtime_resume_call = Some(failed_call_id);
            } else {
                *realtime_deferred_policy_failure = Some((failed_call_id, cause));
            }
            *realtime_terminal_call = None;
        } else if !realtime_failsafe_answer_settled(
            realtime_answered_at
                .as_ref()
                .filter(|(call_id, _)| call_id == &failed_call_id)
                .map(|(_, answered_at)| answered_at.elapsed()),
        ) {
            // Some phones ignore CHUP immediately after ATA. Retain the
            // exact failure and let the answer transition settle first.
        } else {
            let sr = bt.get_sample_rate();
            *realtime_midcall_failure = None;
            *realtime_terminal_call = Some((failed_call_id.clone(), Instant::now(), 0));
            let tts_error = status.tts_error.lock().unwrap().clone();
            let self_test = status.self_test.lock().unwrap().clone();
            let can_speak =
                sr > 0 && realtime_failsafe_can_speak(tts_error.as_deref(), self_test.as_ref());
            let mut operator_ended = false;
            if can_speak {
                eprintln!(
                    "[aokie-plugin] realtime responder failed mid-call ({cause}) — fixed apology then hangup"
                );
                let started = Instant::now();
                let mut probe = ControlProbe::new(&control_rx, &mut *pending_controls);
                let spoken = tts_speak(
                    bt,
                    &synth,
                    FALLBACK_LINE,
                    sr,
                    None,
                    None,
                    Some(&mut probe),
                    1.0,
                    None,
                    None,
                    None,
                );
                note_tts_outcome(&status, &spoken);
                if spoken.dur > Duration::ZERO
                    && reply_owner_is_current(&remote_media, Some(&failed_owner))
                {
                    emit_turn_with_delivery(
                        outbox,
                        sink,
                        &failed_call_id,
                        ctx.turn_index,
                        "bot",
                        FALLBACK_LINE,
                        Some("complete"),
                        Some(&aokie_core::events::iso8601_ago_ms(
                            started.elapsed().as_millis() as u64,
                        )),
                    );
                    ctx.turn_index += 1;
                }
                if let Some(action) = probe.action.take() {
                    perform_cancel_action(action, bt, &mut *tracker, outbox, sink);
                    operator_ended = true;
                    *realtime_terminal_call = Some((failed_call_id.clone(), Instant::now(), 3));
                }
            } else {
                eprintln!(
                    "[aokie-plugin] realtime responder failed mid-call ({cause}); local apology is not proven, hanging up promptly"
                );
            }
            if !operator_ended && !reply_owner_is_current(&remote_media, Some(&failed_owner)) {
                // A Companion claim raced synthesis. AudioLink suppressed
                // post-claim chunks; never issue CHUP under its authority.
                *realtime_terminal_call = None;
                if ctx.desktop_realtime_responder {
                    *realtime_resume_call = Some(failed_call_id.clone());
                } else {
                    *realtime_deferred_policy_failure =
                        Some((failed_call_id.clone(), cause.clone()));
                }
            } else if !operator_ended {
                match remote_media.with_aokie_owner(&failed_owner, || {
                    tracker.note_intent(crate::call_session::TerminationIntent::AgentHangup);
                    bt.hangup()
                }) {
                    Ok(Ok(())) => {
                        ctx.agent_hung_up = true;
                        *realtime_terminal_call =
                            Some((failed_call_id.clone(), Instant::now(), 1));
                    }
                    Ok(Err(error)) => {
                        eprintln!("[aokie-plugin] realtime fail-safe hangup failed: {error}");
                        *realtime_terminal_call =
                            Some((failed_call_id.clone(), Instant::now(), 1));
                    }
                    Err(reason) => {
                        eprintln!(
                            "[aokie-plugin] realtime fail-safe hangup skipped after owner race: {reason}"
                        );
                        *realtime_terminal_call = None;
                        if ctx.desktop_realtime_responder {
                            *realtime_resume_call = Some(failed_call_id);
                        } else {
                            *realtime_deferred_policy_failure =
                                Some((failed_call_id, cause.clone()));
                        }
                    }
                }
            }
        }
    }

    #[cfg(feature = "voice")]
    if let Some((terminal_call_id, last_attempt, attempts)) = realtime_terminal_call.clone() {
        let exact_call = tracker
            .current()
            .is_some_and(|call| call.is_active() && call.id == terminal_call_id);
        if !exact_call {
            *realtime_terminal_call = None;
        } else if remote_media.radio_reserved() {
            *realtime_terminal_call = None;
            if ctx.desktop_realtime_responder {
                *realtime_resume_call = Some(terminal_call_id);
            } else {
                *realtime_deferred_policy_failure = Some((
                    terminal_call_id,
                    "legacy policy responder remains unavailable".to_string(),
                ));
            }
        } else if (1..3).contains(&attempts)
            && last_attempt.elapsed() >= std::time::Duration::from_millis(1500)
        {
            if let Some(owner) = aokie_owner_for_call(&remote_media, &terminal_call_id) {
                eprintln!(
                    "[aokie-plugin] realtime fail-safe call is still active; retrying exact-owner hangup"
                );
                let result = remote_media.with_aokie_owner(&owner, || {
                    tracker.note_intent(crate::call_session::TerminationIntent::AgentHangup);
                    bt.hangup()
                });
                match result {
                    Ok(Ok(())) => {
                        ctx.agent_hung_up = true;
                        *realtime_terminal_call =
                            Some((terminal_call_id, Instant::now(), attempts + 1));
                    }
                    Ok(Err(error)) => {
                        eprintln!(
                            "[aokie-plugin] realtime fail-safe hangup retry failed: {error}"
                        );
                        *realtime_terminal_call =
                            Some((terminal_call_id, Instant::now(), attempts + 1));
                    }
                    Err(reason) => {
                        eprintln!(
                            "[aokie-plugin] realtime fail-safe hangup retry lost ownership: {reason}"
                        );
                        *realtime_terminal_call = None;
                        if ctx.desktop_realtime_responder {
                            *realtime_resume_call = Some(terminal_call_id);
                        } else {
                            *realtime_deferred_policy_failure = Some((
                                terminal_call_id,
                                "legacy policy responder remains unavailable".to_string(),
                            ));
                        }
                    }
                }
            }
        }
    }
}
