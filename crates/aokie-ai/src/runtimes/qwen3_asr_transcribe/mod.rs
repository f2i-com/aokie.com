//! Qwen3-ASR-0.6B STT runtime — backed by the vendored `transcribe-rs`
//! crate (`crates/transcribe-rs`). Qwen3-ASR is an encoder-decoder
//! Whisper-style seq2seq model whose decoder is the Qwen3-0.6B chat
//! backbone, exported as int4-quantised ONNX by andrewleech. Multi-
//! lingual (en/zh/ja/ko/fr/de/es/ru/pt/ar) and tends to outperform
//! Whisper on mixed accents at the cost of a 2 GB on-disk footprint.
//!
//! Like the other transcribe-rs-backed runtimes, we own the lifecycle
//! (load / unload) but the inference loop lives inside
//! `transcribe_rs::onnx::qwen3_asr::Qwen3AsrModel`.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use transcribe_rs::onnx::qwen3_asr::{Qwen3AsrModel, Qwen3AsrVariant};
use transcribe_rs::onnx::Quantization;

pub struct Qwen3AsrTranscribeRuntime {
    model: Qwen3AsrModel,
}

#[derive(Default)]
pub struct Qwen3AsrTranscribeState {
    pub inner: Arc<Mutex<Option<Qwen3AsrTranscribeRuntime>>>,
}

impl Qwen3AsrTranscribeRuntime {
    /// Load Qwen3-ASR from a directory containing the encoder /
    /// decoder_init / decoder_step ONNX files, plus `tokenizer.json`
    /// and `embed_tokens.bin`. The Int4 variant is the only published
    /// quantisation today; FP32 is included as a fallback name only.
    pub fn load(
        model_dir: &Path,
        variant: Qwen3AsrVariant,
        quantization: Quantization,
    ) -> Result<Self, String> {
        let model = Qwen3AsrModel::load(model_dir, variant, &quantization)
            .map_err(|e| format!("qwen3-asr load: {}", e))?;
        Ok(Self { model })
    }

    /// Transcribe a 16 kHz mono f32 utterance. Empty input → empty.
    /// `language` is a BCP-47 code (e.g. "en") that pins the decoder
    /// to that language via the prompt-priming hook in transcribe-rs;
    /// `None` (or an empty/unrecognised code) falls back to the model's
    /// built-in language-ID step.
    pub fn transcribe(
        &mut self,
        samples_16k: &[f32],
        language: Option<&str>,
    ) -> Result<String, String> {
        if samples_16k.is_empty() {
            return Ok(String::new());
        }
        let opts = transcribe_rs::TranscribeOptions {
            language: language.map(|s| s.to_string()),
            ..Default::default()
        };
        use transcribe_rs::SpeechModel;
        let res = self
            .model
            .transcribe(samples_16k, &opts)
            .map_err(|e| format!("qwen3-asr transcribe: {}", e))?;
        Ok(res.text)
    }
}
