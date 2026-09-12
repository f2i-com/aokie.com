//! `handle_event`: mapping bluetooth events onto sessions and `aokie.*` events.

#[allow(unused_imports)]
use super::*;

/// Map one `BluetoothEvent` to the `aokie.*` contract: drive the call-session
/// state machine (audit AK-001) + mirror shared status. Available on all
/// targets (it only touches `BluetoothEvent`, which is not Windows-gated) so
/// the whole lifecycle stays unit-testable.
pub(super) fn handle_event(
    ev: aokie_dongle::bluetooth::BluetoothEvent,
    tracker: &mut crate::call_session::SessionTracker,
    outbox: OutboxRef<'_>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
) {
    use aokie_core::events::{aokie_event, aokie_event_occurrence, now_iso8601, occurrence_id};
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    // Radio-lifecycle incidents share the `radio` correlation but are DISTINCT
    // occurrences (replug → a second dongle.ready, reconnect → a second
    // phone.connected, every hardware error is its own incident): each mints a
    // fresh occurrence id HERE — once, at detection — so repeats aren't
    // silently dropped by key collision while replays of one occurrence keep
    // one key (audit AOK-EVENT-001).
    match ev {
        E::Initialized(addr) => {
            status.initialized.store(true, Ordering::Relaxed);
            *status.local_address.lock().unwrap() = Some(addr.clone());
            *status.last_error.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::DONGLE_READY,
                    "radio",
                    &occurrence_id(),
                    json!({"address": addr, "source": "radio"}),
                ),
            );
        }
        E::DeviceConnected(addr) => {
            status.connected.store(true, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = Some(addr.clone());
            // A working phone link supersedes whatever transient error last
            // landed (a stalled reconnect attempt, a keepalive hiccup): the
            // slot otherwise held health "degraded — radio error: …" FOREVER
            // with no way to clear it (live report 2026-07-13). The full
            // history stays in the Hardware Events records + desktop log;
            // this slot means "why the line is not working RIGHT NOW".
            *status.last_error.lock().unwrap() = None;
            {
                let mut paired = status.paired.lock().unwrap();
                if !paired.iter().any(|d| d.address == addr) {
                    paired.push(PairedDevice {
                        address: addr.clone(),
                        name: "Paired phone".to_string(),
                    });
                }
            }
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::PHONE_CONNECTED,
                    "radio",
                    &occurrence_id(),
                    json!({"address": addr}),
                ),
            );
        }
        E::DeviceDisconnected(addr) => {
            // A live session cannot outlive its radio link (audit
            // AOK-LIF-003): synthesize the terminal outcome NOW — otherwise
            // call.current and the operator UI stay "live" on hardware that
            // is gone. A late real CallTerminated lands on an idle tracker
            // and is a no-op (no duplicate terminal event).
            if tracker.current().is_some() {
                flush_incoming_if_pending(tracker, outbox, sink);
                status.call_active.store(false, Ordering::Relaxed);
                tracker.note_intent(crate::call_session::TerminationIntent::DeviceLost);
                if let Some(ended) = tracker.terminate() {
                    eprintln!(
                        "[aokie-plugin] phone link lost during call {} — synthesized termination (outcome {})",
                        ended.id, ended.outcome
                    );
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
            }
            status.connected.store(false, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::PHONE_DISCONNECTED,
                    "radio",
                    &occurrence_id(),
                    json!({"address": addr}),
                ),
            );
        }
        E::CallIncoming => {
            // Some phones emit the ring / callsetup indicator more than once
            // for a single call — the tracker starts a NEW session only when
            // idle, so incoming + greeting fire exactly once per call.
            let id = format!("call_{}", uuid::Uuid::new_v4().simple());
            if let Some(s) = tracker.ring(id, now_iso8601()) {
                // Shared call identity: `call.current` recovers a live call
                // from these after a browser refresh (audit C-02), and call
                // controls verify their `callId` against it (audit C-01).
                *status.current_caller.lock().unwrap() = None;
                *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                *status.call_started_at.lock().unwrap() = Some(s.started_at_iso.clone());
            }
        }
        E::OutgoingDialing => {
            // Phase 2: an OUTBOUND call setup (callsetup,2). If WE just
            // dialed and the tracker is idle — our dial session was killed
            // by a stale terminate from the previous call (live incident
            // 2026-07-14) or the setup indicator simply beat the control
            // path — attach this setup to the REAL dial: agent-owned, the
            // ORIGINAL call id, so the opening-line overlay and the
            // outbound.dialing event correlation stay intact. Otherwise
            // it's a handset-originated dial we merely OBSERVE: session
            // tracked for call-state truth, but the receptionist stays
            // silent and deaf on the owner's own call.
            let pending: Option<(String, String)> = status
                .pending_dial
                .lock()
                .unwrap()
                .as_ref()
                .filter(|p| p.at.elapsed() < std::time::Duration::from_secs(10))
                .map(|p| (p.call_id.clone(), p.number.clone()));
            if let Some((call_id, number)) = pending {
                if tracker.current().is_none() {
                    if let Some(s) =
                        tracker.dial(call_id, Some(number.clone()), now_iso8601(), true)
                    {
                        eprintln!(
                            "[aokie-plugin] outbound setup attached to our pending dial ({}) — agent owns the call",
                            s.id
                        );
                        *status.outbound_call_id.lock().unwrap() = Some(s.id.clone());
                        *status.current_caller.lock().unwrap() = Some(number);
                        *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                        *status.call_started_at.lock().unwrap() = Some(s.started_at_iso.clone());
                    }
                }
                // Tracker busy = our dial session is already live — the
                // indicator echo is expected; nothing to do.
                return;
            }
            let id = format!("call_{}", uuid::Uuid::new_v4().simple());
            if let Some(s) = tracker.dial(id, None, now_iso8601(), false) {
                eprintln!(
                    "[aokie-plugin] OUTBOUND call setup observed ({}) — receptionist stays out of it",
                    s.id
                );
                *status.outbound_call_id.lock().unwrap() = Some(s.id.clone());
                *status.current_caller.lock().unwrap() = None;
                *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                *status.call_started_at.lock().unwrap() = Some(s.started_at_iso.clone());
            }
        }
        E::CallerId(num) => {
            // The FIRST time this call learns a (non-empty) number, announce
            // it: with instant auto-answer the ringing-phase +CLIP usually
            // loses the race, so `call.incoming` often went out with an empty
            // `from` — this event (fed by +CLIP or the AT+CLCC rescue) is
            // what lets flows personalize the LIVE call (greet a matched
            // customer by name). Emitted at most once per call: +CLIP repeats
            // per ring and +CLCC answers too, and the idempotency key
            // (corr + `caller_id` step) must be minted exactly once.
            // OUTBOUND sessions never mint caller_id events (Phase 2): the
            // number is the DIALED remote, and announcing it would run the
            // personalize/screening flows against our own call — whitelist
            // mode would REJECT the call we just placed.
            let newly_known = !num.trim().is_empty()
                && tracker.current().is_some_and(|s| {
                    !s.outbound && s.caller_id.as_deref().unwrap_or("").is_empty()
                });
            tracker.caller_id(num.clone());
            if newly_known {
                // Lifecycle order (AOK-LIF-001): the number is known now, so
                // the held `incoming` can flush WITH it — and must go first.
                flush_incoming_if_pending(tracker, outbox, sink);
                if let Some(corr) = tracker.call_id() {
                    emit(
                        outbox,
                        sink,
                        aokie_event(
                            crate::contract::events::CALL_CALLER_ID,
                            corr,
                            json!({"callId": corr, "from": num.clone(), "at": now_iso8601()}),
                        ),
                    );
                }
            }
            *status.current_caller.lock().unwrap() = Some(num);
        }
        E::CallWaiting { number } => {
            // Phase 4 observe-only lane: a SECOND caller is knocking while a
            // call is active (call-waiting-negotiated connections only). No
            // hold/switch happens yet — the waiting caller hears the
            // network's tone — but the knock is recorded durably so flows
            // can follow up (and so live soak data accumulates for the
            // switchboard slice). One event per episode; a late number for
            // an anonymously-started episode is log-only.
            let num = number.unwrap_or_default();
            let Some(corr) = tracker.call_id().map(str::to_string) else {
                eprintln!("[aokie-plugin] call-waiting signal with no tracked call — ignored");
                return;
            };
            {
                let mut announced = status.call_waiting_announced.lock().unwrap();
                if announced.as_deref() == Some(corr.as_str()) {
                    if !num.is_empty() {
                        eprintln!(
                            "[aokie-plugin] waiting caller identified: {}",
                            aokie_core::redact::Phone(&num)
                        );
                        // Late number for an anonymously-started episode:
                        // the switchboard leg keeps its minted id.
                        if let Some(leg) = status.waiting_call.lock().unwrap().as_mut() {
                            if leg.from.is_empty() {
                                leg.from = num;
                            }
                        }
                    }
                    return;
                }
                *announced = Some(corr.clone());
            }
            // Phase 4 switchboard: the waiting caller gets a STABLE identity
            // at the knock — `call.activate` targets it, and if they are
            // accepted this becomes their session's call id.
            let waiting_id = format!("call_{}", uuid::Uuid::new_v4().simple());
            *status.waiting_call.lock().unwrap() = Some(SwitchboardLeg {
                call_id: waiting_id.clone(),
                from: num.clone(),
                since_iso: now_iso8601(),
            });
            status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
            status.call_waiting_episodes.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[aokie-plugin] SECOND CALLER waiting during call {} ({}) as {} — the active call continues; call.activate can accept them",
                corr,
                if num.is_empty() {
                    "number withheld/unknown".to_string()
                } else {
                    aokie_core::redact::Phone(&num).to_string()
                },
                waiting_id,
            );
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::CALL_WAITING,
                    &corr,
                    &occurrence_id(),
                    json!({
                        "callId": corr,
                        "from": num,
                        "waitingCallId": waiting_id,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::CallWaitingEnded => {
            eprintln!("[aokie-plugin] waiting caller gone — active call untouched");
            *status.call_waiting_announced.lock().unwrap() = None;
            if let Some(leg) = status.waiting_call.lock().unwrap().take() {
                // Deferred give-up classification: an accept (juggle,
                // cascade, call.activate — all mint the session under this
                // very id) or a promotion (fresh id, same number) claims the
                // knock within moments; anything unclaimed becomes an honest
                // MISSED call so follow-ups ring the caller back. A queue,
                // so several give-ups during one long call ALL get records.
                {
                    let mut q = status.gave_up_knock.lock().unwrap();
                    if q.len() < 8 {
                        q.push((leg, std::time::Instant::now(), tracker.generation()));
                    }
                }
                status.switchboard_revision.fetch_add(1, Ordering::Relaxed);
            }
        }
        E::CallHeld { state } => {
            // Diagnostics only in the observe slice: a transition here with
            // no CHLD command from us means the OWNER juggled calls on the
            // handset while Aokie was on the line — worth seeing in logs.
            eprintln!(
                "[aokie-plugin] callheld indicator -> {state} (0 none / 1 held+active / 2 held only) — no switchboard yet, observe-only"
            );
            status
                .call_held_state
                .store(state.max(0) as u64, Ordering::Relaxed);
        }
        E::CallListEntry {
            index,
            direction,
            status: leg_status,
            multiparty,
            number,
        } => {
            // Phase 4 observe topology: fold the entry into the rolling
            // CLCC snapshot (dongle.diagnostics.callWaiting.lastClcc) —
            // entries within one response burst accumulate; a later entry
            // starts a fresh snapshot. The switchboard slice reconciles
            // hold/activate against exactly these.
            let rendered = format!(
                "idx={} dir={} status={}{}{}",
                index,
                direction,
                match leg_status {
                    0 => "active".to_string(),
                    1 => "held".to_string(),
                    2 => "dialing".to_string(),
                    3 => "alerting".to_string(),
                    4 => "incoming".to_string(),
                    5 => "waiting".to_string(),
                    other => format!("{other}"),
                },
                if multiparty { " mpty" } else { "" },
                number
                    .as_deref()
                    .map(|n| format!(" number={n}"))
                    .unwrap_or_default(),
            );
            let mut snapshot = status.clcc_snapshot.lock().unwrap();
            match snapshot.as_mut() {
                Some((started, lines))
                    if started.elapsed() < std::time::Duration::from_millis(1500)
                        && lines.len() < 8 =>
                {
                    lines.push(rendered);
                }
                _ => *snapshot = Some((std::time::Instant::now(), vec![rendered])),
            }
            drop(snapshot);
            // Structured twin — same burst window, consumed by the
            // verified-swap machinery (numbers matter there, rendering
            // doesn't).
            let leg = ClccLeg {
                index,
                status: leg_status,
                number,
            };
            let mut calls = status.clcc_calls.lock().unwrap();
            match calls.as_mut() {
                Some((started, legs))
                    if started.elapsed() < std::time::Duration::from_millis(1500)
                        && legs.len() < 8 =>
                {
                    legs.push(leg);
                }
                _ => *calls = Some((std::time::Instant::now(), vec![leg])),
            }
        }
        E::CallRinging => {
            flush_incoming_if_pending(tracker, outbox, sink);
            // Phase 2: callsetup,3 is MO alerting — classifies a
            // never-answered outbound attempt as no_answer (vs failed).
            tracker.note_alerted();
            if let Some(corr) = tracker.call_id() {
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        crate::contract::events::CALL_RINGING,
                        corr,
                        json!({"at": now_iso8601()}),
                    ),
                );
            }
        }
        E::CallAnswered => {
            // AOK-CTRL-001 recovery: an answer with NO tracked session means
            // an earlier indicator was misread as a terminate (or events were
            // lost) while the phone call is genuinely up — without a session
            // every audio frame is dropped and the receptionist goes deaf on
            // a LIVE call (observed 2026-07-13; the HFP held-verdict fix
            // prevents the known ordering, this catches any other). Rebuild a
            // session so the call is heard; the greeting replays, which also
            // tells the caller the line reset. Gated on the phone still being
            // CONNECTED: a stale answered queued behind a device-loss
            // termination must never build a phantom session on a dead link
            // (AOK-LIF-003).
            if tracker.current().is_none() && status.connected.load(Ordering::Relaxed) {
                let id = format!("call_{}", uuid::Uuid::new_v4().simple());
                eprintln!(
                    "[aokie-plugin] call ANSWERED with no tracked session — recovering as {id} (a terminate was misread or events were lost)"
                );
                if let Some(s) = tracker.ring(id, now_iso8601()) {
                    *status.current_caller.lock().unwrap() = None;
                    *status.current_call_id.lock().unwrap() = Some(s.id.clone());
                    *status.call_started_at.lock().unwrap() = Some(s.started_at_iso.clone());
                }
            }
            // Lifecycle order (audit AOK-LIF-001): incoming ALWAYS precedes
            // answered — even when the caller-ID hold hasn't elapsed yet.
            flush_incoming_if_pending(tracker, outbox, sink);
            tracker.answered();
            // Phase 2: the dial reached its call — the in-flight context has
            // done its job (a terminate from here on is REAL).
            if tracker
                .current()
                .is_some_and(|s| s.outbound && s.agent_owned)
            {
                *status.pending_dial.lock().unwrap() = None;
            }
            if let Some(corr) = tracker.call_id() {
                // Only a TRACKED call may read as active — an orphaned
                // answer that wasn't recovered (dead link) must not leave
                // `call_active` true with no session behind it.
                status.call_active.store(true, Ordering::Relaxed);
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        crate::contract::events::CALL_ANSWERED,
                        corr,
                        json!({"at": now_iso8601()}),
                    ),
                );
            }
        }
        E::CallTerminated => {
            // Phase 2 stale-verdict guard (live incident 2026-07-14): a
            // terminate landing within moments of OUR OWN dial — before the
            // attempt even ALERTED — belongs to the PREVIOUS call (a held
            // ring verdict or a late SCO-teardown synthesis), never to the
            // fresh outbound session. Consuming it killed the dial session,
            // emitted a spurious failed call.ended (which sent the apology
            // text while the callee's phone was still ringing) and left a
            // silent observed session for them to answer.
            let stale_for_dial = tracker.current().is_some_and(|s| {
                s.outbound
                    && s.agent_owned
                    && !s.is_active()
                    && !s.alerted
                    && status
                        .pending_dial
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|p| {
                            p.call_id == s.id && p.at.elapsed() < std::time::Duration::from_secs(3)
                        })
            });
            if stale_for_dial {
                eprintln!(
                    "[aokie-plugin] CallTerminated moments after our dial, before alerting — it belongs to the previous call; the outbound attempt continues"
                );
                return;
            }
            // Even an instantly-abandoned ring gets its incoming record
            // before the terminal event (audit AOK-LIF-001).
            flush_incoming_if_pending(tracker, outbox, sink);
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
        }
        E::AudioConnected {
            codec,
            sample_rate,
            armed,
        } => {
            flush_incoming_if_pending(tracker, outbox, sink);
            // SCO can drop and re-arm repeatedly within ONE call, so even with
            // a call correlation these are per-incident occurrences.
            let corr = tracker.call_id().unwrap_or("radio").to_string();
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::CALL_AUDIO_CONNECTED,
                    &corr,
                    &occurrence_id(),
                    json!({"codec": codec, "sampleRate": sample_rate, "armed": armed}),
                ),
            );
            // Silent SCO must never LOOK healthy (audit AOK-HW-001): the
            // link is up but the iso pipes didn't arm — record it where the
            // Device Setup console shows it, with the concrete recovery.
            if !armed {
                emit(
                    outbox,
                    sink,
                    aokie_event_occurrence(
                        crate::contract::events::HARDWARE_ERROR,
                        &corr,
                        &occurrence_id(),
                        json!({
                            "message": "Call audio failed to arm (SCO alternate setting) — this call will be SILENT both ways. Hang up, unplug and replug the dongle, then take the next call.",
                            "code": "sco_unarmed",
                        }),
                    ),
                );
            }
        }
        E::AudioDisconnected => {
            flush_incoming_if_pending(tracker, outbox, sink);
            let corr = tracker.call_id().unwrap_or("radio").to_string();
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::CALL_AUDIO_DISCONNECTED,
                    &corr,
                    &occurrence_id(),
                    json!({}),
                ),
            );
        }
        E::SmsReceived(p) => {
            // Stable inbound identity (audit AOK-EVENT-001): the MAP handle is
            // the AG's own stable id for this message on this phone, so a
            // re-fetch of the SAME message (MNS re-notification, reconnect
            // replay) dedupes instead of duplicating the record. Scope it by
            // device address — handles are only unique per phone. A phone
            // that sends no handle falls back to a fresh occurrence id (no
            // dedupe possible, matching the old behaviour).
            let corr = if p.handle.is_empty() {
                format!("sms_{}", occurrence_id())
            } else {
                let device = status
                    .connected_address
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default()
                    .replace(':', "");
                format!("sms_{device}_{}", p.handle)
            };
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_RECEIVED,
                    &corr,
                    json!({
                        "from": p.sender_phone,
                        "name": p.sender_name,
                        "body": p.body,
                        "handle": p.handle,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::SmsSent {
            message_id,
            recipient_phone,
        } => {
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_SENT,
                    &message_id,
                    json!({"messageId": message_id, "to": recipient_phone, "at": now_iso8601()}),
                ),
            );
        }
        E::SmsSendFailed {
            message_id,
            recipient_phone,
            reason,
        } => {
            // The radio abandoned an outbound SMS (MAS PUT failed / aged out
            // across recovery cycles). Surface it truthfully — a queued send
            // that quietly evaporates is audit C-16's exact failure mode.
            emit(
                outbox,
                sink,
                aokie_event(
                    crate::contract::events::SMS_FAILED,
                    &message_id,
                    json!({
                        "messageId": message_id,
                        "to": recipient_phone,
                        "reason": reason,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::PairingConfirmRequired {
            address,
            numeric_value,
        } => {
            // PAIR-001: surface the held SSP numeric comparison so the Desktop
            // pairing UI can prompt the operator (phone.status carries the same
            // data for pollers). Not essential/outboxed — the prompt expires in
            // seconds, so replaying it after a host restart would be wrong.
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::PHONE_PAIRING_CONFIRM_REQUIRED,
                    "radio",
                    &occurrence_id(),
                    json!({
                        "address": address,
                        "numericValue": numeric_value,
                        "at": now_iso8601(),
                    }),
                ),
            );
        }
        E::ContactsFetched(_) | E::MapNotificationsSubscribed => {
            // Phonebook / MNS-subscribe are diagnostic; nothing to surface yet.
        }
        E::Error(e) => {
            *status.last_error.lock().unwrap() = Some(e.clone());
            // Every hardware error is its own incident — without an occurrence
            // id the SECOND distinct error would collide with the first's key
            // and be silently dropped (essential/outboxed event!).
            emit(
                outbox,
                sink,
                aokie_event_occurrence(
                    crate::contract::events::HARDWARE_ERROR,
                    "radio",
                    &occurrence_id(),
                    json!({"message": e}),
                ),
            );
        }
    }
}
