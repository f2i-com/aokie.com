//! "Whoever is currently configured" lookup for the AI providers.
//!
//! Pipeline call sites (SMS auto-reply, post-call extraction, live
//! call streaming) ask this module for "the LLM" / "the TTS" / "the
//! STT" rather than naming a specific adapter. That keeps the call
//! sites provider-agnostic — adding a new engine means editing the
//! dispatch in this file, not chasing references across the caller.
//!
//! ## Light vs. runtime dispatch
//!
//! The crate splits the registry in two so the default build stays
//! free of the heavy ML runtimes:
//!
//! * [`active_llm`] / [`active_tts`] / [`active_stt`] take **no**
//!   in-process runtime handles. They resolve the config `kind` to the
//!   providers that need no heavy runtime — today the OpenAI-compatible
//!   HTTP LLM — and route every in-process kind to a fail-loud
//!   `Unknown*` provider. These always compile.
//! * [`active_llm_with_runtimes`] / [`active_tts_with_runtimes`] /
//!   [`active_stt_with_runtimes`] are the full dispatch that also wires
//!   the in-process ONNX / Candle / sherpa runtimes. Each is gated
//!   behind the feature(s) whose runtime types it names, so it only
//!   exists when those engines are compiled in.
//!
//! Unknown kinds get a fail-loud `Unknown*` provider that errors on
//! every request. The previous behaviour silently fell back to the
//! bundled on-device default, which masked typos in `ai_providers.json`.
//! Surfacing a `NotReady` error per request makes the misconfiguration
//! obvious in the UI bubble and in logs.

use async_trait::async_trait;

use crate::adapters::HttpOpenAiLlm;
use crate::config::active;
use crate::llm::TokenSink;
use crate::stt::SttRequest;
use crate::tts::ChunkSink;
use crate::{
    LlmCapabilities, LlmError, LlmProvider, LlmRequest, SttCapabilities, SttError, SttProvider,
    TtsCapabilities, TtsError, TtsProvider, TtsRequest,
};

// ---- Light dispatch (always compiled) -----------------------------------

/// Resolve the currently-configured LLM provider using only the light
/// (no in-process runtime) adapters. `openai-http` / `llama-server`
/// route through the HTTP adapter; every other kind — including the
/// in-process `onnx-genai` kind when the `onnx` feature is off — routes
/// to the fail-loud [`UnknownLlm`]. Build with `--features onnx` and use
/// [`active_llm_with_runtimes`] to reach the in-process Gemma runtime.
pub fn active_llm() -> Box<dyn LlmProvider> {
    let cfg = active().llm.clone();
    match cfg.kind.as_str() {
        // Remote HTTP provider — base_url + model + (optional) api_key
        // come from `ai_providers.json`. `llama-server` is an alias:
        // same wire protocol (OpenAI `/v1/chat/completions`), a
        // different sidecar at the other end of `cfg.base_url`.
        "openai-http" | "llama-server" => Box::new(HttpOpenAiLlm::new(cfg)),
        unknown => {
            eprintln!(
                "[ai::registry] LLM kind {:?} needs an in-process runtime not compiled into this build (or is unknown) — every request will error until the config is fixed or the crate is built with the matching feature",
                unknown
            );
            Box::new(UnknownLlm {
                kind: unknown.to_string(),
            })
        }
    }
}

/// Light TTS dispatch. No TTS provider ships without an in-process
/// runtime today, so every kind resolves to the fail-loud [`UnknownTts`].
/// Build with `--features onnx`/`--features sherpa` and use
/// [`active_tts_with_runtimes`] to reach a real synthesizer.
pub fn active_tts() -> Box<dyn TtsProvider> {
    let cfg = active().tts.clone();
    eprintln!(
        "[ai::registry] TTS kind {:?} needs an in-process runtime not compiled into this build — build aokie-ai with the onnx/sherpa feature and call active_tts_with_runtimes",
        cfg.kind
    );
    Box::new(UnknownTts { kind: cfg.kind })
}

/// Light STT dispatch. Same story as [`active_tts`] — every STT engine
/// is in-process, so the light path always yields [`UnknownStt`]. Use
/// [`active_stt_with_runtimes`] under the onnx/candle/sherpa features.
pub fn active_stt() -> Box<dyn SttProvider> {
    let cfg = active().stt.clone();
    eprintln!(
        "[ai::registry] STT kind {:?} needs an in-process runtime not compiled into this build — build aokie-ai with the onnx/candle/sherpa feature and call active_stt_with_runtimes",
        cfg.kind
    );
    Box::new(UnknownStt { kind: cfg.kind })
}

// ---- Runtime dispatch (feature-gated) -----------------------------------
//
// These mirror the legacy app's registry: they accept the in-process
// model handles the caller already holds and dispatch the config `kind`
// across the full adapter catalogue. Each is gated behind the feature(s)
// whose runtime types appear in its signature, so a partial feature set
// (e.g. `--features onnx` without `candle`) still compiles.

/// Full LLM dispatch including the in-process ONNX-GenAI (Gemma 4)
/// runtime. `onnx-genai` wraps the passed handle; the HTTP kinds and
/// unknowns behave exactly as in [`active_llm`].
#[cfg(feature = "onnx")]
pub fn active_llm_with_runtimes(
    gemma_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::onnx_genai::OnnxGenAiRuntime>>,
    >,
) -> Box<dyn LlmProvider> {
    use crate::adapters::OnnxGenAiLlm;
    let cfg = active().llm.clone();
    match cfg.kind.as_str() {
        "onnx-genai" => Box::new(OnnxGenAiLlm::new(cfg, gemma_instance)),
        "openai-http" | "llama-server" => Box::new(HttpOpenAiLlm::new(cfg)),
        unknown => {
            eprintln!(
                "[ai::registry] Unknown LLM kind {:?} in ai_providers.json — every request will error until this is fixed",
                unknown
            );
            Box::new(UnknownLlm {
                kind: unknown.to_string(),
            })
        }
    }
}

/// Full TTS dispatch across the in-process ONNX-TTS and sherpa-onnx
/// engines. Gated behind both `onnx` and `sherpa` because it names a
/// runtime handle from each.
#[cfg(all(feature = "onnx", feature = "sherpa"))]
pub fn active_tts_with_runtimes(
    pocket_tts_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::onnx_tts::OnnxTtsRuntime>>,
    >,
    sherpa_tts_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::sherpa_onnx::tts::SherpaOnnxTtsRuntime>>,
    >,
) -> Box<dyn TtsProvider> {
    use crate::adapters::{OnnxTts, SherpaOnnxTts};
    let cfg = active().tts.clone();
    match cfg.kind.as_str() {
        "onnx-tts" => Box::new(OnnxTts::new(cfg, pocket_tts_instance)),
        "sherpa-onnx-tts" => Box::new(SherpaOnnxTts::new(cfg, sherpa_tts_instance)),
        unknown => {
            eprintln!(
                "[ai::registry] Unknown TTS kind {:?} in ai_providers.json — every request will error until this is fixed",
                unknown
            );
            Box::new(UnknownTts {
                kind: unknown.to_string(),
            })
        }
    }
}

/// Full STT dispatch across every in-process engine: candle-Whisper
/// (`candle`), sherpa-onnx (`sherpa`), and the ONNX/`transcribe-rs`
/// engines Parakeet / Moonshine / Qwen3-ASR (`onnx`). Gated behind all
/// three features because its signature names a handle from each.
#[cfg(all(feature = "onnx", feature = "candle", feature = "sherpa"))]
#[allow(clippy::type_complexity)]
pub fn active_stt_with_runtimes(
    whisper_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::candle_whisper::CandleWhisperRuntime>>,
    >,
    sherpa_stt_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::sherpa_onnx::stt::SherpaOnnxSttRuntime>>,
    >,
    parakeet_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::parakeet_onnx::ParakeetOnnxRuntime>>,
    >,
    moonshine_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::moonshine_transcribe::MoonshineTranscribeRuntime>>,
    >,
    qwen3_asr_instance: std::sync::Arc<
        tokio::sync::Mutex<Option<crate::runtimes::qwen3_asr_transcribe::Qwen3AsrTranscribeRuntime>>,
    >,
) -> Box<dyn SttProvider> {
    use crate::adapters::{
        MoonshineTranscribeStt, ParakeetOnnxStt, Qwen3AsrTranscribeStt, SherpaOnnxStt,
        WhisperCandleStt,
    };
    let cfg = active().stt.clone();
    match cfg.kind.as_str() {
        "whisper-candle" => Box::new(WhisperCandleStt::new(cfg, whisper_instance)),
        "sherpa-onnx-stt" => Box::new(SherpaOnnxStt::new(cfg, sherpa_stt_instance)),
        "parakeet-onnx" => Box::new(ParakeetOnnxStt::new(cfg, parakeet_instance)),
        "moonshine-onnx" => Box::new(MoonshineTranscribeStt::new(cfg, moonshine_instance)),
        "qwen3-asr-onnx" => Box::new(Qwen3AsrTranscribeStt::new(cfg, qwen3_asr_instance)),
        unknown => {
            eprintln!(
                "[ai::registry] Unknown STT kind {:?} in ai_providers.json — every request will error until this is fixed",
                unknown
            );
            Box::new(UnknownStt {
                kind: unknown.to_string(),
            })
        }
    }
}

// ---- Fail-loud placeholders for unavailable provider kinds --------------
//
// Every adapter method returns `NotReady` with the offending kind
// string, so the UI surfaces the typo / missing-feature instead of
// silently routing to an on-device fallback. Capabilities are zeroed so
// any optional-flag gating (audio input, streaming, etc.) skips this
// provider cleanly.

struct UnknownLlm {
    kind: String,
}

#[async_trait]
impl LlmProvider for UnknownLlm {
    fn id(&self) -> &str {
        "unknown-llm"
    }

    fn capabilities(&self) -> LlmCapabilities {
        LlmCapabilities::default()
    }

    async fn generate(&self, _req: LlmRequest) -> Result<String, LlmError> {
        Err(LlmError::NotReady(format!(
            "unavailable LLM kind {:?} — set kind to a valid value (e.g. \"openai-http\"), or build aokie-ai with the runtime feature for this kind and use active_llm_with_runtimes",
            self.kind
        )))
    }

    async fn generate_stream(
        &self,
        _req: LlmRequest,
        _on_token: TokenSink,
    ) -> Result<String, LlmError> {
        Err(LlmError::NotReady(format!(
            "unavailable LLM kind {:?} — set kind to a valid value (e.g. \"openai-http\"), or build aokie-ai with the runtime feature for this kind and use active_llm_with_runtimes",
            self.kind
        )))
    }
}

struct UnknownTts {
    kind: String,
}

#[async_trait]
impl TtsProvider for UnknownTts {
    fn id(&self) -> &str {
        "unknown-tts"
    }

    fn capabilities(&self) -> TtsCapabilities {
        TtsCapabilities::default()
    }

    async fn synthesize_stream(
        &self,
        _req: TtsRequest,
        _on_chunk: ChunkSink,
    ) -> Result<(), TtsError> {
        Err(TtsError::NotReady(format!(
            "unavailable TTS kind {:?} — build aokie-ai with the onnx/sherpa feature and use active_tts_with_runtimes",
            self.kind
        )))
    }
}

struct UnknownStt {
    kind: String,
}

#[async_trait]
impl SttProvider for UnknownStt {
    fn id(&self) -> &str {
        "unknown-stt"
    }

    fn capabilities(&self) -> SttCapabilities {
        SttCapabilities::default()
    }

    async fn transcribe(&self, _req: SttRequest) -> Result<String, SttError> {
        Err(SttError::NotReady(format!(
            "unavailable STT kind {:?} — build aokie-ai with the onnx/candle/sherpa feature and use active_stt_with_runtimes",
            self.kind
        )))
    }
}
