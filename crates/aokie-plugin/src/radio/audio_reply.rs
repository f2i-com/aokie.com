//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `pump_audio_and_reply`.

#[allow(unused_imports)]
use super::*;

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn pump_audio_and_reply(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    status: &Arc<RadioStatus>,
    host_rpc: &Arc<crate::host_rpc::HostRpc>,
    data_dir: &std::path::Path,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    realtime_selected: bool,
    synth: &crate::synth::SynthHandle,
    stt_tx: &std::sync::mpsc::Sender<SttWork>,
    stt_result_rx: &std::sync::mpsc::Receiver<SttResult>,
    probe_result_rx: &std::sync::mpsc::Receiver<SttResult>,
    stt_buf: &mut Vec<f32>,
    stt_outstanding: &mut usize,
    spec_utterance: &mut Option<u32>,
    stale_specs: &mut Vec<u32>,
    stt_had_speech: &mut bool,
    stt_silence: &mut std::time::Duration,
    stt_endpoint: std::time::Duration,
    agent_enabled: bool,
    agent_endpoint: &Arc<Mutex<Option<String>>>,
    agent_persona: &String,
    agent_model: &Option<String>,
    agent_client: &mut Option<crate::agent::LlmClient>,
    live_hyp: &mut Option<String>,
    live_hyp_prev: &mut Option<String>,
    live_hyp_shipped: &mut usize,
    live_hyp_at: &mut Option<std::time::Instant>,
    live_probe_in_flight: &mut bool,
    spec_reply: &mut Option<ReplyStream>,
    pending_agent_client: &Arc<Mutex<Option<crate::agent::LlmClient>>>,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    mute_stt_until: &mut Option<std::time::Instant>,
    barge_in: bool,
    send_audio: bool,
    audio_transcript: bool,
    audio_capture: bool,
    heard_tx: &std::sync::mpsc::Sender<TranscriptCorrectionResult>,
    heard_rx: &std::sync::mpsc::Receiver<TranscriptCorrectionResult>,
    transcript_client_cache: &std::sync::Arc<std::sync::OnceLock<Option<crate::agent::LlmClient>>>,
    screen_policy: &mut crate::screen::ScreenPolicy,
    utt_audio: &mut std::collections::VecDeque<(u32, Vec<i16>)>,
    agent_hangup: bool,
    barge_rms: f32,
    aec: &mut Option<crate::aec::EchoCanceller>,
    protected_max_ms: u32,
    turn_overlapped: &mut bool,
    turn_overlap_at: &mut Option<String>,
    tracker: &mut crate::call_session::SessionTracker,
    pending_companion_end_caller: &mut Option<PendingCompanionEndCaller>,
    voice_call_gen: u64,
    realtime_lane: &mut Option<RealtimeCallLane>,
    realtime_legacy_call: &mut Option<String>,
    realtime_midcall_failure: &mut Option<(String, crate::remote_media::AokieOwnerFence, String)>,
    realtime_terminal_call: &mut Option<(String, std::time::Instant, u8)>,
    realtime_deferred_policy_failure: &mut Option<(String, String)>,
    ctx: &mut CallVoiceContext,
    silence_window: std::time::Duration,
    idle: &mut bool,
) {
    #[cfg(feature = "voice")]
    {
        const SPEECH_RMS: f32 = 350.0;
        let endpoint = stt_endpoint;
        let muted = mute_stt_until.is_some_and(|t| Instant::now() < t);
        while let Some(frame) = bt.try_recv_audio() {
            *idle = false;
            remote_media.try_push_sco(&frame.samples, frame.sample_rate as u32);
            // ACTIVE calls only: some phones open the SCO during RINGING
            // (in-band ringtone) — transcribing that seeds the first
            // caller turn with garbage, and no caller can speak before
            // the call is answered anyway. OUTBOUND: only an
            // AGENT-PLACED call (call.dial) is transcribed/answered —
            // the owner's own handset-dialed call stays private; the
            // receptionist has no business on that line.
            if !tracker
                .current()
                .is_some_and(|s| s.is_active() && (!s.outbound || s.agent_owned))
            {
                continue;
            }
            // During a pending/active human route, caller audio belongs to
            // the native Companion lane only. Never feed it to Aokie's STT
            // or start a competing reply generation.
            if remote_media.radio_reserved() {
                continue;
            }
            if exact_failure_call(
                tracker.call_id(),
                [
                    realtime_deferred_policy_failure
                        .as_ref()
                        .map(|(call_id, _)| call_id.as_str()),
                    realtime_midcall_failure
                        .as_ref()
                        .map(|(call_id, _, _)| call_id.as_str()),
                    realtime_terminal_call
                        .as_ref()
                        .map(|(call_id, _, _)| call_id.as_str()),
                ],
            ) {
                // A terminal policy/responder failure owns this call until
                // verified termination or human return. Never demote it
                // into local STT/LLM work during settle/retry windows.
                continue;
            }
            let realtime_current = realtime_owns_call(
                realtime_selected,
                realtime_legacy_call.as_deref(),
                tracker.call_id(),
            );
            if realtime_current {
                let mut input_failure: Option<(
                    String,
                    crate::remote_media::AokieOwnerFence,
                    String,
                )> = None;
                if let Some(lane) = realtime_lane.as_mut().filter(|lane| {
                    lane.begun
                        && tracker.call_id() == Some(lane.call_id.as_str())
                        && reply_owner_is_current(&remote_media, lane.owner.as_ref())
                }) {
                    let rate = frame.sample_rate as u32;
                    if lane.sco_rate != rate {
                        if let Some(owner) = lane.owner.clone() {
                            input_failure = Some((
                                lane.call_id.clone(),
                                owner,
                                format!(
                                    "SCO sample rate changed from {}Hz to {rate}Hz during realtime voice",
                                    lane.sco_rate
                                ),
                            ));
                        }
                    } else {
                        if aec.is_none() {
                            *aec = Some(crate::aec::EchoCanceller::new(rate));
                        }
                        if let Some(realtime_aec) = aec.as_mut() {
                            let cleaned = realtime_aec.process_capture(&frame.samples);
                            if !cleaned.is_empty() {
                                if crate::voice::frame_rms(&cleaned) > SPEECH_RMS {
                                    if let Some(timer) = ctx.silence_timer.as_mut() {
                                        timer.note_activity(Instant::now());
                                    }
                                    // Do not wait for the upstream VAD
                                    // round trip to veto a voluntary call
                                    // finish. Cleaned SCO speech is the
                                    // closest physical evidence that the
                                    // caller is still talking, and it also
                                    // cancels every bounded CHUP retry.
                                    lane.note_caller_activity();
                                }
                                // Caller PCM flows LIVE even while a tool
                                // result is outstanding: the Desktop
                                // bridge tolerates a VAD-created response
                                // during a pending tool, so the caller is
                                // never met with a frozen, deaf line while
                                // a lookup runs. The deferred buffer only
                                // drains a residue left by an earlier
                                // withholding build.
                                let input_result = if !lane.deferred_input.is_empty() {
                                    // Preserve old-before-new ordering and
                                    // coalesce the catch-up into one bounded
                                    // 100 ms command per incoming SCO frame.
                                    lane.deferred_input.push(&cleaned, rate);
                                    let chunk = lane.deferred_input.take_flush_chunk(rate);
                                    lane.session.send_input(&chunk, rate)
                                } else {
                                    lane.session.send_input(&cleaned, rate)
                                };
                                if let Err(error) = input_result {
                                    if let Some(owner) = lane.owner.clone() {
                                        input_failure =
                                            Some((lane.call_id.clone(), owner, error));
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some((call_id, owner, error)) = input_failure {
                    if let Some(lane) = realtime_lane.take() {
                        lane.session.stop("realtime input failed");
                    }
                    bt.flush_tx_audio();
                    if let Some(failed_aec) = aec.as_mut() {
                        failed_aec.reset();
                    }
                    status.realtime_ready.store(false, Ordering::Relaxed);
                    *status.realtime_error.lock().unwrap() = Some(error.clone());
                    *realtime_midcall_failure = Some((call_id, owner, error));
                }
                // Exact lane exclusivity: a selected Realtime call never
                // enters local VAD/STT/probe/LLM, even while its prepared
                // session is not yet begun or has failed. Before-answer
                // failure rings through; mid-call failure is handled by
                // the fixed caller-safe disposition below.
                continue;
            }
            // Full-duplex: echo-cancel the mic (so Aokie's own voice, even
            // when it's mid-reply, doesn't transcribe as the caller) and keep
            // reference consumption 1:1 with the mic. Half-duplex: the mute
            // window swallows the echo instead, so we just skip while muted.
            let samples: std::borrow::Cow<[i16]> = if barge_in {
                match aec.as_mut() {
                    Some(a) => {
                        let cleaned = a.process_capture(&frame.samples);
                        if cleaned.is_empty() {
                            continue;
                        }
                        std::borrow::Cow::Owned(cleaned)
                    }
                    None => std::borrow::Cow::Borrowed(&frame.samples[..]),
                }
            } else {
                if muted {
                    continue;
                }
                std::borrow::Cow::Borrowed(&frame.samples[..])
            };
            let f16 = crate::voice::to_f32_16k(&samples, frame.sample_rate as u32);
            let frame_dur =
                Duration::from_secs_f32(samples.len() as f32 / frame.sample_rate.max(1) as f32);
            if ctx.capture_activity.detect(&samples, &f16, *stt_had_speech) {
                if !*stt_had_speech {
                    ctx.capture_activity.prepend_onset(stt_buf);
                }
                *stt_had_speech = true;
                *stt_silence = Duration::ZERO;
                // Speech resumed after a speculative send: that spec no
                // longer matches the utterance — drop its result by id.
                if let Some(id) = spec_utterance.take() {
                    stale_specs.push(id);
                }
                stt_buf.extend_from_slice(&f16);
                // AOK-CTRL-001: live caller audio resets the max-silence window.
                if let Some(t) = ctx.silence_timer.as_mut() {
                    t.note_activity(Instant::now());
                }
            } else if *stt_had_speech {
                *stt_silence += frame_dur;
                stt_buf.extend_from_slice(&f16); // keep trailing silence for context
            } else {
                ctx.capture_activity.remember_quiet(&f16);
            }
            if stt_buf.len() > 16_000 * 15 {
                *stt_silence = endpoint; // force-flush a runaway (~15 s) utterance
                if let Some(id) = spec_utterance.take() {
                    stale_specs.push(id);
                }
            }
        }
        // Speculative early transcription: 200 ms into the pause (well
        // before the endpoint), ship the buffer as-is. If the caller
        // stays quiet, the endpoint below has NOTHING left to do — the
        // text is already in flight (or back).
        if *stt_had_speech
            && spec_utterance.is_none()
            && *stt_silence >= Duration::from_millis(200)
            && *stt_silence < endpoint
            && stt_buf.len() >= 16_000 / 5
        {
            if let Some(s) = tracker.current_mut() {
                let utterance = s.next_utterance_id();
                if audio_capture {
                    stash_utt_audio(&mut *utt_audio, utterance, &stt_buf);
                }
                if stt_tx
                    .send(SttWork::Utterance {
                        generation: s.generation,
                        utterance,
                        samples: stt_buf.clone(),
                    })
                    .is_ok()
                {
                    *stt_outstanding += 1;
                    *spec_utterance = Some(utterance);
                }
            }
        }
        if *stt_had_speech && *stt_silence >= endpoint {
            if spec_utterance.take().is_some() {
                // The speculation IS this utterance (nothing new was said
                // since it was sent) — never transcribe it twice. Its
                // audio was stashed at the SPEC SEND, so the result drain
                // already paired it into the pending turn.
                status.early_stt_hits.fetch_add(1, Ordering::Relaxed);
                stt_buf.clear();
            } else if stt_buf.len() >= 16_000 / 5 {
                // Stamp the job with the call it belongs to (audit C-05).
                if let Some(s) = tracker.current_mut() {
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
                } else {
                    stt_buf.clear();
                }
            } else {
                stt_buf.clear();
            }
            *stt_had_speech = false;
            *stt_silence = Duration::ZERO;
        }
        status
            .loop_phase
            .store(loop_phase::RESULTS, Ordering::Relaxed);
        // ── LIVE HYPOTHESIS LANE (guide phase 2/5) ──────────────────────
        // While the caller is mid-utterance and the bot is silent, ship
        // the ACCUMULATING buffer for partial transcription (~600 ms
        // cadence, ≤1 in flight, local probe channel — no HTTP egress).
        // Phase 3: while the PIN gate is waiting, the utterance IS the
        // PIN — no partial captions, no probes, no speculation on it.
        if agent_enabled && *stt_had_speech && !ctx.manager_gate.awaiting_pin {
            // LAT-002 (2026-07-17, specLlmStarted was 0 on EVERY live
            // call): `live_probe_in_flight` was a one-way latch — an
            // empty/failed probe transcription sends NO result back, and
            // a reply/greeting lane's drain can eat a late one — so one
            // lost result killed the hypothesis lane for the rest of the
            // call. Mirror the playback SttProbeLane's lost-result rule:
            // unblock after 1.5s.
            if *live_probe_in_flight
                && live_hyp_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(1500))
            {
                *live_probe_in_flight = false;
            }
            let due = live_hyp_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(400));
            let grown =
                stt_buf.len() >= *live_hyp_shipped + 16_000 / 2 && stt_buf.len() >= 16_000 / 2;
            if !*live_probe_in_flight && due && grown {
                if let Some(sess) = tracker.current() {
                    let from = stt_buf.len().saturating_sub(16_000 * 8);
                    if stt_tx
                        .send(SttWork::Probe {
                            generation: sess.generation,
                            lane: LIVE_HYP_LANE,
                            samples: stt_buf[from..].to_vec(),
                        })
                        .is_ok()
                    {
                        *live_probe_in_flight = true;
                        *live_hyp_shipped = stt_buf.len();
                        *live_hyp_at = Some(Instant::now());
                        status.probes_sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            while let Ok(res) = probe_result_rx.try_recv() {
                // Foreign-lane result (a playback lane's probe landing
                // after its lane was dropped): not ours — it must not
                // clear OUR in-flight slot or become a hypothesis.
                if res.utterance != LIVE_HYP_LANE {
                    continue;
                }
                *live_probe_in_flight = false;
                if res.generation != voice_call_gen || res.text.trim().is_empty() {
                    continue;
                }
                *live_hyp_prev = live_hyp.take();
                *live_hyp = Some(res.text);
                if let (Some(lane), Some(cur)) = (ctx.rt_lane.as_mut(), live_hyp.as_deref()) {
                    if let Some(line) = lane.user_partial(cur, Instant::now()) {
                        let _ = sink.send_line(&line);
                    }
                }
                // Content-free tuning telemetry: how often consecutive
                // partials look stable on REAL calls (specLlmStarted was
                // 0 for a whole evening before this existed).
                if let (Some(p), Some(c)) = (live_hyp_prev.as_deref(), live_hyp.as_deref()) {
                    eprintln!(
                        "[aokie-plugin] hypothesis pair: {}w -> {}w, stable: {}",
                        p.split_whitespace().count(),
                        c.split_whitespace().count(),
                        hypothesis_stable(p, c)
                    );
                }
            }
            // A stable, intent-sized hypothesis starts the reply EARLY —
            // the generation streams while the caller finishes; the turn
            // flush below adopts it when the final text matches.
            if spec_reply.is_none() && !ctx.dialogue.is_paused() {
                // Adopt a ring-warmed client so the FIRST turn speculates.
                if agent_client.is_none() {
                    *agent_client = pending_agent_client.lock().unwrap().take();
                }
                if let (Some(prev), Some(cur)) = (live_hyp_prev.as_deref(), live_hyp.as_deref())
                {
                    if hypothesis_stable(prev, cur) {
                        if let Some(client) = agent_client.as_ref().filter(|client| {
                            llm_endpoint_allows_speculative_reply(client.endpoint())
                        }) {
                            let is_mgr_call = tracker.current().is_some_and(|s| {
                                !s.outbound && screen_policy.is_manager(s.caller_id.as_deref())
                            });
                            // Manager line (2026-07-17): a dedicated
                            // persona frame replaces the customer framing,
                            // and the KNOWN-CALLER overlay (customer
                            // personalization) never applies to it.
                            let persona_base: &str = if is_mgr_call {
                                &agent_persona
                            } else {
                                ctx.call_agent_overlay
                                    .as_ref()
                                    .and_then(|o| o.persona.as_deref())
                                    .unwrap_or(&agent_persona)
                            };
                            // Phase 3: same manager block as the real
                            // reply path — an adopted speculation must be
                            // primed identically.
                            let persona_now: String = if manager_access_allowed(
                                ctx.manager_gate.verified,
                                is_mgr_call,
                            ) {
                                format!("{persona_base}{MANAGER_INSTRUCTION}")
                            } else if is_mgr_call {
                                format!("{persona_base}{MANAGER_LINE_BLOCK}")
                            } else {
                                persona_base.to_string()
                            };
                            // PEEK the nudge tail (never consume — a
                            // discarded speculation must leave it for the
                            // real reply).
                            let mut sys = compose_agent_system_prompt(
                                &persona_now,
                                agent_hangup,
                                ctx.last_cut_context.as_deref(),
                                is_mgr_call,
                            );
                            let mut grounding_history = ctx.history.clone();
                            grounding_history.push(serde_json::json!({"role":"user","content":cur}));
                            let remote = remote_media.snapshot();
                            sys.push_str(&crate::conversation_policy::context(
                                chrono::Local::now().date_naive(), &grounding_history,
                                remote.consent.assistance_enabled, remote.consent.takeover_enabled,
                            ));
                            let mut messages =
                                vec![serde_json::json!({ "role": "system", "content": sys })];
                            messages.extend(ctx.history.iter().cloned());
                            messages
                                .push(serde_json::json!({ "role": "user", "content": cur }));
                            eprintln!(
                                "[aokie-plugin] speculative reply START on stable hypothesis: {}",
                                content_for_log(cur)
                            );
                            status.spec_llm_started.fetch_add(1, Ordering::Relaxed);
                            *spec_reply = Some(spawn_reply_stream(
                                client,
                                serde_json::json!(messages),
                                cur.to_string(),
                            ));
                        }
                    }
                }
            }
        } else if !*stt_had_speech {
            // Utterance over (or none): partials for it are dead. A late
            // probe result must not seed the NEXT utterance's hypothesis.
            if live_hyp.is_some() || live_hyp_prev.is_some() {
                *live_hyp = None;
                *live_hyp_prev = None;
            }
            *live_hyp_shipped = 0;
            while probe_result_rx.try_recv().is_ok() {}
            // LAT-002: reset UNCONDITIONALLY — the empty-transcription
            // loss route leaves nothing in the channel to drain, and any
            // in-flight result is dead for the next utterance anyway.
            // This revives the lane per-utterance instead of per-call.
            *live_probe_in_flight = false;
        }

        // Finished transcripts: accumulate into the OPEN caller turn
        // (audit AK-008). A transcript whose tail looks unfinished — a
        // digit group mid-phone-number, "my number is…" — keeps the turn
        // open for CONTINUATION_HOLD instead of triggering a reply into
        // the caller's pause; the next utterance merges into it. Anything
        // else flushes on the spot (no added latency for normal turns).
        while let Ok(SttResult {
            generation,
            utterance,
            text,
        }) = stt_result_rx.try_recv()
        {
            *idle = false;
            *stt_outstanding = stt_outstanding.saturating_sub(1);
            // The stash pops on EVERY arm — a discarded result's audio
            // must never pair with a later turn.
            let utt_pcm = take_utt_audio(&mut *utt_audio, utterance);
            // A superseded speculative transcription (its utterance grew
            // after it was sent): the whole utterance re-transcribed —
            // drop this partial result, never merge it.
            if let Some(pos) = stale_specs.iter().position(|&u| u == utterance) {
                stale_specs.remove(pos);
                continue;
            }
            // Stale-result gate (audit C-05): only text whose generation IS
            // the current call may be recorded or answered. A slow result
            // from a previous call is dropped and counted — never spoken
            // to, or attributed to, the next caller.
            if tracker.current().map(|s| s.generation) != Some(generation) {
                let n = status.stale_stt_results.fetch_add(1, Ordering::Relaxed) + 1;
                eprintln!(
                    "[aokie-plugin] DROPPED stale STT result (call gen {generation}, utterance {utterance}, current gen {}, {n} total): {}",
                    tracker.generation(),
                    content_for_log(&text)
                );
                continue;
            }
            // Drop a transcript that's really Aokie's own reply echoing back
            // (belt-and-suspenders over the half-duplex mute) so it never
            // records it as a caller turn or answers itself.
            if agent_enabled && looks_like_echo(&text, &ctx.last_bot_reply) {
                eprintln!(
                    "[aokie-plugin] ignored self-echo: {}",
                    content_for_log(&text)
                );
                continue;
            }
            let corr = tracker.call_id().unwrap_or_default().to_string();
            match ctx.pending_turn.as_mut() {
                Some(p) => {
                    p.text.push(' ');
                    p.text.push_str(text.trim());
                    p.corr = corr;
                    if let Some(pcm) = utt_pcm {
                        append_turn_audio(&mut p.audio, pcm);
                    }
                }
                None => {
                    ctx.pending_turn = Some(PendingTurn {
                        corr,
                        text: text.trim().to_string(),
                        // A barge just seeded stt_buf — the interruption
                        // is still in progress, so grant the merge grace.
                        from_overlap: *turn_overlapped,
                        audio: utt_pcm.unwrap_or_default(),
                        flush_at: Instant::now(),
                    })
                }
            }
            let p = ctx.pending_turn.as_mut().expect("just set");
            // An overlap turn gets ONE grace window (consumed here) so the
            // caller's mid-barge sentence completes into a single turn;
            // the normal unfinished-tail heuristic handles the rest.
            let overlap_grace = std::mem::take(&mut p.from_overlap);
            let delay = continuation_delay(&p.text, overlap_grace);
            if !delay.is_zero() {
                p.flush_at = Instant::now() + delay;
                eprintln!(
                    "[aokie-plugin] holding turn open ({}): {}",
                    if overlap_grace && !turn_looks_unfinished(&p.text) {
                        "barge continuation"
                    } else {
                        "looks unfinished"
                    },
                    content_for_log(&p.text)
                );
            } else {
                p.flush_at = Instant::now();
            }
        }
        // audioTranscript corrections (detached lane): each result
        // updates an ALREADY-EMITTED turn — a corrected event for the
        // durable transcript row plus an in-place history patch so
        // follow-up replies read the better text. Corrections for an
        // ended call still emit; only the history patch is current-call.
        while let Ok(completion) = heard_rx.try_recv() {
            let TranscriptCorrectionResult {
                call_id: cid,
                turn: tidx,
                stt,
                result,
            } = completion;
            match result {
                Ok(raw) => {
                    if sanitize_heard(&raw, &stt).is_none() {
                        // Content-free by design: "agreed" covers unchanged AND
                        // empty/oversized model output — either way the STT text
                        // stands and no event is emitted.
                        eprintln!(
                            "[aokie-plugin] audio transcript agreed with STT [turn {tidx}] — no correction"
                        );
                    }
                    if let Some(heard) = sanitize_heard(&raw, &stt) {
                        eprintln!(
                            "[aokie-plugin] audio transcript correction [turn {tidx}]: {}",
                            content_for_log(&heard)
                        );
                        emit(
                            outbox,
                            sink,
                            aokie_core::events::aokie_event_with_step(
                                crate::contract::events::CALL_TURN_CORRECTED,
                                &cid,
                                &format!("turn.{tidx}.corrected"),
                                serde_json::json!({
                                    "callId": cid,
                                    "turn": tidx,
                                    "text": heard,
                                    "sttText": stt,
                                    "at": aokie_core::events::now_iso8601(),
                                }),
                            ),
                        );
                        if tracker.call_id() == Some(cid.as_str()) {
                            if let Some(entry) = ctx.history.iter_mut().rev().find(|m| {
                                m.get("role").and_then(serde_json::Value::as_str)
                                    == Some("user")
                                    && m.get("content").and_then(serde_json::Value::as_str)
                                        == Some(stt.as_str())
                            }) {
                                entry["content"] = serde_json::json!(heard);
                            }
                        }
                    }
                }
                Err(e) => eprintln!("[aokie-plugin] transcript correction failed: {e}"),
            }
            // Complete only AFTER a corrected-turn event has been emitted.
            // The subsequent settled event shares this call's correlation
            // queue, so Desktop applies the correction first.
            transcript_correction_finished(&cid);
            emit_ready_transcript_settlements(outbox, sink);
        }
        // Flush the open turn once its hold expired AND the caller isn't
        // mid-utterance (fresh speech extends the merge window naturally).
        process_flushed_turn(
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
            agent_enabled,
            agent_endpoint,
            agent_persona,
            agent_model,
            agent_client,
            spec_reply,
            pending_agent_client,
            pending_controls,
            mute_stt_until,
            barge_in,
            send_audio,
            audio_transcript,
            audio_capture,
            heard_tx,
            transcript_client_cache,
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
            idle,
        );

        status
            .loop_phase
            .store(loop_phase::WATCHDOGS, Ordering::Relaxed);
        // AOK-CTRL-001: call-level max-silence watchdog (agent mode). A
        // live, answered call where NEITHER side has produced audio for a
        // whole window gets a check-in prompt; a second silent window gets
        // a polite goodbye and a clean hangup — a dead line never holds
        // the phone open indefinitely.
        if agent_enabled && tracker.current().is_some_and(|s| s.is_active()) {
            let sr = bt.get_sample_rate();
            let now = Instant::now();
            let aokie_owner = remote_media.aokie_owner_fence();
            let action = match (sr > 0, aokie_owner) {
                (_, None) => {
                    // Human takeover/consult owns the conversational
                    // clock. Keep forgiving any earlier prompt on every
                    // reserved pass, so Return to Aokie always receives a
                    // complete fresh silence window.
                    if let Some(timer) = ctx.silence_timer.as_mut() {
                        timer.note_activity(now);
                    }
                    None
                }
                (true, Some(owner)) => ctx
                    .silence_timer
                    .as_mut()
                    .and_then(|timer| timer.check(now))
                    .map(|action| (action, owner)),
                (false, Some(_)) => None,
            };
            match action {
                Some((SilenceAction::Prompt, owner)) => {
                    *idle = false;
                    eprintln!(
                        "[aokie-plugin] max-silence: no activity for {}s — checking in with the caller",
                        silence_window.as_secs()
                    );
                    let mut probe = ControlProbe::new(&control_rx, &mut *pending_controls);
                    let (aec_ref, brms) = if barge_in {
                        (aec.as_mut(), Some(barge_rms))
                    } else {
                        (None, None)
                    };
                    let check_started = Instant::now();
                    let out = tts_speak(
                        bt,
                        &synth,
                        SILENCE_CHECK_LINE,
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
                    if barge_in {
                        if out.barged {
                            bt.flush_tx_audio();
                            // The caller spoke over the prompt — that IS activity.
                            if let Some(t) = ctx.silence_timer.as_mut() {
                                t.note_activity(Instant::now());
                            }
                        }
                        // Scratchpad: anything said over the prompt (barge or
                        // not) seeds the caller's next turn.
                        if !out.captured_speech.is_empty() {
                            *stt_buf = crate::voice::to_f32_16k(&out.captured_speech, sr as u32);
                            *stt_had_speech = true;
                            *stt_silence = Duration::ZERO;
                            *turn_overlapped = true;
                            *turn_overlap_at = Some(overlap_backdate(
                                out.captured_speech.len(),
                                sr as usize,
                                check_started.elapsed(),
                            ));
                        }
                    } else {
                        *mute_stt_until =
                            Some(Instant::now() + out.dur + Duration::from_millis(400));
                    }
                    if out.dur > Duration::ZERO {
                        if let Some(corr) = tracker.call_id().map(str::to_string) {
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
                                SILENCE_CHECK_LINE,
                                Some(delivery),
                                Some(&aokie_core::events::iso8601_ago_ms(
                                    check_started.elapsed().as_millis() as u64,
                                )),
                            );
                            ctx.turn_index += 1;
                        }
                        ctx.history.push(serde_json::json!({
                            "role": "assistant",
                            "content": SILENCE_CHECK_LINE,
                        }));
                        ctx.last_bot_reply = SILENCE_CHECK_LINE.to_string();
                    }
                    if let Some(action) = probe.action.take() {
                        perform_cancel_action(action, bt, &mut *tracker, outbox, sink);
                    }
                    if remote_media.aokie_owner_fence().as_ref() != Some(&owner) {
                        // A claim raced the prompt. Its PCM was already
                        // stopped at the TTS chunk gate; now forgive the
                        // prompt so no stale second-window hangup survives
                        // the eventual return.
                        if let Some(timer) = ctx.silence_timer.as_mut() {
                            timer.note_activity(Instant::now());
                        }
                    }
                }
                Some((SilenceAction::HangUp, owner)) => {
                    *idle = false;
                    eprintln!(
                        "[aokie-plugin] max-silence: still nothing after the check-in — saying goodbye and ending the call"
                    );
                    let mut probe = ControlProbe::new(&control_rx, &mut *pending_controls);
                    let gb_t0 = Instant::now();
                    let out = tts_speak(
                        bt,
                        &synth,
                        SILENCE_GOODBYE_LINE,
                        sr,
                        None,
                        None,
                        Some(&mut probe),
                        ctx.pace.base(),
                        None,
                        None,
                        None,
                    );
                    note_tts_outcome(&status, &out);
                    if out.barged {
                        // The caller came back at the last moment — keep the call.
                        bt.flush_tx_audio();
                        if let Some(t) = ctx.silence_timer.as_mut() {
                            t.note_activity(Instant::now());
                        }
                    } else if let Some(action) = probe.action.take() {
                        // An operator action owns the ending instead.
                        perform_cancel_action(action, bt, &mut *tracker, outbox, sink);
                    } else {
                        if out.dur > Duration::ZERO {
                            if let Some(corr) = tracker.call_id().map(str::to_string) {
                                emit_turn_with_delivery(
                                    outbox,
                                    sink,
                                    &corr,
                                    ctx.turn_index,
                                    "bot",
                                    SILENCE_GOODBYE_LINE,
                                    Some("complete"),
                                    Some(&aokie_core::events::iso8601_ago_ms(
                                        gb_t0.elapsed().as_millis() as u64,
                                    )),
                                );
                                ctx.turn_index += 1;
                            }
                            let wait = playout_drain_wait(gb_t0, out.dur, Instant::now());
                            if !wait.is_zero() {
                                std::thread::sleep(wait);
                            }
                        }
                        match remote_media.with_aokie_owner(&owner, || {
                            tracker.note_intent(
                                crate::call_session::TerminationIntent::AgentHangup,
                            );
                            bt.hangup()
                        }) {
                            Ok(Ok(())) => {
                                ctx.agent_hung_up = true;
                                eprintln!(
                                    "[aokie-plugin] max-silence hangup complete (AT+CHUP)"
                                );
                                ctx.silence_timer = None;
                            }
                            Ok(Err(e)) => {
                                eprintln!("[aokie-plugin] max-silence hangup failed: {e}");
                                emit_control_failed(
                                    outbox,
                                    sink,
                                    &tracker,
                                    "agent.hangup",
                                    None,
                                    &e,
                                );
                                ctx.silence_timer = None;
                            }
                            Err(reason) => {
                                eprintln!(
                                    "[aokie-plugin] max-silence hangup skipped: {reason}"
                                );
                                // The claim/return transition consumed the
                                // old two-window decision. Begin a complete
                                // new window under the new Aokie owner.
                                if let Some(timer) = ctx.silence_timer.as_mut() {
                                    timer.note_activity(Instant::now());
                                }
                            }
                        }
                    }
                }
                None => {}
            }
        }
    }
}
