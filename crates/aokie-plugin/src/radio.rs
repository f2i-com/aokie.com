//! Live Bluetooth radio integration.
//!
//! Runs the real `aokie_radio` `AokieRuntime` (via
//! [`aokie_dongle::bluetooth::BluetoothManager`]) on a dedicated background
//! thread, maps its `BluetoothEvent`s onto the `aokie.*` Desktop-event
//! contract (emitted straight to stdout **and** the durable outbox), and
//! accepts control requests — answer / reject / hangup / send-SMS / speak —
//! from the main RPC thread over an mpsc channel.
//!
//! ## Why a second `Outbox` + `StdoutSink` on this thread
//! The plugin's main loop blocks on stdin, so an asynchronous call event (a
//! call can ring at any instant) must be delivered without waiting for the
//! next RPC. `StdoutSink` writes one whole line under the stdout lock
//! (line-atomic), so this thread's sink and the main thread's sink never
//! interleave. The [`Outbox`] here is a *second* SQLite connection to the
//! same `outbox.sqlite` file — `idempotency_key` is UNIQUE and SQLite
//! serialises writers, so essential call/SMS records survive a Desktop
//! restart exactly as they do on the command path.
//!
//! The whole radio surface is Windows-only (WinUSB); on other targets
//! [`spawn`] returns an error and the plugin simply never has a radio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use aokie_core::events::DesktopEvent;
use serde_json::json;

use crate::config::PairedDevice;
use crate::event_bridge::{emit_event, Sink};
use crate::outbox::Outbox;

/// A control request from the main RPC thread to the radio thread. Every
/// variant is fire-and-forget: the *result* of the action arrives back as an
/// asynchronous `aokie.*` event from the radio thread, matching the mock
/// contract (`call.answer` → later `aokie.call.answered`, etc.).
pub enum RadioControl {
    Answer,
    Reject,
    Hangup,
    SendSms { to: String, body: String },
    /// Speak text to the caller. Stage 1 acknowledges + logs; the TTS →
    /// SCO audio path is wired in Stage 2.
    Speak { text: String },
    Shutdown,
}

/// Live radio status, shared (via `Arc`) between the radio thread (writer)
/// and the main RPC thread (reader) so `phone.status` / `dongle.diagnostics`
/// answer without round-tripping the radio thread.
#[derive(Default)]
pub struct RadioStatus {
    pub initialized: AtomicBool,
    pub connected: AtomicBool,
    pub call_active: AtomicBool,
    pub local_address: Mutex<Option<String>>,
    pub connected_address: Mutex<Option<String>>,
    pub current_caller: Mutex<Option<String>>,
    /// Devices seen connected during this radio session (the durable link
    /// keys live in the aokie pairing store; this is the live view).
    pub paired: Mutex<Vec<PairedDevice>>,
    /// Last fatal reason the radio reported (no dongle, driver not bound, …).
    pub last_error: Mutex<Option<String>>,
}

/// Handle held by the [`Plugin`](crate::connector::Plugin): send control
/// requests and read live status. Dropping it (process shutdown) drops the
/// `control_tx`, which ends the radio loop and shuts the runtime down.
pub struct RadioHandle {
    control_tx: Sender<RadioControl>,
    pub status: Arc<RadioStatus>,
}

impl RadioHandle {
    pub fn send(&self, c: RadioControl) -> Result<(), String> {
        self.control_tx
            .send(c)
            .map_err(|_| "the radio thread is not running".to_string())
    }

    pub fn is_initialized(&self) -> bool {
        self.status.initialized.load(Ordering::Relaxed)
    }
    pub fn is_connected(&self) -> bool {
        self.status.connected.load(Ordering::Relaxed)
    }
    pub fn is_call_active(&self) -> bool {
        self.status.call_active.load(Ordering::Relaxed)
    }
    pub fn local_address(&self) -> Option<String> {
        self.status.local_address.lock().unwrap().clone()
    }
    pub fn connected_address(&self) -> Option<String> {
        self.status.connected_address.lock().unwrap().clone()
    }
    pub fn current_caller(&self) -> Option<String> {
        self.status.current_caller.lock().unwrap().clone()
    }
    pub fn paired(&self) -> Vec<PairedDevice> {
        self.status.paired.lock().unwrap().clone()
    }
    pub fn last_error(&self) -> Option<String> {
        self.status.last_error.lock().unwrap().clone()
    }
}

/// Emit one event best-effort: essential events route through the outbox
/// (write-before-emit) when it is open; otherwise fall back to a direct
/// stdout notification so a failed outbox never swallows a live call event.
fn emit(outbox: Option<&Outbox>, sink: &mut dyn Sink, event: DesktopEvent) {
    match outbox {
        Some(o) => {
            if let Err(e) = emit_event(sink, o, &event, false) {
                eprintln!("[aokie-plugin] radio emit '{}' failed: {e}", event.name);
            }
        }
        None => {
            let line = crate::rpc::notification_line("event.emit", json!({ "event": event }));
            let _ = sink.send_line(&line);
        }
    }
}

// ── Windows: the real radio ──────────────────────────────────────────────

/// Start the live radio on a background thread. Returns immediately with a
/// handle; initialisation happens asynchronously and is reflected in
/// [`RadioStatus`] (and via an `aokie.dongle.ready` / `aokie.hardware.error`
/// event). `preferred_path` pins a specific WinUSB dongle path, or `None`
/// takes the first enumerated HCI controller.
#[cfg(target_os = "windows")]
pub fn spawn(
    data_dir: std::path::PathBuf,
    preferred_path: Option<String>,
    auto_answer: bool,
) -> Result<RadioHandle, String> {
    use aokie_dongle::bluetooth::BluetoothManager;
    use std::sync::mpsc;

    let (control_tx, control_rx) = mpsc::channel::<RadioControl>();
    let status = Arc::new(RadioStatus::default());
    let status_thread = status.clone();

    std::thread::Builder::new()
        .name("aokie-plugin-radio".to_string())
        // Match the runtime thread's generous stack — the deep ACL → L2CAP →
        // RFCOMM → HFP dispatch overflowed the 1 MiB Windows default.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let mut bt = match BluetoothManager::new_with_preferred_dongle(preferred_path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("[aokie-plugin] radio failed to start: {e}");
                    *status_thread.last_error.lock().unwrap() = Some(e);
                    return;
                }
            };
            // Second connection to the same outbox file (see module docs).
            let outbox = Outbox::open(&data_dir.join(crate::connector::OUTBOX_FILE)).ok();
            if outbox.is_none() {
                eprintln!("[aokie-plugin] radio: outbox unavailable, emitting without durability");
            }
            let mut sink = crate::event_bridge::StdoutSink::new();
            run_loop(&mut bt, outbox.as_ref(), &mut sink, control_rx, status_thread, auto_answer);
        })
        .map_err(|e| format!("spawn radio thread: {e}"))?;

    Ok(RadioHandle { control_tx, status })
}

/// The radio poll loop: drain events → map+emit; buffer the incoming-call
/// emission until the caller id lands (or a short timeout); drain audio
/// (Stage 2 feeds the AI here); service control requests. Runs until the
/// control channel closes or a Shutdown is received.
#[cfg(target_os = "windows")]
fn run_loop(
    bt: &mut aokie_dongle::bluetooth::BluetoothManager,
    outbox: Option<&Outbox>,
    sink: &mut dyn Sink,
    control_rx: std::sync::mpsc::Receiver<RadioControl>,
    status: Arc<RadioStatus>,
    auto_answer: bool,
) {
    use std::sync::mpsc::TryRecvError;
    use std::time::{Duration, Instant};

    // Per-call bookkeeping. `pending_incoming` holds a just-rung call whose
    // `aokie.call.incoming` we delay briefly so the CLIP (caller id) can be
    // folded into its `from` field — matching the mock's `{from, at}` shape.
    // `answered_corr` records the call we've already auto-answered so we
    // answer exactly once.
    //
    // Auto-answer fires IMMEDIATELY when a call appears — NOT after a ring
    // delay — because on some dongles the SCO/audio channel that comes up
    // right after the ring blocks the radio's main loop, so a late answer
    // never gets serviced. Answering in the brief pre-SCO window is what
    // gets the AT+ATA out. (Emitting `aokie.call.incoming` still waits for
    // the caller id; only the answer is hurried.)
    let mut caller_id: Option<String> = None;
    let mut current_corr: Option<String> = None;
    let mut pending_incoming: Option<(String, Instant)> = None;
    let mut answered_corr: Option<String> = None;

    loop {
        let mut idle = true;

        while let Some(ev) = bt.try_recv_event() {
            idle = false;
            handle_event(
                ev,
                &mut caller_id,
                &mut current_corr,
                &mut pending_incoming,
                outbox,
                sink,
                &status,
            );
        }

        // Flush a buffered incoming call once the caller id is known or the
        // grace window elapses.
        if let Some((corr, since)) = pending_incoming.clone() {
            if caller_id.is_some() || since.elapsed() > Duration::from_millis(800) {
                let from = caller_id.clone().unwrap_or_else(|| "unknown".to_string());
                emit(
                    outbox,
                    sink,
                    aokie_core::events::aokie_event(
                        "aokie.call.incoming",
                        &corr,
                        json!({"from": from, "at": aokie_core::events::now_iso8601()}),
                    ),
                );
                pending_incoming = None;
            }
        }

        // Auto-answer ASAP: the instant a call is present and not yet answered,
        // send the answer — before the audio channel comes up and freezes the
        // loop. Answer exactly once per call (tracked by `answered_corr`).
        if auto_answer {
            match current_corr.as_deref() {
                Some(corr)
                    if answered_corr.as_deref() != Some(corr)
                        && !status.call_active.load(Ordering::Relaxed) =>
                {
                    match bt.answer_call() {
                        Ok(()) => eprintln!("[aokie-plugin] auto-answered incoming call (immediate)"),
                        Err(e) => eprintln!("[aokie-plugin] auto-answer failed: {e}"),
                    }
                    answered_corr = Some(corr.to_string());
                    idle = false;
                }
                None => answered_corr = None, // call cleared — ready for the next
                _ => {}
            }
        }

        // Stage 1 discards captured audio; Stage 2 feeds it to STT here.
        while bt.try_recv_audio().is_some() {
            idle = false;
        }

        loop {
            match control_rx.try_recv() {
                Ok(RadioControl::Answer) => {
                    if let Err(e) = bt.answer_call() {
                        eprintln!("[aokie-plugin] radio answer failed: {e}");
                    }
                }
                Ok(RadioControl::Reject) => {
                    if let Err(e) = bt.reject_call() {
                        eprintln!("[aokie-plugin] radio reject failed: {e}");
                    }
                }
                Ok(RadioControl::Hangup) => {
                    if let Err(e) = bt.hangup() {
                        eprintln!("[aokie-plugin] radio hangup failed: {e}");
                    }
                }
                Ok(RadioControl::SendSms { to, body }) => {
                    if let Err(e) = bt.send_sms(to, body, None) {
                        emit(
                            outbox,
                            sink,
                            aokie_core::events::aokie_event(
                                "aokie.hardware.error",
                                "radio",
                                json!({"message": format!("send_sms failed: {e}")}),
                            ),
                        );
                    }
                }
                Ok(RadioControl::Speak { text }) => {
                    // Stage 2 synthesises this to SCO audio via TTS +
                    // `send_audio`. Stage 1 acknowledges + logs so the flow's
                    // operatorSpeak node doesn't error.
                    eprintln!(
                        "[aokie-plugin] radio operatorSpeak ({} chars) — TTS wiring pending",
                        text.chars().count()
                    );
                }
                Ok(RadioControl::Shutdown) => return,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
            idle = false;
        }

        if idle {
            std::thread::sleep(Duration::from_millis(15));
        }
    }
}

/// Map one `BluetoothEvent` to the `aokie.*` contract + update shared status.
/// Available on all targets (it only touches `BluetoothEvent`, which is not
/// Windows-gated) so it stays unit-testable.
fn handle_event(
    ev: aokie_dongle::bluetooth::BluetoothEvent,
    caller_id: &mut Option<String>,
    current_corr: &mut Option<String>,
    pending_incoming: &mut Option<(String, std::time::Instant)>,
    outbox: Option<&Outbox>,
    sink: &mut dyn Sink,
    status: &Arc<RadioStatus>,
) {
    use aokie_core::events::{aokie_event, now_iso8601};
    use aokie_dongle::bluetooth::BluetoothEvent as E;

    match ev {
        E::Initialized(addr) => {
            status.initialized.store(true, Ordering::Relaxed);
            *status.local_address.lock().unwrap() = Some(addr.clone());
            *status.last_error.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event("aokie.dongle.ready", "radio", json!({"address": addr, "source": "radio"})),
            );
        }
        E::DeviceConnected(addr) => {
            status.connected.store(true, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = Some(addr.clone());
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
                aokie_event("aokie.phone.connected", "radio", json!({"address": addr})),
            );
        }
        E::DeviceDisconnected(addr) => {
            status.connected.store(false, Ordering::Relaxed);
            *status.connected_address.lock().unwrap() = None;
            emit(
                outbox,
                sink,
                aokie_event("aokie.phone.disconnected", "radio", json!({"address": addr})),
            );
        }
        E::CallIncoming => {
            let corr = format!("call_{}", uuid::Uuid::new_v4().simple());
            *current_corr = Some(corr.clone());
            *caller_id = None;
            *status.current_caller.lock().unwrap() = None;
            *pending_incoming = Some((corr, std::time::Instant::now()));
        }
        E::CallerId(num) => {
            *caller_id = Some(num.clone());
            *status.current_caller.lock().unwrap() = Some(num);
        }
        E::CallRinging => {
            if let Some(corr) = current_corr.as_ref() {
                emit(
                    outbox,
                    sink,
                    aokie_event("aokie.call.ringing", corr, json!({"at": now_iso8601()})),
                );
            }
        }
        E::CallAnswered => {
            status.call_active.store(true, Ordering::Relaxed);
            if let Some(corr) = current_corr.as_ref() {
                emit(
                    outbox,
                    sink,
                    aokie_event("aokie.call.answered", corr, json!({"at": now_iso8601()})),
                );
            }
        }
        E::CallTerminated => {
            status.call_active.store(false, Ordering::Relaxed);
            if let Some(corr) = current_corr.take() {
                emit(
                    outbox,
                    sink,
                    aokie_event(
                        "aokie.call.ended",
                        &corr,
                        json!({"at": now_iso8601(), "reason": "remote_or_operator"}),
                    ),
                );
            }
            *caller_id = None;
            *pending_incoming = None;
            *status.current_caller.lock().unwrap() = None;
        }
        E::AudioConnected { codec, sample_rate } => {
            let corr = current_corr.clone().unwrap_or_else(|| "radio".to_string());
            emit(
                outbox,
                sink,
                aokie_event(
                    "aokie.call.audio.connected",
                    &corr,
                    json!({"codec": codec, "sampleRate": sample_rate}),
                ),
            );
        }
        E::AudioDisconnected => {
            let corr = current_corr.clone().unwrap_or_else(|| "radio".to_string());
            emit(
                outbox,
                sink,
                aokie_event("aokie.call.audio.disconnected", &corr, json!({})),
            );
        }
        E::SmsReceived(p) => {
            let corr = format!("sms_{}", uuid::Uuid::new_v4().simple());
            emit(
                outbox,
                sink,
                aokie_event(
                    "aokie.sms.received",
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
        E::SmsSent { recipient_phone } => {
            let message_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
            emit(
                outbox,
                sink,
                aokie_event(
                    "aokie.sms.sent",
                    &message_id,
                    json!({"messageId": message_id, "to": recipient_phone, "at": now_iso8601()}),
                ),
            );
        }
        E::ContactsFetched(_) | E::MapNotificationsSubscribed => {
            // Phonebook / MNS-subscribe are diagnostic; nothing to surface yet.
        }
        E::Error(e) => {
            *status.last_error.lock().unwrap() = Some(e.clone());
            emit(
                outbox,
                sink,
                aokie_event("aokie.hardware.error", "radio", json!({"message": e})),
            );
        }
    }
}

// ── Non-Windows: no radio ────────────────────────────────────────────────

#[cfg(not(target_os = "windows"))]
pub fn spawn(
    _data_dir: std::path::PathBuf,
    _preferred_path: Option<String>,
    _auto_answer: bool,
) -> Result<RadioHandle, String> {
    Err("the Aokie radio is only supported on Windows (WinUSB)".to_string())
}
