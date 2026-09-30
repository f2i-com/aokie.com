//! The radio loop's realtime block (`service_realtime_lane`), run for real.
//!
//! Second review F3: five one-line deletions in this function survived every
//! test, because the tests called the helpers it calls and only a scan of its
//! source checked that it does. These run the function itself, pass after pass,
//! against a phone with no radio behind it (`FakeBackend`), a session with no
//! socket (`RealtimeVoiceSession::detached`, started through the function's
//! `spawn_session` parameter, which records what each start said), a private
//! assistance mailbox and a real remote-media state. OAIY's side is the far end
//! of the detached session: what it is sent is what the tests read.

use super::*;
use crate::realtime_voice::{
    DetachedSession, RealtimeEventKind, RealtimeVoiceSession, SentControl, SessionConfig,
};
use crate::transfer::call::tests::Rig;
use crate::transfer::{CancelReason, Outcome, ResumeInfo, Via, TOOL_NAME};
use aokie_dongle::bluetooth::{AudioData, BluetoothEvent, PairingConfirmSlot, PairingWindow};
use serde_json::{json, Value};
use std::cell::RefCell;

const CALL: &str = "call_a";

/// A phone link that answers everything and carries no audio.
struct FakeBackend {
    rate: u16,
}

impl crate::backend::RadioBackend for FakeBackend {
    fn try_recv_event(&mut self) -> Option<BluetoothEvent> {
        None
    }
    fn try_recv_audio(&mut self) -> Option<AudioData> {
        None
    }
    fn send_audio(&self, _samples: &[i16]) -> bool {
        true
    }
    fn flush_tx_audio(&self) {}
    fn send_sms(&self, _id: String, _to: String, _body: String, _kind: Option<String>) -> Result<(), String> {
        Ok(())
    }
    fn answer_call(&self) -> Result<(), String> {
        Ok(())
    }
    fn reject_call(&self) -> Result<(), String> {
        Ok(())
    }
    fn hangup(&self) -> Result<(), String> {
        Ok(())
    }
    fn dial(&self, _number: String) -> Result<(), String> {
        Ok(())
    }
    fn hold_swap(&self) -> Result<(), String> {
        Ok(())
    }
    fn query_calls(&self) -> Result<(), String> {
        Ok(())
    }
    fn open_pairing_window(&self, _seconds: u64) {}
    fn close_pairing_window(&self) {}
    fn pairing_window(&self) -> PairingWindow {
        PairingWindow::default()
    }
    fn pairing_confirm_slot(&self) -> PairingConfirmSlot {
        PairingConfirmSlot::default()
    }
    fn bonded_devices(&self) -> Vec<(String, Option<String>)> {
        Vec::new()
    }
    fn connected_name(&self) -> Option<String> {
        None
    }
    fn remove_paired(&self, _address: &str) -> Result<bool, String> {
        Ok(false)
    }
    fn disconnect(&self, _address: &str) -> Result<bool, String> {
        Ok(false)
    }
    fn connect(&self, _address: &str) -> Result<bool, String> {
        Ok(false)
    }
    fn confirm_pairing(&self, _address: &str, _accept: bool) -> Result<(), String> {
        Ok(())
    }
    fn is_connected(&self) -> bool {
        true
    }
    fn get_sample_rate(&self) -> u16 {
        self.rate
    }
    fn realtime_call_audio_supported(&self) -> bool {
        true
    }
    fn sco_dead_air_watchdog(&self) -> bool {
        false
    }
    fn backend_name(&self) -> &'static str {
        "test backend"
    }
}

/// Everything the radio loop hands `service_realtime_lane`, for one call.
struct Loop {
    rig: Rig,
    bt: FakeBackend,
    sink: crate::event_bridge::VecSink,
    status: Arc<RadioStatus>,
    synth: crate::synth::SynthHandle,
    stt_tx: std::sync::mpsc::Sender<SttWork>,
    _stt_rx: std::sync::mpsc::Receiver<SttWork>,
    screen: crate::screen::ScreenPolicy,
    tracker: crate::call_session::SessionTracker,
    config: Option<RealtimeRuntimeConfig>,
    greeting: Option<String>,
    persona: String,
    answer_hold_started: Option<Instant>,
    promote_greet_for: Option<String>,
    aec: Option<crate::aec::EchoCanceller>,
    lane: Option<RealtimeCallLane>,
    legacy: Option<String>,
    failed: Option<(String, String)>,
    resume: Option<String>,
    answered_at: Option<(String, Instant)>,
    midcall: Option<(String, crate::remote_media::AokieOwnerFence, String)>,
    terminal: Option<(String, Instant, u8)>,
    deferred: Option<(String, String)>,
    ctx: CallVoiceContext,
    agent_hangup: bool,
    /// Every session the loop started: what its start said, and OAIY's end of it.
    started: RefCell<Vec<(SessionConfig, DetachedSession)>>,
}

impl Loop {
    /// An inbound call the caller placed, answered, on the OAIY route, with the
    /// host ready for transfers and consent granted.
    fn new() -> Self {
        let rig = Rig::new();
        let mut tracker = crate::call_session::SessionTracker::new();
        tracker.ring(CALL.into(), aokie_core::events::now_iso8601());
        tracker.caller_id("0491 570 006".into());
        tracker.answered();
        let status = Arc::new(RadioStatus::default());
        status.transfer_ready.store(true, std::sync::atomic::Ordering::Relaxed);
        let (stt_tx, stt_rx) = std::sync::mpsc::channel();
        Self {
            rig,
            bt: FakeBackend { rate: 8_000 },
            sink: crate::event_bridge::VecSink::default(),
            status,
            synth: crate::synth::SynthHandle::inert(),
            stt_tx,
            _stt_rx: stt_rx,
            screen: crate::screen::ScreenPolicy::permissive(),
            tracker,
            config: Some(RealtimeRuntimeConfig {
                endpoint: "ws://127.0.0.1:1/never-connected".into(),
                destination: "https://oaiy.localhost".into(),
                voice: "marin".into(),
                turn_detection: crate::realtime_voice::TurnDetection::ServerVad,
                max_output_tokens: 4_096,
            }),
            greeting: None,
            persona: "You are a receptionist.".into(),
            answer_hold_started: None,
            promote_greet_for: None,
            aec: None,
            lane: None,
            legacy: None,
            failed: None,
            resume: None,
            answered_at: None,
            midcall: None,
            terminal: None,
            deferred: None,
            ctx: CallVoiceContext::fresh(None),
            agent_hangup: false,
            started: RefCell::new(Vec::new()),
        }
    }

    /// An outbound call the plugin placed itself.
    fn outbound() -> Self {
        let mut this = Self::new();
        this.tracker = crate::call_session::SessionTracker::new();
        this.tracker.dial(
            CALL.into(),
            Some("0491 570 156".into()),
            aokie_core::events::now_iso8601(),
            true,
        );
        this.tracker.answered();
        this
    }

    /// One turn of the radio loop's realtime block.
    fn pass(&mut self) {
        let started = &self.started;
        let spawn = |config: SessionConfig| -> Result<RealtimeVoiceSession, String> {
            let (session, far_end) = RealtimeVoiceSession::detached(&config.call_id, config.generation);
            started.borrow_mut().push((config, far_end));
            Ok(session)
        };
        service_realtime_lane(
            &mut self.bt,
            None,
            &mut self.sink,
            &self.status,
            &self.greeting,
            &self.rig.host,
            &self.rig.media,
            true,
            true,
            &self.config,
            &self.synth,
            &self.stt_tx,
            &self.persona,
            &mut self.answer_hold_started,
            &self.screen,
            self.agent_hangup,
            &mut self.promote_greet_for,
            &mut self.aec,
            &mut self.tracker,
            7,
            &mut self.lane,
            &mut self.legacy,
            &mut self.failed,
            &mut self.resume,
            &mut self.answered_at,
            &mut self.midcall,
            &mut self.terminal,
            &mut self.deferred,
            &mut self.ctx,
            &self.rig.broker,
            &spawn,
        );
    }

    fn sessions(&self) -> usize {
        self.started.borrow().len()
    }

    /// What the loop's start number `n` (0 is the first) said.
    fn start(&self, n: usize) -> SessionConfig {
        self.started.borrow()[n].0.clone()
    }

    /// OAIY says something on the newest session.
    fn oaiy(&self, kind: RealtimeEventKind) {
        let started = self.started.borrow();
        let (config, far_end) = started.last().expect("the loop started a session");
        far_end.push_event(&config.call_id, config.generation, kind);
    }

    /// What the plugin sent OAIY on the newest session since the last look.
    fn sent(&self) -> Vec<SentControl> {
        self.started.borrow().last().expect("a session").1.drain()
    }

    fn ready(&self, features: &[&str]) {
        self.oaiy(RealtimeEventKind::Ready {
            destination_origin: "https://oaiy.localhost".into(),
            features: features.iter().map(|name| name.to_string()).collect(),
        });
    }

    fn tool(&self, id: &str, name: &str, arguments: Value) {
        self.oaiy(RealtimeEventKind::ToolCall {
            tool_call_id: id.into(),
            name: name.into(),
            arguments,
        });
    }

    fn caller_says(&mut self, text: &str) {
        self.ctx.history.push(json!({"role": "user", "content": text}));
    }

    /// The host answers the plan request the plugin wrote to the sink.
    fn host_answers(&self, result: Value) {
        let id = self
            .sink
            .lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["method"] == "oaiy.ring.plan")
            .map(|value| value["id"].as_u64().unwrap())
            .expect("the plugin asked the host for a ring plan");
        assert!(self.rig.host.try_route_response(&json!({"jsonrpc": "2.0", "id": id, "result": result})));
    }

    /// Start a session, negotiate the contract and begin the call.
    fn begun_with_transfer() -> Self {
        let mut this = Self::new();
        this.pass();
        this.ready(&["transfer_v1"]);
        this.pass();
        let lane = this.lane.as_ref().expect("the loop started the call's session");
        assert!(lane.transfer_negotiated && lane.begun, "negotiated and begun");
        assert!(this.sent().contains(&SentControl::Begin));
        this
    }

    /// The caller asks for the owner, the host plans a ring, and the answer
    /// that says so reaches OAIY: the request id it names.
    fn ring(&mut self) -> String {
        self.caller_says("Can I speak to the owner?");
        self.tool("tool_1", TOOL_NAME, json!({"reason": "caller_asked"}));
        self.pass();
        self.host_answers(Rig::ring_plan());
        self.pass();
        let answer = tool_result(&self.sent(), "tool_1").expect("the model is answered");
        assert!(answer.0, "{answer:?}");
        assert_eq!(answer.1["status"], "ringing", "{answer:?}");
        answer.1["requestId"].as_str().expect("the answer names the request").to_string()
    }
}

/// The result the plugin sent for one tool call: `(ok, output)`.
fn tool_result(sent: &[SentControl], tool_call_id: &str) -> Option<(bool, Value)> {
    sent.iter().find_map(|control| match control {
        SentControl::ToolResult { tool_call_id: id, ok, output, .. } if id == tool_call_id => {
            Some((*ok, output.clone()))
        }
        _ => None,
    })
}

fn outcomes(sent: &[SentControl]) -> Vec<Outcome> {
    sent.iter()
        .filter_map(|control| match control {
            SentControl::TransferOutcome { frame } => Some(frame.outcome),
            _ => None,
        })
        .collect()
}

fn stop_reasons(sent: &[SentControl]) -> Vec<String> {
    sent.iter()
        .filter_map(|control| match control {
            SentControl::Stop { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_call_the_loop_starts_offers_the_transfer_and_ringing_reaches_oaiy_and_then_what_happens() {
    let mut l = Loop::new();
    l.pass();
    // The fresh start offered the transfer and resumes nothing.
    assert_eq!(l.sessions(), 1);
    let start = l.start(0);
    assert!(start.allow_transfer, "the start says allowTransfer");
    assert!(start.resume.is_none(), "a fresh start resumes nothing");
    assert_eq!(start.call_id, CALL);
    assert!(l.lane.as_ref().unwrap().allow_transfer_sent, "the lane knows it offered it");
    assert!(!l.lane.as_ref().unwrap().transfer_negotiated, "not before OAIY says ready");

    // OAIY says ready with the feature: the loop negotiates and begins the call.
    l.ready(&["transfer_v1"]);
    l.pass();
    let lane = l.lane.as_ref().unwrap();
    assert!(lane.transfer_negotiated && lane.begun);
    assert!(l.sent().contains(&SentControl::Begin));

    // The tool call reaches the machine and the host is asked; the answer
    // reaches OAIY on a later pass, when the plan has come back.
    l.caller_says("Can I speak to the owner?");
    l.tool("tool_1", TOOL_NAME, json!({"reason": "caller_asked"}));
    l.pass();
    assert!(tool_result(&l.sent(), "tool_1").is_none(), "nothing to say until the host has planned the ring");
    l.host_answers(Rig::ring_plan());
    l.pass();
    let sent = l.sent();
    let (ok, output) = tool_result(&sent, "tool_1").expect("ringing reaches OAIY");
    assert!(ok);
    assert_eq!(output["status"], "ringing");
    let request_id = output["requestId"].as_str().unwrap().to_string();
    assert!(l.rig.broker.is_busy(), "the request is open");

    // An owner device accepts: OAIY hears it on the next pass, once.
    l.rig.accept(&request_id);
    l.pass();
    assert_eq!(outcomes(&l.sent()), vec![Outcome::Accepted]);
    l.pass();
    assert!(outcomes(&l.sent()).is_empty(), "said once");
}

/// The loop asks the mailbox it was given, not some other one: while another
/// request holds it, the tool is refused and the host is not asked.
#[test]
fn a_tool_call_is_refused_while_another_request_holds_the_mailbox() {
    let mut l = Loop::begun_with_transfer();
    let snapshot = l.rig.media.snapshot();
    let fence = crate::assistance::AssistanceCallFence {
        call_id: CALL.into(),
        call_epoch: snapshot.call_epoch,
        owner_epoch: snapshot.owner_epoch,
        switchboard_revision: 0,
        remote_revision: snapshot.remote_revision,
    };
    l.rig.broker.request(fence, "Is Tuesday free?", None, 60).expect("the mailbox was free");
    assert!(l.rig.broker.is_busy());
    l.caller_says("Can I speak to the owner?");
    l.tool("tool_1", TOOL_NAME, json!({"reason": "caller_asked"}));
    l.pass();
    let (ok, output) = tool_result(&l.sent(), "tool_1").expect("refused, not ignored");
    assert!(!ok);
    assert_eq!(output["reason"], "pending_request");
    assert!(
        l.sink.lines.iter().all(|line| !line.contains("oaiy.ring.plan")),
        "the host is not asked"
    );
}

#[test]
fn a_session_that_says_nothing_of_the_feature_never_gets_the_tool() {
    let mut l = Loop::new();
    l.pass();
    l.ready(&[]);
    l.pass();
    assert!(l.lane.as_ref().unwrap().begun);
    assert!(!l.lane.as_ref().unwrap().transfer_negotiated);
    l.sent();
    l.tool("tool_1", TOOL_NAME, json!({"reason": "caller_asked"}));
    l.pass();
    let (ok, output) = tool_result(&l.sent(), "tool_1").expect("refused, not ignored");
    assert!(!ok);
    assert_eq!(output, json!({"error": "unsupported"}));
    assert!(!l.rig.broker.is_busy());
}

/// The offer: only on the OAIY route, only for a call the caller placed, only
/// with the host ready and consent granted. The loop decides it at both of the
/// places it starts a session, and a resumed call is offered it the same way.
#[test]
fn the_offer_is_made_only_when_the_call_the_host_and_consent_allow_it_at_either_kind_of_start() {
    let offered = |l: &mut Loop| {
        l.pass();
        assert_eq!(l.sessions(), 1, "a session was started");
        let offered = l.start(0).allow_transfer;
        assert_eq!(l.lane.as_ref().unwrap().allow_transfer_sent, offered, "the lane records what the start said");
        l.ready(&["transfer_v1"]);
        l.pass();
        assert_eq!(l.lane.as_ref().unwrap().transfer_negotiated, offered, "negotiated exactly when offered");
        offered
    };
    // Each way of starting: a fresh one, and a resume of a call the AI had.
    for resumed in [false, true] {
        let make = |setup: &dyn Fn(&mut Loop)| {
            let mut l = Loop::new();
            setup(&mut l);
            if resumed {
                l.resume = Some(CALL.into());
            }
            offered(&mut l)
        };
        assert!(make(&|_| {}), "resumed {resumed}: an ordinary call is offered it");
        assert!(!make(&|l| l.status.transfer_ready.store(false, std::sync::atomic::Ordering::Relaxed)), "resumed {resumed}: the host is not ready");
        assert!(!make(&|l| l.rig.media.set_remote_consent(crate::remote_media::RemoteConsentGate::default())), "resumed {resumed}: no consent");
        assert!(
            !make(&|l| {
                l.rig.media.set_remote_consent(crate::remote_media::RemoteConsentGate {
                    enabled: true,
                    acknowledged: true,
                    acknowledged_at: Some("2026-07-18T00:00:00Z".into()),
                    expires_at: None,
                    assistance_enabled: true,
                    takeover_enabled: false,
                    ..Default::default()
                })
            }),
            "resumed {resumed}: takeover not granted"
        );
        // A call the plugin placed itself is never offered it.
        let mut l = Loop::outbound();
        if resumed {
            l.resume = Some(CALL.into());
        }
        assert!(!offered(&mut l), "resumed {resumed}: an outbound call");
    }
}

#[test]
fn oaiy_withdrawing_the_request_reaches_the_machine_and_a_call_is_not_finished_while_it_is_open() {
    let mut l = Loop::begun_with_transfer();
    l.agent_hangup = true;
    let request_id = l.ring();

    // finish_call while the request is open is answered with the typed error, and nothing else.
    l.tool("finish_1", "finish_call", json!({}));
    l.pass();
    let (ok, output) = tool_result(&l.sent(), "finish_1").expect("finish_call is answered");
    assert!(!ok);
    assert_eq!(output["error"], "transfer_in_progress");
    assert!(l.lane.as_ref().unwrap().authorized_finish_tool.is_none(), "no goodbye is armed");

    // OAIY withdraws the request: it hears cancelled, the mailbox is free.
    l.oaiy(RealtimeEventKind::TransferCancel {
        request_id: request_id.clone(),
        reason: CancelReason::OwnerDeclined,
    });
    l.pass();
    assert_eq!(outcomes(&l.sent()), vec![Outcome::Cancelled]);
    assert!(!l.rig.broker.is_busy());

    // With nothing open the call can be finished again.
    l.tool("finish_2", "finish_call", json!({}));
    l.pass();
    let (ok, output) = tool_result(&l.sent(), "finish_2").expect("finish_call is answered");
    assert!(ok, "{output}");
    assert_eq!(output["accepted"], true);
    assert!(l.lane.as_ref().unwrap().authorized_finish_tool.is_some());
}

#[test]
fn a_takeover_stops_the_session_as_a_handoff_and_the_loop_starts_a_fresh_one_that_says_how_it_got_the_caller_back() {
    let mut l = Loop::begun_with_transfer();
    l.ring();

    // Somebody else takes the caller: OAIY hears the request is moot, then the
    // stop that says handoff, not the end of the call. The loop lets go of the session.
    l.rig.media.observe_physical_call(Some("call_b"), true);
    l.pass();
    let sent = l.sent();
    assert_eq!(outcomes(&sent), vec![Outcome::Cancelled], "{sent:?}");
    assert_eq!(stop_reasons(&sent), vec![crate::transfer::STOP_HANDOFF_TAKEOVER.to_string()], "{sent:?}");
    assert!(matches!(sent.last(), Some(SentControl::Stop { .. })), "the stop comes last: {sent:?}");
    assert!(l.lane.is_none());
    assert_eq!(l.resume.as_deref(), Some(CALL), "the call is waiting for a fresh session");
    assert!(!l.rig.broker.is_busy());

    // The caller is back with the AI: a fresh session starts, and says how.
    l.rig.media.observe_physical_call(Some(CALL), true);
    l.pass();
    assert_eq!(l.sessions(), 2, "a fresh session for the returned caller");
    let start = l.start(1);
    let resume: ResumeInfo = start.resume.clone().expect("the fresh start carries start.resume");
    assert_eq!(resume.via, Via::Return);
    assert!(resume.after_handoff);
    assert_eq!(start.greeting, crate::transfer::RETURN_GREETING);
    assert!(start.allow_transfer, "it is offered the transfer again");
    assert!(l.lane.as_ref().unwrap().allow_transfer_sent);
    assert!(l.resume.is_none());
    assert!(
        l.ctx.transfer.peek_resume(Instant::now(), &l.rig.broker).is_none(),
        "the handoff is over once the fresh session exists"
    );

    // That session negotiates on its own ready.
    l.ready(&["transfer_v1"]);
    l.pass();
    assert!(l.lane.as_ref().unwrap().transfer_negotiated);
}

#[test]
fn a_resume_that_follows_no_handoff_says_nothing_of_one() {
    let mut l = Loop::new();
    l.resume = Some(CALL.into());
    l.pass();
    assert_eq!(l.sessions(), 1);
    let start = l.start(0);
    assert!(start.resume.is_none());
    assert_ne!(start.greeting, crate::transfer::RETURN_GREETING);
    assert!(start.allow_transfer);
}

/// What was held for a session that was not there is sent the turn one negotiates,
/// before anything that stops it, and what waits behind an answer follows the answer
/// in the same turn.
#[test]
fn held_outcomes_go_out_before_a_stop_and_right_behind_the_answer_they_waited_for() {
    let frame = |request: &str, outcome| crate::transfer::OutcomeFrame {
        request_id: request.into(),
        outcome,
        message: None,
        at_ms: 1_789_000_000_000,
    };

    // Held, and the session is about to be stopped because somebody took the
    // caller: OAIY hears the outcome first, then the stop.
    let mut l = Loop::begun_with_transfer();
    let token = l.lane.as_ref().unwrap().session_token;
    l.ctx.transfer.hold_outcome(frame("assist_held", Outcome::Unavailable), token);
    l.rig.media.observe_physical_call(Some("call_b"), true);
    l.pass();
    let sent = l.sent();
    assert_eq!(outcomes(&sent), vec![Outcome::Unavailable], "{sent:?}");
    let outcome_at = sent.iter().position(|control| matches!(control, SentControl::TransferOutcome { .. }));
    let stop_at = sent.iter().position(|control| matches!(control, SentControl::Stop { .. }));
    assert!(outcome_at.is_some() && outcome_at < stop_at, "the outcome before the stop: {sent:?}");
    assert!(!l.ctx.transfer.has_held_outcomes());

    // Held behind an answer that is going out this turn: the answer, then the outcome, in one turn.
    let mut l = Loop::begun_with_transfer();
    let token = l.lane.as_ref().unwrap().session_token;
    l.lane.as_mut().unwrap().queue_tool_answer("tool_9".into(), TOOL_NAME, true, json!({"status": "ringing"}));
    l.ctx.transfer.hold_outcome(frame("assist_held", Outcome::Cancelled), token);
    l.pass();
    let sent = l.sent();
    let answer_at = sent.iter().position(|control| matches!(control, SentControl::ToolResult { .. }));
    let outcome_at = sent.iter().position(|control| matches!(control, SentControl::TransferOutcome { .. }));
    assert!(answer_at.is_some() && outcome_at.is_some() && answer_at < outcome_at, "answer, then outcome: {sent:?}");
    assert!(!l.ctx.transfer.has_held_outcomes());
}
