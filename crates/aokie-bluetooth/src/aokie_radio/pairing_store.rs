use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkKeyRecord {
    pub address: String,
    pub link_key: [u8; 16],
    pub key_type: u8,
}

#[derive(Debug, Clone)]
pub struct AokiePairingStore {
    path: PathBuf,
    records: BTreeMap<String, StoredLinkKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreFile {
    version: u8,
    link_keys: BTreeMap<String, StoredLinkKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredLinkKey {
    key_hex: String,
    key_type: u8,
}

impl AokiePairingStore {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let records = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("read pairing store {}: {}", path.display(), e))?;
            let file: StoreFile = serde_json::from_str(&content)
                .map_err(|e| format!("parse pairing store {}: {}", path.display(), e))?;
            if file.version != 1 {
                return Err(format!(
                    "unsupported pairing store version {} in {}",
                    file.version,
                    path.display()
                ));
            }
            file.link_keys
        } else {
            BTreeMap::new()
        };
        Ok(Self { path, records })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Enumerate every paired BD_ADDR currently in the store. Link keys
    /// are deliberately NOT exposed — this list is meant for the
    /// frontend's "paired devices" UI and any IPC surface, where the
    /// link key would be a credential leak.
    pub fn list_addresses(&self) -> Vec<String> {
        self.records.keys().cloned().collect()
    }

    pub fn get(&self, address: &str) -> Result<Option<LinkKeyRecord>, String> {
        self.records
            .get(&normalize_address(address))
            .map(|record| {
                Ok(LinkKeyRecord {
                    address: normalize_address(address),
                    link_key: decode_link_key_hex(&record.key_hex)?,
                    key_type: record.key_type,
                })
            })
            .transpose()
    }

    pub fn put(&mut self, address: &str, link_key: [u8; 16], key_type: u8) -> Result<(), String> {
        self.records.insert(
            normalize_address(address),
            StoredLinkKey {
                key_hex: encode_link_key_hex(&link_key),
                key_type,
            },
        );
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let file = StoreFile {
            version: 1,
            link_keys: self.records.clone(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("serialize pairing store: {}", e))?;

        // Defer to the central atomic-write helper: tmp + fsync + rename,
        // and on Unix it creates the tmp file with 0600 so the link keys
        // (a credential) are never world-readable at any point.
        aokie_core::paths::atomic_write(&self.path, json.as_bytes())
    }
}

pub fn default_store_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("aokie_radio").join("pairing_store.json")
}

fn normalize_address(address: &str) -> String {
    address.to_ascii_uppercase()
}

fn encode_link_key_hex(link_key: &[u8; 16]) -> String {
    link_key.iter().map(|b| format!("{:02x}", b)).collect()
}

fn decode_link_key_hex(value: &str) -> Result<[u8; 16], String> {
    if value.len() != 32 {
        return Err(format!(
            "link key hex has {} chars, expected 32",
            value.len()
        ));
    }
    let mut out = [0u8; 16];
    for index in 0..16 {
        let offset = index * 2;
        out[index] = u8::from_str_radix(&value[offset..offset + 2], 16)
            .map_err(|_| format!("link key hex contains invalid byte at offset {}", offset))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_link_key_hex() {
        let key = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let encoded = encode_link_key_hex(&key);
        assert_eq!(encoded, "000102030405060708090a0b0c0d0e0f");
        assert_eq!(decode_link_key_hex(&encoded).unwrap(), key);
    }

    #[test]
    fn pairing_store_round_trips_records() {
        let path = std::env::temp_dir().join(format!(
            "aokie_pairing_store_test_{}_{}.json",
            std::process::id(),
            rand::random::<u64>()
        ));
        let _ = std::fs::remove_file(&path);

        let key = [0x11; 16];
        let mut store = AokiePairingStore::load(&path).unwrap();
        assert_eq!(store.path(), path.as_path());
        assert_eq!(store.len(), 0);
        store.put("00:19:86:00:22:6c", key, 0x04).unwrap();

        let store = AokiePairingStore::load(&path).unwrap();
        let record = store.get("00:19:86:00:22:6C").unwrap().unwrap();
        assert_eq!(record.address, "00:19:86:00:22:6C");
        assert_eq!(record.link_key, key);
        assert_eq!(record.key_type, 0x04);

        let _ = std::fs::remove_file(&path);
    }
}
