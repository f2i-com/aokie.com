//! Extracted from the radio conversation engine (carve-up 2026-07-21).

#[allow(unused_imports)]
use super::*;

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn process_flushed_turn(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    status: &Arc<RadioStatus>,
    host_rpc: &Arc<crate::host_rpc::HostRpc>,
    data_dir: &std::path::Path,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    synth: &crate::synth::SynthHandle,
    stt_tx: &std::sync::mpsc::Sender<SttWork>,
    probe_result_rx: &std::sync::mpsc::Receiver<SttResult>,
    stt_buf: &mut Vec<f32>,
    stt_had_speech: &mut bool,
    stt_silence: &mut std::time::Duration,
    agent_enabled: bool,
    agent_endpoint: &Arc<Mutex<Option<String>>>,
    agent_persona: &String,
    agent_model: &Option<String>,
    agent_client: &mut Option<crate::agent::LlmClient>,
    spec_reply: &mut Option<ReplyStream>,
    pending_agent_client: &Arc<Mutex<Option<crate::agent::LlmClient>>>,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    mute_stt_until: &mut Option<std::time::Instant>,
    barge_in: bool,
    send_audio: bool,
    audio_transcript: bool,
    audio_capture: bool,
    heard_tx: &std::sync::mpsc::Sender<TranscriptCorrectionResult>,
    transcript_client_cache: &std::sync::Arc<std::sync::OnceLock<Option<crate::agent::LlmClient>>>,
    screen_policy: &mut crate::screen::ScreenPolicy,
    agent_hangup: bool,
    barge_rms: f32,
    aec: &mut Option<crate::aec::EchoCanceller>,
    protected_max_ms: u32,
    turn_overlapped: &mut bool,
    turn_overlap_at: &mut Option<String>,
    tracker: &mut crate::call_session::SessionTracker,
    pending_companion_end_caller: &mut Option<PendingCompanionEndCaller>,
    voice_call_gen: u64,
    ctx: &mut CallVoiceContext,
    silence_window: std::time::Duration,
    idle: &mut bool,
) {
    status.loop_phase.store(loop_phase::TURN, Ordering::Relaxed);
    let flushed_turn = match ctx.pending_turn.as_ref() {
        Some(p) if Instant::now() >= p.flush_at && !*stt_had_speech => {
            ctx.pending_turn.take().map(|p| (p.corr, p.text, p.audio))
        }
        _ => None,
    };
    if let Some((corr, text, turn_audio)) = flushed_turn {
        *idle = false;
        // The flushed turn's PAIRED audio is what both the reply
        // attach and the transcript correction must use.
        ctx.last_turn_audio = turn_audio;
        'turn_done: {
            // ── Phase 3 PIN gate ─────────────────────────────────
            // The utterance IS the PIN attempt: it must never reach
            // the transcript, the model's history, the captions or
            // any reply generation. Verified by deterministic digit
            // comparison — the model never judges a PIN.
            // Bare-PIN fast path (live call 88a20001): a manager saying
            // just the PIN unprompted — typically right after the
            // greeting invites it — verifies immediately through the
            // SAME judge path as the prompted gate (redacted turn,
            // throttled attempt), instead of the digits reaching the
            // LLM as conversation.
            let bare_pin_turn = !ctx.manager_gate.awaiting_pin
                && !ctx.manager_gate.verified
                && agent_enabled
                && tracker.current().is_some_and(|s| {
                    !s.outbound && screen_policy.is_manager(s.caller_id.as_deref())
                })
                && looks_like_bare_pin(
                    &text,
                    crate::speech_plan::spoken_digits(
                        &std::env::var("AOKIE_MANAGER_PIN").unwrap_or_default(),
                    )
                    .len(),
                );
            if ctx.manager_gate.awaiting_pin || bare_pin_turn {
                emit_turn_full(
                    outbox,
                    sink,
                    &corr,
                    ctx.turn_index,
                    "caller",
                    "[manager PIN redacted]",
                    None,
                    Some("control"),
                    false,
                    None,
                );
                status
                    .last_caller_turn
                    .store(ctx.turn_index, Ordering::Relaxed);
                ctx.turn_index += 1;
                *turn_overlapped = false;
                *turn_overlap_at = None;
                let heard = crate::speech_plan::spoken_digits(&text);
                let expected = crate::speech_plan::spoken_digits(
                    &std::env::var("AOKIE_MANAGER_PIN").unwrap_or_default(),
                );
                // A PIN said digit by digit splits across STT turns —
                // collect partial fragments (still redacted, above)
                // and only judge a full-length attempt.
                let given = match pin_gate_step(
                    &mut ctx.manager_gate.pin_digits,
                    &heard,
                    expected.len(),
                ) {
                    PinStep::Collect => {
                        eprintln!(
                            "[aokie-plugin] manager PIN: partial digits heard — waiting for the rest"
                        );
                        break 'turn_done;
                    }
                    PinStep::Judge(given) => given,
                };
                ctx.manager_gate.awaiting_pin = false;
                let auth = crate::manager_auth::verify(data_dir, &expected, &given);
                if auth == crate::manager_auth::Decision::Verified {
                    ctx.manager_gate.verified = true;
                    ctx.manager_gate.attempts = 0;
                    eprintln!("[aokie-plugin] manager PIN verified");
                    if let Some(req) = ctx.manager_gate.pending.take() {
                        let manager_owner = aokie_owner_for_call(&remote_media, &corr);
                        if let Some(manager_owner) = manager_owner {
                            speak_manager_line(
                                bt,
                                &synth,
                                outbox,
                                sink,
                                &status,
                                &corr,
                                &mut ctx.turn_index,
                                &mut ctx.history,
                                MANAGER_ACTION_FILLER,
                            );
                            let mgr_from = tracker
                                .current()
                                .and_then(|s| s.caller_id.clone())
                                .unwrap_or_default();
                            if let Some(outcome) = manager_plan_and_execute(
                                &host_rpc,
                                sink,
                                outbox,
                                &mut *screen_policy,
                                &status,
                                &remote_media,
                                &manager_owner,
                                &corr,
                                &mgr_from,
                                &req,
                            ) {
                                speak_manager_line(
                                    bt,
                                    &synth,
                                    outbox,
                                    sink,
                                    &status,
                                    &corr,
                                    &mut ctx.turn_index,
                                    &mut ctx.history,
                                    &outcome,
                                );
                            }
                        } else {
                            eprintln!(
                                "[aokie-plugin] manager action skipped: exact Aokie caller ownership changed"
                            );
                        }
                    } else {
                        speak_manager_line(
                            bt,
                            &synth,
                            outbox,
                            sink,
                            &status,
                            &corr,
                            &mut ctx.turn_index,
                            &mut ctx.history,
                            PIN_OK_NOACTION_LINE,
                        );
                    }
                } else {
                    ctx.manager_gate.attempts += 1;
                    if matches!(auth, crate::manager_auth::Decision::Locked { .. }) {
                        ctx.manager_gate.pending = None;
                        speak_manager_line(
                            bt,
                            &synth,
                            outbox,
                            sink,
                            &status,
                            &corr,
                            &mut ctx.turn_index,
                            &mut ctx.history,
                            PIN_LOCKED_LINE,
                        );
                    } else if ctx.manager_gate.attempts < 2 && !expected.is_empty() {
                        ctx.manager_gate.awaiting_pin = true;
                        speak_manager_line(
                            bt,
                            &synth,
                            outbox,
                            sink,
                            &status,
                            &corr,
                            &mut ctx.turn_index,
                            &mut ctx.history,
                            PIN_RETRY_LINE,
                        );
                    } else {
                        ctx.manager_gate.pending = None;
                        speak_manager_line(
                            bt,
                            &synth,
                            outbox,
                            sink,
                            &status,
                            &corr,
                            &mut ctx.turn_index,
                            &mut ctx.history,
                            PIN_FAIL_LINE,
                        );
                    }
                }
                break 'turn_done;
            }
            // Duplex floor coordination: parse the caller's words for a
            // deterministic FLOOR COMMAND before any model runs. "Wait"
            // / "stop" / "let me think" produce INTENTIONAL SILENCE
            // (the most human response is sometimes nothing at all);
            // pace commands and replays are handled without burning a
            // generation. Ambiguity always reads as Content.
            let intent = if agent_enabled {
                crate::duplex::parse_caller_intent(&text)
            } else {
                crate::duplex::CallerIntent::Content
            };
            let is_control = intent != crate::duplex::CallerIntent::Content;
            eprintln!(
                "[aokie-plugin] heard [turn {}]{}: {}",
                ctx.turn_index,
                if is_control {
                    format!(" (control: {intent:?})")
                } else {
                    String::new()
                },
                content_for_log(&text)
            );
            // Control turns are recorded truthfully but tagged, so
            // flows/business logic can skip them.
            emit_turn_full(
                outbox,
                sink,
                &corr,
                ctx.turn_index,
                "caller",
                &text,
                None,
                if is_control { Some("control") } else { None },
                *turn_overlapped,
                turn_overlap_at.as_deref(),
            );
            // §9.2: this is now the newest caller turn — a flow reply
            // naming an older one is stale (typed refusal upstream).
            status
                .last_caller_turn
                .store(ctx.turn_index, Ordering::Relaxed);
            if let Some(lane) = ctx.rt_lane.as_mut() {
                lane.turn_final();
                if let Some(line) = lane.phase("thinking", Instant::now()) {
                    let _ = sink.send_line(&line);
                }
            }
            // audioTranscript is detached: the live reply never waits
            // for it. The shared helper also covers turns flushed by
            // call termination, keeping settlement registration and
            // PIN exclusion identical on every path.
            let setting = ctx
                .call_agent_overlay
                .as_ref()
                .and_then(|overlay| overlay.persona.as_deref())
                .unwrap_or(&agent_persona);
            maybe_spawn_transcript_correction(
                audio_transcript,
                &corr,
                ctx.turn_index,
                &text,
                &ctx.last_turn_audio,
                ctx.prev_heard.as_ref(),
                &ctx.history,
                setting,
                agent_client
                    .clone()
                    .or_else(|| pending_agent_client.lock().unwrap().clone()),
                &transcript_client_cache,
                &heard_tx,
            );
            // Every caller turn with audio becomes the next turn's
            // continuity context (hesitations included — their audio
            // is real), tracked AFTER the spawn read the previous one.
            // Capture-gated (not sendAudio): the only consumer is the
            // correction lane's split-utterance continuity prepend.
            if audio_capture && !ctx.last_turn_audio.is_empty() {
                ctx.prev_heard =
                    Some((ctx.last_turn_audio.clone(), text.clone(), Instant::now()));
            }
            *turn_overlapped = false;
            *turn_overlap_at = None;
            ctx.turn_index += 1;

            // A bare hesitation ("Uh", "Well...") that outlived the
            // continuation hold is the caller THINKING: record it,
            // reply with NOTHING, keep listening. Filler never
            // reaches the model — as history or as a prompt.
            let hesitation = !is_control && crate::duplex::is_hesitation(&text);
            if agent_enabled && hesitation {
                eprintln!(
                    "[aokie-plugin] caller hesitation — staying quiet while they finish thinking"
                );
                // The turn-flush already stamped 'thinking'; a
                // hesitation never runs the reply block, so restore
                // 'listening' HERE or the caption strip lies (live
                // call 20563f53: stuck on 'Live - thinking' while the
                // bot was correctly staying quiet).
                if let Some(lane) = ctx.rt_lane.as_mut() {
                    if let Some(line) = lane.phase("listening", Instant::now()) {
                        let _ = sink.send_line(&line);
                    }
                }
            }

            // Whether to fall through to the normal LLM reply path.
            let mut respond_with_llm = false;
            if ctx.agent_hung_up {
                // The agent already said goodbye and hung up: a late
                // STT result from goodbye-overlap capture is part of
                // the record, never a prompt for one more reply into
                // a dying line.
                eprintln!(
                    "[aokie-plugin] turn arrived after the agent hung up — recorded, not answered"
                );
            }
            if agent_enabled && !hesitation && !ctx.agent_hung_up {
                ctx.prev_caller_text = text.clone();
                ctx.history
                    .push(serde_json::json!({ "role": "user", "content": text }));
                if ctx.history.len() > 24 {
                    let drop = ctx.history.len() - 24;
                    ctx.history.drain(..drop);
                }
                match ctx.dialogue.apply(intent) {
                    crate::duplex::DialogueAction::ReplyNormally => {
                        respond_with_llm = true;
                    }
                    crate::duplex::DialogueAction::StaySilent => {
                        // The caller asked for the floor to stay open
                        // ("wait", "stop", "let me think"): keep
                        // listening, generate nothing, speak nothing.
                        eprintln!(
                            "[aokie-plugin] caller holds the floor ({intent:?}) — waiting silently"
                        );
                    }
                    crate::duplex::DialogueAction::AdjustPace(cmd) => {
                        let line = match cmd {
                            crate::duplex::PaceCommand::Slower => {
                                ctx.pace.slower();
                                "Sure, I'll slow down."
                            }
                            crate::duplex::PaceCommand::Faster => {
                                ctx.pace.faster();
                                "Sure, I'll speed up."
                            }
                            crate::duplex::PaceCommand::Normal => {
                                ctx.pace.reset();
                                "Okay, back to normal speed."
                            }
                        };
                        eprintln!(
                            "[aokie-plugin] caller pace command {cmd:?} — base rate now {:.2}",
                            ctx.pace.base()
                        );
                        let sr = bt.get_sample_rate();
                        if sr > 0 {
                            let mut probe =
                                ControlProbe::new(&control_rx, &mut *pending_controls);
                            let (aec_ref, brms) = if barge_in {
                                (aec.as_mut(), Some(barge_rms))
                            } else {
                                (None, None)
                            };
                            // The ack itself plays at the NEW pace —
                            // the confirmation demonstrates the change.
                            let ack_started = Instant::now();
                            let out = tts_speak(
                                bt,
                                &synth,
                                line,
                                sr,
                                aec_ref,
                                brms,
                                Some(&mut probe),
                                ctx.pace.base(),
                                None,
                                None,
                                None,
                            );
                            note_tts_outcome(&status, &out);
                            if !barge_in {
                                *mute_stt_until = Some(
                                    Instant::now() + out.dur + Duration::from_millis(400),
                                );
                            }
                            if out.barged {
                                bt.flush_tx_audio();
                            }
                            if !out.captured_speech.is_empty() {
                                let mut seeded = crate::voice::to_f32_16k(
                                    &out.captured_speech,
                                    sr as u32,
                                );
                                seeded.extend_from_slice(&stt_buf);
                                *stt_buf = seeded;
                                *stt_had_speech = true;
                                *stt_silence = Duration::ZERO;
                                *turn_overlapped = true;
                                *turn_overlap_at = Some(overlap_backdate(
                                    out.captured_speech.len(),
                                    sr as usize,
                                    ack_started.elapsed(),
                                ));
                            }
                            if out.dur > Duration::ZERO {
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
                                    line,
                                    Some(delivery),
                                    Some(&aokie_core::events::iso8601_ago_ms(
                                        ack_started.elapsed().as_millis() as u64,
                                    )),
                                );
                                ctx.turn_index += 1;
                                ctx.history.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": line,
                                }));
                                ctx.last_bot_reply = line.to_string();
                            }
                            if let Some(action) = probe.action.take() {
                                perform_cancel_action(
                                    action,
                                    bt,
                                    &mut *tracker,
                                    outbox,
                                    sink,
                                );
                            }
                        }
                    }
                    crate::duplex::DialogueAction::Replay { slower } => {
                        if ctx.last_bot_speech.is_empty() {
                            // Nothing to replay yet — let the model
                            // answer the request instead.
                            respond_with_llm = true;
                        } else {
                            let replay_pace = if slower {
                                ctx.pace.replay_slower()
                            } else {
                                ctx.pace.clone()
                            };
                            eprintln!(
                                "[aokie-plugin] replaying the last reply{} (deterministic)",
                                if slower { " slower" } else { "" }
                            );
                            let sr = bt.get_sample_rate();
                            if sr > 0 {
                                let replay_text = ctx.last_bot_speech.clone();
                                let mut probe =
                                    ControlProbe::new(&control_rx, &mut *pending_controls);
                                let (aec_ref, brms) = if barge_in {
                                    (aec.as_mut(), Some(barge_rms))
                                } else {
                                    (None, None)
                                };
                                let mut lane = SttProbeLane::new(
                                    &stt_tx,
                                    &probe_result_rx,
                                    voice_call_gen,
                                    &status,
                                );
                                lane.set_bot_context(replay_text.clone());
                                let lane_ref =
                                    if barge_in { Some(&mut lane) } else { None };
                                let replay_started = Instant::now();
                                let planned = speak_planned(
                                    bt,
                                    &synth,
                                    &replay_text,
                                    sr,
                                    aec_ref,
                                    brms,
                                    Some(&mut probe),
                                    &replay_pace,
                                    protected_max_ms,
                                    lane_ref,
                                    None,
                                );
                                let out = planned.outcome;
                                note_tts_outcome(&status, &out);
                                if !barge_in {
                                    *mute_stt_until = Some(
                                        Instant::now()
                                            + out.dur
                                            + Duration::from_millis(400),
                                    );
                                }
                                if out.barged {
                                    bt.flush_tx_audio();
                                }
                                if !out.captured_speech.is_empty() {
                                    let mut seeded = crate::voice::to_f32_16k(
                                        &out.captured_speech,
                                        sr as u32,
                                    );
                                    seeded.extend_from_slice(&stt_buf);
                                    *stt_buf = seeded;
                                    *stt_had_speech = true;
                                    *stt_silence = Duration::ZERO;
                                    *turn_overlapped = true;
                                    *turn_overlap_at = Some(overlap_backdate(
                                        out.captured_speech.len(),
                                        sr as usize,
                                        replay_started.elapsed(),
                                    ));
                                }
                                if out.dur > Duration::ZERO
                                    && !planned.played_text.is_empty()
                                {
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
                                            replay_started.elapsed().as_millis() as u64,
                                        )),
                                    );
                                    ctx.turn_index += 1;
                                    ctx.history.push(serde_json::json!({
                                        "role": "assistant",
                                        "content": planned.played_text,
                                    }));
                                    ctx.last_bot_reply = planned.sent_text;
                                }
                                if let Some(action) = probe.action.take() {
                                    perform_cancel_action(
                                        action,
                                        bt,
                                        &mut *tracker,
                                        outbox,
                                        sink,
                                    );
                                }
                            }
                        }
                    }
                }
                // The silence watchdog follows the floor state: a
                // DELIBERATE pause stretches the window (never nag a
                // caller who asked for quiet); everything else runs
                // the normal window from now.
                if !silence_window.is_zero()
                    && tracker.current().is_some_and(|s| s.is_active())
                {
                    let w = if ctx.dialogue.is_paused() {
                        silence_window * 3
                    } else {
                        silence_window
                    };
                    ctx.silence_timer = Some(SilenceTimer::new(w, Instant::now()));
                }
            }

            if !respond_with_llm {
                // The turn resolved WITHOUT a reply (hesitation, floor
                // command, paused dialogue): a pending speculation has
                // nothing to be adopted by — kill the worker.
                if let Some(sp) = spec_reply.take() {
                    sp.cancel.store(true, Ordering::Relaxed);
                    status.spec_llm_wasted.fetch_add(1, Ordering::Relaxed);
                }
            }
            if agent_enabled && respond_with_llm {
                // Lazily connect to the local LLM on the first caller turn.
                if agent_client.is_none() {
                    // Prefer the ring-warmed client (already connected).
                    *agent_client = pending_agent_client.lock().unwrap().take();
                }
                if agent_client.is_none() {
                    // PROC-001: a live connect is fresher than the probe.
                    *agent_client =
                        connect_agent_client(&agent_endpoint, agent_model.clone(), &status);
                }
                if let Some(client) = agent_client.as_ref() {
                    run_reply_rounds(
                        bt,
                        outbox,
                        sink,
                        control_rx,
                        status,
                        host_rpc,
                        data_dir,
                        remote_media,
                        synth,
                        stt_tx,
                        probe_result_rx,
                        stt_buf,
                        stt_had_speech,
                        stt_silence,
                        agent_persona,
                        spec_reply,
                        pending_controls,
                        mute_stt_until,
                        barge_in,
                        send_audio,
                        screen_policy,
                        agent_hangup,
                        barge_rms,
                        aec,
                        protected_max_ms,
                        turn_overlapped,
                        turn_overlap_at,
                        tracker,
                        pending_companion_end_caller,
                        voice_call_gen,
                        ctx,
                        silence_window,
                        client,
                        &corr,
                        &text,
                    );
                }
            }
        }
    }
}
