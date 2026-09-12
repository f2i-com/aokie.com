//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `service_controls`.

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
// `greeting` and friends are only mutated by the voice build's Configure arm.
#[cfg_attr(not(feature = "voice"), allow(unused_variables))]
pub(super) fn service_controls(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    status: &Arc<RadioStatus>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    greeting: &mut Option<String>,
    tracker: &mut crate::call_session::SessionTracker,
    pending_companion_end_caller: &mut Option<PendingCompanionEndCaller>,
    ctx: &mut CallVoiceContext,
    parked: &mut Option<(crate::call_session::CallSession, CallVoiceContext)>,
    pending_ctx_restore: &mut Option<CallVoiceContext>,
    promote_greet_for: &mut Option<String>,
    resume_line_for: &mut Option<(String, std::time::Instant)>,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    idle: &mut bool,
    #[cfg(feature = "voice")] synth: &crate::synth::SynthHandle,
    #[cfg(feature = "voice")] stt_tx: &std::sync::mpsc::Sender<SttWork>,
    #[cfg(feature = "voice")] stt_buf: &mut Vec<f32>,
    #[cfg(feature = "voice")] stt_had_speech: &mut bool,
    #[cfg(feature = "voice")] stt_silence: &mut std::time::Duration,
    #[cfg(feature = "voice")] agent_enabled: bool,
    #[cfg(feature = "voice")] agent_endpoint: &Arc<Mutex<Option<String>>>,
    #[cfg(feature = "voice")] agent_persona: &mut String,
    #[cfg(feature = "voice")] agent_model: &mut Option<String>,
    #[cfg(feature = "voice")] agent_client: &mut Option<crate::agent::LlmClient>,
    #[cfg(feature = "voice")] mute_stt_until: &mut Option<std::time::Instant>,
    #[cfg(feature = "voice")] send_audio: &mut bool,
    #[cfg(feature = "voice")] audio_transcript: &mut bool,
    #[cfg(feature = "voice")] screen_policy: &mut crate::screen::ScreenPolicy,
    #[cfg(feature = "voice")] agent_hangup: bool,
    #[cfg(feature = "voice")] aec: &mut Option<crate::aec::EchoCanceller>,
    #[cfg(feature = "voice")] protected_max_ms: u32,
    #[cfg(feature = "voice")] realtime_lane: &mut Option<RealtimeCallLane>,
    #[cfg(feature = "voice")] realtime_resume_call: &mut Option<String>,
) -> bool {
    use std::sync::mpsc::TryRecvError;
    status
        .loop_phase
        .store(loop_phase::CONTROLS, Ordering::Relaxed);
    loop {
        // Controls deferred by the mid-reply poll (audit AK-003) run first,
        // in arrival order, before anything newly queued.
        let next = match pending_controls.pop_front() {
            Some(c) => Ok(c),
            None => control_rx.try_recv(),
        };
        match next {
            Ok(RadioControl::Answer { op }) => {
                if let Err(e) = bt.answer_call() {
                    eprintln!("[aokie-plugin] radio answer failed: {e}");
                    // AOK-CTRL-001: the command result only said "accepted" —
                    // this is the authoritative failure record for it.
                    emit_control_failed(
                        outbox,
                        sink,
                        &tracker,
                        "call.answer",
                        op.as_deref(),
                        &e,
                    );
                }
            }
            Ok(RadioControl::Reject { op }) => {
                // Record WHY before the phone acts, so the eventual
                // CallTerminated reads outcome "rejected", never "missed"
                // (audit AK-001/AK-01).
                tracker.note_intent(crate::call_session::TerminationIntent::OperatorReject);
                if let Err(e) = bt.reject_call() {
                    eprintln!("[aokie-plugin] radio reject failed: {e}");
                    emit_control_failed(
                        outbox,
                        sink,
                        &tracker,
                        "call.reject",
                        op.as_deref(),
                        &e,
                    );
                }
            }
            Ok(RadioControl::Hangup { op }) => {
                tracker.note_intent(crate::call_session::TerminationIntent::OperatorHangup);
                if let Err(e) = bt.hangup() {
                    eprintln!("[aokie-plugin] radio hangup failed: {e}");
                    emit_control_failed(
                        outbox,
                        sink,
                        &tracker,
                        "call.hangup",
                        op.as_deref(),
                        &e,
                    );
                }
            }
            Ok(RadioControl::EndCallerFromCompanion { request, reply }) => {
                // Revalidation and AT+CHUP execute on the single radio
                // owner thread. Return/Revoke transitions and this
                // operation therefore have a deterministic order.
                perform_companion_end_caller(
                    request,
                    reply,
                    bt,
                    &mut *tracker,
                    status.as_ref(),
                    &remote_media,
                    &mut *pending_companion_end_caller,
                );
            }
            Ok(RadioControl::Dial {
                call_id,
                number,
                purpose,
                opening_line,
                op,
            }) => {
                // Phase 2: the connector enforced every guardrail; the
                // radio is authoritative for line state (a call may have
                // arrived between accept and here).
                if tracker.current().is_some() {
                    emit_control_failed(
                        outbox,
                        sink,
                        &tracker,
                        "call.dial",
                        op.as_deref(),
                        "a call arrived before the dial could start — outbound attempt dropped",
                    );
                } else if let Err(e) = bt.dial(number.clone()) {
                    eprintln!("[aokie-plugin] radio dial failed: {e}");
                    emit_control_failed(outbox, sink, &tracker, "call.dial", op.as_deref(), &e);
                } else {
                    eprintln!(
                        "[aokie-plugin] dialing OUT ({call_id}) — opening line ready, agent owns the call"
                    );
                    if let Some(s) = tracker.dial(
                        call_id.clone(),
                        Some(number.clone()),
                        aokie_core::events::now_iso8601(),
                        true,
                    ) {
                        *status.outbound_call_id.lock().unwrap() = Some(s.id.clone());
                        *status.current_caller.lock().unwrap() = Some(number.clone());
                        *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                        *status.call_started_at.lock().unwrap() =
                            Some(s.started_at_iso.clone());
                    }
                    // The in-flight dial context: lets the event mapper
                    // survive a stale terminate / late OutgoingDialing
                    // (see RadioStatus::pending_dial).
                    *status.pending_dial.lock().unwrap() = Some(PendingDial {
                        call_id: call_id.clone(),
                        number: number.clone(),
                        at: std::time::Instant::now(),
                    });
                    // Agent context rides the CALL-SCOPED overlay (§9.3
                    // machinery, wiped at the call boundary): the opening
                    // line IS this call's greeting, the persona gains the
                    // outbound block. A later personalize push for this
                    // call would replace it — acceptable; outbound calls
                    // never mint caller_id events, so none arrives.
                    #[cfg(feature = "voice")]
                    {
                        ctx.call_agent_overlay = Some(CallAgentOverlay {
                            call_id: call_id.clone(),
                            persona: Some(format!(
                                "{agent_persona}{}\nYour requested introduction, to use once in your first response: {opening_line}",
                                outbound_call_block(&number, purpose.as_deref())
                            )),
                            greeting: Some(opening_line.clone()),
                        });
                    }
                    #[cfg(not(feature = "voice"))]
                    let _ = (&purpose, &opening_line);
                    // The outbound lifecycle announcement — the rest
                    // (ringing/answered/ended) rides the existing family
                    // with this callId.
                    emit(
                        outbox,
                        sink,
                        aokie_core::events::aokie_event(
                            crate::contract::events::CALL_OUTBOUND_DIALING,
                            &call_id,
                            json!({
                                "callId": call_id,
                                "to": number,
                                "purpose": purpose,
                                "at": aokie_core::events::now_iso8601(),
                            }),
                        ),
                    );
                }
            }
            Ok(RadioControl::Activate { call_id, op }) => {
                // Phase 4 switchboard: make `call_id` the foreground.
                // The connector already validated against the mirrors;
                // the radio re-validates against its OWN state (events
                // drained this iteration may have changed the topology)
                // and owns the wire. Exactly ONE AT+CHLD=2 per accepted
                // request — a toggle, never blind-retried.
                use aokie_core::events::{aokie_event, now_iso8601};
                let waiting_leg = status.waiting_call.lock().unwrap().clone();
                let parked_leg = status.parked_call.lock().unwrap().clone();
                let switch_recent = status
                    .switch_in_flight
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() < std::time::Duration::from_secs(4));
                if remote_media.radio_reserved() {
                    emit_control_failed(
                        outbox,
                        sink,
                        &tracker,
                        "call.activate",
                        op.as_deref(),
                        "Companion remote ownership is pending/active; CHLD is blocked until Aokie owns the radio again",
                    );
                } else if switch_recent {
                    emit_control_failed(
                        outbox, sink, &tracker, "call.activate", op.as_deref(),
                        "a switch is already in progress — reconcile before retrying (CHLD=2 is a toggle)",
                    );
                } else if waiting_leg.as_ref().is_some_and(|w| w.call_id == call_id) {
                    // ── accept the WAITING caller: park the foreground ──
                    let w = waiting_leg.expect("checked above");
                    let waiting_switch = remote_media.aokie_switch_fence().filter(|fence| {
                        tracker.call_id() == Some(fence.owner.call_id.as_str())
                    });
                    if parked.is_some() {
                        emit_control_failed(
                            outbox, sink, &tracker, "call.activate", op.as_deref(),
                            "a caller is already parked — one parked caller max in this release",
                        );
                    } else if !tracker.current().is_some_and(|s| s.is_active()) {
                        emit_control_failed(
                            outbox,
                            sink,
                            &tracker,
                            "call.activate",
                            op.as_deref(),
                            "no ACTIVE foreground call to put on hold",
                        );
                    } else if waiting_switch.is_none() {
                        emit_control_failed(
                            outbox,
                            sink,
                            &tracker,
                            "call.activate",
                            op.as_deref(),
                            "a Companion media claim or physical call transition crossed this switch",
                        );
                    } else if let Err(e) = remote_media
                        .with_aokie_switch_owner(
                            waiting_switch
                                .as_ref()
                                .expect("waiting switch checked above"),
                            || {
                                #[cfg(feature = "voice")]
                                {
                                    if ctx.desktop_realtime_responder {
                                        if let Some(lane) = realtime_lane.take() {
                                            if let Some(item_id) =
                                                lane.output_pacer.active_item()
                                            {
                                                let _ = lane.session.cancel_output(
                                                    item_id,
                                                    lane.output_pacer
                                                        .audible_played_ms(Instant::now()),
                                                );
                                            }
                                            *realtime_resume_call = Some(lane.call_id.clone());
                                            lane.session
                                                .stop("physical call focus is switching");
                                        }
                                        status.realtime_ready.store(false, Ordering::Relaxed);
                                        *aec = None;
                                    }
                                }
                                *status.switch_in_flight.lock().unwrap() = Some((
                                    "accept_waiting".to_string(),
                                    std::time::Instant::now(),
                                ));
                                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                bt.flush_tx_audio();
                                bt.hold_swap()
                            },
                        )
                        .and_then(|result| result)
                    {
                        emit_control_failed(
                            outbox,
                            sink,
                            &tracker,
                            "call.activate",
                            op.as_deref(),
                            &e,
                        );
                    } else {
                        let sess_a = tracker.park().expect("checked active above");
                        let ctx_a = std::mem::replace(&mut *ctx, CallVoiceContext::fresh(None));
                        eprintln!(
                            "[aokie-plugin] SWITCHBOARD: parked {} — accepting waiting caller {} (AT+CHLD=2 sent)",
                            sess_a.id, w.call_id
                        );
                        *status.parked_call.lock().unwrap() = Some(SwitchboardLeg {
                            call_id: sess_a.id.clone(),
                            from: sess_a.caller_id.clone().unwrap_or_default(),
                            since_iso: now_iso8601(),
                        });
                        *parked = Some((sess_a, ctx_a));
                        {
                            // Id-guarded: only the accepted knock clears —
                            // a newer knock stays tracked.
                            let mut wl = status.waiting_call.lock().unwrap();
                            if wl.as_ref().is_some_and(|l| l.call_id == w.call_id) {
                                *wl = None;
                            }
                        }
                        // The minted waiting identity becomes a REAL call:
                        // lifecycle order incoming → caller_id → answered
                        // (AOK-LIF-001), then the normal machinery greets
                        // them and the caller_id event personalizes them.
                        tracker.ring(w.call_id.clone(), now_iso8601());
                        if !w.from.is_empty() {
                            tracker.caller_id(w.from.clone());
                        }
                        flush_incoming_if_pending(&mut *tracker, outbox, sink);
                        if !w.from.is_empty() {
                            emit(
                                outbox,
                                sink,
                                aokie_event(
                                    crate::contract::events::CALL_CALLER_ID,
                                    &w.call_id,
                                    json!({"callId": w.call_id, "from": w.from, "at": now_iso8601()}),
                                ),
                            );
                        }
                        tracker.answered();
                        status.call_active.store(true, Ordering::Relaxed);
                        *status.current_call_id.lock().unwrap() = Some(w.call_id.clone());
                        *status.current_caller.lock().unwrap() = if w.from.is_empty() {
                            None
                        } else {
                            Some(w.from.clone())
                        };
                        *status.call_started_at.lock().unwrap() =
                            tracker.current().map(|s| s.started_at_iso.clone());
                        emit(
                            outbox,
                            sink,
                            aokie_event(
                                crate::contract::events::CALL_ANSWERED,
                                &w.call_id,
                                json!({"at": now_iso8601()}),
                            ),
                        );
                        status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                    }
                } else if parked_leg.as_ref().is_some_and(|p| p.call_id == call_id) {
                    // ── swap back to / retrieve the PARKED caller ──
                    if waiting_leg.is_some() {
                        emit_control_failed(
                            outbox, sink, &tracker, "call.activate", op.as_deref(),
                            "a waiting caller is knocking — CHLD=2 would accept THEM; handle the knock first",
                        );
                    } else if let Some((sess_a, ctx_a)) = parked.take() {
                        let foreground_active =
                            tracker.current().is_some_and(|session| session.is_active());
                        let swap_result = if foreground_active {
                            remote_media
                                .aokie_switch_fence()
                                .filter(|fence| {
                                    tracker.call_id()
                                        == Some(fence.owner.call_id.as_str())
                                })
                                .ok_or_else(|| {
                                    "a Companion media claim or physical call transition crossed this switch"
                                        .to_string()
                                })
                                .and_then(|fence| {
                                    remote_media
                                        .with_aokie_switch_owner(&fence, || {
                                            #[cfg(feature = "voice")]
                                            {
                                                if ctx.desktop_realtime_responder {
                                                    if let Some(lane) = realtime_lane.take() {
                                                        if let Some(item_id) =
                                                            lane.output_pacer.active_item()
                                                        {
                                                            let _ = lane.session.cancel_output(
                                                                item_id,
                                                                lane.output_pacer
                                                                    .audible_played_ms(
                                                                        Instant::now(),
                                                                    ),
                                                            );
                                                        }
                                                        *realtime_resume_call =
                                                            Some(lane.call_id.clone());
                                                        lane.session.stop(
                                                            "physical call focus is switching",
                                                        );
                                                    }
                                                    status
                                                        .realtime_ready
                                                        .store(false, Ordering::Relaxed);
                                                    *aec = None;
                                                }
                                            }
                                            *status.switch_in_flight.lock().unwrap() = Some((
                                                "activate_parked".to_string(),
                                                std::time::Instant::now(),
                                            ));
                                            status
                                                .switchboard_revision
                                                .fetch_add(1, Ordering::Relaxed);
                                            bt.flush_tx_audio();
                                            bt.hold_swap()
                                        })
                                        .and_then(|result| result)
                                })
                        } else {
                            // With no foreground call there is no live
                            // claimant to race. Publish the transition
                            // fence before retrieving the held leg so a
                            // stale deferred relay claim cannot arm.
                            *status.switch_in_flight.lock().unwrap() = Some((
                                "activate_parked".to_string(),
                                std::time::Instant::now(),
                            ));
                            status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                            bt.flush_tx_audio();
                            bt.hold_swap()
                        };
                        if let Err(e) = swap_result {
                            *parked = Some((sess_a, ctx_a));
                            emit_control_failed(
                                outbox,
                                sink,
                                &tracker,
                                "call.activate",
                                op.as_deref(),
                                &e,
                            );
                        } else {
                            if tracker.current().is_some() {
                                let sess_b = tracker.park().expect("checked current");
                                let ctx_b =
                                    std::mem::replace(&mut *ctx, CallVoiceContext::fresh(None));
                                eprintln!(
                                    "[aokie-plugin] SWITCHBOARD: swap — {} parked, resuming {}",
                                    sess_b.id, sess_a.id
                                );
                                *status.parked_call.lock().unwrap() = Some(SwitchboardLeg {
                                    call_id: sess_b.id.clone(),
                                    from: sess_b.caller_id.clone().unwrap_or_default(),
                                    since_iso: now_iso8601(),
                                });
                                *parked = Some((sess_b, ctx_b));
                            } else {
                                eprintln!(
                                    "[aokie-plugin] SWITCHBOARD: retrieving parked caller {}",
                                    sess_a.id
                                );
                                *status.parked_call.lock().unwrap() = None;
                            }
                            let resumed_id = sess_a.id.clone();
                            let resumed_from = sess_a.caller_id.clone();
                            let was_greeted = sess_a.greeted;
                            match tracker.restore(sess_a) {
                                Ok(_generation) => {
                                    *pending_ctx_restore = Some(ctx_a);
                                    // User feedback (first supervised test):
                                    // a swap must never be SILENT — greet a
                                    // never-greeted caller, welcome everyone
                                    // else back.
                                    if !was_greeted {
                                        *promote_greet_for = Some(resumed_id.clone());
                                    } else {
                                        *resume_line_for = Some((
                                            resumed_id.clone(),
                                            std::time::Instant::now(),
                                        ));
                                    }
                                    status.call_active.store(true, Ordering::Relaxed);
                                    *status.current_call_id.lock().unwrap() =
                                        Some(resumed_id.clone());
                                    *status.current_caller.lock().unwrap() = resumed_from;
                                    *status.call_started_at.lock().unwrap() =
                                        tracker.current().map(|s| s.started_at_iso.clone());
                                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                    eprintln!(
                                        "[aokie-plugin] SWITCHBOARD: {} resumed — their conversation context is restored",
                                        resumed_id
                                    );
                                }
                                Err(sess_back) => {
                                    // Cannot happen (we just parked/checked
                                    // idle) — keep the caller parked rather
                                    // than lose them.
                                    eprintln!(
                                        "[aokie-plugin] SWITCHBOARD: restore refused unexpectedly — {} stays parked",
                                        sess_back.id
                                    );
                                    *parked = Some((sess_back, ctx_a));
                                }
                            }
                        }
                    }
                } else {
                    emit_control_failed(
                        outbox, sink, &tracker, "call.activate", op.as_deref(),
                        &format!(
                            "{call_id} is not the waiting or parked caller — stale switchboard view"
                        ),
                    );
                }
            }
            Ok(RadioControl::SendSms {
                message_id,
                to,
                body,
            }) => {
                if let Err(e) = bt.send_sms(message_id.clone(), to.clone(), body, None) {
                    emit(
                        outbox,
                        sink,
                        aokie_core::events::aokie_event(
                            crate::contract::events::SMS_FAILED,
                            &message_id,
                            json!({
                                "messageId": message_id.clone(),
                                "to": to.clone(),
                                "reason": e.clone(),
                                "at": aokie_core::events::now_iso8601(),
                            }),
                        ),
                    );
                    emit(
                        outbox,
                        sink,
                        aokie_core::events::aokie_event_occurrence(
                            crate::contract::events::HARDWARE_ERROR,
                            "radio",
                            &aokie_core::events::occurrence_id(),
                            json!({"message": format!("send_sms failed: {e}")}),
                        ),
                    );
                }
            }
            Ok(RadioControl::Speak { text, op }) => {
                #[cfg(not(feature = "voice"))]
                let _ = &op;
                #[cfg(feature = "voice")]
                if agent_enabled {
                    // Belt-and-suspenders: the connector now REFUSES
                    // operatorSpeak while the agent owns replies
                    // (AOK-CTRL-001), so this only catches a request that
                    // raced a radio restart. Never spoken (the caller must
                    // not be answered twice) — and never silently either:
                    // the accepted command gets its authoritative failure.
                    eprintln!(
                        "[aokie-plugin] dropping operatorSpeak (agent owns replies): {}",
                        content_for_log(&text)
                    );
                    emit(
                        outbox,
                        sink,
                        aokie_core::events::aokie_event_occurrence(
                            crate::contract::events::HARDWARE_ERROR,
                            tracker.call_id().unwrap_or("radio"),
                            &aokie_core::events::occurrence_id(),
                            json!({
                                "message": "call.operatorSpeak was dropped: the in-plugin AI receptionist owns replies on this install",
                                "code": "speak_failed",
                                "action": "call.operatorSpeak",
                                "operationId": op,
                            }),
                        ),
                    );
                } else {
                    let sr = bt.get_sample_rate();
                    // AOK-CTRL-001: a hangup/reject queued behind this speak
                    // cuts it at chunk granularity. Flow/operator speech runs
                    // through the SAME span planner as the built-in agent —
                    // markers ([[slow]]/[[rate=…]]/[[important]]) are
                    // validated + clamped identically, digit runs slow down
                    // identically: the coordinator doesn't care where the
                    // words came from.
                    let mut probe = ControlProbe::new(&control_rx, &mut *pending_controls);
                    let op_started = Instant::now();
                    let planned = speak_planned(
                        bt,
                        &synth,
                        &text,
                        sr,
                        None,
                        None,
                        Some(&mut probe),
                        &ctx.pace,
                        protected_max_ms,
                        None,
                        None,
                    );
                    let out = planned.outcome;
                    if sr > 0 && !planned.text.trim().is_empty() {
                        note_tts_outcome(&status, &out);
                    }
                    *mute_stt_until =
                        Some(Instant::now() + out.dur + Duration::from_millis(400));
                    stt_buf.clear();
                    *stt_had_speech = false;
                    *stt_silence = Duration::ZERO;
                    // Truthful transcript (audit AOK-VOICE-002): record a
                    // bot turn ONLY when synthesis actually produced audio
                    // for the caller — and only the spans that played.
                    if out.dur > Duration::ZERO && !planned.played_text.is_empty() {
                        if let Some(corr) = tracker.call_id().map(str::to_string) {
                            let delivery = if out.cancelled {
                                "operator_ended"
                            } else {
                                "complete"
                            };
                            emit_turn_with_delivery(
                                outbox,
                                sink,
                                &corr,
                                ctx.turn_index,
                                "bot",
                                &planned.played_text,
                                Some(delivery),
                                Some(&aokie_core::events::iso8601_ago_ms(
                                    op_started.elapsed().as_millis() as u64,
                                )),
                            );
                            ctx.turn_index += 1;
                        }
                        // Spoken audio is conversational activity.
                        if let Some(t) = ctx.silence_timer.as_mut() {
                            t.note_activity(Instant::now());
                        }
                    } else if !out.cancelled && sr > 0 && !planned.text.trim().is_empty() {
                        eprintln!(
                            "[aokie-plugin] operatorSpeak produced NO audio (TTS failed) — not recorded as a spoken turn: {}",
                            content_for_log(&text)
                        );
                        // AOK-CTRL-001: the accepted command's authoritative
                        // failure — the text was NOT spoken to the caller.
                        emit(
                            outbox,
                            sink,
                            aokie_core::events::aokie_event_occurrence(
                                crate::contract::events::HARDWARE_ERROR,
                                tracker.call_id().unwrap_or("radio"),
                                &aokie_core::events::occurrence_id(),
                                json!({
                                    "message": "call.operatorSpeak produced no audio (TTS failure) — the text was NOT spoken to the caller",
                                    "code": "speak_failed",
                                    "action": "call.operatorSpeak",
                                    "operationId": op,
                                }),
                            ),
                        );
                    }
                    if let Some(action) = probe.action.take() {
                        perform_cancel_action(action, bt, &mut *tracker, outbox, sink);
                    }
                }
                #[cfg(not(feature = "voice"))]
                eprintln!(
                    "[aokie-plugin] operatorSpeak ({} chars) â€” voice feature not built",
                    text.chars().count()
                );
            }
            Ok(RadioControl::ConfigureCallAgent {
                call_id,
                persona,
                greeting,
            }) => {
                #[cfg(feature = "voice")]
                {
                    // Apply only to the CURRENT call — a push that raced
                    // the call's end dies here instead of configuring the
                    // next caller (the point of call-scoped config).
                    if tracker.current().is_some_and(|s| s.id == call_id) {
                        let persona = persona.filter(|p| !p.trim().is_empty());
                        let greeting = greeting.filter(|s| !s.trim().is_empty());
                        eprintln!(
                            "[aokie-plugin] call-scoped agent config for {call_id} (persona {}, greeting {})",
                            persona.is_some(),
                            greeting.is_some()
                        );
                        // The overlay persona REPLACES the prefix the
                        // ring warm primed — re-warm so the first reply
                        // still hits the prompt cache.
                        if agent_enabled {
                            if let Some(p) = persona.as_deref() {
                                spawn_llm_prefix_warm(
                                    &agent_endpoint,
                                    agent_model.clone(),
                                    &status,
                                    compose_agent_system_prompt(p, agent_hangup, None, false),
                                    ctx.history.clone(),
                                    "overlay",
                                    None,
                                );
                            }
                        }
                        ctx.call_agent_overlay = Some(CallAgentOverlay {
                            call_id,
                            persona,
                            greeting,
                        });
                    } else {
                        eprintln!(
                            "[aokie-plugin] call-scoped agent config DROPPED — {call_id} is not the current call"
                        );
                    }
                }
                #[cfg(not(feature = "voice"))]
                let _ = (call_id, persona, greeting);
            }
            Ok(RadioControl::ReloadScreening) => {
                // Rebuild from env (the connector set the vars first): a
                // block/unblock applies to the NEXT call, no reconnect.
                #[cfg(feature = "voice")]
                {
                    *screen_policy = crate::screen::ScreenPolicy::from_env();
                    eprintln!(
                        "[aokie-plugin] call-screening policy reloaded (active: {})",
                        screen_policy.is_active()
                    );
                }
            }
            Ok(RadioControl::Configure {
                persona,
                greeting: g,
                voice,
                model,
                endpoint,
                stt_endpoint,
                tts_endpoint,
                reload_tts_engine,
            }) => {
                // Live-reconfigure the agent from a flow / settings.set push. Each
                // field is Some only when it changed. Greeting applies to the NEXT
                // call; persona/voice/model take effect on the next caller turn.
                #[cfg(feature = "voice")]
                {
                    // The Desktop's two reserved ChatGPT/Codex adapters
                    // accept text only. An endpoint can change while the
                    // radio is live, so disable an already-armed audio
                    // attachment lane before adopting that endpoint. The
                    // connector separately normalizes persisted
                    // `sendAudio=false`; this is the in-flight fail-safe.
                    if matches!(
                        &endpoint,
                        EndpointUpdate::Set(value)
                            if crate::connector::is_codex_live_call_endpoint(value)
                    ) {
                        if *send_audio {
                            eprintln!(
                                "[aokie-plugin] sendAudio disabled: ChatGPT via Codex live-call adapters are text-only"
                            );
                        }
                        *send_audio = false;
                        if *audio_transcript
                            && std::env::var("AOKIE_AUDIO_TRANSCRIPT_ENDPOINT")
                                .ok()
                                .is_none_or(|value| value.trim().is_empty())
                        {
                            eprintln!(
                                "[aokie-plugin] audioTranscript disabled: ChatGPT via Codex is text-only and no separate correction endpoint is configured"
                            );
                            *audio_transcript = false;
                        }
                    }
                    if let Some(g) = g {
                        // Blank = default, never silence (see DEFAULT_GREETING).
                        *greeting = if g.trim().is_empty() {
                            Some(DEFAULT_GREETING.to_string())
                        } else {
                            Some(g)
                        };
                    }
                    if let Some(p) = persona {
                        if !p.trim().is_empty() {
                            *agent_persona = p;
                        }
                    }
                    if let Some(v) = voice {
                        std::env::set_var("AOKIE_TTS_VOICE", v.trim());
                    }
                    let mut client_stale = false;
                    if let Some(m) = model {
                        let m = m.trim().to_string();
                        let new = if m.is_empty() { None } else { Some(m) };
                        if new != *agent_model {
                            *agent_model = new;
                            client_stale = true;
                        }
                    }
                    if !matches!(endpoint, EndpointUpdate::Unchanged) {
                        let new = match endpoint {
                            EndpointUpdate::Unchanged => unreachable!(),
                            EndpointUpdate::Clear => None,
                            EndpointUpdate::Set(e) => normalize_endpoint(Some(e)),
                        };
                        let mut current = agent_endpoint.lock().unwrap();
                        if new != *current {
                            *current = new;
                            client_stale = true;
                        }
                    }
                    if client_stale {
                        // Force a reconnect with the new endpoint/model next turn.
                        *agent_client = None;
                    }
                    match stt_endpoint {
                        EndpointUpdate::Unchanged => {}
                        EndpointUpdate::Clear => {
                            let _ = stt_tx.send(SttWork::Configure { endpoint: None });
                        }
                        EndpointUpdate::Set(e) => {
                            let _ = stt_tx.send(SttWork::Configure {
                                endpoint: normalize_endpoint(Some(e)),
                            });
                        }
                    }
                    match tts_endpoint {
                        EndpointUpdate::Unchanged => {}
                        EndpointUpdate::Clear => synth.configure(None),
                        EndpointUpdate::Set(e) => synth.configure(normalize_endpoint(Some(e))),
                    }
                    if reload_tts_engine {
                        // Env already re-stamped by the connector; the
                        // worker reloads eagerly so a bad ttsModelDir
                        // shows up in the log now, not on the next call.
                        synth.reload_engine();
                    }
                    eprintln!("[aokie-plugin] agent reconfigured (persona/greeting/voice/model/endpoints)");
                }
                #[cfg(not(feature = "voice"))]
                {
                    let _ = (
                        persona,
                        g,
                        voice,
                        model,
                        endpoint,
                        stt_endpoint,
                        tts_endpoint,
                        reload_tts_engine,
                    );
                }
            }
            Ok(RadioControl::StartPairing { seconds }) => {
                // AOK-BT-001: make the radio discoverable for a bounded window.
                bt.open_pairing_window(seconds);
                eprintln!("[aokie-plugin] pairing window opened for {seconds}s");
            }
            Ok(RadioControl::StopPairing) => {
                bt.close_pairing_window();
                eprintln!("[aokie-plugin] pairing window closed");
            }
            Ok(RadioControl::RemovePaired { address, reply }) => {
                let _ = reply.send(bt.remove_paired(&address));
            }
            Ok(RadioControl::Disconnect { address, reply }) => {
                let _ = reply.send(bt.disconnect(&address));
            }
            Ok(RadioControl::Connect { address, reply }) => {
                let _ = reply.send(bt.connect(&address));
            }
            Ok(RadioControl::ConfirmPairing {
                address,
                accept,
                reply,
            }) => {
                let _ = reply.send(bt.confirm_pairing(&address, accept));
            }
            Ok(RadioControl::ListBonded { reply }) => {
                let _ = reply.send(bt.bonded_devices());
            }
            Ok(RadioControl::ConnectedName { reply }) => {
                let _ = reply.send(bt.connected_name());
            }
            Ok(RadioControl::Shutdown { completion }) => {
                // `call.ended` may already be visible while one of its
                // detached transcript corrections is still pending. A
                // graceful stop must not strand the one-shot barrier that
                // owns summary/after-call automation. Mark every such
                // call timed out, synchronously outbox/emit it, and only
                // then let the main plugin acknowledge shutdown.
                if emit_forced_transcript_settlements(outbox, sink) {
                    if let Some(completion) = completion {
                        let _ = completion.send(());
                    }
                    return false;
                }
                // Keep both the settlement and the shutdown waiter alive
                // and retry on the next pass. The caller times out rather
                // than receiving a false graceful-shutdown acknowledgement
                // if durable storage stays unavailable.
                pending_controls.push_front(RadioControl::Shutdown { completion });
                break;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                // All handles disappeared (host/process teardown without
                // an explicit shutdown request). Best-effort synchronous
                // drain while stdout/outbox are still alive.
                if emit_forced_transcript_settlements(outbox, sink) {
                    return false;
                }
                break;
            }
        }
        *idle = false;
    }    true
}
