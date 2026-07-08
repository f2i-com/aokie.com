//! In-process text-to-speech for the voice receptionist (behind the `voice`
//! feature). Wraps aokie-ai's Pocket-TTS ONNX runtime and adapts its output to
//! the radio's SCO audio path: synthesize text → f32 @ the model rate (~24 kHz)
//! → resample to the negotiated SCO rate (8 kHz CVSD / 16 kHz mSBC) → i16 PCM
//! ready for `BluetoothManager::send_audio`.
//!
//! Loading the bundle is heavy (~200 MB of ONNX graphs) so callers load once,
//! lazily, on the first thing Aokie needs to say.

use aokie_ai::runtimes::onnx_tts::{onnx_tts_models_dir, OnnxTtsRuntime};
use aokie_ai::runtimes::parakeet_onnx::ParakeetOnnxRuntime;

/// In-process speech-to-text for the receptionist — the "ears". Wraps aokie-ai's
/// Parakeet ONNX transducer (int8, ~600 MB) which transcribes a 16 kHz mono f32
/// utterance to text. Loaded once, lazily, on the first caller utterance.
pub struct SttEngine {
    rt: ParakeetOnnxRuntime,
}

impl SttEngine {
    /// Load the Parakeet bundle from `<app_data>/models/parakeet`
    /// (encoder.int8.onnx + decoder_joint.int8.onnx + tokenizer.model). Shares the
    /// same ONNX Runtime DLL as the TTS engine (resolved next to the plugin).
    pub fn load() -> Result<Self, String> {
        ensure_ort_dylib();
        let app_data = aokie_core::paths::app_data_dir()
            .ok_or_else(|| "no app_data_dir for the STT models".to_string())?;
        let dir = app_data.join("models").join("parakeet");
        let rt = ParakeetOnnxRuntime::load(
            &dir.join("encoder.int8.onnx"),
            &dir.join("decoder_joint.int8.onnx"),
            &dir.join("tokenizer.model"),
            2,
        )?;
        Ok(Self { rt })
    }

    /// Transcribe a 16 kHz mono f32 utterance to trimmed text (may be empty).
    pub fn transcribe(&mut self, samples_16k: &[f32]) -> Result<String, String> {
        Ok(self.rt.transcribe(samples_16k)?.trim().to_string())
    }
}

/// Resample i16 PCM at `from` Hz to 16 kHz mono f32 (what the STT engine wants).
pub fn to_f32_16k(samples: &[i16], from: u32) -> Vec<f32> {
    let f: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
    resample_linear(&f, from, 16_000)
}

/// RMS amplitude of an i16 frame in i16 units (0..32767) — the VAD's speech gate.
pub fn frame_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / samples.len() as f64).sqrt() as f32
}

pub struct TtsEngine {
    rt: OnnxTtsRuntime,
    native_rate: u32,
}

impl TtsEngine {
    /// Load the Pocket-TTS bundle from the aokie app-data models dir
    /// (`<app_data>/models/pocket_tts_onnx`). Requires the ONNX Runtime DLL to
    /// be resolvable at load time (shipped next to the plugin binary).
    pub fn load() -> Result<Self, String> {
        ensure_ort_dylib();
        let app_data = aokie_core::paths::app_data_dir()
            .ok_or_else(|| "no app_data_dir for the TTS models".to_string())?;
        let dir = onnx_tts_models_dir(&app_data, None)?;
        let rt = OnnxTtsRuntime::open(&dir)?;
        let native_rate = rt.sample_rate();
        Ok(Self { rt, native_rate })
    }

    /// Synthesize `text` (with reference `voice`, empty = the bundle default) to
    /// mono i16 PCM at `target_rate` — the current SCO rate — ready to hand to
    /// `send_audio`.
    pub fn synthesize(&mut self, text: &str, voice: &str, target_rate: u32) -> Result<Vec<i16>, String> {
        let mut f32_samples: Vec<f32> = Vec::new();
        self.rt.synthesize_stream(text, voice, |chunk, _rate| {
            f32_samples.extend_from_slice(chunk);
            true
        })?;
        let resampled = resample_linear(&f32_samples, self.native_rate, target_rate);
        Ok(resampled
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect())
    }
}

/// Point `ort` at the ONNX Runtime DLL shipped next to the plugin binary, unless
/// the operator already set `ORT_DYLIB_PATH`. `ort` is built with `load-dynamic`,
/// so it resolves onnxruntime.dll at runtime from this env var.
fn ensure_ort_dylib() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["onnxruntime.dll", "onnxruntime_1.25.0.dll"] {
                let dll = dir.join(name);
                if dll.exists() {
                    std::env::set_var("ORT_DYLIB_PATH", &dll);
                    eprintln!("[aokie-plugin] ORT_DYLIB_PATH → {}", dll.display());
                    return;
                }
            }
        }
    }
}

/// Minimal linear-interpolation resampler (mono f32). Speech over an 8/16 kHz
/// SCO link doesn't need a high-order filter; linear is clean enough and cheap.
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || to == 0 || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((input.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = input.get(idx).copied().unwrap_or(0.0);
        let b = input.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}
