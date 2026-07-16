//! Native-owned compatible-server profiles and discovery-key trust.
//!
//! The WebView may choose an origin and explicitly approve a displayed
//! fingerprint. It never receives OAuth credentials, endpoint private keys,
//! refresh tokens, admission tokens, or TURN credentials.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, State};
use tokio::sync::Mutex;
use url::Url;

use crate::discovery::{fetch_discovery, DiscoveryDocument};
use crate::endpoint_identity::{
    load_or_create, native_store_delete, native_store_get, native_store_put,
};
use crate::managed_auth::{
    active_profile_id, managed_authorize_profile, managed_forget_profile, managed_restore_profile,
    ManagedAuthState, ManagedConnectConfig,
};

const AUTHORIZATION_TTL_SECONDS: u64 = 5 * 60;
const PROFILE_INDEX_ACCOUNT: &str = "aokie-server-profile-index-v1";
const PROFILE_ACCOUNT_PREFIX: &str = "aokie-server-profile-v1:";
const MAX_PROFILES: usize = 16;
const MAX_PROFILE_BYTES: usize = 3_200;

#[derive(Clone, Default)]
pub struct ServerProfileState {
    pending: Arc<Mutex<HashMap<String, PendingAuthorization>>>,
}

#[derive(Clone)]
struct PendingAuthorization {
    authorization_id: String,
    profile_id: String,
    discovery_url: String,
    origin: String,
    deployment_id: String,
    app_id: String,
    device_id: String,
    discovery_fingerprint: String,
    endpoint_fingerprint: String,
    previous_discovery_fingerprint: Option<String>,
    trust_state: ServerTrustState,
    expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ServerTrustState {
    FirstUse,
    Trusted,
    RotationRequired,
    IdentityChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedProfileIndex {
    schema_version: u8,
    profile_ids: Vec<String>,
}

impl Default for PersistedProfileIndex {
    fn default() -> Self {
        Self {
            schema_version: 1,
            profile_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedServerProfile {
    schema_version: u8,
    profile_id: String,
    discovery_url: String,
    origin: String,
    deployment_id: String,
    app_id: String,
    device_id: String,
    discovery_fingerprint: String,
    endpoint_fingerprint: String,
    confirmed_at: u64,
    updated_at: u64,
}

/// Narrow, credential-free profile binding used by the native Desktop
/// pairing ceremony. OAuth material and server URLs never cross this seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairingProfileBinding {
    pub(crate) profile_id: String,
    pub(crate) app_id: String,
    pub(crate) device_id: String,
    pub(crate) endpoint_key_thumbprint: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BeginCustomServerAuthorizationRequest {
    server_url: String,
    #[serde(default)]
    app_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeginCustomServerAuthorizationResponse {
    authorization_id: String,
    profile_id: String,
    origin: String,
    discovery_fingerprint: String,
    endpoint_fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_discovery_fingerprint: Option<String>,
    trust_state: ServerTrustState,
    expires_at: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmCustomServerTrustRequest {
    authorization_id: String,
    discovery_fingerprint: String,
    approved: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmCustomServerTrustResponse {
    profile_id: String,
    device_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileIdRequest {
    profile_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProfileSummary {
    profile_id: String,
    server_url: String,
    origin: String,
    deployment_id: String,
    app_id: String,
    device_id: String,
    discovery_fingerprint: String,
    endpoint_fingerprint: String,
    trust_state: &'static str,
    active: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetServerProfileResponse {
    profile_id: String,
    forgotten: bool,
    remote_cleanup: &'static str,
}

#[tauri::command]
pub async fn native_begin_custom_server_authorization(
    app: AppHandle,
    state: State<'_, ServerProfileState>,
    request: BeginCustomServerAuthorizationRequest,
) -> Result<BeginCustomServerAuthorizationResponse, String> {
    begin_authorization(&app, state.inner(), request).await
}

async fn begin_authorization(
    app: &AppHandle,
    state: &ServerProfileState,
    request: BeginCustomServerAuthorizationRequest,
) -> Result<BeginCustomServerAuthorizationResponse, String> {
    let discovery_url = normalize_discovery_url(&request.server_url)?;
    let discovery = fetch_discovery(&discovery_url).await?;
    validate_profile_discovery(&discovery)?;
    let app_id = select_app_id(&discovery, request.app_id.as_deref())?;
    let discovery_fingerprint = discovery
        .signing_key_fingerprint
        .clone()
        .ok_or("compatible server discovery omitted a verified signing-key fingerprint")?;
    let endpoint_fingerprint = load_or_create(app).await?.thumbprint().to_owned();
    let profile_id = profile_id(&discovery.issuer, &discovery.deployment_id, &app_id);
    let existing = load_profile(app, &profile_id).await?;
    let (device_id, previous_discovery_fingerprint, trust_state) = match existing.as_ref() {
        Some(profile) if profile.endpoint_fingerprint != endpoint_fingerprint => (
            profile.device_id.clone(),
            Some(profile.discovery_fingerprint.clone()),
            ServerTrustState::IdentityChanged,
        ),
        Some(profile) if profile.discovery_fingerprint == discovery_fingerprint => (
            profile.device_id.clone(),
            Some(profile.discovery_fingerprint.clone()),
            ServerTrustState::Trusted,
        ),
        Some(profile) => (
            profile.device_id.clone(),
            Some(profile.discovery_fingerprint.clone()),
            ServerTrustState::RotationRequired,
        ),
        None => (
            format!("mobile_{}", random_base64url(18)),
            None,
            ServerTrustState::FirstUse,
        ),
    };
    let now = unix_now()?;
    let authorization_id = format!("authorization_{}", random_base64url(24));
    let origin = normalized_origin(&discovery.issuer)?;
    let pending = PendingAuthorization {
        authorization_id: authorization_id.clone(),
        profile_id: profile_id.clone(),
        discovery_url,
        origin: origin.clone(),
        deployment_id: discovery.deployment_id,
        app_id,
        device_id,
        discovery_fingerprint: discovery_fingerprint.clone(),
        endpoint_fingerprint: endpoint_fingerprint.clone(),
        previous_discovery_fingerprint: previous_discovery_fingerprint.clone(),
        trust_state,
        expires_at: now.saturating_add(AUTHORIZATION_TTL_SECONDS),
    };
    let response = BeginCustomServerAuthorizationResponse {
        authorization_id: authorization_id.clone(),
        profile_id,
        origin,
        discovery_fingerprint,
        endpoint_fingerprint,
        previous_discovery_fingerprint,
        trust_state,
        expires_at: pending.expires_at,
    };
    let mut pending_map = state.pending.lock().await;
    pending_map.retain(|_, candidate| candidate.expires_at > now);
    pending_map.insert(authorization_id, pending);
    Ok(response)
}

#[tauri::command]
pub async fn native_confirm_custom_server_trust(
    app: AppHandle,
    state: State<'_, ServerProfileState>,
    managed_auth: State<'_, ManagedAuthState>,
    request: ConfirmCustomServerTrustRequest,
) -> Result<ConfirmCustomServerTrustResponse, String> {
    validate_id(&request.authorization_id, "authorizationId")?;
    validate_id(&request.discovery_fingerprint, "discoveryFingerprint")?;
    let now = unix_now()?;
    let pending = {
        let mut pending_map = state.pending.lock().await;
        pending_map.retain(|_, candidate| candidate.expires_at > now);
        pending_map
            .get(&request.authorization_id)
            .cloned()
            .ok_or("custom-server trust confirmation is missing or expired")?
    };
    if pending.authorization_id != request.authorization_id
        || pending.discovery_fingerprint != request.discovery_fingerprint
        || pending.expires_at <= now
    {
        return Err("custom-server trust confirmation crossed its fingerprint fence".into());
    }
    if !request.approved {
        state.pending.lock().await.remove(&request.authorization_id);
        return Err("custom-server trust was rejected by the owner".into());
    }

    // Re-fetch inside managed authorization and require the exact fingerprint
    // approved above. A DNS/TLS/key change between confirmation and OAuth is
    // therefore fatal rather than silently re-pinned.
    managed_authorize_profile(
        &app,
        managed_auth.inner(),
        pending.discovery_url.clone(),
        pending.device_id.clone(),
        Some(pending.app_id.clone()),
        pending.profile_id.clone(),
        Some(&pending.discovery_fingerprint),
    )
    .await?;

    let profile = PersistedServerProfile {
        schema_version: 1,
        profile_id: pending.profile_id.clone(),
        discovery_url: pending.discovery_url,
        origin: pending.origin,
        deployment_id: pending.deployment_id,
        app_id: pending.app_id,
        device_id: pending.device_id.clone(),
        discovery_fingerprint: pending.discovery_fingerprint,
        endpoint_fingerprint: pending.endpoint_fingerprint,
        confirmed_at: now,
        updated_at: now,
    };
    if let Err(error) = store_profile(&app, &profile).await {
        let _ = managed_forget_profile(&app, managed_auth.inner(), Some(&profile.profile_id)).await;
        return Err(format!(
            "trusted server profile could not be persisted: {error}"
        ));
    }
    state.pending.lock().await.remove(&request.authorization_id);
    Ok(ConfirmCustomServerTrustResponse {
        profile_id: profile.profile_id,
        device_id: profile.device_id,
    })
}

#[tauri::command]
pub async fn native_connect_profile(
    app: AppHandle,
    managed_auth: State<'_, ManagedAuthState>,
    request: ProfileIdRequest,
) -> Result<ManagedConnectConfig, String> {
    validate_id(&request.profile_id, "profileId")?;
    let profile = load_profile(&app, &request.profile_id)
        .await?
        .ok_or("server profile does not exist")?;
    let current_endpoint = load_or_create(&app).await?.thumbprint().to_owned();
    if current_endpoint != profile.endpoint_fingerprint {
        return Err(
            "the install identity changed; explicitly re-enroll this server profile".into(),
        );
    }
    managed_restore_profile(&app, managed_auth.inner(), Some(&profile.profile_id))
        .await?
        .ok_or("this server profile needs native authorization before it can connect".into())
}

#[tauri::command]
pub async fn native_list_server_profiles(
    app: AppHandle,
) -> Result<Vec<ServerProfileSummary>, String> {
    let active = active_profile_id(&app).await?;
    let index = load_index(&app).await?;
    let mut profiles = Vec::with_capacity(index.profile_ids.len());
    for profile_id in index.profile_ids {
        let Some(profile) = load_profile(&app, &profile_id).await? else {
            continue;
        };
        profiles.push(ServerProfileSummary {
            profile_id: profile.profile_id.clone(),
            server_url: profile.discovery_url,
            origin: profile.origin,
            deployment_id: profile.deployment_id,
            app_id: profile.app_id,
            device_id: profile.device_id,
            discovery_fingerprint: profile.discovery_fingerprint,
            endpoint_fingerprint: profile.endpoint_fingerprint,
            trust_state: "trusted",
            active: active.as_deref() == Some(profile.profile_id.as_str()),
        });
    }
    profiles.sort_by(|left, right| {
        left.origin
            .cmp(&right.origin)
            .then(left.app_id.cmp(&right.app_id))
    });
    Ok(profiles)
}

#[tauri::command]
pub async fn native_rotate_server_trust(
    app: AppHandle,
    state: State<'_, ServerProfileState>,
    request: ProfileIdRequest,
) -> Result<BeginCustomServerAuthorizationResponse, String> {
    validate_id(&request.profile_id, "profileId")?;
    let profile = load_profile(&app, &request.profile_id)
        .await?
        .ok_or("server profile does not exist")?;
    begin_authorization(
        &app,
        state.inner(),
        BeginCustomServerAuthorizationRequest {
            server_url: profile.discovery_url,
            app_id: Some(profile.app_id),
        },
    )
    .await
}

#[tauri::command]
pub async fn native_forget_server_profile(
    app: AppHandle,
    managed_auth: State<'_, ManagedAuthState>,
    request: ProfileIdRequest,
) -> Result<ForgetServerProfileResponse, String> {
    validate_id(&request.profile_id, "profileId")?;
    let profile_id = request.profile_id;
    let cleanup = managed_forget_profile(&app, managed_auth.inner(), Some(&profile_id))
        .await
        .ok();
    // Local deletion is fail-safe and does not depend on remote availability.
    delete_profile(&app, &profile_id).await?;
    Ok(ForgetServerProfileResponse {
        profile_id,
        forgotten: true,
        remote_cleanup: match cleanup {
            Some(outcome) if !outcome.active => "not_active",
            Some(outcome) if outcome.remote_cleanup_succeeded => "completed",
            _ => "best_effort_failed",
        },
    })
}

fn validate_profile_discovery(discovery: &DiscoveryDocument) -> Result<(), String> {
    if discovery.schema_version != 2
        || !discovery.signature_verified
        || discovery.signing_key_fingerprint.is_none()
    {
        return Err("compatible servers must publish signed schema-v2 discovery".into());
    }
    if !discovery.available {
        return Err("this compatible server has not enabled Companion admission".into());
    }
    Ok(())
}

fn select_app_id(discovery: &DiscoveryDocument, requested: Option<&str>) -> Result<String, String> {
    match (discovery.app_id.as_deref(), requested) {
        (Some(signed), Some(requested)) if signed != requested => {
            Err("requested appId does not match signed discovery".into())
        }
        (Some(signed), _) => Ok(signed.to_owned()),
        (None, Some(requested)) => {
            validate_id(requested, "appId")?;
            Ok(requested.to_owned())
        }
        (None, None) => Err("compatible-server enrollment requires an appId".into()),
    }
}

fn normalize_discovery_url(server_url: &str) -> Result<String, String> {
    if server_url.is_empty() || server_url.len() > 1_024 || server_url.chars().any(char::is_control)
    {
        return Err("serverUrl is invalid".into());
    }
    let mut url = Url::parse(server_url).map_err(|_| "serverUrl is invalid".to_string())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("serverUrl contains unsupported URL components".into());
    }
    match url.path() {
        "" | "/" => url.set_path("/.well-known/aokie-companion"),
        path if path.ends_with("aokie-discovery")
            || path.ends_with("/.well-known/aokie-companion") => {}
        _ => {
            return Err("serverUrl must be an origin or an Aokie signed-discovery endpoint".into())
        }
    }
    Ok(url.to_string())
}

fn normalized_origin(issuer: &str) -> Result<String, String> {
    let mut origin = Url::parse(issuer).map_err(|_| "discovery issuer is invalid".to_string())?;
    origin.set_path("");
    origin.set_query(None);
    origin.set_fragment(None);
    Ok(origin.to_string().trim_end_matches('/').to_owned())
}

fn profile_id(issuer: &str, deployment_id: &str, app_id: &str) -> String {
    let digest = Sha256::digest(format!("{issuer}\n{deployment_id}\n{app_id}").as_bytes());
    format!("profile_{}", URL_SAFE_NO_PAD.encode(digest))
}

fn profile_account(profile_id: &str) -> String {
    format!("{PROFILE_ACCOUNT_PREFIX}{profile_id}")
}

async fn load_index(app: &AppHandle) -> Result<PersistedProfileIndex, String> {
    let Some(encoded) = native_store_get(app, PROFILE_INDEX_ACCOUNT).await? else {
        return Ok(PersistedProfileIndex::default());
    };
    if encoded.len() > 2_048 {
        return Err("saved server-profile index is too large".into());
    }
    let index: PersistedProfileIndex = serde_json::from_str(&encoded)
        .map_err(|_| "saved server-profile index is malformed".to_string())?;
    validate_index(&index)?;
    Ok(index)
}

async fn store_index(app: &AppHandle, index: &PersistedProfileIndex) -> Result<(), String> {
    validate_index(index)?;
    let encoded = serde_json::to_string(index)
        .map_err(|_| "server-profile index could not be encoded".to_string())?;
    native_store_put(app, PROFILE_INDEX_ACCOUNT, &encoded).await?;
    if native_store_get(app, PROFILE_INDEX_ACCOUNT)
        .await?
        .as_deref()
        != Some(encoded.as_str())
    {
        return Err("server-profile index persistence verification failed".into());
    }
    Ok(())
}

async fn load_profile(
    app: &AppHandle,
    profile_id: &str,
) -> Result<Option<PersistedServerProfile>, String> {
    validate_id(profile_id, "profileId")?;
    let Some(encoded) = native_store_get(app, &profile_account(profile_id)).await? else {
        return Ok(None);
    };
    if encoded.len() > MAX_PROFILE_BYTES {
        return Err("saved server profile is too large".into());
    }
    let profile: PersistedServerProfile = serde_json::from_str(&encoded)
        .map_err(|_| "saved server profile is malformed".to_string())?;
    validate_profile(&profile)?;
    if profile.profile_id != profile_id {
        return Err("saved server profile crossed its storage key".into());
    }
    Ok(Some(profile))
}

pub(crate) async fn load_active_pairing_profile(
    app: &AppHandle,
    profile_id: &str,
) -> Result<PairingProfileBinding, String> {
    validate_id(profile_id, "profileId")?;
    if active_profile_id(app).await?.as_deref() != Some(profile_id) {
        return Err("Desktop pairing must use the active server profile".into());
    }
    let profile = load_profile(app, profile_id)
        .await?
        .ok_or("server profile does not exist")?;
    Ok(PairingProfileBinding {
        profile_id: profile.profile_id,
        app_id: profile.app_id,
        device_id: profile.device_id,
        endpoint_key_thumbprint: profile.endpoint_fingerprint,
    })
}

async fn store_profile(app: &AppHandle, profile: &PersistedServerProfile) -> Result<(), String> {
    validate_profile(profile)?;
    let encoded = serde_json::to_string(profile)
        .map_err(|_| "server profile could not be encoded".to_string())?;
    if encoded.len() > MAX_PROFILE_BYTES {
        return Err("server profile is too large".into());
    }
    let account = profile_account(&profile.profile_id);
    native_store_put(app, &account, &encoded).await?;
    if native_store_get(app, &account).await?.as_deref() != Some(encoded.as_str()) {
        return Err("server-profile persistence verification failed".into());
    }
    let mut index = load_index(app).await?;
    if !index.profile_ids.contains(&profile.profile_id) {
        if index.profile_ids.len() >= MAX_PROFILES {
            let _ = native_store_delete(app, &account).await;
            return Err("server-profile limit reached".into());
        }
        index.profile_ids.push(profile.profile_id.clone());
        index.profile_ids.sort();
        if let Err(error) = store_index(app, &index).await {
            let _ = native_store_delete(app, &account).await;
            return Err(error);
        }
    }
    Ok(())
}

async fn delete_profile(app: &AppHandle, profile_id: &str) -> Result<(), String> {
    let mut index = load_index(app).await?;
    index
        .profile_ids
        .retain(|candidate| candidate != profile_id);
    store_index(app, &index).await?;
    native_store_delete(app, &profile_account(profile_id)).await
}

fn validate_index(index: &PersistedProfileIndex) -> Result<(), String> {
    if index.schema_version != 1 || index.profile_ids.len() > MAX_PROFILES {
        return Err("saved server-profile index is invalid".into());
    }
    let mut sorted = index.profile_ids.clone();
    for profile_id in &sorted {
        validate_id(profile_id, "profileId")?;
    }
    sorted.sort();
    sorted.dedup();
    if sorted != index.profile_ids {
        return Err("saved server-profile index is not canonical".into());
    }
    Ok(())
}

fn validate_profile(profile: &PersistedServerProfile) -> Result<(), String> {
    if profile.schema_version != 1
        || profile.discovery_url.len() > 1_024
        || profile.origin.len() > 512
        || profile.confirmed_at == 0
        || profile.updated_at < profile.confirmed_at
    {
        return Err("saved server profile is invalid".into());
    }
    for (value, label) in [
        (&profile.profile_id, "profileId"),
        (&profile.deployment_id, "deploymentId"),
        (&profile.app_id, "appId"),
        (&profile.device_id, "deviceId"),
        (&profile.discovery_fingerprint, "discoveryFingerprint"),
        (&profile.endpoint_fingerprint, "endpointFingerprint"),
    ] {
        validate_id(value, label)?;
    }
    if normalize_discovery_url(&profile.discovery_url)? != profile.discovery_url
        || normalized_origin(&profile.origin)? != profile.origin
    {
        return Err("saved server profile URL binding is invalid".into());
    }
    Ok(())
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

fn random_base64url(length: usize) -> String {
    let mut bytes = vec![0_u8; length];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before Unix epoch".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_normalized_to_signed_discovery_without_credentials() {
        assert_eq!(
            normalize_discovery_url("https://pbx.example").unwrap(),
            "https://pbx.example/.well-known/aokie-companion"
        );
        assert!(normalize_discovery_url("https://user:secret@pbx.example").is_err());
        assert!(normalize_discovery_url("https://pbx.example/unrelated").is_err());
    }

    #[test]
    fn profile_identity_is_deterministic_and_app_scoped() {
        let first = profile_id("https://pbx.example/", "deployment_a", "app_a");
        assert_eq!(
            first,
            profile_id("https://pbx.example/", "deployment_a", "app_a")
        );
        assert_ne!(
            first,
            profile_id("https://pbx.example/", "deployment_a", "app_b")
        );
    }

    #[test]
    fn profile_storage_contract_is_strict_and_contains_no_credentials() {
        let profile = PersistedServerProfile {
            schema_version: 1,
            profile_id: "profile_a".into(),
            discovery_url: "https://pbx.example/.well-known/aokie-companion".into(),
            origin: "https://pbx.example".into(),
            deployment_id: "deployment_a".into(),
            app_id: "app_a".into(),
            device_id: "mobile_a".into(),
            discovery_fingerprint: "discovery_key_a".into(),
            endpoint_fingerprint: "mobile_key_a".into(),
            confirmed_at: 1,
            updated_at: 1,
        };
        validate_profile(&profile).unwrap();
        let encoded = serde_json::to_string(&profile).unwrap();
        assert!(!encoded.contains("accessToken"));
        assert!(!encoded.contains("refreshToken"));
        assert!(!encoded.contains("turnCredential"));

        let mut malformed = encoded.trim_end_matches('}').to_owned();
        malformed.push_str(",\"bearer\":\"secret\"}");
        assert!(serde_json::from_str::<PersistedServerProfile>(&malformed).is_err());
    }

    #[test]
    fn profile_index_must_be_sorted_unique_and_bounded() {
        let valid = PersistedProfileIndex {
            schema_version: 1,
            profile_ids: vec!["profile_a".into(), "profile_b".into()],
        };
        validate_index(&valid).unwrap();
        let mut invalid = valid.clone();
        invalid.profile_ids.reverse();
        assert!(validate_index(&invalid).is_err());
    }
}
