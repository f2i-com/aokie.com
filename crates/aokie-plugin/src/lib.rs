//! # aokie-plugin
//!
//! Aokie packaged as a **FormLogic Desktop plugin**: a standalone
//! process speaking JSON-RPC 2.0, newline-delimited, over stdio, per
//! `docs/FORMLOGIC_PLUGIN_CONTRACT.md` and the host-side
//! `DESKTOP_PLUGIN_SDK.md` contract (formlogic-app repo).
//!
//! Module map (fixed by the contract's extraction rules):
//!
//! * [`rpc`] — JSON-RPC framing: line parse (1 MiB cap), response /
//!   notification serialisation, error codes.
//! * [`contract`] — the canonical event/command/error-code constants;
//!   manifest.json + every emit site are test-locked to it.
//! * [`connector`] — plugin state + `connector.request` command
//!   dispatch (the `dongle.* / phone.* / call.* / sms.* / settings.*`
//!   MVP surface, incl. the dev-mode scripted call lifecycle).
//! * [`event_bridge`] — `event.emit` / `log.emit` notifications and
//!   the write-before-emit outbox coupling for essential events.
//! * [`outbox`] — the local SQLite `aokie_outbox` table (UNIQUE
//!   idempotency_key; pending → sent | failed → dead).
//! * [`config`] — JSON settings persisted under
//!   `FORMLOGIC_PLUGIN_DATA_DIR`.
//! * [`radio`] — the live Bluetooth radio: runs the real `aokie_radio`
//!   runtime on a background thread and bridges its call/SMS events +
//!   control onto the same contract as the mock (Windows-only).
//!
//! Hard rule from the SDK: **never write non-protocol output to
//! stdout** — diagnostics go to stderr or `log.emit`.

#[cfg(feature = "voice")]
pub mod aec;
#[cfg(feature = "voice")]
pub mod agent;
pub mod call_session;
pub mod config;
pub mod connector;
pub mod consent;
pub mod contract;
#[cfg(feature = "voice")]
pub mod endpoint_http;
pub mod event_bridge;
pub mod outbox;
pub mod radio;
pub mod rpc;
pub mod speech_wire;
#[cfg(feature = "voice")]
pub mod voice;
