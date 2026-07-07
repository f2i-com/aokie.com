//! UsefulSensors Moonshine STT runtime — backed by the vendored
//! `transcribe-rs` crate (`crates/transcribe-rs`). Moonshine is an
//! encoder-decoder seq2seq model designed for low-latency edge/mobile
//! ASR; it tends to handle phone-band audio (mSBC SCO, 300 Hz–7 kHz)
//! more cleanly than Whisper-family models because its training mix
//! emphasises short conversational utterances rather than long-form
//! studio audio.
//!
//! We keep the runtime in our own `runtimes/` tree so the app's
//! state-and-load lifecycle is consistent across providers (parakeet,
//! sherpa-onnx whisper, candle whisper, moonshine all live in
//! `runtimes::*`). The actual inference loop is owned by
//! `transcribe_rs::onnx::moonshine::MoonshineModel`.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use transcribe_rs::onnx::moonshine::{MoonshineModel, MoonshineVariant};
use transcribe_rs::onnx::Quantization;

/// Loaded Moonshine runtime. Holds the encoder + decoder ONNX
/// sessions and the tokenizer, ready to transcribe.
pub struct MoonshineTranscribeRuntime {
    model: MoonshineModel,
}

#[derive(Default)]
pub struct MoonshineTranscribeState {
    pub inner: Arc<Mutex<Option<MoonshineTranscribeRuntime>>>,
}

impl MoonshineTranscribeRuntime {
    /// Load Moonshine from a directory containing the encoder /
    /// decoder ONNX files plus `tokenizer.json`. Variant + quantization
    /// pick which of the engine's expected file names is used.
    pub fn load(
        model_dir: &Path,
        variant: MoonshineVariant,
        quantization: Quantization,
    ) -> Result<Self, String> {
        let model = MoonshineModel::load(model_dir, variant, &quantization)
            .map_err(|e| format!("moonshine load: {}", e))?;
        Ok(Self { model })
    }

    /// Transcribe a 16 kHz mono f32 utterance. Empty input → empty.
    pub fn transcribe(&mut self, samples_16k: &[f32]) -> Result<String, String> {
        if samples_16k.is_empty() {
            return Ok(String::new());
        }
        let opts = transcribe_rs::TranscribeOptions::default();
        // SpeechModel trait is in scope as the path-imported method
        // — but bringing the trait via `use` keeps the call site clean.
        use transcribe_rs::SpeechModel;
        let res = self
            .model
            .transcribe(samples_16k, &opts)
            .map_err(|e| format!("moonshine transcribe: {}", e))?;
        Ok(res.text)
    }
}
