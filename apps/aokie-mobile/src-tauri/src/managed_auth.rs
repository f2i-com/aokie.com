//! Native managed-deployment OAuth and short-lived admission lifecycle.
//!
//! Authorization uses the RFC 8252 loopback redirect with PKCE S256. OAuth
//! access/refresh credentials never enter the WebView. Windows uses Credential
//! Manager; Android uses an AES-GCM key that never leaves Android Keystore.
//! A minimal native-only session descriptor enables safe restart rehydration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aokie_media::IceServerConfig;
use aokie_protocol::v2::Grant;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::{rngs::OsRng, RngCore};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::redirect::Policy;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

use crate::discovery::{
    deserialize_nullable_unix_timestamp, fetch_discovery, validate_managed_ice_configuration,
    DiscoveryDocument, DiscoveryIceServer, NullableUnixTimestamp,
};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_ADMISSION_RESPONSE_BYTES: usize = 64 * 1024;
const REFRESH_EARLY_SECONDS: u64 = 45;
#[cfg(target_os = "windows")]
const KEYRING_SERVICE: &str = "Aokie Companion OAuth";
const SESSION_INDEX_ACCOUNT: &str = "managed-session-active-v1";
const MAX_PERSISTED_SESSION_BYTES: usize = 2_400;
const MAX_MOBILE_API_BYTES: usize = 512 * 1024;
// Accommodates the server's 4 KiB provider-token ceiling plus the strict JSON
// envelope while remaining far below the response/body limits.
const MAX_MOBILE_REQUEST_BYTES: usize = 8 * 1024;
// Every requested capability remains intersected with the signed discovery
// document.  Private consult is now safe to request because Desktop proves a
// receive-only preparation/owner-epoch rotation before exposing microphone.
const MANAGED_REQUESTED_SCOPES: &[&str] = &[
    "aokie:state",
    "aokie:assistance",
    "aokie:monitor",
    "aokie:consult",
    "aokie:takeover",
    "aokie:resume",
    "aokie:end_caller",
    "offline_access",
];
const MANAGED_ADMISSION_GRANTS: &[&str] = &[
    "state_read",
    "caller_read",
    "captions_read",
    "assistance_read",
    "assistance_respond",
    "monitor",
    "consult",
    "takeover",
    "resume_aokie",
    "end_caller",
    "rtc_signal",
    "participants_read",
    "participant_identity_read",
    "audio_levels_read",
];

#[derive(Clone, Default)]
pub struct ManagedAuthState {
    sessions: std::sync::Arc<Mutex<HashMap<String, ManagedSession>>>,
    lifecycle: std::sync::Arc<Mutex<()>>,
    app: std::sync::Arc<std::sync::OnceLock<AppHandle>>,
    push_watcher_generation: std::sync::Arc<AtomicU64>,
}

#[derive(Clone)]
struct ManagedSession {
    profile_id: String,
    deployment_id: String,
    gateway_url: String,
    oauth_token_url: String,
    oauth_resource: String,
    admission_endpoint: String,
    client_id: String,
    app_id: String,
    device_id: String,
    oauth_device_id: Option<String>,
    access_token: String,
    access_expires_at: u64,
    scopes: Vec<String>,
    discovery_relay_only: bool,
    refresh_account: String,
}

impl ManagedSession {
    fn matches_binding(
        &self,
        profile_id: &str,
        deployment_id: &str,
        app_id: &str,
        device_id: &str,
    ) -> bool {
        self.profile_id == profile_id
            && self.deployment_id == deployment_id
            && self.app_id == app_id
            && self.device_id == device_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedSession {
    schema_version: u8,
    profile_id: String,
    discovery_url: String,
    deployment_id: String,
    app_id: String,
    device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth_device_id: Option<String>,
    discovery_fingerprint: String,
    refresh_account: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedConnectConfig {
    gateway_url: String,
    app_id: String,
    device_id: String,
    access_token: String,
    protocol_version: u16,
    managed_deployment_id: String,
    managed_profile_id: String,
    ice_servers: Vec<IceServerConfig>,
    relay_only: bool,
    local_pilot: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedAdmission {
    pub(crate) gateway_url: String,
    pub(crate) access_token: String,
    pub(crate) expires_at: u64,
    pub(crate) ice_servers: Vec<IceServerConfig>,
    pub(crate) relay_only: bool,
    pub(crate) oauth_device_id: String,
    pub(crate) expected_peer_key_thumbprint: String,
    /// The FormLogic-hosted frame mailbox, when this admission advertises one
    /// AND it survived [`usable_relay_endpoints`]. `None` keeps the session on
    /// the WebSocket gateway: the relay is an additive sibling carrier, never a
    /// substitution for the signed `gateway_url`.
    pub(crate) relay: Option<RelayEndpoints>,
    /// The admission's own granted scopes, as the protocol enum.
    ///
    /// Only the relay path reads these. Over the WebSocket the gateway stamps
    /// `grants` onto every projected frame it emits; on the relay there is no
    /// gateway, so the shim that translates plugin frames has to supply them
    /// from the admission the server actually issued.
    pub(crate) grants: Vec<Grant>,
}

/// The three relay routes an admission may advertise.
///
/// ⚠️ Deliberately NOT `deny_unknown_fields`, unlike every security-bearing
/// document around it. This is an additive transport hint: a server that later
/// advertises another member (the long-poll fallback the relay controller
/// already serves is the obvious next one) must leave this build using the
/// three URLs it does understand, not lose the whole admission over a member it
/// was never taught.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RelayEndpoints {
    pub(crate) challenge_url: String,
    pub(crate) frames_url: String,
    pub(crate) stream_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManagedAdmissionError {
    Policy { code: String, message: String },
    Other(String),
}

impl ManagedAdmissionError {
    pub(crate) fn policy(&self) -> Option<(&str, &str)> {
        match self {
            Self::Policy { code, message } => Some((code, message)),
            Self::Other(_) => None,
        }
    }
}

impl std::fmt::Display for ManagedAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Policy { code, message } => {
                write!(formatter, "managed admission {code}: {message}")
            }
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl From<String> for ManagedAdmissionError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<&str> for ManagedAdmissionError {
    fn from(message: &str) -> Self {
        Self::Other(message.to_owned())
    }
}

pub(crate) struct ManagedForgetOutcome {
    pub(crate) active: bool,
    pub(crate) remote_cleanup_succeeded: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    refresh_token: String,
    scope: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdmissionResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    expires_at: u64,
    gateway_url: String,
    app_id: String,
    subject_id: String,
    role: String,
    holder_key_thumbprint: String,
    expected_peer_key_thumbprint: String,
    scopes: Vec<String>,
    #[serde(default)]
    ice_servers: Vec<DiscoveryIceServer>,
    relay_only: bool,
    #[serde(deserialize_with = "deserialize_nullable_unix_timestamp")]
    turn_credential_expires_at: NullableUnixTimestamp,
    device: DeviceRecord,
    /// Tolerated ahead of the backend that advertises it: this decoder is
    /// `deny_unknown_fields`, so the member has to be accepted before it can
    /// ever arrive. Shipping the two in the other order takes every installed
    /// Companion down with "managed admission response is invalid".
    ///
    /// Held as a raw value rather than a typed member ON PURPOSE. Decoding it
    /// inline would make a malformed or reshaped advertisement fail the whole
    /// admission, when the transport is additive and the correct answer is to
    /// stay on the WebSocket gateway.
    #[serde(default)]
    relay: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceRecord {
    id: String,
    app_id: String,
    subject_id: String,
    role: String,
    display_name: String,
    grants: Vec<String>,
    approved_at: String,
    last_seen_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MobileApiError {
    error: bool,
    code: String,
    message: String,
}

enum MobileHttpFailure {
    Unauthorized,
    Status(StatusCode, Vec<u8>),
    Other(String),
}

pub(crate) struct NativeMobileApiResponse {
    pub(crate) body: Vec<u8>,
    pub(crate) app_id: String,
    pub(crate) device_id: String,
    #[cfg_attr(not(any(target_os = "android", target_os = "ios")), allow(dead_code))]
    pub(crate) status: StatusCode,
}

impl ManagedAuthState {
    async fn session_for_profile(&self, profile_id: &str) -> Option<ManagedSession> {
        self.sessions.lock().await.get(profile_id).cloned()
    }

    async fn store_session(&self, session: ManagedSession) {
        let profile_id = session.profile_id.clone();
        self.sessions.lock().await.insert(profile_id, session);
    }

    #[cfg_attr(not(any(target_os = "android", target_os = "ios")), allow(dead_code))]
    pub(crate) async fn active_binding(
        &self,
        app: &AppHandle,
    ) -> Result<Option<(String, String)>, String> {
        self.bind_app(app)?;
        let _lifecycle = self.lifecycle.lock().await;
        let Some(persisted) = load_persisted_session(app).await? else {
            return Ok(None);
        };
        validate_persisted_session(&persisted)?;
        let session = self.session_for_profile(&persisted.profile_id).await;
        if session.as_ref().is_none_or(|session| {
            !session.matches_binding(
                &persisted.profile_id,
                &persisted.deployment_id,
                &persisted.app_id,
                &persisted.device_id,
            ) || session.oauth_device_id != persisted.oauth_device_id
        }) {
            return Err("saved managed sign-in has not been restored in this process".into());
        }
        Ok(persisted
            .oauth_device_id
            .map(|oauth_device_id| (persisted.app_id, oauth_device_id)))
    }

    fn start_push_watcher(&self, app: &AppHandle) {
        let generation = self
            .push_watcher_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
            .max(1);
        #[cfg(target_os = "android")]
        {
            let state = self.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    if state.push_watcher_generation.load(Ordering::Acquire) != generation {
                        return;
                    }
                    let _ = crate::push_registration::reconcile_android_fcm(&app, &state).await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
        #[cfg(not(target_os = "android"))]
        let _ = (app, generation);
    }

    fn stop_push_watcher(&self) {
        self.push_watcher_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub async fn secure_storage_available(&self, app: &AppHandle) -> bool {
        if cfg!(target_os = "windows") {
            true
        } else if cfg!(target_os = "android") {
            crate::android_runtime::secure_store_available(app).await
        } else {
            false
        }
    }

    pub(crate) async fn admission(
        &self,
        profile_id: &str,
        deployment_id: &str,
        expected_app_id: &str,
        expected_device_id: &str,
        holder_key_thumbprint: &str,
    ) -> Result<ManagedAdmission, ManagedAdmissionError> {
        let app = self
            .app
            .get()
            .ok_or("managed native session has not been initialized")?;
        let _lifecycle = self.lifecycle.lock().await;
        validate_id(profile_id, "managed profileId")?;
        validate_id(deployment_id, "managed deploymentId")?;
        let mut session = self
            .session_for_profile(profile_id)
            .await
            .ok_or("managed profile is not authorized in this process")?;
        if !session.matches_binding(
            profile_id,
            deployment_id,
            expected_app_id,
            expected_device_id,
        ) {
            return Err("managed profile identity does not match the realtime request".into());
        }
        if session.access_expires_at <= unix_now()?.saturating_add(REFRESH_EARLY_SECONDS) {
            session = self.refresh_or_forget(app, profile_id, session).await?;
            self.store_session(session.clone()).await;
        }
        validate_id(holder_key_thumbprint, "endpoint holder key thumbprint")?;
        let admission = match request_admission(&session, holder_key_thumbprint).await {
            Ok(admission) => admission,
            Err(AdmissionFailure::Unauthorized) => {
                // Server-side session expiry/revocation can precede the local
                // clock. Refresh exactly once; refresh-family reuse/revocation
                // remains authoritative at the server.
                session = self.refresh_or_forget(app, profile_id, session).await?;
                self.store_session(session.clone()).await;
                match request_admission(&session, holder_key_thumbprint).await {
                    Ok(admission) => admission,
                    Err(AdmissionFailure::Unauthorized) => {
                        self.forget_locked(app, profile_id, &session.refresh_account)
                            .await;
                        return Err(ManagedAdmissionError::Other(
                            "managed device access was revoked; sign in again".into(),
                        ));
                    }
                    Err(error) => return Err(error.into_managed_error()),
                }
            }
            Err(error) => return Err(error.into_managed_error()),
        };
        if session.oauth_device_id.as_deref() != Some(admission.oauth_device_id.as_str()) {
            if let Some(current) = self.sessions.lock().await.get_mut(profile_id) {
                current.oauth_device_id = Some(admission.oauth_device_id.clone());
            }
            let mut persisted = load_persisted_session(app)
                .await?
                .ok_or("managed session metadata disappeared after admission")?;
            if persisted.profile_id != profile_id
                || persisted.deployment_id != deployment_id
                || persisted.app_id != expected_app_id
                || persisted.device_id != expected_device_id
            {
                return Err("managed admission crossed its persisted profile binding".into());
            }
            persisted.oauth_device_id = Some(admission.oauth_device_id.clone());
            store_persisted_session(app, &persisted).await?;
        }
        Ok(admission)
    }

    fn bind_app(&self, app: &AppHandle) -> Result<(), String> {
        if self.app.get().is_some() {
            return Ok(());
        }
        let _ = self.app.set(app.clone());
        Ok(())
    }

    async fn refresh_or_forget(
        &self,
        app: &AppHandle,
        profile_id: &str,
        session: ManagedSession,
    ) -> Result<ManagedSession, String> {
        if session.profile_id != profile_id {
            return Err("managed refresh crossed its profile binding".into());
        }
        match refresh_access_token(app, session.clone()).await {
            Ok(refreshed) => Ok(refreshed),
            Err(message) => {
                // An invalid/reused refresh family and a failed rotation write
                // both require reauthorization. Leaving an unusable token in
                // native storage would create an infinite restart loop.
                if message.contains("revoked")
                    || message.contains("rejected")
                    || message.contains("rotation could not be persisted")
                {
                    self.forget_locked(app, profile_id, &session.refresh_account)
                        .await;
                }
                Err(message)
            }
        }
    }

    async fn forget_locked(&self, app: &AppHandle, profile_id: &str, refresh_account: &str) {
        let active = load_persisted_session(app)
            .await
            .ok()
            .flatten()
            .is_some_and(|persisted| persisted.profile_id == profile_id);
        if active {
            self.stop_push_watcher();
        }
        self.sessions.lock().await.remove(profile_id);
        let _ = delete_refresh(app, refresh_account).await;
        if active {
            let _ = delete_persisted_session(app).await;
        }
        #[cfg(target_os = "android")]
        if active {
            let _ = crate::android_runtime::invalidate_push_token(app).await;
        }
    }

    pub(crate) async fn mobile_api_request(
        &self,
        app: &AppHandle,
        method: Method,
        route: &str,
        query: &[(&'static str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<NativeMobileApiResponse, String> {
        self.mobile_api_request_expected(app, method, route, query, body, &[StatusCode::OK])
            .await
    }

    pub(crate) async fn mobile_api_request_expected(
        &self,
        app: &AppHandle,
        method: Method,
        route: &str,
        query: &[(&'static str, String)],
        body: Option<Vec<u8>>,
        expected_statuses: &[StatusCode],
    ) -> Result<NativeMobileApiResponse, String> {
        self.bind_app(app)?;
        if expected_statuses.is_empty()
            || expected_statuses.iter().any(|status| {
                !matches!(
                    *status,
                    StatusCode::OK | StatusCode::CREATED | StatusCode::NOT_FOUND
                )
            })
        {
            return Err("mobile API success status policy is invalid".into());
        }
        if body
            .as_ref()
            .is_some_and(|body| body.len() > MAX_MOBILE_REQUEST_BYTES)
        {
            return Err("mobile API request is too large".into());
        }
        let _lifecycle = self.lifecycle.lock().await;
        let persisted = load_persisted_session(app)
            .await?
            .ok_or("managed sign-in is required for Companion data")?;
        let mut session = self
            .session_for_profile(&persisted.profile_id)
            .await
            .ok_or("saved managed sign-in has not been restored in this process")?;
        if !session.matches_binding(
            &persisted.profile_id,
            &persisted.deployment_id,
            &persisted.app_id,
            &persisted.device_id,
        ) {
            return Err("managed mobile session binding is inconsistent".into());
        }
        if session.access_expires_at <= unix_now()?.saturating_add(REFRESH_EARLY_SECONDS) {
            session = self
                .refresh_or_forget(app, &persisted.profile_id, session)
                .await?;
            self.store_session(session.clone()).await;
        }
        let url = mobile_api_url(&session.oauth_resource, route, query)?;
        match send_mobile_api_request(
            &session,
            method.clone(),
            url.clone(),
            body.as_deref(),
            expected_statuses,
        )
        .await
        {
            Ok((status, bytes)) => Ok(NativeMobileApiResponse {
                body: bytes,
                app_id: session.app_id.clone(),
                device_id: session.device_id.clone(),
                status,
            }),
            Err(MobileHttpFailure::Unauthorized) => {
                let refreshed = self
                    .refresh_or_forget(app, &persisted.profile_id, session)
                    .await?;
                self.store_session(refreshed.clone()).await;
                match send_mobile_api_request(
                    &refreshed,
                    method,
                    url,
                    body.as_deref(),
                    expected_statuses,
                )
                .await
                {
                    Ok((status, bytes)) => Ok(NativeMobileApiResponse {
                        body: bytes,
                        app_id: refreshed.app_id.clone(),
                        device_id: refreshed.device_id.clone(),
                        status,
                    }),
                    Err(MobileHttpFailure::Unauthorized) => {
                        self.forget_locked(app, &persisted.profile_id, &refreshed.refresh_account)
                            .await;
                        Err("managed mobile access was revoked; sign in again".into())
                    }
                    Err(error) => Err(mobile_failure_message(error)),
                }
            }
            Err(error) => Err(mobile_failure_message(error)),
        }
    }
}

#[tauri::command]
pub async fn managed_authorize(
    app: AppHandle,
    state: State<'_, ManagedAuthState>,
    discovery_url: String,
    device_id: String,
    app_id: Option<String>,
) -> Result<ManagedConnectConfig, String> {
    if !cfg!(debug_assertions) {
        return Err(
            "release enrollment must use the native server-profile trust confirmation flow".into(),
        );
    }
    let debug_profile_id = format!(
        "debug_{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(
            format!(
                "{discovery_url}\n{}\n{device_id}",
                app_id.as_deref().unwrap_or("")
            )
            .as_bytes()
        ))
    );
    managed_authorize_profile(
        &app,
        state.inner(),
        discovery_url,
        device_id,
        app_id,
        debug_profile_id,
        None,
    )
    .await
}

pub(crate) async fn managed_authorize_profile(
    app: &AppHandle,
    state: &ManagedAuthState,
    discovery_url: String,
    device_id: String,
    app_id: Option<String>,
    profile_id: String,
    expected_discovery_fingerprint: Option<&str>,
) -> Result<ManagedConnectConfig, String> {
    state.bind_app(app)?;
    if !state.secure_storage_available(app).await {
        return Err("managed OAuth requires an available native secure-store backend".into());
    }
    validate_id(&profile_id, "profileId")?;
    validate_id(&device_id, "deviceId")?;
    let _lifecycle = state.lifecycle.lock().await;
    let discovery = fetch_discovery(&discovery_url).await?;
    validate_managed_discovery(&discovery)?;
    let discovery_fingerprint = discovery
        .signing_key_fingerprint
        .clone()
        .ok_or("verified discovery omitted its signing-key fingerprint")?;
    match expected_discovery_fingerprint {
        Some(expected) if expected == discovery_fingerprint => {}
        Some(_) => {
            return Err("the discovery signing key changed after owner trust confirmation".into())
        }
        None if cfg!(debug_assertions) => {}
        None => return Err("release enrollment omitted a confirmed discovery key pin".into()),
    }
    let selected_app_id = match (discovery.app_id.as_deref(), app_id.as_deref()) {
        (Some(signed), Some(requested)) if signed != requested => {
            return Err("requested appId does not match signed app discovery".into());
        }
        (Some(signed), _) => signed.to_owned(),
        (None, Some(requested)) => {
            validate_id(requested, "appId")?;
            requested.to_owned()
        }
        (None, None) => {
            return Err(
                "managed authorization requires an app-specific discovery URL or appId".into(),
            )
        }
    };
    let requested_scopes = managed_requested_scopes(&discovery.scopes_supported)?;
    let token = authorize_code_pkce(
        app,
        &discovery,
        &device_id,
        &selected_app_id,
        &requested_scopes,
    )
    .await?;
    validate_token_response(&token, &requested_scopes)?;
    let refresh_account = refresh_account(&discovery, &device_id)?;
    store_refresh(app, &refresh_account, &token.refresh_token).await?;
    let now = unix_now()?;
    let session = ManagedSession {
        profile_id: profile_id.clone(),
        deployment_id: discovery.deployment_id.clone(),
        gateway_url: discovery.gateway_url.clone().expect("validated gateway"),
        oauth_token_url: discovery.oauth_token_url.clone(),
        oauth_resource: discovery
            .oauth_resource
            .clone()
            .expect("validated resource"),
        admission_endpoint: discovery
            .admission_endpoint
            .clone()
            .expect("validated admission endpoint"),
        client_id: discovery.client_id.clone().expect("validated client"),
        app_id: selected_app_id.clone(),
        device_id: device_id.clone(),
        oauth_device_id: None,
        access_token: token.access_token,
        access_expires_at: now.saturating_add(token.expires_in),
        scopes: token
            .scope
            .split_ascii_whitespace()
            .map(str::to_owned)
            .collect(),
        discovery_relay_only: discovery.relay_only,
        refresh_account: refresh_account.clone(),
    };
    state.store_session(session).await;
    let persisted = PersistedSession {
        schema_version: 2,
        profile_id: profile_id.clone(),
        discovery_url,
        deployment_id: discovery.deployment_id.clone(),
        app_id: selected_app_id.clone(),
        device_id: device_id.clone(),
        oauth_device_id: None,
        discovery_fingerprint,
        refresh_account: refresh_account.clone(),
    };
    if let Err(message) = store_persisted_session(app, &persisted).await {
        state.sessions.lock().await.remove(&profile_id);
        let _ = delete_refresh(app, &refresh_account).await;
        return Err(format!(
            "managed session metadata could not be persisted: {message}"
        ));
    }
    let config = connect_config(&discovery, selected_app_id, device_id, profile_id);
    drop(_lifecycle);
    state.start_push_watcher(app);
    Ok(config)
}

#[tauri::command]
pub async fn managed_restore(
    app: AppHandle,
    state: State<'_, ManagedAuthState>,
) -> Result<Option<ManagedConnectConfig>, String> {
    managed_restore_profile(&app, state.inner(), None).await
}

pub(crate) async fn managed_restore_profile(
    app: &AppHandle,
    state: &ManagedAuthState,
    expected_profile_id: Option<&str>,
) -> Result<Option<ManagedConnectConfig>, String> {
    state.bind_app(app)?;
    if !state.secure_storage_available(app).await {
        return Ok(None);
    }
    let _lifecycle = state.lifecycle.lock().await;
    let Some(persisted) = load_persisted_session(app).await? else {
        return Ok(None);
    };
    if expected_profile_id.is_some_and(|expected| expected != persisted.profile_id) {
        return Ok(None);
    }
    validate_persisted_session(&persisted)?;
    let discovery = fetch_discovery(&persisted.discovery_url).await?;
    validate_managed_discovery(&discovery)?;
    if discovery.deployment_id != persisted.deployment_id
        || discovery
            .app_id
            .as_ref()
            .is_some_and(|app_id| app_id != &persisted.app_id)
        || discovery.signing_key_fingerprint.as_deref()
            != Some(persisted.discovery_fingerprint.as_str())
        || refresh_account(&discovery, &persisted.device_id)? != persisted.refresh_account
    {
        return Err("saved managed session no longer matches signed discovery".into());
    }
    if load_refresh(app, &persisted.refresh_account)
        .await?
        .is_none()
    {
        let _ = delete_persisted_session(app).await;
        return Ok(None);
    }
    let scopes = managed_requested_scopes(&discovery.scopes_supported)?;
    let session = ManagedSession {
        profile_id: persisted.profile_id.clone(),
        deployment_id: persisted.deployment_id.clone(),
        gateway_url: discovery.gateway_url.clone().expect("validated gateway"),
        oauth_token_url: discovery.oauth_token_url.clone(),
        oauth_resource: discovery
            .oauth_resource
            .clone()
            .expect("validated resource"),
        admission_endpoint: discovery
            .admission_endpoint
            .clone()
            .expect("validated admission endpoint"),
        client_id: discovery.client_id.clone().expect("validated client"),
        app_id: persisted.app_id.clone(),
        device_id: persisted.device_id.clone(),
        oauth_device_id: persisted.oauth_device_id.clone(),
        access_token: String::new(),
        access_expires_at: 0,
        scopes,
        discovery_relay_only: discovery.relay_only,
        refresh_account: persisted.refresh_account.clone(),
    };
    let refreshed = state
        .refresh_or_forget(app, &persisted.profile_id, session)
        .await?;
    state.store_session(refreshed).await;
    let config = connect_config(
        &discovery,
        persisted.app_id,
        persisted.device_id,
        persisted.profile_id,
    );
    drop(_lifecycle);
    state.start_push_watcher(app);
    Ok(Some(config))
}

#[tauri::command]
pub async fn managed_forget(
    app: AppHandle,
    state: State<'_, ManagedAuthState>,
) -> Result<(), String> {
    let _ = managed_forget_profile(&app, state.inner(), None).await?;
    Ok(())
}

pub(crate) async fn managed_forget_profile(
    app: &AppHandle,
    state: &ManagedAuthState,
    expected_profile_id: Option<&str>,
) -> Result<ManagedForgetOutcome, String> {
    state.bind_app(app)?;
    if let Some(expected) = expected_profile_id {
        validate_id(expected, "profileId")?;
    }
    let persisted = load_persisted_session(app).await?;
    if let Some(expected) = expected_profile_id {
        let Some(persisted) = persisted.as_ref() else {
            if let Some(session) = state.session_for_profile(expected).await {
                let _lifecycle = state.lifecycle.lock().await;
                state
                    .forget_locked(app, expected, &session.refresh_account)
                    .await;
            }
            return Ok(ManagedForgetOutcome {
                active: false,
                remote_cleanup_succeeded: true,
            });
        };
        if persisted.profile_id != expected {
            if let Some(session) = state.session_for_profile(expected).await {
                let _lifecycle = state.lifecycle.lock().await;
                state
                    .forget_locked(app, expected, &session.refresh_account)
                    .await;
            }
            return Ok(ManagedForgetOutcome {
                active: false,
                remote_cleanup_succeeded: true,
            });
        }
    }
    let active = persisted.is_some();
    state.stop_push_watcher();
    #[cfg(target_os = "android")]
    let remote_cleanup_succeeded = {
        let succeeded = crate::push_registration::unregister_managed_push_endpoint(
            app,
            state,
            crate::push_registration::NativePushKind::Fcm,
        )
        .await
        .is_ok();
        let _ = crate::android_runtime::invalidate_push_token(app).await;
        succeeded
    };
    #[cfg(not(target_os = "android"))]
    let remote_cleanup_succeeded = true;
    let _lifecycle = state.lifecycle.lock().await;
    if let Some(persisted) = persisted {
        state
            .forget_locked(app, &persisted.profile_id, &persisted.refresh_account)
            .await;
    }
    Ok(ManagedForgetOutcome {
        active,
        remote_cleanup_succeeded,
    })
}

pub(crate) async fn active_profile_id(app: &AppHandle) -> Result<Option<String>, String> {
    Ok(load_persisted_session(app)
        .await?
        .map(|session| session.profile_id))
}

fn connect_config(
    discovery: &DiscoveryDocument,
    app_id: String,
    device_id: String,
    profile_id: String,
) -> ManagedConnectConfig {
    ManagedConnectConfig {
        gateway_url: discovery.gateway_url.clone().expect("validated gateway"),
        app_id,
        device_id,
        // Sentinel is never used as a bearer. RealtimeConfig requires a field
        // for backwards compatibility; the managed deployment/profile pair
        // selects native admission and the peer-trust namespace.
        access_token: String::new(),
        protocol_version: 2,
        managed_deployment_id: discovery.deployment_id.clone(),
        managed_profile_id: profile_id,
        ice_servers: discovery.ice_servers.clone(),
        relay_only: discovery.relay_only,
        local_pilot: cfg!(feature = "managed-beta-local"),
    }
}

fn validate_managed_discovery(discovery: &DiscoveryDocument) -> Result<(), String> {
    if discovery.schema_version != 2 || !discovery.signature_verified {
        return Err("managed enrollment requires a verified schema-v2 discovery document".into());
    }
    if !discovery.available {
        return Err("this deployment has not enabled Aokie Companion admission".into());
    }
    if discovery.gateway_url.is_none()
        || discovery.oauth_resource.is_none()
        || discovery.admission_endpoint.is_none()
        || discovery.client_id.is_none()
    {
        return Err("verified discovery omitted managed connection fields".into());
    }
    Ok(())
}

fn managed_requested_scopes(supported_scopes: &[String]) -> Result<Vec<String>, String> {
    let scopes: Vec<String> = MANAGED_REQUESTED_SCOPES
        .iter()
        .filter(|scope| {
            supported_scopes
                .iter()
                .any(|supported| supported == **scope)
        })
        .map(|scope| (*scope).to_owned())
        .collect();
    if !scopes.iter().any(|scope| scope == "aokie:state")
        || !scopes.iter().any(|scope| scope == "offline_access")
    {
        return Err(
            "managed discovery must support aokie:state and rotating offline access".into(),
        );
    }
    Ok(scopes)
}

async fn authorize_code_pkce(
    app: &AppHandle,
    discovery: &DiscoveryDocument,
    device_id: &str,
    app_id: &str,
    requested_scopes: &[String],
) -> Result<TokenResponse, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| "could not bind the private OAuth callback listener")?;
    let port = listener
        .local_addr()
        .map_err(|_| "could not inspect the OAuth callback listener")?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
    let verifier = random_base64url(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_base64url(32);
    let authorization = build_authorization_url(
        &discovery.oauth_authorization_url,
        discovery.client_id.as_deref().expect("validated"),
        &redirect_uri,
        requested_scopes,
        &state,
        &challenge,
        discovery.oauth_resource.as_deref().expect("validated"),
        device_id,
        app_id,
    )?;
    app.opener()
        .open_url(authorization.as_str(), None::<&str>)
        .map_err(|_| "could not open the native browser for authorization")?;
    let code = wait_for_callback(listener, &state).await?;
    exchange_code(discovery, &code, &verifier, &redirect_uri).await
}

#[allow(clippy::too_many_arguments)]
fn build_authorization_url(
    base_url: &str,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    state: &str,
    challenge: &str,
    resource: &str,
    device_id: &str,
    app_id: &str,
) -> Result<Url, String> {
    let mut authorization =
        Url::parse(base_url).map_err(|_| "verified authorization URL became invalid")?;
    let scope = scopes.join(" ");
    {
        let mut query = authorization.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", client_id);
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("scope", &scope);
        query.append_pair("state", state);
        query.append_pair("code_challenge", challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("resource", resource);
        query.append_pair("device", device_id);
        query.append_pair("appId", app_id);
    }
    Ok(authorization)
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String, String> {
    let (mut stream, _) = tokio::time::timeout(CALLBACK_TIMEOUT, listener.accept())
        .await
        .map_err(|_| "authorization timed out before the browser returned")?
        .map_err(|_| "OAuth callback listener failed")?;
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 1024];
        let count = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .map_err(|_| "OAuth callback request timed out")?
            .map_err(|_| "could not read OAuth callback")?;
        if count == 0 {
            break;
        }
        if count > MAX_CALLBACK_BYTES.saturating_sub(bytes.len()) {
            return Err("OAuth callback request exceeded the size limit".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| "OAuth callback was not valid HTTP")?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.strip_suffix(" HTTP/1.1"))
        .ok_or("OAuth callback used an unsupported HTTP request")?;
    let callback = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| "OAuth callback URL is invalid")?;
    if callback.path() != "/oauth/callback" {
        return Err("OAuth callback path is invalid".into());
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in callback.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "error" if error.is_none() => error = Some(value.into_owned()),
            _ => {}
        }
    }
    let successful = error.is_none() && state.as_deref() == Some(expected_state) && code.is_some();
    let body = if successful {
        "Aokie Companion authorization completed. You can close this browser tab."
    } else {
        "Aokie Companion authorization was rejected. Return to the app for details."
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    if let Some(error) = error {
        return Err(format!("authorization server returned {error}"));
    }
    if state.as_deref() != Some(expected_state) {
        return Err("OAuth callback state did not match the native request".into());
    }
    let code = code.ok_or("OAuth callback omitted the authorization code")?;
    if code.is_empty() || code.len() > 4_096 || code.chars().any(char::is_control) {
        return Err("OAuth authorization code is invalid".into());
    }
    Ok(code)
}

async fn exchange_code(
    discovery: &DiscoveryDocument,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, String> {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        (
            "client_id",
            discovery.client_id.as_deref().expect("validated"),
        ),
        (
            "resource",
            discovery.oauth_resource.as_deref().expect("validated"),
        ),
    ];
    token_request(&discovery.oauth_token_url, &form).await
}

async fn refresh_access_token(
    app: &AppHandle,
    mut session: ManagedSession,
) -> Result<ManagedSession, String> {
    let refresh = load_refresh(app, &session.refresh_account)
        .await?
        .ok_or("managed refresh credential is missing from native secure storage")?;
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
        ("client_id", session.client_id.as_str()),
        ("resource", session.oauth_resource.as_str()),
    ];
    let token = token_request(&session.oauth_token_url, &form)
        .await
        .map_err(|message| {
            if message.contains("HTTP 400") || message.contains("HTTP 401") {
                "managed refresh credential was revoked or rejected".to_string()
            } else {
                message
            }
        })?;
    validate_token_response(&token, &session.scopes)?;
    // Public-client refresh rotation retires the old token server-side. The
    // new token must reach Credential Manager before it becomes authoritative
    // in memory; a storage failure is surfaced immediately.
    if store_refresh(app, &session.refresh_account, &token.refresh_token)
        .await
        .is_err()
    {
        let _ = delete_refresh(app, &session.refresh_account).await;
        return Err("managed refresh rotation could not be persisted; sign in again".into());
    }
    session.access_token = token.access_token;
    session.access_expires_at = unix_now()?.saturating_add(token.expires_in);
    session.scopes = token
        .scope
        .split_ascii_whitespace()
        .map(str::to_owned)
        .collect();
    Ok(session)
}

async fn token_request(url: &str, form: &[(&str, &str)]) -> Result<TokenResponse, String> {
    let client = native_client()?;
    let response = client
        .post(url)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .map_err(|_| "OAuth token endpoint is unavailable")?;
    if !response.status().is_success() {
        return Err(format!(
            "OAuth token endpoint returned HTTP {}",
            response.status()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "could not read OAuth token response")?;
    if bytes.len() > 64 * 1024 {
        return Err("OAuth token response is too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "OAuth token response is invalid".into())
}

fn validate_token_response(token: &TokenResponse, allowed_scopes: &[String]) -> Result<(), String> {
    if token.token_type != "Bearer"
        || token.access_token.len() < 16
        || token.access_token.len() > MAX_TOKEN_BYTES
        || token.access_token.chars().any(char::is_control)
        || token.refresh_token.len() < 16
        || token.refresh_token.len() > MAX_TOKEN_BYTES
        || token.refresh_token.chars().any(char::is_control)
        || token.expires_in == 0
        || token.expires_in > 3_600
    {
        return Err("OAuth token response contains invalid credentials or expiry".into());
    }
    let scopes: Vec<_> = token.scope.split_ascii_whitespace().collect();
    if scopes.is_empty()
        || scopes
            .iter()
            .any(|scope| !allowed_scopes.iter().any(|allowed| allowed == scope))
        || !scopes.contains(&"aokie:state")
    {
        return Err("OAuth token response contains unexpected scopes".into());
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum AdmissionFailure {
    Unauthorized,
    Policy { code: String, message: String },
    Other(String),
}

impl AdmissionFailure {
    fn into_managed_error(self) -> ManagedAdmissionError {
        match self {
            Self::Unauthorized => ManagedAdmissionError::Other(
                "managed OAuth access was rejected after refresh".into(),
            ),
            Self::Policy { code, message } => ManagedAdmissionError::Policy { code, message },
            Self::Other(message) => ManagedAdmissionError::Other(message),
        }
    }
}

async fn request_admission(
    session: &ManagedSession,
    holder_key_thumbprint: &str,
) -> Result<ManagedAdmission, AdmissionFailure> {
    let client = native_client().map_err(AdmissionFailure::Other)?;
    let mut response = client
        .post(&session.admission_endpoint)
        .header("accept", "application/json")
        .bearer_auth(&session.access_token)
        .json(&serde_json::json!({
            "appId":session.app_id,
            "deviceId":session.device_id,
            "displayName":"Aokie Companion",
            "holderKeyThumbprint":holder_key_thumbprint,
            // Opt in to the hosted relay. A server that has never heard of the
            // member ignores it — the admission route tolerates unknown request
            // keys — so this is safe against every deployed backend, and a
            // backend that does understand it will not advertise `relay` to a
            // build that stayed silent.
            "supportedTransports":["relay"]
        }))
        .send()
        .await
        .map_err(|_| AdmissionFailure::Other("managed admission endpoint is unavailable".into()))?;
    let status = response.status();
    let response_is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"));
    if status == StatusCode::UNAUTHORIZED {
        return Err(AdmissionFailure::Unauthorized);
    }
    let bytes = read_admission_body(&mut response).await?;
    if !status.is_success() {
        return Err(classify_admission_http_failure(
            status,
            response_is_json,
            &bytes,
        ));
    }
    let admission: AdmissionResponse = serde_json::from_slice(&bytes)
        .map_err(|_| AdmissionFailure::Other("managed admission response is invalid".into()))?;
    let ice_servers = validate_admission(session, &admission, holder_key_thumbprint)
        .map_err(AdmissionFailure::Other)?;
    Ok(ManagedAdmission {
        oauth_device_id: admission.device.id.clone(),
        expected_peer_key_thumbprint: admission.expected_peer_key_thumbprint,
        gateway_url: admission.gateway_url,
        access_token: admission.access_token,
        expires_at: admission.expires_at,
        ice_servers,
        relay_only: admission.relay_only,
        grants: admission_grants(&admission.scopes),
        // Evaluated AFTER validation so a rejected relay can never mask a
        // failed admission, and never so as to fail one.
        relay: admission.relay.and_then(usable_relay_endpoints),
    })
}

async fn read_admission_body(
    response: &mut reqwest::Response,
) -> Result<Vec<u8>, AdmissionFailure> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AdmissionFailure::Other("could not read managed admission".into()))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_ADMISSION_RESPONSE_BYTES {
            return Err(AdmissionFailure::Other(
                "managed admission is too large".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn classify_admission_http_failure(
    status: StatusCode,
    response_is_json: bool,
    bytes: &[u8],
) -> AdmissionFailure {
    if status == StatusCode::UNAUTHORIZED {
        return AdmissionFailure::Unauthorized;
    }
    if response_is_json && bytes.len() <= MAX_ADMISSION_RESPONSE_BYTES {
        let parsed: Result<MobileApiError, _> = serde_json::from_slice(bytes);
        if let Ok(error) = parsed {
            if error.error
                && validate_id(&error.code, "managed admission error code").is_ok()
                && !error.message.is_empty()
                && error.message.len() <= 240
                && !error.message.chars().any(char::is_control)
            {
                return AdmissionFailure::Policy {
                    code: error.code,
                    message: error.message,
                };
            }
        }
    }
    AdmissionFailure::Other(format!(
        "managed admission endpoint returned HTTP {}",
        status.as_u16()
    ))
}

fn validate_admission(
    session: &ManagedSession,
    admission: &AdmissionResponse,
    holder_key_thumbprint: &str,
) -> Result<Vec<IceServerConfig>, String> {
    let now = unix_now()?;
    if admission.token_type != "Bearer"
        || admission.access_token.len() < 16
        || admission.access_token.len() > MAX_TOKEN_BYTES
        || admission.expires_in == 0
        || admission.expires_in > 300
        || admission.expires_at <= now
        || admission.expires_at > now.saturating_add(300)
        || admission.gateway_url != session.gateway_url
        || admission.app_id != session.app_id
        || admission.subject_id != session.device_id
        || admission.role != "mobile"
        || admission.holder_key_thumbprint != holder_key_thumbprint
        || admission.expected_peer_key_thumbprint == holder_key_thumbprint
        || admission.relay_only != session.discovery_relay_only
    {
        return Err("managed admission is not bound to the signed deployment and device".into());
    }
    validate_id(
        &admission.holder_key_thumbprint,
        "admission holder key thumbprint",
    )?;
    validate_id(
        &admission.expected_peer_key_thumbprint,
        "admission expected peer key thumbprint",
    )?;
    validate_id(&admission.device.id, "admission device id")?;
    if admission.device.app_id != session.app_id
        || admission.device.subject_id != session.device_id
        || admission.device.role != "mobile"
        || admission.device.grants != admission.scopes
        || admission.device.display_name.is_empty()
        || admission.device.display_name.len() > 120
        || admission.device.approved_at.is_empty()
        || admission.device.last_seen_at.is_empty()
    {
        return Err("managed admission device record is inconsistent".into());
    }
    if admission.scopes.is_empty()
        || !admission.scopes.iter().any(|grant| grant == "state_read")
        || admission
            .scopes
            .iter()
            .any(|grant| !MANAGED_ADMISSION_GRANTS.contains(&grant.as_str()))
    {
        return Err("managed admission contains invalid grants".into());
    }
    validate_managed_ice_configuration(
        &admission.ice_servers,
        admission.relay_only,
        admission.turn_credential_expires_at.value(),
        now,
    )
}

/// Accept an advertised relay only when it decodes to the shape this build
/// understands, every URL is safe, AND all three share one origin.
///
/// A rejected advertisement degrades to the WebSocket gateway rather than
/// failing the admission: the transport is additive, and refusing the whole
/// admission over it would take the Companion surface down harder than simply
/// not adopting the new path.
pub(crate) fn usable_relay_endpoints(advertisement: serde_json::Value) -> Option<RelayEndpoints> {
    let relay: RelayEndpoints = match serde_json::from_value(advertisement) {
        Ok(relay) => relay,
        Err(_) => {
            eprintln!(
                "[AokieCompanion][relay] advertisement rejected: not the shape this build understands"
            );
            return None;
        }
    };
    let checked = [
        normalize_relay_url(&relay.challenge_url, "challengeUrl"),
        normalize_relay_url(&relay.frames_url, "framesUrl"),
        normalize_relay_url(&relay.stream_url, "streamUrl"),
    ];
    let mut origins = Vec::with_capacity(checked.len());
    for outcome in &checked {
        match outcome {
            Ok(url) => origins.push(url.origin()),
            Err(message) => {
                eprintln!("[AokieCompanion][relay] advertisement rejected: {message}");
                return None;
            }
        }
    }
    if origins.windows(2).any(|pair| pair[0] != pair[1]) {
        eprintln!(
            "[AokieCompanion][relay] advertisement rejected: relay URLs span more than one origin"
        );
        return None;
    }
    Some(relay)
}

/// The relay's own URL gate.
///
/// Deliberately NOT [`managed_gateway_url`]: that one pins the `/v2/realtime`
/// path and the `ws`/`wss` scheme, so every relay URL would fail it. The
/// plaintext carve-out is the same narrow one the rest of this module applies —
/// the exact `api.formlogic.local` WAMP host, and only in a managed-beta-local
/// build.
fn normalize_relay_url(raw: &str, label: &str) -> Result<Url, String> {
    let invalid = |detail: &str| format!("relay {label} {detail}");
    let url = Url::parse(raw).map_err(|_| invalid("is not an absolute URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not contain credentials"));
    }
    if url.fragment().is_some() {
        return Err(invalid("must not contain a fragment"));
    }
    if url.host_str().is_none() {
        return Err(invalid("has no host"));
    }
    let local_beta = cfg!(feature = "managed-beta-local")
        && url.scheme() == "http"
        && url.host_str() == Some("api.formlogic.local");
    if url.scheme() != "https" && !local_beta {
        return Err(invalid("must use https"));
    }
    Ok(url)
}

/// Map the admission's granted scopes onto the protocol enum.
///
/// [`validate_admission`] has already refused anything outside
/// [`MANAGED_ADMISSION_GRANTS`], so an unmapped name cannot reach here; it is
/// dropped rather than guessed at, because a grant this build does not know is
/// a capability it cannot honour anyway.
fn admission_grants(scopes: &[String]) -> Vec<Grant> {
    scopes
        .iter()
        .filter_map(|scope| {
            serde_json::from_value::<Grant>(serde_json::Value::String(scope.clone())).ok()
        })
        .collect()
}

fn native_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|_| "could not initialise native OAuth TLS".into())
}

fn mobile_api_url(
    resource: &str,
    route: &str,
    query: &[(&'static str, String)],
) -> Result<Url, String> {
    const ROUTES: &[&str] = &[
        "/api/aokie-companion/mobile/bootstrap",
        "/api/aokie-companion/mobile/history",
        "/api/aokie-companion/mobile/routing",
        "/api/aokie-companion/mobile/availability",
        "/api/aokie-companion/mobile/call-records",
    ];
    let push_route = valid_push_endpoint_route(route);
    let call_record_detail_route = valid_call_record_detail_route(route);
    let valid_query = match route {
        "/api/aokie-companion/mobile/history" => query
            .iter()
            .all(|(key, _)| matches!(*key, "limit" | "before")),
        "/api/aokie-companion/mobile/call-records" => query.iter().all(|(key, _)| *key == "limit"),
        _ => query.is_empty(),
    };
    if (!ROUTES.contains(&route) && !push_route && !call_record_detail_route) || !valid_query {
        return Err("mobile API route is invalid".into());
    }
    let mut url = Url::parse(resource).map_err(|_| "managed OAuth resource is invalid")?;
    let local_beta = cfg!(feature = "managed-beta-local")
        && url.scheme() == "http"
        && url.host_str() == Some("api.formlogic.local");
    if (url.scheme() != "https" && !local_beta)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err("managed OAuth resource origin is invalid".into());
    }
    url.set_path(route);
    url.set_query(None);
    url.set_fragment(None);
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(key, value);
        }
    }
    Ok(url)
}

fn valid_push_endpoint_route(route: &str) -> bool {
    const PREFIX: &str = "/api/aokie-companion/mobile/devices/";
    let Some(remainder) = route.strip_prefix(PREFIX) else {
        return false;
    };
    let Some((device_id, kind)) = remainder.split_once("/push-endpoints/") else {
        return false;
    };
    validate_id(device_id, "push endpoint OAuth device id").is_ok()
        && matches!(kind, "fcm" | "apns" | "apns_voip")
}

fn valid_call_record_detail_route(route: &str) -> bool {
    const PREFIX: &str = "/api/aokie-companion/mobile/call-records/";
    route
        .strip_prefix(PREFIX)
        .is_some_and(|record_id| validate_id(record_id, "call record id").is_ok())
}

async fn send_mobile_api_request(
    session: &ManagedSession,
    method: Method,
    url: Url,
    body: Option<&[u8]>,
    expected_statuses: &[StatusCode],
) -> Result<(StatusCode, Vec<u8>), MobileHttpFailure> {
    let client = native_client().map_err(MobileHttpFailure::Other)?;
    let mut request = client
        .request(method, url)
        .header(ACCEPT, "application/json")
        // Negotiate the additive routing-member shape explicitly: older
        // native clients strictly reject unknown response fields and must
        // continue receiving schema 1 during a staged rollout.
        .header("x-aokie-companion-routing-schema", "2")
        .bearer_auth(&session.access_token);
    if let Some(body) = body {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_vec());
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| MobileHttpFailure::Other("mobile API is unavailable".into()))?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MobileHttpFailure::Other("mobile API response could not be read".into()))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_MOBILE_API_BYTES {
            return Err(MobileHttpFailure::Other(
                "mobile API response is too large".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if status == StatusCode::UNAUTHORIZED {
        return Err(MobileHttpFailure::Unauthorized);
    }
    if !content_type.starts_with("application/json") {
        return Err(MobileHttpFailure::Other(
            "mobile API response is not JSON".into(),
        ));
    }
    if !expected_statuses.contains(&status) {
        return Err(MobileHttpFailure::Status(status, bytes));
    }
    if bytes.is_empty() {
        return Err(MobileHttpFailure::Other(
            "mobile API returned an empty response".into(),
        ));
    }
    Ok((status, bytes))
}

fn mobile_failure_message(error: MobileHttpFailure) -> String {
    match error {
        MobileHttpFailure::Unauthorized => {
            "managed mobile access was rejected; sign in again".into()
        }
        MobileHttpFailure::Other(message) => message,
        MobileHttpFailure::Status(status, bytes) => {
            let parsed: Result<MobileApiError, _> = serde_json::from_slice(&bytes);
            match parsed {
                Ok(error)
                    if error.error
                        && validate_id(&error.code, "mobile API error code").is_ok()
                        && !error.message.is_empty()
                        && error.message.len() <= 240
                        && !error.message.chars().any(char::is_control) =>
                {
                    format!("mobile API {}: {}", error.code, error.message)
                }
                _ => format!("mobile API returned HTTP {}", status.as_u16()),
            }
        }
    }
}

fn refresh_account(discovery: &DiscoveryDocument, device_id: &str) -> Result<String, String> {
    let issuer = Url::parse(&discovery.issuer).map_err(|_| "verified issuer became invalid")?;
    let host = issuer.host_str().ok_or("verified issuer omitted host")?;
    let account = format!("{}:{host}:{device_id}", discovery.deployment_id);
    if account.len() > 512 || account.chars().any(char::is_control) {
        return Err("managed refresh account is invalid".into());
    }
    Ok(account)
}

#[cfg(target_os = "windows")]
async fn native_store_put(_app: &AppHandle, account: &str, value: &str) -> Result<(), String> {
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
async fn native_store_put(app: &AppHandle, account: &str, value: &str) -> Result<(), String> {
    crate::android_runtime::secure_store_put(app, account, value).await
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
async fn native_store_put(_app: &AppHandle, _account: &str, _value: &str) -> Result<(), String> {
    Err("native secure storage is unavailable on this platform".into())
}

#[cfg(target_os = "windows")]
async fn native_store_get(_app: &AppHandle, account: &str) -> Result<Option<String>, String> {
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
async fn native_store_get(app: &AppHandle, account: &str) -> Result<Option<String>, String> {
    crate::android_runtime::secure_store_get(app, account).await
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
async fn native_store_get(_app: &AppHandle, _account: &str) -> Result<Option<String>, String> {
    Err("native secure storage is unavailable on this platform".into())
}

#[cfg(target_os = "windows")]
async fn native_store_delete(_app: &AppHandle, account: &str) -> Result<(), String> {
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
async fn native_store_delete(app: &AppHandle, account: &str) -> Result<(), String> {
    crate::android_runtime::secure_store_delete(app, account).await
}

#[cfg(not(any(target_os = "windows", target_os = "android")))]
async fn native_store_delete(_app: &AppHandle, _account: &str) -> Result<(), String> {
    Err("native secure storage is unavailable on this platform".into())
}

async fn store_refresh(app: &AppHandle, account: &str, refresh: &str) -> Result<(), String> {
    native_store_put(app, account, refresh).await
}

async fn load_refresh(app: &AppHandle, account: &str) -> Result<Option<String>, String> {
    native_store_get(app, account).await
}

async fn delete_refresh(app: &AppHandle, account: &str) -> Result<(), String> {
    native_store_delete(app, account).await
}

async fn store_persisted_session(
    app: &AppHandle,
    persisted: &PersistedSession,
) -> Result<(), String> {
    validate_persisted_session(persisted)?;
    let encoded = serde_json::to_string(persisted)
        .map_err(|_| "managed session metadata could not be encoded".to_string())?;
    if encoded.len() > MAX_PERSISTED_SESSION_BYTES {
        return Err("managed session metadata is too large".into());
    }
    native_store_put(app, SESSION_INDEX_ACCOUNT, &encoded).await
}

async fn load_persisted_session(app: &AppHandle) -> Result<Option<PersistedSession>, String> {
    let Some(encoded) = native_store_get(app, SESSION_INDEX_ACCOUNT).await? else {
        return Ok(None);
    };
    if encoded.len() > MAX_PERSISTED_SESSION_BYTES {
        return Err("saved managed session metadata is too large".into());
    }
    let persisted: PersistedSession = serde_json::from_str(&encoded)
        .map_err(|_| "saved managed session metadata is invalid".to_string())?;
    validate_persisted_session(&persisted)?;
    Ok(Some(persisted))
}

async fn delete_persisted_session(app: &AppHandle) -> Result<(), String> {
    native_store_delete(app, SESSION_INDEX_ACCOUNT).await
}

fn validate_persisted_session(persisted: &PersistedSession) -> Result<(), String> {
    if persisted.schema_version != 2
        || persisted.discovery_url.len() > 2_048
        || persisted.discovery_fingerprint.len() > 200
        || persisted.refresh_account.len() > 512
    {
        return Err("saved managed session metadata is invalid".into());
    }
    validate_id(&persisted.profile_id, "saved profileId")?;
    validate_id(&persisted.deployment_id, "saved deploymentId")?;
    validate_id(&persisted.app_id, "saved appId")?;
    validate_id(&persisted.device_id, "saved deviceId")?;
    if let Some(oauth_device_id) = &persisted.oauth_device_id {
        validate_id(oauth_device_id, "saved OAuth deviceId")?;
    }
    validate_id(
        &persisted.discovery_fingerprint,
        "saved discovery fingerprint",
    )?;
    if persisted.refresh_account.is_empty()
        || persisted.refresh_account.chars().any(char::is_control)
    {
        return Err("saved refresh account is invalid".into());
    }
    let url = Url::parse(&persisted.discovery_url)
        .map_err(|_| "saved discovery URL is invalid".to_string())?;
    let local_beta = cfg!(feature = "managed-beta-local")
        && url.scheme() == "http"
        && url.host_str() == Some("api.formlogic.local");
    if url.scheme() != "https" && !local_beta {
        return Err("saved discovery URL must use https".into());
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
        .map_err(|_| "system clock is before the Unix epoch".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::test_discovery_document_with_relay_policy;

    fn admission_body(relay: Option<serde_json::Value>) -> serde_json::Value {
        let mut body = serde_json::json!({
            "accessToken": "a".repeat(32),
            "tokenType": "Bearer",
            "expiresIn": 120,
            "expiresAt": 1_800_000_120_u64,
            "gatewayUrl": "wss://gateway.example.test/v2/realtime",
            "appId": "app_a",
            "subjectId": "device_a",
            "role": "mobile",
            "holderKeyThumbprint": "holder",
            "expectedPeerKeyThumbprint": "peer",
            "scopes": ["state_read", "monitor"],
            "iceServers": [],
            "relayOnly": false,
            "turnCredentialExpiresAt": serde_json::Value::Null,
            "device": {
                "id": "oauth_device_a",
                "appId": "app_a",
                "subjectId": "device_a",
                "role": "mobile",
                "displayName": "Aokie Companion",
                "grants": ["state_read", "monitor"],
                "approvedAt": "2026-07-18T00:00:00Z",
                "lastSeenAt": "2026-07-18T00:00:00Z"
            }
        });
        if let Some(relay) = relay {
            body["relay"] = relay;
        }
        body
    }

    fn relay_advertisement(origin: &str) -> serde_json::Value {
        serde_json::json!({
            "challengeUrl": format!("{origin}/api/aokie-companion/relay/challenge"),
            "framesUrl": format!("{origin}/api/aokie-companion/relay/frames"),
            "streamUrl": format!("{origin}/api/aokie-companion/relay/stream"),
        })
    }

    /// The whole reason the decoder change ships BEFORE the backend advertises
    /// the member: `AdmissionResponse` is `deny_unknown_fields`, so an
    /// unexpected `relay` would fail the entire admission and take the
    /// Companion surface down with "managed admission response is invalid".
    #[test]
    fn an_advertised_relay_does_not_fail_the_admission_decoder() {
        let encoded = serde_json::to_vec(&admission_body(Some(relay_advertisement(
            "https://api.example.test",
        ))))
        .expect("fixture encodes");

        let admission: AdmissionResponse =
            serde_json::from_slice(&encoded).expect("an advertised relay decodes");

        assert!(admission.relay.is_some());
        // Absent is still the normal case and must stay valid.
        let without = serde_json::to_vec(&admission_body(None)).expect("fixture encodes");
        let admission: AdmissionResponse =
            serde_json::from_slice(&without).expect("an admission without a relay decodes");
        assert!(admission.relay.is_none());
    }

    /// The member is held as a raw value precisely so a reshaped advertisement
    /// degrades to the WebSocket gateway instead of failing the admission.
    #[test]
    fn a_malformed_relay_advertisement_degrades_rather_than_erroring() {
        for malformed in [
            serde_json::json!("https://api.example.test"),
            serde_json::json!({"challengeUrl": "https://api.example.test/challenge"}),
            serde_json::json!({"challengeUrl": 7, "framesUrl": 8, "streamUrl": 9}),
        ] {
            let encoded = serde_json::to_vec(&admission_body(Some(malformed.clone())))
                .expect("fixture encodes");

            let admission: AdmissionResponse = serde_json::from_slice(&encoded)
                .unwrap_or_else(|_| panic!("{malformed} must not fail the admission decoder"));

            assert!(
                admission.relay.and_then(usable_relay_endpoints).is_none(),
                "{malformed} must not be adopted as a carrier"
            );
        }
    }

    #[test]
    fn a_usable_relay_advertisement_is_adopted_whole() {
        let relay = usable_relay_endpoints(relay_advertisement("https://api.example.test"))
            .expect("a well-formed same-origin https advertisement is usable");

        assert_eq!(
            relay.challenge_url,
            "https://api.example.test/api/aokie-companion/relay/challenge"
        );
        // Unknown members are additive hints, not grounds to refuse the carrier:
        // a backend that later advertises a long-poll route must leave this
        // build using the three URLs it does understand.
        let mut forward_compatible = relay_advertisement("https://api.example.test");
        forward_compatible["longPollUrl"] = serde_json::json!("https://api.example.test/poll");
        assert!(usable_relay_endpoints(forward_compatible).is_some());
    }

    #[test]
    fn relay_advertisements_that_are_not_safe_are_refused() {
        // Cross-origin: one route pointing somewhere else is how a carrier gets
        // split across a host the admission never authorised.
        let mut cross_origin = relay_advertisement("https://api.example.test");
        cross_origin["streamUrl"] =
            serde_json::json!("https://elsewhere.example.test/api/aokie-companion/relay/stream");
        assert!(usable_relay_endpoints(cross_origin).is_none());

        for unsafe_url in [
            // Credentials in the URL.
            "https://user:pass@api.example.test/api/aokie-companion/relay/stream",
            // A fragment.
            "https://api.example.test/api/aokie-companion/relay/stream#x",
            // Not absolute.
            "/api/aokie-companion/relay/stream",
            // A scheme this carrier does not speak.
            "ftp://api.example.test/api/aokie-companion/relay/stream",
        ] {
            let mut advertisement = relay_advertisement("https://api.example.test");
            advertisement["streamUrl"] = serde_json::json!(unsafe_url);
            assert!(
                usable_relay_endpoints(advertisement).is_none(),
                "{unsafe_url} must not be adopted"
            );
        }
    }

    /// Plaintext is a managed-beta-local carve-out for the exact WAMP API host
    /// and nothing else — `formlogic.local` is a DIFFERENT origin that serves
    /// the web app, not the API.
    #[test]
    fn plaintext_relay_urls_follow_the_same_narrow_local_carve_out() {
        let local = usable_relay_endpoints(relay_advertisement("http://api.formlogic.local"));
        assert_eq!(local.is_some(), cfg!(feature = "managed-beta-local"));

        assert!(usable_relay_endpoints(relay_advertisement("http://formlogic.local")).is_none());
        assert!(usable_relay_endpoints(relay_advertisement("http://127.0.0.1:8080")).is_none());
        assert!(usable_relay_endpoints(relay_advertisement("http://api.example.test")).is_none());
    }

    #[test]
    fn admission_grants_map_onto_the_protocol_enum() {
        assert_eq!(
            admission_grants(&["state_read".into(), "monitor".into()]),
            vec![Grant::StateRead, Grant::Monitor]
        );
        // validate_admission already refuses unknown grants; an unmapped name
        // is dropped rather than guessed at.
        assert_eq!(
            admission_grants(&["state_read".into(), "not_a_grant".into()]),
            vec![Grant::StateRead]
        );
    }

    #[test]
    fn pkce_material_has_rfc7636_lengths() {
        let verifier = random_base64url(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert!((43..=128).contains(&verifier.len()));
        assert_eq!(challenge.len(), 43);
        assert_ne!(verifier, random_base64url(32));
    }

    #[test]
    fn token_response_rejects_scope_escalation() {
        let token = TokenResponse {
            access_token: "a".repeat(32),
            token_type: "Bearer".into(),
            expires_in: 600,
            refresh_token: "r".repeat(32),
            scope: "aokie:state aokie:takeover".into(),
        };
        assert!(validate_token_response(&token, &["aokie:state".into()]).is_err());
    }

    #[test]
    fn token_response_rejects_control_characters_in_native_credentials() {
        let token = TokenResponse {
            access_token: format!("{}\n", "a".repeat(31)),
            token_type: "Bearer".into(),
            expires_in: 600,
            refresh_token: "r".repeat(32),
            scope: "aokie:state".into(),
        };
        assert!(validate_token_response(&token, &["aokie:state".into()]).is_err());
    }

    #[test]
    fn authorization_request_binds_app_and_device() {
        let url = build_authorization_url(
            "https://formlogic.example/oauth/authorize",
            "aokie-companion",
            "http://127.0.0.1:18181/oauth/callback",
            &["aokie:state".into(), "aokie:takeover".into()],
            "state_1",
            "challenge_1",
            "https://formlogic.example",
            "device_1",
            "app_1",
        )
        .unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("appId").map(String::as_str), Some("app_1"));
        assert_eq!(query.get("device").map(String::as_str), Some("device_1"));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
    }

    #[test]
    fn managed_scopes_include_advertised_private_consult() {
        let supported = [
            "aokie:state",
            "aokie:assistance",
            "aokie:monitor",
            "aokie:consult",
            "aokie:takeover",
            "aokie:resume",
            "aokie:end_caller",
            "offline_access",
        ]
        .map(str::to_owned);
        let requested = managed_requested_scopes(&supported).unwrap();
        assert!(requested.iter().any(|scope| scope == "aokie:state"));
        assert!(requested.iter().any(|scope| scope == "aokie:assistance"));
        assert!(requested.iter().any(|scope| scope == "offline_access"));
        assert!(requested.iter().any(|scope| scope == "aokie:consult"));
        assert!(requested.iter().any(|scope| scope == "aokie:end_caller"));
    }

    #[test]
    fn signed_discovery_relay_policy_reaches_managed_and_custom_profile_config() {
        let relay = connect_config(
            &test_discovery_document_with_relay_policy(true),
            "app_1".into(),
            "device_1".into(),
            "profile_issuer_a_deployment_1_app_1".into(),
        );
        assert!(relay.relay_only);
        assert_eq!(
            relay.managed_profile_id,
            "profile_issuer_a_deployment_1_app_1"
        );
        assert_ne!(relay.managed_profile_id, relay.managed_deployment_id);
        assert_ne!(relay.managed_profile_id, relay.app_id);

        let direct = connect_config(
            &test_discovery_document_with_relay_policy(false),
            "app_1".into(),
            "device_1".into(),
            "profile_issuer_a_deployment_1_app_1".into(),
        );
        assert!(!direct.relay_only);
    }

    #[tokio::test]
    async fn native_sessions_do_not_collide_when_profiles_share_a_deployment_id() {
        let state = ManagedAuthState::default();
        let first = ManagedSession {
            profile_id: "profile_issuer_a_deployment_shared_app_a".into(),
            deployment_id: "deployment_shared".into(),
            gateway_url: "wss://issuer-a.example/v2/realtime".into(),
            oauth_token_url: "https://issuer-a.example/oauth/token".into(),
            oauth_resource: "https://issuer-a.example".into(),
            admission_endpoint: "https://issuer-a.example/admission".into(),
            client_id: "aokie-companion".into(),
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            oauth_device_id: None,
            access_token: "a".repeat(32),
            access_expires_at: 600,
            scopes: vec!["aokie:state".into()],
            discovery_relay_only: false,
            refresh_account: "deployment_shared:issuer-a.example:device_a".into(),
        };
        let mut second = first.clone();
        second.profile_id = "profile_issuer_b_deployment_shared_app_b".into();
        second.gateway_url = "wss://issuer-b.example/v2/realtime".into();
        second.oauth_token_url = "https://issuer-b.example/oauth/token".into();
        second.oauth_resource = "https://issuer-b.example".into();
        second.admission_endpoint = "https://issuer-b.example/admission".into();
        second.app_id = "app_b".into();
        second.device_id = "device_b".into();
        second.refresh_account = "deployment_shared:issuer-b.example:device_b".into();

        state.store_session(first.clone()).await;
        state.store_session(second.clone()).await;

        let sessions = state.sessions.lock().await;
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions
                .get(&first.profile_id)
                .map(|session| session.app_id.as_str()),
            Some("app_a")
        );
        assert_eq!(
            sessions
                .get(&second.profile_id)
                .map(|session| session.app_id.as_str()),
            Some("app_b")
        );
        assert_eq!(first.deployment_id, second.deployment_id);
        assert!(first.matches_binding(
            "profile_issuer_a_deployment_shared_app_a",
            "deployment_shared",
            "app_a",
            "device_a",
        ));
        assert!(!first.matches_binding(
            "profile_issuer_b_deployment_shared_app_b",
            "deployment_shared",
            "app_b",
            "device_b",
        ));
    }

    #[test]
    fn managed_admission_accepts_the_complete_consult_contract() {
        let now = unix_now().unwrap();
        let session = ManagedSession {
            profile_id: "profile_issuer_a_deployment_1_app_1".into(),
            deployment_id: "deployment_1".into(),
            gateway_url: "wss://formlogic.example/realtime".into(),
            oauth_token_url: "https://formlogic.example/oauth/token".into(),
            oauth_resource: "https://formlogic.example".into(),
            admission_endpoint: "https://formlogic.example/admission".into(),
            client_id: "aokie-companion".into(),
            app_id: "app_1".into(),
            device_id: "device_1".into(),
            oauth_device_id: Some("oauth_device_1".into()),
            access_token: "a".repeat(32),
            access_expires_at: now + 600,
            scopes: MANAGED_REQUESTED_SCOPES
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect(),
            discovery_relay_only: false,
            refresh_account: "deployment:app_1:device_1".into(),
        };
        let grants = MANAGED_ADMISSION_GRANTS
            .iter()
            .map(|grant| (*grant).to_owned())
            .collect::<Vec<_>>();
        let mut admission = AdmissionResponse {
            access_token: "b".repeat(32),
            token_type: "Bearer".into(),
            expires_in: 120,
            expires_at: now + 120,
            gateway_url: session.gateway_url.clone(),
            app_id: session.app_id.clone(),
            subject_id: session.device_id.clone(),
            role: "mobile".into(),
            holder_key_thumbprint: "mobile_key_thumbprint_1".into(),
            expected_peer_key_thumbprint: "desktop_key_thumbprint_1".into(),
            scopes: grants.clone(),
            ice_servers: Vec::new(),
            relay_only: false,
            turn_credential_expires_at: None.into(),
            device: DeviceRecord {
                id: "device_record_1".into(),
                app_id: session.app_id.clone(),
                subject_id: session.device_id.clone(),
                role: "mobile".into(),
                display_name: "Reception desk".into(),
                grants,
                approved_at: "2026-07-16T00:00:00Z".into(),
                last_seen_at: "2026-07-16T00:00:00Z".into(),
            },
            relay: None,
        };

        validate_admission(&session, &admission, "mobile_key_thumbprint_1").unwrap();
        admission.holder_key_thumbprint = "different_mobile_key".into();
        assert!(validate_admission(&session, &admission, "mobile_key_thumbprint_1").is_err());
        admission.holder_key_thumbprint = "mobile_key_thumbprint_1".into();
        admission.expected_peer_key_thumbprint = "mobile_key_thumbprint_1".into();
        assert!(validate_admission(&session, &admission, "mobile_key_thumbprint_1").is_err());
        admission.expected_peer_key_thumbprint = "desktop_key_thumbprint_1".into();
        admission.relay_only = true;
        assert!(validate_admission(&session, &admission, "mobile_key_thumbprint_1").is_err());
        admission.relay_only = false;
        admission.scopes.push("dongle_control".into());
        admission.device.grants.push("dongle_control".into());
        assert!(validate_admission(&session, &admission, "mobile_key_thumbprint_1").is_err());
    }

    #[test]
    fn managed_admission_strictly_requires_relay_and_turn_expiry_metadata() {
        let response = serde_json::json!({
            "accessToken":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "tokenType":"Bearer",
            "expiresIn":120,
            "expiresAt":1_800_000_120_u64,
            "gatewayUrl":"wss://formlogic.example/v2/realtime",
            "appId":"app_1",
            "subjectId":"device_1",
            "role":"mobile",
            "holderKeyThumbprint":"mobile_key_thumbprint_1",
            "expectedPeerKeyThumbprint":"desktop_key_thumbprint_1",
            "scopes":["state_read","rtc_signal"],
            "iceServers":[],
            "relayOnly":false,
            "turnCredentialExpiresAt":null,
            "device":{
                "id":"device_record_1",
                "appId":"app_1",
                "subjectId":"device_1",
                "role":"mobile",
                "displayName":"Reception desk",
                "grants":["state_read","rtc_signal"],
                "approvedAt":"2026-07-16T00:00:00Z",
                "lastSeenAt":"2026-07-16T00:00:00Z"
            }
        });
        assert!(serde_json::from_value::<AdmissionResponse>(response.clone()).is_ok());

        let mut missing_relay = response.clone();
        missing_relay.as_object_mut().unwrap().remove("relayOnly");
        assert!(serde_json::from_value::<AdmissionResponse>(missing_relay).is_err());

        let mut missing_expiry = response.clone();
        missing_expiry
            .as_object_mut()
            .unwrap()
            .remove("turnCredentialExpiresAt");
        assert!(serde_json::from_value::<AdmissionResponse>(missing_expiry).is_err());

        let mut unknown = response;
        unknown["futureRelayPolicy"] = serde_json::json!(false);
        assert!(serde_json::from_value::<AdmissionResponse>(unknown).is_err());
    }

    #[test]
    fn admission_refresh_is_reserved_for_http_401() {
        let body = br#"{"error":true,"code":"mobile_not_paired","message":"Approve this endpoint in Aokie Desktop"}"#;
        assert_eq!(
            classify_admission_http_failure(StatusCode::UNAUTHORIZED, true, body),
            AdmissionFailure::Unauthorized,
        );
        assert_eq!(
            classify_admission_http_failure(StatusCode::FORBIDDEN, true, body),
            AdmissionFailure::Policy {
                code: "mobile_not_paired".into(),
                message: "Approve this endpoint in Aokie Desktop".into(),
            },
        );
    }

    #[test]
    fn admission_preserves_desktop_and_other_policy_failures() {
        let desktop = br#"{"error":true,"code":"desktop_identity_unavailable","message":"Start the assigned Aokie Desktop"}"#;
        assert_eq!(
            classify_admission_http_failure(StatusCode::CONFLICT, true, desktop),
            AdmissionFailure::Policy {
                code: "desktop_identity_unavailable".into(),
                message: "Start the assigned Aokie Desktop".into(),
            },
        );

        let policy = br#"{"error":true,"code":"insufficient_scope","message":"Current consent does not permit Companion state"}"#;
        assert_eq!(
            classify_admission_http_failure(StatusCode::FORBIDDEN, true, policy),
            AdmissionFailure::Policy {
                code: "insufficient_scope".into(),
                message: "Current consent does not permit Companion state".into(),
            },
        );
    }

    #[test]
    fn admission_policy_parser_is_bounded_and_never_echoes_untyped_bodies() {
        let untyped = classify_admission_http_failure(
            StatusCode::FORBIDDEN,
            false,
            br#"{"error":true,"code":"mobile_not_paired","message":"secret"}"#,
        );
        assert_eq!(
            untyped,
            AdmissionFailure::Other("managed admission endpoint returned HTTP 403".into(),),
        );

        let malformed = classify_admission_http_failure(
            StatusCode::FORBIDDEN,
            true,
            br#"{"error":true,"code":"mobile_not_paired","message":"secret\nvalue"}"#,
        );
        assert_eq!(
            malformed,
            AdmissionFailure::Other("managed admission endpoint returned HTTP 403".into(),),
        );

        let oversized = vec![b'x'; MAX_ADMISSION_RESPONSE_BYTES + 1];
        assert_eq!(
            classify_admission_http_failure(StatusCode::FORBIDDEN, true, &oversized),
            AdmissionFailure::Other("managed admission endpoint returned HTTP 403".into(),),
        );
    }

    #[test]
    fn persisted_session_contains_only_restart_metadata() {
        let persisted = PersistedSession {
            schema_version: 2,
            profile_id: "profile_1".into(),
            discovery_url: "https://formlogic.example/.well-known/aokie-companion".into(),
            deployment_id: "deployment_1".into(),
            app_id: "app_1".into(),
            device_id: "device_1".into(),
            oauth_device_id: Some("oauth_device_1".into()),
            discovery_fingerprint: "fingerprint_1".into(),
            refresh_account: "deployment_1:app_1:device_1".into(),
        };

        validate_persisted_session(&persisted).unwrap();
        let encoded = serde_json::to_string(&persisted).unwrap();
        assert!(!encoded.contains("accessToken"));
        assert!(!encoded.contains("refreshToken"));
        assert!(!encoded.contains("admission"));
    }

    #[test]
    fn persisted_session_rejects_plain_http_and_control_characters() {
        let mut persisted = PersistedSession {
            schema_version: 2,
            profile_id: "profile_1".into(),
            discovery_url: "http://formlogic.example/.well-known/aokie-companion".into(),
            deployment_id: "deployment_1".into(),
            app_id: "app_1".into(),
            device_id: "device_1".into(),
            oauth_device_id: Some("oauth_device_1".into()),
            discovery_fingerprint: "fingerprint_1".into(),
            refresh_account: "deployment_1:app_1:device_1".into(),
        };
        assert!(validate_persisted_session(&persisted).is_err());

        persisted.discovery_url = "https://formlogic.example/.well-known/aokie-companion".into();
        persisted.refresh_account.push('\n');
        assert!(validate_persisted_session(&persisted).is_err());
    }

    #[test]
    fn mobile_api_url_is_derived_from_the_resource_origin() {
        let url = mobile_api_url(
            "https://formlogic.example/oauth/resource?ignored=yes",
            "/api/aokie-companion/mobile/history",
            &[("limit", "50".into()), ("before", "123".into())],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://formlogic.example/api/aokie-companion/mobile/history?limit=50&before=123"
        );
        assert!(mobile_api_url(
            "http://192.168.1.10",
            "/api/aokie-companion/mobile/bootstrap",
            &[],
        )
        .is_err());
        assert!(mobile_api_url("https://formlogic.example", "/api/forms", &[],).is_err());

        let records = mobile_api_url(
            "https://formlogic.example/oauth/resource",
            "/api/aokie-companion/mobile/call-records",
            &[("limit", "25".into())],
        )
        .unwrap();
        assert_eq!(
            records.as_str(),
            "https://formlogic.example/api/aokie-companion/mobile/call-records?limit=25"
        );
        let record = mobile_api_url(
            "https://formlogic.example/oauth/resource",
            "/api/aokie-companion/mobile/call-records/call_record_1",
            &[],
        )
        .unwrap();
        assert_eq!(
            record.as_str(),
            "https://formlogic.example/api/aokie-companion/mobile/call-records/call_record_1"
        );
        assert!(mobile_api_url(
            "https://formlogic.example",
            "/api/aokie-companion/mobile/call-records/../escape",
            &[],
        )
        .is_err());
        assert!(mobile_api_url(
            "https://formlogic.example",
            "/api/aokie-companion/mobile/call-records/call_record_1",
            &[("limit", "1".into())],
        )
        .is_err());
        assert!(mobile_api_url(
            "https://formlogic.example",
            "/api/aokie-companion/mobile/call-records",
            &[("before", "1".into())],
        )
        .is_err());

        let push = mobile_api_url(
            "https://formlogic.example/oauth/resource",
            "/api/aokie-companion/mobile/devices/oauth_device_1/push-endpoints/fcm",
            &[],
        )
        .unwrap();
        assert_eq!(
            push.as_str(),
            "https://formlogic.example/api/aokie-companion/mobile/devices/oauth_device_1/push-endpoints/fcm"
        );
        assert!(mobile_api_url(
            "https://formlogic.example",
            "/api/aokie-companion/mobile/devices/../escape/push-endpoints/fcm",
            &[],
        )
        .is_err());
        assert!(mobile_api_url(
            "https://formlogic.example",
            "/api/aokie-companion/mobile/devices/oauth_device_1/push-endpoints/fcm",
            &[("limit", "1".into())],
        )
        .is_err());
    }

    #[cfg(feature = "managed-beta-local")]
    #[test]
    fn managed_beta_local_accepts_only_the_canonical_api_origin() {
        assert!(mobile_api_url(
            "http://api.formlogic.local/api/aokie-companion",
            "/api/aokie-companion/mobile/bootstrap",
            &[],
        )
        .is_ok());
        assert!(mobile_api_url(
            "http://formlogic.local/api/aokie-companion",
            "/api/aokie-companion/mobile/bootstrap",
            &[],
        )
        .is_err());

        let mut persisted = PersistedSession {
            schema_version: 2,
            profile_id: "profile_1".into(),
            discovery_url: "http://api.formlogic.local/.well-known/aokie-companion".into(),
            deployment_id: "deployment_1".into(),
            app_id: "app_1".into(),
            device_id: "device_1".into(),
            oauth_device_id: Some("oauth_device_1".into()),
            discovery_fingerprint: "fingerprint_1".into(),
            refresh_account: "deployment_1:app_1:device_1".into(),
        };
        assert!(validate_persisted_session(&persisted).is_ok());
        persisted.discovery_url = "http://formlogic.local/.well-known/aokie-companion".into();
        assert!(validate_persisted_session(&persisted).is_err());
    }

    #[test]
    fn mobile_api_error_parser_never_echoes_untyped_body() {
        let typed = mobile_failure_message(MobileHttpFailure::Status(
            StatusCode::FORBIDDEN,
            br#"{"error":true,"code":"device_revoked","message":"Sign in again"}"#.to_vec(),
        ));
        assert_eq!(typed, "mobile API device_revoked: Sign in again");
        let untyped = mobile_failure_message(MobileHttpFailure::Status(
            StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"html":"<secret>"}"#.to_vec(),
        ));
        assert_eq!(untyped, "mobile API returned HTTP 500");
    }
}
