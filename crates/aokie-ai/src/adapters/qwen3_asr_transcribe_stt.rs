//! Qwen3-ASR STT adapter — wraps `Qwen3AsrTranscribeRuntime` in the
//! `SttProvider` trait. Same `spawn_blocking` shape as the moonshine /
//! parakeet adapters so the heavy decode (encoder + greedy autoregressive
//! step loop) doesn't pin a tokio worker thread.
//!
//! Capabilities: full-utterance only (no streaming) but multilingual —
//! the model accepts a language hint that the engine layer surfaces via
//! `Qwen3AsrParams::language`. The hint resolves per call as
//! `req.language` first (whatever the call answerer plumbed in),
//! `cfg.qwen3_asr.language` next (provider-config default, defaults to
//! `"en"`), and `None` otherwise — see `transcribe()` below for the
//! priority chain. The model still language-IDs the audio when nothing
//! resolves, but in practice the config default keeps short / accented
//! English from tokenising into Chinese.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::SttProviderConfig;
use crate::runtimes::qwen3_asr_transcribe::Qwen3AsrTranscribeRuntime;
use crate::stt::{SttCapabilities, SttError, SttProvider, SttRequest};

pub struct Qwen3AsrTranscribeStt {
    cfg: SttProviderConfig,
    state: Arc<Mutex<Option<Qwen3AsrTranscribeRuntime>>>,
}

impl Qwen3AsrTranscribeStt {
    pub fn new(
        cfg: SttProviderConfig,
        state: Arc<Mutex<Option<Qwen3AsrTranscribeRuntime>>>,
    ) -> Self {
        Self { cfg, state }
    }
}

#[async_trait]
impl SttProvider for Qwen3AsrTranscribeStt {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> SttCapabilities {
        SttCapabilities {
            streaming: false,
            language_hint: true,
        }
    }

    async fn transcribe(&self, req: SttRequest) -> Result<String, SttError> {
        let state = self.state.clone();
        // Language priming: per-request hint from SttRequest wins; the
        // provider config's static `language` field (default "en")
        // covers everything else. Empty string opts back into the
        // model's built-in auto-detect path.
        let language: Option<String> = if !req.language.trim().is_empty() {
            Some(req.language.clone())
        } else {
            self.cfg
                .qwen3_asr
                .as_ref()
                .map(|c| c.language.clone())
                .filter(|s| !s.trim().is_empty())
        };
        tokio::task::spawn_blocking(move || -> Result<String, SttError> {
            let mut guard = state.blocking_lock();
            let rt = guard.as_mut().ok_or_else(|| {
                SttError::NotReady(
                    "qwen3-asr runtime not loaded — call initialize_qwen3_asr first \
                     (or visit AI Stack to retry)"
                        .to_string(),
                )
            })?;
            rt.transcribe(&req.samples_16k, language.as_deref())
                .map_err(SttError::Transcribe)
        })
        .await
        .map_err(|join_err| {
            SttError::Transcribe(format!("qwen3-asr STT task panicked: {}", join_err))
        })?
    }
}
