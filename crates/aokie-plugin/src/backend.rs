//! `RadioBackend` — the transport seam between the plugin's voice pipeline
//! (the `radio` module, transport-ignorant) and a phone-link engine. Two backends:
//!
//! - [`UsbRadioBackend`] — the WinUSB dongle runtime (full AT/HFP control:
//!   switchboard, call waiting, codec forcing). Today this is the only
//!   feature-complete backend.
//! - [`NativeRadioBackend`] — the built-in Windows Bluetooth stack via
//!   `aokie-winbt` (no driver install; call control via the WinRT Calls API,
//!   audio via the Hands-Free WASAPI endpoints, SMS via MAP-over-RFCOMM).
//!
//! Selection rides the `transportMode` setting (`dongle` default / `native` /
//! `auto`), applied at radio start like `hfpCodec`.

use aokie_dongle::bluetooth::{AudioData, BluetoothEvent, PairingConfirmSlot, PairingWindow};

/// Which phone-link transport the radio starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// WinUSB dongle (today's behaviour; the only mode with the switchboard).
    Dongle,
    /// Built-in Windows Bluetooth stack — no driver install.
    Native,
    /// Prefer native when a Windows Bluetooth adapter is present, else dongle.
    Auto,
}

impl TransportMode {
    pub fn from_setting(value: Option<&serde_json::Value>) -> Self {
        match value
            .and_then(|v| v.as_str())
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("native") => Self::Native,
            Some("auto") => Self::Auto,
            _ => Self::Dongle,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Dongle => "dongle",
            Self::Native => "native",
            Self::Auto => "auto",
        }
    }
}

/// The backend surface the radio's run_loop drives. Mirrors
/// `aokie_dongle::bluetooth::BluetoothManager`; backends answer honestly for
/// capabilities their transport lacks (e.g. native `hold_swap` is a stub
/// until Windows-side hold semantics are proven).
pub trait RadioBackend: Send {
    fn try_recv_event(&mut self) -> Option<BluetoothEvent>;
    fn try_recv_audio(&mut self) -> Option<AudioData>;
    fn send_audio(&self, samples: &[i16]) -> bool;
    fn flush_tx_audio(&self);
    fn send_sms(
        &self,
        message_id: String,
        recipient_phone: String,
        body: String,
        msg_type: Option<String>,
    ) -> Result<(), String>;
    fn answer_call(&self) -> Result<(), String>;
    fn reject_call(&self) -> Result<(), String>;
    fn hangup(&self) -> Result<(), String>;
    fn dial(&self, number: String) -> Result<(), String>;
    fn hold_swap(&self) -> Result<(), String>;
    fn query_calls(&self) -> Result<(), String>;
    /// `AT+BCC` audio self-heal for an active call with no SCO. Backends
    /// where the OS owns call audio (native Windows stack) keep the default
    /// refusal — their dead-air watchdog is disabled anyway.
    fn codec_connect(&self) -> Result<(), String> {
        Err("codec connection nudge is not supported on this backend".to_string())
    }
    fn open_pairing_window(&self, seconds: u64);
    fn close_pairing_window(&self);
    fn pairing_window(&self) -> PairingWindow;
    fn pairing_confirm_slot(&self) -> PairingConfirmSlot;
    fn bonded_devices(&self) -> Vec<(String, Option<String>)>;
    fn connected_name(&self) -> Option<String>;
    fn remove_paired(&self, address: &str) -> Result<bool, String>;
    fn disconnect(&self, address: &str) -> Result<bool, String>;
    fn connect(&self, address: &str) -> Result<bool, String>;
    fn confirm_pairing(&self, address: &str, accept: bool) -> Result<(), String>;
    fn is_connected(&self) -> bool;
    fn get_sample_rate(&self) -> u16;
    /// Whether this backend can expose bidirectional phone-call PCM to the
    /// plugin. Desktop Realtime replaces the responder, not this physical
    /// audio transport, so it must never auto-answer on a control-only link.
    fn realtime_call_audio_supported(&self) -> bool;
    /// Whether the no-SCO dead-air watchdog (an answered call with no audio
    /// channel for one continuous 8s window gets hung up) applies to this
    /// transport. It is an SCO-link safety net: on the native Windows-stack
    /// transport the call's audio path is owned by Windows (and may sit
    /// entirely outside our WASAPI pump), so ending the call is wrong.
    fn sco_dead_air_watchdog(&self) -> bool;
    /// Short human label for logs and health ("WinUSB dongle" / "Windows native Bluetooth").
    fn backend_name(&self) -> &'static str;
}

/// WinUSB dongle backend — the proven full-control transport.
#[cfg(target_os = "windows")]
pub struct UsbRadioBackend {
    inner: aokie_dongle::bluetooth::BluetoothManager,
}

#[cfg(target_os = "windows")]
impl UsbRadioBackend {
    pub fn new(preferred_path: Option<String>) -> Result<Self, String> {
        Ok(Self {
            inner: aokie_dongle::bluetooth::BluetoothManager::new_with_preferred_dongle(
                preferred_path,
            )?,
        })
    }
}

#[cfg(target_os = "windows")]
impl RadioBackend for UsbRadioBackend {
    fn try_recv_event(&mut self) -> Option<BluetoothEvent> {
        self.inner.try_recv_event()
    }
    fn try_recv_audio(&mut self) -> Option<AudioData> {
        self.inner.try_recv_audio()
    }
    fn send_audio(&self, samples: &[i16]) -> bool {
        self.inner.send_audio(samples)
    }
    fn flush_tx_audio(&self) {
        self.inner.flush_tx_audio();
    }
    fn send_sms(
        &self,
        message_id: String,
        recipient_phone: String,
        body: String,
        msg_type: Option<String>,
    ) -> Result<(), String> {
        self.inner
            .send_sms(message_id, recipient_phone, body, msg_type)
    }
    fn answer_call(&self) -> Result<(), String> {
        self.inner.answer_call()
    }
    fn reject_call(&self) -> Result<(), String> {
        self.inner.reject_call()
    }
    fn hangup(&self) -> Result<(), String> {
        self.inner.hangup()
    }
    fn dial(&self, number: String) -> Result<(), String> {
        self.inner.dial(number)
    }
    fn hold_swap(&self) -> Result<(), String> {
        self.inner.hold_swap()
    }
    fn query_calls(&self) -> Result<(), String> {
        self.inner.query_calls()
    }
    fn open_pairing_window(&self, seconds: u64) {
        self.inner.open_pairing_window(seconds);
    }
    fn close_pairing_window(&self) {
        self.inner.close_pairing_window();
    }
    fn pairing_window(&self) -> PairingWindow {
        self.inner.pairing_window()
    }
    fn pairing_confirm_slot(&self) -> PairingConfirmSlot {
        self.inner.pairing_confirm_slot()
    }
    fn bonded_devices(&self) -> Vec<(String, Option<String>)> {
        self.inner.bonded_devices()
    }
    fn connected_name(&self) -> Option<String> {
        self.inner.connected_name()
    }
    fn remove_paired(&self, address: &str) -> Result<bool, String> {
        self.inner.remove_paired(address)
    }
    fn disconnect(&self, address: &str) -> Result<bool, String> {
        self.inner.disconnect(address)
    }
    fn connect(&self, address: &str) -> Result<bool, String> {
        self.inner.connect(address)
    }
    fn confirm_pairing(&self, address: &str, accept: bool) -> Result<(), String> {
        self.inner.confirm_pairing(address, accept)
    }
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
    fn get_sample_rate(&self) -> u16 {
        self.inner.get_sample_rate()
    }
    fn sco_dead_air_watchdog(&self) -> bool {
        true
    }
    fn realtime_call_audio_supported(&self) -> bool {
        true
    }
    fn codec_connect(&self) -> Result<(), String> {
        self.inner.codec_connect()
    }
    fn backend_name(&self) -> &'static str {
        "WinUSB dongle"
    }
}

/// Native Windows-stack backend — the no-driver-install transport.
#[cfg(target_os = "windows")]
pub struct NativeRadioBackend {
    inner: aokie_winbt::NativeBtRuntime,
}

#[cfg(target_os = "windows")]
impl NativeRadioBackend {
    pub fn start() -> Result<Self, String> {
        Ok(Self {
            inner: aokie_winbt::NativeBtRuntime::start()?,
        })
    }
}

#[cfg(target_os = "windows")]
impl RadioBackend for NativeRadioBackend {
    fn try_recv_event(&mut self) -> Option<BluetoothEvent> {
        self.inner.try_recv_event()
    }
    fn try_recv_audio(&mut self) -> Option<AudioData> {
        self.inner.try_recv_audio()
    }
    fn send_audio(&self, samples: &[i16]) -> bool {
        self.inner.send_audio(samples)
    }
    fn flush_tx_audio(&self) {
        self.inner.flush_tx_audio();
    }
    fn send_sms(
        &self,
        message_id: String,
        recipient_phone: String,
        body: String,
        msg_type: Option<String>,
    ) -> Result<(), String> {
        self.inner
            .send_sms(message_id, recipient_phone, body, msg_type)
    }
    fn answer_call(&self) -> Result<(), String> {
        self.inner.answer_call()
    }
    fn reject_call(&self) -> Result<(), String> {
        self.inner.reject_call()
    }
    fn hangup(&self) -> Result<(), String> {
        self.inner.hangup()
    }
    fn dial(&self, number: String) -> Result<(), String> {
        self.inner.dial(number)
    }
    fn hold_swap(&self) -> Result<(), String> {
        self.inner.hold_swap()
    }
    fn query_calls(&self) -> Result<(), String> {
        self.inner.query_calls()
    }
    fn open_pairing_window(&self, seconds: u64) {
        self.inner.open_pairing_window(seconds);
    }
    fn close_pairing_window(&self) {
        self.inner.close_pairing_window();
    }
    fn pairing_window(&self) -> PairingWindow {
        self.inner.pairing_window()
    }
    fn pairing_confirm_slot(&self) -> PairingConfirmSlot {
        self.inner.pairing_confirm_slot()
    }
    fn bonded_devices(&self) -> Vec<(String, Option<String>)> {
        self.inner.bonded_devices()
    }
    fn connected_name(&self) -> Option<String> {
        self.inner.connected_name()
    }
    fn remove_paired(&self, address: &str) -> Result<bool, String> {
        self.inner.remove_paired(address)
    }
    fn disconnect(&self, address: &str) -> Result<bool, String> {
        self.inner.disconnect(address)
    }
    fn connect(&self, address: &str) -> Result<bool, String> {
        self.inner.connect(address)
    }
    fn confirm_pairing(&self, address: &str, accept: bool) -> Result<(), String> {
        self.inner.confirm_pairing(address, accept)
    }
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
    fn get_sample_rate(&self) -> u16 {
        self.inner.get_sample_rate()
    }
    fn sco_dead_air_watchdog(&self) -> bool {
        false
    }
    fn realtime_call_audio_supported(&self) -> bool {
        // Windows 11 25H2 on the supported field host has no HFP-HF service
        // or usable hands-free endpoint. Native mode is call-control only.
        false
    }
    fn backend_name(&self) -> &'static str {
        "Windows native Bluetooth"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_mode_parses_setting_values() {
        assert_eq!(TransportMode::from_setting(None), TransportMode::Dongle);
        assert_eq!(
            TransportMode::from_setting(Some(&serde_json::json!("native"))),
            TransportMode::Native
        );
        assert_eq!(
            TransportMode::from_setting(Some(&serde_json::json!(" Auto "))),
            TransportMode::Auto
        );
        assert_eq!(
            TransportMode::from_setting(Some(&serde_json::json!("dongle"))),
            TransportMode::Dongle
        );
        // Unknown values fall back to the proven transport, never native.
        assert_eq!(
            TransportMode::from_setting(Some(&serde_json::json!("bluetooth"))),
            TransportMode::Dongle
        );
        assert_eq!(TransportMode::Auto.as_str(), "auto");
        assert_eq!(TransportMode::Native.as_str(), "native");
    }
}
