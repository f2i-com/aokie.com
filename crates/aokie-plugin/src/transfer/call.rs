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

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aokie_core::events::DesktopEvent;
use serde_json::Value;

use super::{
    caller_asked, opened_params, parse_arguments, parse_plan, plan_params, refusal, ringing,
    sanitize_owner_message, CancelReason, Decision, Notice, NoticeFrame, Outcome, OutcomeFrame,
    Reason, RefusalStatus, ResumeInfo, Targets, ToolAnswer, Via,
};
use crate::assistance::{
    AssistanceBroker, AssistanceCallFence, AssistanceResolution, Withdrawal,
    TRANSFER_RESOLUTION_GRACE_SECONDS, TRANSFER_SETUP_SECONDS,
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
/// How long after a request ends (declined, expired, failed, withdrawn) the
/// next may open. Measured from the end, not from the opening: a ring lasts at
/// least 20 seconds, so a gap from the opening would only ever hold back a
/// request that ended within it, and a decline is exactly that.
pub const MIN_GAP_BETWEEN_ATTEMPTS: Duration = Duration::from_secs(15);
pub const MAX_ATTEMPTS_PER_HOUR: usize = 20;
/// Requests one caller number may cost the owner in an hour, across calls: a
/// caller who rings back again and again must not be able to ring the owner
/// without end even if each call stays inside the per-call allowance.
pub const MAX_ATTEMPTS_PER_CALLER_PER_HOUR: usize = 3;
/// Calls with no usable number (withheld, "anonymous", empty, too short) cannot
/// be told apart, so they share ONE small bucket: a caller who withholds the
/// number can neither ring the owner without end nor drain the hourly ceiling
/// above for everyone else.
pub const MAX_ATTEMPTS_PER_WITHHELD_PER_HOUR: usize = 2;
/// Caller numbers remembered (as keyed hashes) at once.
const MAX_CALLERS_TRACKED: usize = 256;

/// Slack after the broker's own last deadline before the plugin ends the
/// request itself.
const HARD_DEADLINE_SLACK: Duration = Duration::from_secs(5);

// --- The hourly ceiling ----------------------------------------------------

/// Requests opened in the last hour, across calls, and per caller number.
///
/// A caller number is never stored. It is reduced to its digits (an Australian
/// `+61` prefix folded to the leading `0`, so the two ways a network writes the
/// same mobile agree) and keyed with a hash whose key is random for this
/// process: the table holds hashes and times, lives in memory only, and is
/// bounded.
#[derive(Default)]
pub struct Governor {
    opened: Mutex<VecDeque<Instant>>,
    callers: Mutex<HashMap<u64, VecDeque<Instant>>>,
    /// The one bucket every call with no usable number shares.
    withheld: Mutex<VecDeque<Instant>>,
    key: RandomState,
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

    /// The table key for a caller number, or `None` when there is no usable
    /// number (withheld, "anonymous", empty).
    pub(crate) fn caller_key(&self, number: &str) -> Option<u64> {
        let mut digits: String = number.chars().filter(char::is_ascii_digit).collect();
        if digits.len() == 11 && digits.starts_with("61") {
            digits.replace_range(..2, "0");
        }
        (digits.len() >= 6).then(|| self.key.hash_one(digits))
    }

    fn allows_caller(&self, key: Option<u64>, now: Instant) -> bool {
        let Some(key) = key else {
            // No usable number: the shared bucket.
            let Ok(mut withheld) = self.withheld.lock() else {
                return false;
            };
            Self::prune(&mut withheld, now);
            return withheld.len() < MAX_ATTEMPTS_PER_WITHHELD_PER_HOUR;
        };
        let Ok(mut callers) = self.callers.lock() else {
            return false;
        };
        let Some(opened) = callers.get_mut(&key) else {
            return true;
        };
        Self::prune(opened, now);
        opened.len() < MAX_ATTEMPTS_PER_CALLER_PER_HOUR
    }

    fn note_opened(&self, now: Instant, key: Option<u64>) {
        if let Ok(mut opened) = self.opened.lock() {
            Self::prune(&mut opened, now);
            opened.push_back(now);
        }
        let Some(key) = key else {
            if let Ok(mut withheld) = self.withheld.lock() {
                Self::prune(&mut withheld, now);
                withheld.push_back(now);
            }
            return;
        };
        if let Ok(mut callers) = self.callers.lock() {
            if callers.len() >= MAX_CALLERS_TRACKED && !callers.contains_key(&key) {
                callers.retain(|_, opened| {
                    Self::prune(opened, now);
                    !opened.is_empty()
                });
                // Still full of live callers: forget the one seen longest ago.
                if callers.len() >= MAX_CALLERS_TRACKED {
                    let oldest = callers
                        .iter()
                        .filter_map(|(key, opened)| opened.back().map(|at| (*key, *at)))
                        .min_by_key(|(_, at)| *at)
                        .map(|(key, _)| key);
                    if let Some(oldest) = oldest {
                        callers.remove(&oldest);
                    }
                }
            }
            let opened = callers.entry(key).or_default();
            Self::prune(opened, now);
            opened.push_back(now);
        }
    }

    /// How many caller numbers are remembered.
    #[cfg(test)]
    pub(crate) fn callers_tracked(&self) -> usize {
        self.callers.lock().map(|callers| callers.len()).unwrap_or(0)
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
    /// The caller's newest turns not yet spent, oldest first.
    pub recent_caller_turns: &'a [String],
    /// How many turns the caller has said in this call so far, spent or not:
    /// the request this begins spends that many if it opens.
    pub caller_turns_heard: usize,
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
    /// Tell OAIY how the request ended. Goes on a session that negotiated
    /// `transfer_v1`; while none is open a terminal outcome is held for the
    /// next one (`hold_outcome`), and an `accepted` is dropped.
    Outcome(OutcomeFrame),
    /// Tell OAIY that a `transfer_cancel` it sent changed nothing (and why).
    /// Sent at once on the session that asked; never held for a later one.
    Notice(NoticeFrame),
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
    /// The phrase check on the caller's recent turns, taken when the tool was
    /// called (only the verdict is kept, never the turns).
    caller_asked: bool,
    /// How many of the caller's turns had been said when the tool was called:
    /// what the request spends when it opens.
    heard: usize,
    /// The keyed hash of the caller's number (never the number).
    caller_key: Option<u64>,
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

/// An outcome waiting to be sent, and the session it was made for.
struct Held {
    frame: OutcomeFrame,
    session_token: u64,
}

/// The transfers of one call. Lives in the call's context, so a new call
/// starts with a fresh one and a parked call keeps its own.
pub struct TransferCall {
    stage: Stage,
    attempts: u32,
    /// When the last request ended; the gap before the next is measured from
    /// here.
    last_ended: Option<Instant>,
    governor: Arc<Governor>,
    last_end: Option<End>,
    handoff_started: Option<Instant>,
    held: Vec<Held>,
    /// The handoff an open request belonged to is over (its fresh session has
    /// started), so however the request ends is not the next handoff's story.
    end_is_spent: bool,
    /// How many of the caller's turns (counted from the start of the call, in
    /// the order they were said) are spent: the phrase floor reads only what
    /// was said after them. A caller's ask is spent by the request it opens
    /// (the host authorised the ring and the request is open: not by a plan
    /// that was refused, was not answered or could not be read, which leave
    /// the ask to be tried again) and by a hand-back to the AI (a fresh session
    /// after a takeover or a hold), so it counts for one request and never for
    /// one that comes after the AI lost the line and got it back.
    caller_turns_spent: usize,
    /// Test seam: runs against the broker between the machine's last look and
    /// the withdrawal, the instant a phone could accept.
    #[cfg(test)]
    pub(crate) before_withdrawal: Option<BeforeWithdrawal>,
}

/// What runs at the [`TransferCall::before_withdrawal`] seam.
#[cfg(test)]
pub(crate) type BeforeWithdrawal = Box<dyn FnOnce(&AssistanceBroker)>;

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
            last_ended: None,
            governor,
            last_end: None,
            handoff_started: None,
            held: Vec::new(),
            end_is_spent: false,
            caller_turns_spent: 0,
            #[cfg(test)]
            before_withdrawal: None,
        }
    }

    /// How many of the caller's turns are spent (see the field): the phrase
    /// floor and the plan request read the turns after this many.
    pub fn caller_turns_spent(&self) -> usize {
        self.caller_turns_spent
    }

    /// The caller's first `heard` turns are spent. Never fewer than were.
    pub fn spend_caller_turns(&mut self, heard: usize) {
        self.caller_turns_spent = self.caller_turns_spent.max(heard);
    }

    /// Test seam: the gap since the last request has passed, so the next one is
    /// judged on what the caller said and not on the clock.
    #[cfg(all(test, target_os = "windows", feature = "voice"))]
    pub(crate) fn forget_the_last_end(&mut self) {
        self.last_ended = None;
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

        let Some(reason) = parse_arguments(args.arguments) else {
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
            .last_ended
            .is_some_and(|at| env.now.saturating_duration_since(at) < MIN_GAP_BETWEEN_ATTEMPTS)
        {
            return refused(RefusalStatus::Refused, "limit_gap");
        }
        // A caller who rings back again and again costs the owner one request
        // each time however tidy each call is. Only a keyed hash of the number
        // is kept, and calls with no usable number share one small bucket.
        let caller_key = args
            .caller_number
            .and_then(|number| self.governor.caller_key(number));
        if !self.governor.allows_caller(caller_key, env.now) {
            return refused(RefusalStatus::Refused, "limit_caller");
        }
        if !self.governor.allows(env.now) {
            return refused(RefusalStatus::Refused, "limit_global");
        }
        let caller_did_ask = caller_asked(args.recent_caller_turns);
        if reason == Reason::CallerAsked && !caller_did_ask {
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
            caller_asked: caller_did_ask,
            heard: args.caller_turns_heard,
            caller_key,
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

    /// Withdraw a request nobody has won, ATOMICALLY: one broker operation
    /// decides. If an owner device won it (or it resolved) between the last look
    /// and now, the request is left exactly as it is and the poll reports what
    /// happened (`accepted`, or how it ended), so a phone that was already
    /// acknowledged is never discarded from under its takeover. Returns whether
    /// it was withdrawn.
    fn withdraw_unwon(
        &mut self,
        open: Open,
        env: &mut TransferEnv<'_>,
        effects: &mut Vec<Effect>,
        why: &str,
    ) -> bool {
        #[cfg(test)]
        if let Some(interleave) = self.before_withdrawal.take() {
            interleave(env.broker);
        }
        match env.broker.withdraw_if_unaccepted(&open.request_id) {
            Withdrawal::Withdrawn | Withdrawal::Gone => {
                self.cancel(open, env, effects, why);
                true
            }
            Withdrawal::Won | Withdrawal::Resolved => {
                self.stage = Stage::Ringing(open);
                effects.extend(self.poll(env));
                false
            }
        }
    }

    /// The session is about to stop because the owner fence changed. A request
    /// nobody has won is withdrawn (its ring is moot: someone took the call
    /// another way); an accepted one stays, since its takeover is exactly what
    /// is changing the fence.
    pub fn withdraw_unaccepted(&mut self, env: &mut TransferEnv<'_>) -> Vec<Effect> {
        // Ask the broker before trusting what was last seen: an endpoint may
        // have won since the last turn, and that one's takeover must not be
        // cancelled from under it. Polling also reports its acceptance.
        let mut effects = self.poll(env);
        if let Stage::Ringing(_) = self.stage {
            if let Stage::Ringing(open) = std::mem::replace(&mut self.stage, Stage::Idle) {
                self.withdraw_unwon(open, env, &mut effects, "the caller changed hands another way");
            }
        }
        effects
    }

    /// OAIY withdraws a request (`formlogic.realtime.transfer_cancel`): its
    /// ring dialog was declined, the caller was offered a message instead, or it
    /// gave up waiting. Handled exactly as a request withdrawn because someone
    /// took the caller another way: an acceptance since the last turn is seen
    /// first and wins, a request nobody has won is withdrawn (its offers stop,
    /// `cancelled` is reported once, the audit closes), and one that an owner
    /// device has won is left alone, because its takeover is already under way:
    /// OAIY is told `too_late`. An id this call has no open request for (never
    /// seen, another call's, already ended, already cancelled) changes nothing
    /// and is answered `unknown_request`, so a replayed cancel does nothing
    /// twice. The withdrawal is one broker operation, so a phone that accepts
    /// at the same instant either wins (`too_late`) or finds it withdrawn:
    /// never both.
    pub fn cancel_requested(
        &mut self,
        env: &mut TransferEnv<'_>,
        request_id: &str,
        reason: CancelReason,
    ) -> Vec<Effect> {
        let notice = |kind: Notice| {
            Effect::Notice(NoticeFrame {
                request_id: request_id.to_string(),
                notice: kind,
                at_ms: unix_ms(),
            })
        };
        if self.request_id() != Some(request_id) {
            return vec![notice(Notice::UnknownRequest)];
        }
        // Ask the broker before trusting what was last seen: an endpoint may
        // have won, or the request may have ended, since the last turn.
        let mut effects = self.poll(env);
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Ringing(open) if open.request_id == request_id => {
                eprintln!(
                    "[aokie-plugin] transfer {request_id}: OAIY withdrew it ({})",
                    reason.as_str()
                );
                if self.withdraw_unwon(open, env, &mut effects, "OAIY withdrew it") {
                    // Answered by the `cancelled` outcome itself.
                    return effects;
                }
            }
            other => self.stage = other,
        }
        // Not withdrawn: an owner device won it (before the cancel, or at the
        // very moment of it), or it ended and the poll reported how.
        match &self.stage {
            Stage::Accepted(open) if open.request_id == request_id => {
                eprintln!(
                    "[aokie-plugin] transfer {request_id}: OAIY's withdrawal ({}) came after an owner device accepted",
                    reason.as_str()
                );
                effects.push(notice(Notice::TooLate));
            }
            _ => effects.push(notice(Notice::UnknownRequest)),
        }
        effects
    }

    /// The call is over (the radio's per-call reset is about to replace this
    /// call's context). Whatever is open is withdrawn and reported, so OAIY
    /// hears `cancelled` before the session stops and the audit trail closes
    /// its request. A plan still being awaited is simply dropped.
    pub fn end_call(&mut self, env: &mut TransferEnv<'_>) -> Vec<Effect> {
        self.withdraw_open(env, "the call ended")
    }

    /// The call is about to be put on hold behind another caller and its
    /// context stowed. Nobody polls a stowed context, so a request left open
    /// would keep the single mailbox (and the owner's phone ringing) for a call
    /// that no longer has the line, and could not be reported until the call
    /// came back. A request nobody has won is withdrawn now (atomically) and
    /// `cancelled` waits for the session the call gets when it resumes.
    ///
    /// A request an owner device HAS won is never withdrawn by this: its
    /// takeover is being connected (the phone has accepted; its media setup can
    /// take up to 45 s), and putting the caller on hold behind another call
    /// would fail it after the phone accepted, with OAIY having heard
    /// `accepted` and then `cancelled`. The broker is asked first, so an
    /// acceptance the machine had not seen is reported, and the answer is
    /// whether the call may be parked: `false` means the second caller is left
    /// to the network's call waiting, and the request carries on untouched.
    pub fn park(&mut self, env: &mut TransferEnv<'_>) -> (Vec<Effect>, bool) {
        let mut effects = self.poll(env);
        if let Stage::Ringing(_) = self.stage {
            if let Stage::Ringing(open) = std::mem::replace(&mut self.stage, Stage::Idle) {
                self.withdraw_unwon(open, env, &mut effects, "the call was put on hold");
            }
        }
        if let Stage::Planning(_) = self.stage {
            // The session that asked is about to be disposed: nobody would get the answer.
            self.stage = Stage::Idle;
        }
        let may_park = !matches!(self.stage, Stage::Accepted(_));
        (effects, may_park)
    }

    /// [`park`](Self::park) when the call has already left the line (the swap
    /// happened) and an accepted request cannot be left behind either: it is
    /// withdrawn whatever its state, failing closed. Reached only if an
    /// acceptance raced the gate that should have stopped the swap.
    pub fn park_forced(&mut self, env: &mut TransferEnv<'_>) -> Vec<Effect> {
        let mut effects = self.poll(env);
        effects.extend(self.withdraw_open(env, "the call was put on hold"));
        effects
    }

    fn withdraw_open(&mut self, env: &mut TransferEnv<'_>, why: &str) -> Vec<Effect> {
        let mut effects = Vec::new();
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Ringing(open) | Stage::Accepted(open) => {
                self.cancel(open, env, &mut effects, why);
            }
            Stage::Planning(_) | Stage::Idle => {}
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
        // The phrase floor is not skipped for `urgent` and `policy_rule`
        // unless the host's plan says the reason holds for this call: the
        // plugin cannot see an emergency or a business rule, and a model
        // steered by what a caller says must not be able to ring the owner by
        // naming a reason nobody checked.
        if planning.reason != Reason::CallerAsked && !plan.reason_allowed && !planning.caller_asked {
            eprintln!(
                "[aokie-plugin] transfer refused: the host did not confirm reason {} and the caller did not ask",
                planning.reason.as_str()
            );
            let code = match planning.reason {
                Reason::Urgent => "not_urgent",
                _ => "caller_did_not_ask",
            };
            answered(effects, &planning, refusal(RefusalStatus::Refused, code));
            return;
        }
        // Who may be offered the request: the devices the plan names, and
        // nobody else. A ring aimed at nobody could only time out, and would
        // tell the model somebody was being rung; a toast is not a target, so
        // it cannot open the request to whoever happens to be connected.
        let target_devices = match plan.target_rule() {
            Targets::Only(devices) => devices,
            Targets::Nobody => {
                answered(effects, &planning, refusal(RefusalStatus::Unavailable, "no_endpoint"));
                return;
            }
        };

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
        // decides, and a claim that won leaves nothing open. The fence is read
        // again here, not reused from the tool call: another AI action of this
        // call (an appointment request) may have advanced it while the host
        // thought, which changes nothing about who owns the caller. What must
        // still hold is the call and its epoch, and that Aokie owns it now.
        let owner = env
            .media
            .aokie_owner_fence()
            .filter(|owner| {
                owner.call_id == planning.call_id && owner.call_epoch == planning.owner.call_epoch
            });
        let Some(owner) = owner else {
            answered(effects, &planning, refusal(RefusalStatus::Unavailable, "call_changed"));
            return;
        };
        let opened = env.media.with_aokie_owner(&owner, || {
            env.broker.request_transfer_to(
                fence.clone(),
                "",
                None,
                plan.ring_seconds,
                Some(target_devices.clone()),
            )
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
        self.governor.note_opened(env.now, planning.caller_key);
        // The request is open: what the caller said up to the tool call is
        // spent, so the same ask cannot open a second one.
        self.spend_caller_turns(planning.heard);
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
            target_devices.len(),
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
            self.finish(open, resolution, env.now, effects);
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
            self.finish(open, AssistanceResolution::Expired, env.now, effects);
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
        self.note_end(End::Other, env.now);
    }

    fn finish(
        &mut self,
        mut open: Open,
        resolution: AssistanceResolution,
        now: Instant,
        effects: &mut Vec<Effect>,
    ) {
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
        self.note_end(end, now);
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
        let resume = self.peek_resume(now, broker)?;
        self.finish_resume();
        Some(resume)
    }

    /// [`take_resume`](Self::take_resume) without consuming the record, for a
    /// session start that may fail and be tried again.
    ///
    /// `return` is claimed only for a takeover that was seen to complete
    /// (`TransferTaken`, or no transfer of ours in the story at all: a person
    /// took the caller by hand and gave it back). A request that is still
    /// accepted-and-unresolved when the caller comes back is a setup that
    /// failed and has not been written down yet: the gateway returns the media
    /// first and records `TransferUnavailable` a moment later, so the radio can
    /// get here in between. Saying `return` then would tell OAIY the owner
    /// spoke to the caller.
    pub fn peek_resume(&self, now: Instant, broker: &AssistanceBroker) -> Option<ResumeInfo> {
        let started = self.handoff_started?;
        let via = match &self.stage {
            Stage::Ringing(open) | Stage::Accepted(open) => {
                match broker.peek_resolution(&open.request_id) {
                    Some(AssistanceResolution::TransferTaken { .. }) => Via::Return,
                    _ => Via::Failback,
                }
            }
            _ if self.last_end == Some(End::Unavailable) => Via::Failback,
            _ => Via::Return,
        };
        Some(ResumeInfo::new(
            now.saturating_duration_since(started).as_secs(),
            via,
        ))
    }

    // --- Outcomes with nobody to tell ---------------------------------------

    /// Keep an outcome that cannot go out yet: either no session can carry it
    /// (the handoff stopped the session for the AI, or the fresh one has not
    /// said `ready`), or the session it was made for has not yet been given
    /// the answer to the tool call that named the request (an outcome never
    /// overtakes that answer). It goes to the session that can carry it once it
    /// negotiates `transfer_v1`; the call's end drops it. `session_token`
    /// identifies the session it was made for (0 for none). Bounded, oldest
    /// out.
    pub fn hold_outcome(&mut self, frame: OutcomeFrame, session_token: u64) {
        if self.held.len() >= MAX_ATTEMPTS_PER_CALL as usize {
            self.held.remove(0);
        }
        self.held.push(Held { frame, session_token });
    }

    /// Whether an outcome is waiting.
    pub fn has_held_outcomes(&self) -> bool {
        !self.held.is_empty()
    }

    /// The held outcomes, oldest first, for the session `session_token`, which
    /// can carry them now. An `accepted` is news only to the session it was
    /// made for: a later session (after a handoff) would be told a takeover is
    /// starting that has already ended, so it is dropped instead.
    pub fn take_held_outcomes(&mut self, session_token: u64) -> Vec<OutcomeFrame> {
        std::mem::take(&mut self.held)
            .into_iter()
            .filter(|held| held.frame.outcome != Outcome::Accepted || held.session_token == session_token)
            .map(|held| held.frame)
            .collect()
    }

    /// The fresh session was started: the handoff is over, and how it ended
    /// belongs to it alone. Left behind, a failed setup would make the next
    /// handoff of this call (a person taking the caller by hand) read as a
    /// failback. A request still open now belongs to the handoff that just
    /// ended too: the gateway may record its failure after the session
    /// started, and that must not be remembered for the next one either.
    pub fn finish_resume(&mut self) {
        self.handoff_started = None;
        self.last_end = None;
        self.end_is_spent = matches!(self.stage, Stage::Ringing(_) | Stage::Accepted(_));
    }

    fn note_end(&mut self, end: End, now: Instant) {
        self.last_ended = Some(now);
        self.last_end = if std::mem::take(&mut self.end_is_spent) {
            None
        } else {
            Some(end)
        };
    }
}

#[cfg(test)]
#[path = "call_tests.rs"]
pub(crate) mod tests;
