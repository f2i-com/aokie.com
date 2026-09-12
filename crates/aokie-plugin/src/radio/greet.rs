//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `play_tone_and_greet`.

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "voice"), allow(unused_variables))]
pub(super) fn play_tone_and_greet(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
    answer_tone: bool,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    tracker: &mut crate::call_session::SessionTracker,
    ctx: &mut CallVoiceContext,
    idle: &mut bool,
    #[cfg(feature = "voice")] control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    #[cfg(feature = "voice")] greeting: &mut Option<String>,
    #[cfg(feature = "voice")] realtime_selected: bool,
    #[cfg(feature = "voice")] synth: &crate::synth::SynthHandle,
    #[cfg(feature = "voice")] stt_tx: &std::sync::mpsc::Sender<SttWork>,
    #[cfg(feature = "voice")] probe_result_rx: &std::sync::mpsc::Receiver<SttResult>,
    #[cfg(feature = "voice")] stt_buf: &mut Vec<f32>,
    #[cfg(feature = "voice")] stt_had_speech: &mut bool,
    #[cfg(feature = "voice")] stt_silence: &mut std::time::Duration,
    #[cfg(feature = "voice")] agent_enabled: bool,
    #[cfg(feature = "voice")] greet_hold_started: &mut Option<std::time::Instant>,
    #[cfg(feature = "voice")] screened_hangup_pending_for: &mut Option<String>,
    #[cfg(feature = "voice")] pending_controls: &mut std::collections::VecDeque<RadioControl>,
    #[cfg(feature = "voice")] mute_stt_until: &mut Option<std::time::Instant>,
    #[cfg(feature = "voice")] barge_in: bool,
    #[cfg(feature = "voice")] screen_policy: &crate::screen::ScreenPolicy,
    #[cfg(feature = "voice")] promote_greet_for: &mut Option<String>,
    #[cfg(feature = "voice")] barge_rms: f32,
    #[cfg(feature = "voice")] aec: &mut Option<crate::aec::EchoCanceller>,
    #[cfg(feature = "voice")] protected_max_ms: u32,
    #[cfg(feature = "voice")] turn_overlapped: &mut bool,
    #[cfg(feature = "voice")] turn_overlap_at: &mut Option<String>,
    #[cfg(feature = "voice")] voice_call_gen: u64,
    #[cfg(feature = "voice")] realtime_legacy_call: &Option<String>,
    #[cfg(feature = "voice")] silence_window: std::time::Duration,
) {
    // Outbound agent calls start like a normal phone conversation: listen for
    // the recipient's hello, then let the reply engine deliver the introduction
    // using the call-scoped purpose. Do not inject an inbound greeting or tone.
    #[cfg(feature = "voice")]
    if agent_enabled && !realtime_selected {
        if tracker.current_mut().is_some_and(|call| call.begin_outbound_listening(bt.get_sample_rate() > 0)) {
            ctx.silence_timer = Some(SilenceTimer::new(silence_window, Instant::now()));
            eprintln!("[aokie-plugin] outbound ready — listening for the recipient before introducing the call");
        }
    }
    #[cfg(feature = "voice")]
    let realtime_blocks_answer_tone = realtime_owns_call(
        realtime_selected,
        realtime_legacy_call.as_deref(),
        tracker.call_id(),
    );
    #[cfg(not(feature = "voice"))]
    let realtime_blocks_answer_tone = false;
    #[cfg(feature = "voice")]
    let play_answer_tone = should_send_answer_tone(answer_tone, realtime_blocks_answer_tone);
    #[cfg(not(feature = "voice"))]
    let play_answer_tone = answer_tone && !realtime_blocks_answer_tone;
    if play_answer_tone {
        let sr = bt.get_sample_rate();
        if let Some(s) = tracker.current_mut() {
            if !s.toned && sr > 0 && !s.outbound {
                let tone = greeting_tone(sr);
                eprintln!(
                    "[aokie-plugin] answerTone: sending {} samples @ {}Hz to the caller",
                    tone.len(),
                    sr
                );
                if !remote_media.radio_reserved() {
                    remote_media.try_push_caller_output(&tone, sr as u32);
                    let _ = crate::backend::RadioBackend::send_audio(bt, &tone);
                }
                s.toned = true;
                *idle = false;
            }
        }
    }

    // Greet the caller with real TTS speech once the SCO audio channel is up
    // (voice build). Plays exactly once per call (the session's `greeted`
    // flag); per-call voice state RESETS live in the generation block above.
    let greet_now = {
        let sr = bt.get_sample_rate();
        #[cfg(feature = "voice")]
        let realtime_owns_current = realtime_owns_call(
            realtime_selected,
            realtime_legacy_call.as_deref(),
            tracker.call_id(),
        );
        #[cfg(not(feature = "voice"))]
        let realtime_owns_current = false;
        match tracker.current_mut() {
            // `is_active()` (answered) is REQUIRED, not just `sr > 0`:
            // some phones open the SCO channel during RINGING (in-band
            // ringtone) — the greeting must never speak into a line the
            // caller isn't connected to yet. OUTBOUND sessions greet only
            // when the AGENT placed the call (the opening line rides the
            // greeting slot via the call overlay); a handset-dialed call
            // we merely observe is never greeted — greeting into the
            // owner's own outgoing call was a latent bug this gate closes.
            Some(s)
                if !realtime_owns_current
                    && !s.greeted
                    && s.is_active()
                    && sr > 0
                    && (!s.outbound || s.agent_owned) =>
            {
                // The caller sometimes OPENS the conversation before the
                // greeting speaks (long ring window + personalize hold =
                // a beat of silence, so they say "hello?"/"yeah?"): the
                // agent's reply to that turn already opened the call, and
                // a greeting after it is a SECOND hello (live call
                // 2122425933: generic reply-greeting at turn 2, then the
                // late overlay's "Hi Lance!" greeting at turn 3). Any
                // assistant turn in this call's history means the
                // conversation is underway — mark greeted, never speak.
                #[cfg(feature = "voice")]
                let already_conversed = ctx.history.iter().any(|m| {
                    m.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                });
                #[cfg(not(feature = "voice"))]
                let already_conversed = false;
                if already_conversed {
                    s.greeted = true;
                    eprintln!(
                        "[aokie-plugin] greeting skipped — the agent already replied before it could speak (conversation underway)"
                    );
                    None
                } else {
                    // §9.3 personalization race: the caller-id flow's
                    // call-scoped overlay ("Hi <name>!") usually lands 1–3 s
                    // after answer — briefly hold the greeting for it instead
                    // of speaking the generic one a beat too early. Under the
                    // old GLOBAL settings.set design a lost race left the
                    // personalized greeting stuck in settings, so the NEXT
                    // call (or caller!) inherited it — the exact leak
                    // call-scoping removed; this hold is the leak-free way to
                    // win the race within the call it belongs to.
                    #[cfg(feature = "voice")]
                    let hold = {
                        let overlay_matches = ctx
                            .call_agent_overlay
                            .as_ref()
                            .is_some_and(|o| o.call_id == s.id);
                        let started = *greet_hold_started.get_or_insert_with(Instant::now);
                        // ⚠️ The post-answer SETTLE is deliberately NOT a
                        // hold here: an arm-level hold leaves the line in
                        // "listening" state, so the caller's pickup word
                        // during the silence becomes a normal first turn,
                        // the agent replies to it, and the already-conversed
                        // guard above then SKIPS the personalized greeting
                        // (live call e77457c6 2026-07-18). The settle rides
                        // the speak call as an audio EGRESS gate instead —
                        // the greeting takes the floor immediately and the
                        // caller's early words ride overlap capture.
                        // rejectPrivate needs to KNOW the id is absent, not
                        // merely late: this phone's CLCC id lands ~100ms
                        // post-answer, so give it a bounded window before
                        // declaring the number withheld.
                        let private_id_wait = screen_policy.reject_private
                            && s.caller_id.is_none()
                            && started.elapsed() < std::time::Duration::from_millis(1200);
                        // A manager-number call greets with the fixed manager
                        // line — the personalization overlay is irrelevant to
                        // it, so don't delay the greeting waiting for one.
                        let manager_line =
                            !s.outbound && screen_policy.is_manager(s.caller_id.as_deref());
                        private_id_wait
                            || (!manager_line
                                && hold_greeting_for_overlay(
                                    overlay_matches,
                                    s.caller_id.is_some(),
                                    started.elapsed(),
                                    GREETING_PERSONALIZE_HOLD,
                                ))
                    };
                    #[cfg(not(feature = "voice"))]
                    let hold = false;
                    if hold {
                        *idle = false;
                        None
                    } else {
                        s.greeted = true;
                        // Screening is an INBOUND policy: never screen the
                        // number WE dialed (a blocked-list hit or accept-
                        // pattern miss on our own outbound target would
                        // hang up our own call).
                        #[cfg(feature = "voice")]
                        let screened = if s.outbound {
                            None
                        } else {
                            screen_policy.verdict(s.caller_id.as_deref())
                        };
                        #[cfg(not(feature = "voice"))]
                        let screened: Option<&'static str> = None;
                        // Post-answer settle → audio EGRESS gate: the
                        // greeting (or screen message) starts synthesizing
                        // and holding the floor NOW, but its first frame
                        // reaches the SCO only once the carrier's answer
                        // transition has settled.
                        #[cfg(feature = "voice")]
                        let egress_gate =
                            greet_hold_started.map(|t| t + greeting_answer_settle());
                        #[cfg(not(feature = "voice"))]
                        let egress_gate: Option<
                            std::time::Instant,
                        > = None;
                        Some((s.id.clone(), sr, screened, egress_gate))
                    }
                }
            }
            _ => None,
        }
    };
    if let Some((corr, sr, screened, egress_gate)) = greet_now {
        #[cfg(not(feature = "voice"))]
        let _ = (&corr, sr, screened, egress_gate);
        #[cfg(feature = "voice")]
        if let Some(reason) = screened {
            let screened_owner = aokie_owner_for_call(&remote_media, &corr);
            // Screened call (spec Phase 0): no greeting, no agent — the
            // optional screen message, then hangup. Enforced HERE because
            // it is universally correct: phones that only deliver the id
            // post-answer (this Pixel) still get screened within ~1.5s.
            let screen_msg = screen_policy.message_for(reason);
            eprintln!(
                "[aokie-plugin] call screened ({reason}) — {}",
                if screen_msg.is_empty() {
                    "hanging up"
                } else {
                    "message + hangup"
                }
            );
            let screen_tts_ready = status.tts_error.lock().unwrap().is_none();
            if screened_call_needs_tts(screen_msg) && screen_tts_ready {
                let mut probe = ControlProbe::new(&control_rx, &mut *pending_controls);
                let _ = speak_planned(
                    bt,
                    &synth,
                    screen_msg,
                    sr,
                    None,
                    None,
                    Some(&mut probe),
                    &ctx.pace,
                    protected_max_ms,
                    None,
                    egress_gate,
                );
            } else {
                if screened_call_needs_tts(screen_msg) {
                    eprintln!(
                        "[aokie-plugin] screened-call message skipped because local TTS is unavailable"
                    );
                }
                // A blank message hangs up SILENTLY — but an AT+CHUP fired
                // the instant we answer is ignored by the phone (the call
                // hasn't stabilized), so a blank block used to leave the
                // caller connected to dead air (live report 2026-07-14).
                // The speak path above settles the call for ~1.5s; the
                // silent path needs the same brief settle before CHUP —
                // the agent-hangup drain uses the same bounded sleep.
                std::thread::sleep(Duration::from_millis(900));
            }
            if let Some(expected) = screened_owner.as_ref() {
                match remote_media.with_aokie_owner(expected, || {
                    tracker.note_intent(crate::call_session::TerminationIntent::AgentHangup);
                    bt.flush_tx_audio();
                    bt.hangup()
                }) {
                    Ok(Ok(())) => {
                        *screened_hangup_pending_for = None;
                        ctx.agent_hung_up = true;
                    }
                    Ok(Err(e)) => {
                        eprintln!("[aokie-plugin] screened-call hangup failed: {e}");
                        *screened_hangup_pending_for = None;
                        // Preserve the old ghost-turn latch after an
                        // attempted Aokie-owned CHUP.
                        ctx.agent_hung_up = true;
                    }
                    Err(reason) => {
                        eprintln!("[aokie-plugin] screened-call hangup deferred: {reason}");
                        *screened_hangup_pending_for = Some(corr.clone());
                    }
                }
            } else {
                eprintln!(
                    "[aokie-plugin] screened-call hangup deferred: exact Aokie caller ownership changed"
                );
                *screened_hangup_pending_for = Some(corr.clone());
            }
            let _ = &corr;
        } else {
            // Build the echo canceller once we know the negotiated SCO
            // rate (full-duplex only). Reused for every phrase this call.
            if barge_in && aec.is_none() {
                *aec = Some(crate::aec::EchoCanceller::new(sr as u32));
                eprintln!(
                    "[aokie-plugin] full-duplex barge-in ON (AEC @ {sr}Hz, rms>{barge_rms})"
                );
            }
            // §9.3: a call-scoped greeting (personalize-caller) wins for
            // ITS call; the configured global greeting is the fallback.
            let overlay_greeting = ctx
                .call_agent_overlay
                .as_ref()
                .filter(|o| o.call_id == corr)
                .and_then(|o| o.greeting.as_deref());
            // Phase 4: a caller promoted from hold hears "thanks for
            // holding, how can I help" instead of the cold-open greeting.
            let promoted = promote_greet_for.as_deref() == Some(corr.as_str());
            if promoted {
                *promote_greet_for = None;
            }
            // Phase 3 manager line: a manager-number caller is greeted AS
            // the manager line, beating the personalize overlay's customer
            // greeting (live call 085ce239: the restored Customers record
            // made the overlay greet the manager with 'Hi Lance! …').
            let manager_line = tracker
                .current()
                .filter(|s| s.id == corr && !s.outbound)
                .is_some_and(|s| screen_policy.is_manager(s.caller_id.as_deref()));
            let chosen_greeting: Option<&str> = if promoted {
                Some(HOLD_PROMOTED_GREET_LINE)
            } else if manager_line {
                Some(MANAGER_GREET_LINE)
            } else {
                overlay_greeting.or(greeting.as_deref())
            };
            if let Some(text) = chosen_greeting {
                // The caller often says "hello" OVER the greeting (both
                // parties greeting at once is normal telephony) — live
                // 2026-07-14 that energy-barged the greeting after ONE
                // word. Speak it as a protected span: ordinary overlap
                // rides the bounded budget; a spoken "wait"/"stop" or an
                // operator control still cuts instantly, and the overlap
                // is still captured for the scratchpad either way.
                let text = &format!("[[important]]{text}[[/important]]");
                // In barge-in mode the caller can talk over the greeting;
                // in half-duplex we mute STT for its playout instead.
                let (aec_ref, brms) = if barge_in {
                    (aec.as_mut(), Some(barge_rms))
                } else {
                    (None, None)
                };
                // AOK-CTRL-001: a hangup/reject arriving DURING the
                // greeting cuts it at chunk granularity. The greeting runs
                // through the same span planner as every other speech
                // origin (pacing + digit handling + the probe lane, so a
                // spoken "wait" cuts even the greeting).
                let mut probe = ControlProbe::new(&control_rx, &mut *pending_controls);
                let mut lane =
                    SttProbeLane::new(&stt_tx, &probe_result_rx, voice_call_gen, &status);
                lane.set_bot_context(text.to_string());
                let lane_ref = if barge_in { Some(&mut lane) } else { None };
                let speak_started = Instant::now();
                let planned = speak_planned(
                    bt,
                    &synth,
                    text,
                    sr,
                    aec_ref,
                    brms,
                    Some(&mut probe),
                    &ctx.pace,
                    protected_max_ms,
                    lane_ref,
                    egress_gate,
                );
                let out = planned.outcome;
                if !planned.text.trim().is_empty() {
                    note_tts_outcome(&status, &out);
                }
                if let Some(action) = probe.action.take() {
                    perform_cancel_action(action, bt, &mut *tracker, outbox, sink);
                }
                // A spoken floor command over the greeting = the caller
                // holds the floor from the very first words.
                if let Some(intent) = out.commanded {
                    ctx.dialogue.apply(intent);
                    eprintln!(
                        "[aokie-plugin] caller commanded {intent:?} over the greeting — holding the floor"
                    );
                }
                if barge_in {
                    if out.barged {
                        bt.flush_tx_audio();
                    }
                    *mute_stt_until = None;
                } else {
                    *mute_stt_until =
                        Some(Instant::now() + out.dur + Duration::from_millis(400));
                }
                // Phase 2 (live report 2026-07-14, call 5ba0ab2f): on an
                // agent-owned OUTBOUND call there is no ringtone phase —
                // anything captured between the remote pickup and the
                // opening line is the CALLEE's own words ("Hello?").
                // Keep it; the inbound clear below exists to drop
                // pre-answer ringtone garbage, which outbound never has.
                let pre_line: Vec<f32> = if *stt_had_speech
                    && tracker
                        .current()
                        .is_some_and(|s| s.outbound && s.agent_owned)
                {
                    std::mem::take(&mut *stt_buf)
                } else {
                    Vec::new()
                };
                stt_buf.clear();
                *stt_had_speech = false;
                *stt_silence = Duration::ZERO;
                // AK-008 + scratchpad: seed the caller's turn with EVERY
                // piece of echo-cancelled speech captured while we were
                // talking — barge or not — so words spoken over the
                // greeting are heard, never discarded.
                if !out.captured_speech.is_empty() {
                    *stt_buf = crate::voice::to_f32_16k(&out.captured_speech, sr as u32);
                    *stt_had_speech = true;
                    *turn_overlapped = true;
                    *turn_overlap_at = Some(overlap_backdate(
                        out.captured_speech.len(),
                        sr as usize,
                        speak_started.elapsed(),
                    ));
                }
                if !pre_line.is_empty() {
                    // The hello came BEFORE anything captured during the
                    // line — prepend, and back-date to its actual start
                    // (pre-line length + the opening line's duration ago).
                    let pre_ms = (pre_line.len() as u64 * 1000) / 16_000;
                    let mut seeded = pre_line;
                    seeded.extend_from_slice(&stt_buf);
                    *stt_buf = seeded;
                    *stt_had_speech = true;
                    *turn_overlapped = true;
                    *turn_overlap_at = Some(aokie_core::events::iso8601_ago_ms(
                        pre_ms + out.dur.as_millis() as u64,
                    ));
                }
                // Truthful transcript (audit AOK-VOICE-001/002): record the
                // greeting only when synthesis actually produced audio —
                // and only the spans that PLAYED.
                if out.dur > Duration::ZERO && !planned.played_text.is_empty() {
                    let delivery = if out.barged {
                        "interrupted"
                    } else if out.cancelled {
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
                            speak_started.elapsed().as_millis() as u64,
                        )),
                    );
                    ctx.turn_index += 1;
                    ctx.history.push(
                        serde_json::json!({ "role": "assistant", "content": planned.played_text }),
                    );
                    // Echo guard + replay compare against what was SENT —
                    // the flushed-but-echoed tail must still match (§6.3).
                    ctx.last_bot_reply = planned.sent_text.clone();
                    ctx.last_bot_speech = planned.sent_text;
                } else {
                    eprintln!(
                        "[aokie-plugin] greeting produced NO audio (TTS failed) — not recorded as a spoken turn"
                    );
                }
            }
            // AOK-CTRL-001: the conversation is live from here — start the
            // call-level max-silence watchdog (agent mode only; in flow
            // mode the host owns pacing).
            if agent_enabled {
                ctx.silence_timer = Some(SilenceTimer::new(silence_window, Instant::now()));
            }
        }
        *idle = false;
    }
}
