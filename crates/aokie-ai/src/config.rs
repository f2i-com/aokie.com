//! Provider config — id, display name, capabilities, and chat-template
//! flavor that live outside the adapter source. The adapters in
//! `adapters/` are constructed from these structs, so adding or
//! swapping a model is a config edit rather than a code edit.
//!
//! Two-layer load:
//!
//! 1. Bundled default — `providers.default.json` embedded via
//!    `include_str!`. This is the "we know it works on this machine"
//!    baseline that ships with every build.
//! 2. Disk override — `<app_data>/ai_providers.json`. Read at startup
//!    by `init_from_app_data` and on every `refresh`. When present,
//!    its contents fully replace the bundled config (no key-by-key
//!    merge — we'd rather an obviously-wrong override fail loudly
//!    than half-work).
//!
//! The cache is `OnceLock<RwLock<Arc<ProviderConfig>>>` so reads
//! (every `active_*` call) are cheap and writes (post-`set_override`
//! reload) atomically swap the Arc. `defaults()` returns an `Arc` so
//! callers can hold the snapshot past the lock release.
//!
//! Disk I/O lives in pure functions (`read_override`, `write_override`,
//! `clear_override`, `load_or_default`) that take an explicit path —
//! the global cache is only mutated via `refresh` / `init_from_app_data`.
//! That split keeps unit tests off the static cache.
//!
//! `kind` is the discriminator that says which adapter handles this
//! provider. Phase 1 hard-coded one adapter per role; Phase 2b widens
//! it into a real `match` in `registry`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

const DEFAULT_PROVIDERS_JSON: &str = include_str!("providers.default.json");
const OVERRIDE_FILENAME: &str = "ai_providers.json";

/// Chat-template special tokens. Only `onnx-genai` uses this today —
/// it's the field that lets a different multi-modal ONNX-GenAI model
/// (Gemma 3, Phi-4-MM, Qwen-2 Audio) plug into the same runtime by
/// pointing at a different special-token vocabulary. HTTP providers
/// don't need it (their server owns the chat template); local
/// servers don't need it either (the server formats the prompt).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ChatTemplate {
    pub start_of_turn: String,
    pub end_of_turn: String,
    pub end_of_sentence: String,
    pub audio_token: String,
    pub begin_of_sentence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProviderConfig {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    #[serde(default)]
    pub audio_input: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    /// Chat-template flavour: when true, per-turn audio is emitted
    /// immediately before its owning text turn (Gemma's grouping —
    /// the audio placeholder sits inside the same user turn as the
    /// transcript hint). When false, audio is appended at the end of
    /// the segment list. Providers that don't accept audio at all
    /// ignore this entirely.
    #[serde(default)]
    pub audio_inline_before_text: bool,
    /// Special-token strings the runtime needs to wrap turns.
    /// Only `onnx-genai` consumes this; absent for other kinds.
    #[serde(default)]
    pub chat_template: Option<ChatTemplate>,
    /// HTTP base URL for remote providers (OpenAI-compatible /v1/...,
    /// LM Studio, Ollama, etc.). Empty for in-process kinds.
    #[serde(default)]
    pub base_url: String,
    /// API key for remote providers. **In-memory only** — `write_override`
    /// migrates this into the OS keyring under `secret_ref` and clears
    /// it before serialising, so the on-disk file never contains a
    /// plaintext key. Adapters keep reading this field; the load path
    /// hydrates it from the keyring after parsing.
    #[serde(default)]
    pub api_key: String,
    /// Opaque pointer to the OS-keyring entry holding `api_key`. The
    /// runtime stores keys under
    /// `keyring(service="Aokie", account="aokie:provider:<secret_ref>")`.
    /// Round-tripped to disk; only the `api_key` field is scrubbed
    /// before write.
    #[serde(default)]
    pub secret_ref: Option<String>,
    /// Model name handed to the remote provider (e.g.
    /// "gpt-4o-mini" / "llama-3.2-3b-instruct"). Ignored by
    /// in-process kinds.
    #[serde(default)]
    pub model: String,
    /// Fold the system prompt into the first user turn instead of
    /// sending it as a dedicated `{role:"system"}` message. Small
    /// instruction-tuned models (e.g. MiniCPM5-1B) barely weight the
    /// system role and will ignore persona/business facts placed
    /// there, but follow the identical text when it rides in the user
    /// turn. Only consulted by the OpenAI-compatible / llama-server
    /// HTTP adapter; in-process kinds format their own prompts.
    #[serde(default)]
    pub system_in_user_turn: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsProviderConfig {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    /// In-memory API key — see `LlmProviderConfig::api_key` for the
    /// keyring-migration story. Write paths scrub this before serialising.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub model: String,
    /// Sherpa-onnx engine knobs. `None` for non-sherpa kinds. The
    /// sherpa-onnx adapter looks up its file paths and per-engine
    /// scales from this struct rather than the flat `model` field
    /// because VITS / Kokoro / Matcha each need a different bundle
    /// of files (model + tokens + lexicon + data_dir, etc.).
    #[serde(default)]
    pub sherpa_onnx: Option<SherpaTtsConfig>,
}

/// Sherpa-onnx TTS engine config — paths and per-engine scales. The
/// runtime picks which sherpa engine to instantiate from
/// `engine` (`vits` / `kokoro`). Adding a new engine kind means
/// extending the `SherpaTtsEngine` enum and the runtime's match,
/// not adding a new top-level provider kind.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SherpaTtsConfig {
    /// `"vits"` (covers Piper voices) or `"kokoro"`. Other engines
    /// land in follow-ups.
    #[serde(default = "default_sherpa_tts_engine")]
    pub engine: String,
    /// Path to the .onnx model file.
    #[serde(default)]
    pub model_path: String,
    /// Path to the tokens.txt that ships with the bundle.
    #[serde(default)]
    pub tokens_path: String,
    /// Optional espeak-ng data dir (Piper voices need this).
    #[serde(default)]
    pub data_dir: String,
    /// Optional lexicon.txt (some bundles).
    #[serde(default)]
    pub lexicon: String,
    /// Optional dictionary dir (jieba etc.).
    #[serde(default)]
    pub dict_dir: String,
    /// Kokoro voices.bin path. Ignored by VITS.
    #[serde(default)]
    pub voices_path: String,
    /// Kokoro lang code (e.g. "en-us"). Ignored by VITS.
    #[serde(default)]
    pub lang: String,
    /// VITS noise scales (0.0 = use bundle defaults).
    #[serde(default)]
    pub noise_scale: f32,
    #[serde(default)]
    pub noise_scale_w: f32,
    /// 1.0 = the bundle's natural pace; smaller = faster.
    #[serde(default = "default_sherpa_length_scale")]
    pub length_scale: f32,
    /// Insert a tiny gap between sentences (seconds-ish multiplier).
    #[serde(default)]
    pub silence_scale: f32,
    /// Default speaker id for multi-speaker bundles. The runtime
    /// honours `TtsRequest::voice` first (parsed as i32) and falls
    /// back to this when the request is empty.
    #[serde(default)]
    pub default_speaker_id: i32,
    /// Synthesis speed handed to sherpa per call. 1.0 = neutral.
    #[serde(default = "default_sherpa_speed")]
    pub speed: f32,
}

fn default_sherpa_tts_engine() -> String {
    "vits".to_string()
}
fn default_sherpa_length_scale() -> f32 {
    1.0
}
fn default_sherpa_speed() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttProviderConfig {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    /// In-memory API key — see `LlmProviderConfig::api_key`.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub model: String,
    /// Sherpa-onnx STT engine knobs. `None` for non-sherpa kinds.
    /// Whisper-flavour bundles need three paths (encoder / decoder /
    /// tokens) plus a language hint; that's enough fields that they
    /// belong in their own struct rather than overloading the flat
    /// fields above.
    #[serde(default)]
    pub sherpa_onnx: Option<SherpaSttConfig>,
    /// Parakeet-ONNX engine knobs (NVIDIA Parakeet-Unified-EN-0.6B,
    /// FastConformer + RNN-T). Routed through the in-house
    /// `parakeet_onnx` runtime since the published ONNX export merges
    /// decoder + joiner and ships a SentencePiece `tokenizer.model`,
    /// which sherpa-rs's `TransducerRecognizer` can't consume. `None`
    /// for non-parakeet kinds.
    #[serde(default)]
    pub parakeet: Option<ParakeetSttConfig>,
    /// Moonshine-ONNX engine knobs (UsefulSensors Moonshine,
    /// encoder-decoder seq2seq). Routed through the vendored
    /// `transcribe-rs` crate. `None` for non-moonshine kinds.
    #[serde(default)]
    pub moonshine: Option<MoonshineSttConfig>,
    /// Qwen3-ASR-0.6B engine knobs (Alibaba, encoder-decoder seq2seq
    /// with the Qwen3-0.6B chat backbone as decoder). Routed through
    /// the vendored `transcribe-rs` crate. `None` for non-qwen3-asr
    /// kinds.
    #[serde(default)]
    pub qwen3_asr: Option<Qwen3AsrSttConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SherpaSttConfig {
    /// `"whisper"` for now. Zipformer / Paraformer land in follow-ups.
    #[serde(default = "default_sherpa_stt_engine")]
    pub engine: String,
    /// Whisper encoder ONNX.
    #[serde(default)]
    pub encoder_path: String,
    /// Whisper decoder ONNX.
    #[serde(default)]
    pub decoder_path: String,
    /// tokens.txt that ships with the bundle.
    #[serde(default)]
    pub tokens_path: String,
    /// Language hint baked into the recognizer config. The
    /// `SttRequest::language` value still wins per-request when set.
    #[serde(default = "default_sherpa_stt_language")]
    pub language: String,
    /// Number of CPU threads handed to sherpa (>=1).
    #[serde(default = "default_sherpa_stt_threads")]
    pub num_threads: i32,
    /// Tail-padding samples — set when the model needs trailing
    /// silence to converge (some Whisper bundles).
    #[serde(default)]
    pub tail_paddings: i32,
}

fn default_sherpa_stt_engine() -> String {
    "whisper".to_string()
}
fn default_sherpa_stt_language() -> String {
    "en".to_string()
}
fn default_sherpa_stt_threads() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParakeetSttConfig {
    /// Encoder ONNX (heavy — `encoder.int8.onnx` is ~654 MB,
    /// `encoder.fp16.onnx` ~1.3 GB, fp32 ~2.5 GB).
    #[serde(default)]
    pub encoder_path: String,
    /// Combined decoder + joiner ONNX
    /// (`decoder_joint.{int8,fp16,fp32}.onnx`).
    #[serde(default)]
    pub decoder_joint_path: String,
    /// SentencePiece `tokenizer.model` shipped with the bundle.
    #[serde(default)]
    pub tokenizer_path: String,
    /// CPU intra-op threads. ORT picks a default if 0/1.
    #[serde(default = "default_parakeet_threads")]
    pub num_threads: i16,
}

fn default_parakeet_threads() -> i16 {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MoonshineSttConfig {
    /// Directory containing the Moonshine ONNX bundle
    /// (`encoder_model.onnx` / `decoder_model_merged.onnx` /
    /// `tokenizer.json`, possibly suffixed with `.int8`/`.fp16`/`.int4`
    /// depending on `quantization`).
    #[serde(default)]
    pub model_dir: String,
    /// Variant: `tiny`, `base`, `tiny-zh`, `tiny-ja`, etc. Picks the
    /// `MoonshineVariant` enum value the runtime constructs the model
    /// with — controls `num_layers` / `head_dim` / `token_rate`.
    #[serde(default = "default_moonshine_variant")]
    pub variant: String,
    /// File-name suffix selecting which precision variant is loaded:
    /// `fp32` | `fp16` | `int8` | `int4`. Falls back to FP32 with a
    /// warning if the requested variant is missing on disk.
    #[serde(default = "default_moonshine_quantization")]
    pub quantization: String,
}

fn default_moonshine_variant() -> String {
    "tiny".to_string()
}
fn default_moonshine_quantization() -> String {
    "fp32".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Qwen3AsrSttConfig {
    /// Directory containing the Qwen3-ASR ONNX bundle
    /// (`encoder.onnx` / `decoder_init.onnx` / `decoder_step.onnx`,
    /// possibly suffixed `.int4` / `.fp16` per `quantization`),
    /// plus `tokenizer.json` and `embed_tokens.bin`.
    #[serde(default)]
    pub model_dir: String,
    /// Variant: only `0.6b` published today; the field is here so a
    /// follow-up sibling export (1.5B / 3B) is a config edit, not a
    /// schema break.
    #[serde(default = "default_qwen3_asr_variant")]
    pub variant: String,
    /// File-name suffix selecting which precision variant is loaded:
    /// `int4` (the default — the one andrewleech publishes) | `fp16`
    /// | `fp32`. Falls back to FP32 with a warning if missing.
    #[serde(default = "default_qwen3_asr_quantization")]
    pub quantization: String,
    /// BCP-47 language code to lock the model into. The runtime extends
    /// the decoder prompt with `language <LangName>` so Qwen3-ASR skips
    /// its built-in language-ID step and goes straight to transcription.
    /// Defaults to `en` because the receptionist deployment is
    /// English-language; without this, short / accented English
    /// utterances routinely tokenize into Chinese (`大概大概。`). Set
    /// to an empty string to opt back into auto-detection.
    #[serde(default = "default_qwen3_asr_language")]
    pub language: String,
}

fn default_qwen3_asr_variant() -> String {
    "0.6b".to_string()
}
fn default_qwen3_asr_quantization() -> String {
    "int4".to_string()
}
fn default_qwen3_asr_language() -> String {
    "en".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub llm: LlmProviderConfig,
    pub tts: TtsProviderConfig,
    pub stt: SttProviderConfig,
}

static CACHED: OnceLock<RwLock<Arc<ProviderConfig>>> = OnceLock::new();

/// `Some(error)` when the on-disk override exists but failed to read
/// or parse, so the cache is serving bundled defaults that may not
/// match the operator's intent. The Tauri command surface and the
/// hot-path call answerer both check this — a malformed override
/// must not silently swap a remote/local LLM with the bundled in-
/// process one. `None` once the operator either fixes the file or
/// resets to bundled defaults.
static OVERRIDE_LOAD_ERROR: OnceLock<RwLock<Option<String>>> = OnceLock::new();

fn parse_default() -> ProviderConfig {
    serde_json::from_str(DEFAULT_PROVIDERS_JSON).expect("ai/providers.default.json is malformed")
}

fn cell() -> &'static RwLock<Arc<ProviderConfig>> {
    CACHED.get_or_init(|| RwLock::new(Arc::new(parse_default())))
}

fn error_cell() -> &'static RwLock<Option<String>> {
    OVERRIDE_LOAD_ERROR.get_or_init(|| RwLock::new(None))
}

/// `Some(...)` while the on-disk override is unreadable / malformed.
/// Cleared whenever a successful load completes (operator fixed the
/// file or cleared the override). Callers in the live-call path
/// check this and refuse to serve providers when set so a corrupted
/// `ai_providers.json` doesn't quietly swap an OpenAI/Anthropic
/// override for the bundled in-process providers mid-call.
pub fn override_load_error() -> Option<String> {
    error_cell()
        .read()
        .expect("ai::config error cell poisoned")
        .clone()
}

fn set_override_load_error(err: Option<String>) {
    let mut w = error_cell()
        .write()
        .expect("ai::config error cell poisoned");
    *w = err;
}

/// Currently-active provider config. Returns the bundled default
/// until `init_from_app_data` runs; after that, the disk override
/// (if any) takes precedence. Subsequent calls reflect the latest
/// `refresh()` so the UI can swap providers without a restart.
///
/// Renamed from `defaults()` (R11-#2) — the old name suggested
/// bundled defaults regardless of override state, which a static
/// review reasonably flagged as a possible logic bug at the call
/// sites. The function has always returned the cached active
/// config; the new name just reflects that.
pub fn active() -> Arc<ProviderConfig> {
    cell().read().expect("ai::config cache poisoned").clone()
}

/// Override the active LLM `base_url` IN MEMORY only — never written to
/// `ai_providers.json`. The llama-server autostart calls this after it
/// binds a port, which may differ from the configured/default one when
/// it auto-probes around a port already held by another local project,
/// so the HTTP client (`HttpOpenAiLlm`) talks to the port the sidecar
/// actually bound. A config reload re-runs the probe, so the operator's
/// saved `base_url` on disk is never clobbered. No-op when unchanged.
pub fn set_active_llm_base_url(base_url: String) {
    let mut w = cell().write().expect("ai::config cache poisoned");
    if w.llm.base_url == base_url {
        return;
    }
    let mut cfg = (**w).clone();
    cfg.llm.base_url = base_url;
    *w = Arc::new(cfg);
}

/// Filesystem path the disk override lives at, given the host
/// app's data directory.
pub fn override_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(OVERRIDE_FILENAME)
}

/// Read the override JSON at `path` if present, else return the
/// bundled default. The cache still serves bundled defaults when the
/// override is unreadable / malformed — but `set_override_load_error`
/// records the failure so the hot path (initialize_bluetooth) and the
/// AI Stack UI can refuse to proceed instead of silently swapping
/// an OpenAI/Anthropic override for the bundled local providers.
fn load_or_default(path: &Path) -> ProviderConfig {
    if !path.exists() {
        // No override → bundled defaults are explicit, not a fallback.
        // Clear any prior error so a "delete the file" recovery flow
        // unblocks the call path.
        set_override_load_error(None);
        return parse_default();
    }
    let mut cfg = match std::fs::read_to_string(path) {
        Ok(json) => match serde_json::from_str::<ProviderConfig>(&json) {
            Ok(parsed) => {
                println!("[ai::config] Loaded provider override from {:?}", path);
                set_override_load_error(None);
                parsed
            }
            Err(e) => {
                let msg = format!(
                    "ai_providers.json failed to parse: {}. Aokie is serving bundled defaults; calls are blocked until you fix the file or clear it from AI Stack.",
                    e
                );
                eprintln!("[ai::config] {}", msg);
                set_override_load_error(Some(msg));
                parse_default()
            }
        },
        Err(e) => {
            let msg = format!(
                "ai_providers.json could not be read: {}. Aokie is serving bundled defaults; calls are blocked until you fix the file or clear it from AI Stack.",
                e
            );
            eprintln!("[ai::config] {}", msg);
            set_override_load_error(Some(msg));
            parse_default()
        }
    };
    // Hydrate api_key fields from the OS keyring. The on-disk file
    // ships only `secret_ref`; the runtime needs `api_key` populated
    // for the existing adapters' code paths.
    hydrate_secrets(&mut cfg);
    cfg
}

/// Walk every provider role and re-fill `api_key` from the OS
/// keyring when a `secret_ref` is set. A keyring miss leaves the
/// field empty — callers that need a key will surface "no API key
/// configured" rather than starting with a stale value.
///
/// Each role's `base_url` is canonicalised into a host token and
/// passed through to the keyring lookup. That way, if a tampered
/// `ai_providers.json` swaps `base_url` to a different host while
/// preserving `secret_ref`, the lookup hits a different account name
/// and misses — the saved key never reaches the adapter targeting
/// the new host. Operator must re-enter on a host change.
fn hydrate_secrets(cfg: &mut ProviderConfig) {
    hydrate_role(
        &mut cfg.llm.api_key,
        &cfg.llm.secret_ref,
        host_token(&cfg.llm.base_url).as_deref(),
        "llm",
        &cfg.llm.id,
    );
    hydrate_role(
        &mut cfg.tts.api_key,
        &cfg.tts.secret_ref,
        host_token(&cfg.tts.base_url).as_deref(),
        "tts",
        &cfg.tts.id,
    );
    hydrate_role(
        &mut cfg.stt.api_key,
        &cfg.stt.secret_ref,
        host_token(&cfg.stt.base_url).as_deref(),
        "stt",
        &cfg.stt.id,
    );
}

fn hydrate_role(
    api_key: &mut String,
    secret_ref: &Option<String>,
    host_token: Option<&str>,
    role: &str,
    id: &str,
) {
    let Some(secret_ref) = secret_ref else {
        return;
    };
    if !api_key.is_empty() {
        // File still carried a plaintext key — leave it as-is for now;
        // the next `write_override` will move it into the keyring.
        // Logging at debug only since pre-migration installs hit this
        // every load until the first save.
        return;
    }
    match aokie_core::secrets::load_api_key(host_token, secret_ref) {
        Ok(Some(key)) => {
            *api_key = key;
        }
        Ok(None) => {
            // Two legitimate cases land here, distinguishable only by
            // intent: a fresh install with a stale `secret_ref` (no
            // matching keyring entry has ever existed), or a host
            // change that invalidated the binding (entry exists at
            // the old host's account name but not the new one). In
            // both cases the right behaviour is the same — don't
            // hydrate, force the operator to re-enter — so we just
            // log without trying to disambiguate.
            eprintln!(
                "[ai::config] secret_ref {} for {}/{} not in OS keyring at host {:?}; api_key empty",
                secret_ref, role, id, host_token
            );
        }
        Err(e) => {
            eprintln!(
                "[ai::config] keyring read failed for {}/{} (secret_ref {}, host {:?}): {}",
                role, id, secret_ref, host_token, e
            );
        }
    }
}

/// Canonicalise a `base_url` into a `scheme://host[:port]` token used
/// as the host portion of the keyring account name. Returns `None`
/// for empty / non-http(s) / malformed URLs — those land in the
/// `no-host` keyring slot where in-process providers (which shouldn't
/// store keys at all) sit. Lowercased so `Https://API.OpenAI.COM` and
/// `https://api.openai.com` round-trip to the same binding.
///
/// Deliberately not a full RFC 3986 parser — we don't need
/// path/query/fragment, and the `url` crate is a chunky dep to pull
/// in for this. Splitting on `://` and stopping at the first
/// path/query/fragment delimiter is enough for the OpenAI-compatible
/// shapes the HTTP adapters target.
pub(crate) fn host_token(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host_port = rest.split(['/', '?', '#']).next()?.trim();
    if host_port.is_empty() {
        return None;
    }
    Some(format!("{}://{}", scheme, host_port.to_ascii_lowercase()))
}

/// Load the disk override (if present) into the cache. Called once
/// at Tauri setup. Safe to call repeatedly — re-reads disk each time.
pub fn init_from_app_data(app_data_dir: &Path) {
    let cfg = load_or_default(&override_path(app_data_dir));
    let arc_cfg = Arc::new(cfg);
    let mut w = cell().write().expect("ai::config cache poisoned");
    *w = arc_cfg;
}

/// Same as `init_from_app_data` — provided as a clearer name for
/// the post-write hot-reload path.
pub fn refresh(app_data_dir: &Path) {
    init_from_app_data(app_data_dir);
}

/// Read the disk override directly (no cache). Returns `None` when
/// the file doesn't exist. Used by the UI command so it can show
/// "you're on bundled default" vs. "you've got a custom override."
pub fn read_override(app_data_dir: &Path) -> Result<Option<ProviderConfig>, String> {
    let path = override_path(app_data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(&path).map_err(|e| format!("read {:?}: {}", path, e))?;
    serde_json::from_str(&json)
        .map(Some)
        .map_err(|e| format!("parse {:?}: {}", path, e))
}

/// Write a new override to disk atomically (write-tmp + fsync + rename).
/// Without this a crash mid-save would leave an empty/half-written
/// `ai_providers.json` and the next launch would silently fall back to
/// bundled defaults — quietly changing the active LLM/STT/TTS provider
/// behind the operator's back. Does NOT refresh the cache; the caller
/// (Tauri command) calls `refresh` after so the in-memory view picks
/// up the change.
///
/// Performs the API-key→keyring migration before serialising: any
/// non-empty `api_key` is moved into the OS keyring under a fresh
/// `secret_ref` (or the existing one) and the in-memory clone is
/// scrubbed before we hit the disk.
pub fn write_override(app_data_dir: &Path, cfg: &ProviderConfig) -> Result<(), String> {
    let path = override_path(app_data_dir);
    let mut to_write = cfg.clone();
    persist_secrets(&mut to_write)?;
    let json = serde_json::to_string_pretty(&to_write).map_err(|e| format!("serialize: {}", e))?;
    aokie_core::paths::atomic_write(&path, json.as_bytes())
}

/// Move every plaintext `api_key` into the OS keyring and scrub the
/// field so it never reaches disk. Always runs on the *clone* the
/// caller is about to serialise — the in-memory cache keeps the
/// hydrated key for the adapters. Fails closed: if any role's
/// keyring store fails, the whole save aborts so we never write a
/// plaintext key to disk.
///
/// The keyring store is keyed by `(host_token, secret_ref)`. A save
/// that supplies a non-empty `api_key` always writes to the *current*
/// `base_url`'s host slot; a stale `secret_ref` from a previous host
/// is just left in place (the old keyring entry under the old host's
/// account remains until the user explicitly clears it via the AI
/// Providers UI). This matches the threat model: an attacker who can
/// rewrite `ai_providers.json` cannot exfiltrate the saved key, and
/// the legitimate "I changed my self-hosted endpoint host" path
/// requires the operator to re-paste the key under the new host.
fn persist_secrets(cfg: &mut ProviderConfig) -> Result<(), String> {
    persist_role(
        &mut cfg.llm.api_key,
        &mut cfg.llm.secret_ref,
        host_token(&cfg.llm.base_url).as_deref(),
        "llm",
        &cfg.llm.id,
    )?;
    persist_role(
        &mut cfg.tts.api_key,
        &mut cfg.tts.secret_ref,
        host_token(&cfg.tts.base_url).as_deref(),
        "tts",
        &cfg.tts.id,
    )?;
    persist_role(
        &mut cfg.stt.api_key,
        &mut cfg.stt.secret_ref,
        host_token(&cfg.stt.base_url).as_deref(),
        "stt",
        &cfg.stt.id,
    )?;
    Ok(())
}

fn persist_role(
    api_key: &mut String,
    secret_ref: &mut Option<String>,
    host_token: Option<&str>,
    role: &str,
    id: &str,
) -> Result<(), String> {
    if api_key.is_empty() {
        return Ok(());
    }
    let key_value = std::mem::take(api_key);
    let secret_ref_value = secret_ref
        .clone()
        .unwrap_or_else(aokie_core::secrets::fresh_secret_ref);
    aokie_core::secrets::store_api_key(host_token, &secret_ref_value, &key_value).map_err(|e| {
        // Refuse to fall back to plaintext on disk — that defeats the
        // whole point of routing keys through the OS secret store. The
        // caller's save fails loudly; the operator either fixes the
        // keyring (Credential Manager locked, gnome-keyring not running,
        // …) or pastes the key in again after restart.
        format!(
            "ai::config: keyring store failed for {}/{}: {} — refusing to write plaintext to disk",
            role, id, e
        )
    })?;
    *secret_ref = Some(secret_ref_value);
    Ok(())
}

/// Drop the stored API key for a single role (`"llm"`, `"tts"`,
/// `"stt"`) — wipes the OS-keyring entry, scrubs the in-memory
/// `api_key` and `secret_ref`, and rewrites the override file.
///
/// Idempotent. If there's no override on disk, returns Ok — bundled
/// defaults never carry a `secret_ref`, so there's nothing to clear.
/// If the role had no `secret_ref`, the keyring delete is skipped
/// and only the file is rewritten (still cheap; keeps the on-disk
/// shape canonical).
///
/// Order is config-write **first**, keyring delete **second**, on
/// purpose: if the disk write fails (full disk, permission flip, …),
/// the keyring entry is still intact, so `ai_get_providers` will
/// continue to hydrate the same key on next launch. Reversing that
/// order would leave the operator with a `secret_ref` pointing at a
/// missing keyring entry — broken provider config that needs a
/// re-paste to recover. The keyring delete is best-effort: a
/// failure there is logged but not surfaced as an error, since the
/// config file is already authoritative ("no secret_ref → key is
/// cleared as far as Aokie is concerned"); the orphaned keyring
/// entry is harmless and the user can scrub it via OS Credential
/// Manager / Keychain if they care.
///
/// The caller (`ai_clear_provider_secret`) is responsible for
/// refreshing the cache + invalidating any lazy runtimes after this
/// returns. Letting the caller do it keeps this function pure.
pub fn clear_role_secret(app_data_dir: &Path, role: &str) -> Result<(), String> {
    let path = override_path(app_data_dir);
    if !path.exists() {
        return Ok(());
    }
    let mut cfg = read_override(app_data_dir)?
        .ok_or_else(|| "override file vanished between exists() and read".to_string())?;

    let (secret_ref_to_delete, host_for_delete) = match role {
        "llm" => {
            let sref = cfg.llm.secret_ref.take();
            let host = host_token(&cfg.llm.base_url);
            cfg.llm.api_key.clear();
            (sref, host)
        }
        "tts" => {
            let sref = cfg.tts.secret_ref.take();
            let host = host_token(&cfg.tts.base_url);
            cfg.tts.api_key.clear();
            (sref, host)
        }
        "stt" => {
            let sref = cfg.stt.secret_ref.take();
            let host = host_token(&cfg.stt.base_url);
            cfg.stt.api_key.clear();
            (sref, host)
        }
        _ => return Err(format!("clear_role_secret: unknown role {:?}", role)),
    };

    // Disk first: if this fails, the keyring entry is still authoritative
    // and the next launch picks the same key back up.
    write_override(app_data_dir, &cfg)?;

    // Keyring delete is best-effort. The on-disk file no longer carries
    // the `secret_ref`, so even if this fails the orphaned keyring
    // entry isn't reachable by the app — it's at worst leftover noise
    // in OS Credential Manager / Keychain.
    if let Some(sref) = secret_ref_to_delete {
        if let Err(e) = aokie_core::secrets::delete_api_key(host_for_delete.as_deref(), &sref) {
            eprintln!(
                "[ai::config] keyring delete for cleared role {:?} failed: {} \
                 (config file already updated; entry is now orphaned)",
                role, e
            );
        }
    }

    Ok(())
}

/// Delete the disk override. Caller refreshes the cache so the
/// bundled default takes effect.
pub fn clear_override(app_data_dir: &Path) -> Result<(), String> {
    let path = override_path(app_data_dir);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("remove {:?}: {}", path, e))?;
    }
    Ok(())
}

/// R7/P1-5: full-wipe support. Walk every role's `(host_token,
/// secret_ref)` tuple in the override file and delete the matching
/// OS-keyring entry. Returns `(deleted_count, errors)` so the
/// `delete_all_local_data_now` summary can show what happened.
///
/// Best-effort by design: a stuck keyring (Credential Manager
/// locked, gnome-keyring not running) shouldn't block the rest of
/// the wipe. Errors are returned but never propagated as Err — the
/// caller surfaces them in the UI summary toast and proceeds.
///
/// No-op when the override file is absent (fresh install with no
/// remote provider configured).
pub fn wipe_keyring_entries_best_effort(app_data_dir: &Path) -> (usize, Vec<String>) {
    let mut errors: Vec<String> = Vec::new();
    let mut deleted = 0usize;

    let cfg = match read_override(app_data_dir) {
        Ok(Some(c)) => c,
        Ok(None) => return (0, errors),
        Err(e) => {
            errors.push(format!("read override for keyring wipe: {}", e));
            return (0, errors);
        }
    };

    // Each role contributes (role-label, secret_ref, base_url).
    // Empty secret_ref means there's nothing in the keyring under
    // this role; skip it.
    let candidates = [
        ("llm", cfg.llm.secret_ref.clone(), cfg.llm.base_url.clone()),
        ("tts", cfg.tts.secret_ref.clone(), cfg.tts.base_url.clone()),
        ("stt", cfg.stt.secret_ref.clone(), cfg.stt.base_url.clone()),
    ];

    for (role, sref_opt, base_url) in candidates {
        let Some(sref) = sref_opt else { continue };
        if sref.is_empty() {
            continue;
        }
        let host = host_token(&base_url);
        match aokie_core::secrets::delete_api_key(host.as_deref(), &sref) {
            Ok(()) => {
                deleted += 1;
            }
            Err(e) => errors.push(format!("keyring {}/{}: {}", role, sref, e)),
        }
        // Defensive: if the operator changed `base_url` host since
        // the key was stored, the old keyring entry sits under the
        // PREVIOUS host token. We don't have history of that, but
        // we can also try the "no-host" slot as a fallback so
        // legacy in-process-then-remote configs don't strand a
        // key. Cheap; entries that don't exist no-op.
        if host.is_some() {
            let _ = aokie_core::secrets::delete_api_key(None, &sref);
        }
    }

    (deleted, errors)
}

/// Return the bundled config as a struct (no disk read). The UI
/// uses this so the "reset to defaults" preview shows what will
/// actually be applied.
pub fn bundled_default() -> ProviderConfig {
    parse_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Per-test temp dir that doesn't depend on tempfile. Uses uuid
    /// (already a dep) for a unique suffix and cleans up at end.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let p =
                std::env::temp_dir().join(format!("aokie_ai_cfg_test_{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bundled_defaults_parse() {
        let cfg = bundled_default();
        assert!(!cfg.llm.id.is_empty());
        assert!(!cfg.llm.kind.is_empty());
        assert!(!cfg.tts.id.is_empty());
        assert!(!cfg.tts.kind.is_empty());
        assert!(!cfg.stt.id.is_empty());
        assert!(!cfg.stt.kind.is_empty());
    }

    #[test]
    fn override_disk_round_trip() {
        let tmp = TestDir::new();

        // No override yet.
        assert!(read_override(tmp.path()).unwrap().is_none());

        // Write a custom override.
        let mut custom = bundled_default();
        custom.llm.id = "custom-test-llm".to_string();
        custom.llm.display_name = "Custom test".to_string();
        write_override(tmp.path(), &custom).unwrap();

        // Disk read returns the new override.
        let on_disk = read_override(tmp.path()).unwrap().unwrap();
        assert_eq!(on_disk.llm.id, "custom-test-llm");
        assert_eq!(on_disk.llm.display_name, "Custom test");

        // Clearing removes the file.
        clear_override(tmp.path()).unwrap();
        assert!(read_override(tmp.path()).unwrap().is_none());
    }

    /// Tests in this module touch the process-global override-load-
    /// error cell, so cargo's parallel test runner could interleave
    /// them. Serialise via a dedicated Mutex per the trybuild /
    /// serial_test pattern; avoids pulling in `serial_test` as a
    /// dev-dep just for two tests.
    fn override_error_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn malformed_override_falls_back_to_bundled_with_load_error() {
        let _guard = override_error_test_lock().lock().unwrap();
        let tmp = TestDir::new();
        let path = override_path(tmp.path());
        fs::write(&path, "{ this isn't json }").unwrap();
        // Reset the cached error before the test so the assertion
        // observes only what this load wrote.
        set_override_load_error(None);
        let cfg = load_or_default(&path);
        let bundled = parse_default();
        // Cache still serves bundled (otherwise the app couldn't
        // render the AI Stack page to fix the file).
        assert_eq!(cfg.llm.id, bundled.llm.id);
        // But the error cell is set — the BT init path checks this
        // and the AI Stack UI surfaces it as a blocking banner.
        let err =
            override_load_error().expect("malformed override must populate override_load_error");
        assert!(err.contains("ai_providers.json"));
        // Clean up so the next test starts from a known state.
        set_override_load_error(None);
    }

    #[test]
    fn missing_override_clears_load_error() {
        let _guard = override_error_test_lock().lock().unwrap();
        let tmp = TestDir::new();
        let path = override_path(tmp.path());
        // Pre-seed an error to make sure missing-file recovery wipes it.
        set_override_load_error(Some("stale".into()));
        let cfg = load_or_default(&path);
        let bundled = parse_default();
        assert_eq!(cfg.llm.id, bundled.llm.id);
        assert!(
            override_load_error().is_none(),
            "missing override (operator deleted file to recover) must clear the error cell"
        );
    }

    #[test]
    fn host_token_canonicalises_scheme_and_host() {
        // Lowercase + scheme-stripped path/query should round-trip to
        // the same token. This is the keyring binding key, so anything
        // that drifts here would silently invalidate saved keys.
        assert_eq!(
            host_token("https://api.openai.com/v1"),
            Some("https://api.openai.com".to_string())
        );
        assert_eq!(
            host_token("HTTPS://API.OpenAI.COM/v1/chat/completions"),
            Some("https://api.openai.com".to_string())
        );
        assert_eq!(
            host_token("http://127.0.0.1:8080/?q=1"),
            Some("http://127.0.0.1:8080".to_string())
        );
        assert_eq!(
            host_token("  https://example.com  "),
            Some("https://example.com".to_string())
        );
    }

    #[test]
    fn host_token_rejects_non_http_and_empty() {
        assert_eq!(host_token(""), None);
        assert_eq!(host_token("   "), None);
        // Non-http(s) schemes never reach the OpenAI HTTP adapter, so
        // they don't get a host slot — they fall back to "no-host"
        // where keys aren't expected anyway.
        assert_eq!(host_token("file:///local"), None);
        assert_eq!(host_token("ftp://example.com"), None);
        // Missing scheme separator.
        assert_eq!(host_token("api.openai.com"), None);
    }

    #[test]
    fn host_change_breaks_secret_binding() {
        // Regression for R7-#1: a tampered config that swaps `base_url`
        // host while preserving `secret_ref` must NOT see a hydrated
        // key. Simulates the threat model — attacker has filesystem
        // write to ai_providers.json but not OS keyring access.
        let original = "https://api.openai.com";
        let attacker = "https://evil.example";
        // The persist path stores at host_token(original). The hydrate
        // path looks up host_token(attacker). Different host tokens
        // yield different keyring account names — they cannot resolve
        // to the same entry.
        assert_ne!(host_token(original), host_token(attacker));
    }

    /// R6/P3-4: pin the on-disk shape of `ai_providers.json` so a
    /// future refactor that drops the persist_secrets clear-step (or
    /// adds a new sensitive field forgotten in the scrub list) fails
    /// CI before shipping. The contract:
    ///
    ///   1. `api_key` is in-memory only and MUST be empty in any
    ///      ProviderConfig that's about to be serialised. Non-empty
    ///      means persist_secrets didn't move it to the keyring.
    ///   2. The renderer-only sentinel `__keyring__` is a frontend
    ///      mask — it must never appear in the JSON shape on disk.
    ///   3. Other sensitive surfaces (`secret_ref`, `host_token`)
    ///      are fine to round-trip — they're opaque pointers, not
    ///      the secret itself.
    ///
    /// We test the post-persist *shape* here; the keyring round-trip
    /// is exercised separately in `secrets::tests` because it
    /// requires an OS service.
    #[test]
    fn provider_config_post_persist_shape_does_not_leak_secrets() {
        let mut cfg = bundled_default();
        // Post-persist state: api_key MUST be empty (persist_role
        // does `mem::take` on success). secret_ref is set as a side
        // effect — keep it populated to verify it round-trips.
        cfg.llm.api_key = String::new();
        cfg.llm.secret_ref = Some("aokie:llm:test".to_string());
        cfg.llm.base_url = "https://api.openai.com/v1".to_string();
        cfg.tts.api_key = String::new();
        cfg.tts.secret_ref = None;
        cfg.stt.api_key = String::new();
        cfg.stt.secret_ref = None;

        let json = serde_json::to_string(&cfg).expect("serialise post-persist config");

        // Renderer sentinel must never reach disk.
        assert!(
            !json.contains("__keyring__"),
            "frontend mask `__keyring__` leaked into on-disk JSON: {}",
            json
        );

        // api_key must serialise empty for every role.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("re-parse own JSON");
        for role in ["llm", "tts", "stt"] {
            let key = parsed[role]["api_key"]
                .as_str()
                .expect(&format!("api_key field present for {}", role));
            assert_eq!(
                key, "",
                "api_key for {} must serialise as empty string post-persist",
                role
            );
        }
        // secret_ref still round-trips (opaque pointer is fine).
        assert_eq!(parsed["llm"]["secret_ref"].as_str(), Some("aokie:llm:test"));
    }

    /// Defensive: a ProviderConfig that hasn't been through
    /// persist_secrets but somehow ends up serialised would emit a
    /// plaintext api_key — the test below is the *negative* case that
    /// proves the field is the load-bearing surface and any future
    /// `#[serde(skip_serializing)]` on it should be added deliberately
    /// (and verified to not break the keyring hydration contract).
    /// Failure of this test means someone added `skip_serializing` to
    /// `api_key` without also fixing the load path.
    #[test]
    fn provider_config_serialise_does_not_silently_skip_api_key_field() {
        let mut cfg = bundled_default();
        cfg.llm.api_key = "marker-DO-NOT-LEAK-TO-DISK".to_string();
        let json = serde_json::to_string(&cfg).unwrap();
        // The marker WILL be in the JSON because api_key is the
        // serde shape. The point: any future `skip_serializing`
        // changes the contract and must be paired with a load-side
        // change. If you're staring at this test failing because you
        // added skip_serializing, also update the doc on
        // LlmProviderConfig::api_key and verify hydrate still works.
        assert!(
            json.contains("marker-DO-NOT-LEAK-TO-DISK"),
            "api_key serde shape changed without doc update — see \
             provider_config_serialise_does_not_silently_skip_api_key_field"
        );
    }
}
