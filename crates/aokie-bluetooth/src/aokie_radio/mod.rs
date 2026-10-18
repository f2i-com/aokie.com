// Bluetooth host stack — works on Windows (via WinUSB) and on Linux and
// macOS (via libusb / rusb). The portable code (HCI/L2CAP/RFCOMM/SDP/HFP/
// SCO/OBEX/MAP/PBAP framing + state machines) is the bulk of this module
// and builds on any host. Only the transport layer is OS-specific:
//
//   * `winusb` — Windows. Talks to the dongle through WinUSB.
//   * `libusb` — Linux and macOS. Talks to the dongle through libusb-1.0
//     / rusb (on macOS libusb is built into the program).
//   * `transport` — facade that re-exports the right one per cfg.
//
// Other targets (hosts with neither) are not in scope; the module gate
// below excludes them so we never accidentally try to compile the
// transport against an OS that has no raw USB path.
#![cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]

pub mod bmessage;
pub mod hci;
pub mod hfp;
pub mod hfp_client;
pub mod hfp_connect;
pub mod l2cap;
pub mod manager;
pub mod map_listing;
pub mod map_mas;
pub mod map_mns;
pub mod map_runtime;
pub mod obex;
pub mod pairing_store;
pub mod pbap;
pub mod pbap_runtime;
pub mod realtek;
pub mod rfcomm;
pub mod runtime;
pub mod sco;
pub mod sco_dump;
pub mod sdp;
pub mod sdp_client;
pub(crate) mod transport_recovery;
pub mod usb_hci;
pub mod vcard;

#[cfg(target_os = "windows")]
pub mod winusb;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod libusb;

pub mod transport;

/// Cross-platform accessor for the SCO TX stream-reset counter.
/// On Windows this returns the Win32-87 → ContinueStream=FALSE re-arm
/// count from the WinUSB iso TX path. On Linux there is no analogous
/// per-iso-transfer "stream restart" failure mode (libusb iso transfers
/// don't carry a ContinueStream flag), so we report zero — the libusb
/// path uses URB-cancellation telemetry that has no Win32-87 analogue.
pub fn sco_tx_stream_resets() -> u64 {
    #[cfg(target_os = "windows")]
    {
        winusb::sco_tx_stream_resets()
    }
    #[cfg(not(target_os = "windows"))]
    {
        0
    }
}

/// Whether the radio logs each step of its routine traffic (every OBEX
/// answer of a message-folder session, every AT+CIND? keepalive):
/// `AOKIE_RADIO_DEBUG_LOGS=1`. Off, those come as summaries (review 4 #16:
/// an idle phone filled OAIY's 1,000-line log ring in 20-30 minutes).
pub fn debug_logs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("AOKIE_RADIO_DEBUG_LOGS")
            .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
            .unwrap_or(false)
    })
}

/// A Bluetooth address for the log (review 4 #30): its last two bytes
/// ("…:5E:F1"), enough to tell two phones apart in one log, not to trace a
/// device. In full under `AOKIE_VERBOSE_LOGS=1`.
pub struct LogAddr<'a>(pub &'a str);

impl std::fmt::Display for LogAddr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if aokie_core::redact::verbose_logs_active() {
            return f.write_str(self.0);
        }
        let parts: Vec<&str> = self.0.split(':').collect();
        if parts.len() == 6 {
            write!(f, "…:{}:{}", parts[4], parts[5])
        } else {
            write!(f, "<address, {} chars>", self.0.chars().count())
        }
    }
}
