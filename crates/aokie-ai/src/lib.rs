//! # aokie-ai
//!
//! Provider-trait surface for the swappable AI stack, lifted out of the
//! legacy Aokie Tauri app (`aokie-desktop/src-tauri/src/ai`). This crate
//! is the load-bearing boundary that lets a caller talk to "the LLM",
//! "the TTS", "the STT" without knowing which engine is actually behind
//! it. The runtime modules under [`runtimes`] keep their internals; the
//! adapters in [`adapters`] wrap each one as a trait impl so call sites
//! can be migrated one at a time.
//!
//! The traits intentionally don't try to be the union of every
//! provider's capability surface. Instead each provider declares its
//! capabilities (audio input, streaming, native tool use, max context)
//! and the call sites gate on those. A text-only LLM (llama.cpp serving
//! Llama-3) doesn't have to pretend to accept audio; the live-call
//! pipeline reads the capability flag and routes through Whisper first.
//!
//! ## Feature gating — the default build is LIGHT
//!
//! The default feature set (`default = []`) compiles only the parts
//! that need no heavy ML runtime and no CMake / ONNX-Runtime toolchain:
//!
//! * the provider **traits** ([`llm`], [`tts`], [`stt`]) + neutral
//!   [`types`],
//! * the provider [`config`] loader (bundled JSON + disk override,
//!   keyring-backed API keys via `aokie-core`),
//! * the [`registry`] dispatch — its light [`active_llm`] /
//!   [`active_tts`] / [`active_stt`] entry points,
//! * the OpenAI-compatible **HTTP** LLM adapter (the only adapter that
//!   needs no in-process model), and
//! * the [`sidecars::llama_server`] supervisor + [`bundled_models`]
//!   path resolver (both Tauri-free — they take owned dirs, not an
//!   `AppHandle`).
//!
//! The heavy in-process runtimes sit behind opt-in features:
//!
//! * `onnx`   — ONNX-Runtime (`ort`) engines: Gemma-4 GenAI LLM, the
//!   Pocket-TTS ONNX voice, Parakeet, and the vendored `transcribe-rs`
//!   engines (Moonshine, Qwen3-ASR). Pulls `ort` (dynamic-load; no
//!   CMake) + `ndarray` + `tokenizers` + `rustfft` + `safetensors` +
//!   `rubato` + `half`.
//! * `candle` — Candle-based Whisper STT (`candle-core/nn/transformers`
//!   + `tokenizers` + `rustfft`).
//! * `sherpa` — sherpa-onnx STT/TTS via `sherpa-rs` (needs CMake).
//! * `cuda`   — GPU execution providers for `ort` + `candle`.
//!
//! When a feature is off, its runtime + adapter modules simply aren't
//! compiled, and the corresponding provider `kind` in `ai_providers.json`
//! resolves to a fail-loud `Unknown*` provider (see [`registry`]).
//!
//! VAD swappability is intentionally deferred — that lives in
//! `aokie-audio`.

pub mod adapters;
pub mod bundled_models;
pub mod config;
pub mod llm;
pub mod registry;
pub mod runtimes;
pub mod sidecars;
pub mod stt;
pub mod tts;
pub mod types;

pub use registry::{active_llm, active_stt, active_tts};

pub use llm::{LlmCapabilities, LlmError, LlmProvider, LlmRequest};
pub use stt::{SttCapabilities, SttError, SttProvider, SttRequest};
pub use tts::{humanize_for_speech, TtsCapabilities, TtsChunk, TtsError, TtsProvider, TtsRequest};
pub use types::{Role, Turn};
