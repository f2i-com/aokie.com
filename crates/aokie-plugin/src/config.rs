//! Plugin settings persisted as JSON under the per-plugin data dir.
//!
//! Desktop hands the plugin a writable directory via the
//! `FORMLOGIC_PLUGIN_DATA_DIR` env var and again as `dataDir` in
//! `plugin.init` (DESKTOP_PLUGIN_SDK.md §2). Everything the plugin
//! persists lives under it: this settings file (`settings.json`) and
//! the outbox DB (`outbox.sqlite`).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Settings file name inside the plugin data dir.
pub const SETTINGS_FILE: &str = "settings.json";

/// The operator-preferred dongle (`dongle.getPreferred` /
/// `dongle.setPreferred`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreferredDongle {
    pub vid: u16,
    pub pid: u16,
}

/// A phone the operator has paired (mock/config-backed until the
/// radio stack moves into aokie-core).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    pub address: String,
    pub name: String,
}

/// Phase 2 outbound guardrail: how many automated dials were placed on
/// `date` (operator-LOCAL `YYYY-MM-DD`). Persisted so a plugin restart
/// can't reset the daily cap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DialLedger {
    pub date: String,
    pub count: u32,
}

/// The persisted document. `settings` is the free-form key/value bag
/// behind `settings.get` / `settings.set` (e.g. `mockCalls: true`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginConfig {
    pub preferred_dongle: Option<PreferredDongle>,
    pub paired_devices: Vec<PairedDevice>,
    pub settings: Map<String, Value>,
    /// Monotonic change counter (audit AK-006): bumped on every successful
    /// `settings.set` save, so operators and tests can tell exactly which
    /// configuration a call ran under.
    pub config_version: u64,
    /// Phase 2: the outbound daily-dial ledger (None = never dialed).
    pub dial_ledger: Option<DialLedger>,
}

/// Load/save wrapper bound to one data dir.
pub struct ConfigStore {
    path: PathBuf,
    pub config: PluginConfig,
    /// True when load() found a CORRUPT settings file (audit AK-006): the
    /// file was quarantined and safe defaults are in effect — surfaced in
    /// `plugin.health` and `settings.get` so the reset is never silent.
    pub quarantined: bool,
}

impl ConfigStore {
    /// Load `settings.json` from `data_dir`. A missing file falls back to
    /// the last-known-good `settings.json.bak` (audit AOK-CFG-001 — a crash
    /// inside an old save's delete window, or a failed replace, must not
    /// reset the receptionist) and only then to first-run defaults. A
    /// CORRUPT file is quarantined (renamed to `settings.json.corrupt`,
    /// preserving the evidence) and the same recovery ladder applies — with
    /// auto-answer OFF by default (INT-006), a mangled config can never arm
    /// the receptionist even when no backup survives.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(SETTINGS_FILE);
        let bak = path.with_extension("json.bak");
        let restore = |why: &str| -> Option<PluginConfig> {
            let text = std::fs::read_to_string(&bak).ok()?;
            let cfg: PluginConfig = serde_json::from_str(&text).ok()?;
            eprintln!(
                "[aokie-plugin] settings.json {why} — restored last-known-good settings.json.bak (configVersion {})",
                cfg.config_version
            );
            Some(cfg)
        };
        let mut quarantined = false;
        let mut restored = None;
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(c) => c,
                Err(e) => {
                    let quarantine = path.with_extension("json.corrupt");
                    let _ = std::fs::remove_file(&quarantine);
                    let moved = std::fs::rename(&path, &quarantine).is_ok();
                    eprintln!(
                        "[aokie-plugin] settings.json is corrupt ({e}) — {}",
                        if moved {
                            "quarantined to settings.json.corrupt"
                        } else {
                            "quarantine rename failed; ignoring the file"
                        }
                    );
                    quarantined = true;
                    restored = restore("was corrupt");
                    restored.clone().unwrap_or_default()
                }
            },
            Err(_) => {
                restored = restore("is missing");
                restored.clone().unwrap_or_default()
            }
        };
        let store = ConfigStore {
            path,
            config,
            quarantined,
        };
        // A recovery is only durable once it is the PRIMARY file again.
        if restored.is_some() {
            let _ = store.save();
        }
        store
    }

    /// Persist atomically AND recoverably (audit AOK-CFG-001).
    ///
    /// Order of operations matters: (1) the new content is written to a tmp
    /// file and fsynced — a crash mid-write can never touch the live file;
    /// (2) the current live file is copied to `settings.json.bak` — the
    /// last-known-good [`load`](Self::load) restores from; (3) the tmp file
    /// REPLACES the live file in one rename (Rust's Windows rename uses
    /// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`), so there is no window where
    /// no settings file exists. The old delete-then-rename left exactly that
    /// window: a crash or an antivirus lock between the two calls stranded
    /// the receptionist with NO configuration at all.
    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.config)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?; // the bytes must be ON DISK before they can replace the live file
        }
        // Keep the outgoing config as last-known-good (best-effort: a failed
        // backup must not block the save itself).
        if self.path.is_file() {
            let _ = std::fs::copy(&self.path, self.path.with_extension("json.bak"));
        }
        match std::fs::rename(&tmp, &self.path) {
            Ok(()) => Ok(()),
            Err(_) if self.path.exists() => {
                // Fallback for a platform/filesystem where rename won't
                // replace: the pre-existing (riskier) delete-then-rename,
                // now safe to attempt because .bak was captured above.
                std::fs::remove_file(&self.path)?;
                std::fs::rename(&tmp, &self.path)
            }
            Err(e) => Err(e),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::load(dir.path());
        assert_eq!(store.config, PluginConfig::default());
        assert!(store.config.preferred_dongle.is_none());
    }

    #[test]
    fn save_and_reload_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::load(dir.path());
        store.config.preferred_dongle = Some(PreferredDongle {
            vid: 0x0a5c,
            pid: 0x21e8,
        });
        store.config.paired_devices.push(PairedDevice {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "Operator phone".to_string(),
        });
        store
            .config
            .settings
            .insert("mockCalls".to_string(), json!(true));
        store.save().unwrap();

        let back = ConfigStore::load(dir.path());
        assert_eq!(back.config, store.config);
    }

    #[test]
    fn corrupt_file_is_quarantined_with_safe_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "{not json").unwrap();
        let store = ConfigStore::load(dir.path());
        assert_eq!(store.config, PluginConfig::default());
        assert!(store.quarantined, "corruption must be visible, not silent");
        // Evidence preserved, live file gone — the next save starts clean.
        assert!(dir.path().join("settings.json.corrupt").is_file());
        assert!(!dir.path().join(SETTINGS_FILE).exists());
        // A fresh load after quarantine is a normal (non-quarantined) run.
        let again = ConfigStore::load(dir.path());
        assert!(!again.quarantined);
    }

    #[test]
    fn missing_file_is_not_quarantine() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::load(dir.path());
        assert!(!store.quarantined, "first run must not report corruption");
    }

    /// Audit AOK-CFG-001: the crash window of the old delete-then-rename —
    /// primary GONE, only the backup left — must recover the last good
    /// config and re-materialize it as the primary file.
    #[test]
    fn missing_primary_restores_last_known_good() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::load(dir.path());
        store
            .config
            .settings
            .insert("greeting".into(), json!("Hi!"));
        store.config.config_version = 3;
        store.save().unwrap();
        store.save().unwrap(); // second save captures v3 into .bak
        std::fs::remove_file(dir.path().join(SETTINGS_FILE)).unwrap(); // simulated crash window

        let back = ConfigStore::load(dir.path());
        assert_eq!(back.config.config_version, 3, "last-known-good restored");
        assert_eq!(back.config.settings.get("greeting"), Some(&json!("Hi!")));
        assert!(!back.quarantined, "a clean restore is not corruption");
        assert!(
            dir.path().join(SETTINGS_FILE).is_file(),
            "recovery re-materializes the primary file"
        );
    }

    /// Audit AOK-CFG-001 + AK-006: a corrupt primary quarantines AND
    /// recovers the backup instead of falling all the way to defaults.
    #[test]
    fn corrupt_primary_prefers_backup_over_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::load(dir.path());
        store.config.config_version = 5;
        store.save().unwrap();
        store.save().unwrap(); // .bak now holds v5
        std::fs::write(dir.path().join(SETTINGS_FILE), "{mangled").unwrap();

        let back = ConfigStore::load(dir.path());
        assert!(back.quarantined, "corruption is still surfaced");
        assert_eq!(back.config.config_version, 5, "backup beats defaults");
        assert!(dir.path().join("settings.json.corrupt").is_file());
    }

    /// Every save keeps the OUTGOING config as .bak — the invariant the
    /// recovery ladder stands on.
    #[test]
    fn save_captures_previous_version_as_backup() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::load(dir.path());
        store.config.config_version = 1;
        store.save().unwrap();
        store.config.config_version = 2;
        store.save().unwrap();
        let bak: PluginConfig = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("settings.json.bak")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            bak.config_version, 1,
            ".bak holds the version being replaced"
        );
    }

    #[test]
    fn config_version_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::load(dir.path());
        assert_eq!(store.config.config_version, 0);
        store.config.config_version = 7;
        store.save().unwrap();
        assert_eq!(ConfigStore::load(dir.path()).config.config_version, 7);
    }

    #[test]
    fn settings_file_is_camel_case_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ConfigStore::load(dir.path());
        store.config.preferred_dongle = Some(PreferredDongle { vid: 1, pid: 2 });
        store.save().unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(text.contains("preferredDongle"), "got: {text}");
        assert!(text.contains("pairedDevices"), "got: {text}");
    }
}
