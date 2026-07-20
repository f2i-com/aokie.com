//! The native engine worker thread: owns the WinRT apartment, drives the
//! Calls engine / WASAPI pump / MAP loop, and services plugin controls.
//! Mirrors the role of `aokie_radio::runtime`'s IO thread — one thread, one
//! phone, honest errors instead of fabricated success.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use aokie_bluetooth::aokie_radio::runtime::{PairingConfirmSlot, PairingWindow};
use aokie_dongle::bluetooth::{AudioData, BluetoothEvent};

use crate::audio::AudioPump;
use crate::calls::CallsEngine;
use crate::map::MapLoop;
use crate::pairing;
use crate::runtime::{adapter_present, NativeControl, NativeShared, TxAudio};

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
        drain_until_shutdown(&control_rx);
        return;
    }

    // Seed the phone identity from the Windows bond so MAP/PBAP ops can run
    // ON DEMAND: Windows drops the HFP link when idle, but every op forces
    // the link up for its duration (proven live 2026-07-19 — an idle,
    // paired-but-disconnected phone accepted the MAS channel and sent the
    // SMS). Gating messaging on the HFP line instead would block SMS and
    // missed-call callbacks exactly when the line is quiet.
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

    // MAP/PBAP messaging loop (poll-driven; MNS hosting is a later phase).
    let mut map = MapLoop::new(event_tx.clone(), shared.clone());

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
                reply,
            }) => {
                let result = map.send_sms(&message_id, &recipient, &body, msg_type);
                let _ = reply.send(result);
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
                let _ = reply.send(pairing::unpair(&address));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        map.tick();
    }

    if discovery_on {
        let _ = pairing::set_discoverable(false);
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

/// No adapter: keep servicing the control channel with honest refusals so the
/// plugin sees a dead radio (health degraded) instead of a vanished one.
fn drain_until_shutdown(control_rx: &Receiver<NativeControl>) {
    while let Ok(control) = control_rx.recv() {
        match control {
            NativeControl::Shutdown => break,
            NativeControl::SendSms { reply, .. } => {
                let _ = reply.send(Err("native backend: no Bluetooth adapter".to_string()));
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
