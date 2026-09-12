//! Live Bluetooth radio integration.
//!
//! Runs the real `aokie_radio` `AokieRuntime` (via
//! [`aokie_dongle::bluetooth::BluetoothManager`]) on a dedicated background
//! thread, maps its `BluetoothEvent`s onto the `aokie.*` Desktop-event
//! contract (emitted straight to stdout **and** the durable outbox), and
//! accepts control requests â€” answer / reject / hangup / send-SMS / speak â€”
//! from the main RPC thread over an mpsc channel.
//!
//! ## Why a second `Outbox` + `StdoutSink` on this thread
//! The plugin's main loop blocks on stdin, so an asynchronous call event (a
//! call can ring at any instant) must be delivered without waiting for the
//! next RPC. `StdoutSink` writes one whole line under the stdout lock
//! (line-atomic), so this thread's sink and the main thread's sink never
//! interleave. The [`Outbox`] here is a *second* SQLite connection to the
//! same `outbox.sqlite` file â€” `idempotency_key` is UNIQUE and SQLite
//! serialises writers, so essential call/SMS records survive a Desktop
//! restart exactly as they do on the command path.
//!
//! The whole radio surface is Windows-only (WinUSB); on other targets
//! [`spawn`] returns an error and the plugin simply never has a radio.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
#[cfg(any(feature = "voice", target_os = "windows"))]
use std::time::{Duration, Instant};

use aokie_core::events::DesktopEvent;
use serde_json::json;

use crate::config::PairedDevice;
use crate::event_bridge::{emit_event, Sink};
use crate::outbox::Outbox;

/// Default receptionist system prompt when none is configured (voice build) —
/// a goal-directed SCRIPT, not just a style: greet, get the caller's name and
/// reason, capture the key details, and book them in or take a message, one
/// short spoken question at a time. THE persona lives in the always-compiled
/// contract module (audit CROSS-SCHEMA-001) and is test-locked to the shared
/// cross-repo fixture; editable live via the `persona` setting / a flow push.
#[cfg(all(target_os = "windows", feature = "voice"))]
use crate::contract::DEFAULT_AGENT_PERSONA;

mod audio_reply;
#[cfg(feature = "voice")]
mod capture_activity;
mod control;
mod events_drain;
mod greet;
mod remote_transitions;
mod controls;
mod prompts;
mod manager_gate;
mod reply_stream;
mod watchdogs;
mod realtime_lane;
mod reply_rounds;
mod reconcile;
mod realtime_service;
mod transcript;
mod status;
mod handle;
mod events_emit;
mod turn_flush;
mod turns;
mod spawn;
mod http_speech;
mod juggle;
mod playback;
mod lookup;
mod answer;
mod speak;
mod switchboard;
mod assistance_lane;
mod context;
mod run_loop;
mod event_handler;

// Re-exports: every former `crate::radio::X` path keeps working. Modules
// whose items are all `pub(super)` (radio-internal) are pulled in with plain
// `use` globs — submodules still reach those names through their own
// `use super::*;`, and nothing outside `radio` ever could.
pub use self::control::*;
pub use self::handle::*;
#[allow(unused_imports)]
pub(crate) use self::http_speech::*;
pub use self::prompts::*;
pub use self::spawn::*;
pub use self::status::*;
#[allow(unused_imports)]
use self::answer::*;
#[allow(unused_imports)]
use self::audio_reply::*;
#[allow(unused_imports)]
use self::assistance_lane::*;
#[allow(unused_imports)]
use self::context::*;
#[allow(unused_imports)]
use self::controls::*;
#[allow(unused_imports)]
use self::event_handler::*;
#[allow(unused_imports)]
use self::events_drain::*;
#[allow(unused_imports)]
use self::events_emit::*;
#[allow(unused_imports)]
use self::greet::*;
#[allow(unused_imports)]
use self::remote_transitions::*;
#[allow(unused_imports)]
use self::juggle::*;
#[allow(unused_imports)]
use self::lookup::*;
#[allow(unused_imports)]
use self::manager_gate::*;
#[allow(unused_imports)]
use self::playback::*;
#[allow(unused_imports)]
use self::realtime_lane::*;
#[allow(unused_imports)]
use self::reconcile::*;
#[allow(unused_imports)]
use self::realtime_service::*;
#[allow(unused_imports)]
use self::reply_rounds::*;
#[allow(unused_imports)]
use self::reply_stream::*;
#[allow(unused_imports)]
use self::run_loop::*;
#[allow(unused_imports)]
use self::speak::*;
#[allow(unused_imports)]
use self::switchboard::*;
#[allow(unused_imports)]
use self::transcript::*;
#[allow(unused_imports)]
use self::turn_flush::*;
#[allow(unused_imports)]
use self::turns::*;
#[allow(unused_imports)]
use self::watchdogs::*;


#[cfg(test)]
mod tests;

/// §12.3 synthetic audio rig: drives the REAL paced-playback machinery
/// ([`TtsChunkPlayback`] + the real speexdsp [`crate::aec::EchoCanceller`])
/// with scripted audio on a virtual clock — a known "bot voice" waveform
/// whose echo returns through a synthetic echo path, plus an optional caller
/// signal at a chosen onset/level/duration. No radio, no models: this tests
/// the duplex DECISIONS (echo rejection, scratchpad capture, barge timing,
/// interrupt policy, spoken-command cuts, ducking) end-to-end at the sample
/// level, deterministically.
#[cfg(all(test, target_os = "windows", feature = "voice"))]
mod synthetic_audio;
