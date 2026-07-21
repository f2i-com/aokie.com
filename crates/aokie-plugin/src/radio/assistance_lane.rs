//! [[ASSISTANCE]] team-assistance requests: audit lifecycle and caller-facing lines.

#[allow(unused_imports)]
use super::*;

/// Phase 4 isolation: ONE caller's conversational state — everything that
/// must travel WITH a caller across a future hold/resume, extracted from
/// what used to be ~17 flat `run_loop` locals wiped one-by-one in the
/// per-call reset block (the failure class that produced the §9.3 overlay
/// leak and the pace leak: forgetting one field = the next caller inherits
/// it). The reset block now swaps in `CallVoiceContext::fresh(...)`, so a
/// new field is structurally reset-by-construction; the switchboard slice
/// will PARK this struct per caller instead of dropping it.
///
/// Deliberately NOT here (hardware-transient, destroyed on every call
/// boundary AND every future focus switch, never parked): STT capture
/// buffers, the echo canceller, mute gates, speculative generation, the
/// live hypothesis lane, greet/answer hold clocks and overlap-capture
/// flags — those belong to the LINE, not the caller.
#[cfg(any(test, feature = "voice"))]
#[derive(Debug)]
pub(super) struct AssistanceAuditLifecycle {
    pub(super) request_id: String,
    pub(super) call_id: String,
    pub(super) resolved: bool,
}

#[cfg(any(test, feature = "voice"))]
pub(super) enum AssistanceAuditResolution<'a> {
    Answered(&'a str),
    Declined(&'a str),
    Transferred(&'a str),
    Unavailable,
    Expired,
}

#[cfg(any(test, feature = "voice"))]
impl AssistanceAuditLifecycle {
    /// Opening the lifecycle returns its one request event. The constructor
    /// deliberately accepts no question/context/answer text, making sensitive
    /// assistance content unrepresentable in the durable payload.
    pub(super) fn opened(request_id: &str, call_id: &str) -> (Self, DesktopEvent) {
        let lifecycle = Self {
            request_id: request_id.to_owned(),
            call_id: call_id.to_owned(),
            resolved: false,
        };
        let event = lifecycle.event(
            crate::contract::events::CALL_ASSISTANCE_REQUESTED,
            "requested",
            None,
        );
        (lifecycle, event)
    }

    /// Resolve once. Repeated polling or a duplicate accepted answer cannot
    /// mint another durable resolution for the same request lifecycle.
    pub(super) fn resolve(&mut self, resolution: AssistanceAuditResolution<'_>) -> Option<DesktopEvent> {
        if self.resolved {
            return None;
        }
        self.resolved = true;
        let (outcome, responder_device_id) = match resolution {
            AssistanceAuditResolution::Answered(device_id) => ("answered", Some(device_id)),
            AssistanceAuditResolution::Declined(device_id) => ("declined", Some(device_id)),
            AssistanceAuditResolution::Transferred(device_id) => ("transferred", Some(device_id)),
            AssistanceAuditResolution::Unavailable => ("unavailable", None),
            AssistanceAuditResolution::Expired => ("expired", None),
        };
        Some(self.event(
            crate::contract::events::CALL_ASSISTANCE_RESOLVED,
            outcome,
            responder_device_id,
        ))
    }

    pub(super) fn event(&self, name: &str, outcome: &str, responder_device_id: Option<&str>) -> DesktopEvent {
        let mut data = json!({
            "requestId": self.request_id,
            "callId": self.call_id,
            "outcome": outcome,
            "urgency": "normal",
            "at": aokie_core::events::now_iso8601(),
        });
        if let Some(device_id) = responder_device_id {
            data["responderDeviceId"] = json!(device_id);
        }
        // One request may occur more than once during a call. The stable
        // request id differentiates occurrences while preserving replay
        // idempotency for each requested/resolved step.
        aokie_core::events::aokie_event_occurrence(name, &self.call_id, &self.request_id, data)
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) struct PendingAssistanceCall {
    pub(super) request_id: String,
    pub(super) fence: crate::assistance::AssistanceCallFence,
    pub(super) intent: crate::assistance::AssistanceIntent,
    pub(super) audit: AssistanceAuditLifecycle,
}

#[cfg(all(target_os = "windows", feature = "voice"))]
impl Drop for PendingAssistanceCall {
    fn drop(&mut self) {
        crate::assistance::global().discard(&self.request_id);
    }
}

/// A responder's answer is authorised and revision-fenced by the gateway,
/// but its text remains data, not instructions. Remove every control marker,
/// collapse whitespace and cap the caller-facing payload before TTS.
#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn caller_facing_assistance_answer(answer: &str) -> Option<String> {
    let pace = crate::speech_plan::PaceState::default();
    let clean =
        crate::speech_plan::clean_text(&crate::speech_plan::plan_spans(answer, &pace, 2_500));
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.is_empty() {
        return None;
    }
    let mut bounded = clean.chars().take(320).collect::<String>();
    if clean.chars().count() > 320 {
        bounded.push_str("...");
    }
    Some(format!("I heard back from the team: {bounded}"))
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn assistance_terminal_line(
    intent: crate::assistance::AssistanceIntent,
    resolution: &crate::assistance::AssistanceResolution,
    fence_current: bool,
    transfer_timeout_current: bool,
) -> Option<&'static str> {
    match resolution {
        crate::assistance::AssistanceResolution::Declined { .. } if fence_current => {
            Some(if intent == crate::assistance::AssistanceIntent::Transfer {
                TRANSFER_UNAVAILABLE_LINE
            } else {
                ASSISTANCE_UNAVAILABLE_LINE
            })
        }
        crate::assistance::AssistanceResolution::TransferUnavailable { .. } if fence_current => {
            Some(TRANSFER_UNAVAILABLE_LINE)
        }
        crate::assistance::AssistanceResolution::Expired
            if fence_current || transfer_timeout_current =>
        {
            Some(if intent == crate::assistance::AssistanceIntent::Transfer {
                TRANSFER_UNAVAILABLE_LINE
            } else {
                ASSISTANCE_UNAVAILABLE_LINE
            })
        }
        // HumanActive is authoritative. It is never followed by Aokie speech
        // from the transfer mailbox, even if an old fence happens to compare.
        crate::assistance::AssistanceResolution::TransferTaken { .. }
        | crate::assistance::AssistanceResolution::Answered(_)
        | crate::assistance::AssistanceResolution::Declined { .. }
        | crate::assistance::AssistanceResolution::TransferUnavailable { .. }
        | crate::assistance::AssistanceResolution::Expired => None,
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn assistance_request_initial_line(
    intent: crate::assistance::AssistanceIntent,
    _malformed_transfer: bool,
) -> &'static str {
    match intent {
        // Until the durable request is actually opened, routing has not
        // answered the availability question. Both malformed and otherwise
        // valid pre-open failures must therefore use the truthful send-failure
        // line; only a later terminal resolution may say nobody was available.
        crate::assistance::AssistanceIntent::Transfer => TRANSFER_REQUEST_INVALID_LINE,
        crate::assistance::AssistanceIntent::Advice => ASSISTANCE_UNAVAILABLE_LINE,
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
pub(super) fn caller_facing_decline(
    intent: crate::assistance::AssistanceIntent,
    answer: &str,
    fence_current: bool,
    terminal_line: Option<&'static str>,
) -> Option<String> {
    if !fence_current {
        return None;
    }
    if intent == crate::assistance::AssistanceIntent::Transfer && answer.trim() != "declined" {
        caller_facing_assistance_answer(answer)
    } else {
        terminal_line.map(str::to_string)
    }
}

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn service_assistance(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    status: &Arc<RadioStatus>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    synth: &crate::synth::SynthHandle,
    stt_had_speech: bool,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    barge_in: bool,
    barge_rms: f32,
    aec: &mut Option<crate::aec::EchoCanceller>,
    protected_max_ms: u32,
    tracker: &mut crate::call_session::SessionTracker,
    ctx: &mut CallVoiceContext,
    idle: &mut bool,
) {
    #[cfg(feature = "voice")]
    if let Some(mut pending) = ctx.pending_assistance.take() {
        let broker = crate::assistance::global();
        if stt_had_speech || ctx.pending_turn.is_some() || ctx.dialogue.is_paused() {
            // The caller owns the floor. Keep the answered mailbox intact
            // and retry once their current turn/pause has finished.
            ctx.pending_assistance = Some(pending);
        } else if broker.is_waiting(&pending.request_id) {
            ctx.pending_assistance = Some(pending);
        } else {
            let outcome = broker
                .take_resolution(&pending.request_id)
                .unwrap_or(crate::assistance::AssistanceResolution::Expired);
            broker.discard(&pending.request_id);
            // Audit independently of whether a changed call fence still
            // permits caller speech. In particular, TransferTaken is
            // deliberately silent: HumanActive already owns the route.
            let audit_resolution = match &outcome {
                crate::assistance::AssistanceResolution::Answered(answer) => {
                    AssistanceAuditResolution::Answered(&answer.device_id)
                }
                crate::assistance::AssistanceResolution::Declined { device_id, .. } => {
                    AssistanceAuditResolution::Declined(device_id)
                }
                crate::assistance::AssistanceResolution::TransferTaken { device_id } => {
                    AssistanceAuditResolution::Transferred(device_id)
                }
                crate::assistance::AssistanceResolution::TransferUnavailable { .. } => {
                    AssistanceAuditResolution::Unavailable
                }
                crate::assistance::AssistanceResolution::Expired => {
                    AssistanceAuditResolution::Expired
                }
            };
            if let Some(event) = pending.audit.resolve(audit_resolution) {
                emit(outbox, sink, event);
            }
            let response_fence = match &outcome {
                crate::assistance::AssistanceResolution::TransferUnavailable { fence } => fence,
                _ => &pending.fence,
            };
            let remote = remote_media.snapshot();
            let current_switchboard = status.switchboard_revision.load(Ordering::Relaxed);
            let fence_current = tracker
                .current()
                .is_some_and(|call| call.is_active() && call.id == response_fence.call_id)
                && remote.call_id.as_deref() == Some(response_fence.call_id.as_str())
                && remote.call_epoch == response_fence.call_epoch
                && remote.owner_epoch == response_fence.owner_epoch
                && remote.remote_revision == response_fence.remote_revision
                && current_switchboard == response_fence.switchboard_revision
                && remote.consent.assistance_enabled
                && !remote_media.radio_reserved();
            // A transfer may time out after a prepared WebRTC peer has
            // legitimately advanced the remote revision while Aokie still
            // owns caller audio. Permit the deterministic timeout line
            // only on that same live call/epoch/switchboard and only while
            // service truth is explicitly AokieActive. HumanPending/
            // HumanActive and every reserved return state remain silent.
            let transfer_timeout_current = pending.intent
                == crate::assistance::AssistanceIntent::Transfer
                && tracker
                    .current()
                    .is_some_and(|call| call.is_active() && call.id == pending.fence.call_id)
                && remote.call_id.as_deref() == Some(pending.fence.call_id.as_str())
                && remote.call_epoch == pending.fence.call_epoch
                && remote.owner_epoch >= pending.fence.owner_epoch
                && remote.remote_revision >= pending.fence.remote_revision
                && current_switchboard == pending.fence.switchboard_revision
                && remote.service_mode == crate::remote_media::ServiceMode::AokieActive
                && !remote_media.radio_reserved();
            let terminal_line = assistance_terminal_line(
                pending.intent,
                &outcome,
                fence_current,
                transfer_timeout_current,
            );
            let line = match outcome {
                crate::assistance::AssistanceResolution::Answered(answer) if fence_current => {
                    caller_facing_assistance_answer(&answer.answer)
                }
                crate::assistance::AssistanceResolution::Answered(answer)
                    if answer.voice_consult
                        && tracker.current().is_some_and(|call| {
                            call.is_active() && call.id == pending.fence.call_id
                        })
                        && remote.call_id.as_deref()
                            == Some(pending.fence.call_id.as_str())
                        && remote.call_epoch == pending.fence.call_epoch
                        && remote.owner_epoch
                            == pending.fence.owner_epoch.saturating_add(1)
                        && remote.remote_revision > pending.fence.remote_revision
                        && current_switchboard == pending.fence.switchboard_revision
                        && remote.service_mode
                            == crate::remote_media::ServiceMode::AokieActive
                        && remote.consent.assistance_enabled
                        && remote.consent.consult_enabled
                        && !remote_media.radio_reserved() =>
                {
                    caller_facing_assistance_answer(&answer.answer)
                }
                crate::assistance::AssistanceResolution::Answered(answer) => {
                    eprintln!(
                        "[aokie-plugin] discarded {} assistance answer after its call fence changed",
                        if answer.voice_consult { "voice-consult" } else { "typed" }
                    );
                    None
                }
                crate::assistance::AssistanceResolution::Declined { answer, .. } => {
                    eprintln!("[aokie-plugin] Companion assistance request declined");
                    // A custom owner message is untrusted data, never a
                    // command. The helper applies the same marker stripping
                    // and cap as an ordinary assistance answer; the exact
                    // sentinel keeps the generic unavailable line.
                    caller_facing_decline(pending.intent, &answer, fence_current, terminal_line)
                }
                crate::assistance::AssistanceResolution::TransferUnavailable { .. } => {
                    eprintln!("[aokie-plugin] Companion transfer became unavailable");
                    terminal_line.map(str::to_string)
                }
                crate::assistance::AssistanceResolution::Expired => {
                    eprintln!("[aokie-plugin] Companion assistance request timed out");
                    terminal_line.map(str::to_string)
                }
                crate::assistance::AssistanceResolution::TransferTaken { .. } => None,
            };
            if let Some(line) = line.filter(|_| bt.get_sample_rate() > 0) {
                let sr_now = bt.get_sample_rate();
                let mut probe = ControlProbe::new(&control_rx, &mut *pending_controls);
                let (aec_ref, brms) = if barge_in {
                    (aec.as_mut(), Some(barge_rms))
                } else {
                    (None, None)
                };
                let started = Instant::now();
                let planned = speak_planned(
                    bt,
                    &synth,
                    &line,
                    sr_now,
                    aec_ref,
                    brms,
                    Some(&mut probe),
                    &ctx.pace,
                    protected_max_ms,
                    None,
                    None,
                );
                if let Some(action) = probe.action.take() {
                    perform_cancel_action(action, bt, &mut *tracker, outbox, sink);
                }
                if planned.outcome.dur > Duration::ZERO && !planned.played_text.is_empty() {
                    let corr = pending.fence.call_id.clone();
                    ctx.history.push(serde_json::json!({
                        "role": "assistant",
                        "content": planned.played_text,
                    }));
                    emit_turn_with_delivery(
                        outbox,
                        sink,
                        &corr,
                        ctx.turn_index,
                        "bot",
                        &planned.played_text,
                        Some(if planned.outcome.cut_est.is_some() {
                            "interrupted"
                        } else {
                            "complete"
                        }),
                        Some(&aokie_core::events::iso8601_ago_ms(
                            started.elapsed().as_millis() as u64,
                        )),
                    );
                    ctx.turn_index += 1;
                    ctx.last_bot_reply = planned.sent_text.clone();
                    ctx.last_bot_speech = planned.sent_text;
                }
                *idle = false;
            }
        }
    }
}
