//! Concrete `LlmProvider` / `TtsProvider` / `SttProvider` adapters.
//! Each adapter is an *engine kind* — `OnnxGenAiLlm` wraps any model
//! served through the in-process ONNX-Runtime-GenAI runtime, `OnnxTts`
//! wraps the in-process ONNX TTS runtime, and so on. The specific
//! model identity (Gemma 4 / Pocket TTS / Whisper) lives in
//! `config` JSON and is handed in at construction.
//!
//! Adapters are deliberately thin — heavy ML stays in
//! `crate::runtimes::*` (`onnx_genai`, `onnx_tts`, `candle_whisper`).
//! Adding a new engine (sherpa, llama-server sidecar, OpenAI HTTP)
//! means adding a new file here, not editing the underlying modules.
//!
//! Feature gating mirrors `crate::runtimes`: the HTTP adapter is the
//! only one that needs no in-process runtime, so it always compiles.
//! The ONNX adapters are behind `onnx`, the candle-Whisper adapter
//! behind `candle`, and the sherpa adapters behind `sherpa`.

pub mod http_openai_llm;
pub use http_openai_llm::HttpOpenAiLlm;

#[cfg(feature = "onnx")]
pub mod moonshine_transcribe_stt;
#[cfg(feature = "onnx")]
pub mod onnx_genai_llm;
#[cfg(feature = "onnx")]
pub mod onnx_tts;
#[cfg(feature = "onnx")]
pub mod parakeet_onnx_stt;
#[cfg(feature = "onnx")]
pub mod qwen3_asr_transcribe_stt;

#[cfg(feature = "onnx")]
pub use moonshine_transcribe_stt::MoonshineTranscribeStt;
#[cfg(feature = "onnx")]
pub use onnx_genai_llm::OnnxGenAiLlm;
#[cfg(feature = "onnx")]
pub use onnx_tts::OnnxTts;
#[cfg(feature = "onnx")]
pub use parakeet_onnx_stt::ParakeetOnnxStt;
#[cfg(feature = "onnx")]
pub use qwen3_asr_transcribe_stt::Qwen3AsrTranscribeStt;

#[cfg(feature = "candle")]
pub mod whisper_candle_stt;
#[cfg(feature = "candle")]
pub use whisper_candle_stt::WhisperCandleStt;

// Sherpa adapters wrap the sherpa-onnx runtime; both are behind the
// `sherpa` feature so the default build needs no sherpa-rs-sys CMake
// toolchain.
#[cfg(feature = "sherpa")]
pub mod sherpa_onnx_stt;
#[cfg(feature = "sherpa")]
pub mod sherpa_onnx_tts;
#[cfg(feature = "sherpa")]
pub use sherpa_onnx_stt::SherpaOnnxStt;
#[cfg(feature = "sherpa")]
pub use sherpa_onnx_tts::SherpaOnnxTts;
