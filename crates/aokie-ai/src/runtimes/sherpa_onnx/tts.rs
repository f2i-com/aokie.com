//! Sherpa-onnx TTS runtime — thin wrapper around `sherpa_rs::tts::*`.
//!
//! The runtime holds whichever sherpa engine the config picked
//! (today: VITS only — covers Piper voices, the most common
//! pretrained format). The adapter above this calls `synthesize`
//! once per request; sherpa's offline TTS API doesn't stream chunks
//! itself, so the adapter delivers the whole utterance as a single
//! `TtsChunk`. That matches the existing on-device adapter's
//! contract — call sites already handle one-chunk providers.
//!
//! Bundle layout for VITS (matches the sherpa-onnx release tarballs):
//!
//! ```text
//! voice-bundle/
//!   model.onnx
//!   tokens.txt
//!   espeak-ng-data/         (optional, Piper voices)
//!   lexicon.txt             (optional)
//! ```
//!
//! The config carries absolute paths for each piece — we don't
//! make assumptions about the directory layout because users will
//! download voices to whichever folder they like.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::SherpaTtsConfig;

/// Engine-tagged sherpa TTS instance. The variants exist so the
/// `synthesize` dispatch is a regular `match` instead of dynamic
/// dispatch through a trait object — there are only a handful of
/// engines and they each have a slightly different config shape.
///
/// Gated behind the `sherpa` cargo feature; the rest of the module
/// keeps a stub `SherpaOnnxTtsRuntime` available so consumers can
/// still reference the type when sherpa-rs is compiled out.
#[cfg(feature = "sherpa")]
pub enum SherpaOnnxTtsEngine {
    Vits(sherpa_rs::tts::VitsTts),
    Kokoro(sherpa_rs::tts::KokoroTts),
}

pub struct SherpaOnnxTtsRuntime {
    #[cfg(feature = "sherpa")]
    engine: SherpaOnnxTtsEngine,
    /// Speaker id used when the request's `voice` field is empty
    /// or unparseable as an integer.
    #[cfg_attr(not(feature = "sherpa"), allow(dead_code))]
    default_speaker_id: i32,
    /// Synthesis speed handed to sherpa per call. The adapter
    /// surfaces `1.0` to callers; users override via the JSON
    /// config rather than per-request, so we cache it here.
    #[cfg_attr(not(feature = "sherpa"), allow(dead_code))]
    speed: f32,
}

#[derive(Default)]
pub struct SherpaOnnxTtsState {
    pub inner: Arc<Mutex<Option<SherpaOnnxTtsRuntime>>>,
}

/// One synthesized utterance — sample buffer plus the engine's
/// native sample rate. Callers resample as needed.
pub struct SherpaOnnxTtsAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

#[cfg(feature = "sherpa")]
impl SherpaOnnxTtsRuntime {
    /// Build the engine described by `cfg`. Errors come back as
    /// human-readable strings — the adapter wraps them in
    /// `TtsError::NotReady` so the UI can surface them.
    ///
    /// We pre-validate file existence here because sherpa-rs's
    /// `VitsTts::new` / `KokoroTts::new` swallow load failures and
    /// return a struct holding a null pointer; the next `create()`
    /// call dereferences null and SIGSEGVs. Catching missing paths
    /// up-front converts that crash into a `NotReady` error the
    /// settings UI can surface cleanly.
    pub fn load(cfg: &SherpaTtsConfig) -> Result<Self, String> {
        // Fast checks first: missing-field / unknown-engine errors
        // shouldn't depend on file I/O. The require_file gauntlet
        // runs only after the engine arm matches.
        if cfg.model_path.trim().is_empty() {
            return Err("sherpa-onnx TTS: model_path is required".to_string());
        }
        if cfg.tokens_path.trim().is_empty() {
            return Err("sherpa-onnx TTS: tokens_path is required".to_string());
        }
        let engine = match cfg.engine.as_str() {
            "vits" | "" => {
                validate_common_paths(cfg)?;
                SherpaOnnxTtsEngine::Vits(build_vits(cfg))
            }
            "kokoro" => {
                if cfg.voices_path.trim().is_empty() {
                    return Err("sherpa-onnx TTS: kokoro engine requires voices_path".to_string());
                }
                validate_common_paths(cfg)?;
                require_file(&cfg.voices_path, "voices_path")?;
                SherpaOnnxTtsEngine::Kokoro(build_kokoro(cfg))
            }
            unknown => {
                return Err(format!(
                    "sherpa-onnx TTS: unsupported engine {:?} (vits / kokoro are wired)",
                    unknown
                ));
            }
        };
        Ok(Self {
            engine,
            default_speaker_id: cfg.default_speaker_id,
            speed: if cfg.speed > 0.0 { cfg.speed } else { 1.0 },
        })
    }

    /// Synthesize one utterance. The voice string is parsed as a
    /// sherpa speaker id; if it's empty or non-numeric we fall back
    /// to the config default. That matches sherpa's API (voice
    /// selection is an integer, not a name) — providers exposing
    /// human-readable voice names are wrapped at a higher layer.
    pub fn synthesize(&mut self, text: &str, voice: &str) -> Result<SherpaOnnxTtsAudio, String> {
        let sid = match parse_speaker_id(voice) {
            Some(n) => n,
            None => {
                if !voice.trim().is_empty() {
                    // Voice came in as a non-empty non-integer. Most
                    // likely the operator left a Pocket-TTS preset
                    // name ("alba") in their config but switched the
                    // active TTS to sherpa-onnx. Warn so they don't
                    // wonder why the voice doesn't match — sherpa
                    // can't resolve preset names.
                    eprintln!(
                        "[sherpa-onnx TTS] voice {:?} is not numeric — using default_speaker_id={}",
                        voice, self.default_speaker_id
                    );
                }
                self.default_speaker_id
            }
        };
        match &mut self.engine {
            SherpaOnnxTtsEngine::Vits(v) => {
                let audio = v
                    .create(text, sid, self.speed)
                    .map_err(|e| format!("vits synth: {}", e))?;
                Ok(SherpaOnnxTtsAudio {
                    samples: audio.samples,
                    sample_rate: audio.sample_rate,
                })
            }
            SherpaOnnxTtsEngine::Kokoro(k) => {
                let audio = k
                    .create(text, sid, self.speed)
                    .map_err(|e| format!("kokoro synth: {}", e))?;
                Ok(SherpaOnnxTtsAudio {
                    samples: audio.samples,
                    sample_rate: audio.sample_rate,
                })
            }
        }
    }
}

/// Stub impl when the `sherpa` feature is disabled — every method
/// returns a clear "feature not compiled in" error. Mirrors the
/// surface the adapter calls into so the rest of the codebase
/// compiles without any cfg branching at the call sites.
#[cfg(not(feature = "sherpa"))]
impl SherpaOnnxTtsRuntime {
    pub fn load(_cfg: &SherpaTtsConfig) -> Result<Self, String> {
        Err(
            "sherpa-onnx TTS requires the `sherpa` cargo feature, which was disabled at build time"
                .to_string(),
        )
    }
    pub fn synthesize(&mut self, _text: &str, _voice: &str) -> Result<SherpaOnnxTtsAudio, String> {
        Err(
            "sherpa-onnx TTS requires the `sherpa` cargo feature, which was disabled at build time"
                .to_string(),
        )
    }
}

#[cfg(feature = "sherpa")]
fn build_kokoro(cfg: &SherpaTtsConfig) -> sherpa_rs::tts::KokoroTts {
    let length_scale = if cfg.length_scale > 0.0 {
        cfg.length_scale
    } else {
        1.0
    };
    let kokoro_cfg = sherpa_rs::tts::KokoroTtsConfig {
        model: cfg.model_path.clone(),
        voices: cfg.voices_path.clone(),
        tokens: cfg.tokens_path.clone(),
        data_dir: cfg.data_dir.clone(),
        dict_dir: cfg.dict_dir.clone(),
        lexicon: cfg.lexicon.clone(),
        length_scale,
        onnx_config: sherpa_rs::OnnxConfig::default(),
        common_config: sherpa_rs::tts::CommonTtsConfig {
            silence_scale: cfg.silence_scale,
            ..Default::default()
        },
        lang: cfg.lang.clone(),
    };
    sherpa_rs::tts::KokoroTts::new(kokoro_cfg)
}

#[cfg(feature = "sherpa")]
fn build_vits(cfg: &SherpaTtsConfig) -> sherpa_rs::tts::VitsTts {
    let length_scale = if cfg.length_scale > 0.0 {
        cfg.length_scale
    } else {
        1.0
    };
    let vits_cfg = sherpa_rs::tts::VitsTtsConfig {
        model: cfg.model_path.clone(),
        lexicon: cfg.lexicon.clone(),
        dict_dir: cfg.dict_dir.clone(),
        tokens: cfg.tokens_path.clone(),
        data_dir: cfg.data_dir.clone(),
        length_scale,
        noise_scale: cfg.noise_scale,
        noise_scale_w: cfg.noise_scale_w,
        silence_scale: cfg.silence_scale,
        onnx_config: sherpa_rs::OnnxConfig::default(),
        tts_config: sherpa_rs::tts::CommonTtsConfig::default(),
    };
    sherpa_rs::tts::VitsTts::new(vits_cfg)
}

#[cfg(feature = "sherpa")]
fn parse_speaker_id(voice: &str) -> Option<i32> {
    let trimmed = voice.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<i32>().ok()
}

#[cfg(feature = "sherpa")]
fn validate_common_paths(cfg: &SherpaTtsConfig) -> Result<(), String> {
    require_file(&cfg.model_path, "model_path")?;
    require_file(&cfg.tokens_path, "tokens_path")?;
    if !cfg.data_dir.trim().is_empty() {
        require_dir(&cfg.data_dir, "data_dir")?;
    }
    if !cfg.lexicon.trim().is_empty() {
        require_file(&cfg.lexicon, "lexicon")?;
    }
    if !cfg.dict_dir.trim().is_empty() {
        require_dir(&cfg.dict_dir, "dict_dir")?;
    }
    Ok(())
}

#[cfg(feature = "sherpa")]
fn require_file(path: &str, label: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Err(format!(
            "sherpa-onnx TTS: {} {:?} does not exist",
            label, path
        ));
    }
    if !p.is_file() {
        return Err(format!(
            "sherpa-onnx TTS: {} {:?} is not a regular file",
            label, path
        ));
    }
    Ok(())
}

#[cfg(feature = "sherpa")]
fn require_dir(path: &str, label: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Err(format!(
            "sherpa-onnx TTS: {} {:?} does not exist",
            label, path
        ));
    }
    if !p.is_dir() {
        return Err(format!(
            "sherpa-onnx TTS: {} {:?} is not a directory",
            label, path
        ));
    }
    Ok(())
}

#[cfg(all(test, feature = "sherpa"))]
mod tests {
    use super::*;

    #[test]
    fn voice_parsing_falls_back_when_non_numeric() {
        assert_eq!(parse_speaker_id("0"), Some(0));
        assert_eq!(parse_speaker_id("12"), Some(12));
        assert_eq!(parse_speaker_id(""), None);
        assert_eq!(parse_speaker_id("alba"), None);
    }

    #[test]
    fn load_rejects_empty_paths() {
        let cfg = SherpaTtsConfig::default();
        match SherpaOnnxTtsRuntime::load(&cfg) {
            Err(msg) => assert!(msg.contains("model_path"), "got: {}", msg),
            Ok(_) => panic!("expected an error for empty paths"),
        }
    }

    #[test]
    fn load_rejects_unknown_engine() {
        let cfg = SherpaTtsConfig {
            engine: "matcha".to_string(),
            model_path: "model.onnx".to_string(),
            tokens_path: "tokens.txt".to_string(),
            ..Default::default()
        };
        match SherpaOnnxTtsRuntime::load(&cfg) {
            Err(msg) => assert!(msg.contains("unsupported engine"), "got: {}", msg),
            Ok(_) => panic!("expected an unsupported-engine error"),
        }
    }

    #[test]
    fn load_rejects_kokoro_without_voices() {
        let cfg = SherpaTtsConfig {
            engine: "kokoro".to_string(),
            model_path: "model.onnx".to_string(),
            tokens_path: "tokens.txt".to_string(),
            voices_path: String::new(),
            ..Default::default()
        };
        match SherpaOnnxTtsRuntime::load(&cfg) {
            Err(msg) => assert!(msg.contains("voices_path"), "got: {}", msg),
            Ok(_) => panic!("expected a voices_path error"),
        }
    }
}
