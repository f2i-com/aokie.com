//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `run_auto_hold_juggle`.

#[allow(unused_imports)]
use super::*;

#[cfg(all(target_os = "windows", feature = "voice"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_auto_hold_juggle(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    status: &Arc<RadioStatus>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    synth: &crate::synth::SynthHandle,
    stt_current_gen: &Arc<AtomicU64>,
    probe_result_rx: &std::sync::mpsc::Receiver<SttResult>,
    stt_buf: &mut Vec<f32>,
    stt_had_speech: &mut bool,
    stt_silence: &mut std::time::Duration,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    screen_policy: &crate::screen::ScreenPolicy,
    auto_hold: bool,
    auto_hold_done_for: &mut Option<String>,
    promote_greet_for: &mut Option<String>,
    resume_line_for: &mut Option<(String, std::time::Instant)>,
    aec: &mut Option<crate::aec::EchoCanceller>,
    protected_max_ms: u32,
    tracker: &mut crate::call_session::SessionTracker,
    voice_call_gen: &mut u64,
    realtime_lane: &mut Option<RealtimeCallLane>,
    realtime_resume_call: &mut Option<String>,
    ctx: &mut CallVoiceContext,
    parked: &mut Option<(crate::call_session::CallSession, CallVoiceContext)>,
    pending_ctx_restore: &mut Option<CallVoiceContext>,
    prev_call_held: &mut u64,
) {
    #[cfg(feature = "voice")]
    if auto_hold
        && !remote_media.radio_reserved()
        && parked.is_none()
        // The juggle also serves Realtime-owned primaries now: their
        // exclusive session is DISPOSED before the first announcement
        // (the WebSocket is deliberately disposable, same policy as the
        // parked-caller path) so the fixed local announcements own the
        // line and the second caller's audio can never reach the
        // provider; a fresh session with the resume greeting takes over
        // when the primary is restored after the swap.
        && *voice_call_gen == tracker.generation()
        && auto_hold_done_for.as_deref()
            != status
                .waiting_call
                .lock()
                .unwrap()
                .as_ref()
                .map(|w| w.call_id.as_str())
    {
        let primary_active = tracker
            .current()
            .is_some_and(|s| s.is_active() && !s.outbound);
        let auto_hold_switch = remote_media
            .aokie_switch_fence()
            .filter(|fence| tracker.call_id() == Some(fence.owner.call_id.as_str()));
        let busy = ctx.manager_gate.awaiting_pin || ctx.agent_hung_up;
        // ⚠️ Bind the snapshot BEFORE the if-let. In edition 2021 an
        // if-let scrutinee's temporaries — here the waiting_call mutex
        // GUARD — live to the end of the whole if-let, and the juggle
        // body re-locks waiting_call on every settle iteration
        // (settle_swap → swap_snapshot + handle_event). A guard held
        // across that is a same-thread deadlock: live incident
        // 2026-07-15, the radio froze at the first settle of the first
        // juggle (WATCHDOG "stalled in phase 1" forever, line dead).
        let waiting_snapshot = status.waiting_call.lock().unwrap().clone();
        if let Some(w) = waiting_snapshot {
            if primary_active && !busy && auto_hold_switch.is_some() {
                *auto_hold_done_for = Some(w.call_id.clone());
                // Screened callers (blocked list / accept-filter miss /
                // withheld id with rejectPrivate) NEVER interrupt a live
                // conversation: no juggle, no queue spot — they ring out
                // at the network and their give-up leaves no missed-call
                // record (user policy 2026-07-15). A call from them on an
                // IDLE line still gets the normal answer-then-screen flow.
                let knock_screen = screen_policy.verdict(if w.from.is_empty() {
                    None
                } else {
                    Some(w.from.as_str())
                });
                let sr = bt.get_sample_rate();
                if let Some(reason) = knock_screen {
                    eprintln!(
                        "[aokie-plugin] AUTO-HOLD: knocking caller is screened ({reason}) — no juggle, no queue spot; they ring out and leave no missed-call record"
                    );
                } else if sr == 0 {
                    eprintln!(
                        "[aokie-plugin] AUTO-HOLD: no audio path (sr=0) — leaving {} in observe state",
                        w.call_id
                    );
                } else {
                    eprintln!(
                        "[aokie-plugin] AUTO-HOLD: second caller {} knocking — telling the primary to hold",
                        w.call_id
                    );
                    // 0) A Realtime-owned primary: dispose its exclusive
                    // session BEFORE any announcement or swap. The local
                    // hold ceremony then owns the audio lane outright and
                    // the knocker's audio can never reach the provider; a
                    // fresh session (resume greeting) takes over once the
                    // primary is restored.
                    if realtime_lane
                        .as_ref()
                        .is_some_and(|lane| lane.begun)
                    {
                        if let Some(lane) = realtime_lane.take() {
                            if let Some(item_id) = lane.output_pacer.active_item() {
                                let _ = lane.session.cancel_output(
                                    item_id,
                                    lane.output_pacer.audible_played_ms(Instant::now()),
                                );
                            }
                            lane.session.stop("hold juggle — primary parked");
                            bt.flush_tx_audio();
                            *aec = None;
                            status.realtime_ready.store(false, Ordering::Relaxed);
                            eprintln!(
                                "[aokie-plugin] AUTO-HOLD: realtime session for {} disposed for the juggle; it resumes fresh after the swap",
                                lane.call_id
                            );
                            *realtime_resume_call = Some(lane.call_id.clone());
                        }
                    }
                    // 1) Tell the PRIMARY (active) they'll be held briefly.
                    let cancel = speak_announcement(
                        bt,
                        &synth,
                        HOLD_PRIMARY_ASK_LINE,
                        sr,
                        &ctx.pace,
                        protected_max_ms,
                        &control_rx,
                        &mut *pending_controls,
                    );
                    if let Some(action) = cancel {
                        // The primary hung up during the ask — honour it and
                        // abandon the juggle (the phone promotes the waiting
                        // caller to a fresh incoming, which auto-answers).
                        perform_cancel_action(action, bt, &mut *tracker, outbox, sink);
                    } else if let Err(e) = remote_media
                        .with_aokie_switch_owner(
                            auto_hold_switch
                                .as_ref()
                                .expect("auto-hold entry captured a switch fence"),
                            || {
                                *status.switch_in_flight.lock().unwrap() =
                                    Some(("auto_hold_accept".to_string(), Instant::now()));
                                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                bt.flush_tx_audio();
                                bt.hold_swap()
                            },
                        )
                        .and_then(|result| result)
                    {
                        eprintln!("[aokie-plugin] AUTO-HOLD: exact-owner CHLD=2 to reach the newcomer refused/failed: {e}");
                    } else {
                        // 2) VERIFY the accept BEFORE any session state
                        // moves: the knock must resolve into a held+active
                        // pair. The settle pump keeps the tracker truthful
                        // and fires one AT+CLCC for the judge/logs.
                        let knock_id = w.call_id.clone();
                        let snap = settle_swap(
                            bt,
                            &mut *tracker,
                            outbox,
                            sink,
                            &status,
                            std::time::Duration::from_millis(5000),
                            Some(std::time::Duration::from_millis(1500)),
                            |s, _| {
                                matches!(
                                    judge_accept(s, &knock_id),
                                    AcceptVerdict::Accepted | AcceptVerdict::PrimaryGone
                                )
                            },
                        );
                        let accept = judge_accept(&snap, &w.call_id);
                        eprintln!(
                            "[aokie-plugin] AUTO-HOLD: accept verdict {:?} (callheld={}, clcc legs={})",
                            accept,
                            snap.callheld,
                            snap.clcc.as_ref().map(|c| c.len()).unwrap_or(0)
                        );
                        match accept {
                            AcceptVerdict::Accepted => {
                                // 3) The newcomer has the line — NOW park the
                                // primary's session + context and mint the
                                // newcomer as a real answered call.
                                let sess_a = tracker.park().expect("primary was active");
                                let a_id = sess_a.id.clone();
                                let a_number = sess_a.caller_id.clone();
                                let ctx_a =
                                    std::mem::replace(ctx, CallVoiceContext::fresh(None));
                                synth.reset_call();
                                stt_buf.clear();
                                *stt_had_speech = false;
                                *stt_silence = Duration::ZERO;
                                while probe_result_rx.try_recv().is_ok() {}
                                let b_id = w.call_id.clone();
                                let b_from = w.from.clone();
                                tracker.ring(b_id.clone(), aokie_core::events::now_iso8601());
                                if !b_from.is_empty() {
                                    tracker.caller_id(b_from.clone());
                                }
                                flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                if !b_from.is_empty() {
                                    emit(
                                        outbox,
                                        sink,
                                        aokie_core::events::aokie_event(
                                            crate::contract::events::CALL_CALLER_ID,
                                            &b_id,
                                            json!({"callId": b_id, "from": b_from, "at": aokie_core::events::now_iso8601()}),
                                        ),
                                    );
                                }
                                tracker.answered();
                                emit(
                                    outbox,
                                    sink,
                                    aokie_core::events::aokie_event(
                                        crate::contract::events::CALL_ANSWERED,
                                        &b_id,
                                        json!({"at": aokie_core::events::now_iso8601()}),
                                    ),
                                );
                                stt_current_gen.store(tracker.generation(), Ordering::Relaxed);
                                *status.parked_call.lock().unwrap() = Some(SwitchboardLeg {
                                    call_id: a_id.clone(),
                                    from: a_number.clone().unwrap_or_default(),
                                    since_iso: aokie_core::events::now_iso8601(),
                                });
                                *status.current_call_id.lock().unwrap() = Some(b_id.clone());
                                *status.current_caller.lock().unwrap() = if b_from.is_empty() {
                                    None
                                } else {
                                    Some(b_from.clone())
                                };
                                *status.call_started_at.lock().unwrap() =
                                    tracker.current().map(|s| s.started_at_iso.clone());
                                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                // 4) Wait for the (possibly re-established)
                                // SCO, then ask the newcomer to hold with
                                // their queue position.
                                let _ = settle_swap(
                                    bt,
                                    &mut *tracker,
                                    outbox,
                                    sink,
                                    &status,
                                    std::time::Duration::from_millis(2500),
                                    None,
                                    |_, sr_now| sr_now > 0,
                                );
                                let sr_b = bt.get_sample_rate();
                                let hold_line = second_caller_hold_line(0);
                                if sr_b > 0 {
                                    let _ = speak_announcement(
                                        bt,
                                        &synth,
                                        &hold_line,
                                        sr_b,
                                        &ctx.pace,
                                        protected_max_ms,
                                        &control_rx,
                                        &mut *pending_controls,
                                    );
                                    emit_turn(
                                        outbox,
                                        sink,
                                        &b_id,
                                        ctx.turn_index,
                                        "bot",
                                        &hold_line,
                                    );
                                    ctx.turn_index += 1;
                                } else {
                                    eprintln!("[aokie-plugin] AUTO-HOLD: no SCO after the accept — hold line skipped");
                                }
                                // 5) Enforced quiet dwell between the two
                                // toggles (rapid CHLD pairs are what wedged
                                // the z49 test), then swap back.
                                let _ = settle_swap(
                                    bt,
                                    &mut *tracker,
                                    outbox,
                                    sink,
                                    &status,
                                    std::time::Duration::from_millis(1200),
                                    None,
                                    |_, _| false,
                                );
                                // ⚠️ A NEW knock arriving during the hold
                                // line makes CHLD=2 a landmine: with a
                                // waiting call up it ACCEPTS the knock
                                // instead of swapping (live 2026-07-15
                                // round 6: the phone answered the third
                                // caller and dropped the held primary).
                                // Degrade to single-swap instead — the
                                // newcomer keeps the line, the primary
                                // stays parked, and the knocker rings on
                                // as the next in the FIFO queue.
                                let knock_mid_juggle =
                                    status.waiting_call.lock().unwrap().is_some();
                                let mut swap_back_sent: Result<(), String> = Ok(());
                                let mut verdict = if knock_mid_juggle {
                                    eprintln!("[aokie-plugin] AUTO-HOLD: a new caller knocked mid-juggle — swap-back skipped (CHLD=2 would accept them, not swap)");
                                    SwapBackVerdict::StayedOnNewcomer
                                } else {
                                    swap_back_sent = remote_media
                                        .aokie_switch_fence()
                                        .ok_or_else(|| {
                                            "Companion media claim crossed the switch-back"
                                                .to_string()
                                        })
                                        .and_then(|fence| {
                                            remote_media
                                                .with_aokie_switch_owner(&fence, || {
                                                    *status.switch_in_flight.lock().unwrap() =
                                                        Some((
                                                            "auto_hold_return".to_string(),
                                                            Instant::now(),
                                                        ));
                                                    status
                                                        .switchboard_revision
                                                        .fetch_add(1, Ordering::Relaxed);
                                                    bt.flush_tx_audio();
                                                    bt.hold_swap()
                                                })
                                                .and_then(|result| result)
                                        });
                                    if swap_back_sent.is_err() {
                                        eprintln!("[aokie-plugin] AUTO-HOLD: swap-back CHLD=2 failed to send — staying with the newcomer");
                                        SwapBackVerdict::StayedOnNewcomer
                                    } else {
                                        settle_and_judge_swap_back(
                                            bt,
                                            &mut *tracker,
                                            outbox,
                                            sink,
                                            &status,
                                            a_number.as_deref(),
                                            if b_from.is_empty() {
                                                None
                                            } else {
                                                Some(b_from.as_str())
                                            },
                                        )
                                    }
                                };
                                if verdict == SwapBackVerdict::StayedOnNewcomer
                                    && !knock_mid_juggle
                                    && swap_back_sent.is_ok()
                                    // Re-check: a knock can land during the
                                    // first settle too.
                                    && status.waiting_call.lock().unwrap().is_none()
                                {
                                    // The topology is KNOWN (one active + one
                                    // held, no knock): a second CHLD=2 is a
                                    // pure verified swap, not a blind retry.
                                    eprintln!("[aokie-plugin] AUTO-HOLD: swap-back did not take — one verified retry");
                                    let _ = settle_swap(
                                        bt,
                                        &mut *tracker,
                                        outbox,
                                        sink,
                                        &status,
                                        std::time::Duration::from_millis(1500),
                                        None,
                                        |_, _| false,
                                    );
                                    let retry_sent = remote_media
                                        .aokie_switch_fence()
                                        .ok_or_else(|| {
                                            "Companion media claim crossed the switch retry"
                                                .to_string()
                                        })
                                        .and_then(|fence| {
                                            remote_media
                                                .with_aokie_switch_owner(&fence, || {
                                                    *status.switch_in_flight.lock().unwrap() =
                                                        Some((
                                                            "auto_hold_return_retry"
                                                                .to_string(),
                                                            Instant::now(),
                                                        ));
                                                    status
                                                        .switchboard_revision
                                                        .fetch_add(1, Ordering::Relaxed);
                                                    bt.flush_tx_audio();
                                                    bt.hold_swap()
                                                })
                                                .and_then(|result| result)
                                        });
                                    if retry_sent.is_ok() {
                                        verdict = settle_and_judge_swap_back(
                                            bt,
                                            &mut *tracker,
                                            outbox,
                                            sink,
                                            &status,
                                            a_number.as_deref(),
                                            if b_from.is_empty() {
                                                None
                                            } else {
                                                Some(b_from.as_str())
                                            },
                                        );
                                    }
                                }
                                eprintln!(
                                    "[aokie-plugin] AUTO-HOLD: swap-back verdict {verdict:?}"
                                );
                                match verdict {
                                    SwapBackVerdict::Swapped => {
                                        // Newcomer parked; the primary resumes
                                        // with their whole conversation intact.
                                        let sess_b =
                                            tracker.park().expect("newcomer was active");
                                        let ctx_b = std::mem::replace(ctx, ctx_a);
                                        match tracker.restore(sess_a) {
                                            Ok(_gen) => {}
                                            Err(back) => {
                                                eprintln!("[aokie-plugin] AUTO-HOLD: primary restore refused — recovering");
                                                let _ = tracker.restore(back);
                                            }
                                        }
                                        // The primary CONTINUES — not a call
                                        // boundary: align the generation so the
                                        // reset block does not wipe their
                                        // conversation context.
                                        *voice_call_gen = tracker.generation();
                                        stt_current_gen
                                            .store(*voice_call_gen, Ordering::Relaxed);
                                        ctx.rt_lane = tracker.call_id().map(|id| {
                                            crate::realtime::RealtimeLane::new(
                                                id.to_string(),
                                                *voice_call_gen,
                                                Instant::now(),
                                            )
                                        });
                                        synth.reset_call();
                                        stt_buf.clear();
                                        *stt_had_speech = false;
                                        *stt_silence = Duration::ZERO;
                                        while probe_result_rx.try_recv().is_ok() {}
                                        *parked = Some((sess_b, ctx_b));
                                        *status.parked_call.lock().unwrap() =
                                            Some(SwitchboardLeg {
                                                call_id: b_id.clone(),
                                                from: b_from.clone(),
                                                since_iso: aokie_core::events::now_iso8601(),
                                            });
                                        {
                                            // Clear ONLY the knock we just
                                            // served — a NEW caller knocking
                                            // mid-settle must stay tracked
                                            // (live 2026-07-15 15:19: this
                                            // blind clear erased the third
                                            // caller; their give-up never
                                            // became a missed call).
                                            let mut wl = status.waiting_call.lock().unwrap();
                                            if wl.as_ref().is_some_and(|l| l.call_id == b_id) {
                                                *wl = None;
                                            }
                                        }
                                        *status.current_call_id.lock().unwrap() =
                                            tracker.call_id().map(|s| s.to_string());
                                        *status.current_caller.lock().unwrap() =
                                            tracker.current().and_then(|s| s.caller_id.clone());
                                        *status.call_started_at.lock().unwrap() =
                                            tracker.current().map(|s| s.started_at_iso.clone());
                                        status.call_active.store(true, Ordering::Relaxed);
                                        status
                                            .switchboard_revision
                                            .fetch_add(1, Ordering::Relaxed);
                                        // The resume line speaks via the hook
                                        // below the reset block (it waits for
                                        // the SCO to come back and retries).
                                        *resume_line_for = Some((a_id.clone(), Instant::now()));
                                        eprintln!(
                                            "[aokie-plugin] AUTO-HOLD: {} parked (next in queue), resumed with the primary caller",
                                            b_id
                                        );
                                    }
                                    SwapBackVerdict::SwappedNewcomerGone => {
                                        // The swap took but the newcomer's leg
                                        // vanished — close them honestly; the
                                        // primary is active alone.
                                        flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                        if let Some(ended) = tracker.terminate() {
                                            emit_call_ended(
                                                &ended,
                                                status.config_version.load(Ordering::Relaxed),
                                                outbox,
                                                sink,
                                            );
                                        }
                                        *ctx = ctx_a;
                                        match tracker.restore(sess_a) {
                                            Ok(_gen) => {}
                                            Err(back) => {
                                                eprintln!("[aokie-plugin] AUTO-HOLD: primary restore refused — recovering");
                                                let _ = tracker.restore(back);
                                            }
                                        }
                                        *voice_call_gen = tracker.generation();
                                        stt_current_gen
                                            .store(*voice_call_gen, Ordering::Relaxed);
                                        ctx.rt_lane = tracker.call_id().map(|id| {
                                            crate::realtime::RealtimeLane::new(
                                                id.to_string(),
                                                *voice_call_gen,
                                                Instant::now(),
                                            )
                                        });
                                        synth.reset_call();
                                        stt_buf.clear();
                                        *stt_had_speech = false;
                                        *stt_silence = Duration::ZERO;
                                        while probe_result_rx.try_recv().is_ok() {}
                                        *status.parked_call.lock().unwrap() = None;
                                        *status.current_call_id.lock().unwrap() =
                                            tracker.call_id().map(|s| s.to_string());
                                        *status.current_caller.lock().unwrap() =
                                            tracker.current().and_then(|s| s.caller_id.clone());
                                        *status.call_started_at.lock().unwrap() =
                                            tracker.current().map(|s| s.started_at_iso.clone());
                                        status.call_active.store(true, Ordering::Relaxed);
                                        status
                                            .switchboard_revision
                                            .fetch_add(1, Ordering::Relaxed);
                                        *resume_line_for = Some((a_id.clone(), Instant::now()));
                                        eprintln!("[aokie-plugin] AUTO-HOLD: newcomer's leg vanished during the swap-back — primary resumed alone");
                                    }
                                    SwapBackVerdict::StayedOnNewcomer => {
                                        // DEGRADED single-swap: the phone will
                                        // not give the primary back right now.
                                        // The newcomer becomes the conversation
                                        // (the promoted greeting releases them
                                        // from "please hold"); the primary
                                        // stays parked — the reconciliation
                                        // block auto-retrieves them the moment
                                        // this call ends.
                                        *parked = Some((sess_a, ctx_a));
                                        *promote_greet_for = Some(b_id.clone());
                                        *pending_ctx_restore = Some(std::mem::replace(
                                            &mut *ctx,
                                            CallVoiceContext::fresh(None),
                                        ));
                                        status
                                            .switchboard_revision
                                            .fetch_add(1, Ordering::Relaxed);
                                        eprintln!("[aokie-plugin] AUTO-HOLD: staying with the newcomer — the primary remains parked for auto-retrieve");
                                    }
                                    SwapBackVerdict::NewcomerAlone => {
                                        // The primary's held leg vanished — the
                                        // newcomer keeps the line alone. Close
                                        // the primary honestly; release the
                                        // newcomer from "please hold".
                                        let a_intent = parked_end_intent(&sess_a);
                                        let ended_a = crate::call_session::SessionTracker::terminate_detached(sess_a, Some(a_intent));
                                        emit_call_ended(
                                            &ended_a,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                        drop(ctx_a);
                                        *status.parked_call.lock().unwrap() = None;
                                        *promote_greet_for = Some(b_id.clone());
                                        *pending_ctx_restore = Some(std::mem::replace(
                                            &mut *ctx,
                                            CallVoiceContext::fresh(None),
                                        ));
                                        status
                                            .switchboard_revision
                                            .fetch_add(1, Ordering::Relaxed);
                                        eprintln!("[aokie-plugin] AUTO-HOLD: the primary's leg dropped while held — continuing with the newcomer");
                                    }
                                    SwapBackVerdict::ActiveDied => {
                                        // The newcomer died mid-swap: close
                                        // them; park the primary — the proven
                                        // reconciliation retrieve brings them
                                        // back on a later pass.
                                        flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                        if let Some(ended) = tracker.terminate() {
                                            emit_call_ended(
                                                &ended,
                                                status.config_version.load(Ordering::Relaxed),
                                                outbox,
                                                sink,
                                            );
                                        }
                                        *parked = Some((sess_a, ctx_a));
                                        *status.parked_call.lock().unwrap() =
                                            Some(SwitchboardLeg {
                                                call_id: a_id.clone(),
                                                from: a_number.clone().unwrap_or_default(),
                                                since_iso: aokie_core::events::now_iso8601(),
                                            });
                                        status.call_active.store(false, Ordering::Relaxed);
                                        *status.current_call_id.lock().unwrap() = None;
                                        *status.current_caller.lock().unwrap() = None;
                                        *status.call_started_at.lock().unwrap() = None;
                                        status
                                            .switchboard_revision
                                            .fetch_add(1, Ordering::Relaxed);
                                        eprintln!("[aokie-plugin] AUTO-HOLD: the newcomer died mid-swap — the primary will be retrieved from hold");
                                    }
                                    SwapBackVerdict::AllGone => {
                                        // The z49 signature — both legs tore
                                        // down. Close everything honestly; the
                                        // per-call reset re-arms a clean idle.
                                        flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                        if let Some(ended) = tracker.terminate() {
                                            emit_call_ended(
                                                &ended,
                                                status.config_version.load(Ordering::Relaxed),
                                                outbox,
                                                sink,
                                            );
                                        }
                                        let a_intent = parked_end_intent(&sess_a);
                                        let ended_a = crate::call_session::SessionTracker::terminate_detached(sess_a, Some(a_intent));
                                        emit_call_ended(
                                            &ended_a,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                        drop(ctx_a);
                                        *status.parked_call.lock().unwrap() = None;
                                        status.call_active.store(false, Ordering::Relaxed);
                                        *status.current_call_id.lock().unwrap() = None;
                                        *status.current_caller.lock().unwrap() = None;
                                        *status.call_started_at.lock().unwrap() = None;
                                        status
                                            .switchboard_revision
                                            .fetch_add(1, Ordering::Relaxed);
                                        eprintln!("[aokie-plugin] AUTO-HOLD: both calls tore down during the swap-back — line is idle");
                                    }
                                    SwapBackVerdict::StrangerActive => {
                                        // The swap-back CHLD collided with a
                                        // brand-new knock: the phone answered
                                        // the STRANGER and sacrificed the held
                                        // primary. Close the primary honestly
                                        // (lost on hold — the apology-SMS flow
                                        // owns the follow-up), park the
                                        // newcomer (their leg is held), and
                                        // mint the stranger as the foreground
                                        // call so the live audio has an owner.
                                        let ended_a = crate::call_session::SessionTracker::terminate_detached(
                                            sess_a,
                                            Some(crate::call_session::TerminationIntent::AbandonedOnHold),
                                        );
                                        emit_call_ended(
                                            &ended_a,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                        drop(ctx_a);
                                        let sess_b2 =
                                            tracker.park().expect("newcomer was tracked");
                                        let b2_id = sess_b2.id.clone();
                                        let b2_from =
                                            sess_b2.caller_id.clone().unwrap_or_default();
                                        let ctx_b2 = std::mem::replace(
                                            &mut *ctx,
                                            CallVoiceContext::fresh(None),
                                        );
                                        *parked = Some((sess_b2, ctx_b2));
                                        *status.parked_call.lock().unwrap() =
                                            Some(SwitchboardLeg {
                                                call_id: b2_id,
                                                from: b2_from,
                                                since_iso: aokie_core::events::now_iso8601(),
                                            });
                                        // The stranger's identity: the live
                                        // knock leg, or the just-ended episode
                                        // stash (our CHLD consumed the knock).
                                        let stranger =
                                            status.waiting_call.lock().unwrap().take().or_else(
                                                || {
                                                    status
                                                        .gave_up_knock
                                                        .lock()
                                                        .unwrap()
                                                        .pop()
                                                        .map(|(l, _, _)| l)
                                                },
                                            );
                                        let (s_id, s_from) = match stranger {
                                            Some(l) => (l.call_id, l.from),
                                            None => (
                                                format!(
                                                    "call_{}",
                                                    uuid::Uuid::new_v4().simple()
                                                ),
                                                String::new(),
                                            ),
                                        };
                                        tracker.ring(
                                            s_id.clone(),
                                            aokie_core::events::now_iso8601(),
                                        );
                                        if !s_from.is_empty() {
                                            tracker.caller_id(s_from.clone());
                                        }
                                        flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                        if !s_from.is_empty() {
                                            emit(
                                                outbox,
                                                sink,
                                                aokie_core::events::aokie_event(
                                                    crate::contract::events::CALL_CALLER_ID,
                                                    &s_id,
                                                    json!({"callId": s_id, "from": s_from, "at": aokie_core::events::now_iso8601()}),
                                                ),
                                            );
                                        }
                                        tracker.answered();
                                        emit(
                                            outbox,
                                            sink,
                                            aokie_core::events::aokie_event(
                                                crate::contract::events::CALL_ANSWERED,
                                                &s_id,
                                                json!({"at": aokie_core::events::now_iso8601()}),
                                            ),
                                        );
                                        *status.current_call_id.lock().unwrap() =
                                            Some(s_id.clone());
                                        *status.current_caller.lock().unwrap() =
                                            if s_from.is_empty() {
                                                None
                                            } else {
                                                Some(s_from.clone())
                                            };
                                        *status.call_started_at.lock().unwrap() =
                                            tracker.current().map(|s| s.started_at_iso.clone());
                                        status.call_active.store(true, Ordering::Relaxed);
                                        status
                                            .switchboard_revision
                                            .fetch_add(1, Ordering::Relaxed);
                                        // Fresh call: the per-call reset + the
                                        // normal greeting machinery own it now.
                                        eprintln!("[aokie-plugin] AUTO-HOLD: swap-back collided with a new knock — the new caller has the line; primary closed (lost on hold), newcomer parked");
                                    }
                                    SwapBackVerdict::Inconclusive => {
                                        // Structurally unreachable (the
                                        // settle helper always resolves) —
                                        // defensively take the least
                                        // destructive road: nobody's
                                        // session is closed, the newcomer
                                        // keeps the line, the primary
                                        // stays parked for auto-retrieve.
                                        *parked = Some((sess_a, ctx_a));
                                        *promote_greet_for = Some(b_id.clone());
                                        *pending_ctx_restore = Some(std::mem::replace(
                                            &mut *ctx,
                                            CallVoiceContext::fresh(None),
                                        ));
                                        eprintln!("[aokie-plugin] AUTO-HOLD: unresolved swap verdict — degrading to single-swap");
                                    }
                                }
                            }
                            AcceptVerdict::HeldAlone => {
                                // The phone held the primary but gave the line
                                // to nobody. Park them for real — the proven
                                // reconciliation block retrieves a parked
                                // caller the moment the foreground is empty,
                                // with all its raced-ring guards.
                                eprintln!("[aokie-plugin] AUTO-HOLD: primary held with nobody active — parking them for auto-retrieve");
                                let sess_a = tracker.park().expect("primary was tracked");
                                let a_id = sess_a.id.clone();
                                let a_from = sess_a.caller_id.clone().unwrap_or_default();
                                let ctx_a =
                                    std::mem::replace(ctx, CallVoiceContext::fresh(None));
                                *parked = Some((sess_a, ctx_a));
                                *status.parked_call.lock().unwrap() = Some(SwitchboardLeg {
                                    call_id: a_id,
                                    from: a_from,
                                    since_iso: aokie_core::events::now_iso8601(),
                                });
                                status.call_active.store(false, Ordering::Relaxed);
                                *status.current_call_id.lock().unwrap() = None;
                                *status.current_caller.lock().unwrap() = None;
                                *status.call_started_at.lock().unwrap() = None;
                                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                            }
                            AcceptVerdict::NothingChanged | AcceptVerdict::KnockGone => {
                                // The phone ignored the CHLD (or the waiting
                                // caller gave up mid-swap) — the primary still
                                // has the line; own the awkward beat out loud.
                                eprintln!("[aokie-plugin] AUTO-HOLD: juggle abandoned ({accept:?}) — resuming the primary");
                                let sr_now = bt.get_sample_rate();
                                if sr_now > 0 {
                                    let _ = speak_announcement(
                                        bt,
                                        &synth,
                                        HOLD_JUGGLE_ABORT_LINE,
                                        sr_now,
                                        &ctx.pace,
                                        protected_max_ms,
                                        &control_rx,
                                        &mut *pending_controls,
                                    );
                                }
                            }
                            AcceptVerdict::PrimaryGone => {
                                // The primary's call died mid-accept — the
                                // settle pump already closed it truthfully. If
                                // the phone reports a live leg (the newcomer
                                // made it on), mint them as a real answered
                                // call; the promoted greeting releases them.
                                let newcomer_on = snap
                                    .clcc
                                    .as_ref()
                                    .is_some_and(|c| c.iter().any(|l| l.status == 0))
                                    || snap.callheld == 1;
                                if snap.waiting_id.is_none() && newcomer_on {
                                    eprintln!("[aokie-plugin] AUTO-HOLD: primary ended mid-accept — the newcomer has the line; minting their call");
                                    let b_id = w.call_id.clone();
                                    let b_from = w.from.clone();
                                    tracker
                                        .ring(b_id.clone(), aokie_core::events::now_iso8601());
                                    if !b_from.is_empty() {
                                        tracker.caller_id(b_from.clone());
                                    }
                                    flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                    if !b_from.is_empty() {
                                        emit(
                                            outbox,
                                            sink,
                                            aokie_core::events::aokie_event(
                                                crate::contract::events::CALL_CALLER_ID,
                                                &b_id,
                                                json!({"callId": b_id, "from": b_from, "at": aokie_core::events::now_iso8601()}),
                                            ),
                                        );
                                    }
                                    tracker.answered();
                                    emit(
                                        outbox,
                                        sink,
                                        aokie_core::events::aokie_event(
                                            crate::contract::events::CALL_ANSWERED,
                                            &b_id,
                                            json!({"at": aokie_core::events::now_iso8601()}),
                                        ),
                                    );
                                    *status.current_call_id.lock().unwrap() =
                                        Some(b_id.clone());
                                    *status.current_caller.lock().unwrap() =
                                        if b_from.is_empty() {
                                            None
                                        } else {
                                            Some(b_from.clone())
                                        };
                                    *status.call_started_at.lock().unwrap() =
                                        tracker.current().map(|s| s.started_at_iso.clone());
                                    status.call_active.store(true, Ordering::Relaxed);
                                    {
                                        // Same id-guarded clear as the other
                                        // accept paths (never wipe a newer knock).
                                        let mut wl = status.waiting_call.lock().unwrap();
                                        if wl.as_ref().is_some_and(|l| l.call_id == b_id) {
                                            *wl = None;
                                        }
                                    }
                                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                    // A fresh call: the per-call reset fences
                                    // STT/context; the greeting block then
                                    // speaks the promoted greeting.
                                    *promote_greet_for = Some(b_id);
                                } else {
                                    eprintln!("[aokie-plugin] AUTO-HOLD: primary ended mid-accept and no live leg is visible — going idle");
                                }
                            }
                        }
                        *prev_call_held = status.call_held_state.load(Ordering::Relaxed);
                    }
                }
            }
        }
    }
}
