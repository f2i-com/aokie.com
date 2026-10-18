//! The native engine worker thread: owns the WinRT apartment, drives the
//! Calls engine / WASAPI pump, services plugin controls, and starts the
//! MAP loop on a messaging thread of its own. Mirrors the role of
//! `aokie_radio::runtime`'s IO thread — one phone, honest errors instead of
//! fabricated success.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aokie_bluetooth::aokie_radio::runtime::{PairingConfirmSlot, PairingWindow};
use aokie_dongle::bluetooth::{AudioData, BluetoothEvent};

use crate::audio::AudioPump;
use crate::calls::CallsEngine;
use crate::map::MapLoop;
use crate::pairing;
use crate::runtime::{adapter_present, NativeControl, NativeShared, TxAudio};

/// One text for the messaging thread to send.
struct SmsRequest {
    message_id: String,
    recipient: String,
    body: String,
    msg_type: Option<String>,
}

/// How long a shutdown waits for the messaging thread before leaving it to
/// finish on its own (an op in flight ends within rfcomm's limits).
const MESSAGING_STOP_WAIT: Duration = Duration::from_secs(2);

pub(crate) fn run(
    event_tx: Sender<BluetoothEvent>,
    audio_tx: Sender<AudioData>,
    control_rx: Receiver<NativeControl>,
    tx_audio: Arc<TxAudio>,
    shared: Arc<NativeShared>,
    window: PairingWindow,
    _confirm: PairingConfirmSlot,
) {
    // All WinRT objects live on this MTA thread.
    unsafe {
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_TYPE};
        let _ = RoInitialize(RO_INIT_TYPE(1));
    }

    if !adapter_present() {
        let _ = event_tx.send(BluetoothEvent::Error(
            "native backend: no Windows Bluetooth adapter found — enable Bluetooth or set transportMode=dongle"
                .to_string(),
        ));
        drain_until_shutdown(&control_rx, &event_tx);
        return;
    }

    // The phone texts and contacts come from, known before any call line is up.
    seed_phone_from_bond(&shared);

    // Calls engine: call control via the WinRT Calls API. Unavailable ⇒ the
    // rest still starts (SMS/audio report their own errors); the error event
    // keeps health honest instead of silently green.
    let calls = match CallsEngine::start(event_tx.clone(), shared.clone()) {
        Ok(c) => {
            eprintln!("[aokie-winbt] Calls engine started (WinRT Calls API)");
            Some(c)
        }
        Err(e) => {
            let _ = event_tx.send(BluetoothEvent::Error(format!(
                "native backend: Calls API unavailable ({e}) — call control disabled"
            )));
            None
        }
    };

    // WASAPI pump: opens Hands-Free endpoints when a call goes active.
    let audio = match AudioPump::start(event_tx.clone(), audio_tx, tx_audio, shared.clone()) {
        Ok(a) => Some(a),
        Err(e) => {
            let _ = event_tx.send(BluetoothEvent::Error(format!(
                "native backend: audio pump failed to start ({e})"
            )));
            None
        }
    };

    // MAP/PBAP messaging loop (poll-driven; MNS hosting is a later phase),
    // on a thread of its own: its RFCOMM steps wait on the phone (seconds
    // when Windows pages it up, up to rfcomm's limits when it stalls), and
    // answering or hanging up a call must never wait behind a text.
    let (sms_tx, sms_rx) = std::sync::mpsc::channel::<SmsRequest>();
    let messaging = {
        let event_tx = event_tx.clone();
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("aokie-winbt-messaging".to_string())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || run_messaging(event_tx, shared, sms_rx))
    };
    let (mut sms_tx, messaging) = match messaging {
        Ok(handle) => (Some(sms_tx), Some(handle)),
        Err(e) => {
            let _ = event_tx.send(BluetoothEvent::Error(format!(
                "native backend: messaging thread failed to start ({e}) — texts disabled"
            )));
            (None, None)
        }
    };

    let mut discovery_on = false;
    loop {
        // Pairing window transitions drive classic discoverability. Windows
        // owns the actual SSP ceremony from there (system dialog).
        let want_discovery = window.is_open();
        if want_discovery != discovery_on {
            match pairing::set_discoverable(want_discovery) {
                Ok(()) => discovery_on = want_discovery,
                Err(e) => {
                    let _ = event_tx.send(BluetoothEvent::Error(format!(
                        "native backend: could not {} discoverable mode ({e})",
                        if want_discovery { "enter" } else { "leave" }
                    )));
                    // Do not flap: adopt the desired state and retry on the
                    // next transition request.
                    discovery_on = want_discovery;
                }
            }
        }

        match control_rx.recv_timeout(Duration::from_millis(75)) {
            Ok(NativeControl::Shutdown) => break,
            Ok(NativeControl::Answer) => {
                if let Some(c) = &calls {
                    if let Err(e) = c.answer() {
                        let _ = event_tx.send(BluetoothEvent::Error(format!("answer: {e}")));
                    }
                }
            }
            Ok(NativeControl::RejectOrHangup) => {
                if let Some(c) = &calls {
                    if let Err(e) = c.reject_or_hangup() {
                        let _ = event_tx.send(BluetoothEvent::Error(format!("reject/hangup: {e}")));
                    }
                }
            }
            Ok(NativeControl::Dial(number)) => {
                if let Some(c) = &calls {
                    if let Err(e) = c.dial(&number) {
                        let _ = event_tx.send(BluetoothEvent::Error(format!("dial: {e}")));
                    }
                }
            }
            Ok(NativeControl::HoldSwap) => {
                if let Some(c) = &calls {
                    if let Err(e) = c.hold_swap() {
                        let _ = event_tx.send(BluetoothEvent::Error(format!("hold swap: {e}")));
                    }
                }
            }
            Ok(NativeControl::QueryCalls) => {
                if let Some(c) = &calls {
                    if let Err(e) = c.query_calls() {
                        let _ = event_tx.send(BluetoothEvent::Error(format!("query calls: {e}")));
                    }
                }
            }
            Ok(NativeControl::SendSms {
                message_id,
                recipient,
                body,
                msg_type,
            }) => {
                // Sent on the messaging thread; SmsSent / SmsSendFailed say
                // how it went, as the dongle's do.
                let request = SmsRequest {
                    message_id,
                    recipient,
                    body,
                    msg_type,
                };
                let refused = match &sms_tx {
                    Some(tx) => tx.send(request).err().map(|e| e.0),
                    None => Some(request),
                };
                if let Some(request) = refused {
                    let _ = event_tx.send(BluetoothEvent::SmsSendFailed {
                        message_id: request.message_id,
                        recipient_phone: request.recipient,
                        reason: "native backend: the messaging thread is not running".to_string(),
                    });
                }
            }
            Ok(NativeControl::Connect(_address, reply)) => {
                // Windows owns HFP connection lifecycle: a paired phone in
                // range connects itself. Honest no-op (false = no action
                // taken), same shape as the dongle's "already connected".
                let _ = reply.send(Ok(false));
            }
            Ok(NativeControl::Disconnect(_address, reply)) => {
                // No supported user-mode API to drop a single HFP link;
                // unpairing is the only lever and we never surprise-forget.
                let _ = reply.send(Ok(false));
            }
            Ok(NativeControl::RemovePaired(address, reply)) => {
                let result = pairing::unpair(&address);
                // The phone texts went to is forgotten: texts go to the next
                // bonded phone, or nowhere, never on to the unpaired one.
                let seeded = shared.phone_address.read().map(|p| *p).unwrap_or(0);
                if matches!(result, Ok(true)) && pairing::parse_address(&address) == Some(seeded) {
                    if let Ok(mut slot) = shared.phone_address.write() {
                        *slot = 0;
                    }
                    seed_phone_from_bond(&shared);
                }
                let _ = reply.send(result);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    if discovery_on {
        let _ = pairing::set_discoverable(false);
    }
    // Closing the request channel ends the messaging thread after the op in
    // flight; wait a little for it, then let it finish on its own.
    sms_tx.take();
    if let Some(handle) = messaging {
        let started = Instant::now();
        while !handle.is_finished() && started.elapsed() < MESSAGING_STOP_WAIT {
            std::thread::sleep(Duration::from_millis(20));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            eprintln!("[aokie-winbt] messaging thread still in an op at shutdown — left to finish on its own");
        }
    }
    if let Some(a) = audio {
        a.stop();
    }
    shared
        .call_active
        .store(false, std::sync::atomic::Ordering::Relaxed);
    shared
        .connected
        .store(false, std::sync::atomic::Ordering::Relaxed);
    shared
        .sample_rate
        .store(0, std::sync::atomic::Ordering::Relaxed);
}

/// The messaging thread: texts the worker hands over, and the MAP loop's
/// inbox poll and phonebook pull between them, until the worker closes the
/// request channel.
fn run_messaging(
    event_tx: Sender<BluetoothEvent>,
    shared: Arc<NativeShared>,
    requests: Receiver<SmsRequest>,
) {
    unsafe {
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_TYPE};
        let _ = RoInitialize(RO_INIT_TYPE(1));
    }
    let mut map = MapLoop::new(event_tx, shared);
    loop {
        match requests.recv_timeout(Duration::from_millis(75)) {
            Ok(request) => {
                // The outcome goes out as SmsSent / SmsSendFailed.
                let _ = map.send_sms(
                    &request.message_id,
                    &request.recipient,
                    &request.body,
                    request.msg_type,
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        map.tick();
    }
}

/// Seed the phone identity from the Windows bond so MAP/PBAP ops can run
/// ON DEMAND: Windows drops the HFP link when idle, but every op forces
/// the link up for its duration (proven live 2026-07-19 — an idle,
/// paired-but-disconnected phone accepted the MAS channel and sent the
/// SMS). Gating messaging on the HFP line instead would block SMS and
/// missed-call callbacks exactly when the line is quiet.
fn seed_phone_from_bond(shared: &NativeShared) {
    if let Some((addr_str, name)) = pairing::bonded_devices().into_iter().next() {
        if let Some(addr) = pairing::parse_address(&addr_str) {
            if let Ok(mut slot) = shared.phone_address.write() {
                *slot = addr;
            }
            if let Ok(mut slot) = shared.remote_address.write() {
                *slot = addr_str.clone();
            }
            if name.is_some() {
                if let Ok(mut slot) = shared.remote_name.write() {
                    *slot = name.clone();
                }
            }
            eprintln!(
                "[aokie-winbt] phone seeded from Windows bond: {addr_str} ({name:?}) — MAP ops run on demand over the idle link"
            );
        }
    }
}

/// No adapter: keep servicing the control channel with honest refusals so the
/// plugin sees a dead radio (health degraded) instead of a vanished one.
fn drain_until_shutdown(control_rx: &Receiver<NativeControl>, event_tx: &Sender<BluetoothEvent>) {
    while let Ok(control) = control_rx.recv() {
        match control {
            NativeControl::Shutdown => break,
            NativeControl::SendSms {
                message_id,
                recipient,
                ..
            } => {
                let _ = event_tx.send(BluetoothEvent::SmsSendFailed {
                    message_id,
                    recipient_phone: recipient,
                    reason: "native backend: no Bluetooth adapter".to_string(),
                });
            }
            NativeControl::Connect(_, reply)
            | NativeControl::Disconnect(_, reply)
            | NativeControl::RemovePaired(_, reply) => {
                let _ = reply.send(Err("native backend: no Bluetooth adapter".to_string()));
            }
            _ => {}
        }
    }
}
