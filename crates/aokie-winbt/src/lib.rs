//! aokie-winbt — native Windows Bluetooth backend for the Aokie radio.
//!
//! Drives the built-in Windows Bluetooth stack (WinRT Calls API for call
//! control, WASAPI Hands-Free endpoints for audio, WinRT RFCOMM for MAP/PBAP)
//! with stock Bluetooth drivers — no WinUSB dongle binding. Sits beside the
//! WinUSB backend behind the plugin's `RadioBackend` seam.
//!
//! SCOPE (field-verified 2026-07-20, see
//! `docs/NATIVE_BLUETOOTH_TRANSPORT_PLAN.md`): provides SMS (MAP), contacts
//! (PBAP), pairing, and call CONTROL (ring/answer/end events) on Windows
//! builds where the hands-free-unit service exists. **It cannot carry call
//! audio on Windows 11 25H2** — that build ships no `BthHFSrv.dll`, SCO audio
//! is kernel-only, and LE Audio telephony does not engage — so the WinUSB
//! backend remains the only supported transport for calls, and the settings
//! UI does not offer this mode.

#![cfg_attr(not(target_os = "windows"), allow(unused))]

#[cfg(target_os = "windows")]
pub(crate) mod audio;
#[cfg(target_os = "windows")]
pub(crate) mod calls;
#[cfg(target_os = "windows")]
pub mod hfp_enable;
#[cfg(target_os = "windows")]
pub(crate) mod map;
#[cfg(target_os = "windows")]
pub mod obex_probe;
#[cfg(target_os = "windows")]
pub mod pairing;
#[cfg(target_os = "windows")]
pub(crate) mod pbap;
#[cfg(target_os = "windows")]
pub mod probe;
#[cfg(target_os = "windows")]
pub(crate) mod rfcomm;
#[cfg(target_os = "windows")]
pub mod runtime;
#[cfg(target_os = "windows")]
pub mod sms_tool;
#[cfg(target_os = "windows")]
pub(crate) mod worker;

#[cfg(target_os = "windows")]
pub use runtime::NativeBtRuntime;

/// MAP MAS (Message Access Profile server) short UUID on the phone.
pub const MAP_MAS_SHORT_UUID: u16 = 0x1132;
/// MAP MNS (notification server) short UUID we advertise on the PC.
pub const MAP_MNS_SHORT_UUID: u16 = 0x1133;
/// PBAP PSE (phonebook server) short UUID on the phone.
pub const PBAP_PSE_SHORT_UUID: u16 = 0x112F;
