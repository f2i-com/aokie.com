use std::collections::HashSet;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aokie_ai::config::SherpaTtsConfig;
use aokie_ai::runtimes::moonshine_transcribe::MoonshineTranscribeRuntime;
use aokie_ai::runtimes::onnx_tts::OnnxTtsRuntime;
use aokie_ai::runtimes::parakeet_onnx::ParakeetOnnxRuntime;
use aokie_ai::runtimes::qwen3_asr_transcribe::Qwen3AsrTranscribeRuntime;
use aokie_ai::runtimes::sherpa_onnx::tts::SherpaOnnxTtsRuntime;
use serde::Deserialize;
use serde_json::json;
use transcribe_rs::onnx::moonshine::MoonshineVariant;
use transcribe_rs::onnx::qwen3_asr::Qwen3AsrVariant;
use transcribe_rs::onnx::Quantization;

pub const DEFAULT_PORT: u16 = 17_920;
/// Default ports for single-lane instances (VOX-401): the desktop runs
/// `--mode stt` and `--mode tts` as SEPARATE services side by side, so each
/// mode gets its own well-known port when `--port` is absent.
pub const DEFAULT_STT_PORT: u16 = 17_921;
pub const DEFAULT_TTS_PORT: u16 = 17_922;
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

const PARAKEET_MODEL_ID: &str = "parakeet-tdt-0.6b";
const MOONSHINE_MODEL_ID: &str = "moonshine-tiny";
const QWEN3_ASR_MODEL_ID: &str = "qwen3-asr-0.6b";
const POCKET_MODEL_ID: &str = "pocket-tts";

// ---------------------------------------------------------------------------
// Configuration (VOX-401): mode split + engine selection.
// Precedence per field: CLI args > env vars > JSON config file > defaults.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerMode {
    Stt,
    Tts,
    Both,
}

impl ServerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerMode::Stt => "stt",
            ServerMode::Tts => "tts",
            ServerMode::Both => "both",
        }
    }

    pub fn stt_enabled(self) -> bool {
        matches!(self, ServerMode::Stt | ServerMode::Both)
    }

    pub fn tts_enabled(self) -> bool {
        matches!(self, ServerMode::Tts | ServerMode::Both)
    }

    /// The port used when no source names one. `both` keeps the historical
    /// 17920 so an argument-less launch is byte-identical to before the split.
    pub fn default_port(self) -> u16 {
        match self {
            ServerMode::Stt => DEFAULT_STT_PORT,
            ServerMode::Tts => DEFAULT_TTS_PORT,
            ServerMode::Both => DEFAULT_PORT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttEngineKind {
    Parakeet,
    Moonshine,
    Qwen3Asr,
}

impl SttEngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SttEngineKind::Parakeet => "parakeet",
            SttEngineKind::Moonshine => "moonshine",
            SttEngineKind::Qwen3Asr => "qwen3-asr",
        }
    }

    pub fn model_id(self) -> &'static str {
        match self {
            SttEngineKind::Parakeet => PARAKEET_MODEL_ID,
            SttEngineKind::Moonshine => MOONSHINE_MODEL_ID,
            SttEngineKind::Qwen3Asr => QWEN3_ASR_MODEL_ID,
        }
    }

    /// Default model folder name under `<app_data>/models/`.
    fn default_dir_name(self) -> &'static str {
        match self {
            SttEngineKind::Parakeet => "parakeet",
            SttEngineKind::Moonshine => "moonshine",
            SttEngineKind::Qwen3Asr => "qwen3-asr",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsEngineKind {
    Pocket,
    Sherpa,
}

impl TtsEngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TtsEngineKind::Pocket => "pocket",
            TtsEngineKind::Sherpa => "sherpa",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub mode: ServerMode,
    pub port: u16,
    pub stt_engine: SttEngineKind,
    /// Override for the STT model folder; `None` = the engine's convention
    /// under `<app_data>/models/`.
    pub stt_model_dir: Option<PathBuf>,
    pub tts_engine: TtsEngineKind,
    /// Override for the TTS model folder. For pocket this is the bundle dir
    /// (default `<app_data>/models/pocket_tts_onnx`); for sherpa it is a
    /// VOICE BUNDLE folder (one .onnx + tokens.txt) — `None` scans
    /// `<app_data>/models/tts` alphabetically for the first valid bundle.
    pub tts_model_dir: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            mode: ServerMode::Both,
            port: DEFAULT_PORT,
            stt_engine: SttEngineKind::Parakeet,
            stt_model_dir: None,
            tts_engine: TtsEngineKind::Pocket,
            tts_model_dir: None,
        }
    }
}

/// The values named on the command line (all optional — absent falls through
/// to env / config file / defaults).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliConfig {
    pub mode: Option<ServerMode>,
    pub port: Option<u16>,
    pub stt_engine: Option<SttEngineKind>,
    pub stt_model_dir: Option<PathBuf>,
    pub tts_engine: Option<TtsEngineKind>,
    pub tts_model_dir: Option<PathBuf>,
    /// `--config PATH` — explicit JSON config file (else `<app_data>/
    /// voice-server.json` is auto-loaded when present).
    pub config_path: Option<PathBuf>,
}

/// Raw env-var values (`AOKIE_VOICE_*`). Kept as strings so precedence can be
/// decided BEFORE parsing — an env value that loses to a CLI arg is never
/// validated, exactly like the pre-split `--port` behaviour.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvConfig {
    pub mode: Option<String>,
    pub port: Option<String>,
    pub stt_engine: Option<String>,
    pub stt_model_dir: Option<String>,
    pub tts_engine: Option<String>,
    pub tts_model_dir: Option<String>,
}

impl EnvConfig {
    pub fn from_process_env() -> Self {
        fn get(name: &str) -> Option<String> {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        }
        Self {
            mode: get("AOKIE_VOICE_MODE"),
            port: get("AOKIE_VOICE_PORT"),
            stt_engine: get("AOKIE_VOICE_STT_ENGINE"),
            stt_model_dir: get("AOKIE_VOICE_STT_MODEL_DIR"),
            tts_engine: get("AOKIE_VOICE_TTS_ENGINE"),
            tts_model_dir: get("AOKIE_VOICE_TTS_MODEL_DIR"),
        }
    }
}

/// Flat JSON config file shape: `{"mode","port","sttEngine","sttModelDir",
/// "ttsEngine","ttsModelDir"}` — all keys optional, unknown keys tolerated
/// (forward compat).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default, rename = "sttEngine")]
    pub stt_engine: Option<String>,
    #[serde(default, rename = "sttModelDir")]
    pub stt_model_dir: Option<String>,
    #[serde(default, rename = "ttsEngine")]
    pub tts_engine: Option<String>,
    #[serde(default, rename = "ttsModelDir")]
    pub tts_model_dir: Option<String>,
}

pub fn parse_mode(value: &str, source: &str) -> Result<ServerMode, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stt" => Ok(ServerMode::Stt),
        "tts" => Ok(ServerMode::Tts),
        "both" => Ok(ServerMode::Both),
        other => Err(format!(
            "{source} must be one of stt|tts|both, got {other:?}"
        )),
    }
}

pub fn parse_stt_engine(value: &str, source: &str) -> Result<SttEngineKind, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "parakeet" => Ok(SttEngineKind::Parakeet),
        "moonshine" => Ok(SttEngineKind::Moonshine),
        "qwen3-asr" | "qwen3_asr" | "qwen3asr" => Ok(SttEngineKind::Qwen3Asr),
        other => Err(format!(
            "{source} must be one of parakeet|moonshine|qwen3-asr, got {other:?}"
        )),
    }
}

pub fn parse_tts_engine(value: &str, source: &str) -> Result<TtsEngineKind, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "pocket" => Ok(TtsEngineKind::Pocket),
        "sherpa" => Ok(TtsEngineKind::Sherpa),
        other => Err(format!(
            "{source} must be one of pocket|sherpa, got {other:?}"
        )),
    }
}

fn parse_port(value: &str, source: &str) -> Result<u16, String> {
    value
        .trim()
        .parse::<u16>()
        .map_err(|e| format!("{source} must be a TCP port number: {e}"))
}

/// Parse the CLI surface. Accepts `--flag value` and `--flag=value` for every
/// flag; anything unrecognised stays an error (same policy as the pre-split
/// `--port`-only parser).
pub fn parse_cli_args<I>(args: I) -> Result<CliConfig, String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).skip(1).collect();
    let mut cli = CliConfig::default();

    fn take_value(
        argv: &[String],
        i: &mut usize,
        flag: &str,
        inline: Option<String>,
    ) -> Result<String, String> {
        if let Some(v) = inline {
            return Ok(v);
        }
        *i += 1;
        argv.get(*i)
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value"))
    }

    /// Blank path values mean "use the default convention" — stored as None.
    fn nonblank_path(value: String) -> Option<PathBuf> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    }

    let mut i = 0usize;
    while i < argv.len() {
        let (flag, inline): (String, Option<String>) = match argv[i].split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (argv[i].clone(), None),
        };
        match flag.as_str() {
            "--mode" => {
                let v = take_value(&argv, &mut i, "--mode", inline)?;
                cli.mode = Some(parse_mode(&v, "--mode")?);
            }
            "--port" => {
                let v = take_value(&argv, &mut i, "--port", inline)?;
                cli.port = Some(parse_port(&v, "--port")?);
            }
            "--stt-engine" => {
                let v = take_value(&argv, &mut i, "--stt-engine", inline)?;
                cli.stt_engine = Some(parse_stt_engine(&v, "--stt-engine")?);
            }
            "--stt-model-dir" => {
                let v = take_value(&argv, &mut i, "--stt-model-dir", inline)?;
                cli.stt_model_dir = nonblank_path(v);
            }
            "--tts-engine" => {
                let v = take_value(&argv, &mut i, "--tts-engine", inline)?;
                cli.tts_engine = Some(parse_tts_engine(&v, "--tts-engine")?);
            }
            "--tts-model-dir" => {
                let v = take_value(&argv, &mut i, "--tts-model-dir", inline)?;
                cli.tts_model_dir = nonblank_path(v);
            }
            "--config" => {
                let v = take_value(&argv, &mut i, "--config", inline)?;
                cli.config_path = nonblank_path(v);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
        i += 1;
    }
    Ok(cli)
}

fn nonblank(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// Merge the three override layers over the defaults. Pure — every source is
/// passed in, so the whole precedence contract is unit-testable.
pub fn merge_config(
    cli: &CliConfig,
    env: &EnvConfig,
    file: &FileConfig,
) -> Result<ServerConfig, String> {
    let mode = if let Some(m) = cli.mode {
        m
    } else if let Some(v) = nonblank(&env.mode) {
        parse_mode(v, "AOKIE_VOICE_MODE")?
    } else if let Some(v) = nonblank(&file.mode) {
        parse_mode(v, "config file \"mode\"")?
    } else {
        ServerMode::Both
    };

    let port = if let Some(p) = cli.port {
        p
    } else if let Some(v) = nonblank(&env.port) {
        parse_port(v, "AOKIE_VOICE_PORT")?
    } else if let Some(p) = file.port {
        p
    } else {
        mode.default_port()
    };

    let stt_engine = if let Some(e) = cli.stt_engine {
        e
    } else if let Some(v) = nonblank(&env.stt_engine) {
        parse_stt_engine(v, "AOKIE_VOICE_STT_ENGINE")?
    } else if let Some(v) = nonblank(&file.stt_engine) {
        parse_stt_engine(v, "config file \"sttEngine\"")?
    } else {
        SttEngineKind::Parakeet
    };

    let tts_engine = if let Some(e) = cli.tts_engine {
        e
    } else if let Some(v) = nonblank(&env.tts_engine) {
        parse_tts_engine(v, "AOKIE_VOICE_TTS_ENGINE")?
    } else if let Some(v) = nonblank(&file.tts_engine) {
        parse_tts_engine(v, "config file \"ttsEngine\"")?
    } else {
        TtsEngineKind::Pocket
    };

    let stt_model_dir = cli
        .stt_model_dir
        .clone()
        .or_else(|| nonblank(&env.stt_model_dir).map(PathBuf::from))
        .or_else(|| nonblank(&file.stt_model_dir).map(PathBuf::from));
    let tts_model_dir = cli
        .tts_model_dir
        .clone()
        .or_else(|| nonblank(&env.tts_model_dir).map(PathBuf::from))
        .or_else(|| nonblank(&file.tts_model_dir).map(PathBuf::from));

    Ok(ServerConfig {
        mode,
        port,
        stt_engine,
        stt_model_dir,
        tts_engine,
        tts_model_dir,
    })
}

/// Load the JSON config file: the explicit `--config PATH` when given (must
/// exist and parse), else `<app_data com.aokie.app>/voice-server.json` when
/// present (a malformed auto-loaded file is an error too — serving with half
/// the operator's intent silently dropped is worse than refusing to start).
pub fn load_file_config(config_path: Option<&Path>) -> Result<FileConfig, String> {
    let path: PathBuf = match config_path {
        Some(p) => p.to_path_buf(),
        None => {
            let Some(app_data) = aokie_core::paths::app_data_dir() else {
                return Ok(FileConfig::default());
            };
            let p = app_data.join("voice-server.json");
            if !p.is_file() {
                return Ok(FileConfig::default());
            }
            p
        }
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read config file {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("malformed config file {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Server + engine backends
// ---------------------------------------------------------------------------

/// Loaded STT engine — one variant per `SttEngineKind`, lazily constructed on
/// the first transcription request (mirrors the plugin's TtsBackend pattern).
enum SttBackend {
    Parakeet(ParakeetOnnxRuntime),
    Moonshine(MoonshineTranscribeRuntime),
    Qwen3Asr(Qwen3AsrTranscribeRuntime),
}

/// Loaded TTS engine — lazily constructed on the first speech request.
/// Sherpa holds a small per-bundle runtime cache instead of one runtime:
/// the request's "voice" field selects the bundle per call, and each Piper
/// model is ~60-120MB RAM / ~1-3s to load, so recently used voices stay
/// resident (capacity `SHERPA_BUNDLE_CACHE_CAPACITY`, LRU eviction).
enum TtsBackend {
    Pocket(OnnxTtsRuntime),
    Sherpa(LruCache<SherpaOnnxTtsRuntime>),
}

/// How many sherpa voice bundles stay loaded at once.
pub const SHERPA_BUNDLE_CACHE_CAPACITY: usize = 3;

pub struct VoiceServer {
    config: ServerConfig,
    app_data: PathBuf,
    max_body_bytes: usize,
    stt: Mutex<Option<SttBackend>>,
    tts: Mutex<Option<TtsBackend>>,
    /// Unknown sherpa voice values already logged — one warning per distinct
    /// value, never per request (bounded; see `warn_unknown_voice`).
    warned_voices: Mutex<HashSet<String>>,
}

impl VoiceServer {
    pub fn from_app_data(config: ServerConfig, max_body_bytes: usize) -> Result<Self, String> {
        let app_data = aokie_core::paths::app_data_dir()
            .ok_or_else(|| "no app_data_dir for the voice models".to_string())?;
        Ok(Self::new(config, app_data, max_body_bytes))
    }

    pub fn new(config: ServerConfig, app_data: impl Into<PathBuf>, max_body_bytes: usize) -> Self {
        Self {
            config,
            app_data: app_data.into(),
            max_body_bytes,
            stt: Mutex::new(None),
            tts: Mutex::new(None),
            warned_voices: Mutex::new(HashSet::new()),
        }
    }

    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// The STT model folder: the configured override, else the selected
    /// engine's convention under `<app_data>/models/`.
    fn stt_dir(&self) -> PathBuf {
        self.config.stt_model_dir.clone().unwrap_or_else(|| {
            self.app_data
                .join("models")
                .join(self.config.stt_engine.default_dir_name())
        })
    }

    /// The pocket-tts bundle folder (the sherpa lane resolves per-bundle via
    /// `resolve_sherpa_bundle` instead).
    fn pocket_dir(&self) -> PathBuf {
        self.config
            .tts_model_dir
            .clone()
            .unwrap_or_else(|| self.app_data.join("models").join("pocket_tts_onnx"))
    }

    fn resolve_sherpa_bundle(&self) -> Result<PathBuf, String> {
        sherpa_voice_dir(self.config.tts_model_dir.as_deref(), &self.app_data)
    }

    /// The folder that named sherpa voices resolve against (and the
    /// alphabetical default-bundle scan walks): `<app_data>/models/tts`.
    fn sherpa_scan_root(&self) -> PathBuf {
        self.app_data.join("models").join("tts")
    }

    /// Presence-only readiness for the selected STT engine (no model load —
    /// health stays instant).
    fn stt_files_present(&self) -> bool {
        let dir = self.stt_dir();
        match self.config.stt_engine {
            SttEngineKind::Parakeet => [
                "encoder.int8.onnx",
                "decoder_joint.int8.onnx",
                "tokenizer.model",
            ]
            .iter()
            .all(|name| dir.join(name).is_file()),
            SttEngineKind::Moonshine => {
                dir.join("tokenizer.json").is_file()
                    && detect_quantization(&dir, &MOONSHINE_STEMS, &MOONSHINE_QUANT_ORDER).is_some()
            }
            SttEngineKind::Qwen3Asr => {
                dir.join("tokenizer.json").is_file()
                    && dir.join("embed_tokens.bin").is_file()
                    && detect_quantization(&dir, &QWEN3_ASR_STEMS, &QWEN3_ASR_QUANT_ORDER).is_some()
            }
        }
    }

    /// TTS asset readiness: `Ok(model dir)` when the selected engine can load,
    /// `Err(why)` otherwise. Presence-only — no model load.
    fn tts_assets(&self) -> Result<PathBuf, String> {
        match self.config.tts_engine {
            TtsEngineKind::Pocket => {
                let dir = self.pocket_dir();
                if pocket_files_present(&dir) {
                    Ok(dir)
                } else {
                    Err(format!(
                        "TTS model files are not present under {}",
                        dir.display()
                    ))
                }
            }
            TtsEngineKind::Sherpa => {
                let dir = self.resolve_sherpa_bundle()?;
                sherpa_config_for_dir(&dir)?;
                Ok(dir)
            }
        }
    }

    fn transcribe(&self, samples_16k: &[f32]) -> Result<String, AppError> {
        if !self.stt_files_present() {
            return Err(AppError::new(
                503,
                format!(
                    "STT model files are not present under {} (engine {})",
                    self.stt_dir().display(),
                    self.config.stt_engine.as_str()
                ),
            ));
        }

        let mut guard = self
            .stt
            .lock()
            .map_err(|_| AppError::new(500, "STT engine lock poisoned"))?;
        if guard.is_none() {
            ensure_ort_dylib();
            let started = Instant::now();
            let dir = self.stt_dir();
            let backend = match self.config.stt_engine {
                SttEngineKind::Parakeet => SttBackend::Parakeet(
                    ParakeetOnnxRuntime::load(
                        &dir.join("encoder.int8.onnx"),
                        &dir.join("decoder_joint.int8.onnx"),
                        &dir.join("tokenizer.model"),
                        2,
                    )
                    .map_err(|e| AppError::new(500, format!("STT load failed: {e}")))?,
                ),
                SttEngineKind::Moonshine => {
                    let quant = detect_quantization(&dir, &MOONSHINE_STEMS, &MOONSHINE_QUANT_ORDER)
                        .ok_or_else(|| {
                            AppError::new(
                                500,
                                "STT load failed: no complete moonshine ONNX set on disk",
                            )
                        })?;
                    SttBackend::Moonshine(
                        MoonshineTranscribeRuntime::load(&dir, MoonshineVariant::Tiny, quant)
                            .map_err(|e| AppError::new(500, format!("STT load failed: {e}")))?,
                    )
                }
                SttEngineKind::Qwen3Asr => {
                    let quant = detect_quantization(&dir, &QWEN3_ASR_STEMS, &QWEN3_ASR_QUANT_ORDER)
                        .ok_or_else(|| {
                            AppError::new(
                                500,
                                "STT load failed: no complete qwen3-asr ONNX set on disk",
                            )
                        })?;
                    SttBackend::Qwen3Asr(
                        Qwen3AsrTranscribeRuntime::load(&dir, Qwen3AsrVariant::Size0_6B, quant)
                            .map_err(|e| AppError::new(500, format!("STT load failed: {e}")))?,
                    )
                }
            };
            eprintln!(
                "[{}] STT ({}) loaded in {:?}",
                log_tag(),
                self.config.stt_engine.as_str(),
                started.elapsed()
            );
            *guard = Some(backend);
        }

        let rt = guard
            .as_mut()
            .ok_or_else(|| AppError::new(500, "STT engine unavailable after load"))?;
        let text = match rt {
            SttBackend::Parakeet(rt) => rt.transcribe(samples_16k),
            SttBackend::Moonshine(rt) => rt.transcribe(samples_16k),
            // English hint mirrors aokie-ai's Qwen3AsrSttConfig default —
            // without it short utterances routinely tokenize into Chinese.
            SttBackend::Qwen3Asr(rt) => rt.transcribe(samples_16k, Some("en")),
        }
        .map_err(|e| AppError::new(500, format!("STT transcribe failed: {e}")))?;
        Ok(text.trim().to_string())
    }

    /// Lock the TTS slot, loading the configured engine on first use (shared
    /// by the buffered WAV path and the streaming PCM path).
    fn ensure_tts_backend(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<TtsBackend>>, AppError> {
        let mut guard = self
            .tts
            .lock()
            .map_err(|_| AppError::new(500, "TTS engine lock poisoned"))?;
        if guard.is_none() {
            let started = Instant::now();
            let backend = match self.config.tts_engine {
                TtsEngineKind::Pocket => {
                    let dir = self.pocket_dir();
                    if !pocket_files_present(&dir) {
                        return Err(AppError::new(
                            503,
                            format!("TTS model files are not present under {}", dir.display()),
                        ));
                    }
                    ensure_ort_dylib();
                    TtsBackend::Pocket(
                        OnnxTtsRuntime::open(&dir)
                            .map_err(|e| AppError::new(500, format!("TTS load failed: {e}")))?,
                    )
                }
                TtsEngineKind::Sherpa => {
                    // Dynamic sherpa brings its own ORT DLL — ensure_ort_dylib
                    // is deliberately NOT involved here. Bundles load LAZILY
                    // per requested voice through the LRU cache (each load is
                    // logged there) — nothing to do up front.
                    TtsBackend::Sherpa(LruCache::new(SHERPA_BUNDLE_CACHE_CAPACITY))
                }
            };
            if matches!(backend, TtsBackend::Pocket(_)) {
                eprintln!(
                    "[{}] TTS ({}) loaded in {:?}",
                    log_tag(),
                    self.config.tts_engine.as_str(),
                    started.elapsed()
                );
            }
            *guard = Some(backend);
        }
        Ok(guard)
    }

    fn synthesize_wav(&self, input: &str, voice: &str, speed: f32) -> Result<Vec<u8>, AppError> {
        let mut guard = self.ensure_tts_backend()?;
        let rt = guard
            .as_mut()
            .ok_or_else(|| AppError::new(500, "TTS engine unavailable after load"))?;
        let (pcm, sample_rate) = match rt {
            TtsBackend::Pocket(rt) => {
                let sample_rate = rt.sample_rate();
                let mut f32_samples = Vec::new();
                rt.synthesize_stream(input, voice, |chunk, _rate| {
                    f32_samples.extend_from_slice(chunk);
                    true
                })
                .map_err(|e| AppError::new(500, format!("TTS synthesize failed: {e}")))?;
                (f32_to_i16(&f32_samples), sample_rate)
            }
            TtsBackend::Sherpa(cache) => {
                let (samples, sample_rate) = self.sherpa_synthesize(cache, input, voice)?;
                (f32_to_i16(&samples), sample_rate)
            }
        };
        // Pitch-preserving rate change at the model's native sample rate
        // (a no-op at speed 1.0).
        let pcm = aokie_core::time_stretch::stretch_i16(&pcm, sample_rate, speed);
        write_pcm16_wav(&pcm, sample_rate)
            .map_err(|e| AppError::new(500, format!("WAV encode failed: {e}")))
    }

    /// VOX-402 streaming synthesis: raw s16le mono PCM chunks reach `sink`
    /// as the engine produces them. Pocket streams its decode chunks
    /// natively; sherpa is one-shot, so its (loudness-guarded) buffer is
    /// sliced into ~100ms pieces after synthesis. The sink returning `false`
    /// CANCELS synthesis (client disconnect) — that is `Ok(Cancelled)`, not
    /// an error. `Err` before `sink.begin` ran means no bytes hit the wire
    /// yet (a normal error response is still possible).
    fn synthesize_pcm(
        &self,
        input: &str,
        voice: &str,
        sink: &mut dyn PcmSink,
    ) -> Result<PcmOutcome, AppError> {
        let mut guard = self.ensure_tts_backend()?;
        let rt = guard
            .as_mut()
            .ok_or_else(|| AppError::new(500, "TTS engine unavailable after load"))?;
        match rt {
            TtsBackend::Pocket(rt) => {
                // The engine's native rate is known before synthesis — the
                // head can go out ahead of the first decode chunk.
                let sample_rate = rt.sample_rate();
                if !sink.begin(sample_rate) {
                    return Ok(PcmOutcome::Cancelled);
                }
                let mut cancelled = false;
                let result = rt.synthesize_stream(input, voice, |chunk, _rate| {
                    if sink.pcm(&f32_to_i16(chunk)) {
                        true
                    } else {
                        cancelled = true;
                        false
                    }
                });
                if cancelled {
                    return Ok(PcmOutcome::Cancelled);
                }
                result.map_err(|e| AppError::new(500, format!("TTS synthesize failed: {e}")))?;
                Ok(PcmOutcome::Completed)
            }
            TtsBackend::Sherpa(cache) => {
                let (samples, sample_rate) = self.sherpa_synthesize(cache, input, voice)?;
                let pcm = f32_to_i16(&samples);
                Ok(stream_pcm_slices(&pcm, sample_rate, sink))
            }
        }
    }

    /// The ONE sherpa dispatch point (WAV and PCM both land here): resolve
    /// the request's voice to a bundle + speaker, load-or-reuse the bundle's
    /// runtime through the LRU cache, synthesize, and apply the loudness
    /// guard. An unknown/invalid voice logs once per value and falls back to
    /// the default bundle — a bad voice NEVER fails the span.
    fn sherpa_synthesize(
        &self,
        cache: &mut LruCache<SherpaOnnxTtsRuntime>,
        input: &str,
        voice: &str,
    ) -> Result<(Vec<f32>, u32), AppError> {
        let (dir, speaker) = match resolve_sherpa_voice(voice, &self.sherpa_scan_root()) {
            SherpaVoice::Default { speaker } => (
                self.resolve_sherpa_bundle().map_err(|e| {
                    AppError::new(503, format!("TTS model files are not ready: {e}"))
                })?,
                speaker,
            ),
            SherpaVoice::Bundle { dir, speaker } => (dir, speaker),
            SherpaVoice::Unknown { requested } => {
                self.warn_unknown_voice(&requested);
                (
                    self.resolve_sherpa_bundle().map_err(|e| {
                        AppError::new(503, format!("TTS model files are not ready: {e}"))
                    })?,
                    String::new(),
                )
            }
        };
        // ⚠️ NEVER fs::canonicalize the bundle dir: on Windows it returns an
        // extended-length `\\?\C:\...` path, and sherpa's espeak layer joins
        // `espeak-ng-data/phontab` onto it with a FORWARD slash — which the
        // `\\?\` form does not tolerate. The config then fails validation,
        // SherpaOnnxCreateOfflineTts hands back a NULL engine, and the next
        // synthesize SEGFAULTS the whole process (live 2026-07-17: every
        // synthesis crashed the aokie-tts service until crash-recovery
        // restarted it). Key the cache on a case-folded plain-path string
        // instead — name-form and abs-path-form of the same bundle still
        // share a slot, and the CONFIG always gets a plain path.
        let key = PathBuf::from(dir.to_string_lossy().to_lowercase());
        let rt = cache.get_or_insert_with(&key, || {
            let cfg = sherpa_config_for_dir(&dir)
                .map_err(|e| AppError::new(503, format!("TTS model files are not ready: {e}")))?;
            let started = Instant::now();
            let rt = SherpaOnnxTtsRuntime::load(&cfg)
                .map_err(|e| AppError::new(500, format!("TTS load failed: {e}")))?;
            eprintln!(
                "[{}] sherpa bundle {} loaded in {:?}",
                log_tag(),
                dir.display(),
                started.elapsed()
            );
            Ok(rt)
        })?;
        let mut audio = rt
            .synthesize(input, &speaker)
            .map_err(|e| AppError::new(500, format!("TTS synthesize failed: {e}")))?;
        // Sherpa/Piper voices synthesize HOT — attenuate-only loudness
        // normalization before the i16 conversion so peaks never clip.
        // Applies to EVERY sherpa synthesis regardless of bundle.
        let scale = attenuate_hot_signal(&mut audio.samples);
        if scale < 1.0 {
            eprintln!(
                "[{}] sherpa loudness attenuated x{scale:.3} ({} samples)",
                log_tag(),
                audio.samples.len()
            );
        }
        Ok((audio.samples, audio.sample_rate))
    }

    /// Log an unknown sherpa voice value ONCE (bounded set — a flood of
    /// distinct junk values can't grow memory or spam the log forever).
    fn warn_unknown_voice(&self, requested: &str) {
        let Ok(mut warned) = self.warned_voices.lock() else {
            return;
        };
        if warned.len() < 64 && warned.insert(requested.to_string()) {
            eprintln!(
                "[{}] unknown sherpa voice {requested:?} - using the default bundle (installed voices: {:?})", log_tag(),
                installed_sherpa_bundles(&self.sherpa_scan_root())
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming PCM plumbing (VOX-402)
// ---------------------------------------------------------------------------

/// Receiver of a streamed PCM synthesis. Implementations return `false` from
/// either method to cancel synthesis (the client hung up).
pub trait PcmSink {
    /// Called once, before any audio, with the stream's sample rate — the
    /// engine's NATIVE rate; the server never resamples the PCM route.
    fn begin(&mut self, sample_rate: u32) -> bool;
    /// One chunk of mono s16le PCM.
    fn pcm(&mut self, samples: &[i16]) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmOutcome {
    Completed,
    Cancelled,
}

/// In-memory sink — backs `handle_request`'s buffered fallback and tests.
#[derive(Debug, Default)]
pub struct BufferPcmSink {
    pub sample_rate: Option<u32>,
    pub bytes: Vec<u8>,
}

impl PcmSink for BufferPcmSink {
    fn begin(&mut self, sample_rate: u32) -> bool {
        self.sample_rate = Some(sample_rate);
        true
    }

    fn pcm(&mut self, samples: &[i16]) -> bool {
        self.bytes.extend_from_slice(&pcm_bytes(samples));
        true
    }
}

/// s16le encoding of a PCM chunk (the exact bytes the wire carries).
pub fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// ~100ms of samples — the slice size used to drip a one-shot engine's
/// buffer to the client so a disconnect stops the writes promptly.
pub fn pcm_slice_samples(sample_rate: u32) -> usize {
    (sample_rate / 10).max(1) as usize
}

/// Stream an already-synthesized buffer through `sink` in ~100ms slices
/// (the one-shot-engine half of the PCM route). Pure over the sink —
/// unit-testable without a socket.
pub fn stream_pcm_slices(pcm: &[i16], sample_rate: u32, sink: &mut dyn PcmSink) -> PcmOutcome {
    if !sink.begin(sample_rate) {
        return PcmOutcome::Cancelled;
    }
    for chunk in pcm.chunks(pcm_slice_samples(sample_rate)) {
        if !sink.pcm(chunk) {
            return PcmOutcome::Cancelled;
        }
    }
    PcmOutcome::Completed
}

/// Response head for the streaming PCM route: HTTP/1.0-style EOF-delimited
/// body — deliberately NO Content-Length (the length is unknown until the
/// last chunk); `Connection: close` ends the body. `X-Sample-Rate` names
/// the rate of the raw s16le mono stream.
pub fn pcm_stream_head(sample_rate: u32) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {PCM_CONTENT_TYPE}\r\nX-Sample-Rate: {sample_rate}\r\nConnection: close\r\n\r\n"
    )
}

pub const PCM_CONTENT_TYPE: &str = "audio/pcm";

fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect()
}

/// VOX-401 loudness guard: sherpa/Piper output runs near full scale and can
/// clip after i16 conversion. Attenuate-only normalization — computes peak and
/// RMS over the f32 buffer and scales DOWN by `min(1.0, 0.85/peak, 0.12/rms)`;
/// silence and already-quiet signals pass through untouched (never boosts).
/// Returns the applied scale (1.0 = untouched).
pub fn attenuate_hot_signal(samples: &mut [f32]) -> f32 {
    if samples.is_empty() {
        return 1.0;
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &s in samples.iter() {
        peak = peak.max(s.abs());
        sum_sq += (s as f64) * (s as f64);
    }
    if peak <= 0.0 {
        return 1.0;
    }
    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
    let mut scale = 1.0f32.min(0.85 / peak);
    if rms > 0.0 {
        scale = scale.min(0.12 / rms);
    }
    if scale < 1.0 {
        for s in samples.iter_mut() {
            *s *= scale;
        }
    }
    scale.min(1.0)
}

/// Where a sherpa speech request's "voice" field resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SherpaVoice {
    /// The configured default bundle; `speaker` is "" (bundle default) or a
    /// numeric speaker id string.
    Default { speaker: String },
    /// A specific installed bundle (by folder name under the scan root, or
    /// an absolute path), optionally with a numeric speaker id.
    Bundle { dir: PathBuf, speaker: String },
    /// Nothing matched — the caller falls back to the default bundle with
    /// the default speaker, logging once per distinct value.
    Unknown { requested: String },
}

/// Per-request sherpa voice resolution:
/// - ""            → default bundle, default speaker
/// - "3"           → default bundle, speaker id 3 (kokoro multi-speaker)
/// - "jenny"       → installed bundle folder `<scan_root>/jenny`
/// - "C:\...\dir"  → absolute path to a valid bundle dir
/// - "jenny:2"     → that bundle + numeric speaker id
/// - anything else → `Unknown` (caller falls back, never fails the span)
///
/// A "valid bundle" is a directory holding tokens.txt + exactly one .onnx
/// (`is_sherpa_bundle`). Pure over the filesystem — unit-tested with fake
/// bundle dirs.
pub fn resolve_sherpa_voice(voice: &str, scan_root: &Path) -> SherpaVoice {
    let v = voice.trim();
    if v.is_empty() {
        return SherpaVoice::Default {
            speaker: String::new(),
        };
    }
    if v.parse::<i32>().is_ok() {
        return SherpaVoice::Default {
            speaker: v.to_string(),
        };
    }
    if let Some(dir) = bundle_for_token(v, scan_root) {
        return SherpaVoice::Bundle {
            dir,
            speaker: String::new(),
        };
    }
    // Combined "bundlename:N" (also works for "C:\abs\bundle:N" — the split
    // is at the LAST colon, so a drive letter never confuses it).
    if let Some((prefix, suffix)) = v.rsplit_once(':') {
        if suffix.parse::<i32>().is_ok() {
            if let Some(dir) = bundle_for_token(prefix.trim(), scan_root) {
                return SherpaVoice::Bundle {
                    dir,
                    speaker: suffix.to_string(),
                };
            }
        }
    }
    SherpaVoice::Unknown {
        requested: v.to_string(),
    }
}

/// Resolve one bundle token: an absolute path is taken as-is (when valid);
/// a plain folder NAME (no separators, no traversal) resolves under the
/// scan root. Anything else is no match.
fn bundle_for_token(token: &str, scan_root: &Path) -> Option<PathBuf> {
    if token.is_empty() {
        return None;
    }
    let path = Path::new(token);
    if path.is_absolute() {
        return is_sherpa_bundle(path).then(|| path.to_path_buf());
    }
    if token.contains(['/', '\\']) || token == "." || token == ".." {
        return None;
    }
    let candidate = scan_root.join(token);
    is_sherpa_bundle(&candidate).then_some(candidate)
}

/// The bundle test: a directory with tokens.txt + EXACTLY one .onnx (the
/// same shape `sherpa_config_for_dir` will accept).
fn is_sherpa_bundle(dir: &Path) -> bool {
    if !dir.is_dir() || !dir.join("tokens.txt").is_file() {
        return false;
    }
    let onnx_count = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let p = e.path();
                    p.is_file() && p.extension().is_some_and(|x| x == "onnx")
                })
                .count()
        })
        .unwrap_or(0);
    onnx_count == 1
}

/// Installed bundle folder names under the scan root, sorted — the
/// discoverable "voices" surfaced by /v1/models in sherpa mode.
pub fn installed_sherpa_bundles(scan_root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(scan_root)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| is_sherpa_bundle(p))
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Tiny path-keyed LRU (front = most recently used). Capacity is small (3
/// sherpa bundles) so a Vec scan beats any linked-map machinery.
pub struct LruCache<V> {
    capacity: usize,
    entries: Vec<(PathBuf, V)>,
}

impl<V> LruCache<V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Vec::new(),
        }
    }

    /// Fetch the value for `key`, loading it with `load` on a miss (evicting
    /// the least-recently-used entry when at capacity). A failed load caches
    /// NOTHING. Either way the touched entry becomes most-recently-used.
    pub fn get_or_insert_with<E>(
        &mut self,
        key: &Path,
        load: impl FnOnce() -> Result<V, E>,
    ) -> Result<&mut V, E> {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == key) {
            let entry = self.entries.remove(pos);
            self.entries.insert(0, entry);
        } else {
            let value = load()?;
            if self.entries.len() >= self.capacity {
                self.entries.pop();
            }
            self.entries.insert(0, (key.to_path_buf(), value));
        }
        Ok(&mut self.entries[0].1)
    }

    /// Keys in MRU→LRU order (tests assert the eviction order through this).
    pub fn keys(&self) -> Vec<&Path> {
        self.entries.iter().map(|(k, _)| k.as_path()).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Pocket-TTS bundle presence — unchanged from the pre-split `ModelPaths`.
fn pocket_files_present(dir: &Path) -> bool {
    let bundle_path = dir.join("bundle.json");
    if !bundle_path.is_file() {
        return false;
    }

    let tokenizer_ok = std::fs::read_to_string(&bundle_path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value
                .get("tokenizer_file")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .map(|name| dir.join(name).is_file())
        .unwrap_or_else(|| dir.join("tokenizer.model").is_file());
    if !tokenizer_ok {
        return false;
    }

    [
        "text_conditioner",
        "flow_lm_main",
        "flow_lm_flow",
        "mimi_decoder",
        "mimi_encoder",
    ]
    .iter()
    .all(|stem| {
        dir.join(format!("{stem}_int8.onnx")).is_file()
            || dir.join(format!("{stem}.onnx")).is_file()
    })
}

// The transcribe-rs engines publish precision variants as file-name suffixes
// (`{stem}.int8.onnx` etc; FP32 = no suffix). Detection order mirrors the
// aokie-ai config defaults: moonshine ships fp32-first, qwen3-asr int4-first
// (the only published quantisation).
const MOONSHINE_STEMS: [&str; 2] = ["encoder_model", "decoder_model_merged"];
const MOONSHINE_QUANT_ORDER: [Quantization; 4] = [
    Quantization::FP32,
    Quantization::Int8,
    Quantization::FP16,
    Quantization::Int4,
];
const QWEN3_ASR_STEMS: [&str; 3] = ["encoder", "decoder_init", "decoder_step"];
const QWEN3_ASR_QUANT_ORDER: [Quantization; 4] = [
    Quantization::Int4,
    Quantization::FP32,
    Quantization::FP16,
    Quantization::Int8,
];

fn quantized_model_path(dir: &Path, stem: &str, quant: &Quantization) -> PathBuf {
    match quant {
        Quantization::FP32 => dir.join(format!("{stem}.onnx")),
        Quantization::FP16 => dir.join(format!("{stem}.fp16.onnx")),
        Quantization::Int8 => dir.join(format!("{stem}.int8.onnx")),
        Quantization::Int4 => dir.join(format!("{stem}.int4.onnx")),
    }
}

/// First quantization level (in `order`) for which EVERY stem's file exists.
fn detect_quantization(dir: &Path, stems: &[&str], order: &[Quantization]) -> Option<Quantization> {
    order
        .iter()
        .find(|q| {
            stems
                .iter()
                .all(|stem| quantized_model_path(dir, stem, q).is_file())
        })
        .cloned()
}

// ---------------------------------------------------------------------------
// Sherpa voice-bundle resolution — replicates the plugin's rules
// (crates/aokie-plugin/src/voice.rs) so both halves of the stack agree on
// what a usable bundle is. Copied, not imported: the server must not depend
// on the plugin crate.
// ---------------------------------------------------------------------------

/// Resolve the sherpa voice-bundle directory: the configured override when
/// set, else the first usable bundle under `<app_data>/models/tts`
/// (alphabetical — deterministic across restarts). A "usable bundle" holds a
/// model .onnx and tokens.txt, per the sherpa-onnx release-tarball layout.
fn sherpa_voice_dir(override_dir: Option<&Path>, app_data: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = override_dir {
        if !dir.is_dir() {
            return Err(format!(
                "tts model dir {:?} is not a folder - point it at a sherpa voice bundle (model .onnx + tokens.txt)",
                dir.display().to_string()
            ));
        }
        return Ok(dir.to_path_buf());
    }
    let root = app_data.join("models").join("tts");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
        .map_err(|_| {
            format!(
                "no sherpa voice found: set --tts-model-dir to a voice bundle folder (model .onnx + tokens.txt), or place one under {}",
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
            "no sherpa voice found under {} - set --tts-model-dir to a voice bundle folder (model .onnx + tokens.txt)",
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

/// Compose the sherpa engine config from a voice-bundle folder, following the
/// sherpa-onnx release-tarball layout: `<name>.onnx` + `tokens.txt`, with
/// `espeak-ng-data/` (Piper) either inside the bundle or shared in the parent
/// folder, optional `lexicon.txt`/`dict/`, and `voices.bin` marking a Kokoro
/// bundle. Kept pure over the folder contents so it's unit-testable.
fn sherpa_config_for_dir(dir: &Path) -> Result<SherpaTtsConfig, String> {
    let mut onnx: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read tts model dir {}: {e}", dir.display()))?
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
        // Require the phontab file, not just the folder: sherpa validates it
        // at engine-create time and hands back a NULL engine when it's
        // missing — which the next synthesize turns into a process SEGFAULT
        // (sherpa-rs never surfaces the null). Refusing here keeps it a
        // clean 503 instead.
        if data_dir.is_dir() && data_dir.join("phontab").is_file() {
            cfg.data_dir = data_dir.to_string_lossy().to_string();
            break;
        }
    }
    let lexicon = dir.join("lexicon.txt");
    if lexicon.is_file() {
        cfg.lexicon = lexicon.to_string_lossy().to_string();
    }
    if cfg.engine == "vits" && cfg.data_dir.is_empty() && cfg.lexicon.is_empty() {
        return Err(format!(
            "no usable espeak-ng-data (with phontab) or lexicon.txt in {} or its parent - a Piper/VITS bundle needs one",
            dir.display()
        ));
    }
    let dict = dir.join("dict");
    if dict.is_dir() {
        cfg.dict_dir = dict.to_string_lossy().to_string();
    }
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn wav(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "audio/wav",
            body,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct AppError {
    status: u16,
    message: String,
}

impl AppError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn response(self) -> HttpResponse {
        error_response(self.status, self.message)
    }
}

/// Routing outcome (VOX-402): most requests buffer a whole `HttpResponse`;
/// a PCM speech request must instead be EXECUTED against the live socket
/// (headers at the first engine chunk, EOF-delimited body) — routing hands
/// the validated plan back so the connection handler owns the streaming.
pub enum Routed {
    Buffered(HttpResponse),
    PcmStream(PcmStreamRequest),
}

/// A validated `/v1/audio/speech` request bound for the streaming PCM path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmStreamRequest {
    /// Speech-normalized input text (same rewrite as the WAV path).
    pub input: String,
    pub voice: String,
}

pub fn route_request(
    server: &VoiceServer,
    method: &str,
    url: &str,
    headers: &[Header],
    body: &[u8],
) -> Routed {
    if body.len() > server.max_body_bytes() {
        return Routed::Buffered(error_response(413, "request body exceeds 32 MiB limit"));
    }

    let path = url.split('?').next().unwrap_or(url);
    match (method, path) {
        ("GET", "/health") => Routed::Buffered(health_response(server)),
        ("GET", "/v1/models") => Routed::Buffered(models_response(server)),
        ("POST", "/v1/audio/transcriptions") => {
            // Mode gating (VOX-401): a tts-only instance must refuse the STT
            // route with a clear pointer at the right instance, not a load
            // failure.
            if !server.config.mode.stt_enabled() {
                return Routed::Buffered(error_response(
                    404,
                    "this voice server runs in tts mode - transcriptions are not served here (start with --mode stt or --mode both)",
                ));
            }
            Routed::Buffered(
                handle_transcriptions(server, headers, body).unwrap_or_else(AppError::response),
            )
        }
        ("POST", "/v1/audio/speech") => {
            if !server.config.mode.tts_enabled() {
                return Routed::Buffered(error_response(
                    404,
                    "this voice server runs in stt mode - speech synthesis is not served here (start with --mode tts or --mode both)",
                ));
            }
            handle_speech(server, headers, body)
                .unwrap_or_else(|err| Routed::Buffered(err.response()))
        }
        _ => Routed::Buffered(error_response(404, "unknown route")),
    }
}

/// Buffered request entry point — the historical surface, kept for
/// in-process callers and tests. A PCM speech request is served buffered
/// here (whole body in memory, no X-Sample-Rate header); the network server
/// (`run_http`) streams it instead.
pub fn handle_request(
    server: &VoiceServer,
    method: &str,
    url: &str,
    headers: &[Header],
    body: &[u8],
) -> HttpResponse {
    match route_request(server, method, url, headers, body) {
        Routed::Buffered(response) => response,
        Routed::PcmStream(req) => {
            let mut sink = BufferPcmSink::default();
            match server.synthesize_pcm(&req.input, &req.voice, &mut sink) {
                Ok(_) => HttpResponse {
                    status: 200,
                    content_type: PCM_CONTENT_TYPE,
                    body: sink.bytes,
                },
                Err(err) => err.response(),
            }
        }
    }
}

fn health_response(server: &VoiceServer) -> HttpResponse {
    // Truthful readiness (audit AOK-VOICE-SRV-001), now per MODE: "ok" only
    // when every ENABLED lane has its model assets on disk — an stt-only
    // instance is green without any TTS voice, and vice versa. The legacy
    // top-level "stt"/"tts" booleans are kept exactly as before (a disabled
    // lane truthfully reads false — this instance cannot do that job). Build
    // provenance (CROSS-OBS-001) says exactly which build answered.
    let mode = server.config.mode;
    let stt_ready = mode.stt_enabled() && server.stt_files_present();
    let tts_assets = if mode.tts_enabled() {
        Some(server.tts_assets())
    } else {
        None
    };
    let tts_ready = matches!(tts_assets, Some(Ok(_)));
    let healthy = (!mode.stt_enabled() || stt_ready) && (!mode.tts_enabled() || tts_ready);

    let stt_lane = if mode.stt_enabled() {
        json!({
            "enabled": true,
            "engine": server.config.stt_engine.as_str(),
            "modelDir": server.stt_dir().display().to_string(),
            "ready": stt_ready,
        })
    } else {
        json!({ "enabled": false })
    };
    let tts_lane = match tts_assets {
        None => json!({ "enabled": false }),
        Some(Ok(dir)) => json!({
            "enabled": true,
            "engine": server.config.tts_engine.as_str(),
            "modelDir": dir.display().to_string(),
            "ready": true,
        }),
        Some(Err(why)) => json!({
            "enabled": true,
            "engine": server.config.tts_engine.as_str(),
            "ready": false,
            "error": why,
        }),
    };

    HttpResponse::json(
        200,
        json!({
            "status": if healthy { "ok" } else { "degraded" },
            "mode": mode.as_str(),
            "stt": stt_ready,
            "tts": tts_ready,
            "lanes": {
                "stt": stt_lane,
                "tts": tts_lane,
            },
            "build": {
                "version": env!("CARGO_PKG_VERSION"),
                "ref": env!("AOKIE_BUILD_REF"),
            },
        }),
    )
}

fn models_response(server: &VoiceServer) -> HttpResponse {
    // Only the ACTIVE mode's engines are reported, and the ids reflect the
    // actually-selected engines (VOX-401) — not a hardcoded pair.
    let mut data: Vec<serde_json::Value> = Vec::new();
    if server.config.mode.stt_enabled() {
        data.push(json!({
            "id": server.config.stt_engine.model_id(),
            "object": "model",
        }));
    }
    if server.config.mode.tts_enabled() {
        match server.config.tts_engine {
            TtsEngineKind::Pocket => data.push(json!({
                "id": POCKET_MODEL_ID,
                "object": "model",
            })),
            TtsEngineKind::Sherpa => {
                let mut entry = json!({
                    "id": "sherpa",
                    "object": "model",
                });
                if let Ok(dir) = server.resolve_sherpa_bundle() {
                    if let Some(name) = dir.file_name() {
                        entry["bundle"] = json!(name.to_string_lossy());
                    }
                }
                // Voice discovery: the installed bundle folder names, sorted
                // — each is a valid per-request "voice" value.
                entry["voices"] = json!(installed_sherpa_bundles(&server.sherpa_scan_root()));
                data.push(entry);
            }
        }
    }
    HttpResponse::json(
        200,
        json!({
            "object": "list",
            "data": data,
        }),
    )
}

/// Transcription response shapes (OpenAI `response_format`): `json` (the
/// default, `{"text": ...}`) or `text` (bare text/plain body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionFormat {
    Json,
    Text,
}

pub fn parse_transcription_format(value: Option<&str>) -> Result<TranscriptionFormat, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(TranscriptionFormat::Json),
        Some(v) if v.eq_ignore_ascii_case("json") => Ok(TranscriptionFormat::Json),
        Some(v) if v.eq_ignore_ascii_case("text") => Ok(TranscriptionFormat::Text),
        Some(other) => Err(format!(
            "only response_format \"json\" or \"text\" is supported, got {other:?}"
        )),
    }
}

fn handle_transcriptions(
    server: &VoiceServer,
    headers: &[Header],
    body: &[u8],
) -> Result<HttpResponse, AppError> {
    let content_type = header_value(headers, "content-type").unwrap_or("");
    let (wav_bytes, format_field) = if content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        let parts = parse_multipart(body, content_type)?;
        let file = parts
            .file
            .ok_or_else(|| AppError::new(400, "multipart request has no file part"))?;
        (file, parts.response_format)
    } else {
        if !content_type.is_empty()
            && !content_type
                .to_ascii_lowercase()
                .starts_with("application/json")
        {
            return Err(AppError::new(
                400,
                "transcriptions require application/json or multipart/form-data",
            ));
        }
        let request: TranscriptionJson = serde_json::from_slice(body)
            .map_err(|e| AppError::new(400, format!("malformed JSON: {e}")))?;
        let encoded = request
            .audio
            .as_deref()
            .or(request.file.as_deref())
            .ok_or_else(|| AppError::new(400, "missing audio or file field"))?;
        let decoded = decode_audio_field(encoded)
            .map_err(|e| AppError::new(400, format!("invalid audio field: {e}")))?;
        (decoded, request.response_format)
    };
    // Validated BEFORE inference — a bad format never burns a transcription.
    let format =
        parse_transcription_format(format_field.as_deref()).map_err(|e| AppError::new(400, e))?;

    let samples = decode_wav_to_f32_16k(&wav_bytes)
        .map_err(|e| AppError::new(400, format!("invalid WAV: {e}")))?;
    let text = server.transcribe(&samples)?;
    Ok(match format {
        TranscriptionFormat::Json => HttpResponse::json(200, json!({ "text": text })),
        TranscriptionFormat::Text => HttpResponse {
            status: 200,
            content_type: "text/plain; charset=utf-8",
            body: text.into_bytes(),
        },
    })
}

/// The two speech body formats (OpenAI `response_format`). `Wav` buffers the
/// whole utterance; `Pcm` streams raw s16le mono PCM as synthesis produces
/// it (VOX-402 — the latency-critical route).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechFormat {
    Wav,
    Pcm,
}

pub fn parse_speech_format(value: Option<&str>) -> Result<SpeechFormat, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(SpeechFormat::Wav),
        Some(v) if v.eq_ignore_ascii_case("wav") => Ok(SpeechFormat::Wav),
        Some(v) if v.eq_ignore_ascii_case("pcm") => Ok(SpeechFormat::Pcm),
        Some(other) => Err(format!(
            "only response_format \"wav\" or \"pcm\" is supported, got {other:?}"
        )),
    }
}

fn handle_speech(
    server: &VoiceServer,
    headers: &[Header],
    body: &[u8],
) -> Result<Routed, AppError> {
    let content_type = header_value(headers, "content-type").unwrap_or("");
    if !content_type.is_empty()
        && !content_type
            .to_ascii_lowercase()
            .starts_with("application/json")
    {
        return Err(AppError::new(400, "speech requires application/json"));
    }

    let request: SpeechJson = serde_json::from_slice(body)
        .map_err(|e| AppError::new(400, format!("malformed JSON: {e}")))?;
    let input = request
        .input
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::new(400, "missing input field"))?;
    let format = parse_speech_format(request.response_format.as_deref())
        .map_err(|e| AppError::new(400, e))?;

    // OpenAI-compatible `speed`: a speaking-rate multiplier applied as a
    // pitch-preserving WSOLA time stretch on the synthesized waveform
    // (neither pocket-tts nor the sherpa wire here expose a per-request
    // tempo). Validated, not clamped — a wildly out-of-band request is a
    // caller bug worth surfacing.
    let speed = match request.speed {
        None => 1.0f32,
        Some(s)
            if s.is_finite()
                && (aokie_core::time_stretch::MIN_RATE..=aokie_core::time_stretch::MAX_RATE)
                    .contains(&(s as f32)) =>
        {
            s as f32
        }
        Some(_) => {
            return Err(AppError::new(
                400,
                format!(
                    "speed must be between {} and {}",
                    aokie_core::time_stretch::MIN_RATE,
                    aokie_core::time_stretch::MAX_RATE
                ),
            ))
        }
    };
    // The PCM route streams chunks the moment the engine makes them; the
    // pitch-preserving WSOLA rate change needs the WHOLE utterance, so the
    // two are mutually exclusive by construction.
    if format == SpeechFormat::Pcm && speed != 1.0 {
        return Err(AppError::new(
            400,
            "speed is not supported with response_format \"pcm\" - the rate change needs the whole utterance; use response_format \"wav\" for speed",
        ));
    }

    let voice = request.voice.as_deref().unwrap_or("");
    // Speech-normalize ("10 a.m.," -> "10 AM,") so every consumer of this
    // service gets stutter-free synthesis, same rewrite as the plugin's TTS.
    let input = aokie_core::speech::normalize_speech_text(input);
    match format {
        SpeechFormat::Wav => Ok(Routed::Buffered(HttpResponse::wav(
            server.synthesize_wav(&input, voice, speed)?,
        ))),
        SpeechFormat::Pcm => Ok(Routed::PcmStream(PcmStreamRequest {
            input,
            voice: voice.to_string(),
        })),
    }
}

#[derive(Debug, Deserialize)]
struct TranscriptionJson {
    audio: Option<String>,
    file: Option<String>,
    /// OpenAI-spec tolerance: accepted and ignored (this server has exactly
    /// one loaded STT engine).
    #[allow(dead_code)]
    model: Option<String>,
    /// "json" (default) or "text".
    response_format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpeechJson {
    input: Option<String>,
    voice: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
    response_format: Option<String>,
    /// OpenAI-compatible speaking-rate multiplier (1.0 = normal); applied as
    /// a pitch-preserving time stretch. Bounds: aokie_core::time_stretch.
    speed: Option<f64>,
}

fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

pub fn decode_audio_field(value: &str) -> Result<Vec<u8>, String> {
    let trimmed = value.trim();
    let payload = if trimmed
        .get(..5)
        .map(|prefix| prefix.eq_ignore_ascii_case("data:"))
        .unwrap_or(false)
    {
        let comma = trimmed
            .find(',')
            .ok_or_else(|| "data URL has no comma separator".to_string())?;
        let metadata = &trimmed[..comma];
        if !metadata.to_ascii_lowercase().contains(";base64") {
            return Err("data URL is not base64 encoded".to_string());
        }
        &trimmed[comma + 1..]
    } else {
        trimmed
    };
    decode_base64(payload)
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    let mut sextets: Vec<Option<u8>> = Vec::new();
    let mut saw_padding = false;
    for b in input.bytes().filter(|b| !b.is_ascii_whitespace()) {
        match b {
            b'=' => {
                saw_padding = true;
                sextets.push(None);
            }
            _ if saw_padding => return Err("base64 data after padding".to_string()),
            _ => sextets.push(Some(base64_value(b)?)),
        }
    }

    match sextets.len() % 4 {
        0 => {}
        2 => {
            sextets.push(None);
            sextets.push(None);
        }
        3 => sextets.push(None),
        _ => return Err("invalid base64 length".to_string()),
    }

    let mut out = Vec::with_capacity(sextets.len() / 4 * 3);
    for chunk in sextets.chunks_exact(4) {
        let a = chunk[0].ok_or_else(|| "invalid base64 padding".to_string())?;
        let b = chunk[1].ok_or_else(|| "invalid base64 padding".to_string())?;
        out.push((a << 2) | (b >> 4));
        match (chunk[2], chunk[3]) {
            (Some(c), Some(d)) => {
                out.push(((b & 0x0f) << 4) | (c >> 2));
                out.push(((c & 0x03) << 6) | d);
            }
            (Some(c), None) => {
                out.push(((b & 0x0f) << 4) | (c >> 2));
            }
            (None, None) => {}
            (None, Some(_)) => return Err("invalid base64 padding".to_string()),
        }
    }
    Ok(out)
}

fn base64_value(b: u8) -> Result<u8, String> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a' + 26),
        b'0'..=b'9' => Ok(b - b'0' + 52),
        b'+' | b'-' => Ok(62),
        b'/' | b'_' => Ok(63),
        _ => Err(format!("invalid base64 byte 0x{b:02x}")),
    }
}

pub fn decode_wav_to_f32_16k(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let cursor = Cursor::new(bytes);
    let mut reader = hound::WavReader::new(cursor).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(format!(
            "expected 16-bit PCM WAV, got {:?} {} bits",
            spec.sample_format, spec.bits_per_sample
        ));
    }
    if spec.channels != 1 && spec.channels != 2 {
        return Err(format!(
            "expected mono or stereo WAV, got {} channels",
            spec.channels
        ));
    }
    if spec.sample_rate == 0 {
        return Err("sample rate must be non-zero".to_string());
    }

    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let channels = spec.channels as usize;
    if samples.len() % channels != 0 {
        return Err("WAV sample count is not frame-aligned".to_string());
    }

    let mono = if channels == 1 {
        samples
            .iter()
            .map(|&s| s as f32 / 32768.0)
            .collect::<Vec<_>>()
    } else {
        samples
            .chunks_exact(2)
            .map(|frame| ((frame[0] as f32) + (frame[1] as f32)) / (2.0 * 32768.0))
            .collect()
    };
    Ok(resample_linear(&mono, spec.sample_rate, 16_000))
}

pub fn write_pcm16_wav(samples: &[i16], sample_rate: u32) -> Result<Vec<u8>, String> {
    if sample_rate == 0 {
        return Err("sample rate must be non-zero".to_string());
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec).map_err(|e| e.to_string())?;
        for &sample in samples {
            writer.write_sample(sample).map_err(|e| e.to_string())?;
        }
        writer.finalize().map_err(|e| e.to_string())?;
    }
    Ok(cursor.into_inner())
}

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

/// The multipart fields the transcription route consumes. Unknown parts
/// (e.g. `model`) are tolerated and ignored, per the OpenAI surface.
#[derive(Debug, Default)]
struct MultipartParts {
    file: Option<Vec<u8>>,
    response_format: Option<String>,
}

fn parse_multipart(body: &[u8], content_type: &str) -> Result<MultipartParts, AppError> {
    let boundary = parse_boundary(content_type)
        .ok_or_else(|| AppError::new(400, "multipart request is missing boundary"))?;
    let marker = format!("--{boundary}").into_bytes();
    let mut cursor = 0usize;
    let mut parts = MultipartParts::default();

    while let Some(marker_start) = find_subslice(&body[cursor..], &marker).map(|i| cursor + i) {
        let mut part_start = marker_start + marker.len();
        if body.get(part_start..part_start + 2) == Some(b"--") {
            break;
        }
        if body.get(part_start..part_start + 2) != Some(b"\r\n") {
            return Err(AppError::new(400, "malformed multipart boundary"));
        }
        part_start += 2;

        let header_end_rel = find_subslice(&body[part_start..], b"\r\n\r\n")
            .ok_or_else(|| AppError::new(400, "multipart part has no header terminator"))?;
        let header_end = part_start + header_end_rel;
        let data_start = header_end + 4;
        let next_marker = find_subslice(&body[data_start..], &marker)
            .map(|i| data_start + i)
            .ok_or_else(|| AppError::new(400, "multipart part has no closing boundary"))?;
        let mut data_end = next_marker;
        if data_end >= 2 && body.get(data_end - 2..data_end) == Some(b"\r\n") {
            data_end -= 2;
        }

        let headers = std::str::from_utf8(&body[part_start..header_end])
            .map_err(|_| AppError::new(400, "multipart headers are not UTF-8"))?;
        let lower_headers = headers.to_ascii_lowercase();
        if lower_headers.contains("content-disposition:") {
            if lower_headers.contains("name=\"response_format\"") {
                if parts.response_format.is_none() {
                    let value = std::str::from_utf8(&body[data_start..data_end]).map_err(|_| {
                        AppError::new(400, "multipart response_format is not UTF-8")
                    })?;
                    parts.response_format = Some(value.trim().to_string());
                }
            } else if lower_headers.contains("name=\"file\"") || lower_headers.contains("filename=")
            {
                if parts.file.is_none() {
                    parts.file = Some(body[data_start..data_end].to_vec());
                }
            }
        }

        cursor = next_marker;
    }

    Ok(parts)
}

#[cfg(test)]
fn parse_multipart_file(body: &[u8], content_type: &str) -> Result<Vec<u8>, AppError> {
    parse_multipart(body, content_type)?
        .file
        .ok_or_else(|| AppError::new(400, "multipart request has no file part"))
}

fn parse_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        Some(value.trim().trim_matches('"').to_string())
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn error_response(status: u16, message: impl Into<String>) -> HttpResponse {
    HttpResponse::json(
        status,
        json!({
            "error": {
                "message": message.into(),
                "type": "invalid_request_error"
            }
        }),
    )
}

fn ensure_ort_dylib() {
    if std::env::var_os("ORT_DYLIB_PATH").is_some() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // ⚠️ ORDER MATTERS: the VERSIONED name must win. The plugin's
            // sherpa-onnx TTS engine ships its own `onnxruntime.dll` (1.17.1)
            // into this same directory; resolving the unversioned name first
            // would silently load that older ORT and break Parakeet/pocket.
            for name in ["onnxruntime_1.25.0.dll", "onnxruntime.dll"] {
                let dll = dir.join(name);
                if dll.exists() {
                    std::env::set_var("ORT_DYLIB_PATH", &dll);
                    eprintln!("[{}] ORT_DYLIB_PATH -> {}", log_tag(), dll.display());
                    return;
                }
            }
        }
    }
}

/// Mode-aware log tag (user report 2026-07-17: the "Aokie Speech to Text"
/// service's log lines all read `[aokie-voice-server]` — the BINARY's name,
/// not the service identity). Set once at startup from the resolved mode;
/// callers before init (or tests) get the binary name.
static LOG_TAG: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

fn log_tag() -> &'static str {
    LOG_TAG.get().copied().unwrap_or("aokie-voice-server")
}

fn init_log_tag(mode: ServerMode) {
    let tag = match (mode.stt_enabled(), mode.tts_enabled()) {
        (true, false) => "aokie-stt",
        (false, true) => "aokie-tts",
        _ => "aokie-voice-server",
    };
    let _ = LOG_TAG.set(tag);
}

pub fn run_from_env() -> Result<(), String> {
    let cli = parse_cli_args(std::env::args())?;
    let env = EnvConfig::from_process_env();
    let file = load_file_config(cli.config_path.as_deref())?;
    let config = merge_config(&cli, &env, &file)?;
    init_log_tag(config.mode);
    eprintln!(
        "[{}] mode {} (stt: {}, tts: {})",
        log_tag(),
        config.mode.as_str(),
        if config.mode.stt_enabled() {
            config.stt_engine.as_str()
        } else {
            "off"
        },
        if config.mode.tts_enabled() {
            config.tts_engine.as_str()
        } else {
            "off"
        },
    );
    let port = config.port;
    let server = VoiceServer::from_app_data(config, MAX_BODY_BYTES)?;
    run_http(server, port)
}

/// How many requests may run at once (audit AK-007). Inference already
/// serialises per engine behind its Mutex; this bounds the QUEUE of callers
/// waiting on those locks so a burst gets a fast, retryable 503 instead of a
/// pile of stuck sockets. `/health` bypasses the cap — readiness must answer
/// while inference is busy.
const MAX_INFLIGHT: usize = 4;

pub fn run_http(server: VoiceServer, port: u16) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("bind 127.0.0.1:{port}: {e}"))?;
    eprintln!("[{}] listening on http://127.0.0.1:{port}", log_tag());

    let server = Arc::new(server);
    let inflight = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("[{}] accept failed: {e}", log_tag());
                continue;
            }
        };

        // Thread-per-connection (audit AK-007): a multi-second STT/TTS job
        // must not freeze the accept loop — /health stays responsive during
        // inference, and each connection keeps its own read timeout.
        let server = Arc::clone(&server);
        let inflight = Arc::clone(&inflight);
        std::thread::spawn(move || {
            fn respond(stream: &mut TcpStream, response: HttpResponse) {
                if let Err(e) = write_http_response(stream, response) {
                    eprintln!("[{}] respond failed: {e}", log_tag());
                }
            }

            let request = match read_http_request(&mut stream, server.max_body_bytes()) {
                Ok(request) => request,
                Err(err) => {
                    respond(&mut stream, err.response());
                    return;
                }
            };

            // This is a machine-local service for native callers; a
            // browser always sends Origin on POST, so its presence
            // means a web page is probing localhost (DNS-rebinding /
            // drive-by) — refuse outright (audit AK-007).
            if header_value(&request.headers, "origin").is_some() {
                respond(
                    &mut stream,
                    AppError::new(403, "browser origins are not served").response(),
                );
                return;
            }
            if request.url == "/health" {
                let response = handle_request(
                    &server,
                    &request.method,
                    &request.url,
                    &request.headers,
                    &request.body,
                );
                respond(&mut stream, response);
                return;
            }
            if inflight.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
                inflight.fetch_sub(1, Ordering::SeqCst);
                respond(
                    &mut stream,
                    AppError::new(503, "voice server is at capacity — retry shortly").response(),
                );
                return;
            }
            match route_request(
                &server,
                &request.method,
                &request.url,
                &request.headers,
                &request.body,
            ) {
                Routed::Buffered(response) => {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    respond(&mut stream, response);
                }
                Routed::PcmStream(req) => {
                    // Streaming holds the in-flight slot for the whole
                    // synthesis — the stream IS the inference.
                    stream_pcm_to_client(&server, &mut stream, &req);
                    inflight.fetch_sub(1, Ordering::SeqCst);
                }
            }
        });
    }
    Ok(())
}

/// Execute a validated PCM speech request against the live socket. The 200
/// head (with X-Sample-Rate, no Content-Length) goes out at the engine's
/// FIRST chunk boundary; a client disconnect (write error) cancels synthesis
/// through the sink and is logged once. An engine error BEFORE the head was
/// written still produces a normal JSON error response.
fn stream_pcm_to_client(server: &VoiceServer, stream: &mut TcpStream, req: &PcmStreamRequest) {
    // Latency route: don't let Nagle batch the first decode chunk.
    let _ = stream.set_nodelay(true);
    let (result, begun) = {
        let mut sink = TcpPcmSink {
            stream,
            begun: false,
            disconnected: false,
        };
        let result = server.synthesize_pcm(&req.input, &req.voice, &mut sink);
        (result, sink.begun)
    };
    match result {
        Ok(PcmOutcome::Completed) => {
            let _ = stream.flush();
        }
        // The sink already logged the disconnect once.
        Ok(PcmOutcome::Cancelled) => {}
        Err(err) => {
            if begun {
                // The 200 head is on the wire — nothing left but an early
                // close; the client sees a truncated stream.
                eprintln!(
                    "[{}] pcm stream failed mid-flight: {}",
                    log_tag(),
                    err.message
                );
            } else if let Err(e) = write_http_response(stream, err.response()) {
                eprintln!("[{}] respond failed: {e}", log_tag());
            }
        }
    }
}

/// PcmSink over the client socket. Any write error marks the client gone
/// (logged once) and cancels synthesis by returning false.
struct TcpPcmSink<'a> {
    stream: &'a mut TcpStream,
    begun: bool,
    disconnected: bool,
}

impl TcpPcmSink<'_> {
    fn note_disconnect(&mut self, err: std::io::Error) {
        if !self.disconnected {
            self.disconnected = true;
            eprintln!(
                "[{}] pcm client disconnected mid-stream - synthesis cancelled: {err}",
                log_tag()
            );
        }
    }
}

impl PcmSink for TcpPcmSink<'_> {
    fn begin(&mut self, sample_rate: u32) -> bool {
        self.begun = true;
        match self
            .stream
            .write_all(pcm_stream_head(sample_rate).as_bytes())
        {
            Ok(()) => true,
            Err(e) => {
                self.note_disconnect(e);
                false
            }
        }
    }

    fn pcm(&mut self, samples: &[i16]) -> bool {
        if self.disconnected {
            return false;
        }
        match self.stream.write_all(&pcm_bytes(samples)) {
            Ok(()) => true,
            Err(e) => {
                self.note_disconnect(e);
                false
            }
        }
    }
}

struct RawHttpRequest {
    method: String,
    url: String,
    headers: Vec<Header>,
    body: Vec<u8>,
}

fn read_http_request(
    stream: &mut TcpStream,
    max_body_bytes: usize,
) -> Result<RawHttpRequest, AppError> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| AppError::new(400, format!("failed to read request line: {e}")))?;
    let request_line = request_line.trim_end_matches(['\r', '\n']);
    if request_line.is_empty() {
        return Err(AppError::new(400, "empty HTTP request"));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| AppError::new(400, "missing HTTP method"))?
        .to_string();
    let url = parts
        .next()
        .ok_or_else(|| AppError::new(400, "missing HTTP path"))?
        .to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| AppError::new(400, format!("failed to read header: {e}")))?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(AppError::new(400, "malformed HTTP header"));
        };
        headers.push(Header::new(name.trim(), value.trim()));
    }

    if header_value(&headers, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        return Err(AppError::new(
            400,
            "chunked request bodies are not supported",
        ));
    }

    let content_length = match header_value(&headers, "content-length") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|e| AppError::new(400, format!("invalid Content-Length: {e}")))?,
        None => 0,
    };
    if content_length > max_body_bytes {
        return Err(AppError::new(413, "request body exceeds 32 MiB limit"));
    }

    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|e| AppError::new(400, format!("failed to read request body: {e}")))?;

    Ok(RawHttpRequest {
        method,
        url,
        headers,
        body,
    })
}

/// Buffered writer: Content-Length framing. The streaming PCM route
/// deliberately BYPASSES this (see `stream_pcm_to_client` /
/// `pcm_stream_head`) — its body length is unknown until the last chunk, so
/// it is EOF-delimited instead.
fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason_phrase(response.status),
        response.content_type,
        response.body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(&response.body))
        .and_then(|_| stream.flush())
        .map_err(|e| e.to_string())
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "HTTP",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aokie-voice-server-test-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
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

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn test_server(max_body_bytes: usize) -> (TestDir, VoiceServer) {
        test_server_with(ServerConfig::default(), max_body_bytes)
    }

    fn test_server_with(config: ServerConfig, max_body_bytes: usize) -> (TestDir, VoiceServer) {
        let tmp = TestDir::new();
        let server = VoiceServer::new(config, tmp.path().to_path_buf(), max_body_bytes);
        (tmp, server)
    }

    fn json_header() -> Vec<Header> {
        vec![Header::new("Content-Type", "application/json")]
    }

    fn decode_json(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body).unwrap()
    }

    fn make_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
        write_pcm16_wav(samples, sample_rate).unwrap()
    }

    fn b64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);
            out.push(ALPHABET[(b0 >> 2) as usize] as char);
            out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    fn seed_parakeet_files(app_data: &Path) {
        let dir = app_data.join("models/parakeet");
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "encoder.int8.onnx",
            "decoder_joint.int8.onnx",
            "tokenizer.model",
        ] {
            fs::write(dir.join(name), b"x").unwrap();
        }
    }

    /// Resolve a config from string args + explicit env/file layers, the
    /// way `run_from_env` does (minus the process env / disk reads).
    fn resolve(args: &[&str], env: EnvConfig, file: FileConfig) -> Result<ServerConfig, String> {
        let cli = parse_cli_args(args.iter().map(|s| s.to_string()))?;
        merge_config(&cli, &env, &file)
    }

    // -------------------------------------------------------------------
    // Config resolver
    // -------------------------------------------------------------------

    /// Backward compat: the historical launch shapes must resolve exactly as
    /// before the mode split — `--port 17920` (or no args at all) is a `both`
    /// server on 17920 with the parakeet + pocket engines.
    #[test]
    fn config_backward_compat_bare_port() {
        let cfg = resolve(
            &["exe", "--port", "17920"],
            EnvConfig::default(),
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(cfg, ServerConfig::default());

        let cfg = resolve(&["exe"], EnvConfig::default(), FileConfig::default()).unwrap();
        assert_eq!(cfg, ServerConfig::default());

        let cfg = resolve(
            &["exe", "--port=18001"],
            EnvConfig::default(),
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(cfg.port, 18_001);
        assert_eq!(cfg.mode, ServerMode::Both);

        // Env port still honoured when no arg names one.
        let cfg = resolve(
            &["exe"],
            EnvConfig {
                port: Some("18002".to_string()),
                ..Default::default()
            },
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(cfg.port, 18_002);

        // A blank env port reads as absent (pre-split behaviour).
        let cfg = resolve(
            &["exe"],
            EnvConfig {
                port: Some("  ".to_string()),
                ..Default::default()
            },
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(cfg.port, DEFAULT_PORT);
    }

    /// Single-lane modes get their own default ports when `--port` is absent;
    /// an explicit port always wins.
    #[test]
    fn config_mode_default_ports() {
        let stt = resolve(
            &["exe", "--mode", "stt"],
            EnvConfig::default(),
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(stt.mode, ServerMode::Stt);
        assert_eq!(stt.port, DEFAULT_STT_PORT);

        let tts = resolve(
            &["exe", "--mode=tts"],
            EnvConfig::default(),
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(tts.mode, ServerMode::Tts);
        assert_eq!(tts.port, DEFAULT_TTS_PORT);

        let both = resolve(
            &["exe", "--mode", "both"],
            EnvConfig::default(),
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(both.port, DEFAULT_PORT);

        let pinned = resolve(
            &["exe", "--mode", "stt", "--port", "9000"],
            EnvConfig::default(),
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(pinned.port, 9_000);

        // Mode from env also drives the default port.
        let env_tts = resolve(
            &["exe"],
            EnvConfig {
                mode: Some("tts".to_string()),
                ..Default::default()
            },
            FileConfig::default(),
        )
        .unwrap();
        assert_eq!(env_tts.mode, ServerMode::Tts);
        assert_eq!(env_tts.port, DEFAULT_TTS_PORT);
    }

    /// Per-field precedence: args > env > config file > defaults.
    #[test]
    fn config_precedence_args_env_file() {
        let file: FileConfig = serde_json::from_str(
            r#"{
                "mode": "tts",
                "port": 1000,
                "sttEngine": "parakeet",
                "ttsEngine": "sherpa",
                "ttsModelDir": "C:/bundles/jenny"
            }"#,
        )
        .unwrap();
        let env = EnvConfig {
            port: Some("2000".to_string()),
            stt_engine: Some("moonshine".to_string()),
            ..Default::default()
        };
        let cfg = resolve(&["exe", "--port", "3000"], env.clone(), file.clone()).unwrap();
        assert_eq!(cfg.port, 3_000, "arg beats env beats file");
        assert_eq!(cfg.mode, ServerMode::Tts, "mode from the file");
        assert_eq!(
            cfg.stt_engine,
            SttEngineKind::Moonshine,
            "env beats the file"
        );
        assert_eq!(cfg.tts_engine, TtsEngineKind::Sherpa);
        assert_eq!(cfg.tts_model_dir, Some(PathBuf::from("C:/bundles/jenny")));

        // An arg-level engine override beats the env one.
        let cfg = resolve(
            &["exe", "--stt-engine", "qwen3-asr", "--tts-engine", "pocket"],
            env,
            file,
        )
        .unwrap();
        assert_eq!(cfg.stt_engine, SttEngineKind::Qwen3Asr);
        assert_eq!(cfg.tts_engine, TtsEngineKind::Pocket);
        // No port named by args -> env port (2000) wins over file (1000).
        assert_eq!(cfg.port, 2_000);
    }

    #[test]
    fn config_rejects_unknown_and_invalid_values() {
        assert!(resolve(
            &["exe", "--frobnicate"],
            EnvConfig::default(),
            FileConfig::default()
        )
        .unwrap_err()
        .contains("unknown argument"));

        assert!(resolve(
            &["exe", "--mode", "phone"],
            EnvConfig::default(),
            FileConfig::default()
        )
        .unwrap_err()
        .contains("--mode"));

        assert!(resolve(
            &["exe", "--stt-engine", "whisper"],
            EnvConfig::default(),
            FileConfig::default()
        )
        .unwrap_err()
        .contains("--stt-engine"));

        assert!(resolve(
            &["exe", "--tts-engine", "espeak"],
            EnvConfig::default(),
            FileConfig::default()
        )
        .unwrap_err()
        .contains("--tts-engine"));

        assert!(resolve(
            &["exe", "--port"],
            EnvConfig::default(),
            FileConfig::default()
        )
        .unwrap_err()
        .contains("requires a value"));

        // Invalid env values error when they are the selected source.
        assert!(resolve(
            &["exe"],
            EnvConfig {
                mode: Some("sideways".to_string()),
                ..Default::default()
            },
            FileConfig::default()
        )
        .unwrap_err()
        .contains("AOKIE_VOICE_MODE"));
    }

    /// The flat JSON config file parses with camelCase keys and tolerates
    /// unknown keys.
    #[test]
    fn config_file_parses_flat_keys() {
        let file: FileConfig = serde_json::from_str(
            r#"{
                "mode": "stt",
                "port": 17931,
                "sttEngine": "moonshine",
                "sttModelDir": "E:/models/moonshine-tiny",
                "future": {"ignored": true}
            }"#,
        )
        .unwrap();
        let cfg = resolve(&["exe"], EnvConfig::default(), file).unwrap();
        assert_eq!(cfg.mode, ServerMode::Stt);
        assert_eq!(cfg.port, 17_931);
        assert_eq!(cfg.stt_engine, SttEngineKind::Moonshine);
        assert_eq!(
            cfg.stt_model_dir,
            Some(PathBuf::from("E:/models/moonshine-tiny"))
        );
        assert_eq!(cfg.tts_engine, TtsEngineKind::Pocket);
    }

    // -------------------------------------------------------------------
    // Mode-gated routing
    // -------------------------------------------------------------------

    #[test]
    fn stt_mode_refuses_speech_route() {
        let (_tmp, server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Stt,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hello"}"#,
        );
        assert_eq!(response.status, 404);
        let value = decode_json(&response.body);
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("stt mode"));

        // The STT route is still served (503 = models absent, i.e. routed).
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({ "audio": b64(&wav) }).to_string().into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);
    }

    #[test]
    fn tts_mode_refuses_transcriptions_route() {
        let (_tmp, server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Tts,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({ "audio": b64(&wav) }).to_string().into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 404);
        let value = decode_json(&response.body);
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tts mode"));

        // The TTS route is still served (503 = models absent, i.e. routed).
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hello"}"#,
        );
        assert_eq!(response.status, 503);
    }

    // -------------------------------------------------------------------
    // Health + models per mode
    // -------------------------------------------------------------------

    #[test]
    fn health_checks_presence_without_loading_models() {
        let (tmp, server) = test_server(MAX_BODY_BYTES);
        seed_parakeet_files(tmp.path());
        let response = handle_request(&server, "GET", "/health", &[], b"");
        assert_eq!(response.status, 200);
        let value = decode_json(&response.body);
        // Truthful readiness (AOK-VOICE-SRV-001): TTS assets are absent, so
        // a BOTH-mode server must NOT read green — half a voice stack can't
        // take a call.
        assert_eq!(value["status"], "degraded");
        assert_eq!(value["mode"], "both");
        assert_eq!(value["stt"], true);
        assert_eq!(value["tts"], false);
        assert_eq!(value["lanes"]["stt"]["enabled"], true);
        assert_eq!(value["lanes"]["stt"]["engine"], "parakeet");
        assert_eq!(value["lanes"]["stt"]["ready"], true);
        assert_eq!(value["lanes"]["tts"]["enabled"], true);
        assert_eq!(value["lanes"]["tts"]["engine"], "pocket");
        assert_eq!(value["lanes"]["tts"]["ready"], false);
        assert!(
            value["build"]["version"].is_string(),
            "build provenance present"
        );
    }

    /// In a single-lane mode only THAT lane's assets decide ok/degraded — the
    /// disabled lane truthfully reads false but never degrades the instance.
    #[test]
    fn health_is_per_mode() {
        let (tmp, server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Stt,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        seed_parakeet_files(tmp.path());
        let value = decode_json(&handle_request(&server, "GET", "/health", &[], b"").body);
        assert_eq!(value["status"], "ok", "TTS assets must not matter: {value}");
        assert_eq!(value["mode"], "stt");
        assert_eq!(value["stt"], true);
        assert_eq!(value["tts"], false, "legacy boolean: this box can't speak");
        assert_eq!(value["lanes"]["tts"]["enabled"], false);

        // A tts-only instance with no voice assets is degraded and says why.
        let (_tmp2, tts_server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Tts,
                tts_engine: TtsEngineKind::Sherpa,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        let value = decode_json(&handle_request(&tts_server, "GET", "/health", &[], b"").body);
        assert_eq!(value["status"], "degraded");
        assert_eq!(value["mode"], "tts");
        assert_eq!(value["lanes"]["stt"]["enabled"], false);
        assert_eq!(value["lanes"]["tts"]["engine"], "sherpa");
        assert_eq!(value["lanes"]["tts"]["ready"], false);
        assert!(value["lanes"]["tts"]["error"]
            .as_str()
            .unwrap()
            .contains("no sherpa voice found"));
    }

    /// A valid sherpa voice bundle under `<app_data>/models/tts` is found by
    /// the alphabetical scan (plugin rules replicated), flips health green,
    /// and names the bundle in /v1/models.
    #[test]
    fn sherpa_bundle_scan_and_models_report() {
        let (tmp, server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Tts,
                tts_engine: TtsEngineKind::Sherpa,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        // Two bundles: "b-voice" valid, "a-voice" invalid (no tokens.txt) —
        // the scan must skip the invalid one even though it sorts first.
        let root = tmp.path().join("models/tts");
        fs::create_dir_all(root.join("a-voice")).unwrap();
        fs::write(root.join("a-voice/model.onnx"), b"x").unwrap();
        fs::create_dir_all(root.join("b-voice")).unwrap();
        fs::write(root.join("b-voice/model.onnx"), b"x").unwrap();
        fs::write(root.join("b-voice/tokens.txt"), b"x").unwrap();
        // The shared parent espeak data needs its phontab — the config
        // composer refuses a VITS bundle without one (a missing phontab
        // makes sherpa hand back a NULL engine that segfaults on use).
        fs::create_dir_all(root.join("espeak-ng-data")).unwrap();
        fs::write(root.join("espeak-ng-data/phontab"), b"x").unwrap();

        let value = decode_json(&handle_request(&server, "GET", "/health", &[], b"").body);
        assert_eq!(value["status"], "ok", "{value}");
        assert_eq!(value["tts"], true);
        assert!(value["lanes"]["tts"]["modelDir"]
            .as_str()
            .unwrap()
            .contains("b-voice"));

        let models = decode_json(&handle_request(&server, "GET", "/v1/models", &[], b"").body);
        let data = models["data"].as_array().unwrap();
        assert_eq!(data.len(), 1, "tts mode reports only the TTS lane");
        assert_eq!(data[0]["id"], "sherpa");
        assert_eq!(data[0]["bundle"], "b-voice");
        // Voice discovery: only the VALID bundle is listed ("a-voice" has no
        // tokens.txt).
        assert_eq!(data[0]["voices"], json!(["b-voice"]));

        // The bundle config replicates the plugin's composition rules:
        // shared espeak-ng-data in the PARENT + VITS when no voices.bin.
        let cfg = sherpa_config_for_dir(&root.join("b-voice")).unwrap();
        assert_eq!(cfg.engine, "vits");
        assert!(cfg.model_path.ends_with("model.onnx"));
        assert!(cfg.data_dir.contains("espeak-ng-data"));

        // voices.bin flips the bundle to Kokoro.
        fs::write(root.join("b-voice/voices.bin"), b"x").unwrap();
        let cfg = sherpa_config_for_dir(&root.join("b-voice")).unwrap();
        assert_eq!(cfg.engine, "kokoro");

        // Two .onnx files = ambiguous bundle, refused.
        fs::write(root.join("b-voice/other.onnx"), b"x").unwrap();
        assert!(sherpa_config_for_dir(&root.join("b-voice"))
            .unwrap_err()
            .contains("exactly one"));
    }

    // -------------------------------------------------------------------
    // Per-request sherpa voice resolution + bundle cache
    // -------------------------------------------------------------------

    /// Seed a valid bundle folder (tokens.txt + exactly one .onnx) under
    /// `root/name`, following the pattern the models tests use.
    fn seed_bundle(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.onnx"), b"x").unwrap();
        fs::write(dir.join("tokens.txt"), b"x").unwrap();
        dir
    }

    /// The resolution table: empty / numeric / installed name / absolute
    /// path / name:speaker / unknowns — pure over a fake scan root.
    #[test]
    fn sherpa_voice_resolution_table() {
        let tmp = TestDir::new();
        let root = tmp.path().join("models/tts");
        let jenny = seed_bundle(&root, "jenny");
        // "broken" fails the bundle test (no tokens.txt).
        fs::create_dir_all(root.join("broken")).unwrap();
        fs::write(root.join("broken/model.onnx"), b"x").unwrap();
        // "twin" fails it too (two .onnx files).
        let twin = seed_bundle(&root, "twin");
        fs::write(twin.join("other.onnx"), b"x").unwrap();

        let def = |speaker: &str| SherpaVoice::Default {
            speaker: speaker.to_string(),
        };
        let bundle = |dir: &Path, speaker: &str| SherpaVoice::Bundle {
            dir: dir.to_path_buf(),
            speaker: speaker.to_string(),
        };
        let unknown = |req: &str| SherpaVoice::Unknown {
            requested: req.to_string(),
        };

        // Empty / whitespace → default bundle, default speaker.
        assert_eq!(resolve_sherpa_voice("", &root), def(""));
        assert_eq!(resolve_sherpa_voice("   ", &root), def(""));
        // Numeric → default bundle + that speaker id.
        assert_eq!(resolve_sherpa_voice("3", &root), def("3"));
        assert_eq!(resolve_sherpa_voice(" 12 ", &root), def("12"));
        // Installed bundle folder name → that bundle, default speaker.
        assert_eq!(resolve_sherpa_voice("jenny", &root), bundle(&jenny, ""));
        // Absolute path to a valid bundle dir.
        let abs = jenny.to_string_lossy().to_string();
        assert!(Path::new(&abs).is_absolute(), "temp dir must be absolute");
        assert_eq!(resolve_sherpa_voice(&abs, &root), bundle(&jenny, ""));
        // Combined name:speaker and abs-path:speaker.
        assert_eq!(resolve_sherpa_voice("jenny:2", &root), bundle(&jenny, "2"));
        assert_eq!(
            resolve_sherpa_voice(&format!("{abs}:5"), &root),
            bundle(&jenny, "5")
        );
        // Unknowns: absent name, invalid bundles, pocket preset leftovers,
        // traversal attempts, abs path to a non-bundle.
        assert_eq!(resolve_sherpa_voice("nope", &root), unknown("nope"));
        assert_eq!(resolve_sherpa_voice("broken", &root), unknown("broken"));
        assert_eq!(resolve_sherpa_voice("twin", &root), unknown("twin"));
        assert_eq!(resolve_sherpa_voice("alba", &root), unknown("alba"));
        assert_eq!(resolve_sherpa_voice("../jenny", &root), unknown("../jenny"));
        assert_eq!(
            resolve_sherpa_voice("..\\jenny", &root),
            unknown("..\\jenny")
        );
        let non_bundle = root.join("broken").to_string_lossy().to_string();
        assert_eq!(
            resolve_sherpa_voice(&non_bundle, &root),
            unknown(&non_bundle)
        );
        // A bad speaker suffix on a good bundle is unknown as a WHOLE (never
        // half-applied).
        assert_eq!(
            resolve_sherpa_voice("jenny:loud", &root),
            unknown("jenny:loud")
        );
    }

    /// Installed-bundle discovery is sorted and skips invalid folders.
    #[test]
    fn installed_bundles_listing_is_sorted_and_valid_only() {
        let tmp = TestDir::new();
        let root = tmp.path().join("models/tts");
        seed_bundle(&root, "zeta");
        seed_bundle(&root, "alpha");
        fs::create_dir_all(root.join("not-a-bundle")).unwrap();
        fs::create_dir_all(root.join("espeak-ng-data")).unwrap();
        assert_eq!(installed_sherpa_bundles(&root), vec!["alpha", "zeta"]);
        // Missing root = no voices, no error.
        assert_eq!(
            installed_sherpa_bundles(&tmp.path().join("nowhere")),
            Vec::<String>::new()
        );
    }

    /// LRU semantics: capacity 3, access refreshes recency, the least
    /// recently used entry is evicted, a failed load caches nothing.
    #[test]
    fn lru_cache_evicts_least_recently_used() {
        let mut cache: LruCache<u32> = LruCache::new(3);
        let (a, b, c, d) = (
            PathBuf::from("a"),
            PathBuf::from("b"),
            PathBuf::from("c"),
            PathBuf::from("d"),
        );
        let loads = std::cell::Cell::new(0usize);
        let get = |cache: &mut LruCache<u32>, key: &Path, value: u32| -> u32 {
            *cache
                .get_or_insert_with(key, || -> Result<u32, ()> {
                    loads.set(loads.get() + 1);
                    Ok(value)
                })
                .unwrap()
        };
        assert_eq!(get(&mut cache, &a, 1), 1);
        assert_eq!(get(&mut cache, &b, 2), 2);
        assert_eq!(get(&mut cache, &c, 3), 3);
        assert_eq!(loads.get(), 3);
        assert_eq!(cache.keys(), vec![c.as_path(), b.as_path(), a.as_path()]);

        // Hit on `a` refreshes it (no reload) — `b` becomes LRU.
        assert_eq!(get(&mut cache, &a, 99), 1, "hit returns the cached value");
        assert_eq!(loads.get(), 3, "a hit never reloads");
        assert_eq!(cache.keys(), vec![a.as_path(), c.as_path(), b.as_path()]);

        // Insert `d` at capacity — `b` (LRU) is evicted.
        assert_eq!(get(&mut cache, &d, 4), 4);
        assert_eq!(loads.get(), 4);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.keys(), vec![d.as_path(), a.as_path(), c.as_path()]);

        // Re-fetching the evicted `b` loads again.
        assert_eq!(get(&mut cache, &b, 5), 5);
        assert_eq!(loads.get(), 5);
        assert_eq!(cache.keys(), vec![b.as_path(), d.as_path(), a.as_path()]);

        // A failed load caches nothing and surfaces the error.
        let e = PathBuf::from("e");
        assert!(cache
            .get_or_insert_with(&e, || Err::<u32, &str>("boom"))
            .is_err());
        assert_eq!(cache.len(), 3);
        assert!(!cache.keys().contains(&e.as_path()));
    }

    #[test]
    fn models_reflect_selected_engines_and_mode() {
        let (_tmp, server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Stt,
                stt_engine: SttEngineKind::Qwen3Asr,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        let value = decode_json(&handle_request(&server, "GET", "/v1/models", &[], b"").body);
        let data = value["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], QWEN3_ASR_MODEL_ID);

        let (_tmp2, both) = test_server(MAX_BODY_BYTES);
        let value = decode_json(&handle_request(&both, "GET", "/v1/models", &[], b"").body);
        let ids: Vec<&str> = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![PARAKEET_MODEL_ID, POCKET_MODEL_ID]);
    }

    /// Alternate STT engines report presence per THEIR file conventions
    /// (transcribe-rs quantization-suffixed sets).
    #[test]
    fn moonshine_and_qwen3_presence_conventions() {
        let (tmp, server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Stt,
                stt_engine: SttEngineKind::Moonshine,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        let value = decode_json(&handle_request(&server, "GET", "/health", &[], b"").body);
        assert_eq!(value["status"], "degraded");
        assert_eq!(value["lanes"]["stt"]["engine"], "moonshine");

        // An int8-suffixed moonshine set counts as present.
        let dir = tmp.path().join("models/moonshine");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("encoder_model.int8.onnx"), b"x").unwrap();
        fs::write(dir.join("decoder_model_merged.int8.onnx"), b"x").unwrap();
        fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
        let value = decode_json(&handle_request(&server, "GET", "/health", &[], b"").body);
        assert_eq!(value["status"], "ok", "{value}");
        assert_eq!(
            detect_quantization(&dir, &MOONSHINE_STEMS, &MOONSHINE_QUANT_ORDER),
            Some(Quantization::Int8)
        );

        // Qwen3-ASR wants the 3 sessions + tokenizer.json + embed_tokens.bin;
        // the published int4 set is preferred.
        let (tmp2, qserver) = test_server_with(
            ServerConfig {
                mode: ServerMode::Stt,
                stt_engine: SttEngineKind::Qwen3Asr,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        let qdir = tmp2.path().join("models/qwen3-asr");
        fs::create_dir_all(&qdir).unwrap();
        for stem in QWEN3_ASR_STEMS {
            fs::write(qdir.join(format!("{stem}.int4.onnx")), b"x").unwrap();
        }
        fs::write(qdir.join("tokenizer.json"), b"{}").unwrap();
        let value = decode_json(&handle_request(&qserver, "GET", "/health", &[], b"").body);
        assert_eq!(
            value["status"], "degraded",
            "embed_tokens.bin still missing"
        );
        fs::write(qdir.join("embed_tokens.bin"), b"x").unwrap();
        let value = decode_json(&handle_request(&qserver, "GET", "/health", &[], b"").body);
        assert_eq!(value["status"], "ok", "{value}");
        assert_eq!(
            detect_quantization(&qdir, &QWEN3_ASR_STEMS, &QWEN3_ASR_QUANT_ORDER),
            Some(Quantization::Int4)
        );
    }

    // -------------------------------------------------------------------
    // Loudness normalization
    // -------------------------------------------------------------------

    /// Attenuate-only: silence unchanged, a hot near-full-scale signal is
    /// scaled down under both targets, a quiet signal passes untouched.
    #[test]
    fn loudness_normalization_attenuates_only() {
        // Silence: untouched, scale 1.0.
        let mut silence = vec![0.0f32; 480];
        assert_eq!(attenuate_hot_signal(&mut silence), 1.0);
        assert!(silence.iter().all(|&s| s == 0.0));

        // Empty: no panic.
        let mut empty: Vec<f32> = Vec::new();
        assert_eq!(attenuate_hot_signal(&mut empty), 1.0);

        // Hot full-scale sine: attenuated so peak <= 0.85 and rms <= 0.12.
        let mut hot: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.999).collect();
        let scale = attenuate_hot_signal(&mut hot);
        assert!(scale < 1.0, "hot signal must be attenuated, got {scale}");
        let peak = hot.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        let rms = (hot.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / hot.len() as f64)
            .sqrt() as f32;
        assert!(peak <= 0.85 + 1e-3, "peak {peak}");
        assert!(rms <= 0.12 + 1e-3, "rms {rms}");

        // Quiet signal: both targets already met -> untouched.
        let mut quiet: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.05).collect();
        let before = quiet.clone();
        assert_eq!(attenuate_hot_signal(&mut quiet), 1.0);
        assert_eq!(quiet, before);
    }

    // -------------------------------------------------------------------
    // Pre-existing request-surface tests (unchanged behaviour)
    // -------------------------------------------------------------------

    #[test]
    fn wav_decode_mono_pcm16_passthrough() {
        let wav = make_wav(&[0, 16_384, -16_384, 32_767], 16_000);
        let samples = decode_wav_to_f32_16k(&wav).unwrap();
        assert_eq!(samples.len(), 4);
        assert!((samples[0] - 0.0).abs() < 0.0001);
        assert!((samples[1] - 0.5).abs() < 0.0001);
        assert!((samples[2] + 0.5).abs() < 0.0001);
        assert!((samples[3] - (32_767.0 / 32_768.0)).abs() < 0.0001);
    }

    #[test]
    fn wav_decode_stereo_downmixes_to_mono() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for sample in [16_384i16, 0, 0, -16_384] {
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }
        let samples = decode_wav_to_f32_16k(&cursor.into_inner()).unwrap();
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 0.25).abs() < 0.0001);
        assert!((samples[1] + 0.25).abs() < 0.0001);
    }

    #[test]
    fn wav_decode_resamples_to_16khz() {
        let wav = make_wav(&[0, 8192, 16_384, 24_576], 8_000);
        let samples = decode_wav_to_f32_16k(&wav).unwrap();
        assert_eq!(samples.len(), 8);
        assert!((samples[0] - 0.0).abs() < 0.0001);
        assert!((samples[2] - 0.25).abs() < 0.0001);
        assert!((samples[4] - 0.5).abs() < 0.0001);
        assert!(samples.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn decodes_bare_base64_audio_field() {
        let bytes = b"RIFFtest";
        assert_eq!(decode_audio_field(&b64(bytes)).unwrap(), bytes);
    }

    #[test]
    fn decodes_data_url_audio_field() {
        let bytes = b"WAVEdata";
        let data_url = format!("data:audio/wav;base64,{}", b64(bytes));
        assert_eq!(decode_audio_field(&data_url).unwrap(), bytes);
    }

    #[test]
    fn audio_field_wins_over_file_field() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({
            "audio": format!("data:audio/wav;base64,{}", b64(&wav)),
            "file": "not-valid-base64@@",
            "model": PARAKEET_MODEL_ID
        })
        .to_string()
        .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("STT model files"),
            "audio should win over the malformed file alias"
        );
    }

    #[test]
    fn file_alias_accepts_bare_base64_wav() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({ "file": b64(&wav), "model": PARAKEET_MODEL_ID })
            .to_string()
            .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("STT model files"));
    }

    #[test]
    fn missing_audio_and_file_returns_json_4xx() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            br#"{"model":"x"}"#,
        );
        assert_eq!(response.status, 400);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing audio"));
    }

    #[test]
    fn malformed_json_returns_json_4xx() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            br#"{"audio":"nope""#,
        );
        assert_eq!(response.status, 400);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("malformed JSON"));
    }

    #[test]
    fn oversized_body_is_rejected() {
        let (_tmp, server) = test_server(8);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            b"123456789",
        );
        assert_eq!(response.status, 413);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn unknown_route_returns_404() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(&server, "GET", "/missing", &[], b"");
        assert_eq!(response.status, 404);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
    }

    #[test]
    fn wav_emit_round_trips_through_reader() {
        let input = [-32_768, -1, 0, 1, 32_767];
        let wav = write_pcm16_wav(&input, 24_000).unwrap();
        let mut reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 24_000);
        assert_eq!(spec.bits_per_sample, 16);
        let output = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn transcription_with_missing_models_returns_graceful_error() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({ "audio": b64(&wav), "model": PARAKEET_MODEL_ID })
            .to_string()
            .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("STT model files"));
    }

    #[test]
    fn speech_with_missing_models_returns_graceful_error() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hello","voice":"alba","response_format":"wav"}"#,
        );
        assert_eq!(response.status, 503);
        let value = decode_json(&response.body);
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert!(value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TTS model files"));
    }

    /// OpenAI-compatible `speed`: validated BEFORE any engine work — an
    /// out-of-band value is a 400 naming the bounds; an in-band value on a
    /// modelless server still reaches the graceful 503 (i.e. it parsed).
    #[test]
    fn speech_speed_is_validated() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        for bad in ["0.1", "3.5", "-1", "0"] {
            let body = format!(r#"{{"input":"hello","speed":{bad}}}"#);
            let response = handle_request(
                &server,
                "POST",
                "/v1/audio/speech",
                &json_header(),
                body.as_bytes(),
            );
            assert_eq!(response.status, 400, "speed {bad} must be rejected");
            let value = decode_json(&response.body);
            assert!(
                value["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("speed"),
                "error names the field: {value}"
            );
        }
        // Valid speed passes validation (503 = models absent, not a speed error).
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hello","speed":0.75}"#,
        );
        assert_eq!(response.status, 503);
    }

    // -------------------------------------------------------------------
    // Streaming PCM speech route (VOX-402)
    // -------------------------------------------------------------------

    /// Test sink: records the announced rate + per-chunk sample counts, and
    /// can simulate a client disconnect after N chunks.
    struct RecordingSink {
        rate: Option<u32>,
        chunks: Vec<usize>,
        total_bytes: usize,
        cancel_after_chunks: Option<usize>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                rate: None,
                chunks: Vec::new(),
                total_bytes: 0,
                cancel_after_chunks: None,
            }
        }
    }

    impl PcmSink for RecordingSink {
        fn begin(&mut self, sample_rate: u32) -> bool {
            self.rate = Some(sample_rate);
            true
        }

        fn pcm(&mut self, samples: &[i16]) -> bool {
            if self.cancel_after_chunks == Some(self.chunks.len()) {
                return false;
            }
            self.chunks.push(samples.len());
            self.total_bytes += pcm_bytes(samples).len();
            true
        }
    }

    /// The PCM head is EOF-delimited: 200, audio/pcm, X-Sample-Rate names
    /// the stream's rate, Connection: close, and NO Content-Length.
    #[test]
    fn pcm_stream_head_framing() {
        let head = pcm_stream_head(24_000);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Type: audio/pcm\r\n"), "{head}");
        assert!(head.contains("X-Sample-Rate: 24000\r\n"), "{head}");
        assert!(head.contains("Connection: close\r\n"), "{head}");
        assert!(
            !head.to_ascii_lowercase().contains("content-length"),
            "streamed body must be EOF-delimited: {head}"
        );
        assert!(head.ends_with("\r\n\r\n"), "{head}");
    }

    /// One-shot engines drip their buffer in ~100ms slices; every sample
    /// reaches the sink exactly once (correct byte count), the announced
    /// rate is the engine's, and a cancelling sink stops the slicing.
    #[test]
    fn pcm_slicing_streams_whole_buffer_and_honours_cancel() {
        assert_eq!(pcm_slice_samples(16_000), 1_600);
        assert_eq!(pcm_slice_samples(22_050), 2_205);
        assert_eq!(pcm_slice_samples(1), 1, "never a zero-sized slice");

        let pcm: Vec<i16> = (0..4_000).map(|i| i as i16).collect();
        let mut sink = RecordingSink::new();
        assert_eq!(
            stream_pcm_slices(&pcm, 16_000, &mut sink),
            PcmOutcome::Completed
        );
        assert_eq!(sink.rate, Some(16_000));
        assert_eq!(sink.chunks, vec![1_600, 1_600, 800]);
        assert_eq!(sink.total_bytes, 8_000, "2 bytes per s16le sample");

        // Client gone after the first chunk -> synthesis cancelled, no
        // further slices written.
        let mut cancelling = RecordingSink::new();
        cancelling.cancel_after_chunks = Some(1);
        assert_eq!(
            stream_pcm_slices(&pcm, 16_000, &mut cancelling),
            PcmOutcome::Cancelled
        );
        assert_eq!(cancelling.chunks, vec![1_600]);

        // s16le wire encoding is exact.
        assert_eq!(pcm_bytes(&[0x0102i16, -2]), vec![0x02, 0x01, 0xfe, 0xff]);
    }

    /// response_format routing: pcm yields a stream plan (normalized input,
    /// voice carried), wav/absent stays buffered, junk is a 400.
    #[test]
    fn speech_pcm_routes_to_stream_plan() {
        assert_eq!(parse_speech_format(None), Ok(SpeechFormat::Wav));
        assert_eq!(parse_speech_format(Some("WAV")), Ok(SpeechFormat::Wav));
        assert_eq!(parse_speech_format(Some("pcm")), Ok(SpeechFormat::Pcm));
        assert!(parse_speech_format(Some("mp3")).is_err());

        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let body =
            br#"{"input":"  hello there ","voice":"3","model":"tts-1","response_format":"pcm"}"#;
        match route_request(&server, "POST", "/v1/audio/speech", &json_header(), body) {
            Routed::PcmStream(req) => {
                assert_eq!(req.input, "hello there", "trimmed + normalized");
                assert_eq!(req.voice, "3");
            }
            Routed::Buffered(resp) => panic!(
                "pcm must route to the stream plan, got {} {:?}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            ),
        }

        // An unsupported format is refused up front.
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hi","response_format":"mp3"}"#,
        );
        assert_eq!(response.status, 400);
        assert!(decode_json(&response.body)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("response_format"));

        // Mode gating still wins over the format.
        let (_tmp2, stt_only) = test_server_with(
            ServerConfig {
                mode: ServerMode::Stt,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        match route_request(
            &stt_only,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hi","response_format":"pcm"}"#,
        ) {
            Routed::Buffered(resp) => assert_eq!(resp.status, 404),
            Routed::PcmStream(_) => panic!("stt mode must refuse the speech route"),
        }
    }

    /// speed != 1.0 with pcm is refused (WSOLA needs the whole utterance);
    /// an explicit speed of exactly 1.0 stays allowed, and wav keeps speed.
    #[test]
    fn speech_pcm_refuses_speed() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hi","response_format":"pcm","speed":0.75}"#,
        );
        assert_eq!(response.status, 400);
        let message = decode_json(&response.body)["error"]["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(message.contains("speed"), "{message}");
        assert!(message.contains("pcm"), "{message}");

        // speed exactly 1.0 is a no-op — allowed with pcm.
        match route_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hi","response_format":"pcm","speed":1.0}"#,
        ) {
            Routed::PcmStream(_) => {}
            Routed::Buffered(resp) => panic!("speed 1.0 + pcm must stream, got {}", resp.status),
        }

        // wav keeps speed support unchanged (503 = models absent, parsed ok).
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hi","response_format":"wav","speed":0.75}"#,
        );
        assert_eq!(response.status, 503);
    }

    /// Missing TTS models on the pcm route: the same graceful 503 as wav,
    /// and it fails BEFORE the head is written (sink never begun) — the
    /// client still gets a proper JSON error, never a broken stream.
    #[test]
    fn speech_pcm_missing_models_is_graceful_503() {
        let (_tmp, server) = test_server_with(
            ServerConfig {
                mode: ServerMode::Tts,
                ..Default::default()
            },
            MAX_BODY_BYTES,
        );
        let mut sink = BufferPcmSink::default();
        let err = server
            .synthesize_pcm("hello", "", &mut sink)
            .expect_err("no models on disk");
        assert_eq!(err.status, 503);
        assert!(err.message.contains("TTS model files"), "{}", err.message);
        assert_eq!(sink.sample_rate, None, "head must not have been sent");
        assert!(sink.bytes.is_empty());

        // The buffered entry point reports the same surface.
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hello","response_format":"pcm"}"#,
        );
        assert_eq!(response.status, 503);
        assert!(decode_json(&response.body)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("TTS model files"));
    }

    /// OpenAI-spec tolerance: a `model` field on either audio route is
    /// accepted and ignored — never a 400.
    #[test]
    fn model_field_is_tolerated_on_both_routes() {
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/speech",
            &json_header(),
            br#"{"input":"hi","model":"gpt-4o-mini-tts"}"#,
        );
        assert_eq!(response.status, 503, "parsed past model, failed on assets");

        let wav = make_wav(&[0, 1, -1, 0], 16_000);
        let body = json!({ "audio": b64(&wav), "model": "whisper-1" })
            .to_string()
            .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503, "parsed past model, failed on assets");
    }

    /// Transcription response_format: json (default) / text accepted —
    /// validated before inference — and junk refused with a clear 400.
    #[test]
    fn transcription_response_format_is_validated() {
        assert_eq!(
            parse_transcription_format(None),
            Ok(TranscriptionFormat::Json)
        );
        assert_eq!(
            parse_transcription_format(Some("JSON")),
            Ok(TranscriptionFormat::Json)
        );
        assert_eq!(
            parse_transcription_format(Some("text")),
            Ok(TranscriptionFormat::Text)
        );
        assert!(parse_transcription_format(Some("srt")).is_err());

        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let wav = make_wav(&[0, 1, -1, 0], 16_000);

        // "text" parses fine; the modelless 503 proves it got past parsing.
        let body = json!({ "audio": b64(&wav), "response_format": "text" })
            .to_string()
            .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 503);

        // Junk is a 400 naming the field, BEFORE any inference.
        let body = json!({ "audio": b64(&wav), "response_format": "srt" })
            .to_string()
            .into_bytes();
        let response = handle_request(
            &server,
            "POST",
            "/v1/audio/transcriptions",
            &json_header(),
            &body,
        );
        assert_eq!(response.status, 400);
        assert!(decode_json(&response.body)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("response_format"));
    }

    /// Multipart transcriptions carry response_format as a form field.
    #[test]
    fn multipart_response_format_field_is_parsed() {
        let wav = make_wav(&[1, 2, 3], 16_000);
        let mut body = Vec::new();
        body.extend_from_slice(b"--abc123\r\n");
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"response_format\"\r\n\r\n");
        body.extend_from_slice(b"text\r\n");
        body.extend_from_slice(b"--abc123\r\n");
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
        body.extend_from_slice(b"whisper-1\r\n");
        body.extend_from_slice(b"--abc123\r\n");
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
        body.extend_from_slice(&wav);
        body.extend_from_slice(b"\r\n--abc123--\r\n");

        let parts = parse_multipart(&body, "multipart/form-data; boundary=abc123").unwrap();
        assert_eq!(parts.file.as_deref(), Some(wav.as_slice()));
        assert_eq!(parts.response_format.as_deref(), Some("text"));

        // End-to-end on a modelless server: parses (incl. the ignored model
        // part), reaches the graceful 503.
        let (_tmp, server) = test_server(MAX_BODY_BYTES);
        let headers = vec![Header::new(
            "Content-Type",
            "multipart/form-data; boundary=abc123",
        )];
        let response = handle_request(&server, "POST", "/v1/audio/transcriptions", &headers, &body);
        assert_eq!(response.status, 503);
    }

    #[test]
    fn parses_multipart_file_part() {
        let wav = make_wav(&[1, 2, 3], 16_000);
        let boundary = "abc123";
        let mut body = Vec::new();
        body.extend_from_slice(b"--abc123\r\n");
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
        body.extend_from_slice(&wav);
        body.extend_from_slice(b"\r\n--abc123--\r\n");
        let parsed =
            parse_multipart_file(&body, &format!("multipart/form-data; boundary={boundary}"))
                .unwrap();
        assert_eq!(parsed, wav);
    }
}
