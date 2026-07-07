//! ONNX-based speech recognition engines.
//!
//! Each model is available as a top-level module (e.g. `onnx::sense_voice::SenseVoiceModel`)
//! and implements the `SpeechModel` trait for a unified transcription API.

pub mod session;

/// Preferred precision for ONNX model loading.
///
/// This selects which model file variant to load. If the requested
/// variant is not found on disk, falls back to FP32 with a warning.
/// ONNX quantization is baked into the model file — this enum controls
/// file selection, not runtime behavior.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Quantization {
    #[default]
    FP32,
    FP16,
    Int8,
    Int4,
}

pub mod canary;
pub mod cohere;
pub mod gigaam;
pub mod moonshine;
pub mod parakeet;
pub mod sense_voice;

// Qwen3-ASR-0.6B is gated behind the `qwen3-asr` feature: it pulls in
// the HuggingFace `tokenizers` crate, `half`, and `memmap2` for the
// external embed_tokens.bin lookup. Builds that don't ask for it skip
// the extra dep tree entirely.
#[cfg(feature = "qwen3-asr")]
pub mod qwen3_asr;
