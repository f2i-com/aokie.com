//! Extracted from the radio conversation engine (carve-up 2026-07-21).

#[allow(unused_imports)]
use super::*;

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_reply_rounds(
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
    agent_persona: &String,
    spec_reply: &mut Option<ReplyStream>,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    mute_stt_until: &mut Option<std::time::Instant>,
    barge_in: bool,
    send_audio: bool,
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
    client: &crate::agent::LlmClient,
    corr: &str,
    text: &str,
) {
    // ── REPLY ROUNDS (guide P1-16) ──────────────────
    // Round 0 is the normal reply. A [[LOOKUP:]]
    // verdict runs the read-only host flow, injects
    // the result, and regenerates ONCE — bounded to
    // one lookup per caller turn.
    let mut lookup_rounds: u8 = 0;
    // These two bound the loop below, so they live OUTSIDE it, next to
    // lookup_rounds. Declared inside, every `continue 'reply_rounds` re-ran the
    // declaration and cleared the guard the branch had just set one line
    // earlier -- so "retry ONCE" and "regenerate ONCE, never loop" both meant
    // "forever". A model stuck emitting empty replies, or repeating [[WAIT]],
    // regenerated without limit while the branch that would have apologised and
    // hung up stayed unreachable, leaving the caller in silence until they hung
    // up themselves. wait_requested stays inside on purpose: it is this round's
    // answer to "did the model ask to wait", and must reset each time.
    let mut wait_regen_done = false;
    let mut empty_retry_done = false;
    let mut appointment_rounds = 0;
    'reply_rounds: loop {
        let sr = bt.get_sample_rate();
        // Add the standing instructions at reply time (not by
        // mutating agent_persona, which a live Configure could
        // replace): spoken-delivery/markers always, the
        // end-call marker only when agentHangup is on.
        // §9.3 call-scoped overlay: a caller-specific
        // persona (personalize-caller) applies to THIS
        // call only — wiped at the call boundary, it can
        // never leak into the next caller's conversation.
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
        // Phase 3: a manager caller (id matched against
        // managerNumbers — plugin truth, not caller words)
        // gets the READ-ONLY manager block on top.
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
        // The nudge tail is CONSUMED here (or below on
        // adoption — the speculation already baked it in).
        let mut system_prompt = compose_agent_system_prompt(
            &persona_now,
            agent_hangup,
            ctx.last_cut_context.take().as_deref(),
            is_mgr_call,
        );
        let remote = remote_media.snapshot();
        let advice_allowed = remote.consent.assistance_enabled;
        system_prompt.push_str(&crate::conversation_policy::context(
            chrono::Local::now().date_naive(), &ctx.history,
            advice_allowed, remote.consent.takeover_enabled,
        ));
        let mut messages = vec![
            serde_json::json!({ "role": "system", "content": system_prompt }),
        ];
        messages.extend(ctx.history.iter().cloned());
        // sendAudio: the LAST user message becomes content
        // PARTS — the turn's audio + its transcript. Only
        // the WIRE copy: history stays text, so replayed
        // context never re-sends old audio. (Speculative
        // replies stay text-only — they start mid-
        // utterance before the PCM is final.)
        if send_audio && !ctx.last_turn_audio.is_empty() {
            if let Some(last) = messages.last_mut() {
                if last.get("role").and_then(serde_json::Value::as_str)
                    == Some("user")
                {
                    let txt = last
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    // Silence-trimmed (2026-07-17): the
                    // attached WAV covers speech only —
                    // smaller request, less prefill.
                    let trimmed = crate::agent::trim_silence_for_llm(
                        &ctx.last_turn_audio,
                        16_000,
                    );
                    let b64 = crate::agent::LlmClient::wav_base64(
                        &trimmed, 16_000,
                    );
                    eprintln!(
                    "[aokie-plugin] attaching caller-turn audio to the LLM request ({} samples, {} after silence trim)",
                    ctx.last_turn_audio.len(),
                    trimmed.len()
                );
                    *last = serde_json::json!({
                        "role": "user",
                        "content": [
                            { "type": "input_audio", "input_audio": { "data": b64, "format": "wav" } },
                            { "type": "text", "text": txt },
                        ],
                    });
                }
            }
        }
        // Half-duplex: mute STT for the WHOLE reply as it streams.
        // Sentences synthesize faster than they play, so the audio
        // keeps playing (queued) after synthesis finishes; muting
        // only the last sentence let the tail echo back and Aokie
        // answered itself. Track cumulative playback from t0.
        // Full-duplex (barge_in): no mute â€” the AEC keeps the mic
        // clean AND watches for the caller talking over the reply,
        // cutting it short (flush the queued tail) the moment they do.
        let t0 = Instant::now();
        let mut reply_dur = Duration::ZERO;
        let mut barged = false;
        // AK-008 + scratchpad: what the caller said WHILE
        // Aokie spoke — EVERYTHING above the speech gate is
        // captured (barge or not) and prepended to their
        // turn after the reply, so no overlapped words are
        // ever lost. Listening never stops.
        let mut overlap_capture: Vec<i16> = Vec::new();
        // Phase 2: the gaps BETWEEN sentences (LLM still
        // streaming) are listened to as well — sustained
        // speech there barges exactly like speech over a
        // sentence. State for the gap-scan.
        let mut gap_frames: u32 = 0;
        let mut gap_start: Option<usize> = None;
        let mut overlap_has_speech = false;
        // ONE probe lane for the whole reply (the live
        // scratchpad): partials accumulate across
        // sentences so boundary decisions see everything
        // said so far, and a command heard at the tail of
        // one sentence still cuts the next.
        let mut reply_lane = SttProbeLane::new(
            &stt_tx,
            &probe_result_rx,
            voice_call_gen,
            &status,
        );
        // Distinguish a CALLER barge-in from an OPERATOR
        // hangup/reject mid-reply (review sweep): both stop
        // the reply, but the transcript must not label an
        // operator action as "caller interrupted".
        let mut operator_ended = false;
        // AOK-CTRL-001 follow-up: the call's audio channel
        // died mid-reply (link loss / SCO teardown). Nobody
        // can hear the rest — and speaking it anyway queued
        // stale audio that played into the NEXT call
        // (observed live 2026-07-13). Stops the pump; also
        // suppresses the dead-air fail-safe and the agent
        // hangup (both would act on a dead link).
        let mut line_dead = false;
        // Set when the reply carried the [[END_CALL]] marker: the
        // agent finalized the call and should hang up after the
        // goodbye plays (unless the caller barged in over it).
        let mut hangup_requested = false;
        // Set when the reply carried the [[WAIT]] marker: the
        // model chose INTENTIONAL SILENCE (the caller asked for
        // a moment / is thinking). An empty waited reply is NOT
        // dead air, and the floor stays with the caller.
        let mut wait_requested = false;
        // Set when the reply carried the [[ABUSE]] marker
        // (Phase 1): the model flagged an abusive caller.
        // DETERMINISTIC code takes over below — notice,
        // hangup, auto-block; no model prose is spoken.
        let mut abuse_flagged = false;
        // Set when the generation was a [[LOOKUP:]] verdict
        // (round 0 only): run the flow + regenerate.
        let mut lookup_requested: Option<String> = None;
        // A [[ASSISTANCE:]] verdict creates one ephemeral,
        // consent-gated request for the current call. The
        // model cannot choose recipients or grants.
        let mut assistance_requested: Option<String> = None;
        let mut appointment_requested = None;
        let mut holding_appointment_marker = false;
        // A strict [[TRANSFER:]] verdict offers the caller
        // to the owner. Aokie remains the audio owner until
        // the ordinary takeover path proves HumanActive.
        let mut transfer_requested: Option<String> = None;
        let mut malformed_transfer_requested = false;
        // Phase 3: the [[MANAGER:]] change request - PIN
        // gate + deterministic execution own it below.
        let mut manager_requested: Option<String> = None;
        // What the caller actually HEARD: sentences that
        // reached the speaker (audit AK-008 + sweep). The
        // history/turn record uses this, never the full
        // generation — populated in BOTH duplex modes so a
        // mid-reply failure/hangup records what played.
        let mut spoken: Vec<String> = Vec::new();
        // §6.3: the SENT twin of `spoken` — full span text
        // that produced audio (echo guard / replay), while
        // `spoken` holds the conservative audible estimate
        // (transcript / history / nudge).
        let mut sent_spans: Vec<String> = Vec::new();
        eprintln!("[aokie-plugin] agent replying (streaming)â€¦");
        // AOK-CTRL-001: the LLM stream runs on a DETACHED
        // worker; this thread pumps sentences + controls, so
        // a hangup/reject acts within ~25 ms even against a
        // stalled or punctuation-free stream (the old poll
        // only ran per SENTENCE, and a stream that never
        // yields one blocked cancellation entirely). The
        // bounded channel is the backpressure — synthesis
        // paces the worker, a runaway generation blocks the
        // WORKER, never grows a queue. The shared activity
        // stamp feeds the idle-deadline watchdog below. An
        // abandoned worker aborts at its next stream line
        // (cancel flag) or, if stuck mid-read, at the
        // client's whole-request timeout.
        // Guide phase 5: ADOPT a compatible speculative
        // generation (started mid-utterance from a stable
        // hypothesis) — its first sentence is often already
        // waiting in the channel. A diverged speculation is
        // cancelled: answering what the caller REVISED away
        // is worse than the regeneration cost. Either way
        // the nudge tail was consumed above (the adopted
        // stream baked it in at speculation time).
        let stream = match spec_reply.take() {
            Some(sp) if hypothesis_covers(&sp.answering, &text) => {
                status.spec_llm_kept.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                "[aokie-plugin] speculative reply ADOPTED ({}ms head start)",
                sp.started.elapsed().as_millis()
            );
                sp
            }
            other => {
                if let Some(sp) = other {
                    sp.cancel.store(true, Ordering::Relaxed);
                    status.spec_llm_wasted.fetch_add(1, Ordering::Relaxed);
                    eprintln!(
                    "[aokie-plugin] speculative reply discarded (final turn diverged)"
                );
                }
                spawn_reply_stream(
                    client,
                    serde_json::json!(messages),
                    text.to_string(),
                )
            }
        };
        let reply_rx = stream.rx;
        let reply_cancel = stream.cancel;
        let reply_activity = stream.activity;
        // Bind this whole autonomous reply to the exact
        // Aokie owner that accepted the caller turn. A
        // Companion claim can arrive while the detached
        // model or synchronous TTS loop is running; the
        // global PCM chokepoint cuts audio immediately,
        // while this fence prevents the stale reply from
        // reaching any post-reply tool/hangup path.
        let reply_owner = aokie_owner_for_call(&remote_media, &corr);
        let started = Instant::now();
        let mut stream_outcome: Option<Result<String, String>> = None;
        'pump: loop {
            if !reply_owner_is_current(&remote_media, reply_owner.as_ref())
            {
                eprintln!(
                    "[aokie-plugin] Companion ownership changed mid-reply - yielding every autonomous AI action"
                );
                reply_cancel.store(true, Ordering::Relaxed);
                // Existing downstream policy treats this
                // as an operator-owned cut: no dead-air
                // fallback, abuse termination, or agent
                // hangup may fire from the stale reply.
                operator_ended = true;
                break 'pump;
            }
            // The audio channel is gone (SCO teardown /
            // link loss): abandon the reply NOW — the
            // outer loop's event drain will run the real
            // call teardown. `sr > 0` guards the (never
            // legitimate) reply-started-without-audio
            // case, which the dead-air fail-safe owns.
            if sr > 0 && bt.get_sample_rate() == 0 {
                eprintln!(
                "[aokie-plugin] call audio channel gone mid-reply — abandoning the rest of the reply"
            );
                reply_cancel.store(true, Ordering::Relaxed);
                line_dead = true;
                break 'pump;
            }
            // Urgent controls act immediately — no stream
            // progress required (audit AK-003 + AOK-CTRL-001);
            // everything else parks for the main control loop.
            while let Ok(ctl) = control_rx.try_recv() {
                match ctl {
                    RadioControl::Hangup { op } => {
                        tracker.note_intent(
                        crate::call_session::TerminationIntent::OperatorHangup,
                    );
                        bt.flush_tx_audio();
                        if let Err(e) = bt.hangup() {
                            eprintln!("[aokie-plugin] mid-reply hangup failed: {e}");
                            emit_control_failed(
                                outbox,
                                sink,
                                &tracker,
                                "call.hangup",
                                op.as_deref(),
                                &e,
                            );
                        }
                        operator_ended = true; // record only what played, not "caller interrupted"
                    }
                    RadioControl::Reject { op } => {
                        tracker.note_intent(
                        crate::call_session::TerminationIntent::OperatorReject,
                    );
                        bt.flush_tx_audio();
                        if let Err(e) = bt.reject_call() {
                            eprintln!("[aokie-plugin] mid-reply reject failed: {e}");
                            emit_control_failed(
                                outbox,
                                sink,
                                &tracker,
                                "call.reject",
                                op.as_deref(),
                                &e,
                            );
                        }
                        operator_ended = true;
                    }
                    RadioControl::EndCallerFromCompanion {
                        request,
                        reply,
                    } => {
                        operator_ended = perform_companion_end_caller(
                            request,
                            reply,
                            bt,
                            &mut *tracker,
                            status.as_ref(),
                            &remote_media,
                            &mut *pending_companion_end_caller,
                        );
                    }
                    other => pending_controls.push_back(other),
                }
            }
            if operator_ended {
                reply_cancel.store(true, Ordering::Relaxed);
                break 'pump;
            }
            // Phase 2: listening never stops — drain the mic
            // even BETWEEN sentences (while the LLM is still
            // thinking/streaming). Sustained caller speech in
            // a gap barges the reply exactly like speech over
            // a playing sentence, and everything heard rides
            // the scratchpad into their next turn.
            if barge_in {
                if let Some(a) = aec.as_mut() {
                    let gap_frame = (sr as usize / 100).max(80);
                    while let Some(rxa) = bt.try_recv_audio() {
                        let cleaned = a.process_capture(&rxa.samples);
                        if cleaned.is_empty() {
                            continue;
                        }
                        if scan_barge_frames(
                            &cleaned,
                            gap_frame,
                            CAPTURE_RMS,
                            barge_rms,
                            true,
                            &mut gap_frames,
                            22,
                            &mut overlap_capture,
                            &mut gap_start,
                        ) {
                            barged = true;
                        }
                    }
                    // Idle-trim: while nothing in the buffer is
                    // speech, only a short pre-roll tail matters.
                    if !overlap_has_speech && gap_start.is_none() {
                        let keep = (sr as usize).saturating_mul(2).max(1);
                        if overlap_capture.len() > keep * 2 {
                            overlap_capture
                                .drain(..overlap_capture.len() - keep);
                        }
                    }
                    if barged {
                        eprintln!(
                        "[aokie-plugin] caller spoke between sentences — reply cut"
                    );
                        status.gap_yields.fetch_add(1, Ordering::Relaxed);
                        bt.flush_tx_audio();
                        reply_cancel.store(true, Ordering::Relaxed);
                        break 'pump;
                    }
                }
            }
            match reply_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(ReplyMsg::Sentence(sentence)) => {
                    if let Some(lane) = ctx.rt_lane.as_mut() {
                        if let Some(line) =
                            lane.phase("speaking", Instant::now())
                        {
                            let _ = sink.send_line(&line);
                        }
                    }
                    // Strip any [[END_CALL]] marker BEFORE synthesis so the
                    // caller never hears it and it never lands in the
                    // transcript; its presence arms the post-reply hangup
                    // REQUEST (validated by agent_hangup_verdict below).
                    let (mut spoken_text, had_marker) =
                        strip_end_call_marker(&sentence);
                    holding_appointment_marker |= spoken_text.contains("[[APPOINTMENT");
                    if holding_appointment_marker {
                        // JSON can span sentence chunks (e.g. punctuation in
                        // a service name). No part of the tool payload is speech.
                        continue;
                    }
                    if let Some(replacement) = crate::conversation_policy::guard_sentence(
                        &spoken_text, &ctx.history, chrono::Local::now().date_naive(), advice_allowed,
                    ) {
                        spoken_text = replacement;
                    }
                    if had_marker {
                        hangup_requested = true;
                    }
                    // [[WAIT]] = the model chose intentional silence.
                    if crate::speech_plan::has_wait_marker(&spoken_text) {
                        wait_requested = true;
                    }
                    eprintln!(
                        "[aokie-plugin] agent sentence (+{:?}): {}",
                        t0.elapsed(),
                        content_for_log(&spoken_text)
                    );
                    let mut probe = ControlProbe::new(
                        &control_rx,
                        &mut *pending_controls,
                    );
                    let (aec_ref, brms) = if barge_in {
                        (aec.as_mut(), Some(barge_rms))
                    } else {
                        (None, None)
                    };
                    // The caller started talking while the LLM was
                    // still composing this sentence (gap capture,
                    // ≥250 ms above the speech gate): yield instead
                    // of speaking over them — their words are
                    // already on the scratchpad.
                    if barge_in
                        && gap_start.map_or(false, |s| {
                            overlap_capture.len().saturating_sub(s)
                                >= (sr as usize) / 4
                        })
                    {
                        eprintln!(
                        "[aokie-plugin] caller speaking as the next sentence arrived — yielding to them"
                    );
                        status.gap_yields.fetch_add(1, Ordering::Relaxed);
                        barged = true;
                        reply_cancel.store(true, Ordering::Relaxed);
                        break 'pump;
                    }
                    // The span planner validates/strips any model
                    // markers, slows + digit-expands details, and
                    // applies per-span interrupt policy. The probe
                    // lane rides along: a spoken "wait"/"stop" cuts
                    // the sentence mid-playback.
                    if let Some(lane) = ctx.rt_lane.as_mut() {
                        if let Some(line) = lane.delivery(
                            &spoken_text,
                            "sent_to_sco",
                            Instant::now(),
                        ) {
                            let _ = sink.send_line(&line);
                        }
                    }
                    if spoken_text.contains("[[APPOINTMENT") {
                        continue;
                    }
                    if spoken_text.contains("[[LOOKUP") {
                        // A lookup marker leaking through
                        // the stream (often UNCLOSED — the
                        // sentence chunker cut it before
                        // the ]]) must never be spoken;
                        // the whole-reply detection owns
                        // the verdict (live: the caller
                        // heard "LOOKUP: Are ye looking…").
                        eprintln!(
                        "[aokie-plugin] holding a lookup-marker sentence back from speech"
                    );
                        continue;
                    }
                    if spoken_text.contains("[[ASSISTANCE") {
                        // Typed help is a control verdict,
                        // including malformed/unclosed
                        // small-model variants. Never let
                        // it leak into caller TTS.
                        eprintln!(
                        "[aokie-plugin] holding an assistance-marker sentence back from speech"
                    );
                        continue;
                    }
                    if spoken_text
                        .to_ascii_uppercase()
                        .contains("[[TRANSFER")
                    {
                        // A transfer verdict is control-plane
                        // data, including malformed variants.
                        // Availability is announced only by the
                        // deterministic handler below.
                        eprintln!(
                        "[aokie-plugin] holding a transfer-marker sentence back from speech"
                    );
                        continue;
                    }
                    if spoken_text.contains("[[MANAGER") {
                        // A manager marker (even cut or
                        // unclosed) is a verdict, never
                        // speech - the whole-reply
                        // detection owns it.
                        eprintln!(
                        "[aokie-plugin] holding a manager-marker sentence back from speech"
                    );
                        continue;
                    }
                    if spoken_text.contains("[[ABUSE") {
                        // Phase 1: the abuse flag is a
                        // VERDICT, not speech — never
                        // spoken (even unclosed), and the
                        // rest of the generation is moot:
                        // the deterministic notice below
                        // replaces all model prose.
                        eprintln!(
                        "[aokie-plugin] agent flagged abuse — abandoning the reply for the deterministic handler"
                    );
                        continue;
                    }
                    // The mid-span check compares overlap
                    // against everything SENT so far plus
                    // the sentence about to play.
                    reply_lane.set_bot_context({
                        let mut b = sent_spans.join(" ");
                        b.push(' ');
                        b.push_str(&spoken_text);
                        b
                    });
                    let lane_ref = if barge_in {
                        Some(&mut reply_lane)
                    } else {
                        None
                    };
                    let planned = speak_planned(
                        bt,
                        &synth,
                        &spoken_text,
                        sr,
                        aec_ref,
                        brms,
                        Some(&mut probe),
                        &ctx.pace,
                        protected_max_ms,
                        lane_ref,
                        None,
                    );
                    let out = planned.outcome;
                    // A spoken floor command mid-sentence pauses the
                    // dialogue IMMEDIATELY (the final transcript will
                    // re-apply it — idempotent).
                    if let Some(intent) = out.commanded {
                        ctx.dialogue.apply(intent);
                        if !silence_window.is_zero() {
                            ctx.silence_timer = Some(SilenceTimer::new(
                                silence_window * 3,
                                Instant::now(),
                            ));
                        }
                    }
                    if !planned.text.trim().is_empty() {
                        note_tts_outcome(&status, &out);
                    }
                    reply_dur += out.dur;
                    if out.dur > Duration::ZERO {
                        // The reply is audible: later spans may
                        // yield to ONGOING overlap speech, not
                        // just speech starting inside them.
                        reply_lane.note_audio_played();
                    }
                    // Truthful transcript (AOK-VOICE-001): record
                    // only the spans that audibly PLAYED.
                    if out.dur > Duration::ZERO {
                        if !planned.played_text.is_empty() {
                            spoken.push(planned.played_text.to_string());
                        }
                        if !planned.sent_text.is_empty() {
                            sent_spans.push(planned.sent_text.to_string());
                        }
                    }
                    if !barge_in {
                        let plays_until =
                            (t0 + reply_dur).max(Instant::now());
                        *mute_stt_until =
                            Some(plays_until + Duration::from_millis(600));
                    }
                    // Scratchpad: keep whatever the caller said over
                    // this sentence, even when it didn't barge.
                    if !out.captured_speech.is_empty() {
                        overlap_capture
                            .extend_from_slice(&out.captured_speech);
                        overlap_has_speech = true;
                    }
                    if let Some(action) = probe.action.take() {
                        // Operator hangup/reject landed mid-SENTENCE
                        // (chunk-granular, AOK-CTRL-001).
                        perform_cancel_action(
                            action,
                            bt,
                            &mut *tracker,
                            outbox,
                            sink,
                        );
                        operator_ended = true;
                        reply_cancel.store(true, Ordering::Relaxed);
                        break 'pump;
                    }
                    if out.barged {
                        if out.commanded.is_some() {
                            status
                                .semantic_cuts
                                .fetch_add(1, Ordering::Relaxed);
                        } else {
                            status
                                .barge_cuts
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        bt.flush_tx_audio(); // stop the queued tail now
                        barged = true;
                        reply_cancel.store(true, Ordering::Relaxed);
                        break 'pump; // stop pulling from the LLM
                    }
                    // SENTENCE-BOUNDARY STEERING (the live
                    // scratchpad): the caller said something
                    // SUBSTANTIVE over that sentence — even below
                    // the acoustic barge threshold. Yield here, at
                    // a natural pause, so the next thing spoken
                    // answers THEM instead of finishing a stale
                    // paragraph. Backchannels and echo never steer.
                    let bot_so_far = {
                        // Echo comparison wants the SENT
                        // text (§6.3) — echo returns from
                        // audio that left us, estimated
                        // heard or not.
                        let mut b = sent_spans.join(" ");
                        b.push(' ');
                        b.push_str(&planned.text);
                        b
                    };
                    if let Some(said) =
                        reply_lane.substantive_content(&bot_so_far)
                    {
                        eprintln!(
                        "[aokie-plugin] scratchpad steering: yielding at the sentence boundary to {}",
                        content_for_log(&said)
                    );
                        status
                            .boundary_yields
                            .fetch_add(1, Ordering::Relaxed);
                        bt.flush_tx_audio();
                        barged = true;
                        reply_cancel.store(true, Ordering::Relaxed);
                        break 'pump;
                    }
                }
                Ok(ReplyMsg::Done(res)) => {
                    stream_outcome = Some(res);
                    break 'pump;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Named deadlines (the VOICE-001-deferred
                    // per-read idle deadline lives here): a
                    // reply that stops making progress is
                    // abandoned and takes the dead-air path.
                    let last = *reply_activity.lock().unwrap();
                    if let Some(reason) = reply_deadline_exceeded(
                        &REPLY_DEADLINES,
                        started,
                        last,
                        Instant::now(),
                    ) {
                        reply_cancel.store(true, Ordering::Relaxed);
                        stream_outcome = Some(Err(reason));
                        break 'pump;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    stream_outcome = Some(Err(
                        "the reply worker exited without a result"
                            .to_string(),
                    ));
                    break 'pump;
                }
            }
        }
        // Early exits (barge / operator) have no stream result;
        // their transcript comes from `spoken` via the cut path.
        let outcome = stream_outcome.unwrap_or_else(|| Ok(String::new()));
        // `ReplyMsg::Done` and the final ownership
        // change can cross between two pump polls.
        // Fence that narrow gap before any parsed
        // marker is allowed to drive deterministic
        // post-reply behaviour.
        if !reply_owner_is_current(&remote_media, reply_owner.as_ref()) {
            operator_ended = true;
            reply_cancel.store(true, Ordering::Relaxed);
        }
        if !barge_in {
            // Cover audio still queued after the last chunk synthesized.
            let plays_until = (t0 + reply_dur).max(Instant::now());
            *mute_stt_until = Some(plays_until + Duration::from_millis(800));
        }
        // VOICE-001: set below when this reply attempt left the
        // caller in DEAD AIR — triggers the fail-safe after the match.
        let mut dead_air_cause: Option<String> = None;
        match outcome {
            Ok(full) => {
                appointment_requested = crate::conversation_policy::parse_appointment_marker(&full);
                // Truthful transcript (audit AK-008 + sweep): a
                // reply cut short records what actually PLAYED,
                // annotated with WHY — the full generation
                // includes sentences the caller never heard, and
                // an operator hangup is not a caller interruption.
                let cut = if line_dead {
                    Some(" [call dropped mid-reply]")
                } else if barged {
                    Some(" [caller interrupted]")
                } else if operator_ended {
                    Some(" [ended by the operator]")
                } else {
                    None
                };
                // Marker fallbacks in the FULL generation, in case
                // a stream split hid one from the per-sentence
                // detection above.
                let (_, had) = strip_end_call_marker(&full);
                if had {
                    hangup_requested = true;
                }
                if crate::speech_plan::has_wait_marker(&full) {
                    wait_requested = true;
                }
                // Phase 1: whole-generation abuse-flag
                // fallback (a stream split can hide the
                // marker from the per-sentence check).
                if is_exact_abuse_marker(&full) {
                    abuse_flagged = true;
                }
                // Phase 3: manager change request. A
                // garbled/unclosed marker falls back to
                // the caller's own words - their turn WAS
                // the request.
                if let Some(req) =
                    crate::speech_plan::parse_manager_marker(&full)
                {
                    manager_requested = Some(req);
                } else if full.contains("[[MANAGER") {
                    manager_requested = Some(text.to_string());
                }
                if let Some(q) =
                    crate::speech_plan::parse_lookup_marker(&full)
                {
                    lookup_requested = Some(q);
                } else if full.contains("[[LOOKUP") {
                    // Garbled/unclosed marker (live call
                    // 73325204): the intent is clear even
                    // if the syntax isn't — look up the
                    // caller's own words.
                    lookup_requested = Some(text.to_string());
                } else if looks_like_lookup_announcement(&full) {
                    // The model TOLD the caller it would
                    // check but forgot the marker (call
                    // acadcecc: 'Let me check the calendar
                    // for the 16th of August' TWICE, no
                    // marker, no flow, dead air). The
                    // intent is unambiguous — run the
                    // lookup on the caller's own words.
                    eprintln!(
                    "[aokie-plugin] lookup announcement without a marker — looking up the caller's words"
                );
                    lookup_requested = Some(text.to_string());
                } else if lookup_rounds == 0
                    && caller_asked_for_lookup(&text)
                {
                    // The caller EXPLICITLY asked for a
                    // check ('can you look it up?') and
                    // the reply carried no marker (call
                    // 372836dc: it got a team-deferral).
                    // The subject usually lives in their
                    // PREVIOUS turn - send both.
                    eprintln!(
                    "[aokie-plugin] caller asked for a lookup - running it on their words"
                );
                    let subject = if ctx.prev_caller_text.is_empty() {
                        text.to_string()
                    } else {
                        format!("{} {text}", ctx.prev_caller_text)
                    };
                    lookup_requested = Some(subject);
                } else if lookup_rounds == 0
                    && looks_like_availability_claim(&full)
                    && mentions_a_date(&text)
                {
                    // The model ASSERTED availability
                    // without running the lookup (call
                    // 2c00cac0: 'Monday 10 August looks
                    // open' from thin air). Verify: run
                    // the lookup on the caller's words —
                    // the deterministic answer replaces
                    // the guess.
                    eprintln!(
                    "[aokie-plugin] availability claimed without a lookup — verifying against the calendar"
                );
                    lookup_requested = Some(text.to_string());
                } else if lookup_rounds == 0
                    && looks_like_team_deferral(&full)
                    && mentions_a_date(&text)
                {
                    // The model DEFERRED a dated question
                    // to the team without even trying the
                    // lookup (call c01b7dcf: 'what about
                    // twenty first of August?' → 'I'll
                    // have the team confirm' until the
                    // caller pushed 'can you look it up
                    // please'). Round 0 only: a post-
                    // lookup deferral can be legitimate
                    // (beyond-horizon answers say it).
                    eprintln!(
                    "[aokie-plugin] date question deferred to the team without a lookup — looking up the caller's words"
                );
                    lookup_requested = Some(text.to_string());
                }
                if let Some(question) =
                    crate::speech_plan::parse_assistance_marker(&full)
                {
                    assistance_requested = Some(question);
                } else if full.contains("[[ASSISTANCE") {
                    // An unclosed marker still expresses a
                    // clear tool verdict; use the caller's
                    // bounded turn rather than model prose.
                    assistance_requested = Some(text.to_string());
                }
                if let Some(reason) =
                    crate::speech_plan::parse_transfer_marker(&full)
                {
                    transfer_requested = Some(reason);
                } else if full.to_ascii_uppercase().contains("[[TRANSFER") {
                    // Transfer control is intentionally
                    // strict. Never infer authority from a
                    // garbled, wrapped or overlong marker.
                    malformed_transfer_requested = true;
                }
                // The transcript records what audibly PLAYED
                // (span-planned, marker-free) — never the raw
                // generation, which may carry control markup
                // and sentences the caller never heard.
                let played = spoken.join(" ").trim().to_string();
                let heard = match cut {
                    Some(tag) if !played.is_empty() => {
                        format!("{played}{tag}")
                    }
                    _ => played.clone(),
                };
                // The nudge: keep the interrupted reply's
                // unspoken tail for the NEXT generation.
                if barged && !full.trim().is_empty() {
                    let (clean0, _) = strip_end_call_marker(&full);
                    let clean_full = crate::speech_plan::clean_text(
                        &crate::speech_plan::plan_spans(
                            &clean0,
                            &ctx.pace,
                            protected_max_ms,
                        ),
                    );
                    let played_words = played.split_whitespace().count();
                    let tail: Vec<&str> = clean_full
                        .split_whitespace()
                        .skip(played_words)
                        .collect();
                    if !tail.is_empty() {
                        let mut t = tail.join(" ");
                        t.truncate(240);
                        ctx.last_cut_context = Some(t);
                    }
                }
                // VOICE-001: nothing audible + no barge/operator
                // context = the caller is in DEAD AIR — an empty
                // generation or fully-silent synthesis both count.
                // A dead LINE is not dead air (no channel left to
                // apologise on) — and neither is INTENTIONAL
                // silence: a [[WAIT]] reply means the model chose
                // to leave the caller their thinking room.
                if !line_dead
                    && !wait_requested
                    && !abuse_flagged
                    && manager_requested.is_none()
                    && lookup_requested.is_none()
                    && assistance_requested.is_none()
                    && (appointment_requested.is_none() || appointment_rounds > 0)
                    && transfer_requested.is_none()
                    && !malformed_transfer_requested
                    && lookup_rounds == 0
                    && reply_left_dead_air(
                        reply_dur > Duration::ZERO,
                        barged,
                        operator_ended,
                    )
                {
                    if heard.is_empty() && !empty_retry_done {
                        // One retry for an EMPTY generation
                        // (live call 2c00cac0: Gemma 4 hit
                        // a repetition spiral, emitted an
                        // empty reply, and the fail-safe
                        // hung up on a recoverable hiccup).
                        eprintln!(
                        "[aokie-plugin] empty generation — retrying once before the fail-safe"
                    );
                        ctx.history.push(serde_json::json!({
                        "role": "user",
                        "content": "[SYSTEM NOTE - not the caller speaking] Your previous reply was empty. Answer the caller now in one short sentence.",
                    }));
                        empty_retry_done = true;
                        continue 'reply_rounds;
                    }
                    dead_air_cause = Some(if heard.is_empty() {
                        "the assistant produced an empty reply".to_string()
                    } else {
                        "speech synthesis produced no audio for the whole reply"
                        .to_string()
                    });
                }
                // Truthful transcript (AOK-VOICE-001): an
                // un-cut reply whose synthesis produced no
                // audio AT ALL was never heard — record
                // nothing instead of the full generation.
                if !heard.is_empty() && reply_dur > Duration::ZERO {
                    // AOK-CTRL-001: structured per-turn delivery.
                    let delivery = if line_dead {
                        "error"
                    } else if barged {
                        "interrupted"
                    } else if operator_ended {
                        "operator_ended"
                    } else {
                        "complete"
                    };
                    // The model's OWN history keeps the
                    // lookup marker even though speech
                    // strips it: without this, its context
                    // showed announce-WITHOUT-marker turns
                    // being answered, and in-context
                    // imitation beat the instruction —
                    // later "checks" were announced with
                    // no marker at all (call acadcecc).
                    // The transcript stays marker-free.
                    ctx.consecutive_waits = 0;
                    let hist_content = if let Some(reason) =
                        &transfer_requested
                    {
                        format!("{heard} [[TRANSFER: {reason}]]")
                    } else if let Some(lq) = &lookup_requested {
                        format!("{heard} [[LOOKUP: {lq}]]")
                    } else if let Some(question) = &assistance_requested {
                        format!("{heard} [[ASSISTANCE: {question}]]")
                    } else {
                        heard.clone()
                    };
                    ctx.history.push(
                    serde_json::json!({ "role": "assistant", "content": hist_content }),
                );
                    emit_turn_with_delivery(
                        outbox,
                        sink,
                        &corr,
                        ctx.turn_index,
                        "bot",
                        &heard,
                        Some(delivery),
                        Some(&aokie_core::events::iso8601_ago_ms(
                            t0.elapsed().as_millis() as u64,
                        )),
                    );
                    ctx.turn_index += 1;
                    // Echo guard + replay compare/replay what
                    // was SENT (§6.3) — the flushed-but-echoed
                    // tail must still match; the transcript
                    // above stays the conservative estimate.
                    let sent_full = sent_spans.join(" ").trim().to_string();
                    ctx.last_bot_reply = if sent_full.is_empty() {
                        heard
                    } else {
                        match cut {
                            Some(tag) => format!("{sent_full}{tag}"),
                            None => sent_full.clone(),
                        }
                    };
                    ctx.last_bot_speech = if sent_full.is_empty() {
                        played
                    } else {
                        sent_full
                    };
                } else if !heard.is_empty() {
                    eprintln!(
                    "[aokie-plugin] agent reply produced NO audio (TTS failed) — not recorded as a spoken turn"
                );
                }
                if !barged && overlap_capture.is_empty() {
                    // Nothing was said over us: discard the
                    // residue captured while we replied. With
                    // overlap captured (barge or scratchpad),
                    // KEEP the buffer — it's the caller's turn
                    // in progress and is seeded below.
                    stt_buf.clear();
                    *stt_had_speech = false;
                    *stt_silence = Duration::ZERO;
                } else if barged {
                    eprintln!(
                    "[aokie-plugin] caller barged in â€” reply cut short"
                );
                }
            }
            Err(e) => {
                eprintln!("[aokie-plugin] agent reply failed: {e}");
                // Sentences that already PLAYED before the
                // failure are part of the call — record
                // them (audit AOK-VOICE-002/AOK-LLM-001).
                let heard = spoken.join(" ").trim().to_string();
                if !heard.is_empty() {
                    let heard =
                        format!("{heard} [reply cut short by an error]");
                    ctx.history.push(
                    serde_json::json!({ "role": "assistant", "content": heard }),
                );
                    emit_turn_with_delivery(
                        outbox,
                        sink,
                        &corr,
                        ctx.turn_index,
                        "bot",
                        &heard,
                        Some("error"),
                        Some(&aokie_core::events::iso8601_ago_ms(
                            t0.elapsed().as_millis() as u64,
                        )),
                    );
                    ctx.turn_index += 1;
                    ctx.last_bot_speech = heard
                        .trim_end_matches(" [reply cut short by an error]")
                        .to_string();
                    ctx.last_bot_reply = heard;
                } else if !line_dead
                    && reply_left_dead_air(false, barged, operator_ended)
                {
                    // VOICE-001: total failure — the caller heard
                    // nothing at all. Record it as a definitive
                    // live LLM failure (health degrades; the
                    // PROC-001 probe re-clears on recovery) and
                    // take the fail-safe below. A PARTIAL reply
                    // is transient: the caller heard something,
                    // the next turn may still work.
                    *status.llm_error.lock().unwrap() = Some(format!(
                        "agent reply failed during a live call: {e}"
                    ));
                    dead_air_cause = Some(format!(
                        "the assistant failed to reply ({e})"
                    ));
                }
            }
        }
        if let Some(arguments) = appointment_requested {
            if !line_dead && !operator_ended && !barged
                // Yield to newly captured caller speech before committing a
                // selection that may already have been corrected or withdrawn.
                && overlap_capture.is_empty()
                && reply_owner_is_current(&remote_media, reply_owner.as_ref())
                && appointment_rounds == 0
            {
                appointment_rounds += 1;
                let callers: Vec<String> = ctx.history.iter().rev()
                    .filter(|m| m["role"] == "user")
                    .filter_map(|m| m["content"].as_str())
                    .filter(|s| !s.starts_with("[SYSTEM"))
                    .map(str::to_string).collect();
                let assistants: Vec<String> = ctx.history.iter().rev()
                    .filter(|m| m["role"] == "assistant")
                    .filter_map(|m| m["content"].as_str()).take(3).map(str::to_string).collect();
                let result = arguments.and_then(|args| {
                    crate::realtime_appointment::validate_with_readback(
                        &args, corr, Some((status.last_caller_turn.load(Ordering::Relaxed), text)),
                        &callers, &assistants, chrono::Local::now().date_naive(),
                    )
                }).and_then(|request| {
                    if !ctx.queued_appointments.contains(&request.request_id) {
                        let from = tracker.current().and_then(|c| c.caller_id.as_deref()).unwrap_or("");
                        emit_realtime_appointment_request(outbox, sink, corr, from, &request)?;
                        ctx.queued_appointments.push(request.request_id.clone());
                    }
                    Ok(format!("Appointment REQUEST queued for {} on {} at {}. Its flow will deliver it to the Aokie app; app storage and staff confirmation are not yet acknowledged. Do not queue it again. Tell the caller only that their request was queued for staff confirmation.", request.caller_name, request.date, request.time))
                });
                let note = match result {
                    Ok(note) => note,
                    Err(error) => format!("Nothing was queued or booked: {error}. Ask one short clarification question; do not emit another appointment marker this turn."),
                };
                ctx.history.push(serde_json::json!({"role":"user","content":format!("[SYSTEM APPOINTMENT RESULT - not caller speech] {note}")}));
                continue 'reply_rounds;
            }
        }
        // AK-008 + scratchpad: EVERYTHING the caller said over
        // the reply was captured (echo-cancelled) — barge or
        // not. Prepend it to the utterance buffer so the STT
        // hears the WHOLE turn ("zero four two one…", a quick
        // "wait" that never tripped the barge, a "yeah" spoken
        // over a sentence). Overlapped speech is never lost.
        if !overlap_capture.is_empty() {
            if !barged {
                eprintln!(
                "[aokie-plugin] scratchpad: captured {}ms of overlapped caller speech (no barge) — transcribing",
                overlap_capture.len() * 1000 / (sr.max(1) as usize)
            );
            }
            let mut seeded =
                crate::voice::to_f32_16k(&overlap_capture, sr as u32);
            seeded.extend_from_slice(&stt_buf);
            *stt_buf = seeded;
            *stt_had_speech = true;
            *stt_silence = Duration::ZERO;
            *turn_overlapped = true;
            *turn_overlap_at = Some(overlap_backdate(
                overlap_capture.len(),
                sr as usize,
                t0.elapsed(),
            ));
        }
        // ── Phase 3: MANAGER CHANGE REQUEST ─────────
        // The marker never speaks; everything from here
        // is deterministic. Not the manager -> honest
        // refusal. No PIN configured -> say so. Verified
        // already -> execute. Otherwise stash the request
        // and ask for the PIN (the next caller turn is
        // consumed by the gate, redacted everywhere).
        if let Some(req) = manager_requested.take() {
            // The model sometimes copies the instruction's
            // placeholder into the marker (seen live:
            // 'The request in one clear sentence with full
            // dates') — fall back to the caller's own words.
            let req = if req
                .to_ascii_lowercase()
                .contains("one clear sentence")
            {
                text.to_string()
            } else {
                req
            };
            if !line_dead
                && !operator_ended
                && reply_owner_is_current(
                    &remote_media,
                    reply_owner.as_ref(),
                )
                && bt.get_sample_rate() > 0
            {
                let is_mgr = tracker.current().is_some_and(|s| {
                    !s.outbound
                        && screen_policy.is_manager(s.caller_id.as_deref())
                });
                let pin_set = !crate::speech_plan::spoken_digits(
                    &std::env::var("AOKIE_MANAGER_PIN").unwrap_or_default(),
                )
                .is_empty();
                // Inline PIN (live call 085ce239): the request
                // often arrives WITH the PIN in one sentence
                // ("my manager pin is one two three four - what's
                // booked?"). When the triggering turn carries
                // exactly a full-length digit string, judge it as
                // a throttled attempt instead of re-asking for
                // what was already said. That turn was recorded
                // normally BEFORE any gate armed (the caller
                // volunteered it mid-sentence); only gate-prompted
                // turns are redacted. A failed inline match falls
                // through to the normal PIN prompt.
                if is_mgr && pin_set && !ctx.manager_gate.verified {
                    let expected = crate::speech_plan::spoken_digits(
                        &std::env::var("AOKIE_MANAGER_PIN")
                            .unwrap_or_default(),
                    );
                    let inline = crate::speech_plan::spoken_digits(&text);
                    if !expected.is_empty()
                        && inline.len() == expected.len()
                        && crate::manager_auth::verify(
                            data_dir, &expected, &inline,
                        ) == crate::manager_auth::Decision::Verified
                    {
                        ctx.manager_gate.verified = true;
                        eprintln!(
                            "[aokie-plugin] manager PIN verified (inline with the request)"
                        );
                    }
                }
                if !is_mgr {
                    eprintln!(
                    "[aokie-plugin] manager marker on a NON-manager call - refused"
                );
                    speak_manager_line(
                        bt,
                        &synth,
                        outbox,
                        sink,
                        &status,
                        &corr,
                        &mut ctx.turn_index,
                        &mut ctx.history,
                        MANAGER_DENIED_LINE,
                    );
                } else if !pin_set {
                    speak_manager_line(
                        bt,
                        &synth,
                        outbox,
                        sink,
                        &status,
                        &corr,
                        &mut ctx.turn_index,
                        &mut ctx.history,
                        NO_PIN_LINE,
                    );
                } else if ctx.manager_gate.verified {
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
                    if !reply_owner_is_current(
                        &remote_media,
                        reply_owner.as_ref(),
                    ) {
                        eprintln!(
                            "[aokie-plugin] manager action skipped: Companion ownership changed"
                        );
                        break 'reply_rounds;
                    }
                    let mgr_from = tracker
                        .current()
                        .and_then(|s| s.caller_id.clone())
                        .unwrap_or_default();
                    let expected_owner = reply_owner.as_ref().expect(
                        "manager entry checked exact Aokie ownership",
                    );
                    let Some(outcome) = manager_plan_and_execute(
                        &host_rpc,
                        sink,
                        outbox,
                        &mut *screen_policy,
                        &status,
                        &remote_media,
                        expected_owner,
                        &corr,
                        &mgr_from,
                        &req,
                    ) else {
                        eprintln!(
                            "[aokie-plugin] manager action skipped: Companion ownership changed while planning"
                        );
                        break 'reply_rounds;
                    };
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
                } else if crate::manager_auth::lockout_remaining_secs(
                    data_dir,
                ) > 0
                {
                    ctx.manager_gate.pending = None;
                    ctx.manager_gate.awaiting_pin = false;
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
                } else {
                    ctx.manager_gate.pending = Some(req);
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
                        PIN_PROMPT_LINE,
                    );
                }
            }
            break 'reply_rounds;
        }
        // ── Phase 1: ABUSE TERMINATION (deterministic) ──
        // The model only FLAGGED ([[ABUSE]]); everything
        // from here is fixed code: speak the notice, block
        // the number (policy live + env now, persisted via
        // the connector drain), hang up with the ghost-turn
        // latch. Never the LLM's job — and never spoken
        // prose from it either.
        if abuse_flagged && !line_dead && !operator_ended {
            if !reply_owner_is_current(&remote_media, reply_owner.as_ref())
            {
                eprintln!(
                    "[aokie-plugin] abuse termination skipped: Companion ownership changed"
                );
                break 'reply_rounds;
            }
            eprintln!(
            "[aokie-plugin] abusive caller flagged — speaking the notice and ending the call (Phase 1 policy)"
        );
            // Cut anything still queued so the notice is
            // the only thing the caller hears.
            bt.flush_tx_audio();
            let ab_t0 = Instant::now();
            let out = tts_speak(
                bt, &synth, ABUSE_LINE, sr, None, None, None, 1.0, None,
                None, None,
            );
            note_tts_outcome(&status, &out);
            if out.dur > Duration::ZERO {
                // Truthful transcript: the notice WAS heard.
                emit_turn_with_delivery(
                    outbox,
                    sink,
                    &corr,
                    ctx.turn_index,
                    "bot",
                    ABUSE_LINE,
                    Some("complete"),
                    Some(&aokie_core::events::iso8601_ago_ms(
                        ab_t0.elapsed().as_millis() as u64,
                    )),
                );
                ctx.turn_index += 1;
                let wait =
                    playout_drain_wait(ab_t0, out.dur, Instant::now());
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            } else {
                // TTS broken: the hangup still happens —
                // ending the call IS the policy outcome.
                eprintln!(
                "[aokie-plugin] abuse notice produced no audio — ending the call without it"
            );
            }
            if !reply_owner_is_current(&remote_media, reply_owner.as_ref())
            {
                eprintln!(
                    "[aokie-plugin] abuse policy side effects skipped: Companion ownership changed"
                );
                break 'reply_rounds;
            }
            let expected = reply_owner
                .as_ref()
                .expect("abuse entry checked the Aokie owner fence");
            match remote_media.with_aokie_owner(expected, || {
                // Keep the optional policy mutation and
                // physical CHUP in one exact-owner
                // transaction. A takeover either wins
                // before both, or after both.
                if screen_policy.auto_block_abuse {
                    let num = tracker
                        .current()
                        .and_then(|s| s.caller_id.clone())
                        .unwrap_or_default();
                    if screen_policy.block_number(&num) {
                        let mut env_list =
                            std::env::var("AOKIE_BLOCKED_NUMBERS")
                                .unwrap_or_default();
                        if !env_list.trim().is_empty() {
                            env_list.push(',');
                        }
                        env_list.push_str(num.trim());
                        std::env::set_var(
                            "AOKIE_BLOCKED_NUMBERS",
                            env_list,
                        );
                        status
                            .pending_blocked_numbers
                            .lock()
                            .unwrap()
                            .push(num);
                        eprintln!(
                            "[aokie-plugin] abusive caller auto-blocked (live now; persisted at the next host poll) — unblock via the console's Call screening card"
                        );
                    } else if num.trim().is_empty() {
                        eprintln!(
                            "[aokie-plugin] abuse auto-block skipped — caller id withheld/unknown"
                        );
                    }
                }
                tracker.note_intent(
                    crate::call_session::TerminationIntent::AgentTerminateAbuse,
                );
                bt.hangup()
            }) {
                Ok(Ok(())) => {
                    // Ghost-turn latch: words captured
                    // during the notice must never mint an
                    // answered post-hangup turn.
                    ctx.agent_hung_up = true;
                    eprintln!(
                        "[aokie-plugin] abuse termination complete (AT+CHUP)"
                    );
                }
                Ok(Err(e)) => {
                    eprintln!(
                        "[aokie-plugin] abuse-termination hangup failed: {e}"
                    );
                    emit_control_failed(
                        outbox,
                        sink,
                        &tracker,
                        "agent.abuse_hangup",
                        None,
                        &e,
                    );
                }
                Err(reason) => {
                    eprintln!(
                        "[aokie-plugin] abuse termination skipped: {reason}"
                    );
                }
            }
            break 'reply_rounds;
        }
        // VOICE-001 fail-safe: the caller asked something and heard
        // NOTHING — the responder is broken mid-call. Never leave
        // them in dead air: apologise with the canned line
        // (best-effort — TTS may be the broken half) and end the
        // call cleanly. The hangup happens EVEN IF the fallback
        // itself is silent: ending the call IS the safe outcome.
        let mut ended_by_failsafe = false;
        if dead_air_cause.is_some()
            && !reply_owner_is_current(&remote_media, reply_owner.as_ref())
        {
            eprintln!(
                "[aokie-plugin] responder fail-safe skipped: Companion ownership changed"
            );
            dead_air_cause = None;
        }
        if let Some(cause) = dead_air_cause {
            eprintln!(
            "[aokie-plugin] responder failed mid-call ({cause}) — speaking the fallback line and ending the call (VOICE-001)"
        );
            let fb_t0 = Instant::now();
            // The fail-safe stays maximally simple: plain rate,
            // no planning, no barge monitoring.
            let out = tts_speak(
                bt,
                &synth,
                FALLBACK_LINE,
                sr,
                None,
                None,
                None,
                1.0,
                None,
                None,
                None,
            );
            note_tts_outcome(&status, &out);
            if out.dur > Duration::ZERO {
                // Truthful transcript: the apology WAS heard.
                emit_turn_with_delivery(
                    outbox,
                    sink,
                    &corr,
                    ctx.turn_index,
                    "bot",
                    FALLBACK_LINE,
                    Some("complete"),
                    Some(&aokie_core::events::iso8601_ago_ms(
                        fb_t0.elapsed().as_millis() as u64,
                    )),
                );
                ctx.turn_index += 1;
                // AOK-CTRL-001: drain the QUEUED apology before
                // CHUP — computed from what was actually queued
                // (the blind 900 ms cut a long apology short).
                let wait =
                    playout_drain_wait(fb_t0, out.dur, Instant::now());
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            } else {
                eprintln!(
                "[aokie-plugin] fallback line also produced no audio — hanging up without it"
            );
            }
            let expected = reply_owner
                .as_ref()
                .expect("fail-safe entry checked the Aokie owner fence");
            match remote_media.with_aokie_owner(expected, || {
                tracker.note_intent(
                    crate::call_session::TerminationIntent::AgentHangup,
                );
                bt.hangup()
            }) {
                Ok(Ok(())) => {
                    ctx.agent_hung_up = true;
                    eprintln!(
                        "[aokie-plugin] fail-safe hangup complete (AT+CHUP)"
                    );
                }
                Ok(Err(e)) => {
                    eprintln!("[aokie-plugin] fail-safe hangup failed: {e}")
                }
                Err(reason) => {
                    eprintln!(
                        "[aokie-plugin] fail-safe hangup skipped: {reason}"
                    );
                }
            }
            ended_by_failsafe = true;
        }
        // Agent-initiated hangup (AOK-CTRL-001): the end-call
        // marker is only a REQUEST — the pure policy validates
        // it against what actually happened (barge, operator
        // action, fail-safe, farewell audibility) and computes
        // the farewell's remaining playout drain, replacing the
        // old fixed-delay hangup that could cut a goodbye short
        // or fire before one was proven audible.
        let verdict = if line_dead {
            // No link left to hang up on — the outer loop's
            // event drain runs the real teardown.
            HangupVerdict::Skip("the call's audio link is gone")
        } else {
            let aokie_owner_current =
                reply_owner_is_current(&remote_media, reply_owner.as_ref());
            agent_hangup_verdict(
                hangup_requested,
                aokie_owner_current,
                barged,
                operator_ended,
                ended_by_failsafe,
                reply_dur > Duration::ZERO,
                // Any question in what actually PLAYED means
                // the model expects an answer — never hang up
                // on the caller mid-question.
                spoken.iter().any(|s| s.contains('?')),
                t0,
                reply_dur,
                Instant::now(),
            )
        };
        match verdict {
            HangupVerdict::Proceed { wait } => {
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
                let Some(expected) = reply_owner.as_ref() else {
                    eprintln!(
                        "[aokie-plugin] agent hangup skipped: no Aokie caller-owner fence"
                    );
                    break 'reply_rounds;
                };
                match remote_media.with_aokie_owner(expected, || {
                    tracker.note_intent(
                        crate::call_session::TerminationIntent::AgentHangup,
                    );
                    bt.hangup()
                }) {
                    Ok(Ok(())) => {
                        ctx.agent_hung_up = true;
                        eprintln!(
                            "[aokie-plugin] agent finalized the call — hung up (AT+CHUP)"
                        );
                    }
                    Ok(Err(e)) => {
                        eprintln!(
                            "[aokie-plugin] agent hangup failed: {e}"
                        );
                        emit_control_failed(
                            outbox,
                            sink,
                            &tracker,
                            "agent.hangup",
                            None,
                            &e,
                        );
                    }
                    Err(reason) => {
                        eprintln!(
                            "[aokie-plugin] agent hangup skipped: {reason}"
                        );
                    }
                }
            }
            HangupVerdict::Skip(reason) => {
                if hangup_requested {
                    eprintln!(
                    "[aokie-plugin] agent hangup request skipped: {reason}"
                );
                }
            }
        }
        // AOK-CTRL-001: a finished reply attempt (audible or
        // not) is conversational activity — the max-silence
        // window measures from here.
        if let Some(t) = ctx.silence_timer.as_mut() {
            t.note_activity(Instant::now());
        }
        // Realtime phase: the reply is done — the floor is
        // the caller's again (or the held pause).
        if let Some(lane) = ctx.rt_lane.as_mut() {
            let ph = if ctx.dialogue.is_paused() {
                "paused"
            } else {
                "listening"
            };
            if let Some(line) = lane.phase(ph, Instant::now()) {
                let _ = sink.send_line(&line);
            }
        }
        // The model chose intentional silence ([[WAIT]]): the
        // floor stays with the caller — hold the pause state
        // and stretch the silence watchdog so a deliberately
        // quiet caller isn't nagged mid-thought.
        if wait_requested
            && !barged
            && !operator_ended
            && !line_dead
            && reply_owner_is_current(&remote_media, reply_owner.as_ref())
            && ctx.consecutive_waits >= 1
            && !wait_regen_done
            && lookup_rounds == 0
        {
            // Streak breaker: one silent wait is respect,
            // two is a dead line. Regenerate ONCE with the
            // instruction spelled out; if it still waits,
            // accept it (never loop).
            eprintln!(
            "[aokie-plugin] second consecutive [[WAIT]] rejected — regenerating with a speak-now note"
        );
            ctx.history.push(serde_json::json!({
            "role": "user",
            "content": "[SYSTEM NOTE - not the caller speaking] You already waited silently once. The caller has spoken again - [[WAIT]] is not available for this reply. Answer them now in one short sentence.",
        }));
            // (no need to clear wait_requested: the next round re-declares it)
            wait_regen_done = true;
            continue 'reply_rounds;
        }
        if wait_requested
            && !barged
            && !operator_ended
            && !line_dead
            && reply_owner_is_current(&remote_media, reply_owner.as_ref())
        {
            ctx.consecutive_waits += 1;
            eprintln!(
            "[aokie-plugin] agent chose to wait silently ([[WAIT]]) — the caller has the floor (streak {})",
        ctx.consecutive_waits
        );
            ctx.dialogue.apply(crate::duplex::CallerIntent::Pause);
            if !silence_window.is_zero() {
                ctx.silence_timer = Some(SilenceTimer::new(
                    silence_window * 3,
                    Instant::now(),
                ));
            }
        }

        // ── TYPED COMPANION ASSISTANCE ─────────────────
        // The model may ask a short question, but the
        // signed server policy chooses recipients and the
        // plugin's exact call/owner/revision fence decides
        // whether an answer can ever be consumed.
        if transfer_requested.is_some()
            || malformed_transfer_requested
            || assistance_requested.is_some()
        {
            let (intent, question) =
                if let Some(reason) = transfer_requested.take() {
                    (crate::assistance::AssistanceIntent::Transfer, reason)
                } else if malformed_transfer_requested {
                    (
                        crate::assistance::AssistanceIntent::Transfer,
                        String::new(),
                    )
                } else {
                    (
                        crate::assistance::AssistanceIntent::Advice,
                        assistance_requested.take().unwrap_or_default(),
                    )
                };
            // A tool verdict is exclusive. If a small
            // model emitted multiple markers, typed help
            // wins and no unrelated lookup runs.
            let current_call = tracker.call_id().map(str::to_string);
            let remote = remote_media.snapshot();
            let switchboard_revision =
                status.switchboard_revision.load(Ordering::Relaxed);
            let allowed = current_call.as_deref()
                == remote.call_id.as_deref()
                && remote.call_epoch > 0
                && remote.consent.assistance_enabled
                && (intent
                    != crate::assistance::AssistanceIntent::Transfer
                    || remote.consent.takeover_enabled)
                && tracker.current().is_some_and(|call| call.is_active());
            let mut line = assistance_request_initial_line(
                intent,
                malformed_transfer_requested,
            );
            if ctx.pending_assistance.is_some() {
                line = ASSISTANCE_PENDING_LINE;
            } else if allowed
                && !malformed_transfer_requested
                && !operator_ended
                && reply_owner_is_current(
                    &remote_media,
                    reply_owner.as_ref(),
                )
            {
                let fence = crate::assistance::AssistanceCallFence {
                    call_id: current_call.unwrap_or_default(),
                    call_epoch: remote.call_epoch,
                    owner_epoch: remote.owner_epoch,
                    switchboard_revision,
                    remote_revision: remote.remote_revision,
                };
                let expected = reply_owner.as_ref().expect(
                    "assistance entry checked the Aokie owner fence",
                );
                match remote_media.with_aokie_owner(expected, || {
                    if intent
                        == crate::assistance::AssistanceIntent::Transfer
                    {
                        crate::assistance::global().request_transfer(
                            fence.clone(),
                            &question,
                            None,
                            TRANSFER_REQUEST_TTL_SECONDS,
                        )
                    } else {
                        crate::assistance::global().request(
                            fence.clone(),
                            &question,
                            None,
                            ASSISTANCE_REQUEST_TTL_SECONDS,
                        )
                    }
                }) {
                Ok(Ok(request_id)) => {
                    eprintln!(
                        "[aokie-plugin] typed {} requested for the current call",
                        if intent == crate::assistance::AssistanceIntent::Transfer {
                            "owner transfer"
                        } else {
                            "assistance"
                        }
                    );
                    let (audit, requested_event) =
                        AssistanceAuditLifecycle::opened(
                            &request_id,
                            &fence.call_id,
                        );
                    emit(outbox, sink, requested_event);
                    ctx.pending_assistance = Some(PendingAssistanceCall {
                        request_id,
                        fence,
                        intent,
                        audit,
                    });
                    line = if intent
                        == crate::assistance::AssistanceIntent::Transfer
                    {
                        TRANSFER_CHECKING_LINE
                    } else {
                        ASSISTANCE_FILLER_LINE
                    };
                }
                Ok(Err(error)) => eprintln!(
                    "[aokie-plugin] typed assistance request refused: {error}"
                ),
                Err(reason) => eprintln!(
                    "[aokie-plugin] typed assistance request skipped: {reason}"
                ),
            }
            } else {
                if malformed_transfer_requested {
                    eprintln!(
                        "[aokie-plugin] malformed transfer verdict rejected"
                    );
                } else {
                    eprintln!(
                        "[aokie-plugin] typed assistance unavailable: consent or call fence is not current"
                    );
                }
            }

            if !line_dead
                && !operator_ended
                && !barged
                && reply_owner_is_current(
                    &remote_media,
                    reply_owner.as_ref(),
                )
                && bt.get_sample_rate() > 0
            {
                let sr_now = bt.get_sample_rate();
                let mut probe =
                    ControlProbe::new(&control_rx, &mut *pending_controls);
                let (aec_ref, brms) = if barge_in {
                    (aec.as_mut(), Some(barge_rms))
                } else {
                    (None, None)
                };
                let started = Instant::now();
                let planned = speak_planned(
                    bt,
                    &synth,
                    line,
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
                    perform_cancel_action(
                        action,
                        bt,
                        &mut *tracker,
                        outbox,
                        sink,
                    );
                }
                if planned.outcome.dur > Duration::ZERO
                    && !planned.played_text.is_empty()
                {
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
                    ctx.last_bot_reply = planned.sent_text.to_string();
                    ctx.last_bot_speech = planned.sent_text;
                }
            }
            break 'reply_rounds;
        }

        // ── LIVE LOOKUP ROUND (guide P1-16) ─────────────
        // Run the read-only host flow and regenerate with
        // its result. The audible filler plays FIRST (the
        // flow takes 1-4 s); every failure injects an
        // explicit UNAVAILABLE so the model answers from
        // its notes instead of guessing.
        let mut speak_handoff = false;
        if lookup_rounds > 0
            && reply_dur == Duration::ZERO
            && !line_dead
            && !operator_ended
            && reply_owner_is_current(&remote_media, reply_owner.as_ref())
            && !wait_requested
            && !barged
        {
            // The post-lookup round produced NOTHING
            // audible: an empty regeneration must never
            // end in the tech-difficulties apology (live
            // call fefa0e8d — the lookup itself had
            // SUCCEEDED).
            eprintln!(
            "[aokie-plugin] post-lookup round was silent — speaking the handoff line"
        );
            speak_handoff = true;
        }
        if let Some(q) = lookup_requested.take() {
            if lookup_rounds > 0
                && !line_dead
                && !operator_ended
                && reply_owner_is_current(
                    &remote_media,
                    reply_owner.as_ref(),
                )
                && bt.get_sample_rate() > 0
            {
                // The model wants a SECOND lookup after
                // already receiving one: speak an honest
                // handoff instead of silently regenerating
                // (or worse, saying nothing).
                eprintln!(
                "[aokie-plugin] repeat lookup request — speaking the handoff line"
            );
                speak_handoff = true;
            }
            if lookup_rounds == 0
                && !line_dead
                && !operator_ended
                && reply_owner_is_current(
                    &remote_media,
                    reply_owner.as_ref(),
                )
                && !barged
                && bt.get_sample_rate() > 0
            {
                lookup_rounds += 1;
                eprintln!(
                    "[aokie-plugin] agent lookup: {}",
                    content_for_log(&q)
                );
                // Fire the flow BEFORE speaking the filler:
                // the desktop runs it WHILE the filler
                // plays, so the caller's mic-dark window
                // shrinks to whatever remains after ~2.5 s
                // of audible speech (usually nothing).
                let lu_from = tracker
                    .current()
                    .and_then(|s| s.caller_id.clone())
                    .unwrap_or_default();
                let lu_manager = manager_access_allowed(
                    ctx.manager_gate.verified,
                    screen_policy.is_manager(Some(lu_from.as_str())),
                );
                let pending_lookup = begin_business_lookup(
                    &host_rpc, sink, &q, &corr, &lu_from, lu_manager,
                );
                let sr_now = bt.get_sample_rate();
                let mut fprobe =
                    ControlProbe::new(&control_rx, &mut *pending_controls);
                let (aec_ref, brms) = if barge_in {
                    (aec.as_mut(), Some(barge_rms))
                } else {
                    (None, None)
                };
                let _ = speak_planned(
                    bt,
                    &synth,
                    LOOKUP_FILLER_LINE,
                    sr_now,
                    aec_ref,
                    brms,
                    Some(&mut fprobe),
                    &ctx.pace,
                    protected_max_ms,
                    None,
                    None,
                );
                if let Some(action) = fprobe.action.take() {
                    perform_cancel_action(
                        action,
                        bt,
                        &mut *tracker,
                        outbox,
                        sink,
                    );
                    break 'reply_rounds;
                }
                let (result_text, lookup_spoken) =
                    finish_business_lookup(pending_lookup);
                if !reply_owner_is_current(
                    &remote_media,
                    reply_owner.as_ref(),
                ) {
                    eprintln!(
                        "[aokie-plugin] lookup result discarded: Companion ownership changed"
                    );
                    break 'reply_rounds;
                }
                eprintln!(
                    "[aokie-plugin] lookup result: [{} chars], spoken: {}",
                    result_text.chars().count(),
                    lookup_spoken.is_some(),
                );
                // USER role, clearly framed: a TRAILING
                // system message renders badly in many
                // chat templates — the first successful
                // live lookup regenerated to EMPTY and the
                // caller got the tech-difficulties apology
                // (call fefa0e8d).
                ctx.history.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "[SYSTEM LOOKUP RESULT - this is data, not the caller speaking]\n{result_text}\nAnswer the caller's question (\"{q}\") now in one or two short spoken sentences using ONLY this result and your notes. If the result has a DIRECT ANSWER line for the date in question, that line IS the answer - speak it; never say a date is outside your window when a DIRECT ANSWER covers it. Otherwise TRUST the result's own rules about dates that are not listed - an unlisted date inside its window IS open. Only defer to the team when the result itself says to."
                ),
            }));
                if let Some(say) = lookup_spoken {
                    // The flow composed the answer FROM
                    // RECORDS — speak it verbatim and skip
                    // the LLM round entirely: two live
                    // calls proved the model overrides a
                    // correct DIRECT ANSWER with its own
                    // persona-window reasoning. The digest
                    // stays in history so follow-up turns
                    // ("book it then") are grounded.
                    eprintln!(
                    "[aokie-plugin] speaking flow-composed lookup answer verbatim"
                );
                    let sr_say = bt.get_sample_rate();
                    let mut sprobe = ControlProbe::new(
                        &control_rx,
                        &mut *pending_controls,
                    );
                    let (aec_s, brms_s) = if barge_in {
                        (aec.as_mut(), Some(barge_rms))
                    } else {
                        (None, None)
                    };
                    let s_started = Instant::now();
                    let planned = speak_planned(
                        bt,
                        &synth,
                        &say,
                        sr_say,
                        aec_s,
                        brms_s,
                        Some(&mut sprobe),
                        &ctx.pace,
                        protected_max_ms,
                        None,
                        None,
                    );
                    if let Some(action) = sprobe.action.take() {
                        perform_cancel_action(
                            action,
                            bt,
                            &mut *tracker,
                            outbox,
                            sink,
                        );
                    }
                    if planned.outcome.dur > Duration::ZERO
                        && !planned.played_text.is_empty()
                    {
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
                                s_started.elapsed().as_millis() as u64,
                            )),
                        );
                        ctx.turn_index += 1;
                    }
                    break 'reply_rounds;
                }
                continue 'reply_rounds;
            }
        }
        if speak_handoff
            && !line_dead
            && !operator_ended
            && reply_owner_is_current(&remote_media, reply_owner.as_ref())
            && bt.get_sample_rate() > 0
        {
            let sr_now = bt.get_sample_rate();
            let mut hprobe =
                ControlProbe::new(&control_rx, &mut *pending_controls);
            let (aec_ref, brms) = if barge_in {
                (aec.as_mut(), Some(barge_rms))
            } else {
                (None, None)
            };
            let h_started = Instant::now();
            let planned = speak_planned(
                bt,
                &synth,
                LOOKUP_HANDOFF_LINE,
                sr_now,
                aec_ref,
                brms,
                Some(&mut hprobe),
                &ctx.pace,
                protected_max_ms,
                None,
                None,
            );
            if let Some(action) = hprobe.action.take() {
                perform_cancel_action(
                    action,
                    bt,
                    &mut *tracker,
                    outbox,
                    sink,
                );
            }
            if planned.outcome.dur > Duration::ZERO
                && !planned.played_text.is_empty()
            {
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
                    Some("complete"),
                    Some(&aokie_core::events::iso8601_ago_ms(
                        h_started.elapsed().as_millis() as u64,
                    )),
                );
                ctx.turn_index += 1;
            }
        }
        break 'reply_rounds;
    }
}
