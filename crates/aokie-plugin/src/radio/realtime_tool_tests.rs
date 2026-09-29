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
        None,
        &mut sink,
    );
    assert_eq!(
        detached.drain(),
        vec![
            crate::realtime_voice::SentControl::ToolResult {
                tool_call_id: "tool_1".into(),
                name: TOOL_NAME.into(),
                ok: true,
                output: serde_json::json!({"status": "ringing"}),
                continue_response: true,
            },
            crate::realtime_voice::SentControl::TransferOutcome { frame: outcome.clone() },
        ]
    );
    assert_eq!(sink.lines.len(), 1, "the audit event was emitted");
    assert!(sink.lines[0].contains(crate::contract::events::CALL_ASSISTANCE_REQUESTED));

    // A session that did not negotiate the contract never receives an outcome,
    // and neither does a call with no session at all; the audit is still written.
    let (mut old, old_detached) = detached_lane(true);
    let mut sink = crate::event_bridge::VecSink::default();
    let audit = crate::radio::AssistanceAuditLifecycle::opened("assist_2", "call_1").1;
    apply_transfer_effects(
        vec![Effect::Outcome(outcome.clone()), Effect::Audit(audit.clone())],
        Some(&mut old),
        None,
        &mut sink,
    );
    assert!(old_detached.drain().is_empty());
    assert_eq!(sink.lines.len(), 1);
    let mut sink = crate::event_bridge::VecSink::default();
    apply_transfer_effects(vec![Effect::Outcome(outcome), Effect::Audit(audit)], None, None, &mut sink);
    assert_eq!(sink.lines.len(), 1);
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
