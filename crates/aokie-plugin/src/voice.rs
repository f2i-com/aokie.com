//! In-process text-to-speech for the voice receptionist (behind the `voice`
//! feature). Wraps aokie-ai's in-process TTS runtimes — Pocket-TTS (ONNX) and
//! sherpa-onnx (VITS/Piper + Kokoro voices, statically linked) — behind one
//! engine-dispatching [`TtsEngine`] and adapts their output to the radio's SCO
//! audio path: synthesize text → f32 @ the model rate → resample to the
//! negotiated SCO rate (8 kHz CVSD / 16 kHz mSBC) → i16 PCM ready for
//! `BluetoothManager::send_audio`.
//!
//! Engine selection is env-driven (`AOKIE_TTS_ENGINE` from the `ttsEngine`
//! setting, `AOKIE_TTS_MODEL_DIR` from `ttsModelDir`) and read at LOAD time:
//! the synth worker drops its engine on a `ReloadEngine` job and the next
//! load picks up the new selection — no radio restart needed.
//!
//! Loading is heavy (Pocket-TTS ~200 MB of ONNX graphs) so callers load once,
//! lazily, on the first thing Aokie needs to say.

use std::path::{Path, PathBuf};

use aokie_ai::config::SherpaTtsConfig;
use aokie_ai::runtimes::onnx_tts::{onnx_tts_models_dir, OnnxTtsRuntime};
use aokie_ai::runtimes::parakeet_onnx::ParakeetOnnxRuntime;
use aokie_ai::runtimes::sherpa_onnx::tts::SherpaOnnxTtsRuntime;

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

/// The in-process synthesis backend [`TtsEngine`] dispatches to. Pocket-TTS
/// streams chunks as it decodes; sherpa-onnx's offline API is one-shot per
/// span — acceptable because its RTF is a small fraction of realtime, so a
/// whole sentence synthesizes faster than Pocket-TTS's first chunk.
enum TtsBackend {
    Pocket(OnnxTtsRuntime),
    Sherpa(SherpaOnnxTtsRuntime),
}

pub struct TtsEngine {
    backend: TtsBackend,
    native_rate: u32,
}

/// Normalize a `ttsEngine` setting value to the engine it selects. Unknown
/// values fall back to pocket (never a hard failure — the line must speak).
pub fn normalize_tts_engine(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "sherpa" | "sherpa-onnx" | "piper" => "sherpa",
        "" | "pocket" | "pocket-tts" => "pocket",
        other => {
            eprintln!("[aokie-plugin] unknown ttsEngine {other:?} - using pocket-tts");
            "pocket"
        }
    }
}

/// The engine the `AOKIE_TTS_ENGINE` env selects right now.
pub fn selected_tts_engine() -> &'static str {
    normalize_tts_engine(&std::env::var("AOKIE_TTS_ENGINE").unwrap_or_default())
}

fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

/// Attenuate-only loudness guard for one-shot engines (sherpa/Piper): VITS
/// output is un-normalized and routinely sits near (or beyond) full scale.
/// Hard-clipping at the i16 conversion adds nonlinear products a linear echo
/// canceller cannot model, and a hotter far-end raises the AEC residual
/// against the fixed capture gates — the sherpa false-barge class from live
/// call 066d2237. Scales the whole utterance down so peak ≤ 0.85 and
/// RMS ≤ 0.12; never amplifies, silence passes untouched. Returns the gain.
fn loudness_guard(samples: &mut [f32]) -> f32 {
    if samples.is_empty() {
        return 1.0;
    }
    let mut peak = 0f32;
    let mut sum_sq = 0f64;
    for &s in samples.iter() {
        peak = peak.max(s.abs());
        sum_sq += (s as f64) * (s as f64);
    }
    if peak <= 0.0 {
        return 1.0;
    }
    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
    let mut scale = 1.0f32;
    if peak > 0.85 {
        scale = scale.min(0.85 / peak);
    }
    if rms > 0.12 {
        scale = scale.min(0.12 / rms);
    }
    if scale < 1.0 {
        for s in samples.iter_mut() {
            *s *= scale;
        }
    }
    scale
}

/// Sherpa voice selection is a numeric speaker id; anything else (a leftover
/// Pocket-TTS preset name like "alba") would warn on every span, so map it to
/// "" (= the bundle's default speaker) here.
fn sherpa_voice(voice: &str) -> &str {
    if voice.trim().parse::<i32>().is_ok() {
        voice
    } else {
        ""
    }
}

impl TtsEngine {
    /// Load the in-process TTS engine the current env selects:
    /// `AOKIE_TTS_ENGINE=sherpa` → a sherpa-onnx voice bundle (Piper/VITS or
    /// Kokoro) from `AOKIE_TTS_MODEL_DIR`; anything else → the Pocket-TTS
    /// bundle from `<app_data>/models/pocket_tts_onnx`.
    pub fn load() -> Result<Self, String> {
        ensure_ort_dylib();
        match selected_tts_engine() {
            "sherpa" => Self::load_sherpa(),
            _ => Self::load_pocket(),
        }
    }

    fn load_pocket() -> Result<Self, String> {
        let app_data = aokie_core::paths::app_data_dir()
            .ok_or_else(|| "no app_data_dir for the TTS models".to_string())?;
        let dir = onnx_tts_models_dir(&app_data, None)?;
        let rt = OnnxTtsRuntime::open(&dir)?;
        let native_rate = rt.sample_rate();
        Ok(Self {
            backend: TtsBackend::Pocket(rt),
            native_rate,
        })
    }

    fn load_sherpa() -> Result<Self, String> {
        let dir = sherpa_voice_dir()?;
        let cfg = sherpa_config_for_dir(&dir)?;
        let mut rt = SherpaOnnxTtsRuntime::load(&cfg)?;
        // Load-time probe synth: the bundle's native sample rate isn't in the
        // config (sherpa reports it per generated audio), and a corrupt or
        // incompatible model should fail HERE — before auto-answer arms —
        // not on the first live span.
        let probe = rt
            .synthesize("Aokie is ready.", "")
            .map_err(|e| format!("sherpa-onnx TTS probe synthesis failed: {e}"))?;
        if probe.samples.is_empty() || probe.sample_rate == 0 {
            return Err("sherpa-onnx TTS produced no audio for the load-time probe".to_string());
        }
        eprintln!(
            "[aokie-plugin] sherpa-onnx TTS loaded from {} ({} Hz)",
            dir.display(),
            probe.sample_rate
        );
        Ok(Self {
            backend: TtsBackend::Sherpa(rt),
            native_rate: probe.sample_rate,
        })
    }

    /// Human-readable engine tag for logs / self-test detail.
    pub fn engine_name(&self) -> &'static str {
        match self.backend {
            TtsBackend::Pocket(_) => "pocket-tts",
            TtsBackend::Sherpa(_) => "sherpa-onnx",
        }
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
        match &mut self.backend {
            TtsBackend::Pocket(rt) => {
                let mut f32_samples: Vec<f32> = Vec::new();
                rt.synthesize_stream(text, voice, |chunk, _rate| {
                    f32_samples.extend_from_slice(chunk);
                    true
                })?;
                let resampled = crate::speech_wire::resample_linear(
                    &f32_samples,
                    self.native_rate,
                    target_rate,
                );
                Ok(f32_to_i16(&resampled))
            }
            TtsBackend::Sherpa(rt) => {
                let mut audio = rt.synthesize(text, sherpa_voice(voice))?;
                loudness_guard(&mut audio.samples);
                let resampled = crate::speech_wire::resample_linear(
                    &audio.samples,
                    audio.sample_rate,
                    target_rate,
                );
                Ok(f32_to_i16(&resampled))
            }
        }
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
        match &mut self.backend {
            TtsBackend::Pocket(rt) => {
                let mut f32_samples: Vec<f32> = Vec::new();
                rt.synthesize_stream(text, voice, |chunk, _rate| {
                    f32_samples.extend_from_slice(chunk);
                    true
                })?;
                Ok((f32_to_i16(&f32_samples), self.native_rate))
            }
            TtsBackend::Sherpa(rt) => {
                let mut audio = rt.synthesize(text, sherpa_voice(voice))?;
                loudness_guard(&mut audio.samples);
                Ok((f32_to_i16(&audio.samples), audio.sample_rate))
            }
        }
    }

    /// Streaming synthesis: calls `on_pcm` with mono i16 PCM at `target_rate` as
    /// each TTS chunk is produced, so playback can start on the first chunk
    /// instead of after the whole utterance. `on_pcm` returns `false` to
    /// stop early (barge-in / hangup). Returns the total sample count emitted.
    ///
    /// One-shot engines (sherpa) deliver the whole utterance in a single
    /// `on_pcm` call — the synth worker still slices it into bounded blocks
    /// and honours epoch aborts between blocks, so cancellation latency is
    /// unchanged; only time-to-first-audio equals the whole-span synthesis
    /// time (a small fraction of realtime for sherpa voices).
    pub fn synthesize_streaming(
        &mut self,
        text: &str,
        voice: &str,
        target_rate: u32,
        mut on_pcm: impl FnMut(&[i16]) -> bool,
    ) -> Result<usize, String> {
        match &mut self.backend {
            TtsBackend::Pocket(rt) => {
                let native = self.native_rate;
                let mut total = 0usize;
                rt.synthesize_stream(text, voice, |chunk, _rate| {
                    // Per-chunk linear resample: the one-sample boundary discontinuity is
                    // inaudible over an 8/16 kHz phone link and keeps latency minimal.
                    let resampled = crate::speech_wire::resample_linear(chunk, native, target_rate);
                    let pcm = f32_to_i16(&resampled);
                    total += pcm.len();
                    on_pcm(&pcm)
                })?;
                Ok(total)
            }
            TtsBackend::Sherpa(rt) => {
                let mut audio = rt.synthesize(text, sherpa_voice(voice))?;
                let gain = loudness_guard(&mut audio.samples);
                if gain < 0.9 {
                    eprintln!("[aokie-plugin] sherpa loudness guard: x{gain:.2}");
                }
                let resampled = crate::speech_wire::resample_linear(
                    &audio.samples,
                    audio.sample_rate,
                    target_rate,
                );
                let pcm = f32_to_i16(&resampled);
                let total = pcm.len();
                on_pcm(&pcm);
                Ok(total)
            }
        }
    }
}

/// Resolve the sherpa voice-bundle directory: `AOKIE_TTS_MODEL_DIR` when set,
/// else the first usable bundle under `<app_data>/models/tts` (alphabetical —
/// deterministic across restarts). A "usable bundle" holds a model .onnx and
/// tokens.txt, per the sherpa-onnx release-tarball layout.
fn sherpa_voice_dir() -> Result<PathBuf, String> {
    if let Ok(v) = std::env::var("AOKIE_TTS_MODEL_DIR") {
        let v = v.trim();
        if !v.is_empty() {
            let dir = PathBuf::from(v);
            if !dir.is_dir() {
                return Err(format!(
                    "ttsModelDir {v:?} is not a folder - point it at a sherpa voice bundle (model .onnx + tokens.txt)"
                ));
            }
            return Ok(dir);
        }
    }
    let app_data = aokie_core::paths::app_data_dir()
        .ok_or_else(|| "no app_data_dir for the TTS models".to_string())?;
    let root = app_data.join("models").join("tts");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
        .map_err(|_| {
            format!(
                "no sherpa voice found: set ttsModelDir to a voice bundle folder (model .onnx + tokens.txt), or place one under {}",
                root.display()
            )
        })?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("tokens.txt").is_file() && dir_has_onnx(p))
        .collect();
    candidates.sort();
    candidates.into_iter().next().ok_or_else(|| {
        format!(
            "no sherpa voice found under {} - set ttsModelDir to a voice bundle folder (model .onnx + tokens.txt)",
            root.display()
        )
    })
}

fn dir_has_onnx(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "onnx"))
        })
        .unwrap_or(false)
}

/// Installed Pocket-TTS voice preset names: the basenames of
/// `<app_data>/models/pocket_tts_onnx/voices/*.{safetensors,wav}`, deduped and
/// sorted. Empty when the bundle (or app_data) is missing — the console falls
/// back to its built-in list.
pub fn pocket_voice_names() -> Vec<String> {
    let Some(app_data) = aokie_core::paths::app_data_dir() else {
        return Vec::new();
    };
    let dir = app_data
        .join("models")
        .join("pocket_tts_onnx")
        .join("voices");
    let mut names = std::collections::BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let preset = p
                .extension()
                .is_some_and(|x| x == "safetensors" || x == "wav");
            if !preset {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                if !stem.is_empty() {
                    names.insert(stem.to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

/// The console's per-engine voice catalog (`settings.get` →
/// `ttsVoiceCatalog`): pocket voice preset names + installed sherpa voice
/// bundles under the scan root, plus the currently-configured
/// `AOKIE_TTS_MODEL_DIR` when it's a valid bundle OUTSIDE the scan root (the
/// E:\models\piper class). Pure filesystem — never loads an engine.
pub fn tts_voice_catalog() -> serde_json::Value {
    let scan_root = aokie_core::paths::app_data_dir().map(|d| d.join("models").join("tts"));
    let is_bundle = |p: &Path| p.is_dir() && p.join("tokens.txt").is_file() && dir_has_onnx(p);
    let bundle_json = |p: &Path| {
        serde_json::json!({
            "dir": p.to_string_lossy(),
            "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            "kind": if p.join("voices.bin").is_file() { "kokoro" } else { "vits" },
        })
    };
    let mut bundles: Vec<PathBuf> = scan_root
        .as_deref()
        .and_then(|root| std::fs::read_dir(root).ok())
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| is_bundle(p))
                .collect()
        })
        .unwrap_or_default();
    bundles.sort();
    if let Ok(v) = std::env::var("AOKIE_TTS_MODEL_DIR") {
        let configured = PathBuf::from(v.trim());
        if !v.trim().is_empty() && is_bundle(&configured) && !bundles.contains(&configured) {
            bundles.push(configured);
        }
    }
    serde_json::json!({
        "engines": [
            {
                "id": "pocket",
                "label": "Pocket-TTS",
                "voices": pocket_voice_names(),
            },
            {
                "id": "sherpa",
                "label": "Sherpa (Piper/VITS/Kokoro)",
                "bundles": bundles.iter().map(|p| bundle_json(p)).collect::<Vec<_>>(),
                "scanRoot": scan_root
                    .as_deref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            },
        ],
    })
}

/// Compose the sherpa engine config from a voice-bundle folder, following the
/// sherpa-onnx release-tarball layout: `<name>.onnx` + `tokens.txt`, with
/// `espeak-ng-data/` (Piper) either inside the bundle or shared in the parent
/// folder, optional `lexicon.txt`/`dict/`, and `voices.bin` marking a Kokoro
/// bundle. Kept pure over the folder contents so it's unit-testable.
fn sherpa_config_for_dir(dir: &Path) -> Result<SherpaTtsConfig, String> {
    let mut onnx: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read ttsModelDir {}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "onnx"))
        .collect();
    onnx.sort();
    if onnx.is_empty() {
        return Err(format!(
            "no .onnx model file in {} - a sherpa voice bundle needs the voice's .onnx + tokens.txt",
            dir.display()
        ));
    }
    if onnx.len() > 1 {
        return Err(format!(
            "{} holds {} .onnx files - a voice bundle folder must hold exactly one",
            dir.display(),
            onnx.len()
        ));
    }
    let tokens = dir.join("tokens.txt");
    if !tokens.is_file() {
        return Err(format!("no tokens.txt in {}", dir.display()));
    }
    let mut cfg = SherpaTtsConfig::default();
    // struct-Default gives zeroed scales (the serde defaults only apply when
    // deserializing) — set the neutral values explicitly.
    cfg.length_scale = 1.0;
    cfg.speed = 1.0;
    cfg.model_path = onnx[0].to_string_lossy().to_string();
    cfg.tokens_path = tokens.to_string_lossy().to_string();
    let voices = dir.join("voices.bin");
    if voices.is_file() {
        cfg.engine = "kokoro".to_string();
        cfg.voices_path = voices.to_string_lossy().to_string();
        cfg.lang = "en-us".to_string();
    } else {
        cfg.engine = "vits".to_string();
    }
    for data_dir in [dir.join("espeak-ng-data")]
        .into_iter()
        .chain(dir.parent().map(|p| p.join("espeak-ng-data")))
    {
        if data_dir.is_dir() {
            cfg.data_dir = data_dir.to_string_lossy().to_string();
            break;
        }
    }
    let lexicon = dir.join("lexicon.txt");
    if lexicon.is_file() {
        cfg.lexicon = lexicon.to_string_lossy().to_string();
    }
    let dict = dir.join("dict");
    if dict.is_dir() {
        cfg.dict_dir = dict.to_string_lossy().to_string();
    }
    Ok(cfg)
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
    // Which in-process TTS engine is selected decides BOTH which assets to
    // check and whether the ORT DLL matters for the TTS half (sherpa-onnx is
    // statically linked — it doesn't touch the ORT DLL at all).
    let sherpa_selected = selected_tts_engine() == "sherpa";
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
            let tts_ok = if sherpa_selected {
                sherpa_voice_dir()
                    .and_then(|dir| sherpa_config_for_dir(&dir))
                    .is_ok()
            } else {
                onnx_tts_models_dir(&app_data, None).is_ok()
            };
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
        !sherpa_selected,
        stt_endpoint,
        tts_endpoint,
    )
}

/// Pure decision half of [`preflight_assets`], unit-testable: which halves of
/// the voice pipeline are KNOWN broken given what's on disk / configured.
/// `tts_needs_ort` is false when the selected TTS engine is statically linked
/// (sherpa) and works without the ONNX Runtime DLL.
pub fn preflight_decision(
    ort_ok: bool,
    stt_assets_ok: bool,
    tts_assets_ok: bool,
    tts_needs_ort: bool,
    stt_endpoint: bool,
    tts_endpoint: bool,
) -> VoicePreflight {
    // An HTTP endpoint carries its half even with no local assets; local
    // assets additionally need the ONNX Runtime DLL to be loadable.
    let local_stt = ort_ok && stt_assets_ok;
    let local_tts = (ort_ok || !tts_needs_ort) && tts_assets_ok;
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
///
/// ⚠️ ORDER MATTERS: the VERSIONED name must win. The sherpa-onnx TTS engine
/// ships its own `onnxruntime.dll` (1.17.1, imported by sherpa-onnx-c-api.dll
/// by that exact name) into the same directory — if the ort engines resolved
/// the unversioned name first they would silently load sherpa's older ORT and
/// break Pocket-TTS/Parakeet. The two runtimes coexist as separate modules.
fn ensure_ort_dylib() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["onnxruntime_1.25.0.dll", "onnxruntime.dll"] {
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
        let p = preflight_decision(true, true, true, true, false, false);
        assert_eq!(p, VoicePreflight::default());
    }

    #[test]
    fn preflight_missing_models_is_a_known_failure() {
        let p = preflight_decision(true, false, true, true, false, false);
        assert!(p
            .stt_error
            .as_deref()
            .unwrap_or("")
            .contains("model files are missing"));
        assert_eq!(p.tts_error, None);

        let p = preflight_decision(true, true, false, true, false, false);
        assert_eq!(p.stt_error, None);
        assert!(p
            .tts_error
            .as_deref()
            .unwrap_or("")
            .contains("model files are missing"));
    }

    #[test]
    fn preflight_missing_ort_dll_fails_both_and_names_the_dll() {
        let p = preflight_decision(false, true, true, true, false, false);
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

    /// The statically-linked sherpa engine doesn't touch the ORT DLL: a
    /// missing DLL must not fail the TTS half when sherpa is selected —
    /// but STT (Parakeet, ORT-based) still does.
    #[test]
    fn preflight_sherpa_tts_does_not_need_the_ort_dll() {
        let p = preflight_decision(false, true, true, false, false, false);
        assert!(p
            .stt_error
            .as_deref()
            .unwrap_or("")
            .contains("ONNX Runtime DLL"));
        assert_eq!(p.tts_error, None);
        // Missing sherpa voice assets still fail the TTS half.
        let p = preflight_decision(false, true, false, false, false, false);
        assert!(p
            .tts_error
            .as_deref()
            .unwrap_or("")
            .contains("model files are missing"));
    }

    /// The sherpa loudness guard attenuates hot/clipping output but never
    /// touches normal-level or silent audio (attenuate-only by design).
    #[test]
    fn loudness_guard_attenuates_hot_output_only() {
        // Silence: untouched.
        let mut silence = vec![0.0f32; 4_000];
        assert_eq!(loudness_guard(&mut silence), 1.0);
        assert!(silence.iter().all(|&s| s == 0.0));
        // Normal speech level (peak ~0.4, rms well under 0.12): untouched.
        let mut normal: Vec<f32> = (0..4_000).map(|i| (i as f32 * 0.05).sin() * 0.15).collect();
        assert_eq!(loudness_guard(&mut normal), 1.0);
        // Hot Piper-class output (peak beyond full scale): peak capped ≤0.85,
        // rms capped ≤0.12.
        let mut hot: Vec<f32> = (0..4_000).map(|i| (i as f32 * 0.05).sin() * 1.4).collect();
        let gain = loudness_guard(&mut hot);
        assert!(gain < 1.0);
        let peak = hot.iter().fold(0f32, |a, &s| a.max(s.abs()));
        let rms =
            (hot.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / hot.len() as f64).sqrt();
        assert!(peak <= 0.85 + 1e-4, "peak {peak}");
        assert!(rms <= 0.12 + 1e-4, "rms {rms}");
    }

    /// ttsEngine setting values map deterministically; unknown never breaks
    /// the line (falls back to pocket).
    #[test]
    fn tts_engine_normalization() {
        assert_eq!(normalize_tts_engine(""), "pocket");
        assert_eq!(normalize_tts_engine("pocket"), "pocket");
        assert_eq!(normalize_tts_engine("Pocket-TTS"), "pocket");
        assert_eq!(normalize_tts_engine("sherpa"), "sherpa");
        assert_eq!(normalize_tts_engine(" Sherpa-ONNX "), "sherpa");
        assert_eq!(normalize_tts_engine("piper"), "sherpa");
        assert_eq!(normalize_tts_engine("banana"), "pocket");
    }

    /// Sherpa voice selection is a numeric speaker id; preset names map to
    /// the bundle default instead of warning per span.
    #[test]
    fn sherpa_voice_normalization() {
        assert_eq!(sherpa_voice("3"), "3");
        assert_eq!(sherpa_voice(" 12 "), " 12 ");
        assert_eq!(sherpa_voice("alba"), "");
        assert_eq!(sherpa_voice(""), "");
    }

    /// sherpa_config_for_dir composes the engine config from the standard
    /// sherpa-onnx voice-bundle layout, including the shared espeak-ng-data
    /// fallback in the PARENT folder and Kokoro detection via voices.bin.
    #[test]
    fn sherpa_config_composition_from_bundle_dir() {
        let root = std::env::temp_dir().join(format!(
            "aokie-sherpa-cfg-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bundle = root.join("vits-piper-en_US-test-medium");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("en_US-test-medium.onnx"), b"x").unwrap();
        std::fs::write(bundle.join("tokens.txt"), b"x").unwrap();
        // Shared espeak-ng-data in the PARENT (the com.aokie.app/models/tts layout).
        std::fs::create_dir_all(root.join("espeak-ng-data")).unwrap();

        let cfg = sherpa_config_for_dir(&bundle).expect("bundle composes");
        assert_eq!(cfg.engine, "vits");
        assert!(cfg.model_path.ends_with("en_US-test-medium.onnx"));
        assert!(cfg.tokens_path.ends_with("tokens.txt"));
        assert!(cfg.data_dir.ends_with("espeak-ng-data"));
        assert_eq!(cfg.length_scale, 1.0);
        assert_eq!(cfg.speed, 1.0);

        // A bundle-local espeak-ng-data wins over the parent's.
        std::fs::create_dir_all(bundle.join("espeak-ng-data")).unwrap();
        let cfg = sherpa_config_for_dir(&bundle).unwrap();
        assert!(cfg.data_dir.contains("vits-piper-en_US-test-medium"));

        // voices.bin flips the engine to kokoro.
        std::fs::write(bundle.join("voices.bin"), b"x").unwrap();
        let cfg = sherpa_config_for_dir(&bundle).unwrap();
        assert_eq!(cfg.engine, "kokoro");
        assert!(cfg.voices_path.ends_with("voices.bin"));

        // Two .onnx files = ambiguous bundle, refused with a clear error.
        std::fs::write(bundle.join("second.onnx"), b"x").unwrap();
        let err = sherpa_config_for_dir(&bundle).unwrap_err();
        assert!(err.contains("exactly one"), "got: {err}");

        // Missing tokens.txt refused.
        std::fs::remove_file(bundle.join("second.onnx")).unwrap();
        std::fs::remove_file(bundle.join("tokens.txt")).unwrap();
        let err = sherpa_config_for_dir(&bundle).unwrap_err();
        assert!(err.contains("tokens.txt"), "got: {err}");

        let _ = std::fs::remove_dir_all(&root);
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

    /// Manual smoke against a REAL sherpa voice bundle (not run in CI): loads
    /// the engine the env selects and synthesizes a sentence end-to-end.
    /// Run with a Piper bundle on disk:
    /// `AOKIE_TTS_ENGINE=sherpa AOKIE_TTS_MODEL_DIR=<bundle> cargo test -p aokie-plugin --features voice sherpa_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sherpa_smoke_synthesize_real_voice() {
        std::env::set_var("AOKIE_TTS_ENGINE", "sherpa");
        let mut engine = TtsEngine::load().expect("sherpa engine loads");
        assert_eq!(engine.engine_name(), "sherpa-onnx");
        let start = std::time::Instant::now();
        let pcm = engine
            .synthesize(
                "Thank you for calling! How can I help you today?",
                "",
                16_000,
            )
            .expect("synthesis succeeds");
        let synth_ms = start.elapsed().as_millis();
        let audio_ms = pcm.len() as u128 / 16;
        println!(
            "sherpa synth: {} samples ({audio_ms} ms of audio) in {synth_ms} ms",
            pcm.len()
        );
        assert!(pcm.len() > 8_000, "under 0.5s of audio for a full sentence");
        assert!(
            pcm.iter().any(|&s| s.abs() > 1000),
            "synthesized audio is silent"
        );
    }

    #[test]
    fn preflight_http_endpoints_substitute_for_local_engines() {
        // No local assets at all, but both endpoints configured → clean.
        let p = preflight_decision(false, false, false, true, true, true);
        assert_eq!(p, VoicePreflight::default());
        // Endpoint only covers ITS half.
        let p = preflight_decision(false, false, false, true, true, false);
        assert_eq!(p.stt_error, None);
        assert!(p.tts_error.is_some());
    }
}
