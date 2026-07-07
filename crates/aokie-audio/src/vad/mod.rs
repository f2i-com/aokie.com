//! Voice Activity Detection module using Silero VAD
//!
//! Provides accurate speech detection for the phone call processing pipeline.
//! Only the `StreamingVad::accept_samples`/`has_segments`/`pop_segment` path
//! is currently wired; legacy helpers are kept behind `allow(dead_code)` for
//! future reuse.
#![allow(dead_code)]

// VAD is backed by Silero via sherpa-rs when the `sherpa` cargo
// feature is enabled. Without it, `StreamingVad::new` returns a
// "feature not compiled in" error and the consuming BT call body
// falls through to "no VAD" — which is the right answer because
// the BT body itself is Windows-only and Windows release builds
// always have sherpa enabled. The feature-off path exists purely
// so `--no-default-features` Windows CI compiles the lib without
// dragging in `sherpa-rs-sys`'s CMake build.
#[cfg(feature = "sherpa")]
pub use sherpa_rs::silero_vad::SpeechSegment;
#[cfg(feature = "sherpa")]
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};

/// Stub `SpeechSegment` used when the `sherpa` feature is off so
/// the rest of the codebase can keep referencing the type. Same
/// public shape as `sherpa_rs::silero_vad::SpeechSegment`'s minimal
/// surface.
#[cfg(not(feature = "sherpa"))]
#[derive(Debug, Clone, Default)]
pub struct SpeechSegment {
    pub start: f32,
    pub samples: Vec<f32>,
}

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Configuration for VAD
#[derive(Debug, Clone)]
pub struct VadConfig {
    /// Path to the silero_vad.onnx model
    pub model_path: String,
    /// Minimum silence duration (seconds) to consider speech ended
    pub min_silence_duration: f32,
    /// Minimum speech duration (seconds) to be valid
    pub min_speech_duration: f32,
    /// Maximum speech duration (seconds) before forced split
    pub max_speech_duration: f32,
    /// VAD threshold (0.0-1.0, higher = more sensitive)
    pub threshold: f32,
    /// Audio sample rate
    pub sample_rate: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            model_path: String::new(),
            min_silence_duration: 0.3, // 300ms silence to end speech (faster response)
            min_speech_duration: 0.2,  // Minimum 200ms to be valid speech
            max_speech_duration: 30.0, // Max 30 seconds before forced split
            threshold: 0.5,            // Default threshold
            sample_rate: 16000,        // 16kHz for phone calls
        }
    }
}

/// Silero VAD wrapper for streaming voice activity detection.
///
/// The `vad` field is only present when the `sherpa` feature is
/// compiled in; without it the struct keeps its public surface but
/// `new()` returns an error so the consumer never gets an instance.
pub struct StreamingVad {
    #[cfg(feature = "sherpa")]
    vad: SileroVad,
    sample_rate: u32,
}

#[cfg(feature = "sherpa")]
impl StreamingVad {
    /// Create a new streaming VAD instance
    pub fn new(config: VadConfig) -> Result<Self, String> {
        let vad_config = SileroVadConfig {
            model: config.model_path,
            min_silence_duration: config.min_silence_duration,
            min_speech_duration: config.min_speech_duration,
            max_speech_duration: config.max_speech_duration,
            threshold: config.threshold,
            sample_rate: config.sample_rate,
            window_size: 512, // Standard Silero VAD window size
            provider: None,   // Use CPU - VAD is lightweight and avoids CUDA conflicts
            num_threads: Some(4),
            debug: false,
        };

        // Buffer size in seconds - we want enough for max speech duration
        let buffer_seconds = config.max_speech_duration + 5.0;

        let vad = SileroVad::new(vad_config, buffer_seconds)
            .map_err(|e| format!("Failed to create SileroVad: {}", e))?;

        Ok(Self {
            vad,
            sample_rate: config.sample_rate,
        })
    }

    /// Accept audio samples (i16 PCM) for processing
    pub fn accept_samples(&mut self, samples: &[i16]) {
        // Convert i16 to f32
        let f32_samples: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();

        self.vad.accept_waveform(f32_samples);
    }

    /// Check if current audio contains speech
    pub fn is_speech(&mut self) -> bool {
        self.vad.is_speech()
    }

    /// Check if there are any detected speech segments ready
    pub fn has_segments(&mut self) -> bool {
        !self.vad.is_empty()
    }

    /// Get the next complete speech segment
    pub fn pop_segment(&mut self) -> Option<SpeechSegment> {
        if !self.vad.is_empty() {
            let segment = self.vad.front();
            self.vad.pop();
            Some(segment)
        } else {
            None
        }
    }

    /// Flush any remaining audio and return final segments
    pub fn flush(&mut self) -> Vec<SpeechSegment> {
        self.vad.flush();
        let mut segments = Vec::new();
        while !self.vad.is_empty() {
            let seg = self.vad.front();
            segments.push(seg);
            self.vad.pop();
        }
        segments
    }

    /// Clear the VAD state
    pub fn clear(&mut self) {
        self.vad.clear();
    }

    /// Get the sample rate
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Stub impl for `--no-default-features` builds — `new()` errors,
/// every other method is reachable only after a successful `new()`,
/// so they're either trivial defaults or unreachable in practice.
#[cfg(not(feature = "sherpa"))]
impl StreamingVad {
    pub fn new(_config: VadConfig) -> Result<Self, String> {
        Err("VAD requires the `sherpa` cargo feature, which was disabled at build time".to_string())
    }
    pub fn accept_samples(&mut self, _samples: &[i16]) {}
    pub fn is_speech(&mut self) -> bool {
        false
    }
    pub fn has_segments(&mut self) -> bool {
        false
    }
    pub fn pop_segment(&mut self) -> Option<SpeechSegment> {
        None
    }
    pub fn flush(&mut self) -> Vec<SpeechSegment> {
        Vec::new()
    }
    pub fn clear(&mut self) {}
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Thread-safe VAD state
pub struct VadState {
    pub instance: Arc<Mutex<Option<StreamingVad>>>,
}

impl Default for VadState {
    fn default() -> Self {
        Self {
            instance: Arc::new(Mutex::new(None)),
        }
    }
}

/// Get the VAD models directory.
///
/// The legacy Tauri app resolved the app-data dir from an `AppHandle`
/// (`app_handle.path().app_data_dir()`). This crate is Tauri-free, so
/// it goes through [`aokie_core::paths::app_data_dir`], which resolves
/// the identical `<RoamingAppData>/com.aokie.app` path.
pub fn get_vad_models_dir() -> Result<PathBuf, String> {
    let app_data_dir = aokie_core::paths::app_data_dir()
        .ok_or_else(|| "Failed to resolve app data dir".to_string())?;

    let models_dir = app_data_dir.join("models").join("vad");
    std::fs::create_dir_all(&models_dir)
        .map_err(|e| format!("Failed to create VAD models directory: {}", e))?;

    Ok(models_dir)
}

/// Download the Silero VAD model if not present.
///
/// Only compiled with the `sherpa` feature: the downloaded
/// `silero_vad.onnx` is only ever loaded by the sherpa-backed
/// [`StreamingVad`], so gating it keeps the default (feature-off) build
/// free of the `reqwest` HTTP stack.
#[cfg(feature = "sherpa")]
pub async fn ensure_vad_model(models_dir: &PathBuf) -> Result<PathBuf, String> {
    let model_path = models_dir.join("silero_vad.onnx");

    if model_path.exists() {
        println!("[VAD] Model already exists at {:?}", model_path);
        return Ok(model_path);
    }

    println!("[VAD] Downloading Silero VAD model...");

    // Silero VAD model URL from official repo
    let url =
        "https://github.com/snakers4/silero-vad/raw/master/src/silero_vad/data/silero_vad.onnx";

    let response = reqwest::get(url)
        .await
        .map_err(|e| format!("Failed to download VAD model: {}", e))?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to download VAD model: HTTP {}",
            response.status()
        ));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read VAD model bytes: {}", e))?;

    // Write to a sibling .partial first, fsync-via-close, then rename.
    // A direct write to model_path leaves a torn / truncated ONNX in
    // place if the process is killed (or `bytes` was a partial body) —
    // and the next launch happily reads the corrupted file. The
    // rename-on-close pattern guarantees `model_path` only ever points
    // at a fully-written body.
    let tmp_path = model_path.with_extension("onnx.partial");
    std::fs::write(&tmp_path, &bytes)
        .map_err(|e| format!("Failed to write VAD model staging file: {}", e))?;
    std::fs::rename(&tmp_path, &model_path)
        .map_err(|e| format!("Failed to commit VAD model file: {}", e))?;

    println!(
        "[VAD] Model downloaded to {:?} ({} bytes)",
        model_path,
        bytes.len()
    );
    Ok(model_path)
}
