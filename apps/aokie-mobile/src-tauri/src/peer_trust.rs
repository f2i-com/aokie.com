//! Owner-confirmed Desktop endpoint pinning.
//!
//! A server challenge may introduce a fingerprint, but cannot approve it.
//! First use and every rotation stop before the v2 hello until the exact
//! in-memory challenge is confirmed and persisted in native secure storage.

use std::collections::HashMap;
use std::sync::Arc;

use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

const CONFIRMATION_TTL_SECONDS: u64 = 5 * 60;

#[derive(Clone, Default)]
pub struct PeerTrustState {
    pending: Arc<Mutex<HashMap<String, PendingPeerTrust>>>,
}

#[derive(Clone)]
struct PendingPeerTrust {
    challenge_id: String,
    profile_id: String,
    app_id: String,
    device_id: String,
    peer_fingerprint: String,
    previous_peer_fingerprint: Option<String>,
    expires_at: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PeerTrustRequiredEvent {
    challenge_id: String,
    profile_id: String,
    app_id: String,
    device_id: String,
    peer_fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_peer_fingerprint: Option<String>,
    rotation: bool,
    expires_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedPeerPin {
    schema_version: u8,
    profile_id: String,
    app_id: String,
    device_id: String,
    peer_fingerprint: String,
    confirmed_at: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmDesktopPeerTrustRequest {
    challenge_id: String,
    profile_id: String,
    peer_fingerprint: String,
    approved: bool,
}

pub(crate) async fn require_confirmed_peer(
    app: &AppHandle,
    state: &PeerTrustState,
    profile_id: &str,
    app_id: &str,
    device_id: &str,
    peer_fingerprint: &str,
) -> Result<(), String> {
    for (value, label) in [
        (profile_id, "profileId"),
        (app_id, "appId"),
        (device_id, "deviceId"),
        (peer_fingerprint, "Desktop peer fingerprint"),
    ] {
        validate_id(value, label)?;
    }
    let previous = load_pin(app, profile_id).await?;
    if let Some(pin) = &previous {
        validate_pin(pin)?;
        if pin.profile_id != profile_id || pin.app_id != app_id || pin.device_id != device_id {
            return Err("saved Desktop peer pin belongs to another profile binding".into());
        }
        if pin.peer_fingerprint == peer_fingerprint {
            return Ok(());
        }
    }

    let now = unix_now()?;
    if state
        .pending
        .lock()
        .await
        .get(profile_id)
        .is_some_and(|pending| {
            pending.app_id == app_id
                && pending.device_id == device_id
                && pending.peer_fingerprint == peer_fingerprint
                && pending.expires_at > now
        })
    {
        return Err("Desktop peer trust confirmation is required before connecting".into());
    }
    let pending = PendingPeerTrust {
        challenge_id: random_id("peer_trust"),
        profile_id: profile_id.to_owned(),
        app_id: app_id.to_owned(),
        device_id: device_id.to_owned(),
        peer_fingerprint: peer_fingerprint.to_owned(),
        previous_peer_fingerprint: previous.map(|pin| pin.peer_fingerprint),
        expires_at: now + CONFIRMATION_TTL_SECONDS,
    };
    state
        .pending
        .lock()
        .await
        .insert(profile_id.to_owned(), pending.clone());
    app.emit(
        "aokie-companion://desktop-peer-trust-required",
        PeerTrustRequiredEvent {
            challenge_id: pending.challenge_id,
            profile_id: pending.profile_id,
            app_id: pending.app_id,
            device_id: pending.device_id,
            peer_fingerprint: pending.peer_fingerprint,
            rotation: pending.previous_peer_fingerprint.is_some(),
            previous_peer_fingerprint: pending.previous_peer_fingerprint,
            expires_at: pending.expires_at,
        },
    )
    .map_err(|_| "could not present Desktop peer trust confirmation".to_string())?;
    Err("Desktop peer trust confirmation is required before connecting".into())
}

#[tauri::command]
pub async fn native_confirm_desktop_peer_trust(
    app: AppHandle,
    state: State<'_, PeerTrustState>,
    request: ConfirmDesktopPeerTrustRequest,
) -> Result<(), String> {
    for (value, label) in [
        (&request.challenge_id, "challengeId"),
        (&request.profile_id, "profileId"),
        (&request.peer_fingerprint, "peerFingerprint"),
    ] {
        validate_id(value, label)?;
    }
    let pending = state
        .pending
        .lock()
        .await
        .remove(&request.profile_id)
        .ok_or("Desktop peer trust challenge is no longer pending")?;
    if pending.challenge_id != request.challenge_id
        || pending.profile_id != request.profile_id
        || pending.peer_fingerprint != request.peer_fingerprint
        || pending.expires_at <= unix_now()?
    {
        return Err("Desktop peer trust confirmation is stale or mismatched".into());
    }
    if !request.approved {
        return Ok(());
    }
    persist_confirmed_peer_pin(
        &app,
        &pending.profile_id,
        &pending.app_id,
        &pending.device_id,
        &pending.peer_fingerprint,
        unix_now()?,
    )
    .await
}

pub(crate) async fn persist_confirmed_peer_pin(
    app: &AppHandle,
    profile_id: &str,
    app_id: &str,
    device_id: &str,
    peer_fingerprint: &str,
    confirmed_at: u64,
) -> Result<(), String> {
    let pin = PersistedPeerPin {
        schema_version: 1,
        profile_id: profile_id.to_owned(),
        app_id: app_id.to_owned(),
        device_id: device_id.to_owned(),
        peer_fingerprint: peer_fingerprint.to_owned(),
        confirmed_at,
    };
    validate_pin(&pin)?;
    let encoded =
        serde_json::to_string(&pin).map_err(|_| "could not encode Desktop peer pin".to_string())?;
    crate::endpoint_identity::native_store_put(app, &pin_account(&pin.profile_id), &encoded)
        .await?;
    let stored = load_pin(app, &pin.profile_id)
        .await?
        .ok_or("Desktop peer pin did not persist")?;
    if stored.peer_fingerprint != pin.peer_fingerprint
        || stored.app_id != pin.app_id
        || stored.device_id != pin.device_id
    {
        return Err("Desktop peer pin persistence verification failed".into());
    }
    Ok(())
}

async fn load_pin(app: &AppHandle, profile_id: &str) -> Result<Option<PersistedPeerPin>, String> {
    let Some(encoded) =
        crate::endpoint_identity::native_store_get(app, &pin_account(profile_id)).await?
    else {
        return Ok(None);
    };
    if encoded.len() > 4_096 {
        return Err("saved Desktop peer pin is too large".into());
    }
    let pin: PersistedPeerPin = serde_json::from_str(&encoded)
        .map_err(|_| "saved Desktop peer pin is malformed".to_string())?;
    validate_pin(&pin)?;
    Ok(Some(pin))
}

fn validate_pin(pin: &PersistedPeerPin) -> Result<(), String> {
    if pin.schema_version != 1 || pin.confirmed_at == 0 {
        return Err("saved Desktop peer pin has an unsupported version".into());
    }
    for (value, label) in [
        (&pin.profile_id, "profileId"),
        (&pin.app_id, "appId"),
        (&pin.device_id, "deviceId"),
        (&pin.peer_fingerprint, "peerFingerprint"),
    ] {
        validate_id(value, label)?;
    }
    Ok(())
}

fn pin_account(profile_id: &str) -> String {
    let digest = Sha256::digest(profile_id.as_bytes());
    format!("aokie-desktop-peer-pin-v1:{digest:x}")
}

fn random_id(prefix: &str) -> String {
    let mut random = [0_u8; 16];
    OsRng.fill_bytes(&mut random);
    format!("{prefix}_{}", hex(&random))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_id(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
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

    #[test]
    fn peer_pin_account_does_not_expose_profile_text() {
        let account = pin_account("deployment_private_customer");
        assert!(account.starts_with("aokie-desktop-peer-pin-v1:"));
        assert!(!account.contains("private_customer"));
    }

    #[test]
    fn persisted_pin_contract_rejects_cross_profile_control_bytes() {
        let pin = PersistedPeerPin {
            schema_version: 1,
            profile_id: "profile_a\nprofile_b".into(),
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            peer_fingerprint: "desktop_a".into(),
            confirmed_at: 1,
        };
        assert!(validate_pin(&pin).is_err());
    }
}
