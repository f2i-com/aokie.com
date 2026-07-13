// Bluetooth host stack — works on Windows (via WinUSB) and Linux (via
// libusb / rusb). The portable code (HCI/L2CAP/RFCOMM/SDP/HFP/SCO/OBEX/
// MAP/PBAP framing + state machines) is the bulk of this module and
// builds on any host. Only the transport layer is OS-specific:
//
//   * `winusb` — Windows. Talks to the dongle through WinUSB.
//   * `libusb` — Linux. Talks to the dongle through libusb-1.0 / rusb.
//   * `transport` — facade that re-exports the right one per cfg.
//
// Other targets (macOS / non-USB hosts) are not in scope; the module
// gate below excludes them so we never accidentally try to compile
// the transport stubs against an OS that has neither.
#![cfg(any(target_os = "windows", target_os = "linux"))]

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
pub mod rfcomm;
pub mod runtime;
pub mod sco;
pub mod sco_dump;
pub mod sdp;
pub mod sdp_client;
pub mod vcard;

#[cfg(target_os = "windows")]
pub mod winusb;

#[cfg(target_os = "linux")]
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
