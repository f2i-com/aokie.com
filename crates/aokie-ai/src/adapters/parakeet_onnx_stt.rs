//! Parakeet-ONNX STT adapter — wraps `parakeet_onnx::ParakeetOnnxRuntime`
//! in the `SttProvider` trait surface. Parakeet doesn't lazy-load like
//! the sherpa adapter does (parakeet runtime construction is fast — no
//! C++ recognizer to allocate, just two ORT session builds), so the
//! caller MUST `initialize_parakeet` the state before transcribe.
//!
//! Same shape as `whisper_candle_stt` and `sherpa_onnx_stt`:
//! `transcribe()` runs the heavy work on `spawn_blocking` so it doesn't
//! block the runtime; the trait surface is async.
//!
//! Per-request `language` hints are silently dropped — Parakeet is
//! English-only by construction.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::config::SttProviderConfig;
use crate::runtimes::parakeet_onnx::ParakeetOnnxRuntime;
use crate::stt::{SttCapabilities, SttError, SttProvider, SttRequest};

pub struct ParakeetOnnxStt {
    cfg: SttProviderConfig,
    state: Arc<Mutex<Option<ParakeetOnnxRuntime>>>,
}

impl ParakeetOnnxStt {
    pub fn new(cfg: SttProviderConfig, state: Arc<Mutex<Option<ParakeetOnnxRuntime>>>) -> Self {
        Self { cfg, state }
    }
}

#[async_trait]
impl SttProvider for ParakeetOnnxStt {
    fn id(&self) -> &str {
        &self.cfg.id
    }

    fn capabilities(&self) -> SttCapabilities {
        SttCapabilities {
            // Parakeet is offline-only — one transcribe call per
            // utterance. Same shape as the candle/sherpa Whisper
            // adapters.
            streaming: false,
            // English-only model. Per-request language hints would be
            // misleading.
            language_hint: false,
        }
    }

    async fn transcribe(&self, req: SttRequest) -> Result<String, SttError> {
        // Check the runtime is loaded — Parakeet doesn't lazy-load
        // (load is heavy enough — ~650 MB encoder — that doing it
        // on the first transcribe would stall the call). The
        // `initialize_parakeet` Tauri command populates this state.
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || -> Result<String, SttError> {
            let mut guard = state.blocking_lock();
            let rt = guard.as_mut().ok_or_else(|| {
                SttError::NotReady(
                    "parakeet runtime not loaded — call initialize_parakeet first \
                     (or visit AI Stack to retry)"
                        .to_string(),
                )
            })?;
            rt.transcribe(&req.samples_16k)
                .map_err(SttError::Transcribe)
        })
        .await
        .map_err(|join_err| {
            SttError::Transcribe(format!("parakeet STT task panicked: {}", join_err))
        })?
    }
}
