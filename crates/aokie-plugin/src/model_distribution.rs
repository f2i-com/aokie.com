//! Verified, resumable distribution for the local receptionist models.
//!
//! The release carries a small manifest, not ~800 MB of weights. On startup we
//! download from immutable upstream revisions into a staging directory, retain
//! partial files for Range-based resume, verify every declared byte count and
//! SHA-256 digest, then promote the complete directory. The radio cannot arm
//! auto-answer until this module and the measured voice self-test both pass.

use reqwest::blocking::Client;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_RANGE, RANGE};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const MANIFEST_JSON: &str = include_str!("../../../docs/models-manifest.json");

#[derive(Debug, Default)]
pub struct ModelInstallReport {
    pub stt_error: Option<String>,
    pub tts_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    format: u32,
    bundles: Vec<Bundle>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bundle {
    name: String,
    source: Source,
    files: Vec<ModelFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    revision: String,
    base_url: String,
    #[serde(default)]
    path_prefix: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelFile {
    path: String,
    #[serde(default)]
    source_path: Option<String>,
    sha256: String,
    size: u64,
}

/// Ensure all local model bundles needed by the configured speech engines are
/// present and verified. HTTP STT/TTS endpoints deliberately suppress the
/// corresponding local download.
pub fn ensure_required_models() -> ModelInstallReport {
    let stt_remote = nonempty_env("AOKIE_STT_ENDPOINT");
    let tts_remote = nonempty_env("AOKIE_TTS_ENDPOINT");
    let app_data = match aokie_core::paths::app_data_dir() {
        Some(path) => path,
        None => {
            let message = "model installer: the application data directory is unavailable";
            return ModelInstallReport {
                stt_error: (!stt_remote).then(|| message.to_string()),
                tts_error: (!tts_remote).then(|| message.to_string()),
            };
        }
    };

    let manifest = match parse_manifest() {
        Ok(manifest) => manifest,
        Err(error) => {
            let message = format!("model installer manifest is invalid: {error}");
            return ModelInstallReport {
                stt_error: (!stt_remote).then(|| message.clone()),
                tts_error: (!tts_remote).then_some(message),
            };
        }
    };
    let downloads_disabled = std::env::var("AOKIE_DISABLE_MODEL_DOWNLOAD").as_deref() == Ok("1");
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .user_agent(concat!("aokie-plugin/", env!("CARGO_PKG_VERSION")))
        .build();

    let mut report = ModelInstallReport::default();
    for bundle in &manifest.bundles {
        let slot = match bundle.name.as_str() {
            "parakeet" if !stt_remote => &mut report.stt_error,
            "pocket_tts_onnx" if !tts_remote => &mut report.tts_error,
            "parakeet" | "pocket_tts_onnx" => continue,
            _ => continue,
        };
        let target = app_data.join("models").join(&bundle.name);
        if verify_bundle(&target, bundle).is_ok() {
            continue;
        }
        if downloads_disabled {
            *slot = Some(format!(
                "{} model files are absent or corrupt and automatic downloads are disabled",
                bundle.name
            ));
            continue;
        }
        let result = match &client {
            Ok(client) => install_bundle(client, &app_data, bundle),
            Err(error) => Err(format!("create HTTPS client: {error}")),
        };
        if let Err(error) = result {
            *slot = Some(format!(
                "{} model installation failed: {error}",
                bundle.name
            ));
        }
    }
    report
}

fn nonempty_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

fn parse_manifest() -> Result<Manifest, String> {
    let manifest: Manifest =
        serde_json::from_str(MANIFEST_JSON).map_err(|error| error.to_string())?;
    if manifest.format != 2 {
        return Err(format!(
            "unsupported format {} (expected 2)",
            manifest.format
        ));
    }
    for bundle in &manifest.bundles {
        safe_relative_path(&bundle.name)?;
        if bundle.source.revision.len() != 40
            || !bundle
                .source
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!("{} has a non-immutable revision", bundle.name));
        }
        let base = reqwest::Url::parse(&bundle.source.base_url)
            .map_err(|error| format!("{} source URL: {error}", bundle.name))?;
        if base.scheme() != "https" || !base.as_str().contains(&bundle.source.revision) {
            return Err(format!(
                "{} source must be HTTPS and contain its pinned revision",
                bundle.name
            ));
        }
        for file in &bundle.files {
            safe_relative_path(&file.path)?;
            if let Some(source_path) = &file.source_path {
                safe_url_path(source_path)?;
            }
            if file.sha256.len() != 64 || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(format!(
                    "{}:{} has an invalid SHA-256",
                    bundle.name, file.path
                ));
            }
        }
    }
    Ok(manifest)
}

fn install_bundle(client: &Client, app_data: &Path, bundle: &Bundle) -> Result<(), String> {
    let models = app_data.join("models");
    fs::create_dir_all(&models).map_err(|error| format!("create models directory: {error}"))?;
    let _lock = DownloadLock::acquire(&models, &bundle.name)?;
    let target = models.join(&bundle.name);
    if verify_bundle(&target, bundle).is_ok() {
        return Ok(());
    }

    let revision = &bundle.source.revision[..12];
    let staging = models.join(format!(".download-{}-{revision}", bundle.name));
    fs::create_dir_all(&staging).map_err(|error| format!("create staging directory: {error}"))?;
    for file in &bundle.files {
        let relative = safe_relative_path(&file.path)?;
        let destination = staging.join(relative);
        if verify_file(&destination, file).is_ok() {
            continue;
        }
        download_file(client, bundle, file, &destination)?;
    }
    verify_bundle(&staging, bundle)?;

    let backup = models.join(format!(".previous-{}-{}", bundle.name, std::process::id()));
    if backup.exists() {
        fs::remove_dir_all(&backup).map_err(|error| format!("remove stale backup: {error}"))?;
    }
    let had_target = target.exists();
    if had_target {
        fs::rename(&target, &backup).map_err(|error| format!("quarantine old bundle: {error}"))?;
    }
    if let Err(error) = fs::rename(&staging, &target) {
        if had_target {
            let _ = fs::rename(&backup, &target);
        }
        return Err(format!("promote verified bundle: {error}"));
    }
    if had_target {
        let _ = fs::remove_dir_all(&backup);
    }
    verify_bundle(&target, bundle)
}

fn download_file(
    client: &Client,
    bundle: &Bundle,
    file: &ModelFile,
    destination: &Path,
) -> Result<(), String> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create {:?}: {error}", parent))?;
    }
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|error| format!("remove invalid {:?}: {error}", destination))?;
    }
    let part = part_path(destination);
    if part
        .metadata()
        .is_ok_and(|metadata| metadata.len() > file.size)
    {
        fs::remove_file(&part).map_err(|error| format!("reset oversized partial: {error}"))?;
    }
    let url = source_url(bundle, file)?;

    for _ in 0..2 {
        let offset = part.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let mut request = client.get(url.clone()).header(ACCEPT_ENCODING, "identity");
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
        }
        let mut response = request
            .send()
            .map_err(|error| format!("download {}: {error}", file.path))?;
        let status = response.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            let _ = fs::remove_file(&part);
            continue;
        }
        if !status.is_success() {
            return Err(format!("download {} returned HTTP {status}", file.path));
        }

        let append = offset > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        if append {
            let expected = format!("bytes {offset}-");
            let valid_range = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with(&expected));
            if !valid_range {
                let _ = fs::remove_file(&part);
                continue;
            }
        }
        let mut output = OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(&part)
            .map_err(|error| format!("open partial {}: {error}", file.path))?;
        io::copy(&mut response, &mut output)
            .map_err(|error| format!("write partial {}: {error}", file.path))?;
        output
            .sync_all()
            .map_err(|error| format!("sync partial {}: {error}", file.path))?;
        drop(output);

        verify_file(&part, file)?;
        fs::rename(&part, destination)
            .map_err(|error| format!("promote downloaded {}: {error}", file.path))?;
        return Ok(());
    }
    Err(format!(
        "server would not resume a valid download for {}",
        file.path
    ))
}

fn source_url(bundle: &Bundle, file: &ModelFile) -> Result<reqwest::Url, String> {
    let relative = if let Some(source_path) = &file.source_path {
        safe_url_path(source_path)?
    } else if bundle.source.path_prefix.trim().is_empty() {
        safe_url_path(&file.path)?
    } else {
        safe_url_path(&format!("{}/{}", bundle.source.path_prefix, file.path))?
    };
    let base = reqwest::Url::parse(&bundle.source.base_url)
        .map_err(|error| format!("source base URL: {error}"))?;
    base.join(&relative)
        .map_err(|error| format!("source URL for {}: {error}", file.path))
}

fn safe_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe relative path {value:?}"));
    }
    Ok(path.to_path_buf())
}

fn safe_url_path(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("unsafe source path {value:?}"));
    }
    Ok(value.to_string())
}

fn part_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("model");
    destination.with_file_name(format!("{name}.part"))
}

fn verify_bundle(directory: &Path, bundle: &Bundle) -> Result<(), String> {
    for file in &bundle.files {
        verify_file(&directory.join(safe_relative_path(&file.path)?), file)?;
    }
    Ok(())
}

fn verify_file(path: &Path, expected: &ModelFile) -> Result<(), String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("{} missing: {error}", expected.path))?;
    if !metadata.is_file() || metadata.len() != expected.size {
        return Err(format!(
            "{} size mismatch: expected {}, got {}",
            expected.path,
            expected.size,
            metadata.len()
        ));
    }
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(&expected.sha256) {
        return Err(format!("{} SHA-256 mismatch", expected.path));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|error| format!("open {:?}: {error}", path))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("read {:?}: {error}", path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

struct DownloadLock {
    path: PathBuf,
    file: Option<File>,
}

impl DownloadLock {
    fn acquire(models: &Path, bundle: &str) -> Result<Self, String> {
        let path = models.join(format!(".{bundle}.download.lock"));
        #[cfg(windows)]
        let file = {
            use std::os::windows::fs::OpenOptionsExt;

            // Denying every share mode makes the open handle the lock. Windows
            // releases it if the process crashes, so a harmless leftover path
            // never prevents the Range-resume staging directory being reused.
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .share_mode(0)
                .open(&path)
        };
        #[cfg(not(windows))]
        let file = OpenOptions::new().write(true).create_new(true).open(&path);

        let file = file.map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
            ) {
                format!("another Aokie process is installing {bundle}; retry after it finishes")
            } else {
                format!("create model download lock: {error}")
            }
        })?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }
}

impl Drop for DownloadLock {
    fn drop(&mut self) {
        // Windows cannot unlink a file while our no-sharing handle is open.
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_manifest_is_pinned_and_safe() {
        let manifest = parse_manifest().unwrap();
        assert_eq!(manifest.bundles.len(), 2);
        for bundle in &manifest.bundles {
            for file in &bundle.files {
                let url = source_url(bundle, file).unwrap();
                assert_eq!(url.scheme(), "https");
                assert!(url.as_str().contains(&bundle.source.revision));
                assert!(!url.as_str().contains("/main/"));
            }
        }
    }

    #[test]
    fn local_and_source_paths_reject_traversal() {
        for value in ["", "../model", "a/../model", "/absolute", "a\\model"] {
            assert!(safe_relative_path(value).is_err(), "accepted {value:?}");
            assert!(safe_url_path(value).is_err(), "accepted {value:?}");
        }
    }
}
