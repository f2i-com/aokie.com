//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `reconcile_switchboard`.

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
// Several params only matter to the voice build's spoken hold/queue arms.
#[cfg_attr(not(feature = "voice"), allow(unused_variables))]
pub(super) fn reconcile_switchboard(
    bt: &mut dyn crate::backend::RadioBackend,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    control_rx: &std::sync::mpsc::Receiver<RadioControl>,
    status: &Arc<RadioStatus>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    tracker: &mut crate::call_session::SessionTracker,
    pending_companion_end_caller: &mut Option<PendingCompanionEndCaller>,
    ctx: &mut CallVoiceContext,
    parked: &mut Option<(crate::call_session::CallSession, CallVoiceContext)>,
    pending_ctx_restore: &mut Option<CallVoiceContext>,
    prev_call_held: &mut u64,
    promote_greet_for: &mut Option<String>,
    resume_line_for: &mut Option<(String, std::time::Instant)>,
    pending_controls: &mut std::collections::VecDeque<RadioControl>,
    #[cfg(feature = "voice")] synth: &crate::synth::SynthHandle,
    #[cfg(feature = "voice")] stt_current_gen: &Arc<AtomicU64>,
    #[cfg(feature = "voice")] probe_result_rx: &std::sync::mpsc::Receiver<SttResult>,
    #[cfg(feature = "voice")] stt_buf: &mut Vec<f32>,
    #[cfg(feature = "voice")] stt_had_speech: &mut bool,
    #[cfg(feature = "voice")] stt_silence: &mut std::time::Duration,
    #[cfg(feature = "voice")] screen_policy: &crate::screen::ScreenPolicy,
    #[cfg(feature = "voice")] auto_hold: bool,
    #[cfg(feature = "voice")] auto_hold_done_for: &mut Option<String>,
    #[cfg(feature = "voice")] protected_max_ms: u32,
) {
    // ── Phase 4 switchboard reconciliation (only while a caller is
    // parked — normal calls never enter this block). The phone's
    // indicator stream cannot say WHICH leg an edge belongs to once two
    // exist; these are the conservative attribution rules, suppressed
    // inside the ~4s window after our OWN CHLD (whose transitions are
    // expected, not news).
    if parked.is_some() {
        let held_now = status.call_held_state.load(Ordering::Relaxed);
        let switch_recent = status
            .switch_in_flight
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() < std::time::Duration::from_secs(4));
        if !status.connected.load(Ordering::Relaxed) {
            // The phone link died with a caller parked: their session
            // terminates truthfully (the foreground's own device-loss
            // termination was already synthesized by handle_event).
            if let Some((sess, _ctx_lost)) = parked.take() {
                eprintln!(
                    "[aokie-plugin] SWITCHBOARD: phone link lost with {} parked — synthesized termination",
                    sess.id
                );
                let ended = crate::call_session::SessionTracker::terminate_detached(
                    sess,
                    Some(crate::call_session::TerminationIntent::DeviceLost),
                );
                emit_call_ended(
                    &ended,
                    status.config_version.load(Ordering::Relaxed),
                    outbox,
                    sink,
                );
                *status.parked_call.lock().unwrap() = None;
                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
            }
        } else if tracker.current().is_none() {
            // The foreground ended — the parked caller is still on the
            // phone's hold, waiting.
            let knock = status.waiting_call.lock().unwrap().clone();
            if let Some(w) = knock {
                // A second caller is KNOCKING at retrieve time: CHLD=2
                // would accept THEM (a waiting call beats a held one on
                // this phone), not retrieve the parked caller — acting
                // blindly here crossed sessions before v2. With the
                // auto-hold queue on, serve FIFO: accept the knock, ask
                // them to hold with their queue position, then swap to
                // the parked caller (who has waited longest). Without
                // it, WAIT — the knock resolves on its own (the caller
                // gives up, or the phone promotes their ring) and a
                // later pass retrieves the parked caller.
                #[cfg(feature = "voice")]
                if auto_hold
                    && !remote_media.radio_reserved()
                    && auto_hold_done_for.as_deref() != Some(w.call_id.as_str())
                    && screen_policy
                        .verdict(if w.from.is_empty() {
                            None
                        } else {
                            Some(w.from.as_str())
                        })
                        .is_some()
                {
                    // A SCREENED caller knocking while someone is parked:
                    // never serve them FIFO — wait for their ring to
                    // clear, then the plain retrieve brings the parked
                    // caller back (accepting them just to hang up on them
                    // would also delay the caller who matters).
                    *auto_hold_done_for = Some(w.call_id.clone());
                    eprintln!(
                        "[aokie-plugin] SWITCHBOARD: knocking caller is screened — waiting for their ring to clear before retrieving the parked caller"
                    );
                }
                #[cfg(feature = "voice")]
                if auto_hold
                    && !remote_media.radio_reserved()
                    && !switch_recent
                    && auto_hold_done_for.as_deref() != Some(w.call_id.as_str())
                {
                    *auto_hold_done_for = Some(w.call_id.clone());
                    eprintln!(
                        "[aokie-plugin] SWITCHBOARD: foreground ended with a caller parked AND {} knocking — serving FIFO (accept, ask to hold, return to the parked caller)",
                        w.call_id
                    );
                    *status.switch_in_flight.lock().unwrap() =
                        Some(("cascade_accept".to_string(), std::time::Instant::now()));
                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                    bt.flush_tx_audio();
                    if let Err(e) = bt.hold_swap() {
                        eprintln!("[aokie-plugin] SWITCHBOARD: cascade CHLD=2 failed: {e}");
                    } else {
                        let knock_id = w.call_id.clone();
                        // Accepted here = the knock resolved into a
                        // held+active pair (the parked caller stays held
                        // underneath throughout).
                        let snap = settle_swap(
                            bt,
                            &mut *tracker,
                            outbox,
                            sink,
                            &status,
                            std::time::Duration::from_millis(5000),
                            Some(std::time::Duration::from_millis(1500)),
                            |s, _| {
                                s.waiting_id.as_deref() != Some(knock_id.as_str())
                                    && s.callheld == 1
                            },
                        );
                        let accepted = snap.waiting_id.as_deref() != Some(w.call_id.as_str())
                            && snap.callheld == 1;
                        if !accepted {
                            // The knock never became a live leg. If the
                            // parked caller is still held alone a later
                            // pass retrieves them once the knock clears;
                            // callheld==0 means their leg died too.
                            eprintln!(
                                "[aokie-plugin] SWITCHBOARD: cascade accept unverified (callheld={}, knock_up={}) — reconciliation converges from here",
                                snap.callheld,
                                snap.waiting_id.is_some()
                            );
                            if snap.callheld == 0 && snap.waiting_id.is_none() {
                                if let Some((sess_gone, _ctx_gone)) = parked.take() {
                                    eprintln!(
                                        "[aokie-plugin] SWITCHBOARD: parked caller {} is gone too",
                                        sess_gone.id
                                    );
                                    let gone_intent = parked_end_intent(&sess_gone);
                                    let ended =
                                        crate::call_session::SessionTracker::terminate_detached(
                                            sess_gone,
                                            Some(gone_intent),
                                        );
                                    emit_call_ended(
                                        &ended,
                                        status.config_version.load(Ordering::Relaxed),
                                        outbox,
                                        sink,
                                    );
                                    *status.parked_call.lock().unwrap() = None;
                                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        } else {
                            // The newcomer has the line. Mint their call,
                            // ask them to hold (the parked caller is
                            // ahead of them), then swap to the parked
                            // caller — every step verified like the
                            // mid-call juggle above.
                            let c_id = w.call_id.clone();
                            let c_from = w.from.clone();
                            *ctx = CallVoiceContext::fresh(None);
                            synth.reset_call();
                            stt_buf.clear();
                            *stt_had_speech = false;
                            *stt_silence = Duration::ZERO;
                            while probe_result_rx.try_recv().is_ok() {}
                            tracker.ring(c_id.clone(), aokie_core::events::now_iso8601());
                            if !c_from.is_empty() {
                                tracker.caller_id(c_from.clone());
                            }
                            flush_incoming_if_pending(&mut *tracker, outbox, sink);
                            if !c_from.is_empty() {
                                emit(
                                    outbox,
                                    sink,
                                    aokie_core::events::aokie_event(
                                        crate::contract::events::CALL_CALLER_ID,
                                        &c_id,
                                        json!({"callId": c_id, "from": c_from, "at": aokie_core::events::now_iso8601()}),
                                    ),
                                );
                            }
                            tracker.answered();
                            emit(
                                outbox,
                                sink,
                                aokie_core::events::aokie_event(
                                    crate::contract::events::CALL_ANSWERED,
                                    &c_id,
                                    json!({"at": aokie_core::events::now_iso8601()}),
                                ),
                            );
                            stt_current_gen.store(tracker.generation(), Ordering::Relaxed);
                            {
                                // Clear ONLY the knock we just served: a NEW
                                // caller can knock mid-settle, and wiping
                                // their freshly-minted leg makes the plugin
                                // forget them entirely (live 2026-07-15
                                // 15:19: the third caller vanished from
                                // tracking, so their give-up never became a
                                // missed call and no one rang them back).
                                let mut wl = status.waiting_call.lock().unwrap();
                                if wl.as_ref().is_some_and(|l| l.call_id == c_id) {
                                    *wl = None;
                                }
                            }
                            *status.current_call_id.lock().unwrap() = Some(c_id.clone());
                            *status.current_caller.lock().unwrap() = if c_from.is_empty() {
                                None
                            } else {
                                Some(c_from.clone())
                            };
                            *status.call_started_at.lock().unwrap() =
                                tracker.current().map(|s| s.started_at_iso.clone());
                            status.call_active.store(true, Ordering::Relaxed);
                            status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
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
                            let sr_c = bt.get_sample_rate();
                            let hold_line = second_caller_hold_line(1);
                            if sr_c > 0 {
                                let _ = speak_announcement(
                                    bt,
                                    &synth,
                                    &hold_line,
                                    sr_c,
                                    &ctx.pace,
                                    protected_max_ms,
                                    &control_rx,
                                    &mut *pending_controls,
                                );
                                emit_turn(
                                    outbox,
                                    sink,
                                    &c_id,
                                    ctx.turn_index,
                                    "bot",
                                    &hold_line,
                                );
                                ctx.turn_index += 1;
                            }
                            // Dwell, then swap to the parked caller.
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
                            let b_number =
                                parked.as_ref().and_then(|(p, _)| p.caller_id.clone());
                            // Same landmine as the juggle's swap-back: a
                            // fresh knock makes CHLD=2 accept the knocker.
                            let knock_mid_cascade =
                                status.waiting_call.lock().unwrap().is_some();
                            let mut swap_sent: Result<(), String> = Ok(());
                            let mut verdict = if knock_mid_cascade {
                                eprintln!("[aokie-plugin] SWITCHBOARD: a new caller knocked mid-cascade — swap skipped (CHLD=2 would accept them)");
                                SwapBackVerdict::StayedOnNewcomer
                            } else {
                                *status.switch_in_flight.lock().unwrap() = Some((
                                    "cascade_return".to_string(),
                                    std::time::Instant::now(),
                                ));
                                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                bt.flush_tx_audio();
                                swap_sent = bt.hold_swap();
                                if swap_sent.is_err() {
                                    SwapBackVerdict::StayedOnNewcomer
                                } else {
                                    settle_and_judge_swap_back(
                                        bt,
                                        &mut *tracker,
                                        outbox,
                                        sink,
                                        &status,
                                        b_number.as_deref(),
                                        if c_from.is_empty() {
                                            None
                                        } else {
                                            Some(c_from.as_str())
                                        },
                                    )
                                }
                            };
                            if verdict == SwapBackVerdict::StayedOnNewcomer
                                && !knock_mid_cascade
                                && swap_sent.is_ok()
                                && status.waiting_call.lock().unwrap().is_none()
                            {
                                eprintln!("[aokie-plugin] SWITCHBOARD: cascade swap did not take — one verified retry");
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
                                *status.switch_in_flight.lock().unwrap() = Some((
                                    "cascade_return_retry".to_string(),
                                    std::time::Instant::now(),
                                ));
                                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                bt.flush_tx_audio();
                                if bt.hold_swap().is_ok() {
                                    verdict = settle_and_judge_swap_back(
                                        bt,
                                        &mut *tracker,
                                        outbox,
                                        sink,
                                        &status,
                                        b_number.as_deref(),
                                        if c_from.is_empty() {
                                            None
                                        } else {
                                            Some(c_from.as_str())
                                        },
                                    );
                                }
                            }
                            eprintln!(
                                "[aokie-plugin] SWITCHBOARD: cascade verdict {verdict:?}"
                            );
                            match verdict {
                                SwapBackVerdict::Swapped => {
                                    // Newcomer parked; the longest-waiting
                                    // caller finally gets the line.
                                    let sess_c = tracker.park().expect("newcomer was active");
                                    let ctx_c = std::mem::replace(
                                        &mut *ctx,
                                        CallVoiceContext::fresh(None),
                                    );
                                    let (sess_b, ctx_b) = parked
                                        .take()
                                        .expect("cascade runs with a parked caller");
                                    let was_greeted = sess_b.greeted;
                                    let b_id = sess_b.id.clone();
                                    let b_from_leg =
                                        sess_b.caller_id.clone().unwrap_or_default();
                                    match tracker.restore(sess_b) {
                                        Ok(_gen) => {
                                            *pending_ctx_restore = Some(ctx_b);
                                            if !was_greeted {
                                                *promote_greet_for = Some(b_id.clone());
                                            } else {
                                                *resume_line_for = Some((
                                                    b_id.clone(),
                                                    std::time::Instant::now(),
                                                ));
                                            }
                                            *parked = Some((sess_c, ctx_c));
                                            *status.parked_call.lock().unwrap() =
                                                Some(SwitchboardLeg {
                                                    call_id: c_id.clone(),
                                                    from: c_from.clone(),
                                                    since_iso: aokie_core::events::now_iso8601(
                                                    ),
                                                });
                                            status.call_active.store(true, Ordering::Relaxed);
                                            *status.current_call_id.lock().unwrap() =
                                                Some(b_id);
                                            *status.current_caller.lock().unwrap() =
                                                if b_from_leg.is_empty() {
                                                    None
                                                } else {
                                                    Some(b_from_leg)
                                                };
                                            *status.call_started_at.lock().unwrap() = tracker
                                                .current()
                                                .map(|s| s.started_at_iso.clone());
                                            status
                                                .switchboard_revision
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(sess_back) => {
                                            eprintln!("[aokie-plugin] SWITCHBOARD: cascade restore refused — the newcomer's session closes honestly");
                                            *parked = Some((sess_back, ctx_b));
                                            let ended = crate::call_session::SessionTracker::terminate_detached(sess_c, None);
                                            emit_call_ended(
                                                &ended,
                                                status.config_version.load(Ordering::Relaxed),
                                                outbox,
                                                sink,
                                            );
                                            drop(ctx_c);
                                        }
                                    }
                                }
                                SwapBackVerdict::StayedOnNewcomer
                                | SwapBackVerdict::NewcomerAlone => {
                                    // The newcomer keeps the line. If the
                                    // parked caller's leg is gone, close
                                    // them; otherwise they stay parked and
                                    // are retrieved when this call ends.
                                    if verdict == SwapBackVerdict::NewcomerAlone {
                                        if let Some((sess_b, _ctx_b)) = parked.take() {
                                            let b_intent = parked_end_intent(&sess_b);
                                            let ended = crate::call_session::SessionTracker::terminate_detached(sess_b, Some(b_intent));
                                            emit_call_ended(
                                                &ended,
                                                status.config_version.load(Ordering::Relaxed),
                                                outbox,
                                                sink,
                                            );
                                            *status.parked_call.lock().unwrap() = None;
                                        }
                                    }
                                    *promote_greet_for = Some(c_id.clone());
                                    *pending_ctx_restore = Some(std::mem::replace(
                                        &mut *ctx,
                                        CallVoiceContext::fresh(None),
                                    ));
                                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                }
                                SwapBackVerdict::SwappedNewcomerGone => {
                                    // The swap took but the newcomer's leg
                                    // vanished — the parked caller's leg is
                                    // ACTIVE now. Close the newcomer's
                                    // session and restore the parked caller
                                    // directly (no further CHLD — their leg
                                    // already has the line).
                                    flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                    if let Some(ended) = tracker.terminate() {
                                        emit_call_ended(
                                            &ended,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                    }
                                    let (sess_b, ctx_b) = parked
                                        .take()
                                        .expect("cascade runs with a parked caller");
                                    let was_greeted = sess_b.greeted;
                                    let b_id = sess_b.id.clone();
                                    let b_from_leg =
                                        sess_b.caller_id.clone().unwrap_or_default();
                                    match tracker.restore(sess_b) {
                                        Ok(_gen) => {
                                            *pending_ctx_restore = Some(ctx_b);
                                            if !was_greeted {
                                                *promote_greet_for = Some(b_id.clone());
                                            } else {
                                                *resume_line_for = Some((
                                                    b_id.clone(),
                                                    std::time::Instant::now(),
                                                ));
                                            }
                                            *status.parked_call.lock().unwrap() = None;
                                            status.call_active.store(true, Ordering::Relaxed);
                                            *status.current_call_id.lock().unwrap() =
                                                Some(b_id);
                                            *status.current_caller.lock().unwrap() =
                                                if b_from_leg.is_empty() {
                                                    None
                                                } else {
                                                    Some(b_from_leg)
                                                };
                                            *status.call_started_at.lock().unwrap() = tracker
                                                .current()
                                                .map(|s| s.started_at_iso.clone());
                                            status
                                                .switchboard_revision
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(sess_back) => {
                                            eprintln!("[aokie-plugin] SWITCHBOARD: cascade restore refused after newcomer loss — caller stays parked");
                                            *parked = Some((sess_back, ctx_b));
                                        }
                                    }
                                }
                                SwapBackVerdict::ActiveDied => {
                                    // The newcomer died mid-swap and the
                                    // parked caller is still held. Close the
                                    // newcomer; the normal retrieve path
                                    // takes it from here on a later pass.
                                    flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                    if let Some(ended) = tracker.terminate() {
                                        emit_call_ended(
                                            &ended,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                    }
                                    status.call_active.store(false, Ordering::Relaxed);
                                    *status.current_call_id.lock().unwrap() = None;
                                    *status.current_caller.lock().unwrap() = None;
                                    *status.call_started_at.lock().unwrap() = None;
                                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                }
                                SwapBackVerdict::AllGone => {
                                    flush_incoming_if_pending(&mut *tracker, outbox, sink);
                                    if let Some(ended) = tracker.terminate() {
                                        emit_call_ended(
                                            &ended,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                    }
                                    if let Some((sess_b, _ctx_b)) = parked.take() {
                                        let b_intent = parked_end_intent(&sess_b);
                                        let ended = crate::call_session::SessionTracker::terminate_detached(sess_b, Some(b_intent));
                                        emit_call_ended(
                                            &ended,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                    }
                                    *status.parked_call.lock().unwrap() = None;
                                    status.call_active.store(false, Ordering::Relaxed);
                                    *status.current_call_id.lock().unwrap() = None;
                                    *status.current_caller.lock().unwrap() = None;
                                    *status.call_started_at.lock().unwrap() = None;
                                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                    eprintln!("[aokie-plugin] SWITCHBOARD: cascade lost every leg — line is idle");
                                }
                                SwapBackVerdict::StrangerActive => {
                                    // The cascade swap collided with a fresh
                                    // knock: the phone answered the STRANGER
                                    // and sacrificed the held (longest-
                                    // waiting) caller. Close them honestly,
                                    // park the current newcomer, mint the
                                    // stranger as the foreground call.
                                    if let Some((sess_b, _ctx_b)) = parked.take() {
                                        let b_intent = parked_end_intent(&sess_b);
                                        let ended = crate::call_session::SessionTracker::terminate_detached(sess_b, Some(b_intent));
                                        emit_call_ended(
                                            &ended,
                                            status.config_version.load(Ordering::Relaxed),
                                            outbox,
                                            sink,
                                        );
                                    }
                                    if let Some(sess_c) = tracker.park() {
                                        let c2_id = sess_c.id.clone();
                                        let c2_from =
                                            sess_c.caller_id.clone().unwrap_or_default();
                                        let ctx_c = std::mem::replace(
                                            &mut *ctx,
                                            CallVoiceContext::fresh(None),
                                        );
                                        *parked = Some((sess_c, ctx_c));
                                        *status.parked_call.lock().unwrap() =
                                            Some(SwitchboardLeg {
                                                call_id: c2_id,
                                                from: c2_from,
                                                since_iso: aokie_core::events::now_iso8601(),
                                            });
                                    } else {
                                        *status.parked_call.lock().unwrap() = None;
                                    }
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
                                            format!("call_{}", uuid::Uuid::new_v4().simple()),
                                            String::new(),
                                        ),
                                    };
                                    tracker
                                        .ring(s_id.clone(), aokie_core::events::now_iso8601());
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
                                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                                    eprintln!("[aokie-plugin] SWITCHBOARD: cascade swap collided with a new knock — the new caller has the line");
                                }
                                SwapBackVerdict::Inconclusive => {
                                    // Structurally unreachable — same
                                    // defensive road as the juggle: the
                                    // newcomer keeps the line, the parked
                                    // caller waits for auto-retrieve.
                                    *promote_greet_for = Some(c_id.clone());
                                    *pending_ctx_restore = Some(std::mem::replace(
                                        &mut *ctx,
                                        CallVoiceContext::fresh(None),
                                    ));
                                    eprintln!("[aokie-plugin] SWITCHBOARD: unresolved cascade verdict — newcomer keeps the line");
                                }
                            }
                        }
                    }
                }
                #[cfg(not(feature = "voice"))]
                let _ = &w;
            } else if !switch_recent {
                let held_state = status.call_held_state.load(Ordering::Relaxed);
                if held_state == 1 {
                    // Someone is ALREADY active on the phone with someone
                    // held — the "retrieve" premise is wrong, and a
                    // CHLD=2 here would SWAP, activating the WRONG leg
                    // (live incident 2026-07-15 round 2: a transitional
                    // CLCC misjudged the newcomer dead; the phone had
                    // actually finished the swap, so the blind retrieve
                    // re-held the primary and gave the line to a
                    // sessionless leg). The parked caller's leg is the
                    // active one — restore the session DIRECTLY, no wire
                    // command.
                    let (sess, ctx_saved) = parked.take().expect("checked above");
                    eprintln!(
                        "[aokie-plugin] SWITCHBOARD: foreground ended but a leg is already ACTIVE (callheld=1) — restoring {} without CHLD",
                        sess.id
                    );
                    let resumed_id = sess.id.clone();
                    let resumed_from = sess.caller_id.clone();
                    let was_greeted = sess.greeted;
                    match tracker.restore(sess) {
                        Ok(_generation) => {
                            *pending_ctx_restore = Some(ctx_saved);
                            if !was_greeted {
                                *promote_greet_for = Some(resumed_id.clone());
                            } else {
                                *resume_line_for =
                                    Some((resumed_id.clone(), std::time::Instant::now()));
                            }
                            status.call_active.store(true, Ordering::Relaxed);
                            *status.current_call_id.lock().unwrap() = Some(resumed_id);
                            *status.current_caller.lock().unwrap() = resumed_from;
                            *status.call_started_at.lock().unwrap() =
                                tracker.current().map(|s| s.started_at_iso.clone());
                            *status.parked_call.lock().unwrap() = None;
                            status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(sess_back) => {
                            eprintln!(
                                "[aokie-plugin] SWITCHBOARD: a new call raced the direct restore — {} stays parked",
                                sess_back.id
                            );
                            *parked = Some((sess_back, ctx_saved));
                        }
                    }
                } else if held_state == 0 {
                    // Nothing held on the phone — the parked caller's leg
                    // is already gone. Close them honestly instead of
                    // firing a CHLD into an empty line and resurrecting a
                    // dead session.
                    let (sess, _ctx_gone) = parked.take().expect("checked above");
                    eprintln!(
                        "[aokie-plugin] SWITCHBOARD: foreground ended but nothing is held (callheld=0) — parked caller {} is gone",
                        sess.id
                    );
                    let sess_intent = parked_end_intent(&sess);
                    let ended = crate::call_session::SessionTracker::terminate_detached(
                        sess,
                        Some(sess_intent),
                    );
                    emit_call_ended(
                        &ended,
                        status.config_version.load(Ordering::Relaxed),
                        outbox,
                        sink,
                    );
                    *status.parked_call.lock().unwrap() = None;
                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                } else {
                    // Held-only (callheld=2): retrieve the parked caller with
                    // one CHLD=2 (only a held call remains, so the toggle
                    // retrieves), session + context restored.
                    let (sess, ctx_saved) = parked.take().expect("checked above");
                    eprintln!(
                    "[aokie-plugin] SWITCHBOARD: foreground ended with {} parked — retrieving them (AT+CHLD=2)",
                    sess.id
                );
                    *status.switch_in_flight.lock().unwrap() =
                        Some(("auto_retrieve".to_string(), std::time::Instant::now()));
                    status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                    bt.flush_tx_audio();
                    let _ = bt.hold_swap();
                    let resumed_id = sess.id.clone();
                    let resumed_from = sess.caller_id.clone();
                    let was_greeted = sess.greeted;
                    match tracker.restore(sess) {
                        Ok(_generation) => {
                            *pending_ctx_restore = Some(ctx_saved);
                            // A held caller who never got past the "please
                            // hold" line is now given full attention: the
                            // greeting block speaks the "thanks for holding"
                            // line. A caller parked MID-conversation instead
                            // hears the resume line (a silent return was the
                            // live complaint).
                            if !was_greeted {
                                *promote_greet_for = Some(resumed_id.clone());
                            } else {
                                *resume_line_for =
                                    Some((resumed_id.clone(), std::time::Instant::now()));
                            }
                            status.call_active.store(true, Ordering::Relaxed);
                            *status.current_call_id.lock().unwrap() = Some(resumed_id);
                            *status.current_caller.lock().unwrap() = resumed_from;
                            *status.call_started_at.lock().unwrap() =
                                tracker.current().map(|s| s.started_at_iso.clone());
                            *status.parked_call.lock().unwrap() = None;
                            status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(sess_back) => {
                            // A fresh ring raced the retrieve — keep them parked.
                            eprintln!(
                            "[aokie-plugin] SWITCHBOARD: a new call raced the retrieve — {} stays parked",
                            sess_back.id
                        );
                            *parked = Some((sess_back, ctx_saved));
                        }
                    }
                }
            }
        } else if held_now == 2 && *prev_call_held != 2 && !switch_recent {
            // callheld=2 = "held, NO active": the FOREGROUND leg ended on
            // the phone side WITHOUT a CallTerminated edge (the call
            // indicator stays 1 while the held leg lives). Close the
            // foreground truthfully; the next pass retrieves the parked
            // caller via the branch above.
            eprintln!(
                "[aokie-plugin] SWITCHBOARD: callheld=2 — the foreground call ended on the phone side; closing it"
            );
            let physically_ended_call_id = tracker.call_id().map(str::to_owned);
            flush_incoming_if_pending(&mut *tracker, outbox, sink);
            status.call_active.store(false, Ordering::Relaxed);
            if let Some(ended) = tracker.terminate() {
                emit_call_ended(
                    &ended,
                    status.config_version.load(Ordering::Relaxed),
                    outbox,
                    sink,
                );
            }
            *status.current_caller.lock().unwrap() = None;
            *status.current_call_id.lock().unwrap() = None;
            *status.call_started_at.lock().unwrap() = None;
            if let Some(call_id) = physically_ended_call_id {
                complete_companion_end_caller(&mut *pending_companion_end_caller, &call_id);
            }
        } else if held_now == 0 && *prev_call_held != 0 && !switch_recent {
            // callheld=0 with a caller parked and no CHLD from us: the
            // PARKED leg vanished — they hung up while on hold.
            if let Some((sess, _ctx_gone)) = parked.take() {
                eprintln!(
                    "[aokie-plugin] SWITCHBOARD: parked caller {} hung up while on hold",
                    sess.id
                );
                let sess_intent = parked_end_intent(&sess);
                let ended = crate::call_session::SessionTracker::terminate_detached(
                    sess,
                    Some(sess_intent),
                );
                emit_call_ended(
                    &ended,
                    status.config_version.load(Ordering::Relaxed),
                    outbox,
                    sink,
                );
                *status.parked_call.lock().unwrap() = None;
                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    *prev_call_held = status.call_held_state.load(Ordering::Relaxed);

    // Phase 4: a knocker who gave up UNSERVED becomes an honest MISSED
    // call (deferred ~2.5s so an accept or promotion can claim the knock
    // first) — the missed-call flow then rings them back. Before this, a
    // caller who rang out while both slots were busy left NO record and
    // never got a callback (live gap, user report 2026-07-15).
    {
        let pending = status.gave_up_knock.lock().unwrap().clone();
        if !pending.is_empty() {
            let suffix_of = |num: Option<&str>| -> Option<String> {
                num.map(crate::screen::digit_suffix)
                    .filter(|s| s.len() >= 6)
            };
            let mut keep: Vec<(SwitchboardLeg, std::time::Instant, u64)> = Vec::new();
            // Same env-derived truth the screening itself runs on (the
            // classifier is not voice-gated, so the live-reloaded local
            // is out of reach here — from_env matches emit_call_ended).
            let screen = crate::screen::ScreenPolicy::from_env();
            for (leg, ended_at, gen_at_end) in pending {
                // Screened callers leave NO missed-call record: they were
                // never going to be served, and ringing them back would
                // undo the block (user policy 2026-07-15).
                if screen
                    .verdict(if leg.from.is_empty() {
                        None
                    } else {
                        Some(leg.from.as_str())
                    })
                    .is_some()
                {
                    eprintln!(
                        "[aokie-plugin] screened caller {} gave up — no missed-call record by policy",
                        leg.call_id
                    );
                    continue;
                }
                let claimed_by_id = tracker.current().is_some_and(|s| s.id == leg.call_id)
                    || parked.as_ref().is_some_and(|(s, _)| s.id == leg.call_id);
                let knock_suffix = crate::screen::digit_suffix(&leg.from);
                let claimed_by_number = knock_suffix.len() >= 6
                    && (suffix_of(tracker.current().and_then(|s| s.caller_id.as_deref()))
                        .as_deref()
                        == Some(knock_suffix.as_str())
                        || suffix_of(
                            parked.as_ref().and_then(|(s, _)| s.caller_id.as_deref()),
                        )
                        .as_deref()
                            == Some(knock_suffix.as_str()));
                // Withheld number: any fresh session since the episode
                // ended is almost certainly the promotion answering them —
                // never record a miss we cannot verify.
                let anonymous_claim = leg.from.is_empty() && tracker.generation() != gen_at_end;
                if claimed_by_id || claimed_by_number || anonymous_claim {
                    continue; // served — no record
                }
                if ended_at.elapsed() < std::time::Duration::from_millis(2500) {
                    keep.push((leg, ended_at, gen_at_end));
                    continue;
                }
                eprintln!(
                    "[aokie-plugin] waiting caller {} gave up unserved — recording an honest missed call",
                    leg.call_id
                );
                // Classification only: this says the ANI matches the
                // configured owner line so customer-facing follow-up can
                // stay away. It grants no manager authority; a waiting
                // caller never completed the independent PIN challenge.
                let manager = manager_number_classification(
                    false,
                    if leg.from.is_empty() {
                        None
                    } else {
                        Some(leg.from.as_str())
                    },
                );
                let ended_data = json!({
                    "at": aokie_core::events::now_iso8601(),
                    "reason": "gave_up_waiting",
                    "callId": leg.call_id,
                    "from": leg.from,
                    "callerPhone": leg.from,
                    "durationSeconds": 0,
                    "durationMs": 0,
                    "outcome": "missed",
                    "direction": "inbound",
                    "manager": manager,
                    "configVersion": status.config_version.load(Ordering::Relaxed),
                });
                emit(
                    outbox,
                    sink,
                    aokie_core::events::aokie_event(
                        crate::contract::events::CALL_ENDED,
                        &leg.call_id,
                        ended_data.clone(),
                    ),
                );
                TRANSCRIPT_SETTLEMENTS.with(|tracker| {
                    tracker.borrow_mut().call_ended(
                        &leg.call_id,
                        ended_data,
                        std::time::Instant::now(),
                    )
                });
            }
            // Entries pushed by a settle pump later in this pass are
            // appended AFTER this store (same thread) — nothing is lost
            // by the write-back.
            *status.gave_up_knock.lock().unwrap() = keep;
        }
    }
    {
        // Settle the in-flight switch marker once its window passed.
        let mut in_flight = status.switch_in_flight.lock().unwrap();
        if in_flight
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= std::time::Duration::from_secs(4))
        {
            *in_flight = None;
        }
    }
}
