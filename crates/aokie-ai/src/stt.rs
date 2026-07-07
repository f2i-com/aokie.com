//! `SttProvider` — the swappable speech-to-text surface.
//!
//! Adapters: whisper-stt (existing, candle-based Whisper v3 turbo),
//! sherpa-onnx (Phase 3, covers Whisper / Zipformer / Paraformer),
//! OpenAI Whisper HTTP. The receptionist pipeline calls into this
//! after VAD signals end-of-speech.

use async_trait::async_trait;

#[derive(Debug, Clone, Default)]
pub struct SttCapabilities {
    /// Provider supports streaming partial transcripts as audio
    /// arrives (Zipformer, faster-whisper streaming). When false,
    /// the caller buffers a full utterance and submits it whole —
    /// today's behaviour.
    pub streaming: bool,
    /// Provider can detect / honour an explicit language hint. False
    /// when the model is intrinsically single-language.
    pub language_hint: bool,
}

#[derive(Debug, Clone)]
pub struct SttRequest {
    /// Mono f32 PCM at 16 kHz, normalized to [-1, 1].
    pub samples_16k: Vec<f32>,
    /// BCP-47 language tag (e.g. "en", "en-AU"). Providers without
    /// language support ignore this; multilingual Whisper uses it to
    /// skip the lang-detect pass.
    pub language: String,
}

#[derive(Debug)]
pub enum SttError {
    NotReady(String),
    InvalidInput(String),
    Transcribe(String),
}

impl std::fmt::Display for SttError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SttError::NotReady(s) => write!(f, "STT not ready: {}", s),
            SttError::InvalidInput(s) => write!(f, "invalid STT input: {}", s),
            SttError::Transcribe(s) => write!(f, "STT transcription failed: {}", s),
        }
    }
}

impl std::error::Error for SttError {}

#[async_trait]
pub trait SttProvider: Send + Sync {
    fn id(&self) -> &str;

    fn capabilities(&self) -> SttCapabilities;

    /// Transcribe a complete utterance. Plain text out — the call
    /// pipeline never wires this through anywhere it'd surface
    /// timestamps, so the trait stays one-shape narrow.
    async fn transcribe(&self, req: SttRequest) -> Result<String, SttError>;
}
