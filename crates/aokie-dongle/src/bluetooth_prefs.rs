//! Persistence for the operator-facing Bluetooth toggles
//! (auto-answer enabled / delay) consented to in the wizard or
//! Settings panel.
//!
//! Stored as `<app_data>/bluetooth.json`. `BluetoothState::default()`
//! intentionally starts with auto-answer OFF (privacy-first; the
//! security review flagged a default-on as a footgun on a fresh
//! install). Without persistence, that default would override the
//! operator's choice on every relaunch — so this module hydrates the
//! atomics at startup and `configure_bluetooth` writes through here
//! whenever the choice changes.
//!
//! Same conservative pattern as `retention.rs`: missing/malformed
//! file falls back to defaults rather than blocking app launch.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILENAME: &str = "bluetooth.json";

fn default_auto_answer_enabled() -> bool {
    false
}

fn default_auto_answer_delay_ms() -> u32 {
    1000
}

/// Hard ceiling for the auto-answer delay (R5-#8). 10 seconds is well
/// past the longest reasonable "give the SCO link a moment to negotiate"
/// window — any call still ringing this long has rung out / gone to
/// voicemail by typical mobile carrier defaults. Without this clamp,
/// a hand-edited `bluetooth.json` or hand-crafted IPC payload could set
/// the delay anywhere up to `u32::MAX` (≈49 days); the answer thread
/// `sleep`s on it, so a ridiculous value silently turns auto-answer
/// off in practice. Mirrors `retention::clamp_days`.
pub const MAX_AUTO_ANSWER_DELAY_MS: u32 = 10_000;

pub fn clamp_delay_ms(delay_ms: u32) -> u32 {
    delay_ms.min(MAX_AUTO_ANSWER_DELAY_MS)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BluetoothPrefs {
    #[serde(default = "default_auto_answer_enabled")]
    pub auto_answer_enabled: bool,
    #[serde(default = "default_auto_answer_delay_ms")]
    pub auto_answer_delay_ms: u32,
}

impl Default for BluetoothPrefs {
    fn default() -> Self {
        Self {
            auto_answer_enabled: default_auto_answer_enabled(),
            auto_answer_delay_ms: default_auto_answer_delay_ms(),
        }
    }
}

fn config_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(FILENAME)
}

pub fn load(app_data_dir: &Path) -> BluetoothPrefs {
    let path = config_path(app_data_dir);
    if !path.exists() {
        return BluetoothPrefs::default();
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[bluetooth_prefs] read {:?} failed: {} — using defaults",
                path, e
            );
            return BluetoothPrefs::default();
        }
    };
    match serde_json::from_str::<BluetoothPrefs>(&raw) {
        Ok(cfg) => BluetoothPrefs {
            auto_answer_enabled: cfg.auto_answer_enabled,
            auto_answer_delay_ms: clamp_delay_ms(cfg.auto_answer_delay_ms),
        },
        Err(e) => {
            eprintln!(
                "[bluetooth_prefs] parse {:?} failed: {} — using defaults",
                path, e
            );
            BluetoothPrefs::default()
        }
    }
}

pub fn save(app_data_dir: &Path, cfg: &BluetoothPrefs) -> Result<(), String> {
    let path = config_path(app_data_dir);
    let json = serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize: {}", e))?;
    aokie_core::paths::atomic_write(&path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            // Cargo runs tests inside the same process by default, so a
            // PID-only suffix collides between tests in this module.
            // Bump a per-process atomic counter to give each TestDir its
            // own scratch directory.
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "aokie_bt_prefs_test_{}_{}",
                std::process::id(),
                n,
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
    fn defaults_load_when_file_absent() {
        let tmp = TestDir::new();
        let cfg = load(tmp.path());
        assert!(!cfg.auto_answer_enabled);
        assert_eq!(cfg.auto_answer_delay_ms, 1000);
    }

    #[test]
    fn round_trip() {
        let tmp = TestDir::new();
        let cfg = BluetoothPrefs {
            auto_answer_enabled: true,
            auto_answer_delay_ms: 500,
        };
        save(tmp.path(), &cfg).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded, cfg);
    }

    /// R5-#8: a hand-edited bluetooth.json with a stupidly large delay
    /// must come back clamped. Without the clamp, the auto-answer
    /// thread `sleep`s on that value, and the operator silently
    /// loses auto-answer (looks like the app stopped working).
    #[test]
    fn load_clamps_oversized_delay() {
        let tmp = TestDir::new();
        let path = tmp.path().join(FILENAME);
        // Write an out-of-range delay directly so we exercise the
        // load path's defence rather than the save path's clamp.
        fs::write(
            &path,
            r#"{"auto_answer_enabled":true,"auto_answer_delay_ms":4000000000}"#,
        )
        .unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded.auto_answer_delay_ms, MAX_AUTO_ANSWER_DELAY_MS);
        assert!(loaded.auto_answer_enabled);
    }
}
