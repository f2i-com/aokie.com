//! Sherpa-onnx STT adapter — wraps the engine-kind runtime in
//! `crate::runtimes::sherpa_onnx::stt`. Like the TTS sibling,
//! the runtime loads lazily on the first transcribe call so users
//! can pick a bundle from the AI providers settings without a
//! separate initialize step.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::SttProviderConfig;
use crate::runtimes::sherpa_onnx::stt::SherpaOnnxSttRuntime;
use crate::stt::{SttCapabilities, SttError, SttProvider, SttRequest};

pub struct SherpaOnnxStt {
    cfg: SttProviderConfig,
    state: Arc<Mutex<Option<SherpaOnnxSttRuntime>>>,
}

impl SherpaOnnxStt {
    pub fn new(cfg: SttProviderConfig, state: Arc<Mutex<Option<SherpaOnnxSttRuntime>>>) -> Self {
        Self { cfg, state }
    }
}

#[async_trait]
impl SttProvider for SherpaOnnxStt {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> SttCapabilities {
        SttCapabilities {
            // Offline whisper bundle decodes a buffered utterance,
            // not a streaming partial. Same shape as candle-whisper.
            streaming: false,
            // sherpa-rs's WhisperRecognizer locks language at
            // construction (cfg.sherpa_onnx.language). Per-request
            // `SttRequest::language` is silently dropped — rebuilding
            // the recognizer on every mismatch would tank throughput.
            // Callers wanting on-the-fly language switching should
            // pick the candle-whisper provider instead.
            language_hint: false,
        }
    }

    async fn transcribe(&self, req: SttRequest) -> Result<String, SttError> {
        let cfg_blob = self.cfg.sherpa_onnx.clone().ok_or_else(|| {
            SttError::NotReady(
                "sherpa-onnx STT: provider config missing the `sherpa_onnx` block".to_string(),
            )
        })?;
        // One-shot warning when the caller passes a language hint
        // that doesn't match what the recognizer was built with.
        // Sherpa's offline Whisper API doesn't accept per-request
        // language; this surfaces the silent mismatch in logs so
        // operators don't wonder why their hint is ignored.
        let req_lang = req.language.trim();
        let cfg_lang = cfg_blob.language.trim();
        if !req_lang.is_empty() && !cfg_lang.is_empty() && !req_lang.eq_ignore_ascii_case(cfg_lang)
        {
            eprintln!(
                "[sherpa-onnx STT] req.language={:?} differs from cfg.language={:?} — sherpa locks language at construction; the request hint is dropped",
                req_lang, cfg_lang
            );
        }
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || -> Result<String, SttError> {
            let mut guard = state.blocking_lock();
            if guard.is_none() {
                let runtime = SherpaOnnxSttRuntime::load(&cfg_blob).map_err(SttError::NotReady)?;
                *guard = Some(runtime);
            }
            let rt = guard.as_mut().expect("just initialized above");
            rt.transcribe(&req.samples_16k)
                .map_err(SttError::Transcribe)
        })
        .await
        .map_err(|join_err| {
            SttError::Transcribe(format!("sherpa-onnx STT task panicked: {}", join_err))
        })?
    }
}
