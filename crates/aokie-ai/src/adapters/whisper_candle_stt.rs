//! Generic Candle-Whisper adapter (today wraps the in-process
//! module under `crate::runtimes::candle_whisper`). Provider id and display name
//! come from `SttProviderConfig` so the bundled-model identity
//! lives in config rather than this file.
//!
//! Whisper is single-pass: the adapter pushes the synchronous
//! `CandleWhisperRuntime::transcribe` onto `spawn_blocking` so the async
//! caller doesn't pin a tokio worker.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::SttProviderConfig;
use crate::runtimes::candle_whisper::CandleWhisperRuntime;
use crate::stt::{SttCapabilities, SttError, SttProvider, SttRequest};

pub struct WhisperCandleStt {
    cfg: SttProviderConfig,
    inner: Arc<Mutex<Option<CandleWhisperRuntime>>>,
}

impl WhisperCandleStt {
    pub fn new(cfg: SttProviderConfig, inner: Arc<Mutex<Option<CandleWhisperRuntime>>>) -> Self {
        Self { cfg, inner }
    }
}

#[async_trait]
impl SttProvider for WhisperCandleStt {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> SttCapabilities {
        SttCapabilities {
            // candle-whisper is whole-utterance only — partial
            // streaming would need a sliding-window state machine
            // this module doesn't ship today.
            streaming: false,
            // Multilingual: the runtime maps `SttRequest::language`
            // (BCP-47, e.g. "en" / "fr" / "en-AU") to the matching
            // `<|xx|>` Whisper language token. Empty / unknown hints
            // fall back to English.
            language_hint: true,
        }
    }

    async fn transcribe(&self, req: SttRequest) -> Result<String, SttError> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || -> Result<String, SttError> {
            let guard = inner.blocking_lock();
            let stt = guard
                .as_ref()
                .ok_or_else(|| SttError::NotReady("Whisper not loaded".to_string()))?;
            stt.transcribe(&req.samples_16k, &req.language)
                .map_err(SttError::Transcribe)
        })
        .await
        .map_err(|join_err| SttError::Transcribe(format!("Whisper task panicked: {}", join_err)))?
    }
}
