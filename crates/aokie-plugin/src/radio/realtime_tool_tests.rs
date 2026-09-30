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

/// Review finding 8, at the lane the service drives.
#[test]
fn a_looping_model_ends_the_session_after_ten_refusals_and_the_refusal_queue_is_bounded() {
    let (mut lane, _detached) = detached_lane(true);
    for n in 0..MAX_TOOL_CALLS_PER_CALL {
        lane.accept_tool_call(format!("ok{n}"), "finish_call".into(), serde_json::json!({}))
            .unwrap();
        lane.pending_tool_call = None;
    }
    // Ten refusals are answered, one at a time, as OAIY does.
    for n in 0..MAX_REFUSALS_AFTER_LIMIT {
        lane.accept_tool_call(format!("limit{n}"), "finish_call".into(), serde_json::json!({}))
            .expect("still an answer");
        let answers = refused(&mut lane);
        assert_eq!(answers.len(), 1, "{n}");
        assert_eq!(answers[0].3, serde_json::json!({"error": "tool_limit"}));
    }
    // The next call is the loop the model will not leave: the intake reports a
    // failure (the service ends the session with the fixed apology) and queues
    // nothing more.
    let error = lane
        .accept_tool_call("eleventh".into(), "finish_call".into(), serde_json::json!({}))
        .unwrap_err();
    assert!(error.contains("kept calling tools"), "{error}");
    assert!(lane.refused_tools.is_empty());
    assert!(lane.pending_tool_call.is_none());

    // A bridge that sends refusable calls faster than they are answered never
    // grows the queue without bound.
    let (mut lane, _detached) = detached_lane(true);
    for n in 0..MAX_QUEUED_REFUSALS {
        lane.accept_tool_call(format!("odd{n}"), "not_a_tool".into(), serde_json::json!({}))
            .expect("queued");
    }
    assert_eq!(lane.refused_tools.len(), MAX_QUEUED_REFUSALS);
    let error = lane
        .accept_tool_call("one_too_many".into(), "not_a_tool".into(), serde_json::json!({}))
        .unwrap_err();
    assert!(error.contains("faster than they were answered"), "{error}");
    assert_eq!(lane.refused_tools.len(), MAX_QUEUED_REFUSALS, "the queue did not grow");
    // Answering drains it, and intake carries on.
    assert_eq!(refused(&mut lane).len(), MAX_QUEUED_REFUSALS);
    lane.accept_tool_call("after".into(), "not_a_tool".into(), serde_json::json!({}))
        .expect("room again");
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
    // The answer to the tool call is queued for the completion block, which
    // sends it only once the line the model spoke before calling has drained,
    // like every other completion. The outcome about the request that answer
    // names must not overtake it (review finding 9): it waits.
    assert!(detached.drain().is_empty(), "the outcome does not go out ahead of the answer");
    assert!(transfer.has_held_outcomes());
    send_held_outcomes(Some(&mut lane), &mut transfer);
    assert!(detached.drain().is_empty(), "still behind the queued answer");
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
    // The completion block has sent the answer; now the outcome follows.
    send_held_outcomes(Some(&mut lane), &mut transfer);
    assert_eq!(
        detached.drain(),
        vec![crate::realtime_voice::SentControl::TransferOutcome { frame: outcome.clone() }]
    );
    assert!(!transfer.has_held_outcomes());
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

/// Review finding 9. The answer to the tool call names the request; OAIY must
/// have it before it hears anything about that request, however the turns fall.
#[test]
fn outcomes_follow_the_answer_that_names_their_request_and_an_accepted_is_not_replayed() {
    let mut sink = crate::event_bridge::VecSink::default();
    let negotiated = || {
        let (mut lane, detached) = detached_lane(true);
        lane.allow_transfer_sent = true;
        lane.ready = true;
        lane.negotiate_transfer(&features(&["transfer_v1"]));
        (lane, detached)
    };

    // Turn 1: the ringing answer is queued (the model is still speaking).
    let (mut lane, detached) = negotiated();
    let mut transfer = machine();
    apply_transfer_effects(
        vec![Effect::ToolAnswer {
            tool_call_id: "tool_1".into(),
            answer: ToolAnswer { ok: true, output: serde_json::json!({"status": "ringing"}) },
        }],
        Some(&mut lane),
        &mut transfer,
        None,
        &mut sink,
    );
    // Turn 2: an endpoint accepts and the media fails, both before the answer
    // has gone out. Nothing may be sent ahead of it.
    apply_transfer_effects(
        vec![Effect::Outcome(frame(Outcome::Accepted))],
        Some(&mut lane),
        &mut transfer,
        None,
        &mut sink,
    );
    apply_transfer_effects(
        vec![Effect::Outcome(frame(Outcome::Unavailable))],
        Some(&mut lane),
        &mut transfer,
        None,
        &mut sink,
    );
    assert!(sent_outcomes(&detached).is_empty());
    // Turn 3: the completion block sends the answer; the outcomes follow, in order.
    assert!(lane.next_queued_answer().is_some());
    send_held_outcomes(Some(&mut lane), &mut transfer);
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Accepted, Outcome::Unavailable]);

    // An acceptance held behind an answer is not replayed to a later session:
    // the takeover it announced has ended by then.
    let (mut first, first_detached) = negotiated();
    let mut transfer = machine();
    first.queue_tool_answer("tool_1".into(), TOOL_NAME, true, serde_json::json!({}));
    apply_transfer_effects(
        vec![Effect::Outcome(frame(Outcome::Accepted))],
        Some(&mut first),
        &mut transfer,
        None,
        &mut sink,
    );
    assert!(sent_outcomes(&first_detached).is_empty());
    drop(first);
    let (mut second, second_detached) = negotiated();
    apply_transfer_effects(
        vec![Effect::Outcome(frame(Outcome::Unavailable))],
        Some(&mut second),
        &mut transfer,
        None,
        &mut sink,
    );
    send_held_outcomes(Some(&mut second), &mut transfer);
    assert_eq!(sent_outcomes(&second_detached), vec![Outcome::Unavailable]);
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
    transfer.hold_outcome(frame(Outcome::Unavailable), 0);
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
    // The radio holds the call as active, as it does while the request rings.
    let mut tracker = crate::call_session::SessionTracker::new();
    tracker.ring("call_a".into(), aokie_core::events::now_iso8601());
    tracker.answered();
    Laid {
        rig,
        ctx,
        tracker,
        status: RadioStatus::default(),
        sink: crate::event_bridge::VecSink::default(),
        request_id,
    }
}

impl Laid {
    fn withdraw(&mut self, why: TransferWithdrawal, lane: Option<&mut RealtimeCallLane>) -> bool {
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
        )
    }
}

impl Laid {
    /// OAIY sends `transfer_cancel` on `lane`.
    fn cancel(
        &mut self,
        lane: &mut RealtimeCallLane,
        request_id: &str,
        reason: crate::transfer::CancelReason,
    ) {
        handle_transfer_cancel(
            request_id,
            reason,
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

fn negotiated_lane() -> (RealtimeCallLane, crate::realtime_voice::DetachedSession) {
    let (mut lane, detached) = detached_lane(true);
    lane.allow_transfer_sent = true;
    lane.ready = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    (lane, detached)
}

// --- The glue the service loop runs, driven on a session with no socket -----------
//
// `RealtimeVoiceSession::spawn` connects to the one loopback route the desktop
// listens on (ws://127.0.0.1:17872 is the only endpoint `validate_endpoint`
// accepts), so no test may start a real session: the resume path is driven up
// to the spawn and from the spawn's result on, with a detached session standing
// in for the socket.

fn detached_lane_for(call_id: &str) -> (RealtimeCallLane, crate::realtime_voice::DetachedSession) {
    let (session, detached) = crate::realtime_voice::RealtimeVoiceSession::detached(call_id, 7);
    let mut lane = RealtimeCallLane::new(session);
    lane.begun = true;
    lane.allow_transfer_sent = true;
    lane.ready = true;
    lane.negotiate_transfer(&features(&["transfer_v1"]));
    (lane, detached)
}

impl Laid {
    /// `transfer_to_owner` arrives on `lane` with these `arguments`.
    fn call_the_tool(&mut self, lane: &RealtimeCallLane, arguments: serde_json::Value) -> crate::transfer::call::Begin {
        begin_transfer_tool(
            "tool_1",
            &arguments,
            &mut self.ctx,
            lane,
            &self.rig.broker,
            &self.tracker,
            &self.rig.media,
            &self.rig.host,
            &self.status,
            &mut self.sink,
        )
    }
}

/// The params of the first host request of this method written to `sink`.
fn host_request_params(sink: &crate::event_bridge::VecSink, method: &str) -> Option<serde_json::Value> {
    sink.lines
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value["method"] == method)
        .map(|value| value["params"].clone())
}

fn a_call_with_no_open_request() -> Laid {
    let rig = crate::transfer::call::tests::Rig::new();
    let mut tracker = crate::call_session::SessionTracker::new();
    tracker.ring("call_a".into(), aokie_core::events::now_iso8601());
    tracker.caller_id("0491 570 006".into());
    tracker.answered();
    // The host announced ringPlan and a Companion device is approved.
    let status = RadioStatus::default();
    status
        .transfer_ready
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Laid {
        rig,
        ctx: CallVoiceContext::fresh(None),
        tracker,
        status,
        sink: crate::event_bridge::VecSink::default(),
        request_id: String::new(),
    }
}

/// The tool call: the caller's own turns and number come from the call's
/// context and the radio's tracker, so the phrase floor and the per-caller
/// ceiling read what the caller actually said and rang from.
#[test]
fn the_transfer_tool_reads_the_callers_turns_and_number_from_the_radios_own_state() {
    use crate::transfer::call::Begin;
    let user = |text: &str| serde_json::json!({"role": "user", "content": text});
    let bot = |text: &str| serde_json::json!({"role": "assistant", "content": text});
    let call_asked = serde_json::json!({"reason": "caller_asked"});

    // The caller asked for a person: the host is asked for a plan, and the
    // caller's number goes with the request.
    let mut laid = a_call_with_no_open_request();
    let (mut lane, _detached) = detached_lane_for("call_a");
    lane.owner = laid.rig.media.aokie_owner_fence();
    laid.ctx.history = vec![user("Hi"), bot("How can I help?"), user("Can I speak to the owner please?")];
    assert!(matches!(laid.call_the_tool(&lane, call_asked.clone()), Begin::Planning));
    let params = host_request_params(&laid.sink, "oaiy.ring.plan").expect("the host was asked for a ring plan");
    assert_eq!(params["callerNumber"], "0491 570 006");
    assert_eq!(params["recentCallerTurns"], serde_json::json!(["Hi", "Can I speak to the owner please?"]));

    // Nothing the caller said asks for anyone, and the AI's own words do not
    // count: refused before the host is asked.
    let mut laid = a_call_with_no_open_request();
    laid.ctx.history = vec![user("How much for the lawn?"), bot("Can I put you through to the owner?")];
    let Begin::Answered(answer) = laid.call_the_tool(&lane, call_asked.clone()) else {
        panic!("expected a refusal")
    };
    assert_eq!(answer.output["reason"], "caller_did_not_ask");
    assert!(host_request_params(&laid.sink, "oaiy.ring.plan").is_none(), "the host is not asked");

    // Only the last three caller turns are looked at.
    let mut laid = a_call_with_no_open_request();
    laid.ctx.history = vec![
        user("Can I speak to the owner?"),
        user("no"),
        user("sorry"),
        user("the gate is blue"),
    ];
    let Begin::Answered(answer) = laid.call_the_tool(&lane, call_asked.clone()) else {
        panic!("expected a refusal")
    };
    assert_eq!(answer.output["reason"], "caller_did_not_ask");

    // Second review F4: the caller's "yeah", "okay" and "mm-hmm" after the ask
    // are not turns (the plugin cannot tell which the host's record skips), so they neither
    // push the ask out of the last three nor go to the host.
    let mut laid = a_call_with_no_open_request();
    laid.ctx.history = vec![
        user("Can I speak to the owner?"),
        bot("Of course, one moment."),
        user("yeah"),
        user("okay"),
        user("mm-hmm"),
    ];
    assert!(matches!(laid.call_the_tool(&lane, call_asked.clone()), Begin::Planning));
    let params = host_request_params(&laid.sink, "oaiy.ring.plan").expect("the host was asked");
    assert_eq!(params["recentCallerTurns"], serde_json::json!(["Can I speak to the owner?"]));
    // A "yes please" is a turn and does push it out.
    let mut laid = a_call_with_no_open_request();
    laid.ctx.history = vec![
        user("Can I speak to the owner?"),
        user("Yes please"),
        user("Sorry?"),
        user("Stop"),
        user("Yeah"),
    ];
    let Begin::Answered(answer) = laid.call_the_tool(&lane, call_asked.clone()) else {
        panic!("expected a refusal")
    };
    assert_eq!(answer.output["reason"], "caller_did_not_ask");

    // A lane whose exact owner fence is gone cannot start one: the call moved.
    let mut laid = a_call_with_no_open_request();
    lane.owner = None;
    let Begin::Answered(answer) =
        laid.call_the_tool(&lane, serde_json::json!({"reason": "caller_asked"}))
    else {
        panic!("expected a refusal")
    };
    assert_eq!(answer.output["reason"], "call_changed");
    assert_eq!(answer.output["status"], "unavailable");
}

/// The exact owner fence changed under a live session: the stop OAIY hears,
/// what it hears before it, and what the fresh session is told afterwards.
#[test]
fn a_takeover_stops_the_session_as_a_handoff_and_the_fresh_session_says_how_the_ai_got_the_caller_back() {
    use crate::realtime_voice::SentControl;
    use crate::transfer::Via;

    // A request that rings when the caller is taken another way: OAIY hears
    // it is moot, then the stop that says handoff, not the end of the call.
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = negotiated_lane();
    stop_session_for_ownership_change(
        &mut laid.ctx,
        &mut lane,
        &laid.rig.broker,
        &laid.tracker,
        &laid.rig.media,
        &laid.rig.host,
        &laid.status,
        None,
        &mut laid.sink,
    );
    let sent = detached.drain();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        matches!(&sent[0], SentControl::TransferOutcome { frame } if frame.outcome == Outcome::Cancelled),
        "cancelled first: {sent:?}"
    );
    assert_eq!(
        sent[1],
        SentControl::Stop { reason: crate::transfer::STOP_HANDOFF_TAKEOVER.into() },
        "then the handoff stop"
    );
    assert!(!laid.rig.broker.is_busy());

    // The fresh session says how the AI got the caller back. The record of the
    // handoff was made by that stop, so a start now carries `resume`.
    let now = Instant::now() + Duration::from_secs(20);
    let (resume, greeting) = resume_start(&laid.ctx, &laid.rig.broker, now);
    assert_eq!(resume.map(|resume| resume.via), Some(Via::Return));
    assert!(resume.is_some_and(|resume| resume.after_handoff && resume.handoff_seconds >= 20));
    assert_eq!(greeting, crate::transfer::RETURN_GREETING);
    // Nothing was consumed: a start that fails is asked again.
    assert_eq!(resume_start(&laid.ctx, &laid.rig.broker, now).0.map(|resume| resume.via), Some(Via::Return));
    assert!(laid.ctx.transfer.peek_resume(now, &laid.rig.broker).is_some());

    // The session that could not start leaves the record; the one that starts consumes it.
    let (session, _detached) = crate::realtime_voice::RealtimeVoiceSession::detached("call_a", 8);
    let fresh = adopt_resumed_session(&mut laid.ctx, session, true);
    assert!(fresh.allow_transfer_sent);
    assert!(!fresh.transfer_negotiated, "the fresh session negotiates on its own ready");
    assert!(laid.ctx.transfer.peek_resume(now, &laid.rig.broker).is_none(), "the handoff is over");
    let (resume, greeting) = resume_start(&laid.ctx, &laid.rig.broker, now);
    assert_eq!(resume, None);
    assert_ne!(greeting, crate::transfer::RETURN_GREETING, "an ordinary resume greets as it always did");

    // A session that did not negotiate the contract keeps the words it always had
    // and leaves no record: its fresh session says nothing about a handoff.
    let mut laid = a_call_with_no_open_request();
    let (mut old, old_detached) = detached_lane_for("call_a");
    old.transfer_negotiated = false;
    stop_session_for_ownership_change(
        &mut laid.ctx,
        &mut old,
        &laid.rig.broker,
        &laid.tracker,
        &laid.rig.media,
        &laid.rig.host,
        &laid.status,
        None,
        &mut laid.sink,
    );
    assert_eq!(
        old_detached.drain(),
        vec![SentControl::Stop { reason: "Aokie media ownership changed".into() }]
    );
    assert_eq!(laid.ctx.transfer.peek_resume(now, &laid.rig.broker), None);

    // A failed setup: the caller is back before the gateway has written the
    // failure down, so the fresh session says failback, and the failure is
    // still owed to OAIY once it negotiates (finding 3, end to end).
    let mut laid = laid_aside_with_a_ringing_request();
    let request_id = laid.request_id.clone();
    laid.rig.accept(&request_id);
    let (mut lane, _detached) = negotiated_lane();
    stop_session_for_ownership_change(
        &mut laid.ctx,
        &mut lane,
        &laid.rig.broker,
        &laid.tracker,
        &laid.rig.media,
        &laid.rig.host,
        &laid.status,
        None,
        &mut laid.sink,
    );
    assert!(laid.ctx.transfer.is_active(), "an accepted request stays open through its takeover");
    let (resume, greeting) = resume_start(&laid.ctx, &laid.rig.broker, Instant::now());
    assert_eq!(resume.map(|resume| resume.via), Some(Via::Failback));
    assert_eq!(greeting, crate::transfer::RETURN_GREETING);
}

/// A source file as the scan tests read it, with line endings folded: a
/// checkout with core.autocrlf (CRLF) and one without must count the same.
fn scanned(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// The realtime block of the loop is driven for real in `service_loop_tests`
/// (a fake phone link, a detached session, a private mailbox), which is what
/// shows it does what it should; this is the shape guard beside it, and covers
/// what those tests cannot reach, the per-call reset in `run_loop`: each helper
/// is defined once and used once in the loop (the send after the tool block,
/// which only shortens the wait, twice), and nothing in the loop does by hand
/// what a helper does. Changing the wiring means changing this on purpose.
#[test]
fn the_service_loop_runs_the_transfer_helpers_it_is_tested_through() {
    let service = scanned(include_str!("realtime_service.rs"));
    let run_loop = scanned(include_str!("run_loop.rs"));
    let (service, run_loop) = (service.as_str(), run_loop.as_str());
    let count = |text: &str, needle: &str| text.matches(needle).count();

    // Defined once, called once.
    for helper in [
        "stop_session_for_ownership_change(",
        "resume_start(",
        "adopt_resumed_session(",
        "begin_transfer_tool(",
        "handle_transfer_cancel(",
    ] {
        assert_eq!(count(service, helper), 2, "{helper} is defined once and called once");
    }
    // The handoff stop, the resume record and the poll are written in one place.
    assert_eq!(count(service, "STOP_HANDOFF_TAKEOVER"), 1, "only the stop helper says handoff");
    assert_eq!(count(service, "note_handoff("), 1);
    assert_eq!(count(service, "ctx.transfer.finish_resume"), 1, "only the adoption consumes the record");
    assert_eq!(count(service, "ctx.transfer.poll(&mut env)"), 1, "one poll a turn, at the top of the block");
    // Held outcomes are flushed at the top of the block and behind the tool block.
    assert_eq!(count(service, "send_held_outcomes(realtime_lane.as_mut(), &mut ctx.transfer)"), 2);
    assert_eq!(
        count(
            service,
            "if realtime_failure.is_none() && ctx.transfer.has_held_outcomes() {\n            send_held_outcomes(realtime_lane.as_mut(), &mut ctx.transfer);"
        ) + count(
            service,
            "if ctx.transfer.has_held_outcomes() {\n                send_held_outcomes(realtime_lane.as_mut(), &mut ctx.transfer);"
        ),
        2,
        "both flushes are unconditional but for the failure and the held frames"
    );
    // The per-call reset withdraws through the shared helper, and does not do it by hand.
    assert_eq!(count(run_loop, "TransferWithdrawal::CallEnded"), 1);
    assert_eq!(count(run_loop, "end_call("), 0);
    assert_eq!(count(run_loop, "ctx.transfer."), 0);
}

/// The realtime side of OAIY withdrawing a request: what reaches OAIY, on which
/// session, in which form.
#[test]
fn a_cancel_from_oaiy_is_answered_on_its_own_session_by_the_outcome_or_a_typed_notice() {
    use crate::realtime_voice::SentControl;
    use crate::transfer::{CancelReason, Notice, NoticeFrame};
    let notice_of = |sent: &SentControl| match sent {
        SentControl::TransferNotice { frame: NoticeFrame { request_id, notice, .. } } => {
            Some((request_id.clone(), *notice))
        }
        _ => None,
    };

    // A request that rings: withdrawn, reported once as the outcome.
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = negotiated_lane();
    let request_id = laid.request_id.clone();
    laid.cancel(&mut lane, &request_id, CancelReason::OwnerDeclined);
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Cancelled]);
    assert!(!laid.rig.broker.is_busy(), "the mailbox is free");
    assert!(!laid.rig.broker.transfer_admits(&request_id, "thumb_phone_0123456789"));
    assert_eq!(laid.sink.lines.len(), 1, "the audit closes the request");
    assert!(laid.sink.lines[0].contains("cancelled"));

    // The same cancel again: a typed notice, and nothing done twice.
    laid.cancel(&mut lane, &request_id, CancelReason::OwnerDeclined);
    let sent = detached.drain();
    assert_eq!(
        sent.iter().filter_map(notice_of).collect::<Vec<_>>(),
        vec![(request_id.clone(), Notice::UnknownRequest)]
    );
    assert_eq!(sent.len(), 1);
    assert_eq!(laid.sink.lines.len(), 1, "no second audit event");

    // A request an owner device has won: too late, and nothing changes.
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = negotiated_lane();
    let request_id = laid.request_id.clone();
    laid.rig.accept(&request_id);
    laid.cancel(&mut lane, &request_id, CancelReason::MessageInstead);
    let sent = detached.drain();
    assert_eq!(
        sent.iter().filter_map(notice_of).collect::<Vec<_>>(),
        vec![(request_id.clone(), Notice::TooLate)]
    );
    assert!(laid.rig.broker.is_busy(), "the takeover is not affected");
    assert!(laid.ctx.transfer.is_active());

    // A session that did not negotiate the contract: the frame means nothing,
    // nothing is sent, and the request is left exactly as it was.
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut old, old_detached) = detached_lane(true);
    old.ready = true;
    let request_id = laid.request_id.clone();
    laid.cancel(&mut old, &request_id, CancelReason::GaveUp);
    assert!(old_detached.drain().is_empty());
    assert!(laid.rig.broker.is_busy());
    assert!(laid.ctx.transfer.is_active());
    assert!(laid.sink.lines.is_empty());

    // The cancelled outcome of a withdrawal still waits behind the answer that
    // names the request (finding 9); the notice, a reply, does not.
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = negotiated_lane();
    let request_id = laid.request_id.clone();
    lane.queue_tool_answer("tool_1".into(), TOOL_NAME, true, serde_json::json!({}));
    laid.cancel(&mut lane, &request_id, CancelReason::GaveUp);
    assert!(sent_outcomes(&detached).is_empty(), "behind the queued answer");
    assert!(laid.ctx.transfer.has_held_outcomes());
    laid.cancel(&mut lane, &request_id, CancelReason::GaveUp);
    assert_eq!(
        detached.drain().iter().filter_map(notice_of).collect::<Vec<_>>(),
        vec![(request_id.clone(), Notice::UnknownRequest)],
        "the notice goes at once"
    );
    assert!(lane.next_queued_answer().is_some());
    send_held_outcomes(Some(&mut lane), &mut laid.ctx.transfer);
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Cancelled]);

    // A notice is never kept for a later session: with no session, it is dropped.
    let mut sink = crate::event_bridge::VecSink::default();
    let mut transfer = machine();
    apply_transfer_effects(
        vec![crate::transfer::call::Effect::Notice(NoticeFrame {
            request_id: "assist_1".into(),
            notice: Notice::UnknownRequest,
            at_ms: 1,
        })],
        None,
        &mut transfer,
        None,
        &mut sink,
    );
    assert!(!transfer.has_held_outcomes());
    // Nor is one sent on a session that never negotiated the contract.
    let (mut old, old_detached) = detached_lane(true);
    old.ready = true;
    apply_transfer_effects(
        vec![crate::transfer::call::Effect::Notice(NoticeFrame {
            request_id: "assist_1".into(),
            notice: Notice::TooLate,
            at_ms: 1,
        })],
        Some(&mut old),
        &mut transfer,
        None,
        &mut sink,
    );
    assert!(old_detached.drain().is_empty());
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
    let source = scanned(include_str!("juggle.rs"));
    let source = source.as_str();
    let invocations = source.matches("stow_ctx!(").count();
    assert_eq!(invocations, 4, "parking sites that withdraw the transfer first");
    assert_eq!(
        source.matches("std::mem::replace(").count(),
        0,
        "the newcomer's context that is kept for the per-call reset goes through carry_call_context"
    );
    assert_eq!(source.matches("carry_call_context(").count(), 3);
    assert_eq!(source.matches("retire_ctx!(").count(), 1, "the vanished newcomer's context is retired");
    // The transfer gate runs first: the open request is withdrawn while the
    // session is still up, before it is stopped and before the hold
    // announcements begin, and an accepted one abandons the juggle.
    assert_eq!(source.matches("withdraw_open_transfer(").count(), 1);
    assert_eq!(
        source.matches("} else if !withdraw_open_transfer(").count(),
        1,
        "a transfer being connected abandons the juggle"
    );
    let withdrawn_at = source.find("withdraw_open_transfer(").unwrap();
    let stopped_at = source.find("lane.session.stop(\"hold juggle").unwrap();
    let announced_at = source.find("HOLD_PRIMARY_ASK_LINE").unwrap();
    assert!(withdrawn_at < stopped_at, "the request is withdrawn before the session is stopped");
    assert!(withdrawn_at < announced_at, "and before the first hold announcement");
}

/// Second review F1. A call's voice context is laid aside in more places than
/// the juggle (the switchboard's manual swaps, the reconciliation's cascade), and
/// a Ringing request in a stowed context sits there unpolled: the mailbox busy,
/// the owner's phones ringing, OAIY told nothing. So NO code in the radio swaps a
/// call's context by hand: it goes through `stow_call_context` (parked: the
/// request is withdrawn, or the park refused for an accepted one),
/// `retire_call_context` (the call ended) or `carry_call_context` (the call
/// continues), all three defined in realtime_service.rs, which is the only file
/// that may say `mem::replace` on a context.
#[test]
fn no_place_in_the_radio_swaps_a_call_context_by_hand() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join("radio");
    let swap = regex::Regex::new(r"mem::(replace|swap|take)\(\s*(&mut\s*)?\*?\s*ctx\b").unwrap();
    let assign = regex::Regex::new(r"\*ctx\s*=[^=]").unwrap();
    let mut files = 0;
    let mut helper_swaps = 0;
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.ends_with(".rs") || name.ends_with("tests.rs") {
            continue;
        }
        files += 1;
        let text = scanned(&std::fs::read_to_string(&path).unwrap());
        let swaps = swap.find_iter(&text).count();
        if name == "realtime_service.rs" {
            helper_swaps = swaps;
        } else if swaps > 0 || assign.is_match(&text) {
            offenders.push(name);
        }
    }
    assert!(files >= 20, "the scan found the radio's sources ({files})");
    assert!(offenders.is_empty(), "a call context is swapped by hand in {offenders:?}");
    assert_eq!(
        helper_swaps, 5,
        "the swaps live in the stow, retire and carry helpers (two builds of the first two)"
    );

    // Each site is wired as decided: the switchboard's two parks are gated (an
    // accepted transfer is not parked) and stowed, the cascade stows two newcomers,
    // retires the ended foreground's context and carries the newcomer that keeps
    // the line; the juggle's parks are stowed and its newcomers carried.
    let read = |name: &str| scanned(&std::fs::read_to_string(dir.join(name)).unwrap());
    let (controls, reconcile) = (read("controls.rs"), read("reconcile.rs"));
    assert_eq!(controls.matches("stow_ctx!(").count(), 2);
    assert_eq!(controls.matches("transfer_permits_parking!()").count(), 2, "two gates");
    assert!(controls.contains("foreground_active && !transfer_permits_parking!()"));
    assert!(controls.contains("} else if !transfer_permits_parking!() {"));
    assert_eq!(reconcile.matches("stow_ctx!(").count(), 2);
    assert_eq!(reconcile.matches("retire_ctx!(").count(), 1);
    assert_eq!(reconcile.matches("carry_call_context(").count(), 2);
}

/// The helpers the scan above forces every site through, by behaviour: a stowed
/// or retired context leaves no request behind (OAIY told, the audit closed, the
/// mailbox free); a carried one is the same call continuing and is untouched.
#[test]
fn the_retired_context_is_withdrawn_and_the_carried_one_is_left_alone() {
    let mut laid = laid_aside_with_a_ringing_request();
    let request_id = laid.request_id.clone();
    let carried = carry_call_context(&mut laid.ctx, CallVoiceContext::fresh(None));
    assert!(carried.transfer.is_active(), "the call continues: its request is untouched");
    assert!(laid.rig.broker.is_busy());
    assert!(!laid.ctx.transfer.is_active());
    laid.ctx = carried;

    let retired = retire_call_context(
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
    assert!(!retired.transfer.is_active(), "the ended call's request is withdrawn");
    assert!(retired.transfer.has_held_outcomes(), "and OAIY is owed the cancelled");
    assert!(!laid.rig.broker.is_busy());
    assert!(!laid.rig.broker.transfer_admits(&request_id, "thumb_phone_0123456789"));
    assert_eq!(laid.sink.lines.len(), 1, "the audit trail closes the request");
    assert!(laid.sink.lines[0].contains("cancelled"));
}

/// The scan reads sources however the checkout ended their lines.
#[test]
fn the_source_scans_count_the_same_with_crlf_line_endings() {
    let lf = "if x {\n    stow_ctx!(a);\n}\n";
    let crlf = lf.replace('\n', "\r\n");
    assert_ne!(lf, crlf);
    assert_eq!(scanned(&crlf), lf);
    assert_eq!(scanned(&crlf).matches("if x {\n    stow_ctx!(").count(), 1);
    // The sources under test are scanned folded whichever they are.
    for source in [
        include_str!("realtime_service.rs"),
        include_str!("run_loop.rs"),
        include_str!("juggle.rs"),
    ] {
        assert!(!scanned(source).contains('\r'));
    }
}

/// The juggle's step 0: the primary's session is still up when its open request
/// is withdrawn, so OAIY hears `cancelled` on it before the stop.
#[test]
fn a_primary_parked_while_its_session_is_up_hears_cancelled_before_the_stop() {
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = negotiated_lane();
    laid.withdraw(TransferWithdrawal::Parked, Some(&mut lane));
    lane.session.stop("hold juggle — primary parked");
    let sent = detached.drain();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        matches!(&sent[0], crate::realtime_voice::SentControl::TransferOutcome { frame } if frame.outcome == Outcome::Cancelled),
        "{sent:?}"
    );
    assert!(matches!(&sent[1], crate::realtime_voice::SentControl::Stop { .. }), "{sent:?}");
    assert!(!laid.rig.broker.is_busy(), "no phone can accept while the announcements play");
    assert!(!laid.ctx.transfer.is_active());
}

/// Second review F10. The model may call `finish_call` while a request to the
/// owner is open (the caller asked for a person, the model says goodbye): that
/// hangup would drop the caller the owner's phone is answering for. It is
/// refused with a typed error for as long as the transfer is in flight, and only
/// then.
#[test]
fn finish_call_is_refused_while_a_transfer_is_in_flight_and_only_then() {
    let ask = |transfer: &crate::transfer::call::TransferCall| finish_call_answer(true, true, 5, 5, transfer);
    let assert_refused = |transfer: &crate::transfer::call::TransferCall, stage: &str| {
        let (accepted, output) = ask(transfer);
        assert!(!accepted, "{stage}");
        assert_eq!(output["accepted"], false, "{stage}");
        assert_eq!(output["error"], "transfer_in_progress", "{stage}");
        assert!(output["instruction"].as_str().unwrap().contains("keep the caller company"), "{stage}");
        assert_eq!(output.as_object().unwrap().len(), 3, "{stage}: accepted, a typed error and a fixed instruction, nothing else");
    };

    // Nothing open: as ever.
    let idle = machine();
    let (accepted, output) = ask(&idle);
    assert!(accepted);
    assert_eq!(output["accepted"], true);

    // Planning: the host is being asked.
    let mut planning = a_call_with_no_open_request();
    let (mut lane, _detached) = detached_lane_for("call_a");
    lane.owner = planning.rig.media.aokie_owner_fence();
    planning.ctx.history = vec![serde_json::json!({"role": "user", "content": "Can I speak to the owner?"})];
    assert!(matches!(
        planning.call_the_tool(&lane, serde_json::json!({"reason": "caller_asked"})),
        crate::transfer::call::Begin::Planning
    ));
    assert_refused(&planning.ctx.transfer, "planning");

    // Ringing, then accepted with the takeover not completed.
    let mut laid = laid_aside_with_a_ringing_request();
    assert_refused(&laid.ctx.transfer, "ringing");
    let request_id = laid.request_id.clone();
    laid.rig.accept(&request_id);
    assert!(!laid.withdraw(TransferWithdrawal::Parked, None), "the gate polls: accepted, left untouched");
    assert_refused(&laid.ctx.transfer, "accepted");

    // The takeover completes: the transfer has ended.
    let fence = laid.rig.fence(&request_id);
    laid.rig.broker.transfer_taken(&request_id, &fence, "device_owner").unwrap();
    assert!(laid.withdraw(TransferWithdrawal::Parked, None));
    assert!(ask(&laid.ctx.transfer).0, "taken");

    // Declined, and withdrawn.
    let mut declined = laid_aside_with_a_ringing_request();
    let declined_id = declined.request_id.clone();
    declined.rig.decline(&declined_id, "no");
    assert!(declined.withdraw(TransferWithdrawal::Parked, None));
    assert!(ask(&declined.ctx.transfer).0, "declined");
    let mut withdrawn = laid_aside_with_a_ringing_request();
    assert!(withdrawn.withdraw(TransferWithdrawal::Parked, None));
    assert!(ask(&withdrawn.ctx.transfer).0, "withdrawn");

    // What made it refused before stays refused for its old reason.
    let (accepted, output) = finish_call_answer(false, true, 5, 5, &idle);
    assert!(!accepted);
    assert_eq!(output["error"], "Call finishing is disabled or the request was invalid.");
    assert!(!finish_call_answer(true, false, 5, 5, &idle).0, "arguments were not empty");
    assert!(!finish_call_answer(true, true, 5, 6, &idle).0, "the caller spoke again since");
    // And a refused request for another reason does not become a transfer error.
    assert_eq!(
        finish_call_answer(false, true, 5, 5, &laid_aside_with_a_ringing_request().ctx.transfer).1["error"],
        "Call finishing is disabled or the request was invalid."
    );
}

/// Second review F2. The gate the juggle and the switchboard run before a call
/// is parked: a request nobody has won is withdrawn and the call may be parked;
/// one an owner device has accepted is a takeover being connected, is left
/// untouched (its acceptance reported once) and the call may NOT be parked.
#[test]
fn a_call_whose_transfer_a_phone_has_accepted_is_not_parked() {
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = negotiated_lane();
    let request_id = laid.request_id.clone();
    // The phone accepts; the machine has not yet looked.
    laid.rig.accept(&request_id);
    let fence = laid.rig.fence(&request_id);
    for _ in 0..3 {
        assert!(
            !laid.withdraw(TransferWithdrawal::Parked, Some(&mut lane)),
            "the gate says no"
        );
    }
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Accepted], "accepted, once, and nothing cancelled");
    assert!(laid.rig.broker.is_busy(), "the request was left alone");
    assert!(laid.rig.broker.transfer_activation_is_current(&request_id, &fence, "device_owner"));
    assert!(laid.ctx.transfer.is_active());
    assert!(laid.sink.lines.is_empty(), "no audit event closes a request that is still being connected");

    // The phone's takeover completes; the transfer ends as a takeover.
    laid.rig.broker.transfer_taken(&request_id, &fence, "device_owner").unwrap();
    assert!(laid.withdraw(TransferWithdrawal::Parked, Some(&mut lane)), "nothing open now");
    assert!(!laid.ctx.transfer.is_active());

    // A request nobody has won: withdrawn, and the call may be parked.
    let mut laid = laid_aside_with_a_ringing_request();
    let (mut lane, detached) = negotiated_lane();
    assert!(laid.withdraw(TransferWithdrawal::Parked, Some(&mut lane)));
    assert_eq!(sent_outcomes(&detached), vec![Outcome::Cancelled]);
    assert!(!laid.rig.broker.is_busy());

    // The backstop once the swap has happened: nothing may be left in the stowed
    // context, an accepted request included.
    let mut laid = laid_aside_with_a_ringing_request();
    let request_id = laid.request_id.clone();
    laid.rig.accept(&request_id);
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
    assert!(!stowed.transfer.is_active(), "no request is left in a context nobody polls");
    assert!(!laid.rig.broker.is_busy());
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
