//! Sherpa-onnx TTS adapter — wraps the engine-kind runtime in
//! `crate::runtimes::sherpa_onnx::tts`. The adapter loads the
//! sherpa engine lazily on the first synthesis call (rather than
//! at app startup) because the user picks the bundle interactively
//! in the AI providers settings — there's nothing useful to load
//! until the config carries valid paths.
//!
//! Sherpa's offline TTS API is one-shot: it produces a complete
//! utterance in one call, no streaming. The adapter delivers the
//! whole utterance as a single `TtsChunk`. Existing call sites
//! already handle one-chunk providers, so this works through the
//! same plumbing as the cloud / openai-tts options will when
//! they're wired up.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::TtsProviderConfig;
use crate::runtimes::sherpa_onnx::tts::SherpaOnnxTtsRuntime;
use crate::tts::{ChunkSink, TtsCapabilities, TtsChunk, TtsError, TtsProvider, TtsRequest};

pub struct SherpaOnnxTts {
    cfg: TtsProviderConfig,
    state: Arc<Mutex<Option<SherpaOnnxTtsRuntime>>>,
}

impl SherpaOnnxTts {
    pub fn new(cfg: TtsProviderConfig, state: Arc<Mutex<Option<SherpaOnnxTtsRuntime>>>) -> Self {
        Self { cfg, state }
    }
}

#[async_trait]
impl TtsProvider for SherpaOnnxTts {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> TtsCapabilities {
        // Sample rate isn't known until the bundle is loaded —
        // sherpa reports it per generated audio. Surfacing 0 here
        // tells callers "ask the chunk." The audio sink path
        // already reads `sample_rate` off each chunk, so this is
        // safe.
        TtsCapabilities {
            sample_rate: 0,
            // sherpa's offline TTS is one-shot, not streaming. We
            // still emit through `synthesize_stream` because that's
            // the trait surface; it just produces a single chunk.
            streaming: false,
            voices: Vec::new(),
        }
    }

    async fn synthesize_stream(
        &self,
        req: TtsRequest,
        on_chunk: ChunkSink,
    ) -> Result<(), TtsError> {
        let cfg_blob = self.cfg.sherpa_onnx.clone().ok_or_else(|| {
            TtsError::NotReady(
                "sherpa-onnx TTS: provider config missing the `sherpa_onnx` block".to_string(),
            )
        })?;
        let state = self.state.clone();
        let mut sink = on_chunk;
        tokio::task::spawn_blocking(move || -> Result<(), TtsError> {
            let mut guard = state.blocking_lock();
            if guard.is_none() {
                let runtime = SherpaOnnxTtsRuntime::load(&cfg_blob).map_err(TtsError::NotReady)?;
                *guard = Some(runtime);
            }
            let rt = guard.as_mut().expect("just initialized above");
            let audio = rt
                .synthesize(&req.text, &req.voice)
                .map_err(TtsError::Synthesize)?;
            let chunk = TtsChunk {
                samples: audio.samples,
                sample_rate: audio.sample_rate,
            };
            // Single chunk — caller's `false` return doesn't have
            // anything left to interrupt, so we ignore it.
            let _ = sink(chunk);
            Ok(())
        })
        .await
        .map_err(|join_err| {
            TtsError::Synthesize(format!("sherpa-onnx TTS task panicked: {}", join_err))
        })?
    }
}
