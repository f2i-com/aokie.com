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
    /// Load `settings.json` from `data_dir`. A missing file is a normal
    /// first run; a CORRUPT file is quarantined (renamed to
    /// `settings.json.corrupt`, preserving the evidence) and safe defaults
    /// take over — with auto-answer OFF by default (INT-006), a mangled
    /// config can never arm the receptionist.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(SETTINGS_FILE);
        let mut quarantined = false;
        let config = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(c) => c,
                Err(e) => {
                    let quarantine = path.with_extension("json.corrupt");
                    let _ = std::fs::remove_file(&quarantine);
                    let moved = std::fs::rename(&path, &quarantine).is_ok();
                    eprintln!(
                        "[aokie-plugin] settings.json is corrupt ({e}) — {} and running on safe defaults (auto-answer OFF)",
                        if moved { "quarantined to settings.json.corrupt" } else { "quarantine rename failed; ignoring the file" }
                    );
                    quarantined = true;
                    PluginConfig::default()
                }
            },
            Err(_) => PluginConfig::default(),
        };
        ConfigStore {
            path,
            config,
            quarantined,
        }
    }

    /// Persist atomically (write tmp + rename) so a crash mid-write
    /// can't truncate the settings file.
    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.config)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, text)?;
        // Windows rename fails if the target exists; replace explicitly.
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
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
