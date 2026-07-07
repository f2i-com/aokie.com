//! # aokie-audio
//!
//! Audio processing for the Aokie call pipeline, lifted verbatim out of
//! the legacy Tauri app (`aokie-desktop/src-tauri/src`):
//!
//! * [`aec`] — SpeexDSP acoustic echo cancellation (subtracts the bot's
//!   outbound voice from the incoming SCO mic stream).
//! * [`vad`] — voice activity detection. The real detector is Silero via
//!   `sherpa-rs`, behind the off-by-default [`sherpa`](crate#features)
//!   feature; without it, [`vad::StreamingVad`] keeps its public shape
//!   but `new()` returns a "feature not compiled in" error.
//! * [`recording`] — WAV call-recording writer.
//! * [`streaming`] — streaming audio buffer plumbing.
//! * [`notepad`] — call-notepad note model.
//!
//! # Features
//!
//! * `sherpa` (off by default) — pulls in `sherpa-rs` (Silero VAD) and
//!   the `reqwest`-based VAD-model downloader. Off by default so a stock
//!   build needs no CMake / `sherpa-rs-sys` toolchain.

pub mod aec;
pub mod notepad;
pub mod recording;
pub mod streaming;
pub mod vad;
