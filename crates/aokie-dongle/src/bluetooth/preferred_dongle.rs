//! Persistent storage for the operator's selected Bluetooth dongle
//! path.
//!
//! The production runtime path used to open the *first* HCI-capable
//! WinUSB radio it could find. That works on a single-dongle box, but
//! breaks when an operator has more than one bound dongle (a test
//! dongle plus a production dongle, two different chip families, etc.):
//! "first" is enumeration-order-dependent and could swap between
//! reboots. The reviewer flagged that as a real reliability footgun.
//!
//! Persisting the operator's pick from the Pairing UI here lets the
//! runtime open the same physical dongle every launch. The runtime
//! still falls back to enumeration-first when the persisted path is
//! missing or refers to a now-disconnected dongle, so a "moved my
//! dongle to a different USB port" flow still works without manual
//! intervention.
//!
//! Storage is a tiny JSON file alongside the rest of Aokie's app-data
//! state; format is intentionally minimal so a future port (matching
//! by VID/PID instead of full path, etc.) can extend the schema
//! without a migration.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "preferred_dongle.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PreferredDongleFile {
    /// Operator-selected dongle interface path. None or absent means
    /// "use whatever the runtime enumerates first" (default behaviour
    /// for an unconfigured install).
    #[serde(default)]
    path: Option<String>,
}

fn file_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(FILE_NAME)
}

/// Read the persisted dongle path. A missing file, IO error, or
/// malformed JSON all collapse to `None` — the runtime fall-back is
/// always safe (enumerate first). Logging on parse error so a future
/// debug session sees the operator's intent was rejected.
pub fn load(app_data_dir: &Path) -> Option<String> {
    let path = file_path(app_data_dir);
    let json = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<PreferredDongleFile>(&json) {
        Ok(parsed) => parsed.path.filter(|p| !p.is_empty()),
        Err(e) => {
            eprintln!(
                "[preferred_dongle] {:?} failed to parse: {} — falling back to enumeration-first",
                path, e
            );
            None
        }
    }
}

/// Write the dongle path through the hardened atomic-write helper.
/// `None` clears the file (use `clear` for the explicit "forget my
/// pick" recovery path; `save(None)` is the same shape).
///
/// R7/P2-4: previously this used a fixed `<file>.json.tmp` sibling
/// + `std::fs::rename`. That collides between concurrent writers
/// (two Pairing-page saves in flight at once) and skips the
/// parent-dir fsync that makes the rename itself power-loss durable
/// on POSIX. `paths::atomic_write` mints a unique `<pid>-<seq>.tmp`
/// per call, fsyncs the file before rename, and fsyncs the parent
/// dir after — same hardening as `bluetooth_prefs` / `retention` /
/// `consent`. The shape on disk is unchanged.
pub fn save(app_data_dir: &Path, path: Option<String>) -> Result<(), String> {
    if app_data_dir.as_os_str().is_empty() {
        return Err("preferred_dongle: app_data_dir is empty".into());
    }
    let target = file_path(app_data_dir);
    let payload = PreferredDongleFile {
        path: path.filter(|p| !p.is_empty()),
    };
    let json = serde_json::to_string(&payload)
        .map_err(|e| format!("preferred_dongle: serialize: {}", e))?;
    aokie_core::paths::atomic_write(&target, json.as_bytes())
        .map_err(|e| format!("preferred_dongle: {}", e))
}

/// Convenience: delete the file. Equivalent to `save(_, None)` —
/// either is safe — but the explicit "forget my pick" verb reads
/// better at call sites.
pub fn clear(app_data_dir: &Path) -> Result<(), String> {
    save(app_data_dir, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Self-cleaning tempdir helper so tests don't need to care about
    /// platform-specific tempfile ergonomics.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!("aokie-preferred-dongle-test-{}", nanos));
            fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
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
    fn missing_file_loads_as_none() {
        let tmp = TestDir::new();
        assert!(load(tmp.path()).is_none());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TestDir::new();
        save(
            tmp.path(),
            Some("\\\\?\\USB#VID_0A5C&PID_21EC#5&abc".to_string()),
        )
        .unwrap();
        assert_eq!(
            load(tmp.path()).as_deref(),
            Some("\\\\?\\USB#VID_0A5C&PID_21EC#5&abc"),
        );
    }

    #[test]
    fn save_none_clears_existing() {
        let tmp = TestDir::new();
        save(tmp.path(), Some("test-path".into())).unwrap();
        save(tmp.path(), None).unwrap();
        assert!(load(tmp.path()).is_none());
    }

    #[test]
    fn empty_string_is_treated_as_none() {
        let tmp = TestDir::new();
        // An operator who somehow saved an empty string (UI bug, manual
        // edit, etc.) shouldn't make the runtime try to open "" as a
        // device path — collapse to None on the way in and out.
        save(tmp.path(), Some(String::new())).unwrap();
        assert!(load(tmp.path()).is_none());
    }

    #[test]
    fn malformed_file_loads_as_none() {
        let tmp = TestDir::new();
        fs::write(file_path(tmp.path()), "{ this isn't json }").unwrap();
        assert!(load(tmp.path()).is_none());
    }
}
