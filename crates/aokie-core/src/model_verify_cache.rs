//! Disk-backed cache for SHA-256 verification of large model files.
//!
//! `verify_models` re-hashes every model file across every backend on
//! every `initialize_*` call (and the three init calls run in parallel
//! at app launch). Whisper + Gemma + Pocket-TTS combined is ~8-10 GB,
//! so each launch was paying tens of seconds of redundant I/O.
//!
//! Cache key: `(absolute_path, file_size, mtime_secs, mtime_nanos)`.
//! A matching key returns the cached SHA without re-hashing. Any
//! mismatch (size differs, mtime differs, or no entry) falls through
//! to the slow path and writes a fresh entry. Replacing a model via
//! `download_*` writes a new file via atomic rename, so size/mtime
//! both change and the stale entry is correctly invalidated.
//!
//! Stored at `<app_data>/model_verify_cache.json`. Missing/malformed
//! falls back to an empty cache (degrades to the original re-hash
//! behaviour rather than blocking startup).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const FILENAME: &str = "model_verify_cache.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifyCacheEntry {
    pub size: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub sha256: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct VerifyCache {
    pub entries: HashMap<String, VerifyCacheEntry>,
}

impl VerifyCache {
    pub fn lookup(
        &self,
        path: &Path,
        size: u64,
        mtime_secs: u64,
        mtime_nanos: u32,
    ) -> Option<&str> {
        let key = path.to_string_lossy().to_string();
        let entry = self.entries.get(&key)?;
        if entry.size == size && entry.mtime_secs == mtime_secs && entry.mtime_nanos == mtime_nanos
        {
            Some(&entry.sha256)
        } else {
            None
        }
    }

    pub fn put(
        &mut self,
        path: &Path,
        size: u64,
        mtime_secs: u64,
        mtime_nanos: u32,
        sha256: String,
    ) {
        let key = path.to_string_lossy().to_string();
        self.entries.insert(
            key,
            VerifyCacheEntry {
                size,
                mtime_secs,
                mtime_nanos,
                sha256,
            },
        );
    }
}

fn cache_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(FILENAME)
}

pub fn load(app_data_dir: &Path) -> VerifyCache {
    let path = cache_path(app_data_dir);
    if !path.exists() {
        return VerifyCache::default();
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[model_verify_cache] read {:?} failed: {} — using empty cache",
                path, e
            );
            return VerifyCache::default();
        }
    };
    match serde_json::from_str::<VerifyCache>(&raw) {
        Ok(cache) => cache,
        Err(e) => {
            eprintln!(
                "[model_verify_cache] parse {:?} failed: {} — using empty cache",
                path, e
            );
            VerifyCache::default()
        }
    }
}

pub fn save(app_data_dir: &Path, cache: &VerifyCache) -> Result<(), String> {
    let path = cache_path(app_data_dir);
    let json = serde_json::to_string_pretty(cache).map_err(|e| format!("serialize: {}", e))?;
    crate::paths::atomic_write(&path, json.as_bytes())
}

/// Resolve the on-disk size + mtime of `path` into the cache-key
/// fields. Returns `None` if the file is missing or its mtime is
/// pre-UNIX-epoch (which shouldn't happen but lets the caller fall
/// through to a re-hash rather than panic).
pub async fn file_key(path: &Path) -> Option<(u64, u64, u32)> {
    // Legacy source used `tokio::fs::metadata(path).await`; aokie-core
    // is intentionally tokio-free (light foundation crate), so this
    // does the single stat synchronously. The fn stays `async` to keep
    // its call sites (which `.await` it) unchanged.
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some((size, dur.as_secs(), dur.subsec_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "aokie_verify_cache_test_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
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
    fn empty_cache_when_file_absent() {
        let tmp = TestDir::new();
        let cache = load(tmp.path());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn round_trip() {
        let tmp = TestDir::new();
        let mut cache = VerifyCache::default();
        let p = Path::new("/some/model.bin");
        cache.put(p, 1024, 1_700_000_000, 12345, "abc123".to_string());
        save(tmp.path(), &cache).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded.lookup(p, 1024, 1_700_000_000, 12345), Some("abc123"));
    }

    #[test]
    fn lookup_misses_on_size_change() {
        let mut cache = VerifyCache::default();
        let p = Path::new("/some/model.bin");
        cache.put(p, 1024, 1_700_000_000, 0, "abc".to_string());
        assert!(cache.lookup(p, 2048, 1_700_000_000, 0).is_none());
    }

    #[test]
    fn lookup_misses_on_mtime_change() {
        let mut cache = VerifyCache::default();
        let p = Path::new("/some/model.bin");
        cache.put(p, 1024, 1_700_000_000, 0, "abc".to_string());
        assert!(cache.lookup(p, 1024, 1_800_000_000, 0).is_none());
        assert!(cache.lookup(p, 1024, 1_700_000_000, 1).is_none());
    }
}
