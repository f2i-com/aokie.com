//! Per-OS transport facade.
//!
//! The HCI / ACL / SCO transport is the only OS-specific part of
//! `aokie_radio`. Everything else — parsers, framing, state machines,
//! the HFP / MAP / PBAP runtimes — is portable Rust and builds against
//! whichever transport this module re-exports.
//!
//! Layering:
//!
//!   `runtime`, `manager` ──────► `transport::*`
//!                                       │
//!         #[cfg(windows)] ◄─────────────┴────────────► #[cfg(linux)]
//!                │                                            │
//!         `super::winusb`                            `super::libusb`
//!         (Win32 WinUSB API)                         (libusb-1.0 / rusb)
//!
//! Both backends expose the same `AokieHciTransport` API plus the same
//! free functions (`enumerate_radio_interfaces`,
//! `enumerate_hci_radio_interfaces`, `read_first_local_address`,
//! `probe_first_controller`, `probe_controller`,
//! `read_local_address`, `diagnose_first_available`,
//! `diagnose_interface_path`) and the same data types
//! (`RadioInterface`, `PipeInfo`, `HciPipes`, `RadioAddress`,
//! `ControllerProbe`, `ScoTransportConfig`, `InterfaceDiagnostics`,
//! `InterfaceSource`, `PipeKind`, `PipeDirection`).
//!
//! Callers should `use crate::aokie_radio::transport::*` rather than
//! reaching into `winusb` / `libusb` directly. The Windows path stays
//! as the authoritative reference; the Linux path is a work-in-progress
//! whose stubs return runtime errors until phases L2–L4 land (see
//! PLAN.md → "Linux port").

#[cfg(target_os = "windows")]
pub use super::winusb::*;

#[cfg(target_os = "linux")]
pub use super::libusb::*;
