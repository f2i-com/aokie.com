//! Persistent native endpoint identity and protocol-v2 proof signing.
//!
//! The Ed25519 seed is stored only in Windows Credential Manager or an
//! Android-Keystore-encrypted native value. Public keys, thumbprints and
//! signatures may cross the wire; the seed never enters a Tauri command or
//! WebView event.

use aokie_protocol::v2::{
    EndpointBindingClaims, EndpointPublicKey, HelloProofClaims, SignedEndpointBinding,
    SignedHelloProof, SignedTrickleCandidateEnvelope, TrickleCandidateClaims,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tauri::AppHandle;

const STORE_ACCOUNT: &str = "aokie-endpoint-install-identity-v1";
#[cfg(target_os = "windows")]
const KEYRING_SERVICE: &str = "Aokie Companion Endpoint Identity";

#[derive(Clone)]
pub(crate) struct EndpointIdentity {
    signing_key: SigningKey,
    public_key: EndpointPublicKey,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedIdentity {
    schema_version: u8,
    algorithm: String,
    secret_key: String,
    public_key: EndpointPublicKey,
}

impl EndpointIdentity {
    pub(crate) fn from_secret(secret: [u8; 32]) -> Result<Self, String> {
        let signing_key = SigningKey::from_bytes(&secret);
        let public_key =
            EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
        public_key.validate().map_err(|error| error.to_string())?;
        Ok(Self {
            signing_key,
            public_key,
        })
    }

    pub(crate) fn public_key(&self) -> &EndpointPublicKey {
        &self.public_key
    }

    pub(crate) fn thumbprint(&self) -> &str {
        &self.public_key.thumbprint
    }

    fn signature(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(message).to_bytes())
    }

    /// Sign a validated Desktop-pairing response without exposing the
    /// installation seed to a Tauri command or the WebView.
    pub(crate) fn sign_desktop_pairing_response(&self, signing_bytes: &[u8]) -> String {
        self.signature(signing_bytes)
    }

    pub(crate) fn sign_hello(&self, claims: HelloProofClaims) -> Result<SignedHelloProof, String> {
        let signature = self.signature(&claims.signing_bytes().map_err(|error| error.to_string())?);
        let proof = SignedHelloProof {
            endpoint_key: self.public_key.clone(),
            claims,
            signature,
        };
        // A self-check of shape and signature, made at the proof's own issue
        // time: its window may be dated by the gateway's clock rather than
        // this phone's (`EndpointChallengeFrame::hello_proof_window`), and
        // freshness is the gateway's to judge.
        let issued_at = proof.claims.issued_at;
        proof.verify(issued_at).map_err(|error| error.to_string())?;
        Ok(proof)
    }

    pub(crate) fn sign_sdp_binding(
        &self,
        claims: EndpointBindingClaims,
    ) -> Result<SignedEndpointBinding, String> {
        let signature = self.signature(&claims.signing_bytes().map_err(|error| error.to_string())?);
        let binding = SignedEndpointBinding {
            endpoint_key: self.public_key.clone(),
            claims,
            signature,
        };
        binding
            .validate_shape()
            .map_err(|error| error.to_string())?;
        Ok(binding)
    }

    pub(crate) fn sign_candidate(
        &self,
        claims: TrickleCandidateClaims,
    ) -> Result<SignedTrickleCandidateEnvelope, String> {
        let signature = self.signature(&claims.signing_bytes().map_err(|error| error.to_string())?);
        let envelope = SignedTrickleCandidateEnvelope {
            endpoint_key: self.public_key.clone(),
            claims,
            signature,
        };
        envelope
            .validate_shape()
            .map_err(|error| error.to_string())?;
        Ok(envelope)
    }
}

/// Serialises [`load_or_create`]: two first launches racing (a push wake and
/// the UI, say) would otherwise each find no identity, each mint one, and the
/// loser's thumbprint would already be in use somewhere when the winner's
/// overwrote it.
static LOAD_OR_CREATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The install identity, minted on first use.
///
/// Only a store that answers "no such value" leads to a new identity. A read
/// that FAILS (a Keystore or decrypt error) is returned as an error: minting
/// then would overwrite the identity every Desktop has pinned.
pub(crate) async fn load_or_create(app: &AppHandle) -> Result<EndpointIdentity, String> {
    load_or_create_in(&NativeIdentityStore(app)).await
}

/// Where the install identity lives: the platform's secure store in the app,
/// a fake in tests.
trait IdentityStore {
    async fn get(&self, account: &str) -> Result<Option<String>, String>;
    async fn put(&self, account: &str, value: &str) -> Result<(), String>;
}

struct NativeIdentityStore<'a>(&'a AppHandle);

impl IdentityStore for NativeIdentityStore<'_> {
    async fn get(&self, account: &str) -> Result<Option<String>, String> {
        native_store_get(self.0, account).await
    }

    async fn put(&self, account: &str, value: &str) -> Result<(), String> {
        native_store_put(self.0, account, value).await
    }
}

async fn load_or_create_in(store: &impl IdentityStore) -> Result<EndpointIdentity, String> {
    let _serialised = LOAD_OR_CREATE.lock().await;
    if let Some(encoded) = store.get(STORE_ACCOUNT).await? {
        if encoded.len() > 4_096 {
            return Err("saved endpoint identity is too large".into());
        }
        let persisted: PersistedIdentity = serde_json::from_str(&encoded)
            .map_err(|_| "saved endpoint identity is malformed".to_string())?;
        if persisted.schema_version != 1 || persisted.algorithm != "Ed25519" {
            return Err("saved endpoint identity has an unsupported version".into());
        }
        let secret = URL_SAFE_NO_PAD
            .decode(&persisted.secret_key)
            .map_err(|_| "saved endpoint secret is malformed".to_string())?;
        let secret: [u8; 32] = secret
            .try_into()
            .map_err(|_| "saved endpoint secret has the wrong size".to_string())?;
        let identity = EndpointIdentity::from_secret(secret)?;
        if identity.public_key != persisted.public_key {
            return Err("saved endpoint public identity does not match its private key".into());
        }
        return Ok(identity);
    }

    let mut secret = [0_u8; 32];
    OsRng.fill_bytes(&mut secret);
    let signing_key = SigningKey::from_bytes(&secret);
    let identity = EndpointIdentity::from_secret(signing_key.to_bytes())?;
    let persisted = PersistedIdentity {
        schema_version: 1,
        algorithm: "Ed25519".into(),
        secret_key: URL_SAFE_NO_PAD.encode(signing_key.to_bytes()),
        public_key: identity.public_key.clone(),
    };
    let encoded = serde_json::to_string(&persisted)
        .map_err(|_| "could not encode endpoint identity".to_string())?;
    store.put(STORE_ACCOUNT, &encoded).await?;
    // Read-after-write catches credential-store truncation or unexpected
    // replacement before this identity is admitted to a live call.
    let stored = store
        .get(STORE_ACCOUNT)
        .await?
        .ok_or("endpoint identity did not persist")?;
    if stored != encoded {
        return Err("endpoint identity persistence verification failed".into());
    }
    Ok(identity)
}

/// Values longer than one Windows credential holds.
///
/// Credential Manager keeps at most 2,560 bytes per credential and `keyring`
/// stores a password as UTF-16, so one entry carries 1,280 UTF-16 units; a
/// refresh token may reach 16 KiB. A longer value is split across sibling
/// entries named after the account, and the account's own entry then holds a
/// manifest: the part count and the SHA-256 of the whole. Values that fit are
/// stored exactly as before, so entries written by earlier builds read
/// unchanged. Parts carry a generation taken from the hash, so a rewrite never
/// overwrites the parts a concurrent reader is still joining.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod credential_parts {
    use sha2::{Digest, Sha256};

    pub(crate) const MAX_UNITS_PER_ENTRY: usize = 1_280;
    const PART_UNITS: usize = 1_200;
    pub(crate) const MAX_PARTS: usize = 32;
    const MANIFEST_PREFIX: &str = "aokie-credential-parts-v1:";

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Manifest {
        pub(crate) generation: String,
        pub(crate) count: usize,
        sha256: String,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Layout {
        Single(String),
        Parts {
            manifest: String,
            generation: String,
            parts: Vec<String>,
        },
    }

    pub(crate) fn plan(value: &str) -> Result<Layout, String> {
        if value.encode_utf16().count() <= MAX_UNITS_PER_ENTRY
            && !value.starts_with(MANIFEST_PREFIX)
        {
            return Ok(Layout::Single(value.to_owned()));
        }
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut units = 0;
        for c in value.chars() {
            if units + c.len_utf16() > PART_UNITS {
                parts.push(std::mem::take(&mut current));
                units = 0;
            }
            current.push(c);
            units += c.len_utf16();
        }
        if !current.is_empty() {
            parts.push(current);
        }
        if parts.len() > MAX_PARTS {
            return Err("value is too large for Windows Credential Manager".into());
        }
        let sha256 = format!("{:x}", Sha256::digest(value.as_bytes()));
        let generation = sha256[..12].to_owned();
        Ok(Layout::Parts {
            manifest: format!("{MANIFEST_PREFIX}{generation}:{}:{sha256}", parts.len()),
            generation,
            parts,
        })
    }

    pub(crate) fn parse_manifest(value: &str) -> Option<Manifest> {
        let rest = value.strip_prefix(MANIFEST_PREFIX)?;
        let mut fields = rest.split(':');
        let generation = fields.next()?;
        let count = fields.next()?.parse::<usize>().ok()?;
        let sha256 = fields.next()?;
        if fields.next().is_some()
            || generation.len() != 12
            || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !(1..=MAX_PARTS).contains(&count)
            || sha256.len() != 64
            || !sha256.starts_with(generation)
        {
            return None;
        }
        Some(Manifest {
            generation: generation.to_owned(),
            count,
            sha256: sha256.to_owned(),
        })
    }

    pub(crate) fn part_account(account: &str, generation: &str, index: usize) -> String {
        format!("{account}#part-{generation}-{index}")
    }

    /// Join parts read back in order; a hash mismatch is an error, never an
    /// absent value.
    pub(crate) fn join(manifest: &Manifest, parts: &[String]) -> Result<String, String> {
        let joined = parts.concat();
        if parts.len() != manifest.count
            || format!("{:x}", Sha256::digest(joined.as_bytes())) != manifest.sha256
        {
            return Err("Credential Manager value parts do not match their manifest".into());
        }
        Ok(joined)
    }
}

/// Write `value` under `account` in Credential Manager, splitting it when it
/// is longer than one credential holds (see [`credential_parts`]).
#[cfg(target_os = "windows")]
pub(crate) fn windows_credential_put(
    service: &str,
    account: &str,
    value: &str,
) -> Result<(), String> {
    use credential_parts::{parse_manifest, part_account, plan, Layout};
    let entry = |name: &str| {
        keyring::Entry::new(service, name)
            .map_err(|error| format!("Credential Manager entry: {error}"))
    };
    let previous = match entry(account)?.get_password() {
        Ok(value) => parse_manifest(&value),
        Err(_) => None,
    };
    let new_generation = match plan(value)? {
        Layout::Single(value) => {
            entry(account)?
                .set_password(&value)
                .map_err(|error| format!("Credential Manager write: {error}"))?;
            None
        }
        Layout::Parts {
            manifest,
            generation,
            parts,
        } => {
            for (index, part) in parts.iter().enumerate() {
                entry(&part_account(account, &generation, index))?
                    .set_password(part)
                    .map_err(|error| format!("Credential Manager write: {error}"))?;
            }
            entry(account)?
                .set_password(&manifest)
                .map_err(|error| format!("Credential Manager write: {error}"))?;
            Some(generation)
        }
    };
    // The manifest now names the new value; retire the previous parts.
    if let Some(previous) = previous {
        if new_generation.as_deref() != Some(previous.generation.as_str()) {
            for index in 0..previous.count {
                if let Ok(part) = entry(&part_account(account, &previous.generation, index)) {
                    let _ = part.delete_credential();
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_credential_get(
    service: &str,
    account: &str,
) -> Result<Option<String>, String> {
    use credential_parts::{join, parse_manifest, part_account};
    let entry = |name: &str| {
        keyring::Entry::new(service, name)
            .map_err(|error| format!("Credential Manager entry: {error}"))
    };
    let value = match entry(account)?.get_password() {
        Ok(value) => value,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(error) => return Err(format!("Credential Manager read: {error}")),
    };
    let Some(manifest) = parse_manifest(&value) else {
        return Ok(Some(value));
    };
    let mut parts = Vec::with_capacity(manifest.count);
    for index in 0..manifest.count {
        match entry(&part_account(account, &manifest.generation, index))?.get_password() {
            Ok(part) => parts.push(part),
            // A manifest without its parts is damage, not absence.
            Err(error) => return Err(format!("Credential Manager read of a value part: {error}")),
        }
    }
    join(&manifest, &parts).map(Some)
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_credential_delete(service: &str, account: &str) -> Result<(), String> {
    use credential_parts::{parse_manifest, part_account};
    let entry = |name: &str| {
        keyring::Entry::new(service, name)
            .map_err(|error| format!("Credential Manager entry: {error}"))
    };
    let manifest = match entry(account)?.get_password() {
        Ok(value) => parse_manifest(&value),
        Err(_) => None,
    };
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(error) => return Err(format!("Credential Manager delete: {error}")),
    }
    if let Some(manifest) = manifest {
        for index in 0..manifest.count {
            match entry(&part_account(account, &manifest.generation, index))?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(error) => {
                    return Err(format!(
                        "Credential Manager delete of a value part: {error}"
                    ))
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) async fn native_store_put(
    _app: &AppHandle,
    account: &str,
    value: &str,
) -> Result<(), String> {
    let account = account.to_owned();
    let value = value.to_owned();
    tauri::async_runtime::spawn_blocking(move || {
        windows_credential_put(KEYRING_SERVICE, &account, &value)
    })
    .await
    .map_err(|_| "Credential Manager write task failed".to_string())?
}

#[cfg(target_os = "android")]
pub(crate) async fn native_store_put(
    app: &AppHandle,
    account: &str,
    value: &str,
) -> Result<(), String> {
    crate::android_runtime::secure_store_put(app, account, value).await
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
pub(crate) async fn native_store_put(
    _app: &AppHandle,
    _account: &str,
    _value: &str,
) -> Result<(), String> {
    Err("persistent endpoint identity is unavailable on this platform".into())
}

#[cfg(target_os = "windows")]
pub(crate) async fn native_store_get(
    _app: &AppHandle,
    account: &str,
) -> Result<Option<String>, String> {
    let account = account.to_owned();
    tauri::async_runtime::spawn_blocking(move || windows_credential_get(KEYRING_SERVICE, &account))
        .await
        .map_err(|_| "Credential Manager read task failed".to_string())?
}

#[cfg(target_os = "android")]
pub(crate) async fn native_store_get(
    app: &AppHandle,
    account: &str,
) -> Result<Option<String>, String> {
    crate::android_runtime::secure_store_get(app, account).await
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
pub(crate) async fn native_store_get(
    _app: &AppHandle,
    _account: &str,
) -> Result<Option<String>, String> {
    Err("persistent endpoint identity is unavailable on this platform".into())
}

#[cfg(target_os = "windows")]
pub(crate) async fn native_store_delete(_app: &AppHandle, account: &str) -> Result<(), String> {
    let account = account.to_owned();
    tauri::async_runtime::spawn_blocking(move || {
        windows_credential_delete(KEYRING_SERVICE, &account)
    })
    .await
    .map_err(|_| "Credential Manager delete task failed".to_string())?
}

#[cfg(target_os = "android")]
pub(crate) async fn native_store_delete(app: &AppHandle, account: &str) -> Result<(), String> {
    crate::android_runtime::secure_store_delete(app, account).await
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
pub(crate) async fn native_store_delete(_app: &AppHandle, _account: &str) -> Result<(), String> {
    Err("persistent endpoint identity is unavailable on this platform".into())
}

#[cfg(test)]
fn unix_now() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before Unix epoch".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aokie_protocol::v2::{AdmissionRole, HelloProofClaims};

    #[test]
    fn install_identity_signs_a_self_verifying_hello_proof() {
        let identity = EndpointIdentity::from_secret([7; 32]).unwrap();
        let now = unix_now().unwrap();
        let proof = identity
            .sign_hello(HelloProofClaims {
                app_id: "app_a".into(),
                subject_id: "device_a".into(),
                role: AdmissionRole::Mobile,
                connection_id: "connection_a".into(),
                challenge_nonce: "challenge_a".into(),
                admission_jti: "admission_a".into(),
                session_nonce: "session_a".into(),
                holder_key_thumbprint: identity.thumbprint().into(),
                expected_peer_key_thumbprint: Some("desktop_thumbprint_a".into()),
                approved_peer_key_thumbprints: Vec::new(),
                peer_roster_revision: None,
                peer_roster_hash: None,
                nonce: "proof_nonce_a".into(),
                jti: "proof_jti_a".into(),
                issued_at: now,
                expires_at: now + 20,
            })
            .unwrap();
        proof.verify(now).unwrap();
        assert_eq!(proof.endpoint_key.thumbprint, identity.thumbprint());
    }

    /// A store whose reads can be made to fail, recording every write.
    #[derive(Default)]
    struct FakeStore {
        values: std::sync::Mutex<std::collections::HashMap<String, String>>,
        fail_reads: std::sync::atomic::AtomicBool,
        writes: std::sync::atomic::AtomicUsize,
    }

    impl IdentityStore for FakeStore {
        async fn get(&self, account: &str) -> Result<Option<String>, String> {
            if self.fail_reads.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("Android Keystore decrypt failed".into());
            }
            // Yield so concurrent callers really interleave.
            tokio::task::yield_now().await;
            Ok(self.values.lock().unwrap().get(account).cloned())
        }

        async fn put(&self, account: &str, value: &str) -> Result<(), String> {
            tokio::task::yield_now().await;
            self.writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.values
                .lock()
                .unwrap()
                .insert(account.to_owned(), value.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_failed_read_never_mints_a_replacement_identity() {
        let store = FakeStore::default();
        let original = load_or_create_in(&store).await.unwrap();
        assert_eq!(store.writes.load(std::sync::atomic::Ordering::SeqCst), 1);

        store
            .fail_reads
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(load_or_create_in(&store).await.is_err());
        // Nothing was written over the identity every Desktop has pinned.
        assert_eq!(store.writes.load(std::sync::atomic::Ordering::SeqCst), 1);

        store
            .fail_reads
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let again = load_or_create_in(&store).await.unwrap();
        assert_eq!(again.thumbprint(), original.thumbprint());
    }

    #[tokio::test]
    async fn concurrent_first_launches_mint_exactly_one_identity() {
        let store = std::sync::Arc::new(FakeStore::default());
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                load_or_create_in(store.as_ref())
                    .await
                    .map(|identity| identity.thumbprint().to_owned())
            }));
        }
        let mut thumbprints = std::collections::HashSet::new();
        for task in tasks {
            thumbprints.insert(task.await.unwrap().unwrap());
        }
        assert_eq!(thumbprints.len(), 1);
        assert_eq!(store.writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_hello_proof_dated_by_the_gateway_clock_still_passes_its_self_check() {
        // The proof window may sit wholly in this phone's past when its clock
        // runs ahead of the gateway's; the self-check must not refuse it.
        let identity = EndpointIdentity::from_secret([9; 32]).unwrap();
        let issued_at = unix_now().unwrap() - 120;
        identity
            .sign_hello(HelloProofClaims {
                app_id: "app_a".into(),
                subject_id: "device_a".into(),
                role: AdmissionRole::Mobile,
                connection_id: "connection_a".into(),
                challenge_nonce: "challenge_a".into(),
                admission_jti: "admission_a".into(),
                session_nonce: "session_a".into(),
                holder_key_thumbprint: identity.thumbprint().into(),
                expected_peer_key_thumbprint: Some("desktop_thumbprint_a".into()),
                approved_peer_key_thumbprints: Vec::new(),
                peer_roster_revision: None,
                peer_roster_hash: None,
                nonce: "proof_nonce_b".into(),
                jti: "proof_jti_b".into(),
                issued_at,
                expires_at: issued_at + 25,
            })
            .unwrap();
    }

    #[test]
    fn short_credential_values_are_stored_whole_as_before() {
        use credential_parts::{plan, Layout};
        let short = "{\"schemaVersion\":1}".to_owned();
        assert_eq!(plan(&short).unwrap(), Layout::Single(short.clone()));
        let at_limit = "a".repeat(credential_parts::MAX_UNITS_PER_ENTRY);
        assert!(matches!(plan(&at_limit).unwrap(), Layout::Single(_)));
    }

    #[test]
    fn long_credential_values_split_into_bounded_parts_and_join_back() {
        use credential_parts::{join, parse_manifest, plan, Layout, MAX_UNITS_PER_ENTRY};
        // A 16 KiB refresh token, and a value with multi-unit characters.
        for value in [
            "r".repeat(16 * 1024),
            format!("{}{}", "é".repeat(900), "😀".repeat(700)),
        ] {
            let Layout::Parts {
                manifest,
                generation,
                parts,
            } = plan(&value).unwrap()
            else {
                panic!("a long value must be split");
            };
            assert!(manifest.encode_utf16().count() <= MAX_UNITS_PER_ENTRY);
            for part in &parts {
                assert!(part.encode_utf16().count() <= MAX_UNITS_PER_ENTRY);
            }
            let parsed = parse_manifest(&manifest).unwrap();
            assert_eq!(parsed.generation, generation);
            assert_eq!(parsed.count, parts.len());
            assert_eq!(join(&parsed, &parts).unwrap(), value);
            // A missing or altered part is an error, not an absent value.
            assert!(join(&parsed, &parts[..parts.len() - 1]).is_err());
            let mut altered = parts.clone();
            altered[0].push('x');
            assert!(join(&parsed, &altered).is_err());
        }
        // A value that merely looks like a manifest is split, never stored
        // where a reader would mistake it for one.
        assert!(matches!(
            plan("aokie-credential-parts-v1:short").unwrap(),
            Layout::Parts { .. }
        ));
        assert!(plan(&"x".repeat(64 * 1024)).is_err());
        assert!(parse_manifest("aokie-credential-parts-v1:nothex:1:abc").is_none());
    }
}
