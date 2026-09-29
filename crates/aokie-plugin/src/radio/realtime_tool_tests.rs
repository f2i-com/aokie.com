//! MOB-01: realtime tool intake never ends the call.
//!
//! These drive the real lane methods the service calls (`accept_tool_call`,
//! `next_refused_tool`) on a lane whose session has no socket behind it.

use super::*;

fn detached_lane(begun: bool) -> (RealtimeCallLane, crate::realtime_voice::DetachedSession) {
    let (session, detached) = crate::realtime_voice::RealtimeVoiceSession::detached("call_1", 7);
    let mut lane = RealtimeCallLane::new(session);
    lane.begun = begun;
    (lane, detached)
}

type Refused = (String, String, bool, serde_json::Value, bool);

fn refused(lane: &mut RealtimeCallLane) -> Vec<Refused> {
    std::iter::from_fn(|| lane.next_refused_tool()).collect()
}

fn queued_name(lane: &RealtimeCallLane) -> Option<&str> {
    lane.pending_tool_call.as_ref().map(|tool| tool.1.as_str())
}

#[test]
fn an_unknown_tool_is_refused_as_unsupported_and_the_call_carries_on() {
    let (mut lane, detached) = detached_lane(true);
    lane.accept_tool_call(
        "t1".into(),
        "arbitrary_desktop_action".into(),
        serde_json::json!({}),
    )
    .expect("an unknown tool must not be a protocol failure");
    assert!(lane.pending_tool_call.is_none(), "an unknown tool never runs");
    let refusals = refused(&mut lane);
    assert_eq!(
        refusals,
        vec![(
            "t1".to_string(),
            "arbitrary_desktop_action".to_string(),
            false,
            serde_json::json!({"error": "unsupported"}),
            true
        )]
    );
    // What the service does with it: one ordinary result for that call id,
    // and no stop.
    let (id, name, ok, output, continue_response) = refusals.into_iter().next().unwrap();
    lane.session
        .complete_tool(&id, &name, ok, output, continue_response)
        .expect("the refusal is a valid tool result");
    assert_eq!(
        detached.drain(),
        vec![crate::realtime_voice::SentControl::ToolResult {
            tool_call_id: "t1".into(),
            name: "arbitrary_desktop_action".into(),
            ok: false,
            output: serde_json::json!({"error": "unsupported"}),
            continue_response: true,
        }]
    );
    // The lane is untouched: an ordinary tool right after is queued to run.
    lane.accept_tool_call("t2".into(), "finish_call".into(), serde_json::json!({}))
        .unwrap();
    assert_eq!(queued_name(&lane), Some("finish_call"));
    // transfer_to_owner is not a tool on a session that did not negotiate it.
    lane.pending_tool_call = None;
    lane.accept_tool_call(
        "t3".into(),
        "transfer_to_owner".into(),
        serde_json::json!({"reason": "caller_asked"}),
    )
    .unwrap();
    assert!(lane.pending_tool_call.is_none());
    assert_eq!(
        refused(&mut lane)[0].3,
        serde_json::json!({"error": "unsupported"})
    );
}

#[test]
fn an_overlapping_slot_tool_during_a_lookup_is_busy_and_the_lookup_survives() {
    let (mut lane, _detached) = detached_lane(true);
    lane.accept_tool_call(
        "lookup".into(),
        "lookup_business_data".into(),
        serde_json::json!({"question": "Do I have anything on Tuesday?"}),
    )
    .unwrap();
    // The service moves the queued lookup into its polling slot; model that
    // by taking it out of the queue and marking the slot in flight.
    let queued = lane.pending_tool_call.take().expect("the lookup is queued");
    assert_eq!(queued.1, "lookup_business_data");
    let host = crate::host_rpc::HostRpc::new();
    let (id, _line, rx) = host.begin("flow.run", serde_json::json!({}));
    lane.pending_business_lookup = Some(PendingRealtimeBusinessLookup {
        tool_call_id: queued.0.clone(),
        name: queued.1.clone(),
        lookup: PendingBusinessLookup {
            host: host.clone(),
            id: Some(id),
            rx,
            deadline: Instant::now() + Duration::from_secs(5),
        },
    });

    lane.accept_tool_call(
        "appt".into(),
        "request_appointment".into(),
        serde_json::json!({}),
    )
    .expect("an overlapping tool is refused, never fatal");
    lane.accept_tool_call(
        "second".into(),
        "lookup_business_data".into(),
        serde_json::json!({"question": "x"}),
    )
    .expect("a second lookup is refused, never fatal");
    let refusals = refused(&mut lane);
    assert_eq!(refusals.len(), 2);
    assert!(refusals
        .iter()
        .all(|r| !r.2 && r.3 == serde_json::json!({"error": "busy"}) && r.4));
    assert_eq!(refusals[0].0, "appt");
    assert_eq!(refusals[1].0, "second");
    assert!(lane.pending_business_lookup.is_some(), "the lookup keeps polling");
    assert!(lane.pending_tool_call.is_none(), "a refused tool never queues");

    // An instant tool may run while the lookup polls.
    lane.accept_tool_call("finish".into(), "finish_call".into(), serde_json::json!({}))
        .unwrap();
    assert_eq!(queued_name(&lane), Some("finish_call"));
    // Only one place waits: the next call, of either class, is busy.
    lane.accept_tool_call("again".into(), "finish_call".into(), serde_json::json!({}))
        .unwrap();
    assert_eq!(refused(&mut lane)[0].3, serde_json::json!({"error": "busy"}));
}

#[test]
fn the_twenty_fifth_tool_call_of_a_call_is_tool_limit_not_a_hangup() {
    let (mut lane, _detached) = detached_lane(true);
    for n in 0..24 {
        lane.accept_tool_call(format!("t{n}"), "finish_call".into(), serde_json::json!({}))
            .unwrap();
        // Model the service running it, so the queue place is free again.
        lane.pending_tool_call = None;
    }
    assert!(refused(&mut lane).is_empty(), "the first 24 all ran");
    lane.accept_tool_call("t24".into(), "finish_call".into(), serde_json::json!({}))
        .expect("the 25th is refused, not fatal");
    let refusals = refused(&mut lane);
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].3, serde_json::json!({"error": "tool_limit"}));
    assert!(lane.pending_tool_call.is_none());
}

#[test]
fn a_repeated_tool_call_id_gets_no_second_result_and_a_call_before_begin_is_a_protocol_failure() {
    let (mut lane, _detached) = detached_lane(true);
    lane.accept_tool_call("same".into(), "finish_call".into(), serde_json::json!({}))
        .unwrap();
    lane.pending_tool_call = None;
    lane.accept_tool_call("same".into(), "finish_call".into(), serde_json::json!({}))
        .expect("a repeated id is ignored, not fatal");
    assert!(lane.pending_tool_call.is_none());
    assert!(refused(&mut lane).is_empty());

    // Before the call is begun no result may be sent, so this stays the one
    // failure the intake reports.
    let (mut early, _detached) = detached_lane(false);
    let error = early
        .accept_tool_call("t1".into(), "finish_call".into(), serde_json::json!({}))
        .unwrap_err();
    assert!(error.contains("before the call was begun"), "{error}");
}

#[test]
fn malformed_arguments_are_answered_by_the_tool_and_never_end_the_session() {
    // The parse layer hands null for arguments that are not an object; the
    // intake queues the call like any other, so each tool answers it in its
    // own words. Nothing here is a protocol failure.
    let (mut lane, _detached) = detached_lane(true);
    for name in ["lookup_business_data", "request_appointment", "finish_call"] {
        lane.accept_tool_call(format!("t_{name}"), name.into(), serde_json::Value::Null)
            .expect(name);
        assert_eq!(queued_name(&lane), Some(name));
        assert!(lane.pending_tool_call.as_ref().unwrap().2.is_null());
        lane.pending_tool_call = None;
    }
    // finish_call refuses a null or non-empty argument object in its own
    // result rather than by ending the call.
    assert!(!realtime_finish_call_allowed(true, false, 0, 0));
}

// --- MOB-10: the transfer tool on the lane -------------------------------------

use crate::transfer::call::Effect;
use crate::transfer::{Outcome, OutcomeFrame, ToolAnswer, TOOL_NAME};

fn features(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

#[test]
fn the_transfer_tool_exists_only_when_the_start_offered_it_and_oaiy_says_it_implements_it() {
    // Offered, and OAIY lists it: negotiated.
    let (mut lane, _detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    assert!(lane.transfer_negotiated);
    // An older OAIY sends no features at all: nothing changes for it.
    let (mut lane, _detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    lane.negotiate_transfer(&[]);
    assert!(!lane.transfer_negotiated);
    lane.negotiate_transfer(&features(&["something_else"]));
    assert!(!lane.transfer_negotiated);
    // A feature OAIY lists that the start never offered enables nothing.
    let (mut lane, _detached) = detached_lane(true);
    lane.allow_transfer_sent = false;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    assert!(!lane.transfer_negotiated);
    lane.accept_tool_call(
        "t1".into(),
        TOOL_NAME.into(),
        serde_json::json!({"reason": "caller_asked"}),
    )
    .unwrap();
    assert!(lane.pending_tool_call.is_none());
    assert_eq!(refused(&mut lane)[0].3, serde_json::json!({"error": "unsupported"}));
}

#[test]
fn a_negotiated_transfer_tool_is_an_instant_tool_that_may_run_beside_a_lookup() {
    let (mut lane, _detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    let host = crate::host_rpc::HostRpc::new();
    let (id, _line, rx) = host.begin("flow.run", serde_json::json!({}));
    lane.pending_business_lookup = Some(PendingRealtimeBusinessLookup {
        tool_call_id: "lookup".into(),
        name: "lookup_business_data".into(),
        lookup: PendingBusinessLookup {
            host: host.clone(),
            id: Some(id),
            rx,
            deadline: Instant::now() + Duration::from_secs(5),
        },
    });
    lane.accept_tool_call(
        "transfer".into(),
        TOOL_NAME.into(),
        serde_json::json!({"reason": "caller_asked"}),
    )
    .unwrap();
    assert_eq!(queued_name(&lane), Some(TOOL_NAME));
    assert!(refused(&mut lane).is_empty(), "it was not refused as busy");
    // The lookup keeps its slot.
    assert!(lane.pending_business_lookup.is_some());
}

#[test]
fn transfer_effects_reach_oaiy_only_as_typed_results_and_outcomes_on_a_negotiated_session() {
    let (mut lane, detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    let mut sink = crate::event_bridge::VecSink::default();
    let outcome = OutcomeFrame {
        request_id: "assist_1".into(),
        outcome: Outcome::Declined,
        message: Some("Ring after five".into()),
        at_ms: 1,
    };
    let audit = crate::radio::AssistanceAuditLifecycle::opened("assist_1", "call_1").1;
    let mut transfer = machine();
    apply_transfer_effects(
        vec![
            Effect::ToolAnswer {
                tool_call_id: "tool_1".into(),
                answer: ToolAnswer {
                    ok: true,
                    output: serde_json::json!({"status": "ringing"}),
                },
            },
            Effect::Outcome(outcome.clone()),
            Effect::Audit(audit),
        ],
        Some(&mut lane),
        &mut transfer,
        None,
        &mut sink,
    );
    // The outcome goes out at once. The answer to the tool call does not: it is
    // queued for the completion block, which sends it only once the line the
    // model spoke before calling has drained, like every other completion.
    assert_eq!(
        detached.drain(),
        vec![crate::realtime_voice::SentControl::TransferOutcome { frame: outcome.clone() }]
    );
    assert_eq!(
        lane.next_queued_answer(),
        Some((
            "tool_1".to_string(),
            TOOL_NAME.to_string(),
            true,
            serde_json::json!({"status": "ringing"}),
            true
        ))
    );
    assert_eq!(lane.next_queued_answer(), None);
    assert_eq!(sink.lines.len(), 1, "the audit event was emitted");
    assert!(sink.lines[0].contains(crate::contract::events::CALL_ASSISTANCE_REQUESTED));

    // A session that did not negotiate the contract never receives an outcome,
    // and nothing is kept for it; the audit is still written.
    let (mut old, old_detached) = detached_lane(true);
    old.ready = true;
    let mut old_transfer = machine();
    let mut sink = crate::event_bridge::VecSink::default();
    let audit = crate::radio::AssistanceAuditLifecycle::opened("assist_2", "call_1").1;
    apply_transfer_effects(
        vec![Effect::Outcome(outcome.clone()), Effect::Audit(audit.clone())],
        Some(&mut old),
        &mut old_transfer,
        None,
        &mut sink,
    );
    assert!(old_detached.drain().is_empty());
    assert!(!old_transfer.has_held_outcomes());
    assert_eq!(sink.lines.len(), 1);
    // Nor is one kept for a session whose start never offered the contract,
    // ready or not: it can never carry it.
    let (mut never, never_detached) = detached_lane(true);
    let mut never_transfer = machine();
    apply_transfer_effects(
        vec![Effect::Outcome(outcome)],
        Some(&mut never),
        &mut never_transfer,
        None,
        &mut sink,
    );
    assert!(never_detached.drain().is_empty());
    assert!(!never_transfer.has_held_outcomes());
}

fn machine() -> crate::transfer::call::TransferCall {
    crate::transfer::call::TransferCall::new(std::sync::Arc::new(
        crate::transfer::call::Governor::default(),
    ))
}

fn frame(outcome: Outcome) -> OutcomeFrame {
    OutcomeFrame {
        request_id: "assist_1".into(),
        outcome,
        message: None,
        at_ms: 1,
    }
}

fn sent_outcomes(detached: &crate::realtime_voice::DetachedSession) -> Vec<Outcome> {
    detached
        .drain()
        .into_iter()
        .filter_map(|sent| match sent {
            crate::realtime_voice::SentControl::TransferOutcome { frame } => Some(frame.outcome),
            _ => None,
        })
        .collect()
}

/// Review finding 3. A setup failure the gateway records after the fresh
/// session has started, or before it has said `ready`, is not lost: it is
/// kept and sent once the session negotiates the contract.
#[test]
fn an_outcome_with_no_negotiated_session_waits_for_the_next_one_and_is_sent_in_order() {
    let mut transfer = machine();
    let mut sink = crate::event_bridge::VecSink::default();

    // No session at all (the handoff stopped it): held. An acceptance is not.
    apply_transfer_effects(
        vec![
            Effect::Outcome(frame(Outcome::Accepted)),
            Effect::Outcome(frame(Outcome::Unavailable)),
        ],
        None,
        &mut transfer,
        None,
        &mut sink,
    );
    assert!(transfer.has_held_outcomes());

    // The fresh session has started but not said `ready`: still nothing to send on.
    let (mut lane, detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    send_held_outcomes(Some(&mut lane), &mut transfer);
    assert!(sent_outcomes(&detached).is_empty());
    assert!(transfer.has_held_outcomes());
    // A second terminal outcome arrives meanwhile and queues behind the first.
    apply_transfer_effects(
        vec![Effect::Outcome(frame(Outcome::Cancelled))],
        Some(&mut lane),
        &mut transfer,
        None,
        &mut sink,
    );
    assert!(sent_outcomes(&detached).is_empty(), "the session may still negotiate");

    // OAIY says `ready` with the feature: everything goes, oldest first, once.
    lane.ready = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    send_held_outcomes(Some(&mut lane), &mut transfer);
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Unavailable, Outcome::Cancelled]);
    assert!(!transfer.has_held_outcomes());
    send_held_outcomes(Some(&mut lane), &mut transfer);
    assert!(sent_outcomes(&detached).is_empty());

    // A session that turns out not to implement the contract is never sent
    // anything, and what is held for it stays held for nobody.
    let mut transfer = machine();
    transfer.hold_outcome(frame(Outcome::Unavailable));
    let (mut old, old_detached) = detached_lane(true);
    old.allow_transfer_sent = true;
    old.ready = true;
    old.negotiate_transfer(&[]);
    send_held_outcomes(Some(&mut old), &mut transfer);
    assert!(sent_outcomes(&old_detached).is_empty());
}

/// The two places that lay a call's context aside (the per-call reset and the
/// hold juggle) go through `withdraw_open_transfer`, on a real machine with a
/// real request in a private mailbox.
struct Laid {
    rig: crate::transfer::call::tests::Rig,
    ctx: CallVoiceContext,
    tracker: crate::call_session::SessionTracker,
    status: RadioStatus,
    sink: crate::event_bridge::VecSink,
    request_id: String,
}

fn laid_aside_with_a_ringing_request() -> Laid {
    let mut rig = crate::transfer::call::tests::Rig::new();
    let (request_id, _) = rig.ring();
    let mut ctx = CallVoiceContext::fresh(None);
    ctx.transfer = std::mem::replace(&mut rig.machine, machine());
    assert!(ctx.transfer.is_active());
    assert!(rig.broker.is_busy());
    Laid {
        rig,
        ctx,
        tracker: crate::call_session::SessionTracker::new(),
        status: RadioStatus::default(),
        sink: crate::event_bridge::VecSink::default(),
        request_id,
    }
}

impl Laid {
    fn withdraw(&mut self, why: TransferWithdrawal, lane: Option<&mut RealtimeCallLane>) {
        withdraw_open_transfer(
            why,
            &mut self.ctx,
            lane,
            &self.rig.broker,
            &self.tracker,
            &self.rig.media,
            &self.rig.host,
            &self.status,
            None,
            &mut self.sink,
        );
    }
}

/// Review finding 6: a call parked by the hold juggle (no session: the juggle
/// disposed of it first) frees the mailbox, closes the audit trail, and keeps
/// the `cancelled` for the session it gets when it resumes.
#[test]
fn a_call_parked_by_the_juggle_frees_the_mailbox_and_owes_its_next_session_a_cancelled() {
    let mut laid = laid_aside_with_a_ringing_request();
    // The juggle lays the context aside through `stow_call_context`.
    let stowed = stow_call_context(
        &mut laid.ctx,
        CallVoiceContext::fresh(None),
        &laid.rig.broker,
        &laid.tracker,
        &laid.rig.media,
        &laid.rig.host,
        &laid.status,
        None,
        &mut laid.sink,
    );
    assert!(!stowed.transfer.is_active(), "the stowed context holds no open request");
    assert!(!laid.ctx.transfer.is_active(), "the newcomer's context is a fresh one");
    laid.ctx = stowed;
    assert!(!laid.rig.broker.is_busy(), "the newcomer's requests are not blocked");
    assert!(!laid.rig.broker.transfer_admits(&laid.request_id, "thumb_phone_0123456789"));
    assert!(!laid.ctx.transfer.is_active());
    assert_eq!(laid.sink.lines.len(), 1, "the audit trail closes the request it opened");
    assert!(laid.sink.lines[0].contains(crate::contract::events::CALL_ASSISTANCE_RESOLVED));
    assert!(laid.sink.lines[0].contains("cancelled"));
    assert!(laid.ctx.transfer.has_held_outcomes());

    // The primary is restored and its fresh session negotiates the contract.
    let (mut lane, detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    lane.ready = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    send_held_outcomes(Some(&mut lane), &mut laid.ctx.transfer);
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Cancelled]);
    assert!(!laid.ctx.transfer.has_held_outcomes());

    // A call with nothing open is left alone.
    let mut idle = Laid {
        ctx: CallVoiceContext::fresh(None),
        sink: crate::event_bridge::VecSink::default(),
        ..laid
    };
    idle.withdraw(TransferWithdrawal::Parked, None);
    assert!(idle.sink.lines.is_empty());
    assert!(!idle.ctx.transfer.has_held_outcomes());
}

/// The juggle's four parking sites go through `stow_ctx!`; the three that only
/// keep the continuing newcomer's context safe for the reset do not. A new
/// place that lays a context aside must choose, and this fails until it does.
#[test]
fn the_hold_juggle_lays_a_call_aside_only_through_the_withdrawing_helper() {
    let source = include_str!("juggle.rs");
    let invocations = source.matches("stow_ctx!(").count();
    let raw_swaps = source.matches("std::mem::replace(").count();
    assert_eq!(invocations, 4, "parking sites that withdraw the transfer first");
    assert_eq!(
        raw_swaps, 3,
        "the newcomer's context is kept for the per-call reset; it is the call that continues"
    );
}

/// The per-call reset: OAIY hears `cancelled` on the session that is about to
/// stop, before it stops.
#[test]
fn a_call_that_ends_tells_its_negotiated_session_the_request_was_cancelled() {
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    lane.ready = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    laid.withdraw(TransferWithdrawal::CallEnded, Some(&mut lane));
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Cancelled]);
    assert!(!laid.ctx.transfer.has_held_outcomes(), "it went straight out");
    assert!(!laid.rig.broker.is_busy());
    assert_eq!(laid.sink.lines.len(), 1);
}

#[test]
fn the_start_offers_transfer_only_to_an_inbound_oaiy_call_with_consent_a_host_and_a_roster() {
    let consent = |assistance, takeover| crate::remote_media::RemoteConsentGate {
        enabled: true,
        acknowledged: true,
        assistance_enabled: assistance,
        takeover_enabled: takeover,
        ..Default::default()
    };
    // The one case that offers it.
    assert!(realtime_allow_transfer(true, false, true, &consent(true, true)));
    // Every condition on its own.
    assert!(!realtime_allow_transfer(false, false, true, &consent(true, true)), "not the OAIY route");
    assert!(!realtime_allow_transfer(true, true, true, &consent(true, true)), "an outbound call");
    assert!(!realtime_allow_transfer(true, false, false, &consent(true, true)), "no ring plan or no roster");
    assert!(!realtime_allow_transfer(true, false, true, &consent(false, true)), "no remote_assistance");
    assert!(!realtime_allow_transfer(true, false, true, &consent(true, false)), "no remote_takeover");
    assert!(!realtime_allow_transfer(
        true,
        false,
        true,
        &crate::remote_media::RemoteConsentGate::default()
    ));
}
