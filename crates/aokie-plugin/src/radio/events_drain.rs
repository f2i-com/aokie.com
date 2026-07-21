//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `drain_bluetooth_events`.

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "voice"), allow(unused_variables))]
pub(super) fn drain_bluetooth_events(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    phantom_ring_count: &mut u32,
    last_phone_persisted: &mut Option<String>,
    last_phone_path: &std::path::PathBuf,
    tracker: &mut crate::call_session::SessionTracker,
    pending_companion_end_caller: &mut Option<PendingCompanionEndCaller>,
    ctx: &mut CallVoiceContext,
    idle: &mut bool,
    #[cfg(feature = "voice")] realtime_selected: bool,
    #[cfg(feature = "voice")] synth: &crate::synth::SynthHandle,
    #[cfg(feature = "voice")] stt_tx: &std::sync::mpsc::Sender<SttWork>,
    #[cfg(feature = "voice")] stt_result_rx: &std::sync::mpsc::Receiver<SttResult>,
    #[cfg(feature = "voice")] stt_buf: &mut Vec<f32>,
    #[cfg(feature = "voice")] stt_outstanding: &mut usize,
    #[cfg(feature = "voice")] spec_utterance: &mut Option<u32>,
    #[cfg(feature = "voice")] stale_specs: &mut Vec<u32>,
    #[cfg(feature = "voice")] stt_had_speech: &mut bool,
    #[cfg(feature = "voice")] stt_silence: &mut std::time::Duration,
    #[cfg(feature = "voice")] agent_enabled: bool,
    #[cfg(feature = "voice")] agent_endpoint: &Arc<Mutex<Option<String>>>,
    #[cfg(feature = "voice")] agent_persona: &String,
    #[cfg(feature = "voice")] agent_model: &Option<String>,
    #[cfg(feature = "voice")] agent_client: &mut Option<crate::agent::LlmClient>,
    #[cfg(feature = "voice")] pending_agent_client: &Arc<Mutex<Option<crate::agent::LlmClient>>>,
    #[cfg(feature = "voice")] audio_transcript: bool,
    #[cfg(feature = "voice")] audio_capture: bool,
    #[cfg(feature = "voice")] heard_tx: &std::sync::mpsc::Sender<TranscriptCorrectionResult>,
    #[cfg(feature = "voice")] transcript_client_cache: &std::sync::Arc<std::sync::OnceLock<Option<crate::agent::LlmClient>>>,
    #[cfg(feature = "voice")] utt_audio: &mut std::collections::VecDeque<(u32, Vec<i16>)>,
    #[cfg(feature = "voice")] agent_hangup: bool,
) {
    while let Some(ev) = bt.try_recv_event() {
        *idle = false;
        let companion_terminated_call =
            matches!(&ev, aokie_dongle::bluetooth::BluetoothEvent::CallTerminated)
                .then(|| tracker.call_id().map(str::to_owned))
                .flatten();
        let companion_hangup_write_failure = match &ev {
            aokie_dongle::bluetooth::BluetoothEvent::Error(error)
                if error.trim_start().starts_with("hangup:") =>
            {
                Some(error.clone())
            }
            _ => None,
        };
        // Final-transcript drain (audit AOK-LIF-002): the caller's last
        // words must land BEFORE call.ended — summaries and after-call
        // flows key off ended, and the last sentence is often the most
        // important one. On termination of a live call: finalize any
        // buffered audio as the closing utterance, wait (bounded) for
        // in-flight STT, and flush the held turn — THEN let handle_event
        // publish the terminal event. The call is already over, so the
        // short stall cannot delay answering it.
        // Ring-time pre-warm (user idea 2026-07-14): the phone is still
        // RINGING — load both speech engines and connect the LLM now, so
        // the greeting synthesizes hot and the first reply starts fast (a
        // cold TTS engine after a plugin restart cost seconds live, and
        // the STT engine used to load lazily mid-call).
        // Self-heal for a PHANTOM "answered" session (live incident
        // 2026-07-14): a scrambled indicator mapping on one bad SLC made
        // a ringing CIEV read as CallAnswered — the tracker held an
        // ACTIVE call that never existed, auto-answer's !is_active()
        // guard then skipped every REAL ring, and only a manual
        // reconnect recovered the line. RING repeats every ~5 s while
        // ringing, and this line has no call-waiting: a second
        // CallIncoming over an "active" inbound session is proof the
        // answer was phantom. Drop the stale session (the normal
        // terminate path — its record closes truthfully) so the ring
        // re-mints a session and auto-answer takes the call.
        if matches!(ev, aokie_dongle::bluetooth::BluetoothEvent::CallIncoming) {
            if tracker
                .current()
                .is_some_and(|s| s.is_active() && !s.outbound)
            {
                *phantom_ring_count += 1;
                if *phantom_ring_count >= 2 {
                    eprintln!(
                        "[aokie-plugin] RING repeating over an 'active' call — the answer was a PHANTOM (misread indicator); dropping the stale session so this call can be answered"
                    );
                    *phantom_ring_count = 0;
                    handle_event(
                        aokie_dongle::bluetooth::BluetoothEvent::CallTerminated,
                        &mut *tracker,
                        outbox,
                        sink,
                        &status,
                    );
                }
            } else {
                *phantom_ring_count = 0;
            }
        }
        #[cfg(all(target_os = "windows", feature = "voice"))]
        if matches!(ev, aokie_dongle::bluetooth::BluetoothEvent::CallIncoming)
            && should_prepare_local_speech(realtime_selected, false)
        {
            let _ = stt_tx.send(SttWork::Warm);
            synth.warm();
            // LLM warm-up runs OFF-THREAD (an on-loop HTTP call delayed
            // the ANSWER once): discover + prime llama's prompt cache with
            // the BASE reply prefix, so the first token of the first reply
            // only pays for the caller's words. A new call's history is
            // empty by definition.
            if agent_enabled {
                spawn_llm_prefix_warm(
                    &agent_endpoint,
                    agent_model.clone(),
                    &status,
                    compose_agent_system_prompt(&agent_persona, agent_hangup, None, false),
                    Vec::new(),
                    "ring",
                    Some(pending_agent_client.clone()),
                );
            }
        }
        #[cfg(feature = "voice")]
        if matches!(ev, aokie_dongle::bluetooth::BluetoothEvent::CallTerminated)
            && tracker.current().is_some()
        {
            if *stt_had_speech && stt_buf.len() >= 16_000 / 5 {
                if spec_utterance.take().is_some() {
                    // Already in flight speculatively — the drain below
                    // waits for that result; never send it twice.
                } else if let Some(s) = tracker.current_mut() {
                    let utterance = s.next_utterance_id();
                    if audio_capture {
                        stash_utt_audio(&mut *utt_audio, utterance, &stt_buf);
                    }
                    if stt_tx
                        .send(SttWork::Utterance {
                            generation: s.generation,
                            utterance,
                            samples: std::mem::take(&mut *stt_buf),
                        })
                        .is_ok()
                    {
                        *stt_outstanding += 1;
                    }
                }
            }
            stt_buf.clear();
            *stt_had_speech = false;
            *stt_silence = Duration::ZERO;

            let gen_now = tracker.generation();
            let deadline = Instant::now() + Duration::from_millis(1500);
            while *stt_outstanding > 0 {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    eprintln!(
                        "[aokie-plugin] final-transcript drain timed out with {stt_outstanding} STT job(s) in flight"
                    );
                    break;
                }
                match stt_result_rx.recv_timeout(left) {
                    Ok(SttResult {
                        generation,
                        utterance,
                        text,
                    }) => {
                        *stt_outstanding = stt_outstanding.saturating_sub(1);
                        // The stash pops on EVERY arm — a discarded
                        // result's audio must never pair with a later one.
                        let utt_pcm = take_utt_audio(&mut *utt_audio, utterance);
                        if let Some(pos) = stale_specs.iter().position(|&u| u == utterance) {
                            stale_specs.remove(pos);
                            continue;
                        }
                        if generation != gen_now {
                            status.stale_stt_results.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if agent_enabled && looks_like_echo(&text, &ctx.last_bot_reply) {
                            continue;
                        }
                        match ctx.pending_turn.as_mut() {
                            Some(p) => {
                                p.text.push(' ');
                                p.text.push_str(text.trim());
                                if let Some(pcm) = utt_pcm {
                                    append_turn_audio(&mut p.audio, pcm);
                                }
                            }
                            None => {
                                ctx.pending_turn = Some(PendingTurn {
                                    corr: tracker.call_id().unwrap_or_default().to_string(),
                                    text: text.trim().to_string(),
                                    // Call-end drain: this flushes right
                                    // below, so the grace flag is moot.
                                    from_overlap: false,
                                    audio: utt_pcm.unwrap_or_default(),
                                    flush_at: Instant::now(),
                                })
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            if let Some(p) = ctx.pending_turn.take() {
                if !p.corr.is_empty() && !p.text.is_empty() {
                    // A turn spoken while the PIN gate is armed IS the
                    // PIN — never let a boundary flush record it.
                    let pin_turn = ctx.manager_gate.awaiting_pin;
                    let recorded = if pin_turn {
                        "[manager PIN redacted]"
                    } else {
                        p.text.as_str()
                    };
                    emit_turn(outbox, sink, &p.corr, ctx.turn_index, "caller", recorded);
                    if !pin_turn {
                        let setting = ctx
                            .call_agent_overlay
                            .as_ref()
                            .and_then(|overlay| overlay.persona.as_deref())
                            .unwrap_or(&agent_persona);
                        maybe_spawn_transcript_correction(
                            audio_transcript,
                            &p.corr,
                            ctx.turn_index,
                            &p.text,
                            &p.audio,
                            ctx.prev_heard.as_ref(),
                            &ctx.history,
                            setting,
                            agent_client
                                .clone()
                                .or_else(|| pending_agent_client.lock().unwrap().clone()),
                            &transcript_client_cache,
                            &heard_tx,
                        );
                    }
                    ctx.turn_index += 1;
                }
            }
        }
        handle_event(ev, &mut *tracker, outbox, sink, &status);
        if let Some(call_id) = companion_terminated_call {
            // A controller/device loss may synthesize CallTerminated even
            // though the cellular leg can still exist. The runtime's
            // connection bit is stronger proof than the UI mirror alone;
            // never report Completed on that fail-closed edge.
            resolve_companion_end_caller_termination(
                &mut *pending_companion_end_caller,
                &remote_media,
                &call_id,
                status.connected.load(Ordering::Acquire) && bt.is_connected(),
            );
        } else if let Some(error) = companion_hangup_write_failure {
            fail_companion_end_caller(
                &mut *pending_companion_end_caller,
                &remote_media,
                "radio_hangup_failed",
                error,
            );
        }
    }

    poll_companion_end_caller(
        &mut *pending_companion_end_caller,
        &tracker,
        status.as_ref(),
        &remote_media,
        Instant::now(),
    );

    // Remember the last phone that actually connected (auto OR manual) as
    // the auto-connect target for next start. Written on change only.
    {
        let current = status.connected_address.lock().unwrap().clone();
        if let Some(addr) = current {
            if last_phone_persisted.as_deref() != Some(addr.as_str()) {
                let _ = std::fs::write(&last_phone_path, &addr);
                *last_phone_persisted = Some(addr);
            }
        }
    }
}
