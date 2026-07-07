//! Local LLM server sidecar — spawn an external HTTP server (llama.cpp's
//! `llama-server`, koboldcpp, vllm, anything OpenAI-compatible), hold
//! the child handle in app state, and expose start / stop / status to
//! the frontend.
//!
//! "llama-server" is in the type names for legacy reasons — the sidecar
//! itself is engine-agnostic. The `preset` field on
//! [`StartLlamaServerRequest`] picks how the args are assembled:
//!
//! - `"llama-cpp"` (default): forwards `-m {model} --port {port}
//!   --host 127.0.0.1` plus optional `-ngl` / `-c`. Any
//!   `extra_args` append. This is the single-click path most users
//!   want.
//! - `"custom"`: ignores the llama.cpp flag set entirely. The whole
//!   command line comes from `args_template` (with `{model}`,
//!   `{port}`, `{ngl}`, `{ctx}` placeholders) plus `extra_args`.
//!
//! Pointing the `openai-http` LLM adapter at this server is the
//! intended consumption path:
//!   base_url  = "http://127.0.0.1:<port>/v1"
//!   api_key   = "" (most local servers don't require auth)
//! The adapter is unchanged — the sidecar just supervises the
//! process.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// How many trailing stderr lines from the sidecar we retain so a
/// "child exited during startup" failure can quote the *actual* reason
/// (CUDA OOM, port bind error, corrupt GGUF, …) instead of a guess.
const STDERR_TAIL_CAP: usize = 40;

use serde::{Deserialize, Serialize};

// Tauri-free: the legacy app took a `tauri::AppHandle` (to resolve the
// resource / app-data dirs) and a `tauri::State<LlamaServerState>`. Here
// the caller owns the [`LlamaServerState`] and passes it by reference,
// and supplies the resource/app-data dirs as owned `Option<&Path>`
// parameters — `None` when there is no bundled resource dir.

/// R3-#11: process-wide handle on the per-session API key the
/// sidecar was spawned with.
///
/// The reviewer flagged that any local process on the same machine
/// can talk to the sidecar's loopback port — `argv_binds_non_localhost`
/// already prevents LAN exposure, but sibling processes (a malicious
/// node script, a phishing tool that survived a sandbox escape) are
/// still on 127.0.0.1. A random per-session bearer token closes that:
/// llama.cpp's server accepts `--api-key <token>`, rejects requests
/// without a matching `Authorization: Bearer` header, and rotates on
/// every spawn so a captured key from a prior run is useless.
///
/// Stored as `OnceLock<Mutex<Option<String>>>` because the LLM
/// adapter (`ai/registry::active_llm`) reads it without an AppHandle
/// in scope; threading `tauri::State<LlamaServerState>` through every
/// caller would be a much larger refactor for the same end state.
static SIDECAR_SESSION_TOKEN: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Read the current sidecar session token, or `None` when no
/// in-process llama-server has been spawned this session. The HTTP
/// LLM adapter consults this for `kind == "llama-server"` and
/// prepends it as a Bearer header — config-supplied api_key (which
/// power-users might still set for a custom remote endpoint) wins
/// when explicitly populated.
pub fn current_session_token() -> Option<String> {
    let cell = SIDECAR_SESSION_TOKEN.get()?;
    cell.lock().ok()?.clone()
}

/// Update the stored session token. Sidecar spawn writes the freshly
/// generated value here; sidecar stop / kill writes `None` so the
/// next "is the sidecar even up?" call from the adapter sees the
/// truth.
fn set_session_token(value: Option<String>) {
    let cell = SIDECAR_SESSION_TOKEN.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = value;
    }
}

/// Generate a random 32-byte hex-encoded session token. Uses the
/// thread-local CSPRNG `rand::thread_rng`, which seeds from the OS
/// — enough entropy to make a stolen-token attack against the
/// sidecar's tiny attack window a non-issue.
fn generate_session_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Replace the value following any `--api-key` argument with a
/// `<redacted>` placeholder so the spawn log doesn't leak the
/// session token. Returns a new vector — the original argv is still
/// what gets handed to `Command::spawn`.
fn redact_api_key_arg(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut skip_next = false;
    for arg in argv {
        if skip_next {
            out.push("<redacted>".to_string());
            skip_next = false;
            continue;
        }
        if arg == "--api-key" {
            out.push(arg.clone());
            skip_next = true;
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Sane default — avoids the well-known Ollama (11434) and LM Studio
/// (1234) ports so a user who already runs one of those can spin up
/// llama-server alongside without colliding.
pub const DEFAULT_PORT: u16 = 8081;

/// Parse the TCP port out of a `base_url` like `http://127.0.0.1:8081/v1`.
/// `None` when there's no explicit port, so the caller falls back to
/// `DEFAULT_PORT`. Lets an operator pin a port via the provider config.
fn port_from_base_url(base_url: &str) -> Option<u16> {
    let after_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    authority.rsplit(':').next()?.trim().parse::<u16>().ok()
}

/// First free port at or above `start`. llama-server binds
/// `--host 127.0.0.1`, so the IPv4 loopback is the only family that
/// matters. Capped so a saturated range fails fast (the spawn then
/// surfaces the bind error) rather than spinning. Lets the sidecar
/// coexist with other local projects that have grabbed 8081.
fn find_free_port(start: u16) -> u16 {
    use std::net::TcpListener;
    (start..start.saturating_add(64))
        .find(|&p| TcpListener::bind(("127.0.0.1", p)).is_ok())
        .unwrap_or(start)
}

/// Built-in preset: llama.cpp's `llama-server` flag set. Used when
/// the request's `preset` is empty or `"llama-cpp"`.
pub const PRESET_LLAMA_CPP: &str = "llama-cpp";
/// Built-in preset: don't add any default flags — the user provides
/// the entire command line via `args_template` + `extra_args`.
pub const PRESET_CUSTOM: &str = "custom";

#[derive(Default)]
pub struct LlamaServerState {
    /// `None` when no server is running; `Some` while a child is
    /// alive. Wrapped in a std `Mutex` (not tokio) because the only
    /// operations are short — try_wait, kill, swap.
    inner: Arc<Mutex<Option<RunningServer>>>,
}

impl LlamaServerState {
    /// Returns `true` when the spawned `llama-server` child is still
    /// alive. Reaps any zombie that's already exited so the next
    /// status query reflects reality. Callers (e.g. the dashboard's
    /// `get_ml_stack_status`) use this for the "ready for calls"
    /// derivation without needing access to the private `inner`.
    pub fn is_alive(&self) -> bool {
        let inner = self.inner.clone();
        let mut guard = match inner.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        match guard.as_mut() {
            Some(running) => match running.child.try_wait() {
                Ok(None) => true,
                _ => {
                    // Already exited — drop the slot so the caller
                    // doesn't see a stale "running" report next time.
                    *guard = None;
                    false
                }
            },
            None => false,
        }
    }

    /// Like `is_alive`, but on exit captures the child's exit code and
    /// the tail of its stderr so the caller can build an actionable
    /// failure message. Reaps the slot on exit, same as `is_alive`.
    fn liveness(&self) -> Liveness {
        let inner = self.inner.clone();
        let mut guard = match inner.lock() {
            Ok(g) => g,
            Err(_) => return Liveness::NotRunning,
        };
        match guard.as_mut() {
            Some(running) => match running.child.try_wait() {
                Ok(None) => Liveness::Alive,
                Ok(Some(status)) => {
                    let stderr_tail = running
                        .stderr_tail
                        .lock()
                        .map(|t| t.iter().cloned().collect())
                        .unwrap_or_default();
                    let code = status.code();
                    *guard = None;
                    Liveness::Exited { code, stderr_tail }
                }
                Err(_) => {
                    *guard = None;
                    Liveness::NotRunning
                }
            },
            None => Liveness::NotRunning,
        }
    }
}

struct RunningServer {
    child: Child,
    binary_path: String,
    model_path: String,
    port: u16,
    started_at: Instant,
    /// Echoed back through `LlamaServerStatus` so the frontend can
    /// restore the preset dropdown after a page reload.
    preset: String,
    args_template: Vec<String>,
    /// Ring buffer of the child's most recent stderr lines, filled by
    /// the tee thread spawned at start. Read on exit so the failure
    /// message quotes llama.cpp's own diagnostic (CUDA OOM / bind
    /// error / bad GGUF) rather than a generic "most likely cause".
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

/// Snapshot of the sidecar child's liveness, used by `wait_for_ready`
/// to distinguish "still loading" from "died, and here's why".
enum Liveness {
    /// Child is still running.
    Alive,
    /// Child has exited. `code` is its process exit code (None if it
    /// was signalled / unavailable); `stderr_tail` is the last few
    /// lines it printed before dying.
    Exited {
        code: Option<i32>,
        stderr_tail: Vec<String>,
    },
    /// No child in the slot (never started, or already reaped by a
    /// concurrent status poll).
    NotRunning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartLlamaServerRequest {
    /// Path to the server binary. Empty/missing falls back to
    /// `llama-server` so a binary on `PATH` works without explicit
    /// configuration. For other engines (vllm, koboldcpp), set this
    /// to the full path to that binary.
    #[serde(default)]
    pub binary_path: String,
    /// Path to the model file. The current presets pass this in
    /// where they expect a model — llama.cpp expects a GGUF, vllm
    /// expects an HF id or directory; the field is just a string we
    /// substitute into the args template.
    pub model_path: String,
    /// Port to bind on. Falls back to `DEFAULT_PORT` when 0.
    #[serde(default)]
    pub port: u16,
    /// llama.cpp flag passthrough — `-ngl <N>` (number of layers
    /// offloaded to GPU). Honoured by the `llama-cpp` preset; other
    /// presets surface it as the `{ngl}` placeholder in
    /// `args_template`.
    #[serde(default)]
    pub n_gpu_layers: Option<i32>,
    /// llama.cpp flag passthrough — `-c <N>` (context window size).
    /// Same dispatch story as `n_gpu_layers`.
    #[serde(default)]
    pub ctx_size: Option<u32>,
    /// Pass-through extra CLI arguments. Appended verbatim after
    /// the preset/template args. Power-user knob — works under any
    /// preset.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Built-in preset name; see `PRESET_*` constants. Empty
    /// defaults to `llama-cpp` (back-compat with pre-Phase-5b
    /// callers that didn't set this field).
    #[serde(default)]
    pub preset: String,
    /// Custom command line. Used when `preset == "custom"` — the
    /// runtime substitutes `{model}`, `{port}`, `{ngl}`, `{ctx}`
    /// placeholders before spawning. Ignored under the `llama-cpp`
    /// preset.
    #[serde(default)]
    pub args_template: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlamaServerStatus {
    pub running: bool,
    /// Endpoint the openai-http adapter should talk to. Returned
    /// even when not running, so the UI can pre-fill the field.
    pub base_url: String,
    pub port: u16,
    pub binary_path: String,
    pub model_path: String,
    /// Seconds since spawn. None when not running.
    pub uptime_secs: Option<u64>,
    /// Preset the running server was started with, so the UI can
    /// restore the dropdown choice on page reload. Empty when no
    /// server is running.
    #[serde(default)]
    pub preset: String,
    /// args_template the running server was started with (only
    /// meaningful under preset=`custom`). Same UI-restore role.
    #[serde(default)]
    pub args_template: Vec<String>,
}

/// Resolve which `llama-server` binary actually runs for this start
/// request, in priority order:
///
///   1. Explicit `binary_path` from the request (operator-supplied,
///      BYO path) — passes through verbatim.
///   2. Aokie's installer-bundled binary at
///      `<resource_dir>/llama-server/llama-server.exe`. This is the
///      out-of-the-box path: `scripts/stage-llama-server.mjs` stages
///      it, `tauri.conf.json`'s bundle.resources ships it, and
///      `is_bundled_binary` recognises it for the env-var gate bypass.
///   3. Aokie's runtime-downloaded binary at
///      `<app_data>/llama-server/<exe>`. Surfaced when the install
///      didn't ship one (e.g. dev builds, or before the staging
///      script has run); `download_llama_server_bin` populates this.
///   4. Plain `llama-server` on PATH — final BYO fallback.
fn resolve_binary(
    resource_dir: Option<&Path>,
    app_data_dir: Option<&Path>,
    binary_path: &str,
) -> String {
    let trimmed = binary_path.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    let exe_name = if cfg!(target_os = "windows") {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    if let Some(rd) = resource_dir {
        let bundled = rd.join("llama-server").join(exe_name);
        if bundled.exists() {
            return bundled.to_string_lossy().to_string();
        }
    }
    if let Some(app_data) = app_data_dir {
        let downloaded = app_data.join("llama-server").join(exe_name);
        if downloaded.exists() {
            return downloaded.to_string_lossy().to_string();
        }
    }
    // PATH fallback — works in dev shells with llama.cpp on PATH.
    "llama-server".to_string()
}

fn endpoint_for(port: u16) -> String {
    format!("http://127.0.0.1:{}/v1", port)
}

/// Normalize a preset string to one of the `PRESET_*` constants
/// (or empty if the input is empty). ASCII-lowercases first so
/// "Custom" / "LLAMA-CPP" / leading-trailing whitespace round-trip
/// to the same canonical form the matchers compare against.
pub fn normalize_preset(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Build the argv (excluding the binary path itself) for a request,
/// expanding `{model}` / `{port}` / `{ngl}` / `{ctx}` placeholders
/// in templated entries. Pure function so it's easy to unit test.
pub fn build_argv(req: &StartLlamaServerRequest, port: u16) -> Vec<String> {
    let preset = normalize_preset(&req.preset);
    let preset = if preset.is_empty() {
        PRESET_LLAMA_CPP
    } else {
        preset.as_str()
    }
    .to_string();
    let mut argv: Vec<String> = match preset.as_str() {
        PRESET_CUSTOM => req
            .args_template
            .iter()
            .map(|raw| {
                expand_placeholders(raw, &req.model_path, port, req.n_gpu_layers, req.ctx_size)
            })
            .collect(),
        _ => {
            // llama-cpp default. Building from scratch — no template
            // substitution because the values are typed.
            let mut a = vec![
                "-m".to_string(),
                req.model_path.clone(),
                "--port".to_string(),
                port.to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
            ];
            if let Some(ngl) = req.n_gpu_layers {
                a.push("-ngl".to_string());
                a.push(ngl.to_string());
            }
            if let Some(ctx) = req.ctx_size {
                a.push("-c".to_string());
                a.push(ctx.to_string());
            }
            a
        }
    };
    for extra in &req.extra_args {
        argv.push(extra.clone());
    }
    argv
}

fn expand_placeholders(
    raw: &str,
    model: &str,
    port: u16,
    ngl: Option<i32>,
    ctx: Option<u32>,
) -> String {
    let mut out = raw.replace("{model}", model);
    out = out.replace("{port}", &port.to_string());
    if let Some(n) = ngl {
        out = out.replace("{ngl}", &n.to_string());
    }
    if let Some(c) = ctx {
        out = out.replace("{ctx}", &c.to_string());
    }
    out
}

/// Bind flags llama.cpp / vllm / koboldcpp / friends accept. Used both
/// for the `--flag=value` substring check inside `binds_non_localhost`
/// and the two-arg-form pass `argv_binds_non_localhost` does on the
/// full argv. Centralised so the two passes can't drift.
const BIND_FLAGS: &[&str] = &["--host", "--bind", "--listen", "--server-host", "--ip"];

/// True when `value` (the right-hand side of a bind flag) names a
/// loopback target. Treats Bash-style quoting and IPv6 brackets as
/// noise so `'"localhost"'` and `[::1]` round-trip to the loopback
/// branch instead of falling through as exotic.
fn bind_value_is_loopback(value: &str) -> bool {
    let v = value
        .trim()
        .trim_matches(['"', '\'', '[', ']'])
        .to_ascii_lowercase();
    v.starts_with("127.") || v == "::1" || v == "localhost"
}

/// Return true when a single `arg` looks like an attempt to bind the
/// sidecar's listener to a non-loopback interface as a *self-contained
/// token*: bare `0.0.0.0`, the `--host=lan-ip` "joined" form, etc.
/// Doesn't see two-arg forms (`--host 1.2.3.4`) — those are caught by
/// `argv_binds_non_localhost`'s pairwise pass.
fn binds_non_localhost(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    let bad_hosts = ["0.0.0.0", "[::]", "::0", "::", "*"];
    if bad_hosts.iter().any(|bad| {
        lower == *bad
            || lower.ends_with(&format!("={}", bad))
            || lower.ends_with(&format!(":{}", bad))
    }) {
        return true;
    }
    for flag in BIND_FLAGS {
        let pfx = format!("{}=", flag);
        if let Some(value) = lower.strip_prefix(&pfx) {
            if !bind_value_is_loopback(value) {
                return true;
            }
        }
    }
    false
}

/// Walk the assembled argv and return the offending entry whenever a
/// bind argument names a non-loopback target. Catches three shapes:
///
///   1. Self-contained token (`0.0.0.0`, `--host=lan-ip`) — delegated
///      to `binds_non_localhost`.
///   2. Two-arg form (`--host`, `1.2.3.4`) — the original
///      `binds_non_localhost` missed this entirely; the reviewer
///      flagged it as a release blocker because llama.cpp et al
///      accept this shape natively.
///   3. Two-arg form with a literal `0.0.0.0` value (`--host`,
///      `0.0.0.0`) — already caught by the shape-1 check on the
///      value entry, but we report under the flag for a clearer error.
///
/// Returns `Some(arg)` for the offending entry. Caller turns that into
/// the user-facing refusal message.
fn argv_binds_non_localhost(argv: &[String]) -> Option<&str> {
    for (i, arg) in argv.iter().enumerate() {
        if binds_non_localhost(arg) {
            return Some(arg.as_str());
        }
        // Two-arg form: this entry is exactly a bind flag (no `=`),
        // and the next entry is its value. Only flag a non-loopback
        // value — leaving the flag intact alone (e.g. forgotten value
        // at end of argv) is the user's problem to debug, not a
        // security violation.
        let lower = arg.to_ascii_lowercase();
        if BIND_FLAGS.iter().any(|f| lower == *f) {
            if let Some(next) = argv.get(i + 1) {
                if !bind_value_is_loopback(next) {
                    return Some(arg.as_str());
                }
            }
        }
    }
    None
}

/// Sidecar launching is power-user territory — picking an arbitrary
/// binary on disk and feeding it custom CLI args is the kind of thing
/// the reviewer flagged as "okay for power users, risky for mainstream
/// users". Gate it behind an opt-in env var so a fresh install doesn't
/// expose a "browse to any binary" panel out of the box. The Settings
/// page can flip this on once the user clicks an "Advanced / BYO local
/// server" checkbox; until then start/stop refuse cleanly.
fn advanced_mode_enabled() -> bool {
    matches!(
        std::env::var("AOKIE_ENABLE_SIDECAR").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

/// `true` when the resolved binary path lives inside one of Aokie's
/// own llama-server dirs:
///
///   - `<resource_dir>/llama-server/`  — installer-bundled (default,
///     OOTB path; staged by scripts/stage-llama-server.mjs).
///   - `<app_data>/llama-server/`      — runtime-downloaded fallback
///     (dev builds, missing-bundle recovery, etc.) populated by
///     `download_llama_server_bin`.
///
/// Both layouts are sha-verified at install / fetch time, so the
/// "arbitrary binary on disk" risk that motivated the env-var gate
/// doesn't apply. Letting the start command bypass
/// `AOKIE_ENABLE_SIDECAR` for these paths is what makes the Qwen 3.5
/// 4B catalogue entry work out of the box.
fn is_bundled_binary(
    resource_dir: Option<&Path>,
    app_data_dir: Option<&Path>,
    binary_path: &str,
) -> bool {
    let candidate = std::path::Path::new(binary_path);
    if let Some(rd) = resource_dir {
        if candidate.starts_with(rd.join("llama-server")) {
            return true;
        }
    }
    if let Some(app_data) = app_data_dir {
        if candidate.starts_with(app_data.join("llama-server")) {
            return true;
        }
    }
    false
}

/// Spawn the sidecar. Tauri-free: the caller owns the
/// [`LlamaServerState`] and supplies the resource/app-data dirs used to
/// resolve the bundled binary. Consent gating (which the legacy app did
/// via a Tauri command wrapper) is the caller's responsibility now.
pub async fn llama_server_start(
    state: &LlamaServerState,
    resource_dir: Option<&Path>,
    app_data_dir: Option<&Path>,
    request: StartLlamaServerRequest,
) -> Result<LlamaServerStatus, String> {
    // Resolve the path FIRST so the bundled-binary bypass works for the
    // common case where the request leaves binary_path empty. Without
    // this order, `is_bundled_binary("")` always returns false and the
    // autostart hit the "advanced feature" error instead of resolving
    // to the installer-bundled exe.
    let resolved_binary = resolve_binary(resource_dir, app_data_dir, &request.binary_path);
    let bundled = is_bundled_binary(resource_dir, app_data_dir, &resolved_binary);
    if !bundled && !advanced_mode_enabled() {
        return Err(
            "Local LLM sidecar is an advanced feature for arbitrary binaries. \
             Set AOKIE_ENABLE_SIDECAR=1 to enable, or pick the bundled Qwen 3.5 4B \
             entry from the model catalogue (which downloads + uses Aokie's own \
             sha-verified llama-server build)."
                .to_string(),
        );
    }
    aokie_core::redact::audit(
        "llama_server_start",
        format!(
            "binary={:?} model={:?} port={} preset={:?}",
            request.binary_path, request.model_path, request.port, request.preset
        ),
    );
    let inner = state.inner.clone();
    {
        // Trim down any zombie that already exited so a "start"
        // after a crash actually starts a new one.
        let mut guard = inner.lock().map_err(|e| format!("state poisoned: {}", e))?;
        if let Some(running) = guard.as_mut() {
            match running.child.try_wait() {
                Ok(Some(_status)) => {
                    *guard = None;
                }
                Ok(None) => {
                    // Still alive — tell the caller and bail.
                    return Err(format!(
                        "llama-server is already running on port {} (PID {}).",
                        running.port,
                        running.child.id()
                    ));
                }
                Err(e) => {
                    return Err(format!("failed to probe llama-server: {}", e));
                }
            }
        }
    }

    let binary = resolved_binary;
    let port = if request.port == 0 {
        DEFAULT_PORT
    } else {
        request.port
    };

    if request.model_path.trim().is_empty() {
        return Err("model_path is required".to_string());
    }
    let normalized_preset = normalize_preset(&request.preset);
    if !normalized_preset.is_empty()
        && normalized_preset != PRESET_LLAMA_CPP
        && normalized_preset != PRESET_CUSTOM
    {
        return Err(format!(
            "unknown preset {:?} — supported: {:?} / {:?}.",
            request.preset, PRESET_LLAMA_CPP, PRESET_CUSTOM
        ));
    }
    if normalized_preset == PRESET_CUSTOM && request.args_template.is_empty() {
        return Err(
            "preset=\"custom\" requires args_template — list at least the binary's required flags."
                .to_string(),
        );
    }

    // R3-#11: random per-session token. Only auto-injected for the
    // llama-cpp preset because that's the only path where we know
    // the binary accepts `--api-key`. Operators using preset=custom
    // are responsible for their own auth — the args_template can
    // include `--api-key {token}` if they want to opt in (placeholder
    // not currently expanded; explicit hard-coded keys work too).
    let session_token = if normalize_preset(&request.preset) != PRESET_CUSTOM {
        Some(generate_session_token())
    } else {
        None
    };

    let mut argv = build_argv(&request, port);
    if let Some(ref token) = session_token {
        argv.push("--api-key".to_string());
        argv.push(token.clone());
    }
    if let Some(bad) = argv_binds_non_localhost(&argv) {
        return Err(format!(
            "refusing to spawn sidecar with non-localhost bind argument {:?}. \
             The receptionist runs the sidecar for in-process LLM only — exposing \
             the endpoint to the network would let other hosts on your LAN make \
             unauthenticated LLM requests.",
            bad
        ));
    }
    let resolved_preset = if normalized_preset.is_empty() {
        PRESET_LLAMA_CPP.to_string()
    } else {
        normalized_preset.clone()
    };
    let mut cmd = Command::new(&binary);
    for arg in &argv {
        cmd.arg(arg);
    }
    // Inherit stdout so the user can see startup progress in the same
    // terminal the dev build runs from. stderr — where llama.cpp prints
    // its load log AND its failure reason (CUDA OOM, bind error, bad
    // GGUF) — is piped so a tee thread can both echo it to the console
    // AND retain the tail, letting a "child exited" failure quote the
    // real cause instead of guessing. Stdin is closed (Stdio::null())
    // so a stuck sidecar can't read from the parent's stdin and hang.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    // On Windows, spawning a console binary from a windowless GUI
    // parent flashes a console window for a frame before redirection
    // takes hold. CREATE_NO_WINDOW (0x0800_0000) suppresses it.
    // No-op on dev builds where the parent already has a console
    // (the existing console is reused, this flag is ignored).
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd.spawn().map_err(|e| {
        format!(
            "spawn {:?}: {}. Install llama.cpp / your chosen server and either put the binary on PATH or set binary_path explicitly.",
            binary, e
        )
    })?;
    let pid = child.id();
    // Tee the child's stderr: a background thread drains the pipe (so a
    // chatty sidecar can't deadlock on a full buffer), echoes each line
    // to our own stderr to preserve the dev-console view, and retains
    // the last STDERR_TAIL_CAP lines for failure diagnostics.
    let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::with_capacity(STDERR_TAIL_CAP)));
    if let Some(child_stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        let _ = std::thread::Builder::new()
            .name(format!("llama-stderr-{}", pid))
            .spawn(move || {
                let reader = BufReader::new(child_stderr);
                let err = std::io::stderr();
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    // Echo to the console (best-effort; windowless
                    // packaged builds have nowhere to write — ignore).
                    {
                        let mut handle = err.lock();
                        let _ = writeln!(handle, "{}", line);
                    }
                    if let Ok(mut buf) = tail.lock() {
                        if buf.len() == STDERR_TAIL_CAP {
                            buf.pop_front();
                        }
                        buf.push_back(line);
                    }
                }
            });
    }
    let running = RunningServer {
        child,
        binary_path: binary.clone(),
        model_path: request.model_path.clone(),
        port,
        started_at: Instant::now(),
        preset: resolved_preset.clone(),
        args_template: request.args_template.clone(),
        stderr_tail,
    };
    {
        let mut guard = inner.lock().map_err(|e| format!("state poisoned: {}", e))?;
        *guard = Some(running);
    }
    // R3-#11: publish the session token to the process-wide handle so
    // the LLM adapter picks it up on the next request. Done after the
    // spawn confirms — a failed spawn shouldn't leave a stale token
    // around for the next attempt.
    set_session_token(session_token.clone());
    // Redact the api-key from the spawn log so a copy-pasted
    // diagnostic dump doesn't leak the bearer token. The rest of the
    // argv is already public (model path + port + ngl + ctx size).
    let argv_redacted = redact_api_key_arg(&argv);
    println!(
        "[llama-server] spawned PID {} ({:?}, model={:?}, port={}, argv={:?})",
        pid, binary, request.model_path, port, argv_redacted
    );
    Ok(LlamaServerStatus {
        running: true,
        base_url: endpoint_for(port),
        port,
        binary_path: binary,
        model_path: request.model_path,
        uptime_secs: Some(0),
        preset: resolved_preset,
        args_template: request.args_template,
    })
}

pub async fn llama_server_stop(state: &LlamaServerState) -> Result<(), String> {
    let inner = state.inner.clone();
    let mut running = {
        let mut guard = inner.lock().map_err(|e| format!("state poisoned: {}", e))?;
        match guard.take() {
            Some(r) => r,
            None => return Ok(()), // already stopped — idempotent
        }
    };
    let pid = running.child.id();
    if let Err(e) = running.child.kill() {
        // ESRCH means the process already exited — fine, treat as
        // stopped. Any other error gets surfaced.
        if e.kind() != std::io::ErrorKind::InvalidInput && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!("kill llama-server (PID {}): {}", pid, e));
        }
    }
    // Reap so the process table doesn't grow zombies if the user
    // start/stops repeatedly during a session.
    let _ = running.child.wait();
    // R3-#11: clear the published session token. The next adapter
    // request will see `current_session_token() == None` and skip
    // the bearer header — falling through to the operator-supplied
    // api_key (or no auth, when empty), same as before this change.
    set_session_token(None);
    println!("[llama-server] stopped PID {}", pid);
    Ok(())
}

pub async fn llama_server_status(state: &LlamaServerState) -> Result<LlamaServerStatus, String> {
    let inner = state.inner.clone();
    let mut guard = inner.lock().map_err(|e| format!("state poisoned: {}", e))?;
    let Some(running) = guard.as_mut() else {
        return Ok(LlamaServerStatus {
            running: false,
            base_url: endpoint_for(DEFAULT_PORT),
            port: DEFAULT_PORT,
            binary_path: String::new(),
            model_path: String::new(),
            uptime_secs: None,
            preset: String::new(),
            args_template: Vec::new(),
        });
    };
    // Probe — if the child exited (crashed, OOM, model couldn't load)
    // we want the status to reflect that rather than perpetually
    // saying "running". Clear the slot so the next start actually
    // spawns.
    match running.child.try_wait() {
        Ok(Some(_status)) => {
            let port = running.port;
            let binary = running.binary_path.clone();
            let model = running.model_path.clone();
            let preset = running.preset.clone();
            let args_template = running.args_template.clone();
            *guard = None;
            return Ok(LlamaServerStatus {
                running: false,
                base_url: endpoint_for(port),
                port,
                binary_path: binary,
                model_path: model,
                uptime_secs: None,
                preset,
                args_template,
            });
        }
        Ok(None) => { /* still alive */ }
        Err(e) => {
            return Err(format!("probe llama-server: {}", e));
        }
    }
    let uptime = running.started_at.elapsed();
    Ok(LlamaServerStatus {
        running: true,
        base_url: endpoint_for(running.port),
        port: running.port,
        binary_path: running.binary_path.clone(),
        model_path: running.model_path.clone(),
        uptime_secs: Some(uptime.as_secs()),
        preset: running.preset.clone(),
        args_template: running.args_template.clone(),
    })
}

/// Auto-spawn the bundled `llama-server` when the active LLM provider
/// config asks for it. The Aokie warmup phase calls this exactly once
/// per launch, after STT/TTS init, so that picking the Qwen 3.5 4B
/// catalogue entry "just works" without a manual Start in the
/// LlamaServer panel.
///
/// Behaviour:
///   * `llm.kind != "llama-server"` → returns the current status as a
///     no-op. The warmup phase calls this unconditionally and we don't
///     want to error for the (common) Gemma case.
///   * Already-running with the same model → returns existing status,
///     untouched. Idempotent across page reloads / hot restarts.
///   * Already-running with a *different* model (operator switched
///     quants) → kill + respawn under the new args.
///   * Model GGUF not on disk → fail-loud with the exact path and a
///     pointer at the AI Stack download flow.
///
/// Readiness is gated on a successful `GET /v1/models` against the
/// resolved port — TCP-listen alone isn't enough since the loader is
/// holding the port open while it mmap's the GGUF. Cap is 30s, which
/// covers Qwen 3.5 4B Q4_K_M cold-load on both a fast SSD and a CUDA
/// upload.
pub async fn llama_server_autostart(
    state: &LlamaServerState,
    resource_dir: Option<&Path>,
    app_data_dir: &Path,
) -> Result<LlamaServerStatus, String> {
    let active = crate::config::active();
    let llm = active.llm.clone();
    if llm.kind != "llama-server" {
        return llama_server_status(state).await;
    }
    if llm.model.trim().is_empty() {
        return Err(
            "Active LLM provider has no GGUF filename set. Pick a model in AI Stack first."
                .to_string(),
        );
    }
    let models_dir = crate::bundled_models::locate_models_dir(
        app_data_dir,
        resource_dir,
        // Resolve the dir from the active model so MiniCPM5
        // (models/minicpm5/) and Qwen (models/qwen35_4b/) each load from
        // their own dir. Empty / unknown names default to Qwen35_4b.
        crate::bundled_models::llama_server_role_for_model(&llm.model),
    )?;
    let model_path = models_dir.join(&llm.model);
    if !model_path.exists() {
        return Err(format!(
            "Model file not found at {}. Open AI Stack, pick a GGUF LLM entry (Qwen 3.5 4B or MiniCPM5), and click Save to trigger the download.",
            model_path.display(),
        ));
    }
    let model_path_str = model_path.to_string_lossy().to_string();

    {
        let inner = state.inner.clone();
        let mut guard = inner.lock().map_err(|e| format!("state poisoned: {}", e))?;
        if let Some(running) = guard.as_mut() {
            match running.child.try_wait() {
                Ok(None) if running.model_path == model_path_str => {
                    let uptime = running.started_at.elapsed();
                    return Ok(LlamaServerStatus {
                        running: true,
                        base_url: endpoint_for(running.port),
                        port: running.port,
                        binary_path: running.binary_path.clone(),
                        model_path: running.model_path.clone(),
                        uptime_secs: Some(uptime.as_secs()),
                        preset: running.preset.clone(),
                        args_template: running.args_template.clone(),
                    });
                }
                Ok(None) => {
                    let pid = running.child.id();
                    let _ = running.child.kill();
                    let _ = running.child.wait();
                    println!(
                        "[llama-server] autostart: model changed ({} → {}); killed PID {}",
                        running.model_path, model_path_str, pid,
                    );
                    *guard = None;
                }
                _ => {
                    *guard = None;
                }
            }
        }
    }

    // Pick the port: start from the configured base_url's port (or the
    // default) and probe upward for the first free one, so a second local
    // project squatting on 8081 doesn't break the sidecar. The in-process
    // HTTP client is then pointed at whatever we land on.
    let start_port = port_from_base_url(&llm.base_url).unwrap_or(DEFAULT_PORT);
    let port = find_free_port(start_port);
    if port != start_port {
        println!(
            "[llama-server] port {} busy — using {} instead",
            start_port, port
        );
    }

    let request = StartLlamaServerRequest {
        binary_path: String::new(),
        model_path: model_path_str,
        port,
        // Offload everything to GPU under the CUDA build; the CPU
        // build leaves -ngl unset so llama.cpp keeps tensors on host.
        // 99 is llama.cpp's idiomatic "all layers" value.
        n_gpu_layers: if cfg!(feature = "cuda") {
            Some(99)
        } else {
            None
        },
        ctx_size: llm.max_context_tokens,
        extra_args: Vec::new(),
        preset: PRESET_LLAMA_CPP.to_string(),
        args_template: Vec::new(),
    };
    let status = llama_server_start(state, resource_dir, Some(app_data_dir), request).await?;
    // Point the in-process HTTP client (`HttpOpenAiLlm`) at the port the
    // sidecar actually bound — it may differ from the configured one
    // after the auto-probe above. In-memory only; the saved config keeps
    // the operator's value and the probe re-runs next launch.
    crate::config::set_active_llm_base_url(endpoint_for(port));
    wait_for_ready(state, &status.base_url, std::time::Duration::from_secs(30)).await?;
    Ok(status)
}

/// Poll `<base_url>/models` until it returns 2xx or `timeout` elapses.
/// llama.cpp's `llama-server` binds the port early (during `socket(2)`)
/// but only answers HTTP after the GGUF has finished loading, so
/// hitting `/models` is the right liveness check — not a TCP connect.
///
/// Also probes our spawned child between HTTP polls. If the child has
/// exited mid-startup (most commonly: bind failure when an orphan is
/// already squatting on the port), bail out with a clear error instead
/// of waiting for the timeout. Without this check, a successful HTTP
/// response from the orphan would be reported as our child being ready
/// — and chat requests would silently route to whichever build of
/// llama-server happened to leak from the previous Aokie session.
/// Build the user-facing error for a sidecar that exited during
/// startup, quoting llama.cpp's own last words when we captured them.
/// The tail almost always names the real cause (`CUDA error: out of
/// memory`, `bind: address already in use`, `error loading model`), so
/// it leads; the generic checklist is a fallback for the rare case the
/// child died before printing anything.
fn exit_failure_message(code: Option<i32>, stderr_tail: &[String]) -> String {
    let code_str = match code {
        Some(c) => format!("exit code {}", c),
        None => "terminated".to_string(),
    };
    // Keep the last few non-blank lines — that's where the fatal error
    // sits. More than ~12 lines is just startup banner noise.
    let tail: Vec<&str> = stderr_tail
        .iter()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect();
    let quoted = if tail.is_empty() {
        String::new()
    } else {
        let start = tail.len().saturating_sub(12);
        format!("\n\nLast output from llama-server:\n{}", tail[start..].join("\n"))
    };
    format!(
        "llama-server exited during startup ({code_str}).{quoted}\n\n\
         Common causes: GPU out of memory (close other GPU apps, or the active \
         model is too large for your VRAM), another server already bound to the \
         port (run `taskkill /F /IM llama-server.exe`), missing CUDA runtime DLLs \
         on PATH, or a corrupt model file."
    )
}

async fn wait_for_ready(
    state: &LlamaServerState,
    base_url: &str,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|e| format!("reqwest client: {}", e))?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match state.liveness() {
            Liveness::Alive => {}
            Liveness::Exited { code, stderr_tail } => {
                return Err(exit_failure_message(code, &stderr_tail));
            }
            Liveness::NotRunning => {
                // Reaped by a concurrent status poll before we could
                // grab the exit detail — still a startup failure, just
                // without the captured tail.
                return Err(exit_failure_message(None, &[]));
            }
        }
        if let Ok(resp) = client.get(&endpoint).send().await {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "llama-server didn't become ready on {} within {:?}",
                base_url, timeout,
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Best-effort cleanup at app shutdown — Tauri's window-close hook
/// calls into this so the user doesn't have to manually kill
/// llama-server when they quit the app.
pub fn shutdown_blocking(state: &LlamaServerState) {
    let inner = state.inner.clone();
    let mut guard = match inner.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(mut running) = guard.take() {
        let pid = running.child.id();
        let _ = running.child.kill();
        let _ = running.child.wait();
        println!("[llama-server] shutdown: killed PID {}", pid);
    }
    // R3-#11: ditto on shutdown — the next launch generates a fresh
    // token, but a stale value leaking across runs in the same
    // process (e.g. a tauri::generate_context restart hook) would be
    // the wrong shape.
    set_session_token(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(preset: &str) -> StartLlamaServerRequest {
        StartLlamaServerRequest {
            binary_path: String::new(),
            model_path: "/models/foo.gguf".to_string(),
            port: 8081,
            n_gpu_layers: None,
            ctx_size: None,
            extra_args: Vec::new(),
            preset: preset.to_string(),
            args_template: Vec::new(),
        }
    }

    #[test]
    fn llama_cpp_preset_emits_default_flags() {
        let argv = build_argv(&req("llama-cpp"), 8081);
        assert_eq!(
            argv,
            vec![
                "-m",
                "/models/foo.gguf",
                "--port",
                "8081",
                "--host",
                "127.0.0.1"
            ]
        );
    }

    #[test]
    fn empty_preset_defaults_to_llama_cpp() {
        let argv = build_argv(&req(""), 8081);
        assert_eq!(argv[0], "-m");
        assert_eq!(argv[1], "/models/foo.gguf");
    }

    #[test]
    fn llama_cpp_appends_optional_flags() {
        let mut r = req("llama-cpp");
        r.n_gpu_layers = Some(99);
        r.ctx_size = Some(4096);
        let argv = build_argv(&r, 8081);
        assert!(argv.windows(2).any(|w| w == ["-ngl", "99"]));
        assert!(argv.windows(2).any(|w| w == ["-c", "4096"]));
    }

    #[test]
    fn custom_preset_substitutes_placeholders() {
        let mut r = req("custom");
        r.args_template = vec![
            "serve".to_string(),
            "--model".to_string(),
            "{model}".to_string(),
            "--port".to_string(),
            "{port}".to_string(),
        ];
        let argv = build_argv(&r, 9090);
        assert_eq!(
            argv,
            vec!["serve", "--model", "/models/foo.gguf", "--port", "9090"]
        );
    }

    #[test]
    fn extra_args_append_under_any_preset() {
        let mut r = req("llama-cpp");
        r.extra_args = vec!["--threads".to_string(), "8".to_string()];
        let argv = build_argv(&r, 8081);
        assert_eq!(
            &argv[argv.len() - 2..],
            &["--threads".to_string(), "8".to_string()]
        );
    }

    #[test]
    fn build_argv_unknown_preset_falls_through_to_llama_cpp() {
        // build_argv itself is permissive — the unknown-preset
        // gate lives in `llama_server_start`. The test pins the
        // current default-arm behaviour so we notice if it shifts.
        let argv = build_argv(&req("llama-cpx"), 8081);
        assert_eq!(argv[0], "-m");
    }

    #[test]
    fn capitalized_custom_preset_normalizes() {
        // "Custom" / "CUSTOM" / leading-trailing whitespace should
        // all funnel into the custom branch instead of silently
        // falling into the llama-cpp default.
        let mut r = req("  Custom ");
        r.args_template = vec!["x".to_string(), "{port}".to_string()];
        let argv = build_argv(&r, 9090);
        assert_eq!(argv, vec!["x", "9090"]);
    }

    #[test]
    fn normalize_preset_lowers_and_trims() {
        assert_eq!(normalize_preset("LLAMA-CPP"), "llama-cpp");
        assert_eq!(normalize_preset(" Custom "), "custom");
        assert_eq!(normalize_preset(""), "");
    }

    #[test]
    fn binds_non_localhost_flags_zero_zero_zero_zero() {
        assert!(binds_non_localhost("0.0.0.0"));
        assert!(binds_non_localhost("--host=0.0.0.0"));
        assert!(binds_non_localhost("--bind=192.168.1.4"));
        assert!(binds_non_localhost("--listen=[::]"));
    }

    #[test]
    fn binds_non_localhost_allows_loopback() {
        assert!(!binds_non_localhost("--host=127.0.0.1"));
        assert!(!binds_non_localhost("--host=localhost"));
        assert!(!binds_non_localhost("--listen=::1"));
        assert!(!binds_non_localhost("127.0.0.1"));
    }

    #[test]
    fn binds_non_localhost_lets_normal_flags_pass() {
        assert!(!binds_non_localhost("-m"));
        assert!(!binds_non_localhost("/models/foo.gguf"));
        assert!(!binds_non_localhost("--port"));
        assert!(!binds_non_localhost("8081"));
        assert!(!binds_non_localhost("--threads"));
        assert!(!binds_non_localhost("8"));
    }

    #[test]
    fn argv_binds_non_localhost_catches_two_arg_lan_ip() {
        // Reviewer flagged this as a release blocker: the per-token
        // `binds_non_localhost` missed `--host 1.2.3.4` because each
        // entry on its own looks innocent. The argv-walking variant
        // pairs flag + value and catches it.
        let argv: Vec<String> = ["--host", "192.168.1.50"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(argv_binds_non_localhost(&argv), Some("--host"));

        let argv: Vec<String> = ["--listen", "10.0.0.5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(argv_binds_non_localhost(&argv), Some("--listen"));
    }

    #[test]
    fn argv_binds_non_localhost_catches_two_arg_zero_zero() {
        // Same shape, but the value is the wildcard. Shape-1 check on
        // the value already catches this — confirming so a future
        // refactor that drops the per-token check still leaves us
        // protected via the pairwise pass.
        let argv: Vec<String> = ["--host", "0.0.0.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(argv_binds_non_localhost(&argv).is_some());
    }

    #[test]
    fn argv_binds_non_localhost_allows_two_arg_loopback() {
        let argv: Vec<String> = ["--host", "127.0.0.1", "--port", "8081"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(argv_binds_non_localhost(&argv), None);

        let argv: Vec<String> = ["--listen", "localhost"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(argv_binds_non_localhost(&argv), None);

        let argv: Vec<String> = ["--bind", "::1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(argv_binds_non_localhost(&argv), None);
    }

    #[test]
    fn argv_binds_non_localhost_allows_unrelated_two_arg_flags() {
        // Make sure the pairwise pass doesn't gobble innocent
        // flag-value pairs (e.g. `--threads 8`, `--ctx-size 4096`).
        let argv: Vec<String> = [
            "-m",
            "/m.gguf",
            "--port",
            "8081",
            "--threads",
            "8",
            "--ctx-size",
            "4096",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(argv_binds_non_localhost(&argv), None);
    }

    #[test]
    fn argv_binds_non_localhost_handles_quoted_values() {
        // Some templates wrap host values in quotes. The loopback
        // check strips them so the legitimate localhost form isn't
        // mis-flagged.
        let argv: Vec<String> = ["--host", "\"127.0.0.1\""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(argv_binds_non_localhost(&argv), None);
    }

    /* =====================================================================
     * R3-#11: session-token plumbing.
     * ================================================================= */

    #[test]
    fn generate_session_token_returns_64_lowercase_hex_chars() {
        let t = generate_session_token();
        assert_eq!(t.len(), 64, "32 bytes → 64 hex chars: {}", t);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "should be lowercase hex: {}",
            t
        );
        // Two consecutive draws should not be equal — otherwise the
        // RNG is broken or seeded wrong.
        let t2 = generate_session_token();
        assert_ne!(t, t2, "consecutive draws should differ");
    }

    #[test]
    fn redact_api_key_arg_swaps_only_the_key_value() {
        let argv: Vec<String> = [
            "-m",
            "/models/foo.gguf",
            "--port",
            "8081",
            "--api-key",
            "deadbeefcafe",
            "-c",
            "4096",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let redacted = redact_api_key_arg(&argv);
        assert_eq!(redacted[4], "--api-key");
        assert_eq!(redacted[5], "<redacted>");
        // Everything else round-trips untouched.
        for i in [0, 1, 2, 3, 6, 7] {
            assert_eq!(redacted[i], argv[i], "non-key arg {} mutated", i);
        }
    }

    #[test]
    fn redact_api_key_arg_no_op_when_no_key() {
        let argv: Vec<String> = ["-m", "/models/foo.gguf", "--port", "8081"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let redacted = redact_api_key_arg(&argv);
        assert_eq!(redacted, argv);
    }

    /* =====================================================================
     * Startup-failure diagnostics: quote the child's real stderr.
     * ================================================================= */

    #[test]
    fn exit_failure_message_quotes_stderr_tail_and_code() {
        let tail = vec![
            "load_tensors: offloading 36 layers to GPU".to_string(),
            "ggml_cuda_host_malloc: failed to allocate".to_string(),
            "CUDA error: out of memory".to_string(),
        ];
        let msg = exit_failure_message(Some(1), &tail);
        assert!(msg.contains("exit code 1"), "code missing: {msg}");
        assert!(
            msg.contains("CUDA error: out of memory"),
            "real cause not quoted: {msg}"
        );
        assert!(
            msg.contains("Last output from llama-server:"),
            "tail header missing: {msg}"
        );
        // The actionable checklist still rides along as a fallback.
        assert!(msg.contains("GPU out of memory"), "checklist missing: {msg}");
    }

    #[test]
    fn exit_failure_message_handles_empty_tail() {
        // Child died before printing anything (or the tail was reaped by
        // a racing status poll) — no "Last output" block, just the code
        // and the generic checklist.
        let msg = exit_failure_message(None, &[]);
        assert!(msg.contains("terminated"), "no code → 'terminated': {msg}");
        assert!(
            !msg.contains("Last output"),
            "should omit empty tail block: {msg}"
        );
        assert!(msg.contains("Common causes"), "checklist missing: {msg}");
    }

    #[test]
    fn exit_failure_message_caps_tail_to_last_lines() {
        // Feed 30 lines; only the last 12 should survive into the
        // message so the startup banner doesn't drown the real error.
        let tail: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let msg = exit_failure_message(Some(2), &tail);
        assert!(msg.contains("line 29"), "newest line dropped: {msg}");
        assert!(msg.contains("line 18"), "12th-from-last dropped: {msg}");
        assert!(
            !msg.contains("line 17"),
            "older-than-12 line leaked in: {msg}"
        );
    }
}
