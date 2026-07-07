//! `LlmProvider` — the swappable LLM surface.
//!
//! Every concrete adapter (Gemma 4 ONNX, llama.cpp sidecar, ORT-GenAI,
//! OpenAI-compatible HTTP, ...) implements this. Call sites in the
//! receptionist pipeline depend on the trait, not on a specific
//! provider type, so swapping the model at runtime is a registry
//! lookup rather than a re-build.

use async_trait::async_trait;

use super::types::Turn;

/// What a provider can actually do. Read these flags before relying on
/// optional features — e.g. an HTTP provider that doesn't expose a
/// streaming endpoint should set `streaming = false` and the live-call
/// path will fall back to whole-response generation.
#[derive(Debug, Clone, Default)]
pub struct LlmCapabilities {
    /// True for multimodal providers that take raw 16 kHz mono PCM as
    /// input (currently just Gemma 4). Text-only providers ignore the
    /// `audio_16k_mono` field on each turn.
    pub audio_input: bool,
    /// True when `generate_stream` actually streams; false providers
    /// can implement it as a single-chunk callback after a normal
    /// `generate`. Useful for the live-call TTS handoff that wants
    /// sentence-by-sentence text as the model produces it.
    pub streaming: bool,
    /// Provider-reported max context window in tokens. `None` if the
    /// provider doesn't expose this (e.g. user-supplied URL). The
    /// pipeline uses this to decide how much SMS history to include.
    pub max_context_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    /// System / persona prompt, prepended once at the top of the
    /// conversation. Modules like `calendar::prompt` and
    /// `orders::prompt` extend whatever the operator configured —
    /// providers receive the already-extended string here.
    pub system: Option<String>,
    /// Conversation in chronological order. Each provider translates
    /// to its native chat template (Gemma's `<|turn>` / OpenAI's
    /// `{role, content}` / etc.).
    pub turns: Vec<Turn>,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Stop sequences. Empty for providers that don't support them.
    pub stop: Vec<String>,
}

impl LlmRequest {
    /// Sensible defaults for short SMS / classifier passes.
    pub fn new(turns: Vec<Turn>) -> Self {
        Self {
            system: None,
            turns,
            max_tokens: 512,
            temperature: 0.7,
            stop: Vec::new(),
        }
    }

    pub fn with_system(mut self, sys: impl Into<String>) -> Self {
        self.system = Some(sys.into());
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = t;
        self
    }
}

#[derive(Debug)]
pub enum LlmError {
    /// Provider hasn't been initialized / model not loaded.
    NotReady(String),
    /// Caller passed something the provider can't handle (e.g. audio
    /// to a text-only LLM, or context overflow).
    InvalidInput(String),
    /// Something inside generation went wrong — tokenizer errors, ONNX
    /// runtime failures, HTTP errors, etc. The string is for logs;
    /// don't surface it raw to a customer.
    Generate(String),
    /// User-facing cancellation (the streaming `on_token` returned
    /// false). Not a fault, but lets callers distinguish "cut short
    /// by us" from "model errored out."
    Interrupted,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::NotReady(s) => write!(f, "LLM not ready: {}", s),
            LlmError::InvalidInput(s) => write!(f, "invalid LLM input: {}", s),
            LlmError::Generate(s) => write!(f, "LLM generation failed: {}", s),
            LlmError::Interrupted => write!(f, "LLM generation interrupted"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Token sink used by `generate_stream`. Returning `false` from the
/// closure signals "stop generating" — providers that support real
/// interruption (Gemma 4) honour it immediately; HTTP providers stop
/// at the next chunk boundary. Mirrors the existing
/// `OnnxGenAiRuntime::generate_stream` callback contract.
pub type TokenSink = Box<dyn FnMut(&str) -> bool + Send>;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable id used in config + logs (e.g. "gemma4-onnx",
    /// "llama-server", "openai-compatible-http").
    fn id(&self) -> &str;

    fn capabilities(&self) -> LlmCapabilities;

    /// Run the request to completion, return the full text. Concrete
    /// providers either (a) do this natively and avoid streaming
    /// machinery, or (b) call `generate_stream` internally and
    /// concatenate.
    async fn generate(&self, req: LlmRequest) -> Result<String, LlmError>;

    /// Stream tokens through `on_token` and return the concatenated
    /// final string when generation ends (or the closure returns
    /// false). Providers without real streaming can fall back to
    /// `generate` and emit one chunk.
    async fn generate_stream(
        &self,
        req: LlmRequest,
        on_token: TokenSink,
    ) -> Result<String, LlmError>;
}
