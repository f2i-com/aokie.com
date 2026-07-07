//! Optional model bundle detection.
//!
//! Aokie's "full" installer can ship the Whisper / Gemma 4 / Pocket-TTS
//! weights pre-staged inside the Tauri resource dir
//! (`<install>/resources/models/<role>/`). The lite installer omits
//! them — first launch downloads them into `<app_data>/models/<role>/`
//! over HTTP from Hugging Face.
//!
//! This module is the read-side glue that makes both layouts look the
//! same to the runtime: each role's `models_dir()` resolver checks the
//! bundled location first, returns it iff a per-role sentinel file
//! exists, and otherwise falls back to the writable app_data location.
//! Downloads explicitly target the writable location through
//! `app_data_models_dir()` so a partial bundle never tries to
//! permission-fault into a read-only resource folder.
//!
//! Bundle expectation: per role, all-or-nothing. The sentinel files
//! are the single largest "this role is here" identifying file —
//! `model.safetensors` for Whisper, `decoder_model_merged_q4f16.onnx_data`
//! for Gemma 4, and `bundle.json` for Pocket-TTS. If a sentinel is
//! present but the rest of the bundle is missing, the `verify_models`
//! command will surface the gap to the operator and the affected
//! provider initializer will refuse — better to fail loudly than to
//! silently fall through to download from a half-bundled state.

use std::path::{Path, PathBuf};

// Tauri-free: the legacy app resolved the resource/app-data dirs from a
// `tauri::AppHandle`. Here the caller passes those dirs as owned
// parameters instead — `app_data_dir` is the writable per-app data root
// (see `aokie_core::paths::app_data_dir`) and `resource_dir` is the
// optional installer resource root that may ship pre-staged model
// bundles (`None` when there is no bundled resource dir, e.g. dev runs).

/// One of the model bundles the app understands.
///
/// `Gemma4` is the 4-billion-parameter variant (Gemma 4 E4B, the
/// default, fits comfortably in 8 GB VRAM with KV cache). `Gemma4E2B`
/// is the 2-billion-parameter variant — half the VRAM, slightly
/// lower quality, the right pick for 6 GB cards. Both share the same
/// runtime (`OnnxGenAiRuntime`) and chat template; they only differ
/// in weights.
///
/// The `&'static str` form is what the on-disk layout uses and what
/// gets returned by `directory_name()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleRole {
    Whisper,
    /// Gemma 4 E4B — the default 4B-parameter ONNX bundle. Lives at
    /// `models/gemma4/`. Existing installs already use this directory
    /// name, so we keep it for backwards compatibility instead of
    /// renaming to `gemma4_e4b/`.
    Gemma4,
    /// Gemma 4 E2B — 2B-parameter variant for low-VRAM systems. Lives
    /// at `models/gemma4_e2b/`. Selected at runtime when the active
    /// LLM provider config sets `llm.model = "E2B"`.
    Gemma4E2B,
    PocketTtsOnnx,
    /// NVIDIA Parakeet-Unified-EN-0.6B (FastConformer + RNN-T), the
    /// eschmidbauer ONNX export. Three files: int8 encoder, merged
    /// int8 decoder+joiner, SentencePiece tokenizer.model. Selected
    /// when the active STT provider config sets `kind = "parakeet-onnx"`.
    Parakeet,
    /// UsefulSensors Moonshine ONNX bundle (encoder-decoder seq2seq).
    /// Three files: encoder, merged decoder, tokenizer.json. The
    /// quantization variant (fp32/fp16/int8/int4) lives in the
    /// provider config's `moonshine` block — the bundle dir layout
    /// stays the same. Selected when STT kind is `moonshine-onnx`.
    Moonshine,
    /// Alibaba Qwen3-ASR-0.6B int4 ONNX bundle (Whisper-style log-mel
    /// encoder + Qwen3-0.6B decoder backbone). Six files: encoder.int4,
    /// decoder_init.int4, decoder_step.int4, decoder_weights.int4.data
    /// (external int4 weights for both decoder graphs), embed_tokens.bin
    /// (f16 token embedding lookup), tokenizer.json. Selected when STT
    /// kind is `qwen3-asr-onnx`.
    Qwen3Asr,
    /// unsloth/Qwen3.5-4B-GGUF — single-file GGUF weights for the
    /// text-only Qwen 3.5 4B chat model. Runs through llama-server
    /// (kind `llama-server`); selected from the LLM model catalogue.
    /// The same dir hosts every quant variant the operator downloads
    /// (Q3_K_M / Q4_K_M / Q5_K_M / etc.); the active provider config's
    /// `model` field names which file llama-server should load.
    Qwen35_4b,
    /// openbmb/MiniCPM5-1B-GGUF — single-file GGUF weights for the
    /// lightweight 1B text-only MiniCPM5 chat model. Runs through
    /// llama-server (kind `llama-server`); selected from the LLM model
    /// catalogue. Needs the bundled llama-server binary >= b9354 for the
    /// `minicpm5` pre-tokenizer. Lives at `models/minicpm5/`; hosts every
    /// quant variant the operator downloads, with the active provider
    /// config's `model` field naming the file to load.
    MiniCpm5,
}

impl BundleRole {
    /// The directory name for this role under both
    /// `<resource_dir>/models/` (bundled) and `<app_data>/models/`
    /// (downloaded). Kept identical between the two so a swap from
    /// downloaded → bundled doesn't reshuffle the path layout.
    pub fn directory_name(self) -> &'static str {
        match self {
            BundleRole::Whisper => "whisper",
            BundleRole::Gemma4 => "gemma4",
            BundleRole::Gemma4E2B => "gemma4_e2b",
            BundleRole::PocketTtsOnnx => "pocket_tts_onnx",
            BundleRole::Parakeet => "parakeet",
            BundleRole::Moonshine => "moonshine",
            BundleRole::Qwen3Asr => "qwen3_asr",
            BundleRole::Qwen35_4b => "qwen35_4b",
            BundleRole::MiniCpm5 => "minicpm5",
        }
    }

    /// Sentinel file (relative to the role's bundle dir) that the
    /// detector treats as proof that this role's bundle is present.
    /// Picked as the largest required file for each role so a
    /// half-extracted ZIP or a botched copy is unlikely to leave the
    /// sentinel in place without the rest of the files.
    fn sentinel_file(self) -> &'static str {
        match self {
            BundleRole::Whisper => "turbo-encoder.int8.onnx",
            BundleRole::Gemma4 | BundleRole::Gemma4E2B => {
                "onnx/decoder_model_merged_q4f16.onnx_data"
            }
            BundleRole::PocketTtsOnnx => "bundle.json",
            // Parakeet's largest required file by a wide margin (~654 MB).
            BundleRole::Parakeet => "encoder.int8.onnx",
            // Moonshine-tiny default sentinel — encoder is the largest
            // file. Variant + quantization are picked by the provider
            // config and may map to a differently-named file on disk;
            // the sentinel is a "presence proof" so we use the most
            // common bundle layout (tiny FP32).
            BundleRole::Moonshine => "encoder_model.onnx",
            // Qwen3-ASR int4 bundle: the encoder is the largest single
            // file by far (~745 MB). Tracking it as the sentinel means
            // a half-extracted tar that's missing decoder_weights or
            // embed_tokens won't falsely register as bundled.
            BundleRole::Qwen3Asr => "encoder.int4.onnx",
            // The default catalogue quant for Qwen 3.5 4B. Operators
            // who download a different quant via the catalogue may
            // not have this exact file on disk, but we use it as the
            // canonical "is this role populated" check; the runtime
            // honours whatever `cfg.llm.model` actually names.
            BundleRole::Qwen35_4b => "Qwen3.5-4B-Q4_K_M.gguf",
            // Default catalogue quant for MiniCPM5 1B — same "is this
            // role populated" presence-proof semantics as Qwen above.
            BundleRole::MiniCpm5 => "MiniCPM5-1B-Q4_K_M.gguf",
        }
    }

    /// True when this role hosts a Gemma 4 ONNX bundle (any variant).
    /// Used by the resolver to know it needs to consult the active
    /// LLM provider config to pick between E4B and E2B before
    /// returning a directory.
    pub fn is_gemma_variant(self) -> bool {
        matches!(self, BundleRole::Gemma4 | BundleRole::Gemma4E2B)
    }
}

/// Map an active `llama-server` LLM model filename to the BundleRole
/// whose directory hosts it. Both Qwen 3.5 4B and MiniCPM5 are
/// `llama-server` kinds living in distinct dirs, so the sidecar autostart
/// and stack-status paths must pick the role from the model file rather
/// than hardcoding one. Unknown / empty names default to `Qwen35_4b` (the
/// original sole llama-server model), so existing configs resolve
/// unchanged.
pub fn llama_server_role_for_model(model: &str) -> BundleRole {
    if model.trim().starts_with("MiniCPM5") {
        BundleRole::MiniCpm5
    } else {
        BundleRole::Qwen35_4b
    }
}

/// Read-side resolver: returns the bundled directory if a sentinel file
/// is present, otherwise the writable app_data directory. The returned
/// dir is the one the runtime should read from. The app_data fallback
/// is created on demand so a fresh install lands in a known-good state.
///
/// Use this from runtime loaders (Whisper / Gemma / Pocket-TTS) and
/// from the verify pass — both of those just want "where does this
/// role's data live right now".
pub fn locate_models_dir(
    app_data_dir: &Path,
    resource_dir: Option<&Path>,
    role: BundleRole,
) -> Result<PathBuf, String> {
    if let Some(bundled) = bundled_models_dir(resource_dir, role) {
        if has_sentinel(&bundled, role) {
            return Ok(bundled);
        }
    }
    app_data_models_dir(app_data_dir, role)
}

/// Write-side resolver: always returns the writable app_data location,
/// even when a bundled copy exists. The downloader uses this so a
/// partial bundle that's missing a few files can backfill from HF
/// without trying to write into the read-only resource folder.
pub fn app_data_models_dir(app_data_dir: &Path, role: BundleRole) -> Result<PathBuf, String> {
    let dir = app_data_dir.join("models").join(role.directory_name());
    // Gemma 4's `onnx/` subdir is the only multi-level layout; create
    // it eagerly when the role is Gemma so the downloader doesn't have
    // to.
    let to_create = match role {
        BundleRole::Gemma4 => dir.join("onnx"),
        _ => dir.clone(),
    };
    std::fs::create_dir_all(&to_create)
        .map_err(|e| format!("mkdir {}: {}", to_create.display(), e))?;
    Ok(dir)
}

/// True when the bundled bundle for `role` reports complete by sentinel
/// presence. Surfaced for diagnostics ("Models: bundled" vs "downloaded")
/// — the runtime itself just calls `locate_models_dir`.
pub fn is_bundled(resource_dir: Option<&Path>, role: BundleRole) -> bool {
    bundled_models_dir(resource_dir, role)
        .map(|dir| has_sentinel(&dir, role))
        .unwrap_or(false)
}

/// `<resource_dir>/models/<role>/`. `None` when there is no bundled
/// resource dir (very early bring-up, or a setup that bypasses the
/// bundler entirely — both rare, and always the case for dev runs off
/// the crate).
fn bundled_models_dir(resource_dir: Option<&Path>, role: BundleRole) -> Option<PathBuf> {
    resource_dir.map(|root| root.join("models").join(role.directory_name()))
}

fn has_sentinel(dir: &Path, role: BundleRole) -> bool {
    dir.join(role.sentinel_file()).exists()
}

/// Read the active LLM provider config and decide which Gemma 4
/// variant the runtime should load. Defaults to E4B when the
/// `llm.model` field is empty or anything other than the recognised
/// `"E2B"` token — that keeps existing installs (which never set
/// `model` because the field was unused for in-process kinds) on the
/// 4B model.
///
/// The string match is case-insensitive so a config that says `e2b`,
/// `E2B`, or `E2b` all pick the smaller variant.
pub fn active_gemma_variant() -> BundleRole {
    let active = crate::config::active();
    if active.llm.kind != "onnx-genai" {
        // The variant only matters when the in-process Gemma runtime
        // is selected. For HTTP / other LLM kinds, returning the
        // default keeps any incidental file-system probes pointed at
        // a sane directory.
        return BundleRole::Gemma4;
    }
    match active.llm.model.trim().to_ascii_uppercase().as_str() {
        "E2B" => BundleRole::Gemma4E2B,
        // "" (default), "E4B", or anything unrecognised → E4B.
        _ => BundleRole::Gemma4,
    }
}
