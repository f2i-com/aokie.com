//! Call control via the WinRT Calls API — the native backend's answer to the
//! dongle's HFP AT machine (plan §5: "WinRT Calls API (call control, caller
//! ID)", plan §6 Phase 2: "Calls-API call-control engine → RuntimeEvent
//! parity"). It watches Bluetooth phone lines, tracks every `PhoneCall`, and
//! emits the same `BluetoothEvent` stream the plugin's voice pipeline already
//! consumes, so radio.rs runs unchanged on either transport.
//!
//! Threading model: `start()` runs on the crate's MTA worker thread (blocking
//! `.get()` is fine there). Watcher/`StatusChanged` handlers fire on arbitrary
//! COM threads, so they do NOTHING but push an `EngineCmd` into a channel —
//! never a COM call, never a lock. One poll thread drains the channel and owns
//! all state mutation + event emission behind a single `Mutex`, so event order
//! is deterministic. Control methods (`answer`/`dial`/...) clone the target
//! `PhoneCall`/`PhoneLine` under the lock, drop the lock, then perform the
//! (blocking) operation so a slow RPC can never stall the poll loop.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aokie_dongle::bluetooth::BluetoothEvent;
use windows::core::{IInspectable, HSTRING};
use windows::ApplicationModel::Calls::{
    PhoneCall, PhoneCallDirection, PhoneCallHistoryEntry, PhoneCallHistoryManager,
    PhoneCallHistoryStore, PhoneCallHistoryStoreAccessType, PhoneCallManager,
    PhoneCallOperationStatus, PhoneCallStatus, PhoneLine, PhoneLineTransport,
    PhoneLineTransportDevice, PhoneLineWatcher, PhoneLineWatcherEventArgs,
};
use windows::Devices::Bluetooth::{BluetoothAdapter, BluetoothDevice};
use windows::Foundation::TypedEventHandler;

use crate::pairing::format_address;
use crate::runtime::NativeShared;

/// Poll cadence for call enumeration (plan §6 calls for a watcher AND a ~1s
/// poll — `StatusChanged` is best-effort on some builds, the tick is the floor).
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// A call must be missing from `GetAllActivePhoneCalls` for this many
/// consecutive ticks before we declare it terminated — one transient
/// enumeration gap must never end a live call.
const ABSENT_TICKS_TERMINATE: u8 = 2;
/// Caller-id retry cadence for calls whose number is still unknown (the
/// history-store entry can land a beat after the ring, same as late +CLIP on
/// the dongle backend).
const CALLER_ID_RETRY: Duration = Duration::from_secs(5);
/// How far back a ringing/incoming history entry may be and still name the
/// current call (see the caller-id strategy on `resolve_number`).
const HISTORY_RECENT_SECS: i64 = 30;
/// A newly discovered Dialing call inside this window after our own `dial()`
/// is claimed as ours (the dongle's `pending_dial` pattern) — `DialedCall()`
/// occasionally comes back empty even when the dial succeeded.
const PENDING_DIAL_WINDOW: Duration = Duration::from_secs(15);

/// 100-ns ticks between 1601-01-01 and 1970-01-01 (WinRT DateTime epoch).
const SECS_UNIX_TO_WINRT: i64 = 11_644_473_600;
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Transport-simplified call status. `PhoneCallStatus` is a plain i32 enum;
/// mapping it once keeps the transition planner free of COM types so it stays
/// unit-testable without WinRT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimpleStatus {
    Incoming,
    Dialing,
    Talking,
    Held,
    Ended,
    Lost,
}

fn simple_status(status: PhoneCallStatus) -> SimpleStatus {
    match status {
        s if s == PhoneCallStatus::Incoming => SimpleStatus::Incoming,
        s if s == PhoneCallStatus::Dialing => SimpleStatus::Dialing,
        s if s == PhoneCallStatus::Talking => SimpleStatus::Talking,
        s if s == PhoneCallStatus::Held => SimpleStatus::Held,
        s if s == PhoneCallStatus::Ended => SimpleStatus::Ended,
        _ => SimpleStatus::Lost, // Lost(0) and any future value fail safe
    }
}

fn op_status_name(status: PhoneCallOperationStatus) -> &'static str {
    match status {
        s if s == PhoneCallOperationStatus::Succeeded => "succeeded",
        s if s == PhoneCallOperationStatus::TimedOut => "timed out",
        s if s == PhoneCallOperationStatus::ConnectionLost => "connection lost",
        s if s == PhoneCallOperationStatus::InvalidCallState => "invalid call state",
        _ => "operation failed",
    }
}

/// Per-call emission flags — the memory of which `BluetoothEvent`s already
/// went out, so transitions never double-fire. Every flag set means "the
/// matching event (or its deliberate suppression) is settled".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CallTrack {
    /// `CallIncoming` emitted — the call minted a normal inbound session.
    incoming: bool,
    /// Currently a waiting knock (second caller while another call talks).
    waiting: bool,
    /// `OutgoingDialing` emitted (by us at `dial()` time or by the planner).
    outbound: bool,
    /// `CallRinging` emitted for an outbound setup.
    ringing: bool,
    /// `CallAnswered` emitted — guards the once-per-call answer event.
    answered: bool,
    /// Termination settled (event emitted or deliberately none) — a call can
    /// report Ended/Lost repeatedly; the plugin must see exactly one end.
    ended: bool,
}

/// What the planner decided; the executor turns these into `BluetoothEvent`s
/// (and, for `ResolveCallerId`, a best-effort number lookup first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Planned {
    CallIncoming,
    ResolveCallerId,
    CallWaiting,
    OutgoingDialing,
    CallRinging,
    CallAnswered,
    CallTerminated,
    CallWaitingEnded,
}

/// THE event-mapping table (pure — unit-locked without WinRT). Mirrors the
/// dongle backend's HFP semantics so the plugin cannot tell the transports
/// apart:
///
/// | sighting / transition                     | events                                   |
/// |-------------------------------------------|------------------------------------------|
/// | first Incoming, no Talking call          | CallIncoming (+ resolve caller id)       |
/// | first Incoming while another call Talks  | CallWaiting { number }                   |
/// | waiting knock -> Talking                  | CallAnswered (never CallWaitingEnded)    |
/// | waiting knock -> Ended/Lost               | CallWaitingEnded (never CallTerminated)  |
/// | Incoming -> Talking                       | CallAnswered                             |
/// | announced call -> Ended/Lost              | CallTerminated (exactly once)            |
/// | first Dialing (handset-dialed)            | OutgoingDialing + CallRinging            |
/// | our dialed call seen Dialing              | CallRinging (dial() emitted the rest)    |
/// | first Talking (engine start mid-call)     | CallAnswered (recovery, dongle parity)   |
/// | Talking <-> Held                          | nothing here — held-set synth owns it    |
/// | never-announced call -> Ended/Lost        | nothing (no session was minted)          |
fn plan_transition(
    track: &mut CallTrack,
    new: SimpleStatus,
    another_talking: bool,
) -> Vec<Planned> {
    // Terminal first: settles exactly once no matter which path led here.
    if matches!(new, SimpleStatus::Ended | SimpleStatus::Lost) {
        if track.ended {
            return Vec::new();
        }
        track.ended = true;
        if track.waiting {
            track.waiting = false;
            return vec![Planned::CallWaitingEnded];
        }
        if track.incoming || track.answered || track.outbound {
            return vec![Planned::CallTerminated];
        }
        return Vec::new();
    }
    match new {
        SimpleStatus::Incoming => {
            if track.waiting || track.incoming {
                return Vec::new(); // duplicate sighting of the same ring
            }
            if another_talking {
                track.waiting = true;
                vec![Planned::CallWaiting]
            } else {
                track.incoming = true;
                vec![Planned::CallIncoming, Planned::ResolveCallerId]
            }
        }
        SimpleStatus::Dialing => {
            let mut out = Vec::new();
            if !track.outbound {
                track.outbound = true;
                out.push(Planned::OutgoingDialing);
            }
            if !track.ringing {
                track.ringing = true;
                out.push(Planned::CallRinging);
            }
            out
        }
        SimpleStatus::Talking => {
            if track.answered {
                return Vec::new(); // resume-from-hold re-talk is not a new answer
            }
            track.answered = true;
            track.waiting = false; // an answered knock leaves the waiting set
            vec![Planned::CallAnswered]
        }
        // Held sightings emit nothing directly; the HFP `callheld` indicator
        // is a property of the whole call SET, synthesized in `settle()`.
        SimpleStatus::Held => Vec::new(),
        SimpleStatus::Ended | SimpleStatus::Lost => Vec::new(), // handled above
    }
}

/// HFP `callheld` indicator: 0 = none held, 1 = held+active mix, 2 = all held.
fn synthesize_held_state(talking: usize, held: usize) -> i32 {
    if held == 0 {
        0
    } else if talking == 0 {
        2
    } else {
        1
    }
}

/// `+CLCC` status byte (dongle parity): 0 active / 1 held / 2 dialing /
/// 4 incoming / 5 waiting. Alerting (3) has no WinRT equivalent — Dialing
/// covers both setup phases.
fn clcc_status(status: SimpleStatus, another_active: bool) -> u8 {
    match status {
        SimpleStatus::Talking => 0,
        SimpleStatus::Held => 1,
        SimpleStatus::Dialing => 2,
        SimpleStatus::Incoming => {
            if another_active {
                5
            } else {
                4
            }
        }
        // Ended/Lost calls are filtered before emission; 4 is unreachable.
        SimpleStatus::Ended | SimpleStatus::Lost => 4,
    }
}

fn unix_secs_to_winrt_ticks(secs: i64) -> i64 {
    (secs + SECS_UNIX_TO_WINRT) * TICKS_PER_SECOND
}

fn winrt_ticks_now() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unix_secs_to_winrt_ticks(secs)
}

/// `t` within the last `secs` seconds (1s future-skew tolerated for clock
/// disagreement between the phone-side history writer and us).
fn ticks_within(t: i64, now: i64, secs: i64) -> bool {
    t <= now + TICKS_PER_SECOND && now - t <= secs * TICKS_PER_SECOND
}

fn hr(context: &str, e: &windows::core::Error) -> String {
    format!("{context}: 0x{:08X} {}", e.code().0, e.message())
}

fn guid_key(id: &windows::core::GUID) -> String {
    format!("{id:?}")
}

/// Lightweight commands from COM-event threads to the owning poll thread.
/// Handlers push these and return immediately — the only safe pattern when
/// the handler can fire on an arbitrary RPC thread.
enum EngineCmd {
    LineAdded(windows::core::GUID),
    LineRemoved(windows::core::GUID),
    LineUpdated(windows::core::GUID),
    WatcherStopped,
    CallChanged(String),
    Shutdown,
}

struct CallState {
    key: String,
    line_key: String,
    call: PhoneCall,
    /// Raw `PhoneCall::CallId` (the history-store entry id), un-namespaced.
    raw_call_id: String,
    track: CallTrack,
    status: SimpleStatus,
    /// true = outgoing (we dialed, or the call was first seen Dialing, or the
    /// call info says Outgoing) — the `+CLCC` direction byte.
    outgoing: bool,
    /// Resolved caller id / dialed number; None until resolved.
    number: Option<String>,
    status_token: Option<i64>,
    absent_ticks: u8,
    last_id_attempt: Option<Instant>,
}

struct EngineState {
    event_tx: Sender<BluetoothEvent>,
    shared: Arc<NativeShared>,
    cmd_tx: Sender<EngineCmd>,
    watcher: Option<PhoneLineWatcher>,
    /// Bluetooth-transport lines only — VoIP/cellular lines (Teams, eSIM)
    /// must never mark the phone bridge connected.
    lines: HashMap<String, PhoneLine>,
    calls: HashMap<String, CallState>,
    connected_addr: Option<String>,
    initialized_sent: bool,
    last_held_state: i32,
    history: Option<PhoneCallHistoryStore>,
    history_failed: bool,
    register_attempted: HashSet<String>,
    pending_dial: Option<(String, Instant)>,
}

impl EngineState {
    fn emit(&self, event: BluetoothEvent) {
        // The host going away mid-shutdown is normal; never panic on send.
        let _ = self.event_tx.send(event);
    }
}

/// Call control engine — see the module header for the threading model.
pub(crate) struct CallsEngine {
    state: Arc<Mutex<EngineState>>,
    cmd_tx: Sender<EngineCmd>,
    poll: Option<std::thread::JoinHandle<()>>,
    tokens: WatcherTokens,
    stopping: Arc<AtomicBool>,
}

struct WatcherTokens {
    line_added: i64,
    line_removed: i64,
    line_updated: i64,
    enumeration_completed: i64,
    stopped: i64,
}

impl CallsEngine {
    /// Start watching phone lines. Returns fast; discovery is async (line
    /// arrivals surface as DeviceConnected/Initialized events, exactly like
    /// the dongle runtime's bring-up).
    pub(crate) fn start(
        event_tx: Sender<BluetoothEvent>,
        shared: Arc<NativeShared>,
    ) -> Result<Self, String> {
        // RequestStoreAsync is the restricted-capability gate the phase-0
        // probe (plan §6) proved works from this unpackaged process.
        let store = PhoneCallManager::RequestStoreAsync()
            .map_err(|e| hr("native calls: PhoneCallManager::RequestStoreAsync", &e))?
            .get()
            .map_err(|e| hr("native calls: acquiring the phone call store", &e))?;
        let watcher = store
            .RequestLineWatcher()
            .map_err(|e| hr("native calls: RequestLineWatcher refused", &e))?;

        let (cmd_tx, cmd_rx) = channel::<EngineCmd>();

        // Handlers only re-package the event into EngineCmd. `args` is null
        // in theory (IInspectable sender-less raises); guard, never unwrap.
        // NOTE: closure params must be annotated explicitly — the projection
        // cannot infer the handler's TSender/TResult through `Param`.
        let tokens = WatcherTokens {
            line_added: watcher
                .LineAdded(&TypedEventHandler::new({
                    let tx = cmd_tx.clone();
                    move |_: windows::core::Ref<'_, PhoneLineWatcher>,
                          args: windows::core::Ref<'_, PhoneLineWatcherEventArgs>| {
                        if let Some(id) = args.as_ref().and_then(|a| a.LineId().ok()) {
                            let _ = tx.send(EngineCmd::LineAdded(id));
                        }
                        Ok(())
                    }
                }))
                .map_err(|e| hr("native calls: LineAdded handler", &e))?,
            line_removed: watcher
                .LineRemoved(&TypedEventHandler::new({
                    let tx = cmd_tx.clone();
                    move |_: windows::core::Ref<'_, PhoneLineWatcher>,
                          args: windows::core::Ref<'_, PhoneLineWatcherEventArgs>| {
                        if let Some(id) = args.as_ref().and_then(|a| a.LineId().ok()) {
                            let _ = tx.send(EngineCmd::LineRemoved(id));
                        }
                        Ok(())
                    }
                }))
                .map_err(|e| hr("native calls: LineRemoved handler", &e))?,
            line_updated: watcher
                .LineUpdated(&TypedEventHandler::new({
                    let tx = cmd_tx.clone();
                    move |_: windows::core::Ref<'_, PhoneLineWatcher>,
                          args: windows::core::Ref<'_, PhoneLineWatcherEventArgs>| {
                        if let Some(id) = args.as_ref().and_then(|a| a.LineId().ok()) {
                            let _ = tx.send(EngineCmd::LineUpdated(id));
                        }
                        Ok(())
                    }
                }))
                .map_err(|e| hr("native calls: LineUpdated handler", &e))?,
            enumeration_completed: watcher
                .EnumerationCompleted(&TypedEventHandler::new(
                    move |_: windows::core::Ref<'_, PhoneLineWatcher>,
                          _args: windows::core::Ref<'_, IInspectable>| {
                        Ok(())
                    },
                ))
                .map_err(|e| hr("native calls: EnumerationCompleted handler", &e))?,
            stopped: watcher
                .Stopped(&TypedEventHandler::new({
                    let tx = cmd_tx.clone();
                    move |_: windows::core::Ref<'_, PhoneLineWatcher>,
                          _args: windows::core::Ref<'_, IInspectable>| {
                        let _ = tx.send(EngineCmd::WatcherStopped);
                        Ok(())
                    }
                }))
                .map_err(|e| hr("native calls: Stopped handler", &e))?,
        };

        // Start BEFORE spawning the poll thread: a Start refusal is a clean
        // start() error with no threads to unwind.
        watcher
            .Start()
            .map_err(|e| hr("native calls: watcher Start", &e))?;

        let state = Arc::new(Mutex::new(EngineState {
            event_tx,
            shared,
            cmd_tx: cmd_tx.clone(),
            watcher: Some(watcher),
            lines: HashMap::new(),
            calls: HashMap::new(),
            connected_addr: None,
            initialized_sent: false,
            last_held_state: 0,
            history: None,
            history_failed: false,
            register_attempted: HashSet::new(),
            pending_dial: None,
        }));
        let stopping = Arc::new(AtomicBool::new(false));
        let poll = {
            let state = state.clone();
            let stopping = stopping.clone();
            std::thread::Builder::new()
                .name("aokie-winbt-calls".to_string())
                .spawn(move || poll_loop(state, cmd_rx, stopping))
                .map_err(|e| format!("native calls: spawn poll thread: {e}"))?
        };

        Ok(Self {
            state,
            cmd_tx,
            poll: Some(poll),
            tokens,
            stopping,
        })
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, EngineState>, String> {
        self.state
            .lock()
            .map_err(|_| "native calls: engine state lock poisoned".to_string())
    }

    /// Accept the ringing call. Errors honestly when nothing is Incoming.
    pub(crate) fn answer(&self) -> Result<(), String> {
        let call = {
            let st = self.lock_state()?;
            let mut incoming: Vec<&CallState> = st
                .calls
                .values()
                .filter(|c| !c.track.ended && c.status == SimpleStatus::Incoming)
                .collect();
            incoming.sort_by(|a, b| a.key.cmp(&b.key));
            incoming.first().map(|c| c.call.clone())
        };
        let Some(call) = call else {
            return Err("native calls: no incoming call to answer".to_string());
        };
        let status = call
            .AcceptIncoming()
            .map_err(|e| hr("native calls: AcceptIncoming", &e))?;
        if status == PhoneCallOperationStatus::Succeeded {
            Ok(())
        } else {
            Err(format!(
                "native calls: AcceptIncoming refused ({})",
                op_status_name(status)
            ))
        }
    }

    /// Reject the Incoming call if one is ringing; otherwise end the Talking
    /// call, then a Dialing (abort our own ring), then any Held call.
    pub(crate) fn reject_or_hangup(&self) -> Result<(), String> {
        enum Target {
            Reject(PhoneCall),
            End(PhoneCall),
        }
        let target = {
            let st = self.lock_state()?;
            let mut live: Vec<&CallState> = st.calls.values().filter(|c| !c.track.ended).collect();
            live.sort_by(|a, b| a.key.cmp(&b.key));
            let pick =
                |s: SimpleStatus| live.iter().find(|c| c.status == s).map(|c| c.call.clone());
            if let Some(call) = pick(SimpleStatus::Incoming) {
                Some(Target::Reject(call))
            } else if let Some(call) = pick(SimpleStatus::Talking) {
                Some(Target::End(call))
            } else if let Some(call) = pick(SimpleStatus::Dialing) {
                Some(Target::End(call))
            } else {
                pick(SimpleStatus::Held).map(Target::End)
            }
        };
        let Some(target) = target else {
            return Err("native calls: no call to reject or hang up".to_string());
        };
        let (result, verb) = match target {
            Target::Reject(call) => (call.RejectIncoming(), "RejectIncoming"),
            Target::End(call) => (call.End(), "End"),
        };
        let status = result.map_err(|e| hr(&format!("native calls: {verb}"), &e))?;
        if status == PhoneCallOperationStatus::Succeeded {
            Ok(())
        } else {
            Err(format!(
                "native calls: {verb} refused ({})",
                op_status_name(status)
            ))
        }
    }

    /// Dial out on the connected line. Emits `OutgoingDialing` the instant
    /// the line accepts the dial — the plugin's MO sequence expects
    /// OutgoingDialing -> (CallRinging) -> CallAnswered, same as the dongle's.
    pub(crate) fn dial(&self, number: &str) -> Result<(), String> {
        let number = number.trim();
        if number.is_empty() {
            return Err("native calls: empty number".to_string());
        }
        let (line_key, line) = {
            let st = self.lock_state()?;
            // Prefer a line that advertises dialing; fall back to the only
            // line (CanDial can read false transiently mid-call).
            let preferred = st
                .lines
                .iter()
                .find(|(_, l)| l.CanDial().unwrap_or(false))
                .or_else(|| st.lines.iter().next());
            match preferred {
                Some((k, l)) => (k.clone(), l.clone()),
                None => return Err("native calls: no phone line (phone not connected)".to_string()),
            }
        };
        let number_h = HSTRING::from(number);
        let result = line
            .DialWithResult(&number_h, &number_h)
            .map_err(|e| hr("native calls: DialWithResult", &e))?;
        let status = result
            .DialCallStatus()
            .map_err(|e| hr("native calls: reading the dial result", &e))?;
        if status != PhoneCallOperationStatus::Succeeded {
            return Err(format!(
                "native calls: dial refused ({})",
                op_status_name(status)
            ));
        }
        let mut st = self.lock_state()?;
        // The window covers the gap until the call object shows up in the
        // poll enumeration even when DialedCall() comes back empty.
        st.pending_dial = Some((number.to_string(), Instant::now()));
        let mut announce = true;
        if let Ok(call) = result.DialedCall() {
            announce = !upsert_dialed(&mut st, &line_key, &call, number);
        }
        if announce {
            st.emit(BluetoothEvent::OutgoingDialing);
        }
        Ok(())
    }

    /// Two-call swap (CHLD=2 analogue): exactly one Talking + exactly one
    /// Held or Incoming — hold the talking leg, then resume/accept the other.
    /// Any other shape gets an honest error rather than a blind toggle (the
    /// plan's call-waiting risk note: WinRT hold semantics are unproven, so
    /// never guess).
    pub(crate) fn hold_swap(&self) -> Result<(), String> {
        enum Other {
            Resume(PhoneCall),
            Accept(PhoneCall),
        }
        let (talking, other) = {
            let st = self.lock_state()?;
            let live: Vec<&CallState> = st.calls.values().filter(|c| !c.track.ended).collect();
            let talking: Vec<&CallState> = live
                .iter()
                .filter(|c| c.status == SimpleStatus::Talking)
                .copied()
                .collect();
            let held: Vec<&CallState> = live
                .iter()
                .filter(|c| c.status == SimpleStatus::Held)
                .copied()
                .collect();
            let incoming: Vec<&CallState> = live
                .iter()
                .filter(|c| c.status == SimpleStatus::Incoming)
                .copied()
                .collect();
            let shape = || {
                format!(
                    "{} talking / {} held / {} incoming",
                    talking.len(),
                    held.len(),
                    incoming.len()
                )
            };
            if talking.len() != 1 {
                return Err(format!(
                    "native calls: hold swap needs exactly one talking call ({})",
                    shape()
                ));
            }
            let other = match (held.len(), incoming.len()) {
                (1, 0) => Other::Resume(held[0].call.clone()),
                (0, 1) => Other::Accept(incoming[0].call.clone()),
                _ => {
                    return Err(format!(
                        "native calls: hold swap needs exactly one held or incoming call ({})",
                        shape()
                    ))
                }
            };
            (talking[0].call.clone(), other)
        };
        // Order matters: park the current talker first so accepting the
        // other call can never drop it (the dongle's CHLD=2 landmine —
        // answering a knock without holding first can tear down the active
        // call on some phones).
        let held_status = talking.Hold().map_err(|e| hr("native calls: Hold", &e))?;
        if held_status != PhoneCallOperationStatus::Succeeded {
            return Err(format!(
                "native calls: Hold refused ({}); swap aborted, the talking call is untouched",
                op_status_name(held_status)
            ));
        }
        let (result, verb) = match &other {
            Other::Resume(call) => (call.ResumeFromHold(), "ResumeFromHold"),
            Other::Accept(call) => (call.AcceptIncoming(), "AcceptIncoming"),
        };
        let status = result.map_err(|e| hr(&format!("native calls: {verb}"), &e))?;
        if status == PhoneCallOperationStatus::Succeeded {
            Ok(())
        } else {
            Err(format!(
                "native calls: {verb} refused ({}) — the first call is now held, resume it manually",
                op_status_name(status)
            ))
        }
    }

    /// Synthesized AT+CLCC: one `CallListEntry` per live call, built from a
    /// fresh enumeration so the answer is never older than this call.
    pub(crate) fn query_calls(&self) -> Result<(), String> {
        let mut st = self.lock_state()?;
        let line_keys: Vec<String> = st.lines.keys().cloned().collect();
        if line_keys.is_empty() {
            return Err("native calls: no phone line (phone not connected)".to_string());
        }
        for key in &line_keys {
            refresh_line_calls(&mut st, key);
        }
        settle(&mut st);
        // Best-effort numbers for the listing (dongle CLCC carries them).
        let unresolved: Vec<String> = st
            .calls
            .iter()
            .filter(|(_, c)| !c.track.ended && c.number.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        for key in unresolved {
            if let Some(num) = resolve_number(&mut st, &key) {
                if let Some(c) = st.calls.get_mut(&key) {
                    c.number = Some(num);
                }
            }
        }
        let any_talking = st
            .calls
            .values()
            .any(|c| !c.track.ended && c.status == SimpleStatus::Talking);
        let mut live: Vec<&CallState> = st
            .calls
            .values()
            .filter(|c| {
                !c.track.ended && !matches!(c.status, SimpleStatus::Ended | SimpleStatus::Lost)
            })
            .collect();
        live.sort_by(|a, b| a.key.cmp(&b.key));
        for (i, c) in live.iter().enumerate() {
            st.emit(BluetoothEvent::CallListEntry {
                index: (i + 1) as u8,
                direction: if c.outgoing { 0 } else { 1 },
                status: clcc_status(c.status, any_talking),
                // WinRT exposes no multiparty flag; always false (honest
                // degradation vs inventing one).
                multiparty: false,
                number: c.number.clone(),
            });
        }
        Ok(())
    }
}

impl Drop for CallsEngine {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(EngineCmd::Shutdown);
        if let Some(poll) = self.poll.take() {
            let _ = poll.join();
        }
        if let Ok(mut st) = self.state.lock() {
            // Detach every handler so a late COM callback can never fire into
            // a half-dropped engine; the channel is already dead anyway.
            let calls: Vec<CallState> = st.calls.drain().map(|(_, c)| c).collect();
            for c in calls {
                if let Some(token) = c.status_token {
                    let _ = c.call.RemoveStatusChanged(token);
                }
            }
            if let Some(watcher) = st.watcher.take() {
                let _ = watcher.Stop();
                let _ = watcher.RemoveLineAdded(self.tokens.line_added);
                let _ = watcher.RemoveLineRemoved(self.tokens.line_removed);
                let _ = watcher.RemoveLineUpdated(self.tokens.line_updated);
                let _ = watcher.RemoveEnumerationCompleted(self.tokens.enumeration_completed);
                let _ = watcher.RemoveStopped(self.tokens.stopped);
            }
            st.shared.connected.store(false, Ordering::Release);
            st.shared.call_active.store(false, Ordering::Release);
        }
    }
}

/// The owning loop: drain handler commands, then tick every line. One pass
/// per wake means line events are acted on within milliseconds while the 1s
/// timeout keeps a floor under status drift (plan §6's watcher+poll design).
fn poll_loop(
    state: Arc<Mutex<EngineState>>,
    cmd_rx: std::sync::mpsc::Receiver<EngineCmd>,
    stopping: Arc<AtomicBool>,
) {
    init_mta();
    let mut shutdown = false;
    while !shutdown {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(EngineCmd::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                shutdown = true
            }
            Ok(cmd) => {
                let mut cmds = vec![cmd];
                while let Ok(extra) = cmd_rx.try_recv() {
                    cmds.push(extra);
                }
                if let Ok(mut st) = state.lock() {
                    for cmd in cmds {
                        match cmd {
                            EngineCmd::Shutdown => {
                                shutdown = true;
                                break;
                            }
                            EngineCmd::LineAdded(id) => on_line_added(&mut st, &id),
                            EngineCmd::LineRemoved(id) => on_line_removed(&mut st, &id),
                            EngineCmd::LineUpdated(id) => {
                                refresh_line_calls(&mut st, &guid_key(&id))
                            }
                            EngineCmd::CallChanged(key) => {
                                let line_key = st.calls.get(&key).map(|c| c.line_key.clone());
                                if let Some(line_key) = line_key {
                                    refresh_line_calls(&mut st, &line_key);
                                }
                            }
                            EngineCmd::WatcherStopped => on_watcher_stopped(&mut st, &stopping),
                        }
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if shutdown {
            break;
        }
        if let Ok(mut st) = state.lock() {
            let line_keys: Vec<String> = st.lines.keys().cloned().collect();
            for key in &line_keys {
                refresh_line_calls(&mut st, key);
            }
            retry_caller_ids(&mut st);
            settle(&mut st);
            purge_ended(&mut st);
        }
    }
}

/// Defensive MTA init for helper threads (the crate's worker thread does its
/// own RoInitialize; S_FALSE / RPC_E_CHANGED_MODE mean we're already fine).
/// The Win32 projection is unsafe-by-signature — the call itself is benign.
fn init_mta() {
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_TYPE};
    unsafe {
        match RoInitialize(RO_INIT_TYPE(1)) {
            Ok(()) => {}
            Err(e) if e.code().0 == 1 => {} // S_FALSE: already MTA
            Err(e) if e.code().0 == -2147417850i32 => {} // RPC_E_CHANGED_MODE
            Err(_) => {}
        }
    }
}

/// A Bluetooth line appeared: resolve the phone's identity, mark connected,
/// register the app on the transport device (best-effort), then enumerate
/// calls. Only Bluetooth-transport lines count as "the phone connected" —
/// VoIP app lines would otherwise fire DeviceConnected for Teams et al.
fn on_line_added(st: &mut EngineState, id: &windows::core::GUID) {
    let key = guid_key(id);
    if st.lines.contains_key(&key) {
        refresh_line_calls(st, &key);
        return;
    }
    let line = match PhoneLine::FromIdAsync(*id).and_then(|op| op.get()) {
        Ok(line) => line,
        Err(e) => {
            st.emit(BluetoothEvent::Error(hr(
                "native calls: resolving a new phone line",
                &e,
            )));
            return;
        }
    };
    match line.Transport() {
        Ok(t) if t == PhoneLineTransport::Bluetooth => {}
        Ok(_) => return, // VoIP/cellular line: not the phone bridge
        Err(e) => {
            st.emit(BluetoothEvent::Error(hr(
                "native calls: reading a line's transport",
                &e,
            )));
            return;
        }
    }
    let display_name = line
        .DisplayName()
        .map(|h| h.to_string_lossy())
        .unwrap_or_default();
    // TransportDeviceId feeds BluetoothDevice resolution + RegisterApp. It
    // can fail on older builds (IPhoneLine2); the line still works for call
    // control, so degrade rather than drop the line.
    let transport_device_id = line
        .TransportDeviceId()
        .map(|h| h.to_string_lossy())
        .unwrap_or_default();
    st.lines.insert(key.clone(), line.clone());

    if !st.shared.connected.load(Ordering::Acquire) {
        // Identity: the transport device id IS a BluetoothDevice id.
        let mut addr_str = String::new();
        let mut name = display_name;
        if !transport_device_id.is_empty() {
            match BluetoothDevice::FromIdAsync(&HSTRING::from(transport_device_id.as_str()))
                .and_then(|op| op.get())
            {
                Ok(dev) => {
                    let addr = dev.BluetoothAddress().unwrap_or(0);
                    if addr != 0 {
                        addr_str = format_address(addr);
                        if let Ok(mut a) = st.shared.phone_address.write() {
                            *a = addr;
                        }
                    }
                    if let Ok(n) = dev.Name() {
                        let n = n.to_string_lossy();
                        if !n.is_empty() {
                            name = n;
                        }
                    }
                }
                Err(e) => st.emit(BluetoothEvent::Error(hr(
                    "native calls: resolving the line's Bluetooth device",
                    &e,
                ))),
            }
        }
        // The adapter address only needs to go out once per engine run
        // (Initialized is the plugin's "radio is up" marker).
        if !st.initialized_sent {
            if let Ok(adapter) = BluetoothAdapter::GetDefaultAsync().and_then(|op| op.get()) {
                let local = adapter.BluetoothAddress().unwrap_or(0);
                if local != 0 {
                    let local = format_address(local);
                    if let Ok(mut a) = st.shared.local_address.write() {
                        *a = local.clone();
                    }
                    st.initialized_sent = true;
                    st.emit(BluetoothEvent::Initialized(local));
                }
            } // else: retried on the next line arrival
        }
        if let Ok(mut a) = st.shared.remote_address.write() {
            *a = addr_str.clone();
        }
        if let Ok(mut n) = st.shared.remote_name.write() {
            *n = Some(name);
        }
        st.shared.connected.store(true, Ordering::Release);
        st.connected_addr = Some(addr_str.clone());
        st.emit(BluetoothEvent::DeviceConnected(addr_str));
    }

    // RegisterApp mainly matters for Dial visibility (an unregistered app
    // can be denied line dial results). Failure must not kill the watcher —
    // lines may still be delivered (plan §6 spike note).
    if !transport_device_id.is_empty() && st.register_attempted.insert(transport_device_id.clone())
    {
        match PhoneLineTransportDevice::FromId(&HSTRING::from(transport_device_id.as_str())) {
            Ok(dev) => {
                let registered = dev.IsRegistered().unwrap_or(false);
                if !registered {
                    if let Err(e) = dev.RegisterApp() {
                        st.emit(BluetoothEvent::Error(format!(
                            "native calls: RegisterApp failed ({e}); continuing unregistered — outbound dialing may be limited"
                        )));
                    }
                }
            }
            Err(e) => st.emit(BluetoothEvent::Error(hr(
                "native calls: opening the line transport device",
                &e,
            ))),
        }
    }

    refresh_line_calls(st, &key);
}

/// The line went away: every call on it ends honestly, and when no Bluetooth
/// line remains the bridge reports disconnected (dongle parity).
fn on_line_removed(st: &mut EngineState, id: &windows::core::GUID) {
    let key = guid_key(id);
    if st.lines.remove(&key).is_none() {
        return;
    }
    let orphaned: Vec<String> = st
        .calls
        .iter()
        .filter(|(_, c)| c.line_key == key && !c.track.ended)
        .map(|(k, _)| k.clone())
        .collect();
    for call_key in orphaned {
        apply_status(st, &call_key, SimpleStatus::Lost);
    }
    if st.lines.is_empty() && st.shared.connected.swap(false, Ordering::AcqRel) {
        st.shared.call_active.store(false, Ordering::Release);
        let addr = st.connected_addr.take().unwrap_or_default();
        if let Ok(mut a) = st.shared.remote_address.write() {
            *a = String::new();
        }
        if let Ok(mut n) = st.shared.remote_name.write() {
            *n = None;
        }
        if let Ok(mut a) = st.shared.phone_address.write() {
            *a = 0;
        }
        st.emit(BluetoothEvent::DeviceDisconnected(addr));
    }
}

/// The watcher can stop on its own (service restart, access revocation). A
/// successful silent restart is self-healing (no Error event — the dongle
/// backend learned the hard way that recovered hiccups must not page the
/// operator); only a FAILED restart is reported.
fn on_watcher_stopped(st: &mut EngineState, stopping: &AtomicBool) {
    if stopping.load(Ordering::Acquire) {
        return;
    }
    if let Some(watcher) = &st.watcher {
        if let Err(e) = watcher.Start() {
            st.emit(BluetoothEvent::Error(hr(
                "native calls: the phone line watcher stopped and could not be restarted",
                &e,
            )));
        }
    }
}

/// Reconcile one line's tracked calls against `GetAllActivePhoneCalls`:
/// attach to new calls, re-apply statuses, and end calls that vanish for
/// good. This is the single enumeration path used by the poll tick, line
/// events, StatusChanged wake-ups and `query_calls`.
fn refresh_line_calls(st: &mut EngineState, line_key: &str) {
    let Some(line) = st.lines.get(line_key).cloned() else {
        return;
    };
    let calls = line
        .GetAllActivePhoneCalls()
        .and_then(|r| r.AllActivePhoneCalls());
    let Ok(calls) = calls else {
        return; // transient RPC miss: keep last-known state, next tick retries
    };
    let mut present: Vec<(String, PhoneCall, SimpleStatus)> = Vec::new();
    for call in calls {
        let key = call_key(line_key, &call);
        let status = match call.Status() {
            Ok(s) => simple_status(s),
            Err(_) => match st.calls.get(&key) {
                Some(c) => c.status, // keep last-known; do not flap
                None => continue,    // unreadable brand-new call: next tick
            },
        };
        present.push((key, call, status));
    }
    let present_keys: HashSet<String> = present.iter().map(|(k, _, _)| k.clone()).collect();

    for (key, call, status) in present {
        if !st.calls.contains_key(&key) {
            let mut track = CallTrack::default();
            let mut outgoing = initial_direction(&call, status);
            let mut number = None;
            // Claim a fresh Dialing call as ours when a dial just went out
            // (covers DialedCall() returning nothing usable).
            if status == SimpleStatus::Dialing {
                if let Some((num, at)) = st.pending_dial.clone() {
                    if at.elapsed() <= PENDING_DIAL_WINDOW {
                        track.outbound = true;
                        outgoing = true;
                        number = Some(num);
                        st.pending_dial = None;
                    }
                }
            }
            let status_token = attach_status_handler(st, &key, &call);
            let raw_call_id = call
                .CallId()
                .map(|h| h.to_string_lossy())
                .unwrap_or_default();
            st.calls.insert(
                key.clone(),
                CallState {
                    key: key.clone(),
                    line_key: line_key.to_string(),
                    call,
                    raw_call_id,
                    track,
                    status,
                    outgoing,
                    number,
                    status_token,
                    absent_ticks: 0,
                    last_id_attempt: None,
                },
            );
        } else if let Some(c) = st.calls.get_mut(&key) {
            c.absent_ticks = 0;
        }
        apply_status(st, &key, status);
    }

    // Vanished from the enumeration: debounce before declaring the end —
    // GetAllActivePhoneCalls drops ended legs immediately, but one shaky
    // enumeration must never terminate a live session.
    let vanished: Vec<String> = st
        .calls
        .iter()
        .filter(|(k, c)| {
            c.line_key == line_key && !present_keys.contains(k.as_str()) && !c.track.ended
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in vanished {
        let due = match st.calls.get_mut(&key) {
            Some(c) => {
                c.absent_ticks += 1;
                c.absent_ticks >= ABSENT_TICKS_TERMINATE
            }
            None => false,
        };
        if due {
            apply_status(st, &key, SimpleStatus::Ended);
        }
    }
}

/// The pure planner + the event emission + caller-id side effects for one
/// observed status. `another_talking` is computed across the whole call set
/// (any line) because HFP's "waiting" concept is per phone, not per line.
fn apply_status(st: &mut EngineState, key: &str, new_status: SimpleStatus) {
    let another_talking = st
        .calls
        .iter()
        .any(|(k, c)| k.as_str() != key && !c.track.ended && c.status == SimpleStatus::Talking);
    let planned = match st.calls.get_mut(key) {
        Some(c) => {
            let planned = plan_transition(&mut c.track, new_status, another_talking);
            c.status = new_status;
            planned
        }
        None => return,
    };
    for p in planned {
        match p {
            Planned::CallIncoming => st.emit(BluetoothEvent::CallIncoming),
            Planned::ResolveCallerId => {
                // Dongle order: CallIncoming first, CallerId when known.
                if let Some(num) = resolve_number(st, key) {
                    if let Some(c) = st.calls.get_mut(key) {
                        c.number = Some(num.clone());
                    }
                    st.emit(BluetoothEvent::CallerId(num));
                }
            }
            Planned::CallWaiting => {
                // The number rides inside the event (no separate CallerId for
                // a knock — dongle parity).
                let mut number = st.calls.get(key).and_then(|c| c.number.clone());
                if number.is_none() {
                    number = resolve_number(st, key);
                    if number.is_some() {
                        if let Some(c) = st.calls.get_mut(key) {
                            c.number = number.clone();
                        }
                    }
                }
                st.emit(BluetoothEvent::CallWaiting { number });
            }
            Planned::OutgoingDialing => st.emit(BluetoothEvent::OutgoingDialing),
            Planned::CallRinging => st.emit(BluetoothEvent::CallRinging),
            Planned::CallAnswered => st.emit(BluetoothEvent::CallAnswered),
            Planned::CallTerminated => st.emit(BluetoothEvent::CallTerminated),
            Planned::CallWaitingEnded => st.emit(BluetoothEvent::CallWaitingEnded),
        }
    }
}

/// Held-set synthesis + the call_active flag, recomputed after every refresh
/// pass. `CallHeld` fires only on CHANGE (it is an indicator, not a poll
/// result); `call_active` follows "any Talking call", which is what drives
/// the WASAPI pump in the audio module.
fn settle(st: &mut EngineState) {
    let talking = st
        .calls
        .values()
        .filter(|c| !c.track.ended && c.status == SimpleStatus::Talking)
        .count();
    let held = st
        .calls
        .values()
        .filter(|c| !c.track.ended && c.status == SimpleStatus::Held)
        .count();
    let new_state = synthesize_held_state(talking, held);
    if new_state != st.last_held_state {
        st.last_held_state = new_state;
        st.emit(BluetoothEvent::CallHeld { state: new_state });
    }
    st.shared.call_active.store(talking > 0, Ordering::Release);
}

/// Drop settled calls from the map (their termination already went out).
fn purge_ended(st: &mut EngineState) {
    let keys: Vec<String> = st
        .calls
        .iter()
        .filter(|(_, c)| {
            c.track.ended && matches!(c.status, SimpleStatus::Ended | SimpleStatus::Lost)
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in keys {
        if let Some(c) = st.calls.remove(&key) {
            if let Some(token) = c.status_token {
                let _ = c.call.RemoveStatusChanged(token);
            }
        }
    }
}

/// Second-chance caller-id pass. The history entry for a ringing call can
/// land after the first lookup (the native late-+CLIP), so unresolved
/// inbound/waiting calls retry on a slow cadence. A late number for a
/// waiting knock goes out as another CallWaiting (the dongle's documented
/// "late-number upgrade"); a late number for a normal ring is a CallerId.
fn retry_caller_ids(st: &mut EngineState) {
    let now = Instant::now();
    let keys: Vec<String> = st
        .calls
        .iter()
        .filter(|(_, c)| {
            !c.track.ended
                && c.number.is_none()
                && (c.track.waiting || c.track.incoming || c.track.outbound)
                && c.last_id_attempt
                    .map(|t| now.duration_since(t) >= CALLER_ID_RETRY)
                    .unwrap_or(true)
        })
        .map(|(k, _)| k.clone())
        .collect();
    for key in keys {
        if let Some(c) = st.calls.get_mut(&key) {
            c.last_id_attempt = Some(now);
        }
        if let Some(num) = resolve_number(st, &key) {
            let (waiting, incoming) = match st.calls.get(&key) {
                Some(c) => (c.track.waiting, c.track.incoming),
                None => (false, false),
            };
            if let Some(c) = st.calls.get_mut(&key) {
                c.number = Some(num.clone());
            }
            if waiting {
                st.emit(BluetoothEvent::CallWaiting { number: Some(num) });
            } else if incoming {
                st.emit(BluetoothEvent::CallerId(num));
            }
            // Outbound calls store the number silently (dongle: outbound
            // never mints a caller_id event).
        }
    }
}

/// Caller id for a call, best-effort, in authority order:
/// 1. `GetPhoneCallInfo().PhoneNumber()` — the line service's own record.
///    (The metadata DOES expose this even though `PhoneCall` itself has no
///    number property; it is the most authoritative source when populated.)
/// 2. The history store entry whose id IS this call's `CallId`.
/// 3. The newest ringing/incoming history entry inside HISTORY_RECENT_SECS
///    (covers stores that key entries differently from call ids).
///
/// Nothing found = no CallerId at all — the plugin treats that as withheld.
fn resolve_number(st: &mut EngineState, key: &str) -> Option<String> {
    let (call, raw_id) = {
        let c = st.calls.get(key)?;
        (c.call.clone(), c.raw_call_id.clone())
    };
    if let Ok(info) = call.GetPhoneCallInfo() {
        if let Ok(n) = info.PhoneNumber() {
            let n = n.to_string_lossy();
            if !n.is_empty() {
                return Some(n);
            }
        }
    }
    let store = ensure_history_store(st)?;
    if !raw_id.is_empty() {
        if let Ok(entry) = store
            .GetEntryAsync(&HSTRING::from(raw_id.as_str()))
            .and_then(|op| op.get())
        {
            if let Some(n) = history_entry_number(&entry) {
                return Some(n);
            }
        }
    }
    let reader = store.GetEntryReader().ok()?;
    let batch = reader.ReadBatchAsync().and_then(|op| op.get()).ok()?;
    let now = winrt_ticks_now();
    let mut best: Option<(i64, String)> = None;
    for entry in batch {
        let entry_id = entry.Id().map(|h| h.to_string_lossy()).unwrap_or_default();
        if !raw_id.is_empty() && entry_id == raw_id {
            if let Some(n) = history_entry_number(&entry) {
                return Some(n);
            }
            continue;
        }
        let start = entry.StartTime().map(|d| d.UniversalTime).unwrap_or(0);
        let live_ring = entry.IsRinging().unwrap_or(false) || entry.IsIncoming().unwrap_or(false);
        if live_ring && ticks_within(start, now, HISTORY_RECENT_SECS) {
            if let Some(n) = history_entry_number(&entry) {
                if best.as_ref().is_none_or(|(bt, _)| start > *bt) {
                    best = Some((start, n));
                }
            }
        }
    }
    best.map(|(_, n)| n)
}

/// Lazy one-shot history-store acquisition. Full-history access needs a
/// capability an unpackaged process may not have, so try the limited scope
/// first and cache a negative — a capability refusal must not cost an RPC
/// per ringing call forever.
fn ensure_history_store(st: &mut EngineState) -> Option<PhoneCallHistoryStore> {
    if st.history.is_some() {
        return st.history.clone();
    }
    if st.history_failed {
        return None;
    }
    for access in [
        PhoneCallHistoryStoreAccessType::AllEntriesLimitedReadWrite,
        PhoneCallHistoryStoreAccessType::AppEntriesReadWrite,
    ] {
        if let Ok(store) =
            PhoneCallHistoryManager::RequestStoreAsync(access).and_then(|op| op.get())
        {
            st.history = Some(store.clone());
            return Some(store);
        }
    }
    st.history_failed = true;
    None
}

/// The raw phone number off a history entry's address. NOTE: the metadata
/// names this `RawAddress()` — there is no `Address()` string on the
/// address object itself.
fn history_entry_number(entry: &PhoneCallHistoryEntry) -> Option<String> {
    let raw = entry.Address().ok()?.RawAddress().ok()?.to_string_lossy();
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// Stable identity for a PhoneCall. `CallId` is the documented unique id and
/// doubles as the history-entry id; namespace it per line. Empty-id calls
/// (never observed, but the projection allows them) fall back to the info
/// record's start time so two calls on one line can still be told apart.
fn call_key(line_key: &str, call: &PhoneCall) -> String {
    let id = call
        .CallId()
        .map(|h| h.to_string_lossy())
        .unwrap_or_default();
    if !id.is_empty() {
        return format!("{line_key}:{id}");
    }
    let ticks = call
        .GetPhoneCallInfo()
        .and_then(|i| i.StartTime())
        .map(|d| d.UniversalTime)
        .unwrap_or(0);
    format!("{line_key}:t{ticks}")
}

/// First-sight direction: Incoming rings are inbound, Dialing legs are
/// outbound, anything else asks the call info (Unknown falls back to
/// inbound — the receptionist's dominant case; it only affects the CLCC
/// direction byte).
fn initial_direction(call: &PhoneCall, status: SimpleStatus) -> bool {
    match status {
        SimpleStatus::Incoming => false,
        SimpleStatus::Dialing => true,
        _ => matches!(
            call.GetPhoneCallInfo().and_then(|i| i.CallDirection()),
            Ok(d) if d == PhoneCallDirection::Outgoing
        ),
    }
}

/// Register StatusChanged -> EngineCmd. The handler never reads the call
/// (property gets can marshal cross-thread); the poll thread re-reads.
fn attach_status_handler(st: &EngineState, key: &str, call: &PhoneCall) -> Option<i64> {
    let tx = st.cmd_tx.clone();
    let key = key.to_string();
    call.StatusChanged(&TypedEventHandler::new(
        move |_: windows::core::Ref<'_, PhoneCall>, _: windows::core::Ref<'_, IInspectable>| {
            let _ = tx.send(EngineCmd::CallChanged(key.clone()));
            Ok(())
        },
    ))
    .ok()
}

/// Insert (or update) the call object `dial()` just made. Returns true when
/// the poll already announced OutgoingDialing for it (race between the dial
/// result and the 1s enumeration) so `dial()` does not double-emit.
fn upsert_dialed(st: &mut EngineState, line_key: &str, call: &PhoneCall, number: &str) -> bool {
    let key = call_key(line_key, call);
    if let Some(c) = st.calls.get_mut(&key) {
        let announced = c.track.outbound;
        c.track.outbound = true;
        c.outgoing = true;
        if c.number.is_none() {
            c.number = Some(number.to_string());
        }
        return announced;
    }
    let status_token = attach_status_handler(st, &key, call);
    let status = call
        .Status()
        .map(simple_status)
        .unwrap_or(SimpleStatus::Dialing);
    let raw_call_id = call
        .CallId()
        .map(|h| h.to_string_lossy())
        .unwrap_or_default();
    st.calls.insert(
        key.clone(),
        CallState {
            key: key.clone(),
            line_key: line_key.to_string(),
            call: call.clone(),
            raw_call_id,
            track: CallTrack {
                outbound: true, // dial() itself emits OutgoingDialing
                ..CallTrack::default()
            },
            status,
            outgoing: true,
            number: Some(number.to_string()),
            status_token,
            absent_ticks: 0,
            last_id_attempt: None,
        },
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: i32) -> PhoneCallStatus {
        PhoneCallStatus(v)
    }

    #[test]
    fn status_mapping_matches_winrt_values() {
        assert_eq!(simple_status(s(0)), SimpleStatus::Lost);
        assert_eq!(simple_status(s(1)), SimpleStatus::Incoming);
        assert_eq!(simple_status(s(2)), SimpleStatus::Dialing);
        assert_eq!(simple_status(s(3)), SimpleStatus::Talking);
        assert_eq!(simple_status(s(4)), SimpleStatus::Held);
        assert_eq!(simple_status(s(5)), SimpleStatus::Ended);
        // Unknown future values fail safe to Lost (terminal, settled).
        assert_eq!(simple_status(s(99)), SimpleStatus::Lost);
    }

    #[test]
    fn first_incoming_while_idle_rings_and_resolves() {
        let mut t = CallTrack::default();
        let out = plan_transition(&mut t, SimpleStatus::Incoming, false);
        assert_eq!(out, vec![Planned::CallIncoming, Planned::ResolveCallerId]);
        assert!(t.incoming && !t.waiting);
    }

    #[test]
    fn first_incoming_while_talking_is_a_waiting_knock() {
        let mut t = CallTrack::default();
        let out = plan_transition(&mut t, SimpleStatus::Incoming, true);
        assert_eq!(out, vec![Planned::CallWaiting]);
        assert!(t.waiting && !t.incoming);
    }

    #[test]
    fn duplicate_incoming_sighting_emits_nothing() {
        let mut t = CallTrack::default();
        plan_transition(&mut t, SimpleStatus::Incoming, false);
        assert!(plan_transition(&mut t, SimpleStatus::Incoming, false).is_empty());
    }

    #[test]
    fn answered_knock_never_emits_waiting_ended() {
        let mut t = CallTrack::default();
        plan_transition(&mut t, SimpleStatus::Incoming, true); // knock
        let out = plan_transition(&mut t, SimpleStatus::Talking, true);
        assert_eq!(out, vec![Planned::CallAnswered]);
        assert!(t.answered && !t.waiting);
        // ...and when that call later ends, it is an ordinary termination.
        let out = plan_transition(&mut t, SimpleStatus::Ended, false);
        assert_eq!(out, vec![Planned::CallTerminated]);
    }

    #[test]
    fn unanswered_knock_ends_with_waiting_ended_only() {
        let mut t = CallTrack::default();
        plan_transition(&mut t, SimpleStatus::Incoming, true); // knock
        let out = plan_transition(&mut t, SimpleStatus::Lost, false);
        assert_eq!(out, vec![Planned::CallWaitingEnded]);
        // Settled exactly once — a repeated terminal sighting stays silent.
        assert!(plan_transition(&mut t, SimpleStatus::Ended, false).is_empty());
    }

    #[test]
    fn normal_call_lifecycle_emits_answer_then_one_termination() {
        let mut t = CallTrack::default();
        plan_transition(&mut t, SimpleStatus::Incoming, false);
        assert_eq!(
            plan_transition(&mut t, SimpleStatus::Talking, false),
            vec![Planned::CallAnswered]
        );
        // Resume-from-hold style re-talk does not re-answer.
        assert!(plan_transition(&mut t, SimpleStatus::Held, false).is_empty());
        assert!(plan_transition(&mut t, SimpleStatus::Talking, false).is_empty());
        assert_eq!(
            plan_transition(&mut t, SimpleStatus::Ended, false),
            vec![Planned::CallTerminated]
        );
        assert!(plan_transition(&mut t, SimpleStatus::Ended, false).is_empty());
    }

    #[test]
    fn handset_dialed_call_announces_dialing_and_ringing() {
        let mut t = CallTrack::default();
        let out = plan_transition(&mut t, SimpleStatus::Dialing, false);
        assert_eq!(out, vec![Planned::OutgoingDialing, Planned::CallRinging]);
        assert!(t.outbound && t.ringing);
        // Its answer + end still follow the normal rules.
        assert_eq!(
            plan_transition(&mut t, SimpleStatus::Talking, false),
            vec![Planned::CallAnswered]
        );
        assert_eq!(
            plan_transition(&mut t, SimpleStatus::Ended, false),
            vec![Planned::CallTerminated]
        );
    }

    #[test]
    fn our_own_dialed_call_only_adds_ringing_then_answer() {
        // upsert_dialed presets outbound (dial() emitted OutgoingDialing).
        let mut t = CallTrack {
            outbound: true,
            ..CallTrack::default()
        };
        let out = plan_transition(&mut t, SimpleStatus::Dialing, false);
        assert_eq!(out, vec![Planned::CallRinging]);
        assert_eq!(
            plan_transition(&mut t, SimpleStatus::Talking, false),
            vec![Planned::CallAnswered]
        );
    }

    #[test]
    fn first_seen_talking_recovers_as_answered() {
        // Engine started mid-call: mint the session (dongle recovery parity).
        let mut t = CallTrack::default();
        assert_eq!(
            plan_transition(&mut t, SimpleStatus::Talking, false),
            vec![Planned::CallAnswered]
        );
    }

    #[test]
    fn never_announced_call_ends_silently() {
        // Only ever seen Held (e.g. parked by another app before we started):
        // no session events ever went out, so no CallTerminated either.
        let mut t = CallTrack::default();
        assert!(plan_transition(&mut t, SimpleStatus::Held, false).is_empty());
        assert!(plan_transition(&mut t, SimpleStatus::Ended, false).is_empty());
        assert!(t.ended);
    }

    #[test]
    fn ended_first_sight_is_inert() {
        let mut t = CallTrack::default();
        assert!(plan_transition(&mut t, SimpleStatus::Ended, false).is_empty());
        assert!(t.ended);
    }

    #[test]
    fn held_state_synthesis_matches_hfp_callheld() {
        assert_eq!(synthesize_held_state(0, 0), 0);
        assert_eq!(synthesize_held_state(1, 0), 0);
        assert_eq!(synthesize_held_state(1, 1), 1); // held + active mix
        assert_eq!(synthesize_held_state(2, 1), 1);
        assert_eq!(synthesize_held_state(0, 1), 2); // all held
        assert_eq!(synthesize_held_state(0, 2), 2);
    }

    #[test]
    fn clcc_status_bytes_match_dongle_mapping() {
        assert_eq!(clcc_status(SimpleStatus::Talking, false), 0);
        assert_eq!(clcc_status(SimpleStatus::Held, false), 1);
        assert_eq!(clcc_status(SimpleStatus::Dialing, false), 2);
        assert_eq!(clcc_status(SimpleStatus::Incoming, false), 4);
        assert_eq!(clcc_status(SimpleStatus::Incoming, true), 5); // waiting
    }

    #[test]
    fn op_status_names_cover_the_enum() {
        assert_eq!(op_status_name(PhoneCallOperationStatus(0)), "succeeded");
        assert_eq!(op_status_name(PhoneCallOperationStatus(2)), "timed out");
        assert_eq!(
            op_status_name(PhoneCallOperationStatus(3)),
            "connection lost"
        );
        assert_eq!(
            op_status_name(PhoneCallOperationStatus(4)),
            "invalid call state"
        );
        assert_eq!(
            op_status_name(PhoneCallOperationStatus(1)),
            "operation failed"
        );
        assert_eq!(
            op_status_name(PhoneCallOperationStatus(77)),
            "operation failed"
        );
    }

    #[test]
    fn winrt_ticks_epoch_and_window() {
        // 1970-01-01T00:00:00Z == 116444736000000000 ticks since 1601.
        assert_eq!(unix_secs_to_winrt_ticks(0), 116_444_736_000_000_000);
        let now = unix_secs_to_winrt_ticks(1_000_000);
        assert!(ticks_within(now, now, 30));
        assert!(ticks_within(now - 29 * TICKS_PER_SECOND, now, 30));
        assert!(!ticks_within(now - 31 * TICKS_PER_SECOND, now, 30));
        // 1s future skew tolerated, more is not "recent".
        assert!(ticks_within(now + TICKS_PER_SECOND, now, 30));
        assert!(!ticks_within(now + 2 * TICKS_PER_SECOND, now, 30));
    }
}
