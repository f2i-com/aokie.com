//! Generic ONNX-TTS adapter (today wraps the in-process module
//! under `crate::runtimes::onnx_tts`). Provider id and display name
//! come from `TtsProviderConfig` so the bundled-model identity
//! lives in config rather than this file.
//!
//! Phase 1 still imports the concrete underlying runtime type —
//! Phase 2 (sherpa-onnx engine registry) is what makes the runtime
//! itself swappable. The adapter contract above this file is
//! already provider-agnostic.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::TtsProviderConfig;
use crate::runtimes::onnx_tts::OnnxTtsRuntime;
use crate::tts::{ChunkSink, TtsCapabilities, TtsChunk, TtsError, TtsProvider, TtsRequest};

pub struct OnnxTts {
    cfg: TtsProviderConfig,
    inner: Arc<Mutex<Option<OnnxTtsRuntime>>>,
    /// Cached native sample rate. The underlying module exposes this
    /// via a `&self` getter, so we read it once at construction —
    /// avoids an extra blocking_lock just to fill capabilities().
    /// `None` means we'll fall back to the model's reported value at
    /// synthesis time.
    sample_rate: Option<u32>,
}

impl OnnxTts {
    pub fn new(cfg: TtsProviderConfig, inner: Arc<Mutex<Option<OnnxTtsRuntime>>>) -> Self {
        let sample_rate = inner
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|t| t.sample_rate()));
        Self {
            cfg,
            inner,
            sample_rate,
        }
    }
}

#[async_trait]
impl TtsProvider for OnnxTts {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> TtsCapabilities {
        TtsCapabilities {
            sample_rate: self.sample_rate.unwrap_or(0),
            streaming: true,
            // Voice list is per-bundle and the operator picks via
            // settings; reporting it here would force a blocking lock
            // on every capability read. Keep empty — the UI fetches
            // voices through a dedicated command when it needs them.
            voices: Vec::new(),
        }
    }

    async fn synthesize_stream(
        &self,
        req: TtsRequest,
        on_chunk: ChunkSink,
    ) -> Result<(), TtsError> {
        let inner = self.inner.clone();
        let mut sink = on_chunk;
        let outcome = tokio::task::spawn_blocking(move || -> Result<(), TtsError> {
            let mut guard = inner.blocking_lock();
            let tts = guard
                .as_mut()
                .ok_or_else(|| TtsError::NotReady("ONNX TTS not loaded".to_string()))?;
            tts.synthesize_stream(&req.text, &req.voice, |samples, sr| {
                let chunk = TtsChunk {
                    samples: samples.to_vec(),
                    sample_rate: sr,
                };
                sink(chunk)
            })
            .map(|_stats| ())
            .map_err(TtsError::Synthesize)
        })
        .await
        .map_err(|join_err| TtsError::Synthesize(format!("TTS task panicked: {}", join_err)))?;

        outcome
    }
}
