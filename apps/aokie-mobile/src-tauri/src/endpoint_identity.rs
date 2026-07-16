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
        proof
            .verify(unix_now()?)
            .map_err(|error| error.to_string())?;
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

pub(crate) async fn load_or_create(app: &AppHandle) -> Result<EndpointIdentity, String> {
    if let Some(encoded) = native_store_get(app, STORE_ACCOUNT).await? {
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
    native_store_put(app, STORE_ACCOUNT, &encoded).await?;
    // Read-after-write catches credential-store truncation or unexpected
    // replacement before this identity is admitted to a live call.
    let stored = native_store_get(app, STORE_ACCOUNT)
        .await?
        .ok_or("endpoint identity did not persist")?;
    if stored != encoded {
        return Err("endpoint identity persistence verification failed".into());
    }
    Ok(identity)
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
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account)
            .map_err(|error| format!("Credential Manager entry: {error}"))?;
        entry
            .set_password(&value)
            .map_err(|error| format!("Credential Manager write: {error}"))
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
    tauri::async_runtime::spawn_blocking(move || {
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account)
            .map_err(|error| format!("Credential Manager entry: {error}"))?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!("Credential Manager read: {error}")),
        }
    })
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
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account)
            .map_err(|error| format!("Credential Manager entry: {error}"))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!("Credential Manager delete: {error}")),
        }
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
}
