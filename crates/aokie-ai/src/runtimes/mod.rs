//! Heavy ML runtime modules — the actual loaders and inference
//! loops behind the `LlmProvider` / `TtsProvider` / `SttProvider`
//! traits. Adapters in `adapters/*` wrap these via the trait
//! surface; nothing outside `aokie-ai` should be importing them
//! directly.
//!
//! Every module here needs a heavy toolchain (ONNX Runtime, Candle, or
//! sherpa-onnx), so they are all feature-gated and OFF in the default
//! build:
//!
//! - `onnx_genai`     — ONNX-Runtime-GenAI-style multi-modal LLM
//!                      (today: Gemma 4 E4B-it). Feature: `onnx`.
//! - `onnx_tts`       — ONNX-bundle TTS in the pocket-tts shape
//!                      (encoder + decoder + vocoder + per-voice
//!                      .safetensors). Feature: `onnx`.
//! - `candle_whisper` — Candle-based Whisper STT (today:
//!                      whisper-large-v3-turbo). Feature: `candle`.
//! - `parakeet_onnx`  — NVIDIA Parakeet-Unified-EN-0.6B (FastConformer
//!                      + RNN-T) ASR, direct ORT sessions. Feature:
//!                      `onnx`.
//! - `moonshine_transcribe` / `qwen3_asr_transcribe` — the vendored
//!                      `transcribe-rs` ONNX engines. Feature: `onnx`.
//! - `sherpa_onnx`    — Catch-all for sherpa-onnx-shaped bundles
//!                      (Whisper STT, Kokoro/Matcha TTS, …). Feature:
//!                      `sherpa`.

#[cfg(feature = "candle")]
pub mod candle_whisper;

// The mel-filterbank DSP that the onnx mel front-ends (`onnx_genai`
// audio, `parakeet_onnx`) share lives in `candle_whisper::mel` and is
// candle-free (pure rustfft/std). When `onnx` is on without `candle`,
// expose just that submodule at the same path via a `#[path]` shim so
// `crate::runtimes::candle_whisper::mel::mel_filter_bank` still resolves
// without dragging in the candle runtime. (With `candle` on — including
// the `cuda` superset — the full module above already provides it.)
#[cfg(all(feature = "onnx", not(feature = "candle")))]
pub mod candle_whisper {
    // Relative to the inline module's dir (`runtimes/candle_whisper/`).
    #[path = "mel.rs"]
    pub mod mel;
}

#[cfg(feature = "onnx")]
pub mod moonshine_transcribe;
#[cfg(feature = "onnx")]
pub mod onnx_genai;
#[cfg(feature = "onnx")]
pub mod onnx_tts;
#[cfg(feature = "onnx")]
pub mod parakeet_onnx;
#[cfg(feature = "onnx")]
pub mod qwen3_asr_transcribe;

#[cfg(feature = "sherpa")]
pub mod sherpa_onnx;
