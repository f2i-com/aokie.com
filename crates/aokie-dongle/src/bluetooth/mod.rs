//! Bluetooth HFP module backed by the in-tree `aokie_radio` runtime.
//!
//! Provides Hands-Free Profile (HFP) audio for the AI receptionist. All
//! HCI / L2CAP / RFCOMM / SCO traffic is driven directly over WinUSB by
//! `aokie_radio`; the previous BTstack FFI bridge has been removed.
//!
//! The `BluetoothManager` exposes both polling (`try_recv_*`) and awaiting
//! (`recv_*`) APIs; the current event loop uses polling, but we keep the
//! async APIs live for callers that want a different concurrency shape.
#![allow(dead_code)]

pub mod preferred_dongle;

/// Re-exported so the plugin can hold a lock-free handle to the radio's pairing
/// window (AOK-BT-001) without a direct aokie-bluetooth dependency.
pub use aokie_bluetooth::aokie_radio::runtime::PairingWindow;
/// Re-exported so the plugin can surface the held SSP numeric comparison in
/// `phone.status` without a direct aokie-bluetooth dependency (PAIR-001).
pub use aokie_bluetooth::aokie_radio::runtime::{PairingConfirmSlot, PendingPairingConfirm};

use serde::{Deserialize, Serialize};

/// Event types emitted by the Bluetooth manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BluetoothEvent {
    Initialized(String),     // Local BT address
    DeviceConnected(String), // Remote device address
    DeviceDisconnected(String),
    CallIncoming,
    CallRinging,
    /// Phase 2: an OUTBOUND call setup started (`callsetup,2`) — either the
    /// plugin dialed (ATD) or the phone's owner dialed on the handset. The
    /// eventual `CallAnswered` is the REMOTE side picking up, never a
    /// caller to greet.
    OutgoingDialing,
    CallAnswered,
    CallTerminated,
    AudioConnected {
        codec: String,
        sample_rate: u16,
        /// false = SCO up but iso pipes did not arm — silent call (AOK-HW-001).
        armed: bool,
    },
    AudioDisconnected,
    CallerId(String),
    /// Phase 4: a SECOND caller is knocking while a call is active
    /// (call-waiting-negotiated connections only). One per waiting episode
    /// plus one upgrade when the number arrives after an anonymous start.
    CallWaiting {
        number: Option<String>,
    },
    /// The waiting episode ended with the active call untouched.
    CallWaitingEnded,
    /// `callheld` indicator transition (0 none / 1 held+active / 2 held only).
    CallHeld {
        state: i32,
    },
    /// Phase 3e: PBAP fetch finished. Payload is a flattened list of
    /// (phone_number, display_name) pairs — one row per number, so a
    /// vCard with multiple TEL fields produces multiple entries.
    /// Consumers normalize phone_number with
    /// `crate::database::normalize_number` before storing or looking
    /// up.
    ContactsFetched(Vec<ContactPair>),
    /// Phase 4e: NotificationRegistration ack from the AG. Diagnostic
    /// only — UI doesn't act on it directly.
    MapNotificationsSubscribed,
    /// Phase 4e: a new SMS arrived. Sender / body parsed out of the
    /// bMessage envelope the runtime fetched via MAP MAS.
    SmsReceived(SmsReceivedPayload),
    /// Phase 4e: a SendSms call we sent to the runtime was acked by
    /// the AG.
    SmsSent {
        recipient_phone: String,
    },
    /// An outbound SMS was abandoned (MAS PUT failed / aged out across
    /// recovery cycles) — surfaced so the host can record the truth
    /// instead of a silent loss.
    SmsSendFailed {
        recipient_phone: String,
        reason: String,
    },
    /// PAIR-001: SSP numeric comparison held for the operator — both the
    /// phone and the Desktop UI show `numeric_value`; the operator answers
    /// via `confirm_pairing`.
    PairingConfirmRequired {
        address: String,
        numeric_value: u32,
    },
    Error(String),
}

/// (number, name) pair shipped to the Tauri side after a PBAP fetch.
/// JSON shape: `{"phone_number": "+15551234567", "display_name": "Alice"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPair {
    pub phone_number: String,
    pub display_name: String,
}

/// Phase 4e: a new SMS the runtime fetched off the back of an MNS
/// notification. JSON shape mirrors the field names so the Tauri side
/// can deserialize directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsReceivedPayload {
    pub sender_phone: String,
    pub sender_name: Option<String>,
    pub body: String,
    pub handle: String,
    pub msg_type: Option<String>,
}

/// Audio data received from the phone.
#[derive(Debug, Clone)]
pub struct AudioData {
    pub samples: Vec<i16>,
    pub sample_rate: u16,
}

#[cfg(target_os = "windows")]
fn aokie_pairing_store_path() -> std::path::PathBuf {
    use aokie_bluetooth::aokie_radio::pairing_store::default_store_path;
    let dir = aokie_core::paths::app_data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    default_store_path(&dir)
}

/// Bluetooth HFP manager — thin wrapper over `aokie_radio::AokieRuntime`.
#[cfg(target_os = "windows")]
pub struct BluetoothManager {
    runtime: aokie_bluetooth::aokie_radio::runtime::AokieRuntime,
}

#[cfg(target_os = "windows")]
impl BluetoothManager {
    pub fn new() -> Result<Self, String> {
        Self::new_with_preferred_dongle(None)
    }

    /// Variant that lets the caller pin which dongle the runtime
    /// opens. `None` keeps the original "first enumerated HCI
    /// interface" behaviour. `Some(path)` asks the runtime to match
    /// that exact device path; an unmatched preference falls back to
    /// enumeration-first with a log line so a moved-port dongle
    /// still works without a manual reset.
    pub fn new_with_preferred_dongle(preferred_path: Option<String>) -> Result<Self, String> {
        eprintln!(
            "[Bluetooth] Using aokie_radio backend (preferred dongle: {})",
            preferred_path.as_deref().unwrap_or("<enumeration-first>"),
        );
        let runtime = aokie_bluetooth::aokie_radio::runtime::AokieRuntime::start_with_options(
            aokie_pairing_store_path(),
            preferred_path,
        )
        .map_err(|e| format!("aokie_radio runtime: {}", e))?;
        Ok(Self { runtime })
    }

    /// Non-blocking poll for the next runtime event. The aokie runtime
    /// only exposes a try-style channel — there is no awaiting variant.
    /// Callers that need to wait on events should poll this from a tick
    /// task. We deliberately don't expose an async `recv_event` shim:
    /// any such shim would be obliged to return immediately (no
    /// underlying signal to wait on) and a future caller would
    /// silently busy-loop.
    pub fn try_recv_event(&mut self) -> Option<BluetoothEvent> {
        self.runtime
            .try_recv_event()
            .map(bluetooth_event_from_runtime)
    }

    /// Non-blocking poll for the next captured SCO audio frame. Same
    /// "poll, don't await" contract as `try_recv_event` above.
    pub fn try_recv_audio(&mut self) -> Option<AudioData> {
        self.runtime.try_recv_audio().map(|frame| AudioData {
            samples: frame.samples,
            sample_rate: frame.sample_rate,
        })
    }

    pub fn send_audio(&self, samples: &[i16]) {
        if let Err(e) = self.runtime.send_audio(samples.to_vec()) {
            eprintln!("[BTManager] aokie send_audio failed: {}", e);
        }
    }

    /// Drop any TTS bytes already queued for SCO transmission. Used by
    /// "Take Over Call" so the bot goes silent immediately rather than
    /// continuing through the buffered tail of its current sentence.
    pub fn flush_tx_audio(&self) {
        if let Err(e) = self.runtime.flush_tx_audio() {
            eprintln!("[BTManager] aokie flush_tx_audio failed: {}", e);
        }
    }

    /// Phase 4e: send an SMS via MAP MAS PushMessage. The bMessage
    /// envelope is built inside the runtime so callers only need to
    /// provide the recipient and body.
    ///
    /// `msg_type` should match what the inbound `SmsReceived`
    /// reported when this is an auto-reply, so the outbound is
    /// built as MMS for an MMS-typed inbound (Pixel routes that via
    /// its MMS dispatcher, which transparently upgrades to RCS for
    /// RCS-capable recipients — keeping the reply in the original
    /// thread). For manual operator sends, pass `None` and the
    /// runtime falls back to plain SMS_GSM.
    pub fn send_sms(
        &self,
        recipient_phone: String,
        body: String,
        msg_type: Option<String>,
    ) -> Result<(), String> {
        self.runtime.send_sms(recipient_phone, body, msg_type)
    }

    pub fn answer_call(&self) -> Result<(), String> {
        self.runtime.answer()
    }

    pub fn reject_call(&self) -> Result<(), String> {
        self.runtime.reject_or_hangup()
    }

    pub fn hangup(&self) -> Result<(), String> {
        self.runtime.reject_or_hangup()
    }

    /// Phase 2: place an OUTBOUND voice call via HFP `ATD<number>;`.
    /// Progress arrives on the normal event stream (OutgoingDialing →
    /// CallRinging → CallAnswered/CallTerminated) — the AG's indicator
    /// stream stays the single source of call-state truth.
    pub fn dial(&self, number: String) -> Result<(), String> {
        self.runtime.dial(number)
    }

    /// AOK-BT-001: open a bounded, discoverable pairing window for `seconds`.
    /// At rest the radio is connectable-only, so an unknown phone can only
    /// pair while this window is open.
    pub fn open_pairing_window(&self, seconds: u64) {
        self.runtime.open_pairing_window(seconds);
    }

    /// AOK-BT-001: close the pairing window now (cancel / done).
    pub fn close_pairing_window(&self) {
        self.runtime.close_pairing_window();
    }

    /// AOK-BT-001: seconds left in the pairing window (0 = closed).
    pub fn pairing_window_remaining_secs(&self) -> u64 {
        self.runtime.pairing_window_remaining_secs()
    }

    /// AOK-BT-001: a clone of the shared pairing window for lock-free status reads.
    pub fn pairing_window(&self) -> aokie_bluetooth::aokie_radio::runtime::PairingWindow {
        self.runtime.pairing_window()
    }

    /// AOK-BT-001: bonded devices' addresses (revocable identities).
    pub fn bonded_addresses(&self) -> Vec<String> {
        self.runtime.bonded_addresses()
    }

    /// Bonded devices as (address, captured friendly name) — the "paired
    /// phones" list disambiguates multiple phones by model.
    pub fn bonded_devices(&self) -> Vec<(String, Option<String>)> {
        self.runtime
            .bonded_devices()
            .into_iter()
            .map(|d| (d.address, d.name))
            .collect()
    }

    /// The connected phone's captured friendly name/model, if known yet.
    pub fn connected_name(&self) -> Option<String> {
        self.runtime.connected_name()
    }

    /// AOK-BT-001: forget a bonded device so it can no longer reconnect
    /// without pairing again. Disconnects an active session for that device
    /// first (PAIR-001). Returns whether a link key was removed.
    pub fn remove_paired(&self, address: &str) -> Result<bool, String> {
        self.runtime.remove_paired(address.to_string())
    }

    /// Disconnect the connected phone but KEEP the bond (unlike Forget) —
    /// clears a wedged link; the phone reconnects. Returns whether a live
    /// link for `address` was actually disconnected.
    pub fn disconnect(&self, address: &str) -> Result<bool, String> {
        self.runtime.disconnect(address.to_string())
    }

    /// HARD-001: reconnect a bonded phone from OUR side (page + outbound
    /// HFP setup). `Ok(true)` = attempt started; the phone.connected event
    /// is the authoritative outcome. `Ok(false)` = already connected.
    pub fn connect(&self, address: &str) -> Result<bool, String> {
        self.runtime.connect(address.to_string())
    }

    /// PAIR-001: a clone of the shared pending-confirmation slot for
    /// lock-free `phone.status` reads.
    pub fn pairing_confirm_slot(&self) -> PairingConfirmSlot {
        self.runtime.pairing_confirm_slot()
    }

    /// PAIR-001: resolve the held SSP numeric comparison for `address`.
    pub fn confirm_pairing(&self, address: &str, accept: bool) -> Result<(), String> {
        self.runtime.confirm_pairing(address.to_string(), accept)
    }

    pub fn is_initialized(&self) -> bool {
        self.runtime.is_initialized()
    }

    pub fn is_connected(&self) -> bool {
        self.runtime.is_connected()
    }

    pub fn is_call_active(&self) -> bool {
        self.runtime.is_call_active()
    }

    /// Current SCO sample rate (8000 for CVSD, 16000 for mSBC).
    pub fn get_sample_rate(&self) -> u16 {
        self.runtime.sample_rate()
    }

    pub fn get_local_address(&self) -> String {
        self.runtime.local_address()
    }

    pub fn get_remote_address(&self) -> String {
        self.runtime.remote_address()
    }

    /// SCO RX frames the runtime had to drop because the downstream
    /// audio channel was full. A non-zero / climbing counter means the
    /// AI pipeline (Whisper / Gemma / TTS) couldn't keep up with
    /// real-time SCO and audio is going to disk-of-memory rather than
    /// to the model. Surfaced in the UI so an operator notices the
    /// pressure before the call experience degrades.
    pub fn dropped_audio_frames(&self) -> u64 {
        self.runtime.dropped_audio_frames()
    }

    /// True when the radio thread missed its clean-shutdown
    /// deadline (`SHUTDOWN_DEADLINE` in `aokie_radio::runtime`) and
    /// we forced the join. The JoinHandle is gone; the
    /// runtime is in an "abandoned" state where another initialize
    /// call is the only safe path forward. Surfaced so the UI can
    /// warn the operator instead of silently letting them try to
    /// answer a call against a dead radio thread.
    pub fn shutdown_timed_out(&self) -> bool {
        self.runtime.shutdown_timed_out()
    }

    /// Cumulative SCO TX stream-reset count (Win32 error 87 → re-arm)
    /// since process start. A handful per call is normal at TTS-pause
    /// boundaries; a steady climb during active speech indicates SCO
    /// TX FIFO underruns and is the most reliable in-app signal that
    /// the dongle's iso writes are clicking. Always 0 on Linux — the
    /// libusb iso path doesn't have a Win32-87 analogue.
    pub fn sco_tx_stream_resets(&self) -> u64 {
        aokie_bluetooth::aokie_radio::sco_tx_stream_resets()
    }
}

#[cfg(target_os = "windows")]
impl Drop for BluetoothManager {
    fn drop(&mut self) {
        self.runtime.shutdown();
    }
}

#[cfg(target_os = "windows")]
fn bluetooth_event_from_runtime(
    event: aokie_bluetooth::aokie_radio::runtime::RuntimeEvent,
) -> BluetoothEvent {
    use aokie_bluetooth::aokie_radio::runtime::RuntimeEvent;
    match event {
        RuntimeEvent::Initialized(addr) => BluetoothEvent::Initialized(addr),
        RuntimeEvent::DeviceConnected(addr) => BluetoothEvent::DeviceConnected(addr),
        RuntimeEvent::DeviceDisconnected(addr) => BluetoothEvent::DeviceDisconnected(addr),
        RuntimeEvent::CallIncoming => BluetoothEvent::CallIncoming,
        RuntimeEvent::CallRinging => BluetoothEvent::CallRinging,
        RuntimeEvent::OutgoingDialing => BluetoothEvent::OutgoingDialing,
        RuntimeEvent::CallAnswered => BluetoothEvent::CallAnswered,
        RuntimeEvent::CallTerminated => BluetoothEvent::CallTerminated,
        RuntimeEvent::AudioConnected { codec, sample_rate, armed } => {
            BluetoothEvent::AudioConnected { codec, sample_rate, armed }
        }
        RuntimeEvent::AudioDisconnected => BluetoothEvent::AudioDisconnected,
        RuntimeEvent::CallerId(num) => BluetoothEvent::CallerId(num),
        RuntimeEvent::CallWaiting { number } => BluetoothEvent::CallWaiting { number },
        RuntimeEvent::CallWaitingEnded => BluetoothEvent::CallWaitingEnded,
        RuntimeEvent::CallHeld { state } => BluetoothEvent::CallHeld { state },
        RuntimeEvent::PbapContactsFetched(contacts) => BluetoothEvent::ContactsFetched(
            contacts
                .into_iter()
                .map(|c| ContactPair {
                    phone_number: c.phone_number,
                    display_name: c.display_name,
                })
                .collect(),
        ),
        RuntimeEvent::MapNotificationsSubscribed => BluetoothEvent::MapNotificationsSubscribed,
        RuntimeEvent::SmsReceived {
            sender_phone,
            sender_name,
            body,
            handle,
            msg_type,
        } => BluetoothEvent::SmsReceived(SmsReceivedPayload {
            sender_phone,
            sender_name,
            body,
            handle,
            msg_type,
        }),
        RuntimeEvent::SmsSent { recipient_phone } => BluetoothEvent::SmsSent { recipient_phone },
        RuntimeEvent::SmsSendFailed {
            recipient_phone,
            reason,
        } => BluetoothEvent::SmsSendFailed {
            recipient_phone,
            reason,
        },
        RuntimeEvent::PairingConfirmRequired {
            address,
            numeric_value,
        } => BluetoothEvent::PairingConfirmRequired {
            address,
            numeric_value,
        },
        RuntimeEvent::Error(e) => BluetoothEvent::Error(e),
    }
}

#[cfg(target_os = "windows")]
pub fn bluetooth_event_from_aokie_hfp(
    event: aokie_bluetooth::aokie_radio::hfp::HfpEvent,
) -> Option<BluetoothEvent> {
    match event {
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::ServiceLevelConnectionReady => None,
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::ServiceLevelConnectionFailed(reason) => Some(
            BluetoothEvent::Error(format!("HFP SLC failed at {}", reason)),
        ),
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::IncomingCall => Some(BluetoothEvent::CallIncoming),
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::Ringing => Some(BluetoothEvent::CallRinging),
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::OutgoingDialing => {
            Some(BluetoothEvent::OutgoingDialing)
        }
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallAnswered => Some(BluetoothEvent::CallAnswered),
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallTerminated => Some(BluetoothEvent::CallTerminated),
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallerId(number) => {
            Some(BluetoothEvent::CallerId(number))
        }
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallWaiting(number) => {
            Some(BluetoothEvent::CallWaiting { number })
        }
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallWaitingEnded => {
            Some(BluetoothEvent::CallWaitingEnded)
        }
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallHeld(state) => {
            Some(BluetoothEvent::CallHeld { state })
        }
        // Observe-only topology entries stay a radio-log concern for now
        // (the switchboard slice will consume them).
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallListEntry(_) => None,
        aokie_bluetooth::aokie_radio::hfp::HfpEvent::CodecSelected { codec, sample_rate } => {
            // Codec selection precedes iso arming; this legacy path never
            // observed an arming failure, so it reports armed.
            Some(BluetoothEvent::AudioConnected { codec, sample_rate, armed: true })
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn maps_aokie_hfp_events_to_existing_bluetooth_events() {
        assert_eq!(
            bluetooth_event_from_aokie_hfp(aokie_bluetooth::aokie_radio::hfp::HfpEvent::IncomingCall),
            Some(BluetoothEvent::CallIncoming)
        );
        assert_eq!(
            bluetooth_event_from_aokie_hfp(aokie_bluetooth::aokie_radio::hfp::HfpEvent::CallerId(
                "+15551234567".to_string()
            )),
            Some(BluetoothEvent::CallerId("+15551234567".to_string()))
        );
        assert_eq!(
            bluetooth_event_from_aokie_hfp(aokie_bluetooth::aokie_radio::hfp::HfpEvent::CodecSelected {
                codec: "mSBC".to_string(),
                sample_rate: 16000,
            }),
            Some(BluetoothEvent::AudioConnected {
                codec: "mSBC".to_string(),
                sample_rate: 16000,
                armed: true,
            })
        );
        assert_eq!(
            bluetooth_event_from_aokie_hfp(
                aokie_bluetooth::aokie_radio::hfp::HfpEvent::ServiceLevelConnectionReady
            ),
            None
        );
    }
}
