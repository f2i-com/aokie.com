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
    crate::speech_wire::resample_linear(&f, from, 16_000)
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
    pub fn synthesize(
        &mut self,
        text: &str,
        voice: &str,
        target_rate: u32,
    ) -> Result<Vec<i16>, String> {
        let mut f32_samples: Vec<f32> = Vec::new();
        self.rt.synthesize_stream(text, voice, |chunk, _rate| {
            f32_samples.extend_from_slice(chunk);
            true
        })?;
        let resampled =
            crate::speech_wire::resample_linear(&f32_samples, self.native_rate, target_rate);
        Ok(resampled
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect())
    }

    /// Synthesize `text` to mono i16 PCM at the model's NATIVE rate (returned
    /// alongside). Callers that post-process the waveform (pitch-preserving
    /// time stretch for per-span speaking rates) use this so the stretch runs
    /// at full quality BEFORE the SCO downsample.
    pub fn synthesize_native(
        &mut self,
        text: &str,
        voice: &str,
    ) -> Result<(Vec<i16>, u32), String> {
        let mut f32_samples: Vec<f32> = Vec::new();
        self.rt.synthesize_stream(text, voice, |chunk, _rate| {
            f32_samples.extend_from_slice(chunk);
            true
        })?;
        Ok((
            f32_samples
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect(),
            self.native_rate,
        ))
    }

    /// Streaming synthesis: calls `on_pcm` with mono i16 PCM at `target_rate` as
    /// each TTS chunk is produced, so playback can start on the first chunk
    /// (~0.3 s) instead of after the whole utterance. `on_pcm` returns `false` to
    /// stop early (barge-in / hangup). Returns the total sample count emitted.
    pub fn synthesize_streaming(
        &mut self,
        text: &str,
        voice: &str,
        target_rate: u32,
        mut on_pcm: impl FnMut(&[i16]) -> bool,
    ) -> Result<usize, String> {
        let native = self.native_rate;
        let mut total = 0usize;
        self.rt.synthesize_stream(text, voice, |chunk, _rate| {
            // Per-chunk linear resample: the one-sample boundary discontinuity is
            // inaudible over an 8/16 kHz phone link and keeps latency minimal.
            let resampled = crate::speech_wire::resample_linear(chunk, native, target_rate);
            let pcm: Vec<i16> = resampled
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            total += pcm.len();
            on_pcm(&pcm)
        })?;
        Ok(total)
    }
}

/// AOK-VOICE-001: the startup voice-asset preflight verdict. `None` = no known
/// problem; `Some(reason)` = that half of the voice pipeline is KNOWN unable to
/// work (missing ONNX Runtime DLL / model files, with no HTTP endpoint
/// substituting for the in-process engine).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VoicePreflight {
    pub stt_error: Option<String>,
    pub tts_error: Option<String>,
}

/// Fast filesystem preflight (no model load — presence only, so radio startup
/// stays instant): can the receptionist plausibly hear (STT) and speak (TTS)?
/// An HTTP speech endpoint substitutes for the corresponding in-process engine
/// (the missing local models then don't matter). Corruption is caught later by
/// the live engine loads, which update the same status slots.
pub fn preflight_assets() -> VoicePreflight {
    ensure_ort_dylib();
    let ort_ok = std::env::var_os("ORT_DYLIB_PATH")
        .map(|p| std::path::Path::new(&p).is_file())
        .unwrap_or(false);
    let (stt_assets_ok, tts_assets_ok) = match aokie_core::paths::app_data_dir() {
        Some(app_data) => {
            let stt_dir = app_data.join("models").join("parakeet");
            let stt_ok = [
                "encoder.int8.onnx",
                "decoder_joint.int8.onnx",
                "tokenizer.model",
            ]
            .iter()
            .all(|f| stt_dir.join(f).is_file());
            let tts_ok = onnx_tts_models_dir(&app_data, None).is_ok();
            (stt_ok, tts_ok)
        }
        None => (false, false),
    };
    let stt_endpoint = std::env::var("AOKIE_STT_ENDPOINT").is_ok_and(|v| !v.trim().is_empty());
    let tts_endpoint = std::env::var("AOKIE_TTS_ENDPOINT").is_ok_and(|v| !v.trim().is_empty());
    preflight_decision(
        ort_ok,
        stt_assets_ok,
        tts_assets_ok,
        stt_endpoint,
        tts_endpoint,
    )
}

/// Pure decision half of [`preflight_assets`], unit-testable: which halves of
/// the voice pipeline are KNOWN broken given what's on disk / configured.
pub fn preflight_decision(
    ort_ok: bool,
    stt_assets_ok: bool,
    tts_assets_ok: bool,
    stt_endpoint: bool,
    tts_endpoint: bool,
) -> VoicePreflight {
    // An HTTP endpoint carries its half even with no local assets; local
    // assets additionally need the ONNX Runtime DLL to be loadable.
    let local_stt = ort_ok && stt_assets_ok;
    let local_tts = ort_ok && tts_assets_ok;
    let describe = |what: &str, assets_ok: bool| {
        if !ort_ok && assets_ok {
            format!(
                "{what} unavailable: the ONNX Runtime DLL was not found next to the plugin (and no HTTP {what} endpoint is configured)"
            )
        } else {
            format!(
                "{what} unavailable: the local model files are missing (and no HTTP {what} endpoint is configured)"
            )
        }
    };
    VoicePreflight {
        stt_error: (!local_stt && !stt_endpoint).then(|| describe("speech-to-text", stt_assets_ok)),
        tts_error: (!local_tts && !tts_endpoint).then(|| describe("text-to-speech", tts_assets_ok)),
    }
}

/// VOICE-001: the measured TTS→STT loopback self-test. Unlike the presence-only
/// preflight, this EXERCISES the pipeline: synthesize a known phrase with the
/// real TTS engine, transcribe the produced PCM with the real STT engine, and
/// verify the words survive the round trip. A corrupt model, broken ORT
/// provider, or silently-empty synthesis is caught HERE at radio startup —
/// before auto-answer arms — instead of by the first caller.
///
/// Heavy by design (~800 MB of transient ONNX graphs, seconds of inference):
/// callers run it on a dedicated thread. Both engines are loaded fresh and
/// dropped — the live lazy-loaded engines are untouched.
pub const SELF_TEST_PHRASE: &str = "Aokie self test one two three";

pub fn run_loopback_self_test() -> Result<String, String> {
    let mut tts = TtsEngine::load().map_err(|e| format!("TTS engine load failed: {e}"))?;
    let pcm = tts
        .synthesize(SELF_TEST_PHRASE, "", 16_000)
        .map_err(|e| format!("TTS synthesis failed: {e}"))?;
    // Anything under ~0.5 s of audio for a 5-word phrase is silence/garbage.
    if pcm.len() < 8_000 {
        return Err(format!(
            "TTS produced only {} samples (~{} ms) for the test phrase - synthesis is silent",
            pcm.len(),
            pcm.len() / 16
        ));
    }
    let mut stt = SttEngine::load().map_err(|e| format!("STT engine load failed: {e}"))?;
    let heard = stt
        .transcribe(&to_f32_16k(&pcm, 16_000))
        .map_err(|e| format!("STT transcription failed: {e}"))?;
    self_test_verdict(&heard).map(|()| heard)
}

/// Pure verdict half of the loopback, unit-testable: does the transcript prove
/// the round trip? STT phrasing wobbles ("1 2 3" vs "one two three"), so we
/// require a MAJORITY of the expected words, not an exact match.
pub fn self_test_verdict(heard: &str) -> Result<(), String> {
    let h = heard.to_ascii_lowercase();
    let expected: &[&[&str]] = &[
        &["self"],
        &["test"],
        &["one", "1"],
        &["two", "2", "to", "too"],
        &["three", "3"],
    ];
    let hits = expected
        .iter()
        .filter(|alts| alts.iter().any(|w| h.contains(w)))
        .count();
    if hits >= 3 {
        Ok(())
    } else {
        Err(format!(
            "loopback transcript matched only {hits}/5 expected words - heard {heard:?}"
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// AOK-VOICE-001 preflight decision table.
    #[test]
    fn preflight_all_local_assets_present_is_clean() {
        let p = preflight_decision(true, true, true, false, false);
        assert_eq!(p, VoicePreflight::default());
    }

    #[test]
    fn preflight_missing_models_is_a_known_failure() {
        let p = preflight_decision(true, false, true, false, false);
        assert!(p
            .stt_error
            .as_deref()
            .unwrap_or("")
            .contains("model files are missing"));
        assert_eq!(p.tts_error, None);

        let p = preflight_decision(true, true, false, false, false);
        assert_eq!(p.stt_error, None);
        assert!(p
            .tts_error
            .as_deref()
            .unwrap_or("")
            .contains("model files are missing"));
    }

    #[test]
    fn preflight_missing_ort_dll_fails_both_and_names_the_dll() {
        let p = preflight_decision(false, true, true, false, false);
        assert!(p
            .stt_error
            .as_deref()
            .unwrap_or("")
            .contains("ONNX Runtime DLL"));
        assert!(p
            .tts_error
            .as_deref()
            .unwrap_or("")
            .contains("ONNX Runtime DLL"));
    }

    /// VOICE-001: the loopback verdict tolerates STT phrasing wobble but
    /// refuses a transcript that doesn't prove the round trip.
    #[test]
    fn self_test_verdict_accepts_wobble_and_rejects_garbage() {
        assert!(self_test_verdict("Aokie self test one two three").is_ok());
        assert!(self_test_verdict("okie self test 1 2 3").is_ok());
        assert!(
            self_test_verdict("self test one").is_ok(),
            "3/5 words is enough"
        );
        assert!(self_test_verdict("").is_err());
        assert!(self_test_verdict("hello world").is_err());
        let err = self_test_verdict("mumble").unwrap_err();
        assert!(err.contains("0/5"), "got: {err}");
    }

    #[test]
    fn preflight_http_endpoints_substitute_for_local_engines() {
        // No local assets at all, but both endpoints configured → clean.
        let p = preflight_decision(false, false, false, true, true);
        assert_eq!(p, VoicePreflight::default());
        // Endpoint only covers ITS half.
        let p = preflight_decision(false, false, false, true, false);
        assert_eq!(p.stt_error, None);
        assert!(p.tts_error.is_some());
    }
}
