//! One call's transfers to the owner: the state machine behind
//! `transfer_to_owner`.
//!
//! The realtime tool must answer at once and the outcome arrives later, so a
//! transfer is a small machine that the radio loop polls once a turn:
//!
//! ```text
//! Idle --begin--> Planning --plan: ring--> Ringing --endpoint wins--> Accepted
//!                    |                        |  \                       |
//!                    +--refused/failed        |   +--declined            +--taken: no frame,
//!                       (tool answer)         |   +--expired                the session stops
//!                                             |   +--cancelled              (handoff:takeover)
//!                                             +--(any of the above)      +--unavailable: the AI
//!                                                                           has the caller back
//! ```
//!
//! Every path ends in one typed outcome or a tool refusal, and a hard
//! deadline bounds the ring and the media setup even if the broker's own
//! clocks misbehave. Nothing here touches audio or decides who wins: the
//! assistance broker's compare-and-swap does, and the existing v2 takeover
//! path moves the caller. This module only asks, waits and reports, and it
//! never reports anything the call or the owner wrote except the owner's own
//! bounded decline message.
//!
//! Consent is checked at the door (a refusal) and while ringing (a withdrawal
//! cancels the request); nothing here can raise consent, and the plugin's
//! media gates recheck it again at every claim.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aokie_core::events::DesktopEvent;
use serde_json::Value;

use super::{
    caller_asked, opened_params, parse_arguments, parse_plan, plan_params, refusal, ringing,
    sanitize_owner_message, Decision, Outcome, OutcomeFrame, Reason, RefusalStatus, ResumeInfo,
    ToolAnswer, Via,
};
use crate::assistance::{
    AssistanceBroker, AssistanceCallFence, AssistanceResolution, TRANSFER_RESOLUTION_GRACE_SECONDS,
    TRANSFER_SETUP_SECONDS,
};
use crate::event_bridge::Sink;
use crate::host_rpc::{HostResult, HostRpc};
use crate::radio::{AssistanceAuditLifecycle, AssistanceAuditResolution};
use crate::remote_media::{AokieOwnerFence, RemoteMediaHandle};

/// How long the plugin waits for the host's ring plan. Past this the request
/// is refused as `plan_unavailable`.
pub const PLAN_WAIT: Duration = Duration::from_millis(1_500);

/// The plugin's own ceilings, above anything the host's ring policy allows by
/// default (two per call, a minute apart, ten an hour). They stand on their
/// own so that a host with no limits, or a model that will not stop asking,
/// still cannot make the owner's phone ring without end.
pub const MAX_ATTEMPTS_PER_CALL: u32 = 3;
pub const MIN_GAP_BETWEEN_ATTEMPTS: Duration = Duration::from_secs(15);
pub const MAX_ATTEMPTS_PER_HOUR: usize = 20;

/// Slack after the broker's own last deadline before the plugin ends the
/// request itself.
const HARD_DEADLINE_SLACK: Duration = Duration::from_secs(5);

// --- The hourly ceiling ----------------------------------------------------

/// Requests opened in the last hour, across calls.
#[derive(Default)]
pub struct Governor {
    opened: Mutex<VecDeque<Instant>>,
}

impl Governor {
    pub fn global() -> Arc<Governor> {
        static GLOBAL: OnceLock<Arc<Governor>> = OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| Arc::new(Governor::default())))
    }

    fn prune(opened: &mut VecDeque<Instant>, now: Instant) {
        while opened
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= Duration::from_secs(3_600))
        {
            opened.pop_front();
        }
    }

    fn allows(&self, now: Instant) -> bool {
        let Ok(mut opened) = self.opened.lock() else {
            return false;
        };
        Self::prune(&mut opened, now);
        opened.len() < MAX_ATTEMPTS_PER_HOUR
    }

    fn note_opened(&self, now: Instant) {
        if let Ok(mut opened) = self.opened.lock() {
            Self::prune(&mut opened, now);
            opened.push_back(now);
        }
    }
}

// --- What the machine is given and what it returns --------------------------

/// The world the machine reads and acts on for one call into it.
pub struct TransferEnv<'a> {
    pub broker: &'a AssistanceBroker,
    pub media: &'a RemoteMediaHandle,
    pub host: &'a Arc<HostRpc>,
    /// Where host requests are written (`oaiy.ring.plan`, `oaiy.ring.opened`).
    pub sink: &'a mut dyn Sink,
    pub now: Instant,
    /// The call the radio currently holds as active, if any.
    pub active_call_id: Option<&'a str>,
    pub switchboard_revision: u64,
    /// Whether the host announced `ringPlan` at init. Without it nothing asks
    /// the host and nothing is rung.
    pub host_ring_plan: bool,
    /// Identifies the realtime session a tool answer would go to. A tool call
    /// belongs to the session that made it: if the session was replaced while
    /// the plan was awaited, the answer is dropped rather than sent to a
    /// session that never asked.
    pub session_token: u64,
}

pub struct BeginArgs<'a> {
    pub tool_call_id: &'a str,
    pub arguments: &'a Value,
    pub call_id: &'a str,
    /// The realtime lane's exact Aokie owner fence.
    pub owner: &'a AokieOwnerFence,
    /// The caller's newest turns, oldest first.
    pub recent_caller_turns: &'a [String],
    pub caller_number: Option<&'a str>,
}

/// What `begin` did.
pub enum Begin {
    /// Refused or failed at once: this is the tool's answer.
    Answered(ToolAnswer),
    /// Waiting for the host's ring plan. The tool's answer arrives from
    /// `poll` as an `Effect::ToolAnswer` within [`PLAN_WAIT`].
    Planning,
}

/// What the radio loop must do for the machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Complete this tool call (to the session that made it).
    ToolAnswer {
        tool_call_id: String,
        answer: ToolAnswer,
    },
    /// Tell OAIY how the request ended. Only if a session that negotiated
    /// `transfer_v1` is open for this call; otherwise it is dropped.
    Outcome(OutcomeFrame),
    /// A durable `aokie.call.assistance.*` event.
    Audit(DesktopEvent),
}

// --- The machine -------------------------------------------------------------

/// How the last request ended, for a session that starts afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Taken,
    Unavailable,
    Other,
}

struct Planning {
    tool_call_id: String,
    call_id: String,
    reason: Reason,
    owner: AokieOwnerFence,
    session_token: u64,
    host: Arc<HostRpc>,
    rpc_id: Option<u64>,
    rx: Receiver<HostResult>,
    deadline: Instant,
}

impl Drop for Planning {
    fn drop(&mut self) {
        if let Some(id) = self.rpc_id.take() {
            self.host.forget(id);
        }
    }
}

struct Open {
    broker: AssistanceBroker,
    request_id: String,
    fence: AssistanceCallFence,
    audit: AssistanceAuditLifecycle,
    accepted_sent: bool,
    hard_deadline: Instant,
}

impl Drop for Open {
    /// A request still open when the call's context is dropped (the call
    /// boundary) is withdrawn from the mailbox. A resolved one has already
    /// been taken, so this is then nothing.
    fn drop(&mut self) {
        self.broker.discard(&self.request_id);
    }
}

enum Stage {
    Idle,
    Planning(Planning),
    Ringing(Open),
    Accepted(Open),
}

/// The transfers of one call. Lives in the call's context, so a new call
/// starts with a fresh one and a parked call keeps its own.
pub struct TransferCall {
    stage: Stage,
    attempts: u32,
    last_opened: Option<Instant>,
    governor: Arc<Governor>,
    last_end: Option<End>,
    handoff_started: Option<Instant>,
}

impl Default for TransferCall {
    fn default() -> Self {
        Self::new(Governor::global())
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn unix_seconds() -> u64 {
    unix_ms() / 1_000
}

fn answer(tool_call_id: &str, answer: ToolAnswer) -> Effect {
    Effect::ToolAnswer {
        tool_call_id: tool_call_id.to_string(),
        answer,
    }
}

impl TransferCall {
    pub fn new(governor: Arc<Governor>) -> Self {
        Self {
            stage: Stage::Idle,
            attempts: 0,
            last_opened: None,
            governor,
            last_end: None,
            handoff_started: None,
        }
    }

    /// Whether a transfer is planned, ringing or accepted right now.
    pub fn is_active(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }

    /// The open request's id, once there is one.
    pub fn request_id(&self) -> Option<&str> {
        match &self.stage {
            Stage::Ringing(open) | Stage::Accepted(open) => Some(open.request_id.as_str()),
            _ => None,
        }
    }

    /// Whether a request is ringing and no endpoint has won it yet.
    pub fn is_ringing_unaccepted(&self) -> bool {
        matches!(self.stage, Stage::Ringing(_))
    }

    /// How many requests this call has opened.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// `transfer_to_owner` was called. Checks, in order: the arguments, that
    /// no request is open, that the call and the owner fence are current,
    /// consent, the plugin's ceilings, the caller-asked phrase check, and
    /// then asks the host for a ring plan.
    pub fn begin(&mut self, env: &mut TransferEnv<'_>, args: BeginArgs<'_>) -> Begin {
        let refused = |status, reason: &str| Begin::Answered(refusal(status, reason));

        let Ok(reason) = parse_arguments(args.arguments) else {
            return refused(RefusalStatus::Refused, "bad_arguments");
        };
        if !matches!(self.stage, Stage::Idle) || env.broker.is_busy() {
            return refused(RefusalStatus::Refused, "pending_request");
        }
        let remote = env.media.snapshot();
        let exact_call = env.active_call_id == Some(args.call_id)
            && remote.call_id.as_deref() == Some(args.call_id)
            && remote.call_epoch > 0
            && env.media.aokie_owner_fence().as_ref() == Some(args.owner);
        if !exact_call {
            return refused(RefusalStatus::Unavailable, "call_changed");
        }
        if !(remote.consent.assistance_enabled && remote.consent.takeover_enabled) {
            return refused(RefusalStatus::Refused, "consent");
        }
        if self.attempts >= MAX_ATTEMPTS_PER_CALL {
            return refused(RefusalStatus::Refused, "limit_call");
        }
        if self
            .last_opened
            .is_some_and(|at| env.now.saturating_duration_since(at) < MIN_GAP_BETWEEN_ATTEMPTS)
        {
            return refused(RefusalStatus::Refused, "limit_gap");
        }
        if !self.governor.allows(env.now) {
            return refused(RefusalStatus::Refused, "limit_global");
        }
        if reason == Reason::CallerAsked && !caller_asked(args.recent_caller_turns) {
            return refused(RefusalStatus::Refused, "caller_did_not_ask");
        }
        if !env.host_ring_plan {
            return refused(RefusalStatus::Unavailable, "plan_unavailable");
        }

        let params = plan_params(
            args.call_id,
            remote.call_epoch,
            remote.owner_epoch,
            reason,
            args.caller_number,
            args.recent_caller_turns,
        );
        let (rpc_id, line, rx) = env.host.begin("oaiy.ring.plan", params);
        if env.sink.send_line(&line).is_err() {
            env.host.forget(rpc_id);
            return refused(RefusalStatus::Unavailable, "plan_unavailable");
        }
        self.stage = Stage::Planning(Planning {
            tool_call_id: args.tool_call_id.to_string(),
            call_id: args.call_id.to_string(),
            reason,
            owner: args.owner.clone(),
            session_token: env.session_token,
            host: Arc::clone(env.host),
            rpc_id: Some(rpc_id),
            rx,
            deadline: env.now + PLAN_WAIT,
        });
        Begin::Planning
    }

    /// One turn of the radio loop.
    pub fn poll(&mut self, env: &mut TransferEnv<'_>) -> Vec<Effect> {
        let mut effects = Vec::new();
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Idle => {}
            Stage::Planning(planning) => self.poll_planning(planning, env, &mut effects),
            Stage::Ringing(open) => self.poll_open(open, false, env, &mut effects),
            Stage::Accepted(open) => self.poll_open(open, true, env, &mut effects),
        }
        effects
    }

    /// The session is about to stop because the owner fence changed. A request
    /// nobody has won is withdrawn (its ring is moot: someone took the call
    /// another way); an accepted one stays, since its takeover is exactly what
    /// is changing the fence.
    pub fn withdraw_unaccepted(&mut self, env: &mut TransferEnv<'_>) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Stage::Ringing(_) = self.stage {
            if let Stage::Ringing(open) = std::mem::replace(&mut self.stage, Stage::Idle) {
                self.cancel(open, env, &mut effects, "the caller changed hands another way");
            }
        }
        effects
    }

    fn poll_planning(
        &mut self,
        planning: Planning,
        env: &mut TransferEnv<'_>,
        effects: &mut Vec<Effect>,
    ) {
        let answered = |effects: &mut Vec<Effect>, planning: &Planning, reply: ToolAnswer| {
            effects.push(answer(&planning.tool_call_id, reply));
        };
        // The session that asked is gone, or the call is: nobody is waiting
        // for this answer any more. Nothing was opened.
        if planning.session_token != env.session_token
            || env.active_call_id != Some(planning.call_id.as_str())
        {
            eprintln!("[aokie-plugin] transfer plan dropped: its call or session ended first");
            return;
        }
        let result = match planning.rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) if env.now < planning.deadline => {
                self.stage = Stage::Planning(planning);
                return;
            }
            Err(TryRecvError::Empty) => {
                eprintln!("[aokie-plugin] transfer refused: the host did not answer the ring plan in time");
                answered(effects, &planning, refusal(RefusalStatus::Unavailable, "plan_unavailable"));
                return;
            }
            Err(TryRecvError::Disconnected) => {
                answered(effects, &planning, refusal(RefusalStatus::Unavailable, "plan_unavailable"));
                return;
            }
        };
        let plan = match result.map_err(|error| error.to_string()).and_then(|value| parse_plan(&value)) {
            Ok(plan) => plan,
            Err(error) => {
                eprintln!("[aokie-plugin] transfer refused: the ring plan was unusable ({error})");
                answered(effects, &planning, refusal(RefusalStatus::Unavailable, "plan_unavailable"));
                return;
            }
        };
        match plan.decision {
            Decision::Refused => {
                answered(effects, &planning, refusal(RefusalStatus::Refused, &plan.reason));
                return;
            }
            Decision::MessageOnly => {
                answered(effects, &planning, refusal(RefusalStatus::Unavailable, &plan.reason));
                return;
            }
            Decision::Ring => {}
        }
        // A ring aimed at nobody could only time out, and would tell the
        // model somebody was being rung.
        let targets = plan.targets();
        if targets.is_empty() {
            answered(effects, &planning, refusal(RefusalStatus::Unavailable, "no_endpoint"));
            return;
        }

        // The world moved while the host thought about it: check it again.
        let remote = env.media.snapshot();
        if !(remote.consent.assistance_enabled && remote.consent.takeover_enabled) {
            answered(effects, &planning, refusal(RefusalStatus::Refused, "consent"));
            return;
        }
        if remote.call_id.as_deref() != Some(planning.call_id.as_str()) || remote.call_epoch == 0 {
            answered(effects, &planning, refusal(RefusalStatus::Unavailable, "call_changed"));
            return;
        }
        let fence = AssistanceCallFence {
            call_id: planning.call_id.clone(),
            call_epoch: remote.call_epoch,
            owner_epoch: remote.owner_epoch,
            switchboard_revision: env.switchboard_revision,
            remote_revision: remote.remote_revision,
        };
        // The request is created while the exact Aokie owner still holds the
        // caller: whoever wins the media-state lock, the AI or a claim,
        // decides, and a claim that won leaves nothing open.
        let opened = env.media.with_aokie_owner(&planning.owner, || {
            env.broker
                .request_transfer_to(fence.clone(), "", None, plan.ring_seconds, Some(targets.clone()))
        });
        let request_id = match opened {
            Ok(Ok(request_id)) => request_id,
            Ok(Err(_)) => {
                answered(effects, &planning, refusal(RefusalStatus::Refused, "pending_request"));
                return;
            }
            Err(_) => {
                answered(effects, &planning, refusal(RefusalStatus::Unavailable, "call_changed"));
                return;
            }
        };

        self.attempts += 1;
        self.last_opened = Some(env.now);
        self.governor.note_opened(env.now);
        let (audit, requested) = AssistanceAuditLifecycle::opened(&request_id, &fence.call_id);
        effects.push(Effect::Audit(requested));

        // Tell the host the request is open so it can start delivery (the
        // desktop toast, the ring items). Nothing waits for its answer: a
        // host that fails to ring still leaves every connected device offered.
        let opened_line = {
            let params = opened_params(
                &plan.plan_id,
                &request_id,
                &fence.call_id,
                fence.call_epoch,
                fence.owner_epoch,
                unix_seconds() + plan.ring_seconds,
            );
            let (id, line, _rx) = env.host.begin("oaiy.ring.opened", params);
            env.host.forget(id);
            line
        };
        if env.sink.send_line(&opened_line).is_err() {
            eprintln!("[aokie-plugin] transfer {request_id}: the host could not be told the ring is open");
        }
        eprintln!(
            "[aokie-plugin] transfer {request_id} opened: ring {} s, {} target device(s), reason {}",
            plan.ring_seconds,
            targets.len(),
            planning.reason.as_str()
        );

        let hard = Duration::from_secs(
            plan.ring_seconds + TRANSFER_SETUP_SECONDS + TRANSFER_RESOLUTION_GRACE_SECONDS,
        ) + HARD_DEADLINE_SLACK;
        effects.push(answer(&planning.tool_call_id, ringing(&request_id, plan.ring_seconds)));
        self.stage = Stage::Ringing(Open {
            broker: env.broker.clone(),
            request_id,
            fence,
            audit,
            accepted_sent: false,
            hard_deadline: env.now + hard,
        });
    }

    fn poll_open(
        &mut self,
        mut open: Open,
        accepted: bool,
        env: &mut TransferEnv<'_>,
        effects: &mut Vec<Effect>,
    ) {
        // The caller hung up, or the call is no longer the radio's active one.
        if env.active_call_id != Some(open.fence.call_id.as_str()) {
            self.cancel(open, env, effects, "the call ended");
            return;
        }
        // Consent taken back while ringing withdraws the request. Once an
        // endpoint has won, the media path owns consent: revoking it there
        // returns the caller and the broker resolves the request.
        if !accepted {
            let consent = env.media.snapshot().consent;
            if !(consent.assistance_enabled && consent.takeover_enabled) {
                self.cancel(open, env, effects, "consent was withdrawn");
                return;
            }
        }
        // Resolved, or out of time at the broker.
        if !env.broker.is_waiting(&open.request_id) {
            let resolution = env
                .broker
                .take_resolution(&open.request_id)
                .unwrap_or(AssistanceResolution::Expired);
            env.broker.discard(&open.request_id);
            self.finish(open, resolution, effects);
            return;
        }
        // The plugin's own last word, on the monotonic clock, in case the
        // wall-clock deadlines the broker uses were disturbed.
        if env.now >= open.hard_deadline {
            eprintln!(
                "[aokie-plugin] transfer {} ended by the plugin's own deadline",
                open.request_id
            );
            env.broker.discard(&open.request_id);
            self.finish(open, AssistanceResolution::Expired, effects);
            return;
        }
        let won = env
            .broker
            .accepted_transfer(&open.fence.call_id, open.fence.call_epoch)
            .is_some_and(|pending| pending.request_id == open.request_id && pending.accepted_by.is_some());
        if won && !open.accepted_sent {
            open.accepted_sent = true;
            eprintln!("[aokie-plugin] transfer {} accepted by an owner endpoint", open.request_id);
            effects.push(Effect::Outcome(OutcomeFrame {
                request_id: open.request_id.clone(),
                outcome: Outcome::Accepted,
                message: None,
                at_ms: unix_ms(),
            }));
        }
        // An acceptance whose media transaction was rolled back is back to
        // ringing: the broker released it and another endpoint may still win.
        self.stage = if won {
            Stage::Accepted(open)
        } else {
            Stage::Ringing(open)
        };
    }

    fn cancel(
        &mut self,
        mut open: Open,
        env: &mut TransferEnv<'_>,
        effects: &mut Vec<Effect>,
        why: &str,
    ) {
        env.broker.discard(&open.request_id);
        eprintln!("[aokie-plugin] transfer {} cancelled: {why}", open.request_id);
        effects.push(Effect::Outcome(OutcomeFrame {
            request_id: open.request_id.clone(),
            outcome: Outcome::Cancelled,
            message: None,
            at_ms: unix_ms(),
        }));
        if let Some(event) = open.audit.resolve(AssistanceAuditResolution::Cancelled) {
            effects.push(Effect::Audit(event));
        }
        self.last_end = Some(End::Other);
    }

    fn finish(&mut self, mut open: Open, resolution: AssistanceResolution, effects: &mut Vec<Effect>) {
        let request_id = open.request_id.clone();
        let (outcome, message, audit_event, end) = match resolution {
            AssistanceResolution::TransferTaken { device_id } => {
                // The takeover completed: the session stops with
                // `handoff:takeover`, and no outcome is sent.
                let event = open.audit.resolve(AssistanceAuditResolution::Transferred(&device_id));
                (None, None, event, End::Taken)
            }
            AssistanceResolution::Declined {
                device_id, answer, ..
            } => {
                let event = open.audit.resolve(AssistanceAuditResolution::Declined(&device_id));
                (
                    Some(Outcome::Declined),
                    sanitize_owner_message(&answer),
                    event,
                    End::Other,
                )
            }
            AssistanceResolution::TransferUnavailable { .. } | AssistanceResolution::Answered(_) => {
                let event = open.audit.resolve(AssistanceAuditResolution::Unavailable);
                (Some(Outcome::Unavailable), None, event, End::Unavailable)
            }
            AssistanceResolution::Expired if open.accepted_sent => {
                // Somebody accepted and then nothing completed inside the
                // media window: the caller never reached the owner.
                let event = open.audit.resolve(AssistanceAuditResolution::Unavailable);
                (Some(Outcome::Unavailable), None, event, End::Unavailable)
            }
            AssistanceResolution::Expired => {
                let event = open.audit.resolve(AssistanceAuditResolution::Expired);
                (Some(Outcome::Expired), None, event, End::Other)
            }
        };
        eprintln!(
            "[aokie-plugin] transfer {request_id} resolved: {}",
            outcome.map(Outcome::as_str).unwrap_or("taken")
        );
        if let Some(outcome) = outcome {
            effects.push(Effect::Outcome(OutcomeFrame {
                request_id,
                outcome,
                message,
                at_ms: unix_ms(),
            }));
        }
        if let Some(event) = audit_event {
            effects.push(Effect::Audit(event));
        }
        self.last_end = Some(end);
    }

    // --- The handoff and what follows ---------------------------------------

    /// The realtime session for this call was stopped because a human took
    /// the caller. Remember when, so the session that follows can say how long
    /// the AI was away.
    pub fn note_handoff(&mut self, now: Instant) {
        self.handoff_started.get_or_insert(now);
    }

    /// The fresh session for the same call is about to start after a handoff:
    /// how long the AI was away, and whether the owner handed the call back
    /// (`return`) or the media setup failed (`failback`). `None` when there
    /// was no handoff to resume from. Consumes the record.
    pub fn take_resume(&mut self, now: Instant, broker: &AssistanceBroker) -> Option<ResumeInfo> {
        let started = self.handoff_started.take()?;
        let unresolved_failure = match &self.stage {
            Stage::Ringing(open) | Stage::Accepted(open) => matches!(
                broker.peek_resolution(&open.request_id),
                Some(AssistanceResolution::TransferUnavailable { .. })
            ),
            _ => false,
        };
        let via = if unresolved_failure || self.last_end == Some(End::Unavailable) {
            Via::Failback
        } else {
            Via::Return
        };
        Some(ResumeInfo::new(
            now.saturating_duration_since(started).as_secs(),
            via,
        ))
    }
}

#[cfg(test)]
#[path = "call_tests.rs"]
mod tests;
