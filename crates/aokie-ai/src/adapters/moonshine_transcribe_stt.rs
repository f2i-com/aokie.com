//! Moonshine STT adapter — wraps `MoonshineTranscribeRuntime` in the
//! `SttProvider` trait. Same shape as the parakeet/sherpa adapters:
//! `transcribe()` runs the heavy work on `spawn_blocking` so it doesn't
//! pin a tokio worker.
//!
//! Per-request `language` hints are silently dropped — Moonshine's
//! base/tiny English variants are single-language by construction.
//! (The multilingual `tiny-zh` / `tiny-ja` etc. variants would each
//! be a different bundle the operator picks separately.)

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::SttProviderConfig;
use crate::runtimes::moonshine_transcribe::MoonshineTranscribeRuntime;
use crate::stt::{SttCapabilities, SttError, SttProvider, SttRequest};

pub struct MoonshineTranscribeStt {
    cfg: SttProviderConfig,
    state: Arc<Mutex<Option<MoonshineTranscribeRuntime>>>,
}

impl MoonshineTranscribeStt {
    pub fn new(
        cfg: SttProviderConfig,
        state: Arc<Mutex<Option<MoonshineTranscribeRuntime>>>,
    ) -> Self {
        Self { cfg, state }
    }
}

#[async_trait]
impl SttProvider for MoonshineTranscribeStt {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> SttCapabilities {
        SttCapabilities {
            // Moonshine encoder-decoder runs the full utterance in one
            // pass (no streaming partials).
            streaming: false,
            // English-only on the bundles we ship today.
            language_hint: false,
        }
    }

    async fn transcribe(&self, req: SttRequest) -> Result<String, SttError> {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || -> Result<String, SttError> {
            let mut guard = state.blocking_lock();
            let rt = guard.as_mut().ok_or_else(|| {
                SttError::NotReady(
                    "moonshine runtime not loaded — call initialize_moonshine first \
                     (or visit AI Stack to retry)"
                        .to_string(),
                )
            })?;
            rt.transcribe(&req.samples_16k)
                .map_err(SttError::Transcribe)
        })
        .await
        .map_err(|join_err| {
            SttError::Transcribe(format!("moonshine STT task panicked: {}", join_err))
        })?
    }
}
