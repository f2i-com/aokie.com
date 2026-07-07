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
//! * [`connector`] — plugin state + `connector.request` command
//!   dispatch (the `dongle.* / phone.* / call.* / sms.* / settings.*`
//!   MVP surface, incl. the dev-mode scripted call lifecycle).
//! * [`event_bridge`] — `event.emit` / `log.emit` notifications and
//!   the write-before-emit outbox coupling for essential events.
//! * [`outbox`] — the local SQLite `aokie_outbox` table (UNIQUE
//!   idempotency_key; pending → sent | failed → dead).
//! * [`config`] — JSON settings persisted under
//!   `FORMLOGIC_PLUGIN_DATA_DIR`.
//!
//! Hard rule from the SDK: **never write non-protocol output to
//! stdout** — diagnostics go to stderr or `log.emit`.

pub mod config;
pub mod connector;
pub mod event_bridge;
pub mod outbox;
pub mod rpc;
