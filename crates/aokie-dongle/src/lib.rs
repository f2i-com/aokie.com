//! # aokie-dongle
//!
//! USB-dongle setup for Aokie, lifted verbatim out of the legacy Tauri
//! app (`aokie-desktop/src-tauri/src`). It handles device enumeration,
//! the WinUSB driver package (self-signed cat + install), the PKI that
//! makes Win10/11 accept the package without an EV cert, the Linux
//! udev-rule counterpart, and the operator-facing dongle-preference
//! bookkeeping.
//!
//! Depends on [`aokie_core`] (redaction, paths, dongle catalog) and on
//! [`aokie_bluetooth`] — the `bluetooth` bridge module adapts
//! `aokie_bluetooth::aokie_radio` events into the higher-level
//! [`bluetooth::BluetoothEvent`]. No Tauri dependency.
//!
//! ## Layout
//!
//! * [`aokie_dongle`] (Windows) — enumeration + WinUSB package + PKI.
//!   Its public surface is also re-exported at the crate root, so
//!   `aokie_dongle::find_device` / `aokie_dongle::winusb` resolve
//!   without repeating the module name.
//! * `aokie_dongle_linux` (Linux) — udev rule emission.
//! * [`bluetooth`] (Windows) — radio→app event bridge + preferred-dongle
//!   selection.
//! * [`bluetooth_prefs`] — cross-platform dongle-preference store.

#[cfg(target_os = "windows")]
pub mod aokie_dongle;
// Re-export the dongle module's public API at the crate root so callers
// (and the bundled bins) write `aokie_dongle::find_device` rather than
// the doubled `aokie_dongle::aokie_dongle::find_device`.
#[cfg(target_os = "windows")]
pub use aokie_dongle::*;

// Linux counterpart — udev rules; libusb talks straight to /dev/bus/usb
// so there's no driver-install work to do.
#[cfg(target_os = "linux")]
pub mod aokie_dongle_linux;

#[cfg(target_os = "windows")]
pub mod bluetooth;

pub mod bluetooth_prefs;
