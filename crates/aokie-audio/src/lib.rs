//! # aokie-audio
//!
//! Audio processing for the Aokie call pipeline, lifted verbatim out of
//! the legacy Tauri app (`aokie-desktop/src-tauri/src`):
//!
//! * [`aec`] — SpeexDSP acoustic echo cancellation (subtracts the bot's
//!   outbound voice from the incoming SCO mic stream).
//! * [`recording`] — WAV call-recording writer.
//! * [`streaming`] — streaming audio buffer plumbing.
//! * [`notepad`] — call-notepad note model.
//!
//! (The Silero voice-activity detector, on sherpa-onnx, served Aokie's own
//! speech-to-text, which is gone: OAIY Voice hears the caller.)

pub mod aec;
pub mod notepad;
pub mod recording;
pub mod streaming;
