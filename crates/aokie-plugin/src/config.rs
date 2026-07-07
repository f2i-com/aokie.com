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
}

/// Load/save wrapper bound to one data dir.
pub struct ConfigStore {
    path: PathBuf,
    pub config: PluginConfig,
}

impl ConfigStore {
    /// Load `settings.json` from `data_dir`, falling back to defaults
    /// when missing or unreadable (a corrupt settings file must never
    /// keep the plugin from starting — Desktop would mark it crashed).
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(SETTINGS_FILE);
        let config = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        ConfigStore { path, config }
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
    fn corrupt_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "{not json").unwrap();
        let store = ConfigStore::load(dir.path());
        assert_eq!(store.config, PluginConfig::default());
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
