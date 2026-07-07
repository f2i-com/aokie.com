//! Sherpa-onnx runtime — wraps `sherpa-rs` for the swappable TTS
//! and STT surfaces. We keep TTS and STT in sibling submodules
//! because their config shapes diverge enough that a single
//! "Sherpa runtime" type would be a kitchen sink. Each submodule
//! owns its own `*Runtime` + `*State` pair, mirroring the
//! `onnx_tts` / `candle_whisper` layout so call sites can be
//! migrated one engine at a time.
//!
//! Today this covers VITS-flavoured TTS bundles (which includes
//! Piper voices — they're VITS underneath). Kokoro / Matcha and
//! the STT counterparts (Whisper / Zipformer / Paraformer) follow
//! in subsequent commits as the bundle config shapes for each
//! engine settle.

pub mod stt;
pub mod tts;
