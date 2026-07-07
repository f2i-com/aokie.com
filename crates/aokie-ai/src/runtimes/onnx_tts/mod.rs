//! ONNX-bundle TTS runtime. The default bundled model is the
//! KevinAHM/pocket-tts-onnx export of Kyutai's pocket-tts; the
//! loader and generation loop are scoped to that bundle's
//! 5-graph + per-voice-state shape, but other models that follow
//! the same shape can plug in by editing
//! `<app_data>/ai_providers.json`.
//!
//! The ONNX bundle splits the work across five models:
//!   1. `text_conditioner`  — text tokens → text embeddings
//!   2. `flow_lm_main`      — stateful autoregressive transformer. Run once
//!      on text embeddings to condition state, then run per-step with the
//!      current latent to produce a conditioning vector and an EOS logit.
//!   3. `flow_lm_flow`      — velocity field for flow matching. Integrated
//!      via Euler steps (`lsd_steps` sub-steps per latent frame).
//!   4. `mimi_decoder`      — stateful RVQ decoder, latent frames → 24 kHz
//!      f32 PCM in `[-1, 1]`.
//!   5. `mimi_encoder`      — reference audio → voice embedding (only used
//!      when cloning from a wav; predefined voices load state directly
//!      from `.safetensors`).
//!
//! See `bundle.json` for the state manifests that drive each transformer's
//! KV cache and each mimi block's `first`/`previous`/`partial` buffers —
//! every state tensor is typed & shaped there and we feed it back in on
//! each step. This file intentionally mirrors the Python reference at
//! <https://huggingface.co/KevinAHM/pocket-tts-onnx/blob/main/pocket_tts_onnx.py>.

pub mod generate;
pub mod state;
pub mod tokenizer;
pub mod voice;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Subset of `bundle.json` we care about.
#[derive(Debug, Clone, Deserialize)]
pub struct BundleConfig {
    pub bundle_name: String,
    pub language: String,
    pub sample_rate: u32,
    pub frame_rate: f32,
    pub samples_per_frame: u32,
    pub latent_dim: usize,
    pub conditioning_dim: usize,
    pub max_token_per_chunk: usize,
    pub insert_bos_before_voice: bool,
    pub bos_before_voice_file: String,
    pub tokenizer_file: String,
    pub pad_with_spaces_for_short_inputs: bool,
    pub remove_semicolons: bool,
    pub model_recommended_frames_after_eos: Option<usize>,
    pub predefined_voices: Vec<String>,
    pub flow_lm_state_manifest: Vec<StateSlot>,
    pub mimi_state_manifest: Vec<StateSlot>,
}

/// One entry in a state manifest — describes an ONNX input/output pair
/// that carries per-step state (KV cache, streaming-conv buffers, …).
#[derive(Debug, Clone, Deserialize)]
pub struct StateSlot {
    pub dtype: String, // "float32" | "int64" | "bool"
    pub fill: String,  // "nan" | "zeros" | "ones" | "empty"
    pub index: usize,
    pub input_name: String,  // e.g. "state_0"
    pub output_name: String, // e.g. "out_state_0"
    pub shape: Vec<usize>,
    /// `module/key` — used to look up the matching tensor in a predefined
    /// voice's `.safetensors` blob (the safetensors keys mirror this path).
    pub path: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub module: String,
}

/// Pre-initialised state tensors for the flow LM or mimi decoder. Each
/// slot matches an entry in the corresponding manifest by index.
#[derive(Debug)]
pub struct StateBuffers {
    pub slots: Vec<StateValue>,
}

/// Voice cache entry. The mtime is the source file's modification time
/// at insert; if the file is later overwritten in place we detect the
/// drift on lookup and rebuild rather than serving stale state.
#[derive(Debug)]
pub struct CachedVoiceState {
    pub state: StateBuffers,
    pub mtime: Option<std::time::SystemTime>,
}

/// Typed tensor payload that matches a `StateSlot` dtype.
#[derive(Debug, Clone)]
pub enum StateValue {
    F32 { data: Vec<f32>, shape: Vec<usize> },
    I64 { data: Vec<i64>, shape: Vec<usize> },
    Bool { data: Vec<bool>, shape: Vec<usize> },
}

impl StateValue {
    /// Build a zero/one/NaN-filled state slot per the bundle's manifest.
    /// Returns `Err` when the bundle JSON specifies an unknown dtype or
    /// fill — pre-fix this panicked, which crashed the app on any
    /// corrupted download or version-drifted bundle. A returned error
    /// lets the caller surface a clear "bundle config invalid" message
    /// to the UI and prompt for re-download instead.
    pub fn from_manifest(slot: &StateSlot) -> Result<Self, String> {
        let numel: usize = slot.shape.iter().product();
        match slot.dtype.as_str() {
            "float32" => {
                let data = match slot.fill.as_str() {
                    "zeros" | "empty" => vec![0.0f32; numel],
                    "nan" => vec![f32::NAN; numel],
                    "ones" => vec![1.0f32; numel],
                    other => {
                        return Err(format!(
                            "bundle slot {:?}: unexpected float32 fill {:?}",
                            slot.path, other
                        ))
                    }
                };
                Ok(Self::F32 {
                    data,
                    shape: slot.shape.clone(),
                })
            }
            "int64" => {
                let data = match slot.fill.as_str() {
                    "zeros" | "empty" => vec![0i64; numel],
                    "ones" => vec![1i64; numel],
                    other => {
                        return Err(format!(
                            "bundle slot {:?}: unexpected int64 fill {:?}",
                            slot.path, other
                        ))
                    }
                };
                Ok(Self::I64 {
                    data,
                    shape: slot.shape.clone(),
                })
            }
            "bool" => {
                let data = match slot.fill.as_str() {
                    "zeros" | "empty" => vec![false; numel],
                    "ones" => vec![true; numel],
                    other => {
                        return Err(format!(
                            "bundle slot {:?}: unexpected bool fill {:?}",
                            slot.path, other
                        ))
                    }
                };
                Ok(Self::Bool {
                    data,
                    shape: slot.shape.clone(),
                })
            }
            other => Err(format!(
                "bundle slot {:?}: unsupported state dtype {:?}",
                slot.path, other
            )),
        }
    }
}

/// Initialise a fresh state buffer set from a manifest.
pub fn init_state(manifest: &[StateSlot]) -> Result<StateBuffers, String> {
    let slots = manifest
        .iter()
        .map(StateValue::from_manifest)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(StateBuffers { slots })
}

/// Streaming stats reported once a synthesis run completes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StreamStats {
    pub total_samples: usize,
    pub first_chunk_ms: u64,
    pub synth_ms: u64,
    pub sample_rate: u32,
}

/// The live pocket-tts-onnx instance. Heavy and mutable — reuse across
/// calls (the mimi/flow state is reset per utterance, not per instance).
pub struct OnnxTtsRuntime {
    pub bundle_dir: PathBuf,
    pub cfg: BundleConfig,
    pub tokenizer: tokenizer::Tokenizer,
    pub text_conditioner: ort::session::Session,
    pub flow_lm_main: ort::session::Session,
    pub flow_lm_flow: ort::session::Session,
    pub mimi_decoder: ort::session::Session,
    pub mimi_encoder: Option<ort::session::Session>,
    /// Cache of voice-conditioned flow_lm_main state, keyed by resolved wav
    /// path. Encoding a voice takes ~300 ms; this keeps chat turns from
    /// paying that over and over. The `SystemTime` is the file's mtime
    /// at insert; lookups compare against the current mtime and rebuild
    /// on mismatch so a user who overwrites a voice file in place gets
    /// the new audio on the next call instead of stuck stale state.
    pub voice_cache: std::collections::HashMap<PathBuf, CachedVoiceState>,
}

impl OnnxTtsRuntime {
    /// Load all five ONNX sessions + the tokenizer from a bundle directory.
    /// Expects `bundle.json`, `tokenizer.model`, and the int8 ONNX files to
    /// live side-by-side (the layout shipped in
    /// `onnx/<language>/` of the HF repo).
    pub fn open(bundle_dir: &Path) -> Result<Self, String> {
        use ort::session::builder::GraphOptimizationLevel;

        println!(
            "[pocket_tts_onnx] loading bundle from {}",
            bundle_dir.display()
        );
        let bundle_path = bundle_dir.join("bundle.json");
        if !bundle_path.exists() {
            return Err(format!(
                "bundle.json missing at {}. Click Download on Pocket-TTS first.",
                bundle_path.display()
            ));
        }
        let cfg_text =
            std::fs::read_to_string(&bundle_path).map_err(|e| format!("read bundle.json: {e}"))?;
        let cfg: BundleConfig =
            serde_json::from_str(&cfg_text).map_err(|e| format!("parse bundle.json: {e}"))?;
        println!(
            "[pocket_tts_onnx] bundle '{}' @ {} Hz, {} layers of flow state, {} mimi state slots",
            cfg.bundle_name,
            cfg.sample_rate,
            cfg.flow_lm_state_manifest.len(),
            cfg.mimi_state_manifest.len()
        );

        let tokenizer_path = bundle_dir.join(&cfg.tokenizer_file);
        let tokenizer = tokenizer::Tokenizer::open(&tokenizer_path)
            .map_err(|e| format!("open tokenizer.model at {}: {e}", tokenizer_path.display()))?;

        // Default to the int8 variants — ~5× smaller, and the quality loss
        // is negligible for conversational TTS. Fall back to fp32 only if
        // the int8 file isn't on disk. Every session registers the CPU EP
        // explicitly: these models are small (<=80 MB) and autoregressive,
        // so CUDA would add launch overhead without meaningful speedup.
        let load = |stem: &str| -> Result<ort::session::Session, String> {
            use ort::execution_providers::CPUExecutionProvider;

            let int8 = bundle_dir.join(format!("{stem}_int8.onnx"));
            let fp32 = bundle_dir.join(format!("{stem}.onnx"));
            let path = if int8.exists() {
                int8
            } else if fp32.exists() {
                fp32
            } else {
                return Err(format!(
                    "missing: neither {} nor {} exists",
                    bundle_dir.join(format!("{stem}_int8.onnx")).display(),
                    bundle_dir.join(format!("{stem}.onnx")).display()
                ));
            };
            println!("[pocket_tts_onnx] load {}", path.display());
            ort::session::Session::builder()
                .map_err(|e| format!("ort builder ({stem}): {e}"))?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| format!("opt level ({stem}): {e}"))?
                .with_execution_providers([CPUExecutionProvider::default().build()])
                .map_err(|e| format!("register CPU EP ({stem}): {e}"))?
                .commit_from_file(&path)
                .map_err(|e| format!("commit {} ({}): {e}", path.display(), stem))
        };

        let text_conditioner = load("text_conditioner")?;
        let flow_lm_main = load("flow_lm_main")?;
        let flow_lm_flow = load("flow_lm_flow")?;
        let mimi_decoder = load("mimi_decoder")?;
        // mimi_encoder is only needed for voice cloning from a wav; for
        // predefined voices the state comes from a .safetensors blob, so
        // missing mimi_encoder shouldn't block startup.
        let mimi_encoder = match load("mimi_encoder") {
            Ok(s) => Some(s),
            Err(e) => {
                println!("[pocket_tts_onnx] mimi_encoder skipped: {e}");
                None
            }
        };

        // We condition voices from a reference wav on demand. Just check
        // that the default prompt is on disk — if not, surface it clearly
        // rather than failing later in synthesize with an opaque message.
        let reference_wav = bundle_dir.join("reference_sample.wav");
        if !reference_wav.exists() {
            println!(
                "[pocket_tts_onnx] WARNING: {} missing — synthesis needs at least one reference voice wav. Re-run Download.",
                reference_wav.display()
            );
        } else {
            println!(
                "[pocket_tts_onnx] reference voice at {}",
                reference_wav.display()
            );
        }
        if mimi_encoder.is_none() {
            return Err(
                "mimi_encoder.onnx missing — required for voice conditioning. Re-run Download."
                    .into(),
            );
        }

        println!("[pocket_tts_onnx] ready");
        Ok(Self {
            bundle_dir: bundle_dir.to_path_buf(),
            cfg,
            tokenizer,
            text_conditioner,
            flow_lm_main,
            flow_lm_flow,
            mimi_decoder,
            mimi_encoder,
            voice_cache: std::collections::HashMap::new(),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.cfg.sample_rate
    }

    /// Entry point — delegates to the full pipeline in `generate.rs`.
    pub fn synthesize_stream<F>(
        &mut self,
        text: &str,
        voice: &str,
        on_chunk: F,
    ) -> Result<StreamStats, String>
    where
        F: FnMut(&[f32], u32) -> bool,
    {
        self.synthesize_stream_impl(text, voice, on_chunk)
    }
}

/// Thread-safe wrapper for Tauri state.
pub struct OnnxTtsState {
    pub instance: Arc<Mutex<Option<OnnxTtsRuntime>>>,
}

impl Default for OnnxTtsState {
    fn default() -> Self {
        Self {
            instance: Arc::new(Mutex::new(None)),
        }
    }
}

/// Directory on disk where the bundle lives. Reads come from the
/// bundled resource dir when the installer shipped pre-staged weights;
/// downloads write to app_data. See `crate::bundled_models` for
/// the fallback rules.
pub fn onnx_tts_models_dir(
    app_data_dir: &Path,
    resource_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    crate::bundled_models::locate_models_dir(
        app_data_dir,
        resource_dir,
        crate::bundled_models::BundleRole::PocketTtsOnnx,
    )
}
