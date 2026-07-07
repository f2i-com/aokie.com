//! Sherpa-onnx STT runtime — thin wrapper around `sherpa_rs`'s
//! offline recognizers. Today this covers `WhisperRecognizer`
//! (offline Whisper bundles published by k2-fsa); Zipformer /
//! Paraformer follow as the recognizer config shapes settle.
//!
//! The adapter above this calls `transcribe` once per utterance —
//! sherpa's offline API is synchronous and decodes the buffered
//! samples in one shot, so the adapter pushes the call through
//! `spawn_blocking` and surfaces the resulting text.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::SherpaSttConfig;

/// Engine-tagged sherpa STT instance — same dispatch idea as the
/// TTS runtime. Whisper is the only engine wired today.
///
/// Gated behind the `sherpa` cargo feature; without it the runtime
/// still exists as a unit struct so callers can hold its `State`
/// shape, but `load`/`transcribe` return errors.
#[cfg(feature = "sherpa")]
pub enum SherpaOnnxSttEngine {
    Whisper(sherpa_rs::whisper::WhisperRecognizer),
}

pub struct SherpaOnnxSttRuntime {
    #[cfg(feature = "sherpa")]
    engine: SherpaOnnxSttEngine,
}

#[derive(Default)]
pub struct SherpaOnnxSttState {
    pub inner: Arc<Mutex<Option<SherpaOnnxSttRuntime>>>,
}

#[cfg(feature = "sherpa")]
impl SherpaOnnxSttRuntime {
    pub fn load(cfg: &SherpaSttConfig) -> Result<Self, String> {
        let engine = match cfg.engine.as_str() {
            "whisper" | "" => SherpaOnnxSttEngine::Whisper(build_whisper(cfg)?),
            unknown => {
                return Err(format!(
                    "sherpa-onnx STT: unsupported engine {:?} (only `whisper` is wired today)",
                    unknown
                ));
            }
        };
        Ok(Self { engine })
    }

    /// Transcribe an utterance. Samples must be 16 kHz mono f32 in
    /// [-1, 1] — the same input shape the trait surface promises.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String, String> {
        match &mut self.engine {
            SherpaOnnxSttEngine::Whisper(w) => {
                // Sherpa's whisper bundle is internally fixed at
                // 16 kHz — we pass it explicitly because the
                // recognizer accepts the rate per call (some bundles
                // use 8 kHz feature configs).
                let result = w.transcribe(16_000, samples);
                Ok(result.text)
            }
        }
    }
}

/// Stub impl when the `sherpa` feature is disabled — see the
/// matching block in `tts.rs` for the rationale (Windows CI's
/// `sherpa-rs-sys` CMake build).
#[cfg(not(feature = "sherpa"))]
impl SherpaOnnxSttRuntime {
    pub fn load(_cfg: &SherpaSttConfig) -> Result<Self, String> {
        Err(
            "sherpa-onnx STT requires the `sherpa` cargo feature, which was disabled at build time"
                .to_string(),
        )
    }
    pub fn transcribe(&mut self, _samples: &[f32]) -> Result<String, String> {
        Err(
            "sherpa-onnx STT requires the `sherpa` cargo feature, which was disabled at build time"
                .to_string(),
        )
    }
}

#[cfg(feature = "sherpa")]
fn build_whisper(cfg: &SherpaSttConfig) -> Result<sherpa_rs::whisper::WhisperRecognizer, String> {
    if cfg.encoder_path.trim().is_empty() {
        return Err("sherpa-onnx STT: encoder_path is required".to_string());
    }
    if cfg.decoder_path.trim().is_empty() {
        return Err("sherpa-onnx STT: decoder_path is required".to_string());
    }
    if cfg.tokens_path.trim().is_empty() {
        return Err("sherpa-onnx STT: tokens_path is required".to_string());
    }
    // File-existence checks come after the empty-string ones so
    // load_rejects_empty_paths gets a stable "_path is required"
    // message regardless of what's on disk in the test sandbox.
    require_file(&cfg.encoder_path, "encoder_path")?;
    require_file(&cfg.decoder_path, "decoder_path")?;
    require_file(&cfg.tokens_path, "tokens_path")?;
    let language = if cfg.language.trim().is_empty() {
        "en".to_string()
    } else {
        cfg.language.clone()
    };
    let num_threads = if cfg.num_threads >= 1 {
        cfg.num_threads
    } else {
        1
    };
    let whisper_cfg = sherpa_rs::whisper::WhisperConfig {
        encoder: cfg.encoder_path.clone(),
        decoder: cfg.decoder_path.clone(),
        tokens: cfg.tokens_path.clone(),
        language,
        bpe_vocab: None,
        tail_paddings: if cfg.tail_paddings != 0 {
            Some(cfg.tail_paddings)
        } else {
            None
        },
        provider: None,
        num_threads: Some(num_threads),
        debug: false,
    };
    sherpa_rs::whisper::WhisperRecognizer::new(whisper_cfg)
        .map_err(|e| format!("sherpa-onnx whisper init: {}", e))
}

#[cfg(feature = "sherpa")]
fn require_file(path: &str, label: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Err(format!(
            "sherpa-onnx STT: {} {:?} does not exist",
            label, path
        ));
    }
    if !p.is_file() {
        return Err(format!(
            "sherpa-onnx STT: {} {:?} is not a regular file",
            label, path
        ));
    }
    Ok(())
}

#[cfg(all(test, feature = "sherpa"))]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_empty_paths() {
        let cfg = SherpaSttConfig::default();
        match SherpaOnnxSttRuntime::load(&cfg) {
            Err(msg) => assert!(msg.contains("encoder_path"), "got: {}", msg),
            Ok(_) => panic!("expected an encoder_path error"),
        }
    }

    #[test]
    fn load_rejects_unknown_engine() {
        let cfg = SherpaSttConfig {
            engine: "zipformer".to_string(),
            encoder_path: "e.onnx".to_string(),
            decoder_path: "d.onnx".to_string(),
            tokens_path: "tokens.txt".to_string(),
            ..Default::default()
        };
        match SherpaOnnxSttRuntime::load(&cfg) {
            Err(msg) => assert!(msg.contains("unsupported engine"), "got: {}", msg),
            Ok(_) => panic!("expected an unsupported-engine error"),
        }
    }
}
