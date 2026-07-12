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

/// One bonded device's stored credential (PAIR-001).
///
/// Exactly one of the two key fields is set:
/// - `key_sealed` — the 16 link-key bytes DPAPI-sealed under the current
///   Windows user (`dpapi:<base64>`), the ONLY form ever written on
///   Windows. A different user/machine cannot open it, so a copied
///   pairing_store.json is not a usable credential.
/// - `key_hex` — legacy v1 plaintext hex, and the dev fallback on
///   non-Windows builds (Linux libusb transport; 0600 file perms via
///   `atomic_write`). On Windows a loaded v1 record is re-sealed and the
///   plaintext is atomically rewritten away on first load.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredLinkKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_sealed: Option<String>,
    key_type: u8,
}

const STORE_VERSION_PLAINTEXT: u8 = 1;
const STORE_VERSION_SEALED: u8 = 2;

impl AokiePairingStore {
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let records = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("read pairing store {}: {}", path.display(), e))?;
            let file: StoreFile = serde_json::from_str(&content)
                .map_err(|e| format!("parse pairing store {}: {}", path.display(), e))?;
            if file.version != STORE_VERSION_PLAINTEXT && file.version != STORE_VERSION_SEALED {
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
        let mut store = Self { path, records };
        // PAIR-001 migration: any plaintext record on a DPAPI-capable
        // platform is sealed in place and the file rewritten atomically
        // (tmp + fsync + rename), so the plaintext never survives a load.
        if aokie_core::dpapi::platform_supported() && store.seal_plaintext_records()? {
            store.save()?;
            eprintln!(
                "[AokieRadio] pairing store migrated: link keys sealed at rest ({})",
                store.path.display()
            );
        }
        Ok(store)
    }

    /// Seal every record that still carries plaintext hex. Returns true when
    /// anything changed. A record that cannot be sealed is an error — we never
    /// keep plaintext alongside a sealing-capable platform (fail closed).
    fn seal_plaintext_records(&mut self) -> Result<bool, String> {
        let mut changed = false;
        for (address, record) in self.records.iter_mut() {
            if record.key_sealed.is_some() {
                continue;
            }
            let Some(hex) = record.key_hex.take() else {
                return Err(format!(
                    "pairing store record {} has neither plaintext nor sealed key",
                    address
                ));
            };
            let key = decode_link_key_hex(&hex)?;
            record.key_sealed = Some(aokie_core::dpapi::protect(&key)?);
            changed = true;
        }
        Ok(changed)
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

    /// True when a link key is stored for `address` — i.e. this is a bonded
    /// ("known") device. AOK-BT-001 lets bonded devices reconnect even when
    /// the pairing window is closed; strangers can't.
    pub fn contains(&self, address: &str) -> bool {
        self.records.contains_key(&normalize_address(address))
    }

    /// Forget a bonded device (AOK-BT-001 `phone.removePaired`): drop its link
    /// key so it can no longer reconnect without pairing again. Persists
    /// immediately. Returns true when a record was actually removed.
    pub fn remove(&mut self, address: &str) -> Result<bool, String> {
        let removed = self.records.remove(&normalize_address(address)).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn get(&self, address: &str) -> Result<Option<LinkKeyRecord>, String> {
        self.records
            .get(&normalize_address(address))
            .map(|record| {
                let link_key = match (&record.key_sealed, &record.key_hex) {
                    // Sealed form wins; an unopenable seal is an error, never a
                    // silent fallback — the operator re-pairs (PAIR-001).
                    (Some(sealed), _) => {
                        let bytes = aokie_core::dpapi::unprotect(sealed)?;
                        <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| {
                            format!("sealed link key for {} has the wrong length", address)
                        })?
                    }
                    (None, Some(hex)) => decode_link_key_hex(hex)?,
                    (None, None) => {
                        return Err(format!(
                            "pairing store record {} has no key material",
                            address
                        ))
                    }
                };
                Ok(LinkKeyRecord {
                    address: normalize_address(address),
                    link_key,
                    key_type: record.key_type,
                })
            })
            .transpose()
    }

    pub fn put(&mut self, address: &str, link_key: [u8; 16], key_type: u8) -> Result<(), String> {
        let record = if aokie_core::dpapi::platform_supported() {
            // Windows: sealing is mandatory — a bond we can't protect is a
            // bond we refuse to store (the phone will just re-pair).
            StoredLinkKey {
                key_hex: None,
                key_sealed: Some(aokie_core::dpapi::protect(&link_key)?),
                key_type,
            }
        } else {
            // Non-Windows dev transport: 0600 plaintext file (atomic_write).
            StoredLinkKey {
                key_hex: Some(encode_link_key_hex(&link_key)),
                key_sealed: None,
                key_type,
            }
        };
        self.records.insert(normalize_address(address), record);
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let file = StoreFile {
            // v2 is the only format we write; v1 exists only as a legacy
            // read + migrate source.
            version: STORE_VERSION_SEALED,
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

    fn temp_store_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aokie_pairing_store_{}_{}_{}.json",
            tag,
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn encodes_and_decodes_link_key_hex() {
        let key = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let encoded = encode_link_key_hex(&key);
        assert_eq!(encoded, "000102030405060708090a0b0c0d0e0f");
        assert_eq!(decode_link_key_hex(&encoded).unwrap(), key);
    }

    #[test]
    fn pairing_store_round_trips_records() {
        let path = temp_store_path("roundtrip");
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

    #[test]
    fn contains_and_remove_are_case_insensitive_and_persist() {
        let path = temp_store_path("remove");
        let _ = std::fs::remove_file(&path);

        let mut store = AokiePairingStore::load(&path).unwrap();
        store.put("00:19:86:00:22:6c", [0x22; 16], 0x05).unwrap();
        // contains() normalizes the address like get()/put() do.
        assert!(store.contains("00:19:86:00:22:6C"));
        assert!(!store.contains("aa:bb:cc:dd:ee:ff"));

        // Removing an unknown address is a no-op returning false.
        assert!(!store.remove("aa:bb:cc:dd:ee:ff").unwrap());
        // Removing the bonded device returns true and persists.
        assert!(store.remove("00:19:86:00:22:6c").unwrap());
        assert!(!store.contains("00:19:86:00:22:6C"));

        let reloaded = AokiePairingStore::load(&path).unwrap();
        assert_eq!(reloaded.len(), 0, "removal survived a reload");

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn stored_link_keys_are_sealed_at_rest_on_windows() {
        let path = temp_store_path("sealed");
        let _ = std::fs::remove_file(&path);

        let key = [0x5a; 16];
        let mut store = AokiePairingStore::load(&path).unwrap();
        store.put("00:19:86:00:22:6c", key, 0x04).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let plaintext_hex = encode_link_key_hex(&key);
        assert!(
            !raw.contains(&plaintext_hex),
            "pairing store file must not contain the plaintext link key"
        );
        assert!(raw.contains("key_sealed"), "expected a sealed record");
        assert!(raw.contains("\"version\": 2"), "expected v2 store: {}", raw);

        // And it still opens back to the real key bytes.
        let record = AokiePairingStore::load(&path)
            .unwrap()
            .get("00:19:86:00:22:6C")
            .unwrap()
            .unwrap();
        assert_eq!(record.link_key, key);

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn legacy_plaintext_store_migrates_to_sealed_on_load() {
        let path = temp_store_path("migrate");
        let _ = std::fs::remove_file(&path);

        // Hand-write a v1 plaintext store, exactly what old builds produced.
        let key = [0x33; 16];
        let legacy = serde_json::json!({
            "version": 1,
            "link_keys": {
                "00:19:86:00:22:6C": {
                    "key_hex": encode_link_key_hex(&key),
                    "key_type": 4
                }
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        // Loading migrates: plaintext gone from disk, key still usable.
        let store = AokiePairingStore::load(&path).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get("00:19:86:00:22:6c").unwrap().unwrap().link_key,
            key
        );

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains(&encode_link_key_hex(&key)),
            "migration must remove the plaintext hex from disk: {}",
            raw
        );
        assert!(raw.contains("\"version\": 2"));

        let _ = std::fs::remove_file(&path);
    }
}
