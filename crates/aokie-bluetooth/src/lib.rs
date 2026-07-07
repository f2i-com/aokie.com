//! # aokie-bluetooth
//!
//! Aokie's from-scratch Bluetooth stack, lifted verbatim out of the
//! legacy Tauri app (`aokie-desktop/src-tauri/src`). It owns the USB
//! transport (WinUSB on Windows, libusb on Linux), the HCI / L2CAP /
//! RFCOMM / SDP core, the HFP + SCO voice path, and the MAP / PBAP
//! messaging + phonebook profiles — plus the pure-Rust mSBC codec used
//! for HFP wide-band speech.
//!
//! The only Aokie dependency is [`aokie_core`] (redaction + atomic
//! file writes). There is no Tauri dependency: this crate is pure
//! protocol + transport code.
//!
//! ## Layout
//!
//! * [`aokie_radio`] — the transport + protocol stack. Compiles on
//!   Windows (WinUSB) and Linux (libusb); the module self-gates via an
//!   inner `#![cfg(any(windows, linux))]`, so the parsers/state
//!   machines stay reachable (and testable) on both.
//! * [`msbc`] — platform-agnostic mSBC encoder/decoder + H2 framing,
//!   unit-testable on any host.

// mSBC is cross-platform so its wire-format tests run on any CI host.
pub mod msbc;

// `aokie_radio` self-gates to windows/linux via an inner attribute in
// its `mod.rs`; on other targets it resolves to an empty module.
pub mod aokie_radio;
