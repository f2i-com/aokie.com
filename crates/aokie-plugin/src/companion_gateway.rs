//! Authenticated v2 Companion signalling connection.
//!
//! The implementation lives in this plugin so media control remains next to
//! the physical radio truth.  Only SDP/ICE and epoch-bound lease transitions
//! cross the WebSocket; PCM remains inside native WebRTC tracks.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aokie_media::{
    IceCandidateSignal, IceServerConfig, MediaMode, SdpSignal, SdpSignalType, SessionBinding,
};
use aokie_protocol::v2::{
    peer_roster_hash, sdp_dtls_fingerprint, sdp_sha256, AdmissionRole, AuthoritativeCallSnapshot,
    CallerProjection, Caption, CarrierHoldEvidence, EndCallerOutcome, EndpointBindingClaims,
    EndpointChallengeFrame, EndpointKeyAlgorithm, EndpointPublicKey, HelloProofClaims, LeaseClaims,
    LeaseMode, LeasePhase, MediaState, PluginAssistanceAnswerFrame, PluginClaimDecisionFrame,
    PluginEndCallerExecuteFrame, PluginEndCallerResultFrame, PluginHello, PluginIdleFrame,
    PluginLeaseRevokeFrame, PluginRtcSignalFrame, PluginSnapshotFrame, RemoteCapabilities,
    RemoteConsentPolicy, RtcSignal, SecondaryCallObservation, SecondaryCallPolicy,
    ServiceMode as ProtocolServiceMode, SignedEndpointBinding, SignedHelloProof,
    SignedTrickleCandidateEnvelope, TelephonyState, TrickleCandidateClaims, MAX_LEASE_TOKEN_BYTES,
    SCHEMA_VERSION,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer as _, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, http::HeaderValue, Message};
use url::Url;

use crate::event_bridge::{Sink, StdoutSink};
use crate::host_rpc::HostRpc;
use crate::radio::{
    CompanionEndCallerFailure, CompanionEndCallerRequest, RadioControl, RadioHandle,
};
use crate::remote_media::{
    OpenPeerRequest, RemoteMediaEvent, RemoteMediaEventKind, RemoteMediaHandle,
    ServiceMode as LocalServiceMode,
};

const READ_TICK: Duration = Duration::from_millis(100);
const SNAPSHOT_POLL: Duration = Duration::from_millis(250);
const SNAPSHOT_REFRESH: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(15);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
const ADMISSION_RPC_TIMEOUT: Duration = Duration::from_secs(10);
// plugin.init's compact bootstrap intentionally omits token expiry. Consume it
// once and rotate through the Desktop broker quickly rather than assuming the
// gateway's maximum admission lifetime.
const DEFAULT_BOOTSTRAP_LIFETIME: Duration = Duration::from_secs(45);
const MAX_BACKOFF: Duration = Duration::from_secs(20);
const ACTIVE_LEASE_FALLBACK_TTL: u64 = 20;
const MAX_USED_ENDPOINT_JTIS: usize = 4_096;
const ADMISSION_SAFETY_MARGIN_SECONDS: u64 = 10;
const MIN_TURN_CREDENTIAL_TTL_SECONDS: u64 = 30;
const MAX_TURN_CREDENTIAL_TTL_SECONDS: u64 = 24 * 60 * 60;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionBootstrap {
    pub schema_version: u16,
    #[serde(default)]
    pub gateway_url: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    pub plugin_id: String,
    #[serde(default)]
    pub ice_servers: Vec<IceServerConfig>,
    #[serde(default)]
    pub relay_only: bool,
    pub endpoint_identity: EndpointIdentityBootstrap,
    pub approved_mobile_roster: ApprovedMobileRosterBootstrap,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndpointIdentityBootstrap {
    pub algorithm: EndpointKeyAlgorithm,
    pub public_key: String,
    pub thumbprint: String,
    pub private_key_seed: String,
}

impl fmt::Debug for EndpointIdentityBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EndpointIdentityBootstrap")
            .field("algorithm", &self.algorithm)
            .field("public_key", &self.public_key)
            .field("thumbprint", &self.thumbprint)
            .field("private_key_seed", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovedMobileRosterBootstrap {
    pub revision: u64,
    pub roster_hash: String,
    pub keys: Vec<EndpointPublicKey>,
}

#[derive(Clone)]
struct EndpointAuthority {
    signing_key: SigningKey,
    endpoint_key: EndpointPublicKey,
    roster_revision: u64,
    roster_hash: String,
    approved_mobile_keys: HashMap<String, EndpointPublicKey>,
}

impl EndpointAuthority {
    fn from_bootstrap(
        identity: &EndpointIdentityBootstrap,
        roster: &ApprovedMobileRosterBootstrap,
    ) -> Result<Self, String> {
        let seed = URL_SAFE_NO_PAD
            .decode(&identity.private_key_seed)
            .map_err(|_| "privateBootstrap endpointIdentity.privateKeySeed is invalid")?;
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "privateBootstrap endpointIdentity.privateKeySeed must be 32 bytes")?;
        let signing_key = SigningKey::from_bytes(&seed);
        let endpoint_key =
            EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
        if identity.algorithm != EndpointKeyAlgorithm::Ed25519
            || endpoint_key.public_key != identity.public_key
            || endpoint_key.thumbprint != identity.thumbprint
        {
            return Err(
                "privateBootstrap endpoint identity seed, public key and thumbprint disagree"
                    .into(),
            );
        }
        if roster.revision == 0 || roster.keys.is_empty() || roster.keys.len() > 64 {
            return Err(
                "privateBootstrap approvedMobileRoster must contain 1..64 keys at a positive revision"
                    .into(),
            );
        }
        let mut approved_mobile_keys = HashMap::new();
        for key in &roster.keys {
            key.validate()
                .map_err(|_| "privateBootstrap approvedMobileRoster contains an invalid key")?;
            if approved_mobile_keys
                .insert(key.thumbprint.clone(), key.clone())
                .is_some()
            {
                return Err(
                    "privateBootstrap approvedMobileRoster contains duplicate thumbprints".into(),
                );
            }
        }
        let mut thumbprints = approved_mobile_keys.keys().cloned().collect::<Vec<_>>();
        thumbprints.sort();
        if roster.roster_hash != peer_roster_hash(roster.revision, &thumbprints) {
            return Err("privateBootstrap approvedMobileRoster hash is invalid".into());
        }
        Ok(Self {
            signing_key,
            endpoint_key,
            roster_revision: roster.revision,
            roster_hash: roster.roster_hash.clone(),
            approved_mobile_keys,
        })
    }

    fn approved_thumbprints(&self) -> Vec<String> {
        let mut thumbprints = self
            .approved_mobile_keys
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        thumbprints.sort();
        thumbprints
    }

    fn sign(&self, message: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(message).to_bytes())
    }
}

impl fmt::Debug for CompanionBootstrap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompanionBootstrap")
            .field("schema_version", &self.schema_version)
            .field(
                "gateway_url",
                &self.gateway_url.as_deref().map(redacted_url),
            )
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("app_id", &self.app_id)
            .field("plugin_id", &self.plugin_id)
            .field("ice_server_count", &self.ice_servers.len())
            .field("relay_only", &self.relay_only)
            .field("endpoint_identity", &self.endpoint_identity)
            .field("approved_mobile_roster", &self.approved_mobile_roster)
            .finish()
    }
}

impl CompanionBootstrap {
    pub fn parse(value: &Value) -> Result<Self, String> {
        let bootstrap: Self = serde_json::from_value(value.clone())
            .map_err(|_| "privateBootstrap has an invalid v2 shape".to_string())?;
        bootstrap.validate()?;
        Ok(bootstrap)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "privateBootstrap schemaVersion must be {SCHEMA_VERSION}"
            ));
        }
        validate_identity(&self.plugin_id, "pluginId")?;
        match (&self.gateway_url, &self.access_token, &self.app_id) {
            (Some(gateway_url), Some(access_token), Some(app_id)) => {
                validate_identity(app_id, "appId")?;
                validate_bearer(access_token)?;
                normalize_gateway_url(gateway_url)?;
                IceServerConfig::validate_all(&self.ice_servers)
                    .map_err(|error| error.to_string())?;
            }
            (None, None, None) if self.ice_servers.is_empty() && !self.relay_only => {}
            (None, None, None) => {
                return Err(
                    "privateBootstrap identity-only shape cannot include ICE settings".into(),
                )
            }
            _ => {
                return Err(
                    "privateBootstrap admission fields must be all present or all absent".into(),
                )
            }
        }
        EndpointAuthority::from_bootstrap(&self.endpoint_identity, &self.approved_mobile_roster)
            .map(|_| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayConnectionPhase {
    Connecting,
    Connected,
    Reconnecting,
    AdmissionRefresh,
    Expired,
    RebootstrapRequired,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatusSnapshot {
    pub configured: bool,
    pub connected: bool,
    pub phase: GatewayConnectionPhase,
    pub reconnect_attempt: u32,
    pub last_error: Option<String>,
    pub changed_at: String,
}

impl GatewayStatusSnapshot {
    fn starting() -> Self {
        Self {
            configured: true,
            connected: false,
            phase: GatewayConnectionPhase::Connecting,
            reconnect_attempt: 0,
            last_error: None,
            changed_at: aokie_core::events::now_iso8601(),
        }
    }
}

pub struct CompanionGatewayHandle {
    stop_tx: watch::Sender<bool>,
    status: Arc<Mutex<GatewayStatusSnapshot>>,
}

impl CompanionGatewayHandle {
    pub fn spawn(
        bootstrap: CompanionBootstrap,
        radio: RadioHandle,
        host_rpc: Arc<HostRpc>,
    ) -> Result<Self, String> {
        let startup = GatewayStartup::from_bootstrap(bootstrap)?;
        Self::spawn_start(startup, radio, host_rpc, GatewayConnectionPhase::Connecting)
    }

    fn spawn_start(
        startup: GatewayStartup,
        radio: RadioHandle,
        host_rpc: Arc<HostRpc>,
        phase: GatewayConnectionPhase,
    ) -> Result<Self, String> {
        let mut initial_status = GatewayStatusSnapshot::starting();
        initial_status.phase = phase;
        let status = Arc::new(Mutex::new(initial_status));
        let (stop_tx, stop_rx) = watch::channel(false);
        let worker_status = status.clone();
        std::thread::Builder::new()
            .name("aokie-companion-gateway".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        set_status(
                            &worker_status,
                            GatewayConnectionPhase::RebootstrapRequired,
                            0,
                            Some("Companion gateway runtime could not start".into()),
                        );
                        return;
                    }
                };
                runtime.block_on(gateway_worker(
                    startup,
                    radio,
                    host_rpc,
                    stop_rx,
                    worker_status,
                ));
            })
            .map_err(|error| format!("start Companion gateway worker: {error}"))?;
        Ok(Self { stop_tx, status })
    }

    pub fn status(&self) -> GatewayStatusSnapshot {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| GatewayStatusSnapshot {
                configured: true,
                connected: false,
                phase: GatewayConnectionPhase::RebootstrapRequired,
                reconnect_attempt: 0,
                last_error: Some("Companion gateway status is unavailable".into()),
                changed_at: aokie_core::events::now_iso8601(),
            })
    }

    pub fn stop(&self) {
        self.stop_tx.send_replace(true);
    }
}

enum GatewayStartup {
    Managed {
        app_id: Option<String>,
        plugin_id: String,
        endpoint_authority: Arc<EndpointAuthority>,
        initial: Option<SessionCredentials>,
    },
}

impl GatewayStartup {
    fn from_bootstrap(bootstrap: CompanionBootstrap) -> Result<Self, String> {
        bootstrap.validate()?;
        let endpoint_authority = Arc::new(EndpointAuthority::from_bootstrap(
            &bootstrap.endpoint_identity,
            &bootstrap.approved_mobile_roster,
        )?);
        let initial = SessionCredentials::initial(&bootstrap, endpoint_authority.clone())?;
        Ok(Self::Managed {
            app_id: bootstrap.app_id.clone(),
            plugin_id: bootstrap.plugin_id.clone(),
            endpoint_authority,
            initial,
        })
    }
}

impl Drop for CompanionGatewayHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SessionCredentials {
    endpoint: Url,
    token: String,
    app_id: String,
    plugin_id: String,
    lifetime: Duration,
    ice_servers: Vec<IceServerConfig>,
    relay_only: bool,
    turn_credential_expires_at: Option<u64>,
    endpoint_authority: Arc<EndpointAuthority>,
}

impl fmt::Debug for SessionCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionCredentials")
            .field("endpoint", &redacted_url(self.endpoint.as_str()))
            .field("token", &"[REDACTED]")
            .field("app_id", &self.app_id)
            .field("plugin_id", &self.plugin_id)
            .field("lifetime", &self.lifetime)
            .field("ice_server_count", &self.ice_servers.len())
            .field("relay_only", &self.relay_only)
            .field(
                "has_expiring_turn_credentials",
                &self.turn_credential_expires_at.is_some(),
            )
            .field(
                "endpoint_key_thumbprint",
                &self.endpoint_authority.endpoint_key.thumbprint,
            )
            .field(
                "peer_roster_revision",
                &self.endpoint_authority.roster_revision,
            )
            .finish()
    }
}

impl SessionCredentials {
    fn initial(
        bootstrap: &CompanionBootstrap,
        endpoint_authority: Arc<EndpointAuthority>,
    ) -> Result<Option<Self>, String> {
        let (Some(gateway_url), Some(access_token), Some(app_id)) = (
            bootstrap.gateway_url.as_deref(),
            bootstrap.access_token.as_deref(),
            bootstrap.app_id.as_deref(),
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            endpoint: normalize_gateway_url(gateway_url)?,
            token: access_token.to_string(),
            app_id: app_id.to_string(),
            plugin_id: bootstrap.plugin_id.clone(),
            lifetime: DEFAULT_BOOTSTRAP_LIFETIME,
            ice_servers: bootstrap.ice_servers.clone(),
            relay_only: bootstrap.relay_only,
            turn_credential_expires_at: None,
            endpoint_authority,
        }))
    }
}

/// FormLogic attaches credential expiry to each TURN entry. The native media
/// crate deliberately receives only the browser/WebRTC fields after this
/// broker response has been validated and its lifetime has been bounded.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdmissionIceServer {
    urls: Vec<String>,
    username: String,
    credential: String,
    #[serde(default, deserialize_with = "deserialize_optional_unix_timestamp")]
    expires_at: Option<u64>,
}

fn deserialize_optional_unix_timestamp<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Number(number) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("expected a non-negative Unix timestamp")),
        _ => Err(serde::de::Error::custom("expected a Unix timestamp")),
    }
}

impl AdmissionIceServer {
    fn runtime_config(&self) -> IceServerConfig {
        IceServerConfig {
            urls: self.urls.clone(),
            username: self.username.clone(),
            credential: self.credential.clone(),
        }
    }

    fn has_turn_url(&self) -> bool {
        self.urls.iter().any(|url| {
            let lower = url.to_ascii_lowercase();
            lower.starts_with("turn:") || lower.starts_with("turns:")
        })
    }
}

/// A transparent wrapper makes the JSON member itself mandatory while still
/// accepting the backend's explicit `null` when no TURN server is configured.
struct NullableUnixTimestamp(Option<u64>);

impl<'de> Deserialize<'de> for NullableUnixTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match Value::deserialize(deserializer)? {
            Value::Null => Ok(Self(None)),
            Value::Number(number) => number
                .as_u64()
                .map(|value| Self(Some(value)))
                .ok_or_else(|| serde::de::Error::custom("expected a non-negative Unix timestamp")),
            _ => Err(serde::de::Error::custom(
                "expected a Unix timestamp or null",
            )),
        }
    }
}

impl NullableUnixTimestamp {
    fn value(self) -> Option<u64> {
        self.0
    }
}

#[derive(Deserialize)]
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
    scopes: Vec<String>,
    device: Value,
    ice_servers: Vec<AdmissionIceServer>,
    relay_only: bool,
    turn_credential_expires_at: NullableUnixTimestamp,
    endpoint_public_key: EndpointPublicKey,
    holder_key_thumbprint: String,
    approved_peer_key_thumbprints: Vec<String>,
    peer_roster_revision: u64,
    peer_roster_hash: String,
}

impl AdmissionResponse {
    fn into_credentials(
        self,
        expected_app_id: Option<&str>,
        expected_plugin_id: &str,
        endpoint_authority: Arc<EndpointAuthority>,
    ) -> Result<SessionCredentials, WorkerError> {
        let _ = (&self.scopes, &self.device);
        if self.token_type != "Bearer"
            || self.role != "plugin"
            || expected_app_id.is_some_and(|expected| self.app_id != expected)
            || self.subject_id != expected_plugin_id
            || self.endpoint_public_key != endpoint_authority.endpoint_key
            || self.holder_key_thumbprint != endpoint_authority.endpoint_key.thumbprint
            || self.approved_peer_key_thumbprints != endpoint_authority.approved_thumbprints()
            || self.peer_roster_revision != endpoint_authority.roster_revision
            || self.peer_roster_hash != endpoint_authority.roster_hash
        {
            return Err(WorkerError::rebootstrap(
                "Desktop returned a Companion admission for a different identity",
            ));
        }
        validate_identity(&self.app_id, "appId").map_err(WorkerError::rebootstrap)?;
        validate_identity(&self.subject_id, "subjectId").map_err(WorkerError::rebootstrap)?;
        validate_bearer(&self.access_token).map_err(WorkerError::rebootstrap)?;
        let now = unix_now()?;
        let wall_remaining = self.expires_at.saturating_sub(now);
        if self.expires_in <= ADMISSION_SAFETY_MARGIN_SECONDS
            || self.expires_in > 300
            || wall_remaining <= ADMISSION_SAFETY_MARGIN_SECONDS
            || wall_remaining > 300
        {
            return Err(WorkerError::expired(
                "Desktop returned an expired or unsafe Companion admission lifetime",
            ));
        }
        let turn_credential_expires_at = self.turn_credential_expires_at.value();
        let ice_servers = validate_admission_ice_configuration(
            &self.ice_servers,
            self.relay_only,
            turn_credential_expires_at,
            now,
        )
        .map_err(|_| {
            WorkerError::rebootstrap("Desktop returned invalid or unsafe ICE server settings")
        })?;
        let mut safe_remaining = self.expires_in.min(wall_remaining);
        if let Some(expiry) = turn_credential_expires_at {
            safe_remaining = safe_remaining.min(expiry.saturating_sub(now));
        }
        if safe_remaining <= ADMISSION_SAFETY_MARGIN_SECONDS {
            return Err(WorkerError::expired(
                "Desktop returned ICE credentials with no safe connection lifetime",
            ));
        }
        Ok(SessionCredentials {
            endpoint: normalize_gateway_url(&self.gateway_url).map_err(WorkerError::rebootstrap)?,
            token: self.access_token,
            app_id: self.app_id,
            plugin_id: self.subject_id,
            lifetime: Duration::from_secs(
                safe_remaining.saturating_sub(ADMISSION_SAFETY_MARGIN_SECONDS),
            ),
            ice_servers,
            relay_only: self.relay_only,
            turn_credential_expires_at,
            endpoint_authority,
        })
    }
}

fn validate_admission_ice_configuration(
    servers: &[AdmissionIceServer],
    relay_only: bool,
    turn_credential_expires_at: Option<u64>,
    now: u64,
) -> Result<Vec<IceServerConfig>, String> {
    let runtime_servers = servers
        .iter()
        .map(AdmissionIceServer::runtime_config)
        .collect::<Vec<_>>();
    IceServerConfig::validate_all(&runtime_servers).map_err(|error| error.to_string())?;

    let mut earliest_turn_expiry = None;
    for server in servers {
        if server.has_turn_url() {
            if server.username.is_empty() || server.credential.is_empty() {
                return Err("TURN servers require short-lived credentials".into());
            }
            let expiry = server.expires_at.ok_or("TURN servers require expiresAt")?;
            if expiry <= now.saturating_add(MIN_TURN_CREDENTIAL_TTL_SECONDS)
                || expiry > now.saturating_add(MAX_TURN_CREDENTIAL_TTL_SECONDS)
            {
                return Err("TURN expiresAt must be 31 seconds to 24 hours in the future".into());
            }
            earliest_turn_expiry = Some(
                earliest_turn_expiry
                    .map(|current: u64| current.min(expiry))
                    .unwrap_or(expiry),
            );
        } else if !server.username.is_empty()
            || !server.credential.is_empty()
            || server.expires_at.is_some()
        {
            return Err("STUN-only entries cannot contain credentials or expiresAt".into());
        }
    }

    if relay_only && earliest_turn_expiry.is_none() {
        return Err("relayOnly requires at least one TURN server".into());
    }
    if earliest_turn_expiry != turn_credential_expires_at {
        return Err("turnCredentialExpiresAt does not match the earliest TURN expiry".into());
    }
    Ok(runtime_servers)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerErrorKind {
    Reconnect,
    Expired,
    AdmissionRefresh,
    Rebootstrap,
}

impl WorkerErrorKind {
    fn label(self) -> &'static str {
        match self {
            Self::Reconnect => "reconnect",
            Self::Expired => "expired",
            Self::AdmissionRefresh => "admission_refresh",
            Self::Rebootstrap => "rebootstrap",
        }
    }
}

#[derive(Debug)]
struct WorkerError {
    kind: WorkerErrorKind,
    message: String,
}

impl WorkerError {
    fn reconnect(message: impl Into<String>) -> Self {
        Self {
            kind: WorkerErrorKind::Reconnect,
            message: message.into(),
        }
    }

    fn expired(message: impl Into<String>) -> Self {
        Self {
            kind: WorkerErrorKind::Expired,
            message: message.into(),
        }
    }

    fn rebootstrap(message: impl Into<String>) -> Self {
        Self {
            kind: WorkerErrorKind::Rebootstrap,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetrySchedule {
    phase: GatewayConnectionPhase,
    attempt: u32,
    delay: Duration,
}

fn retry_schedule(
    kind: WorkerErrorKind,
    prior_attempt: u32,
    had_connected_session: bool,
) -> RetrySchedule {
    if kind == WorkerErrorKind::AdmissionRefresh {
        return RetrySchedule {
            phase: GatewayConnectionPhase::AdmissionRefresh,
            attempt: 0,
            delay: Duration::ZERO,
        };
    }

    // A completed endpoint handshake proves the previous failure streak has
    // recovered.  Do not carry stale backoff across a healthy 80-second
    // admission window (or any other established session).
    let base_attempt = if had_connected_session {
        0
    } else {
        prior_attempt
    };
    let attempt = base_attempt.saturating_add(1);
    let exponent = attempt.saturating_sub(1).min(5);
    RetrySchedule {
        phase: match kind {
            WorkerErrorKind::Reconnect => GatewayConnectionPhase::Reconnecting,
            WorkerErrorKind::Expired => GatewayConnectionPhase::Expired,
            WorkerErrorKind::AdmissionRefresh => GatewayConnectionPhase::AdmissionRefresh,
            WorkerErrorKind::Rebootstrap => GatewayConnectionPhase::RebootstrapRequired,
        },
        attempt,
        delay: Duration::from_secs(1_u64 << exponent).min(MAX_BACKOFF),
    }
}

async fn gateway_worker(
    startup: GatewayStartup,
    radio: RadioHandle,
    host_rpc: Arc<HostRpc>,
    mut stop_rx: watch::Receiver<bool>,
    status: Arc<Mutex<GatewayStatusSnapshot>>,
) {
    let (mut app_id, plugin_id, endpoint_authority, mut initial) = match startup {
        GatewayStartup::Managed {
            app_id,
            plugin_id,
            endpoint_authority,
            initial,
        } => (app_id, plugin_id, endpoint_authority, initial),
    };
    let mut attempt = 0_u32;

    loop {
        if *stop_rx.borrow() {
            break;
        }
        let credentials = if let Some(credentials) = initial.take() {
            set_status(&status, GatewayConnectionPhase::Connecting, attempt, None);
            Ok(credentials)
        } else {
            set_status(
                &status,
                GatewayConnectionPhase::AdmissionRefresh,
                attempt,
                None,
            );
            refresh_admission(
                &host_rpc,
                app_id.as_deref(),
                &plugin_id,
                endpoint_authority.clone(),
            )
        };

        let outcome = match credentials {
            Ok(credentials) => {
                app_id = Some(credentials.app_id.clone());
                run_socket(
                    credentials,
                    &host_rpc,
                    &radio,
                    &mut stop_rx,
                    &status,
                    attempt,
                )
                .await
            }
            Err(error) => Err(error),
        };
        if *stop_rx.borrow() {
            break;
        }

        let had_connected_session = status
            .lock()
            .map(|status| status.connected)
            .unwrap_or(false);
        let error = outcome
            .err()
            .unwrap_or_else(|| WorkerError::reconnect("Companion gateway disconnected"));
        eprintln!(
            "[aokie-plugin][companion] stage=socket_ended kind={} detail={}",
            error.kind.label(),
            sanitize_status_message(&error.message)
        );
        radio
            .remote_media()
            .inspect(|media| media.fail_closed_all("gateway_disconnected"));
        let retry = retry_schedule(error.kind, attempt, had_connected_session);
        attempt = retry.attempt;
        set_status(&status, retry.phase, attempt, Some(error.message));
        if retry.delay.is_zero() {
            continue;
        }

        let sleep = tokio::time::sleep(retry.delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        set_status(&status, GatewayConnectionPhase::Stopped, attempt, None);
                        return;
                    }
                }
            }
        }
    }

    if let Some(media) = radio.remote_media() {
        media.fail_closed_all("gateway_stopped");
    }
    set_status(&status, GatewayConnectionPhase::Stopped, attempt, None);
}

fn refresh_admission(
    host_rpc: &HostRpc,
    app_id: Option<&str>,
    plugin_id: &str,
    endpoint_authority: Arc<EndpointAuthority>,
) -> Result<SessionCredentials, WorkerError> {
    let mut params = serde_json::Map::new();
    if let Some(app_id) = app_id {
        params.insert("appId".into(), Value::String(app_id.into()));
    }
    params.insert("pluginId".into(), Value::String(plugin_id.into()));
    params.insert("displayName".into(), Value::String("Aokie Desktop".into()));
    params.insert(
        "endpointPublicKey".into(),
        serde_json::to_value(&endpoint_authority.endpoint_key)
            .map_err(|_| WorkerError::rebootstrap("Endpoint public key could not be encoded"))?,
    );
    params.insert(
        "holderKeyThumbprint".into(),
        Value::String(endpoint_authority.endpoint_key.thumbprint.clone()),
    );
    params.insert(
        "approvedPeerKeyThumbprints".into(),
        serde_json::to_value(endpoint_authority.approved_thumbprints())
            .map_err(|_| WorkerError::rebootstrap("Endpoint roster could not be encoded"))?,
    );
    params.insert(
        "peerRosterRevision".into(),
        Value::from(endpoint_authority.roster_revision),
    );
    params.insert(
        "peerRosterHash".into(),
        Value::String(endpoint_authority.roster_hash.clone()),
    );
    let (request_id, line, receiver) = host_rpc.begin("companion.admission", Value::Object(params));
    let mut sink = StdoutSink::new();
    if sink.send_line(&line).is_err() {
        host_rpc.forget(request_id);
        return Err(WorkerError::rebootstrap(
            "Desktop admission broker is unavailable",
        ));
    }
    let value = match receiver.recv_timeout(ADMISSION_RPC_TIMEOUT) {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => {
            return Err(WorkerError::rebootstrap(
                "Desktop rejected Companion admission refresh",
            ))
        }
        Err(_) => {
            host_rpc.forget(request_id);
            return Err(WorkerError::rebootstrap(
                "Desktop admission refresh timed out",
            ));
        }
    };
    let response: AdmissionResponse = serde_json::from_value(value).map_err(|_| {
        WorkerError::rebootstrap("Desktop returned an invalid Companion admission response")
    })?;
    response.into_credentials(app_id, plugin_id, endpoint_authority)
}

fn set_status(
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    phase: GatewayConnectionPhase,
    reconnect_attempt: u32,
    last_error: Option<String>,
) {
    if let Ok(mut status) = status.lock() {
        status.connected = phase == GatewayConnectionPhase::Connected;
        status.phase = phase;
        status.reconnect_attempt = reconnect_attempt;
        status.last_error = last_error.map(|message| sanitize_status_message(&message));
        status.changed_at = aokie_core::events::now_iso8601();
    }
}

fn validate_identity(value: &str, field: &str) -> Result<(), String> {
    if !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Ok(())
    } else {
        Err(format!("privateBootstrap {field} is invalid"))
    }
}

fn validate_bearer(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_LEASE_TOKEN_BYTES
        || value.chars().any(char::is_control)
    {
        Err("privateBootstrap accessToken is invalid".into())
    } else {
        Ok(())
    }
}

fn normalize_gateway_url(raw: &str) -> Result<Url, String> {
    let mut url =
        Url::parse(raw).map_err(|_| "privateBootstrap gatewayUrl is invalid".to_string())?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(
            "privateBootstrap gatewayUrl must not contain credentials or a fragment".into(),
        );
    }
    let secure = url.scheme() == "wss";
    let debug_loopback = cfg!(debug_assertions)
        && url.scheme() == "ws"
        && url.host_str().is_some_and(is_loopback_host);
    let managed_beta_loopback = cfg!(feature = "managed-beta-driver")
        && url.scheme() == "ws"
        && url.host_str().is_some_and(is_numeric_loopback_host);
    if !secure && !debug_loopback && !managed_beta_loopback {
        return Err("privateBootstrap gatewayUrl must use wss (debug loopback may use ws)".into());
    }
    let path = url.path().trim_end_matches('/').to_string();
    if path.is_empty() {
        url.set_path("/v2/realtime");
    } else if path.ends_with("/v2/realtime") {
        url.set_path(&path);
    } else if path != "/v2/realtime" {
        let joined = format!("{path}/v2/realtime");
        url.set_path(&joined);
    }
    Ok(url)
}

fn is_numeric_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1"
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host.ends_with(".localhost")
}

fn redacted_url(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .map(|mut url| {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        })
        .unwrap_or_else(|| "[invalid URL]".into())
}

fn sanitize_status_message(message: &str) -> String {
    let without_lines = message.lines().next().unwrap_or("Companion gateway error");
    without_lines.chars().take(240).collect()
}

fn unix_now() -> Result<u64, WorkerError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| WorkerError::rebootstrap("System clock is before the Unix epoch"))
}

type GatewaySocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct PeerRoute {
    binding: SessionBinding,
    lease_jti: String,
    device_id: String,
    sdp_revision: u64,
    transport_generation: u64,
    lease_ttl_ms: u64,
    connected: bool,
    remote_audio_ready: bool,
    transition_requested: bool,
}

enum OutboundRtcSignal {
    Answer(String),
    Ice {
        candidate: String,
        sdp_mid: String,
        sdp_m_line_index: u16,
    },
    IceComplete,
}

struct PreparedTakeover {
    request_id: String,
    provisional: LeaseClaims,
    confirmed_owner_epoch: Option<u64>,
    provisional_sdp_revision: u64,
    provisional_transport_generation: u64,
    decision_sent: bool,
}

struct GatewaySession {
    app_id: String,
    plugin_id: String,
    plugin_session_nonce: String,
    endpoint_authority: Arc<EndpointAuthority>,
    used_endpoint_jtis: HashMap<String, u64>,
    ice_servers: Vec<IceServerConfig>,
    relay_only: bool,
    leases: HashMap<String, LeaseClaims>,
    peers: HashMap<String, PeerRoute>,
    prepared: Option<PreparedTakeover>,
    last_snapshot_fingerprint: Option<String>,
    last_snapshot_sent: Option<Instant>,
    authoritative_idle: bool,
    next_snapshot_poll: Instant,
    last_assistance_request_sent: Option<String>,
    pending_end_caller: HashMap<String, PendingEndCaller>,
}

struct PendingEndCaller {
    execute: PluginEndCallerExecuteFrame,
    result_rx: std::sync::mpsc::Receiver<Result<(), CompanionEndCallerFailure>>,
}

impl GatewaySession {
    fn new(credentials: &SessionCredentials, plugin_session_nonce: String) -> Self {
        Self {
            app_id: credentials.app_id.clone(),
            plugin_id: credentials.plugin_id.clone(),
            plugin_session_nonce,
            endpoint_authority: credentials.endpoint_authority.clone(),
            used_endpoint_jtis: HashMap::new(),
            ice_servers: credentials.ice_servers.clone(),
            relay_only: credentials.relay_only,
            leases: HashMap::new(),
            peers: HashMap::new(),
            prepared: None,
            last_snapshot_fingerprint: None,
            last_snapshot_sent: None,
            authoritative_idle: false,
            next_snapshot_poll: Instant::now(),
            last_assistance_request_sent: None,
            pending_end_caller: HashMap::new(),
        }
    }

    fn rotate_credentials(
        &mut self,
        credentials: &SessionCredentials,
        plugin_session_nonce: String,
    ) -> Result<(), WorkerError> {
        if self.app_id != credentials.app_id
            || self.plugin_id != credentials.plugin_id
            || self.endpoint_authority.endpoint_key != credentials.endpoint_authority.endpoint_key
            || self.endpoint_authority.roster_revision
                != credentials.endpoint_authority.roster_revision
            || self.endpoint_authority.roster_hash != credentials.endpoint_authority.roster_hash
        {
            return Err(WorkerError::rebootstrap(
                "Rotated Companion admission changed the endpoint authority",
            ));
        }
        self.plugin_session_nonce = plugin_session_nonce;
        self.endpoint_authority = credentials.endpoint_authority.clone();
        self.ice_servers = credentials.ice_servers.clone();
        self.relay_only = credentials.relay_only;
        self.last_snapshot_sent = None;
        self.authoritative_idle = false;
        self.next_snapshot_poll = Instant::now();
        self.last_assistance_request_sent = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aokie_protocol::v2::{tracks_for, MediaTrack, LEASE_AUDIENCE};

    fn test_authority() -> Arc<EndpointAuthority> {
        let signing_key = SigningKey::from_bytes(&[9; 32]);
        let endpoint_key =
            EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
        let mobile_signing_key = SigningKey::from_bytes(&[10; 32]);
        let mobile_key =
            EndpointPublicKey::from_ed25519_bytes(&mobile_signing_key.verifying_key().to_bytes());
        let roster_revision = 1;
        let roster_hash = peer_roster_hash(roster_revision, &[mobile_key.thumbprint.clone()]);
        Arc::new(EndpointAuthority {
            signing_key,
            endpoint_key,
            roster_revision,
            roster_hash,
            approved_mobile_keys: HashMap::from([(mobile_key.thumbprint.clone(), mobile_key)]),
        })
    }

    fn bootstrap_value() -> Value {
        let authority = test_authority();
        let mobile_keys = authority
            .approved_mobile_keys
            .values()
            .cloned()
            .collect::<Vec<_>>();
        json!({
            "schemaVersion": 2,
            "gatewayUrl": "wss://gateway.example.test/custom?ticket=hidden",
            "accessToken": "top-secret-bearer",
            "appId": "app_a",
            "pluginId": "aokie",
            "iceServers": [{
                "urls": ["stun:stun.example.test:3478"],
                "username": "",
                "credential": ""
            }],
            "relayOnly": false,
            "endpointIdentity": {
                "algorithm": authority.endpoint_key.algorithm,
                "publicKey": authority.endpoint_key.public_key,
                "thumbprint": authority.endpoint_key.thumbprint,
                "privateKeySeed": URL_SAFE_NO_PAD.encode(authority.signing_key.to_bytes())
            },
            "approvedMobileRoster": {
                "revision": authority.roster_revision,
                "rosterHash": authority.roster_hash,
                "keys": mobile_keys
            }
        })
    }

    fn admission(
        app_id: &str,
        subject_id: &str,
        authority: &EndpointAuthority,
    ) -> AdmissionResponse {
        serde_json::from_value(admission_value(app_id, subject_id, authority))
            .expect("test admission response is valid")
    }

    fn admission_value(app_id: &str, subject_id: &str, authority: &EndpointAuthority) -> Value {
        let now = unix_now().unwrap();
        let turn_credential_expires_at = now + 120;
        json!({
            "accessToken": "aokie-adm-v2.secret-value",
            "tokenType": "Bearer",
            "expiresIn": 60,
            "expiresAt": now + 60,
            "gatewayUrl": "wss://gateway.example.test",
            "appId": app_id,
            "subjectId": subject_id,
            "role": "plugin",
            "scopes": ["state_read"],
            "device": {"id": subject_id},
            "iceServers": [{
                "urls": ["turns:turn.example.test:5349"],
                "username": "ephemeral",
                "credential": "credential",
                "expiresAt": turn_credential_expires_at
            }],
            "relayOnly": true,
            "turnCredentialExpiresAt": turn_credential_expires_at,
            "endpointPublicKey": authority.endpoint_key,
            "holderKeyThumbprint": authority.endpoint_key.thumbprint,
            "approvedPeerKeyThumbprints": authority.approved_thumbprints(),
            "peerRosterRevision": authority.roster_revision,
            "peerRosterHash": authority.roster_hash
        })
    }

    fn test_gateway_session() -> GatewaySession {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();
        GatewaySession::new(&credentials, "plugin_session_a".into())
    }

    fn remote_snapshot(
        service_mode: LocalServiceMode,
        talk_audio_forwarded: bool,
    ) -> crate::remote_media::RemoteMediaSnapshot {
        crate::remote_media::RemoteMediaSnapshot {
            call_id: Some("call_a".into()),
            call_epoch: 1,
            owner_epoch: 1,
            remote_revision: 1,
            service_mode,
            peer_count: 1,
            talk_device_id: Some("device_a".into()),
            talk_lease_id: Some("lease_a".into()),
            talk_fence: 1,
            talk_audio_forwarded,
            radio_reserved: true,
            dropped_sco_frames: 0,
            quarantined_talk_frames: 0,
            dropped_events: 0,
            consent: crate::remote_media::RemoteConsentGate::default(),
            captions: Vec::new(),
        }
    }

    #[test]
    fn human_media_is_connecting_until_pcm_reaches_the_caller_tx_seam() {
        let waiting = remote_snapshot(LocalServiceMode::HumanActive, false);
        assert_eq!(
            authoritative_media_state(&waiting, true),
            MediaState::Connecting
        );

        let proven = remote_snapshot(LocalServiceMode::HumanActive, true);
        assert_eq!(authoritative_media_state(&proven, true), MediaState::Active);

        let consult = remote_snapshot(LocalServiceMode::ConsultActive, false);
        assert_eq!(
            authoritative_media_state(&consult, true),
            MediaState::Active
        );
    }

    #[test]
    fn admission_rotation_updates_transport_identity_without_dropping_native_routes() {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        let mut session = GatewaySession::new(&credentials, "plugin_session_old".into());
        install_takeover_route(&mut session, LeasePhase::Active);
        let lease_count = session.leases.len();
        let peer_count = session.peers.len();

        let mut refreshed = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();
        refreshed.relay_only = false;
        refreshed.ice_servers.clear();
        session
            .rotate_credentials(&refreshed, "plugin_session_new".into())
            .unwrap();

        assert_eq!(session.plugin_session_nonce, "plugin_session_new");
        assert!(!session.relay_only);
        assert!(session.ice_servers.is_empty());
        assert_eq!(session.leases.len(), lease_count);
        assert_eq!(session.peers.len(), peer_count);
    }

    fn takeover_claims(session: &GatewaySession, phase: LeasePhase) -> LeaseClaims {
        let owner_epoch = if phase == LeasePhase::Prepared { 3 } else { 4 };
        LeaseClaims {
            aud: LEASE_AUDIENCE.into(),
            app_id: session.app_id.clone(),
            plugin_id: session.plugin_id.clone(),
            device_id: "device_a".into(),
            plugin_key_thumbprint: session.endpoint_authority.endpoint_key.thumbprint.clone(),
            mobile_key_thumbprint: session
                .endpoint_authority
                .approved_mobile_keys
                .keys()
                .next()
                .expect("test authority has a mobile key")
                .clone(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch,
            mode: LeaseMode::Takeover,
            phase,
            tracks: tracks_for(LeaseMode::Takeover, phase),
            expires_at: unix_now().unwrap() + 20,
            lease_id: "lease_stable".into(),
            jti: if phase == LeasePhase::Prepared {
                "lease_prepared_jti".into()
            } else {
                "lease_active_jti".into()
            },
            fence: 9,
            session_nonce: "mobile_session".into(),
            rtc_session_id: "rtc_a".into(),
        }
    }

    fn install_takeover_route(session: &mut GatewaySession, phase: LeasePhase) -> RemoteMediaEvent {
        let provisional = takeover_claims(session, LeasePhase::Prepared);
        let claims = takeover_claims(session, phase);
        let binding = binding_for_claims(&claims);
        session.leases.insert(claims.jti.clone(), claims.clone());
        session.prepared = Some(PreparedTakeover {
            request_id: "request_a".into(),
            provisional,
            confirmed_owner_epoch: (phase == LeasePhase::Active).then_some(claims.owner_epoch),
            provisional_sdp_revision: 1,
            provisional_transport_generation: 1,
            decision_sent: phase == LeasePhase::Active,
        });
        session.peers.insert(
            binding.rtc_session_id.clone(),
            PeerRoute {
                binding: binding.clone(),
                lease_jti: claims.jti,
                device_id: claims.device_id,
                sdp_revision: if phase == LeasePhase::Prepared { 1 } else { 2 },
                transport_generation: if phase == LeasePhase::Prepared { 1 } else { 2 },
                lease_ttl_ms: 20_000,
                connected: true,
                remote_audio_ready: phase == LeasePhase::Active,
                transition_requested: true,
            },
        );
        RemoteMediaEvent {
            sequence: 1,
            rtc_session_id: binding.rtc_session_id,
            call_id: binding.call_id,
            call_epoch: binding.call_epoch,
            owner_epoch: binding.owner_epoch,
            kind: RemoteMediaEventKind::ReturningToAokie {
                reason: "sco_unavailable".into(),
            },
        }
    }

    #[test]
    fn scheduled_admission_refresh_is_immediate_and_resets_failure_backoff() {
        let retry = retry_schedule(WorkerErrorKind::AdmissionRefresh, 32, true);
        assert_eq!(retry.phase, GatewayConnectionPhase::AdmissionRefresh);
        assert_eq!(retry.attempt, 0);
        assert_eq!(retry.delay, Duration::ZERO);
    }

    #[test]
    fn established_session_resets_backoff_but_real_failures_still_escalate() {
        let recovered_then_dropped = retry_schedule(WorkerErrorKind::Reconnect, 32, true);
        assert_eq!(
            recovered_then_dropped.phase,
            GatewayConnectionPhase::Reconnecting
        );
        assert_eq!(recovered_then_dropped.attempt, 1);
        assert_eq!(recovered_then_dropped.delay, Duration::from_secs(1));

        let second_consecutive_failure = retry_schedule(
            WorkerErrorKind::Reconnect,
            recovered_then_dropped.attempt,
            false,
        );
        assert_eq!(second_consecutive_failure.attempt, 2);
        assert_eq!(second_consecutive_failure.delay, Duration::from_secs(2));

        let unsafe_admission = retry_schedule(WorkerErrorKind::Expired, 5, false);
        assert_eq!(unsafe_admission.phase, GatewayConnectionPhase::Expired);
        assert_eq!(unsafe_admission.attempt, 6);
        assert_eq!(unsafe_admission.delay, MAX_BACKOFF);
    }

    #[test]
    fn idle_publication_is_explicit_once_per_no_call_transition() {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();
        let mut session = GatewaySession::new(&credentials, "plugin_session_a".into());

        let encoded = session
            .idle_transition_frame()
            .unwrap()
            .expect("initial idle state must be published");
        let frame: PluginIdleFrame = serde_json::from_str(&encoded).unwrap();
        frame.validate().unwrap();
        assert_eq!(frame.app_id, "app_a");
        assert_eq!(
            serde_json::from_str::<Value>(&encoded)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["appId", "eventId", "kind", "schemaVersion"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert!(session.idle_transition_frame().unwrap().is_none());

        // A subsequently published call re-arms exactly one authoritative
        // no-call assertion when physical state returns to idle.
        session.authoritative_idle = false;
        assert!(session.idle_transition_frame().unwrap().is_some());
        assert!(session.idle_transition_frame().unwrap().is_none());
    }

    #[test]
    fn bootstrap_is_strict_and_debug_output_never_contains_bearer() {
        let value = bootstrap_value();
        let bootstrap = CompanionBootstrap::parse(&value).unwrap();
        let rendered = format!("{bootstrap:?}");
        assert!(!rendered.contains("top-secret-bearer"));
        assert!(!rendered.contains("ticket=hidden"));
        let endpoint = normalize_gateway_url(bootstrap.gateway_url.as_deref().unwrap()).unwrap();
        assert_eq!(endpoint.path(), "/custom/v2/realtime");

        let mut extra = value;
        extra["unexpected"] = json!(true);
        assert!(CompanionBootstrap::parse(&extra).is_err());
    }

    #[test]
    fn identity_only_bootstrap_starts_with_brokered_admission_refresh() {
        let mut value = bootstrap_value();
        let object = value.as_object_mut().unwrap();
        object.remove("gatewayUrl");
        object.remove("accessToken");
        object.remove("appId");
        object.remove("iceServers");
        object.remove("relayOnly");

        let bootstrap = CompanionBootstrap::parse(&value).unwrap();
        assert!(bootstrap.gateway_url.is_none());
        assert!(bootstrap.access_token.is_none());
        assert!(bootstrap.app_id.is_none());
        let startup = GatewayStartup::from_bootstrap(bootstrap).unwrap();
        assert!(matches!(
            startup,
            GatewayStartup::Managed {
                app_id: None,
                plugin_id,
                initial: None,
                ..
            } if plugin_id == "aokie"
        ));

        let mut torn = value;
        torn["gatewayUrl"] = json!("wss://gateway.example.test/v2/realtime");
        assert!(CompanionBootstrap::parse(&torn)
            .unwrap_err()
            .contains("all present or all absent"));
    }

    #[test]
    fn managed_admission_accepts_current_formlogic_ice_shape_and_pins_identity() {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        assert_eq!(credentials.app_id, "app_a");
        assert_eq!(credentials.plugin_id, "aokie");
        assert!(credentials.relay_only);
        assert!(credentials.turn_credential_expires_at.is_some());
        assert_eq!(credentials.ice_servers.len(), 1);
        assert_eq!(
            credentials.ice_servers[0].urls,
            vec!["turns:turn.example.test:5349"]
        );
        assert_eq!(credentials.ice_servers[0].username, "ephemeral");
        assert_eq!(credentials.ice_servers[0].credential, "credential");
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("secret-value"));

        let wrong = admission("app_b", "other", &authority)
            .into_credentials(Some("app_a"), "aokie", authority.clone())
            .unwrap_err();
        assert!(matches!(wrong.kind, WorkerErrorKind::Rebootstrap));

        let different_key = EndpointPublicKey::from_ed25519_bytes(
            &SigningKey::from_bytes(&[99; 32]).verifying_key().to_bytes(),
        );
        let mut wrong_endpoint = admission("app_a", "aokie", &authority);
        wrong_endpoint.endpoint_public_key = different_key;
        let error = wrong_endpoint
            .into_credentials(Some("app_a"), "aokie", authority.clone())
            .unwrap_err();
        assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

        let mut missing_endpoint = admission_value("app_a", "aokie", &authority);
        missing_endpoint
            .as_object_mut()
            .unwrap()
            .remove("endpointPublicKey");
        assert!(serde_json::from_value::<AdmissionResponse>(missing_endpoint).is_err());
    }

    #[test]
    fn managed_admission_requires_every_formlogic_ice_policy_member() {
        let authority = test_authority();
        for member in ["iceServers", "relayOnly", "turnCredentialExpiresAt"] {
            let mut value = admission_value("app_a", "aokie", &authority);
            value.as_object_mut().unwrap().remove(member);
            assert!(
                serde_json::from_value::<AdmissionResponse>(value).is_err(),
                "missing {member} must fail closed"
            );
        }
        for member in ["urls", "username", "credential"] {
            let mut value = admission_value("app_a", "aokie", &authority);
            value["iceServers"][0]
                .as_object_mut()
                .unwrap()
                .remove(member);
            assert!(
                serde_json::from_value::<AdmissionResponse>(value).is_err(),
                "missing ICE server {member} must fail closed"
            );
        }

        let mut null_server_expiry = admission_value("app_a", "aokie", &authority);
        null_server_expiry["iceServers"][0]["expiresAt"] = Value::Null;
        assert!(serde_json::from_value::<AdmissionResponse>(null_server_expiry).is_err());

        let mut direct = admission_value("app_a", "aokie", &authority);
        direct["iceServers"] = json!([]);
        direct["relayOnly"] = json!(false);
        direct["turnCredentialExpiresAt"] = Value::Null;
        let credentials = serde_json::from_value::<AdmissionResponse>(direct)
            .unwrap()
            .into_credentials(Some("app_a"), "aokie", authority)
            .unwrap();
        assert!(credentials.ice_servers.is_empty());
        assert!(!credentials.relay_only);
        assert_eq!(credentials.turn_credential_expires_at, None);
    }

    #[test]
    fn managed_admission_rejects_inconsistent_or_unsafe_turn_policy() {
        let authority = test_authority();

        let mut mismatch = admission_value("app_a", "aokie", &authority);
        mismatch["turnCredentialExpiresAt"] =
            json!(mismatch["turnCredentialExpiresAt"].as_u64().unwrap() + 1);
        let error = serde_json::from_value::<AdmissionResponse>(mismatch)
            .unwrap()
            .into_credentials(Some("app_a"), "aokie", authority.clone())
            .unwrap_err();
        assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

        let mut relay_without_turn = admission_value("app_a", "aokie", &authority);
        relay_without_turn["iceServers"] = json!([{
            "urls": ["stun:stun.example.test:3478"],
            "username": "",
            "credential": ""
        }]);
        relay_without_turn["turnCredentialExpiresAt"] = Value::Null;
        let error = serde_json::from_value::<AdmissionResponse>(relay_without_turn)
            .unwrap()
            .into_credentials(Some("app_a"), "aokie", authority.clone())
            .unwrap_err();
        assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

        let mut missing_expiry = admission_value("app_a", "aokie", &authority);
        missing_expiry["iceServers"][0]
            .as_object_mut()
            .unwrap()
            .remove("expiresAt");
        let error = serde_json::from_value::<AdmissionResponse>(missing_expiry)
            .unwrap()
            .into_credentials(Some("app_a"), "aokie", authority.clone())
            .unwrap_err();
        assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

        let now = unix_now().unwrap();
        for unsafe_expiry in [
            now + MIN_TURN_CREDENTIAL_TTL_SECONDS,
            now + MAX_TURN_CREDENTIAL_TTL_SECONDS + 60,
        ] {
            let mut value = admission_value("app_a", "aokie", &authority);
            value["iceServers"][0]["expiresAt"] = json!(unsafe_expiry);
            value["turnCredentialExpiresAt"] = json!(unsafe_expiry);
            let error = serde_json::from_value::<AdmissionResponse>(value)
                .unwrap()
                .into_credentials(Some("app_a"), "aokie", authority.clone())
                .unwrap_err();
            assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));
        }
    }

    #[test]
    fn managed_admission_bounds_socket_lifetime_by_turn_expiry() {
        let authority = test_authority();
        let now = unix_now().unwrap();
        let turn_expiry = now + 40;
        let mut value = admission_value("app_a", "aokie", &authority);
        value["iceServers"][0]["expiresAt"] = json!(turn_expiry);
        value["turnCredentialExpiresAt"] = json!(turn_expiry);
        let credentials = serde_json::from_value::<AdmissionResponse>(value)
            .unwrap()
            .into_credentials(Some("app_a"), "aokie", authority)
            .unwrap();
        assert!(credentials.lifetime <= Duration::from_secs(30));
        assert!(credentials.lifetime >= Duration::from_secs(20));
    }

    #[test]
    fn lease_modes_map_to_immutable_native_bindings() {
        let now = unix_now().unwrap();
        let claims = LeaseClaims {
            aud: LEASE_AUDIENCE.into(),
            app_id: "app_a".into(),
            plugin_id: "aokie".into(),
            device_id: "device_a".into(),
            plugin_key_thumbprint: "plugin_key_a".into(),
            mobile_key_thumbprint: "mobile_key_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 3,
            mode: LeaseMode::Takeover,
            phase: LeasePhase::Prepared,
            tracks: tracks_for(LeaseMode::Takeover, LeasePhase::Prepared),
            expires_at: now + 20,
            lease_id: "lease_stable".into(),
            jti: "lease_jti".into(),
            fence: 9,
            session_nonce: "mobile_session".into(),
            rtc_session_id: "rtc_a".into(),
        };
        assert_eq!(claims.tracks, vec![MediaTrack::PstnIn]);
        let prepared = binding_for_claims(&claims);
        assert_eq!(prepared.mode, MediaMode::PreparedTalk);
        assert!(!prepared.mode.may_transmit_to_caller());

        let mut active = claims;
        active.phase = LeasePhase::Active;
        active.owner_epoch += 1;
        active.tracks = tracks_for(LeaseMode::Takeover, LeasePhase::Active);
        let talk = binding_for_claims(&active);
        assert_eq!(talk.mode, MediaMode::Talk);
        assert!(talk.mode.may_transmit_to_caller());
    }

    #[test]
    fn automatic_takeover_terminal_events_revoke_prepared_and_active_authority() {
        for (phase, event_kind) in [
            (
                LeasePhase::Prepared,
                RemoteMediaEventKind::ReturningToAokie {
                    reason: "sco_unavailable".into(),
                },
            ),
            (
                LeasePhase::Active,
                RemoteMediaEventKind::Closed {
                    reason: "physical_call_changed".into(),
                },
            ),
        ] {
            let mut session = test_gateway_session();
            let mut event = install_takeover_route(&mut session, phase);
            event.kind = event_kind;
            let media = RemoteMediaHandle::spawn().unwrap();

            let reason = match &event.kind {
                RemoteMediaEventKind::ReturningToAokie { reason }
                | RemoteMediaEventKind::Closed { reason } => reason.clone(),
                _ => unreachable!(),
            };
            let encoded = session
                .fail_route_if_current(&event, &reason, &media)
                .unwrap()
                .expect("a current failed route must revoke its lease");
            let frame: PluginLeaseRevokeFrame = serde_json::from_str(&encoded).unwrap();
            frame.validate().unwrap();
            assert_eq!(frame.lease_id, "lease_stable");
            assert_eq!(
                frame.lease_jti,
                if phase == LeasePhase::Prepared {
                    "lease_prepared_jti"
                } else {
                    "lease_active_jti"
                }
            );
            assert_eq!(frame.reason, reason);
            assert!(session.peers.is_empty());
            assert!(session.leases.is_empty());
            assert!(session.prepared.is_none());
        }
    }

    #[test]
    fn authoritative_operator_return_does_not_emit_a_second_plugin_revoke() {
        let mut session = test_gateway_session();
        let mut event = install_takeover_route(&mut session, LeasePhase::Active);
        event.kind = RemoteMediaEventKind::ReturningToAokie {
            reason: "operator_return".into(),
        };
        let media = RemoteMediaHandle::spawn().unwrap();
        session
            .handle_lease_revoked(
                LeaseRevokedNotice {
                    kind: "lease_revoked".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    lease_id: "lease_stable".into(),
                    lease_jti: "lease_active_jti".into(),
                    call_id: "call_a".into(),
                    call_epoch: 7,
                    fence: 9,
                    reason: "operator_return".into(),
                },
                &media,
            )
            .unwrap();

        assert!(session
            .fail_route_if_current(&event, "operator_return", &media)
            .unwrap()
            .is_none());
        assert!(session.peers.is_empty());
        assert!(session.leases.is_empty());
        assert!(session.prepared.is_none());
    }

    #[test]
    fn stale_terminal_event_cannot_revoke_a_newer_takeover_route() {
        let mut session = test_gateway_session();
        let mut event = install_takeover_route(&mut session, LeasePhase::Active);
        event.owner_epoch -= 1;
        let media = RemoteMediaHandle::spawn().unwrap();

        assert!(session
            .fail_route_if_current(&event, "physical_call_changed", &media)
            .unwrap()
            .is_none());
        assert!(session.peers.contains_key("rtc_a"));
        assert!(session.leases.contains_key("lease_active_jti"));
        assert!(session.prepared.is_some());
    }

    #[test]
    fn caller_mask_never_exposes_more_than_four_digits() {
        assert_eq!(mask_number("+61 412 345 678"), Some("***5678".into()));
        assert_eq!(mask_number("private"), None);
    }

    #[test]
    fn caller_end_result_preserves_every_physical_fence_and_typed_failure() {
        let execute = PluginEndCallerExecuteFrame {
            kind: "end_caller_execute".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            operation_id: "operation_a".into(),
            confirmation_id: "confirmation_a".into(),
            device_id: "device_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            switchboard_revision: 11,
            remote_revision: 13,
            lease_id: "media_a".into(),
            lease_jti: "lease_a".into(),
            fence: 9,
        };
        let failure = encode_end_caller_failure(
            &execute,
            "physical_fence_stale",
            "the physical call changed",
        )
        .unwrap();
        let frame: PluginEndCallerResultFrame = serde_json::from_str(&failure).unwrap();
        frame.validate().unwrap();
        assert_eq!(frame.outcome, EndCallerOutcome::Failed);
        assert_eq!(frame.operation_id, execute.operation_id);
        assert_eq!(frame.call_epoch, execute.call_epoch);
        assert_eq!(frame.owner_epoch, execute.owner_epoch);
        assert_eq!(frame.switchboard_revision, execute.switchboard_revision);
        assert_eq!(frame.remote_revision, execute.remote_revision);
        assert_eq!(frame.fence, execute.fence);
    }

    #[test]
    fn managed_beta_allows_only_numeric_loopback_plain_ws() {
        let numeric = normalize_gateway_url("ws://127.0.0.1:18787/v2/realtime");
        if cfg!(feature = "managed-beta-driver") || cfg!(debug_assertions) {
            assert!(numeric.is_ok());
        } else {
            assert!(numeric.is_err());
        }
        assert!(normalize_gateway_url("ws://192.168.1.40:18787/v2/realtime").is_err());
        assert!(normalize_gateway_url("ws://gateway.example.test/v2/realtime").is_err());
        if cfg!(feature = "managed-beta-driver") && !cfg!(debug_assertions) {
            assert!(normalize_gateway_url("ws://localhost:18787/v2/realtime").is_err());
        }
    }

    #[test]
    fn gateway_error_envelope_accepts_typed_fields_without_app_identity() {
        let encoded = json!({
            "kind": "error",
            "schemaVersion": SCHEMA_VERSION,
            "code": "stale_snapshot",
            "message": "plugin snapshot regressed",
            "requestId": null
        })
        .to_string();

        let envelope: Envelope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(envelope.kind, "error");
        assert_eq!(envelope.schema_version, SCHEMA_VERSION);
        assert!(envelope.app_id.is_none());

        let notice: ErrorNotice = parse_gateway_frame(&encoded).unwrap();
        assert_eq!(notice.code, "stale_snapshot");
    }
}

async fn open_gateway_socket(
    credentials: &SessionCredentials,
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    attempt: u32,
) -> Result<(GatewaySocket, String), WorkerError> {
    let mut request = credentials
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| WorkerError::rebootstrap("Companion gateway URL cannot form a request"))?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", credentials.token))
        .map_err(|_| WorkerError::rebootstrap("Companion admission token is invalid"))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("authorization", authorization);
    request.headers_mut().insert(
        "x-aokie-app-id",
        HeaderValue::from_str(&credentials.app_id)
            .map_err(|_| WorkerError::rebootstrap("Companion app identity is invalid"))?,
    );
    let plugin_header = HeaderValue::from_str(&credentials.plugin_id)
        .map_err(|_| WorkerError::rebootstrap("Companion plugin identity is invalid"))?;
    request
        .headers_mut()
        .insert("x-aokie-device-id", plugin_header.clone());
    request
        .headers_mut()
        .insert("x-aokie-plugin-id", plugin_header);

    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(safe_ws_error)?;
    let challenge_encoded = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .map_err(|_| WorkerError::reconnect("Companion endpoint challenge timed out"))?
        .ok_or_else(|| WorkerError::reconnect("Companion gateway closed before challenge"))?
        .map_err(safe_ws_error)?;
    let Message::Text(challenge_encoded) = challenge_encoded else {
        return Err(WorkerError::reconnect(
            "Companion gateway did not send a text endpoint challenge",
        ));
    };
    let challenge: EndpointChallengeFrame = serde_json::from_str(challenge_encoded.as_str())
        .map_err(|_| WorkerError::reconnect("Companion endpoint challenge is malformed"))?;
    let now = unix_now()?;
    challenge
        .validate(now)
        .map_err(|_| WorkerError::reconnect("Companion endpoint challenge is invalid"))?;
    let authority = &credentials.endpoint_authority;
    if challenge.app_id != credentials.app_id
        || challenge.subject_id != credentials.plugin_id
        || challenge.role != AdmissionRole::Plugin
        || challenge.holder_key_thumbprint != authority.endpoint_key.thumbprint
        || challenge.expected_peer_key_thumbprint.is_some()
        || challenge.approved_peer_key_thumbprints != authority.approved_thumbprints()
        || challenge.peer_roster_revision != Some(authority.roster_revision)
        || challenge.peer_roster_hash.as_deref() != Some(authority.roster_hash.as_str())
    {
        return Err(WorkerError::rebootstrap(
            "Companion endpoint challenge does not match the local identity and approved roster",
        ));
    }
    let session_nonce = format!("plugin_session_{}", uuid::Uuid::new_v4().simple());
    let proof_claims = HelloProofClaims {
        app_id: credentials.app_id.clone(),
        subject_id: credentials.plugin_id.clone(),
        role: AdmissionRole::Plugin,
        connection_id: challenge.connection_id.clone(),
        challenge_nonce: challenge.challenge_nonce.clone(),
        admission_jti: challenge.admission_jti.clone(),
        session_nonce: session_nonce.clone(),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        nonce: format!("proof_nonce_{}", uuid::Uuid::new_v4().simple()),
        jti: format!("proof_jti_{}", uuid::Uuid::new_v4().simple()),
        issued_at: now,
        expires_at: challenge.expires_at.min(now.saturating_add(30)),
    };
    proof_claims
        .validate(now)
        .map_err(|_| WorkerError::rebootstrap("Companion endpoint proof claims are invalid"))?;
    let proof = SignedHelloProof {
        endpoint_key: authority.endpoint_key.clone(),
        signature: authority.sign(&proof_claims.signing_bytes().map_err(|_| {
            WorkerError::rebootstrap("Companion endpoint proof could not be canonicalized")
        })?),
        claims: proof_claims,
    };
    let hello = PluginHello {
        kind: "plugin_hello".into(),
        schema_version: SCHEMA_VERSION,
        app_id: credentials.app_id.clone(),
        plugin_id: credentials.plugin_id.clone(),
        session_nonce: session_nonce.clone(),
        endpoint_proof: proof,
    };
    hello
        .validate()
        .map_err(|_| WorkerError::rebootstrap("Companion plugin hello is invalid"))?;
    send_json(&mut socket, &hello).await?;
    set_status(status, GatewayConnectionPhase::Connected, attempt, None);
    Ok((socket, session_nonce))
}

async fn run_socket(
    mut credentials: SessionCredentials,
    host_rpc: &HostRpc,
    radio: &RadioHandle,
    stop_rx: &mut watch::Receiver<bool>,
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    attempt: u32,
) -> Result<(), WorkerError> {
    let (mut socket, session_nonce) = open_gateway_socket(&credentials, status, attempt).await?;
    let media = radio
        .remote_media()
        .ok_or_else(|| WorkerError::reconnect("Companion media endpoint is unavailable"))?;
    let mut session = GatewaySession::new(&credentials, session_nonce);
    let mut admission_deadline = Instant::now() + credentials.lifetime;
    let mut retiring_socket: Option<(GatewaySocket, Instant)> = None;
    let mut next_ping = Instant::now() + PING_INTERVAL;
    let mut awaiting_pong: Option<Instant> = None;

    loop {
        if retiring_socket
            .as_ref()
            .is_some_and(|(_, deadline)| Instant::now() >= *deadline)
        {
            retiring_socket.take();
        }
        if *stop_rx.borrow() {
            let _ = socket.send(Message::Close(None)).await;
            if let Some((mut retiring, _)) = retiring_socket.take() {
                let _ = retiring.send(Message::Close(None)).await;
            }
            return Ok(());
        }
        if Instant::now() >= admission_deadline {
            set_status(
                status,
                GatewayConnectionPhase::AdmissionRefresh,
                attempt,
                None,
            );
            let refreshed = refresh_admission(
                host_rpc,
                Some(&credentials.app_id),
                &credentials.plugin_id,
                credentials.endpoint_authority.clone(),
            )?;
            let (replacement, replacement_nonce) =
                open_gateway_socket(&refreshed, status, attempt).await?;
            session.rotate_credentials(&refreshed, replacement_nonce)?;

            // Keep the authenticated predecessor alive briefly while the
            // gateway consumes the replacement hello. The gateway then sees a
            // live same-authority rotation, preserves every current lease and
            // fences the predecessor itself. Dropping the old socket first
            // would make an otherwise healthy active call look like an outage.
            let predecessor = std::mem::replace(&mut socket, replacement);
            retiring_socket = Some((predecessor, Instant::now() + Duration::from_secs(2)));
            credentials = refreshed;
            admission_deadline = Instant::now() + credentials.lifetime;
            next_ping = Instant::now() + PING_INTERVAL;
            awaiting_pong = None;
            eprintln!(
                "[aokie-plugin][companion] stage=admission_rotated continuity=preserved app={} plugin={} active_peers={}",
                credentials.app_id,
                credentials.plugin_id,
                session.peers.len()
            );
            continue;
        }

        for encoded in session.drain_end_caller_results()? {
            socket
                .send(Message::Text(encoded.into()))
                .await
                .map_err(safe_ws_error)?;
        }
        for encoded in session.drain_media_events(media, radio)? {
            socket
                .send(Message::Text(encoded.into()))
                .await
                .map_err(safe_ws_error)?;
        }
        if Instant::now() >= session.next_snapshot_poll {
            session.next_snapshot_poll = Instant::now() + SNAPSHOT_POLL;
            if let Some(encoded) = session.authoritative_state_frame(radio)? {
                socket
                    .send(Message::Text(encoded.into()))
                    .await
                    .map_err(safe_ws_error)?;
            }
            if let Some(encoded) = session.assistance_frame(radio)? {
                socket
                    .send(Message::Text(encoded.into()))
                    .await
                    .map_err(safe_ws_error)?;
            }
        }

        if let Some(sent_at) = awaiting_pong {
            if sent_at.elapsed() >= PONG_TIMEOUT {
                return Err(WorkerError::reconnect(
                    "Companion gateway heartbeat timed out",
                ));
            }
        }
        if Instant::now() >= next_ping {
            socket
                .send(Message::Ping(Default::default()))
                .await
                .map_err(safe_ws_error)?;
            awaiting_pong = Some(Instant::now());
            next_ping = Instant::now() + PING_INTERVAL;
        }

        match tokio::time::timeout(READ_TICK, socket.next()).await {
            Err(_) => {}
            Ok(Some(Ok(Message::Text(encoded)))) => {
                let inbound_kind = serde_json::from_str::<Envelope>(encoded.as_str())
                    .map(|frame| frame.kind)
                    .unwrap_or_else(|_| "malformed".into());
                let outbound = session
                    .handle_inbound(encoded.as_str(), media, radio)
                    .map_err(|error| {
                        eprintln!(
                            "[aokie-plugin][companion] stage=inbound_rejected frame={} kind={} detail={}",
                            inbound_kind,
                            error.kind.label(),
                            sanitize_status_message(&error.message)
                        );
                        error
                    })?;
                for encoded in outbound {
                    socket
                        .send(Message::Text(encoded.into()))
                        .await
                        .map_err(safe_ws_error)?;
                }
            }
            Ok(Some(Ok(Message::Ping(payload)))) => {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(safe_ws_error)?;
            }
            Ok(Some(Ok(Message::Pong(_)))) => awaiting_pong = None,
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                return Err(WorkerError::reconnect(
                    "Companion gateway closed the socket",
                ));
            }
            Ok(Some(Ok(_))) => {
                return Err(WorkerError::reconnect(
                    "Companion gateway sent a non-text protocol frame",
                ));
            }
            Ok(Some(Err(error))) => return Err(safe_ws_error(error)),
        }
    }
}

async fn send_json<T: Serialize>(socket: &mut GatewaySocket, value: &T) -> Result<(), WorkerError> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?;
    socket
        .send(Message::Text(encoded.into()))
        .await
        .map_err(safe_ws_error)
}

fn safe_ws_error(error: tungstenite::Error) -> WorkerError {
    let message = match error {
        tungstenite::Error::Http(response) => {
            format!(
                "Companion gateway rejected the connection ({})",
                response.status()
            )
        }
        tungstenite::Error::Io(error) => {
            format!("Companion gateway network error ({:?})", error.kind())
        }
        tungstenite::Error::Tls(_) => "Companion gateway TLS validation failed".into(),
        tungstenite::Error::Capacity(_) => "Companion gateway frame exceeded a safety limit".into(),
        tungstenite::Error::Protocol(_) => "Companion gateway WebSocket protocol failed".into(),
        tungstenite::Error::Url(_) => "Companion gateway URL is unsupported".into(),
        tungstenite::Error::HttpFormat(_) => "Companion gateway request headers are invalid".into(),
        _ => "Companion gateway connection failed".into(),
    };
    WorkerError::reconnect(message)
}

impl GatewaySession {
    fn authoritative_state_frame(
        &mut self,
        radio: &RadioHandle,
    ) -> Result<Option<String>, WorkerError> {
        if radio.current_call_id().is_none() {
            return self.idle_transition_frame();
        }

        let encoded = self.snapshot_frame(radio)?;
        if encoded.is_some() {
            self.authoritative_idle = false;
        }
        Ok(encoded)
    }

    fn idle_transition_frame(&mut self) -> Result<Option<String>, WorkerError> {
        if self.authoritative_idle {
            return Ok(None);
        }
        let frame = PluginIdleFrame {
            kind: "plugin_idle".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            event_id: format!("idle_{}", uuid::Uuid::new_v4().simple()),
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Authoritative idle frame is invalid"))?;
        let encoded = serde_json::to_string(&frame)
            .map_err(|_| WorkerError::reconnect("Authoritative idle frame could not be encoded"))?;
        self.authoritative_idle = true;
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        Ok(Some(encoded))
    }

    fn snapshot_frame(&mut self, radio: &RadioHandle) -> Result<Option<String>, WorkerError> {
        let Some(call_id) = radio.current_call_id() else {
            return Ok(None);
        };
        let media = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media endpoint is unavailable"))?;
        let remote = media.snapshot();
        if remote.call_id.as_deref() != Some(call_id.as_str()) || remote.call_epoch == 0 {
            return Ok(None);
        }
        let active = radio.is_call_active();
        let service_mode = map_service_mode(remote.service_mode);
        let media_state = authoritative_media_state(&remote, active);
        let caller = radio.current_caller().map(|number| CallerProjection {
            label: None,
            masked_number: mask_number(&number),
        });
        let snapshot = AuthoritativeCallSnapshot {
            call_id,
            call_epoch: remote.call_epoch,
            owner_epoch: remote.owner_epoch,
            switchboard_revision: radio.switchboard_revision(),
            remote_revision: remote.remote_revision,
            telephony_state: if active {
                TelephonyState::Active
            } else {
                TelephonyState::Ringing
            },
            service_mode,
            media_state,
            remote_capabilities: RemoteCapabilities {
                software_hold: true,
                // The local software hold is authoritative; carrier-network
                // hold and secondary-call behaviour remain unknown until the
                // radio reports actual negotiated/observed evidence.
                carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                secondary_call_observation: SecondaryCallObservation::Unknown,
                voice_consult: remote.consent.consult_enabled,
                takeover: remote.consent.takeover_enabled,
            },
            secondary_call_policy: SecondaryCallPolicy::Normal,
            secondary_call: None,
            remote_consent: RemoteConsentPolicy {
                policy_id: remote.consent.policy_id.clone(),
                policy_version: remote.consent.policy_version,
                enabled: remote.consent.enabled,
                acknowledged: remote.consent.acknowledged,
                acknowledged_at: remote.consent.acknowledged_at.clone(),
                expires_at: remote.consent.expires_at.clone(),
                captions_enabled: remote.consent.captions_enabled,
                assistance_enabled: remote.consent.assistance_enabled,
                monitor_enabled: remote.consent.monitor_enabled,
                consult_enabled: remote.consent.consult_enabled,
                takeover_enabled: remote.consent.takeover_enabled,
            },
            caller,
            captions: remote
                .captions
                .iter()
                .map(|caption| Caption {
                    caption_id: caption.caption_id.clone(),
                    speaker: caption.speaker.clone(),
                    text: caption.text.clone(),
                    occurred_at: caption.occurred_at.clone(),
                    final_text: caption.final_text,
                })
                .collect(),
            audio_levels: None,
            occurred_at: aokie_core::events::now_iso8601(),
        };
        snapshot
            .validate()
            .map_err(|_| WorkerError::reconnect("Authoritative call snapshot is invalid"))?;
        let fingerprint = serde_json::to_string(&json!({
            "callId": snapshot.call_id,
            "callEpoch": snapshot.call_epoch,
            "ownerEpoch": snapshot.owner_epoch,
            "switchboardRevision": snapshot.switchboard_revision,
            "remoteRevision": snapshot.remote_revision,
            "telephonyState": snapshot.telephony_state,
            "serviceMode": snapshot.service_mode,
            "mediaState": snapshot.media_state,
            "remoteCapabilities": snapshot.remote_capabilities,
            "secondaryCallPolicy": snapshot.secondary_call_policy,
            "secondaryCall": snapshot.secondary_call,
            "caller": snapshot.caller,
            "remoteConsent": snapshot.remote_consent,
            "captions": snapshot.captions,
            "audioLevels": snapshot.audio_levels,
        }))
        .map_err(|_| WorkerError::reconnect("Call snapshot fingerprint failed"))?;
        let unchanged = self.last_snapshot_fingerprint.as_deref() == Some(fingerprint.as_str());
        let refresh_due = self
            .last_snapshot_sent
            .is_none_or(|sent| sent.elapsed() >= SNAPSHOT_REFRESH);
        if unchanged && !refresh_due {
            return Ok(None);
        }
        let frame = PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            event_id: format!("snapshot_{}", uuid::Uuid::new_v4().simple()),
            snapshot,
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Plugin snapshot frame is invalid"))?;
        self.last_snapshot_fingerprint = Some(fingerprint);
        self.last_snapshot_sent = Some(Instant::now());
        serde_json::to_string(&frame)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("Plugin snapshot could not be encoded"))
    }

    fn assistance_frame(&mut self, radio: &RadioHandle) -> Result<Option<String>, WorkerError> {
        let Some(frame) = crate::assistance::global().pending_frame(&self.app_id) else {
            self.last_assistance_request_sent = None;
            return Ok(None);
        };
        if self.last_assistance_request_sent.as_deref() == Some(frame.request_id.as_str()) {
            return Ok(None);
        }
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media endpoint is unavailable"))?
            .snapshot();
        if !remote.consent.enabled
            || !remote.consent.acknowledged
            || !remote.consent.assistance_enabled
        {
            return Err(WorkerError::reconnect(
                "current remote consent does not permit assistance",
            ));
        }
        if remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.call_epoch
            || remote.owner_epoch != frame.owner_epoch
            || radio.switchboard_revision() != frame.switchboard_revision
            || remote.remote_revision != frame.remote_revision
        {
            return Err(WorkerError::reconnect(
                "assistance request no longer matches physical call revisions",
            ));
        }
        frame
            .validate(unix_now()?)
            .map_err(|_| WorkerError::reconnect("assistance request is invalid"))?;
        self.last_assistance_request_sent = Some(frame.request_id.clone());
        serde_json::to_string(&frame)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("assistance request could not be encoded"))
    }
}

fn map_service_mode(mode: LocalServiceMode) -> ProtocolServiceMode {
    match mode {
        LocalServiceMode::AokieActive => ProtocolServiceMode::AokieActive,
        LocalServiceMode::SoftHold => ProtocolServiceMode::SoftHold,
        LocalServiceMode::ConsultPending => ProtocolServiceMode::ConsultPending,
        LocalServiceMode::ConsultActive => ProtocolServiceMode::ConsultActive,
        LocalServiceMode::HumanPending => ProtocolServiceMode::HumanPending,
        LocalServiceMode::HumanActive => ProtocolServiceMode::HumanActive,
        LocalServiceMode::ReturningToAokie => ProtocolServiceMode::ReturningToAokie,
        LocalServiceMode::Recovering => ProtocolServiceMode::Recovering,
    }
}

fn authoritative_media_state(
    remote: &crate::remote_media::RemoteMediaSnapshot,
    physical_call_active: bool,
) -> MediaState {
    match remote.service_mode {
        LocalServiceMode::ConsultActive => MediaState::Active,
        LocalServiceMode::HumanActive if remote.talk_audio_forwarded => MediaState::Active,
        LocalServiceMode::HumanActive => MediaState::Connecting,
        LocalServiceMode::SoftHold
        | LocalServiceMode::ConsultPending
        | LocalServiceMode::HumanPending
        | LocalServiceMode::ReturningToAokie
        | LocalServiceMode::Recovering => MediaState::Connecting,
        _ if remote.peer_count > 0 => MediaState::Receiving,
        _ if physical_call_active => MediaState::Ready,
        _ => MediaState::None,
    }
}

fn remote_media_event_kind(kind: &RemoteMediaEventKind) -> &'static str {
    match kind {
        RemoteMediaEventKind::SdpAnswer { .. } => "sdp_answer",
        RemoteMediaEventKind::LocalIce { .. } => "local_ice",
        RemoteMediaEventKind::IceComplete => "ice_complete",
        RemoteMediaEventKind::ConnectionState { .. } => "connection_state",
        RemoteMediaEventKind::RemoteAudioReady => "remote_audio_ready",
        RemoteMediaEventKind::ProtocolViolation { .. } => "protocol_violation",
        RemoteMediaEventKind::TakeoverPending => "takeover_pending",
        RemoteMediaEventKind::TakeoverPrepared { .. } => "takeover_prepared",
        RemoteMediaEventKind::ConsultPrepared { .. } => "consult_prepared",
        RemoteMediaEventKind::ConsultActive => "consult_active",
        RemoteMediaEventKind::HumanActive => "human_active",
        RemoteMediaEventKind::ReturningToAokie { .. } => "returning_to_aokie",
        RemoteMediaEventKind::AokieActive => "aokie_active",
        RemoteMediaEventKind::Closed { .. } => "closed",
        RemoteMediaEventKind::Error { .. } => "error",
    }
}

fn mask_number(number: &str) -> Option<String> {
    let digits = number
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        let suffix = digits
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        Some(format!("***{suffix}"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    kind: String,
    schema_version: u16,
    #[serde(default)]
    app_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseNotice {
    kind: String,
    schema_version: u16,
    app_id: String,
    #[serde(default)]
    request_id: Option<String>,
    device_id: String,
    lease_token: String,
    lease: LeaseClaims,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseRevokedNotice {
    kind: String,
    schema_version: u16,
    app_id: String,
    device_id: String,
    lease_id: String,
    lease_jti: String,
    call_id: String,
    call_epoch: u64,
    fence: u64,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ErrorNotice {
    kind: String,
    schema_version: u16,
    code: String,
    message: String,
    #[serde(default)]
    request_id: Option<String>,
}

impl GatewaySession {
    fn handle_inbound(
        &mut self,
        encoded: &str,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let envelope: Envelope = serde_json::from_str(encoded)
            .map_err(|_| WorkerError::reconnect("Companion gateway frame is malformed"))?;
        if envelope.schema_version != SCHEMA_VERSION {
            return Err(WorkerError::rebootstrap(
                "Companion gateway schemaVersion is unsupported",
            ));
        }
        if let Some(app_id) = envelope.app_id.as_deref() {
            if app_id != self.app_id {
                return Err(WorkerError::rebootstrap(
                    "Companion gateway frame crossed application identity",
                ));
            }
        }
        match envelope.kind.as_str() {
            "claim_proposal" => {
                let notice: LeaseNotice = parse_gateway_frame(encoded)?;
                self.handle_claim_proposal(notice, media, radio)
            }
            "lease_granted" => {
                let notice: LeaseNotice = parse_gateway_frame(encoded)?;
                self.handle_lease_granted(notice, radio)?;
                Ok(Vec::new())
            }
            "lease_renewed" => {
                let notice: LeaseNotice = parse_gateway_frame(encoded)?;
                self.handle_lease_renewed(notice, media, radio)?;
                Ok(Vec::new())
            }
            "lease_revoked" => {
                let notice: LeaseRevokedNotice = parse_gateway_frame(encoded)?;
                self.handle_lease_revoked(notice, media)?;
                Ok(Vec::new())
            }
            "rtc_signal" => {
                let frame: PluginRtcSignalFrame = parse_gateway_frame(encoded)?;
                frame.validate_routed_mobile().map_err(|_| {
                    WorkerError::reconnect("Companion RTC signal failed contract validation")
                })?;
                self.handle_rtc_signal(frame, media, radio)?;
                Ok(Vec::new())
            }
            "assistance_answer" => {
                let frame: PluginAssistanceAnswerFrame = parse_gateway_frame(encoded)?;
                frame.validate().map_err(|_| {
                    WorkerError::reconnect("Companion assistance answer is invalid")
                })?;
                if frame.app_id != self.app_id {
                    return Err(WorkerError::rebootstrap(
                        "Companion assistance answer crossed application identity",
                    ));
                }
                let remote = radio
                    .remote_media()
                    .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
                    .snapshot();
                if !remote.consent.enabled
                    || !remote.consent.acknowledged
                    || !remote.consent.assistance_enabled
                    || remote.call_id.as_deref() != Some(frame.call_id.as_str())
                    || remote.call_epoch != frame.call_epoch
                    || remote.owner_epoch != frame.owner_epoch
                    || remote.remote_revision != frame.remote_revision
                    || radio.switchboard_revision() != frame.switchboard_revision
                {
                    return Err(WorkerError::reconnect(
                        "Companion assistance answer failed consent or call fencing",
                    ));
                }
                crate::assistance::global().accept(frame).map_err(|_| {
                    WorkerError::reconnect("Companion assistance answer was refused")
                })?;
                Ok(Vec::new())
            }
            "end_caller_execute" => {
                let frame: PluginEndCallerExecuteFrame = parse_gateway_frame(encoded)?;
                frame.validate().map_err(|_| {
                    WorkerError::reconnect("Companion caller-ending command is invalid")
                })?;
                self.handle_end_caller_execute(frame, radio)
            }
            "error" => {
                let notice: ErrorNotice = parse_gateway_frame(encoded)?;
                let _ = (
                    &notice.kind,
                    notice.schema_version,
                    &notice.message,
                    &notice.request_id,
                );
                Err(WorkerError::reconnect(format!(
                    "Companion gateway reported {}",
                    sanitize_gateway_code(&notice.code)
                )))
            }
            _ => Err(WorkerError::reconnect(
                "Companion gateway sent an unsupported frame",
            )),
        }
    }

    fn handle_end_caller_execute(
        &mut self,
        frame: PluginEndCallerExecuteFrame,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        if frame.app_id != self.app_id {
            return Err(WorkerError::rebootstrap(
                "Companion caller-ending command crossed application identity",
            ));
        }
        if self.pending_end_caller.contains_key(&frame.operation_id) {
            // Gateway idempotency may replay an accepted response to the
            // mobile, but must not normally relay twice. A byte-identical
            // duplicate is harmless and never queues another radio command.
            return Ok(Vec::new());
        }
        if !self.pending_end_caller.is_empty() {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "end_caller_pending",
                "another caller-ending command is still pending",
            )?]);
        }
        let Some(claims) = self.leases.get(&frame.lease_jti) else {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "stale_lease",
                "the exact takeover lease is no longer present",
            )?]);
        };
        if claims.app_id != frame.app_id
            || claims.plugin_id != self.plugin_id
            || claims.device_id != frame.device_id
            || claims.call_id != frame.call_id
            || claims.call_epoch != frame.call_epoch
            || claims.owner_epoch != frame.owner_epoch
            || claims.lease_id != frame.lease_id
            || claims.jti != frame.lease_jti
            || claims.fence != frame.fence
            || claims.mode != LeaseMode::Takeover
            || claims.phase != LeasePhase::Active
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "lease_fence_mismatch",
                "the command does not match the exact active takeover lease",
            )?]);
        }
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
            .snapshot();
        if !radio.is_call_active()
            || radio.current_call_id().as_deref() != Some(frame.call_id.as_str())
            || radio.switchboard_revision() != frame.switchboard_revision
            || remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.call_epoch
            || remote.owner_epoch != frame.owner_epoch
            || remote.remote_revision != frame.remote_revision
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "physical_fence_stale",
                "the physical call epochs or revisions changed",
            )?]);
        }
        if remote.service_mode != LocalServiceMode::HumanActive
            || remote.talk_device_id.as_deref() != Some(frame.device_id.as_str())
            || remote.talk_lease_id.as_deref() != Some(frame.lease_id.as_str())
            || remote.talk_fence != frame.fence
            || !remote.radio_reserved
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "not_active_takeover_owner",
                "the selected Companion is not the physical caller owner",
            )?]);
        }
        if !remote.consent.enabled
            || !remote.consent.acknowledged
            || !remote.consent.takeover_enabled
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "consent_required",
                "current remote consent no longer permits takeover control",
            )?]);
        }

        let request = CompanionEndCallerRequest {
            call_id: frame.call_id.clone(),
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            device_id: frame.device_id.clone(),
            lease_id: frame.lease_id.clone(),
            fence: frame.fence,
        };
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        if radio
            .send(RadioControl::EndCallerFromCompanion {
                request,
                reply: result_tx,
            })
            .is_err()
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "radio_unavailable",
                "the physical radio command queue is unavailable",
            )?]);
        }
        self.pending_end_caller.insert(
            frame.operation_id.clone(),
            PendingEndCaller {
                execute: frame,
                result_rx,
            },
        );
        Ok(Vec::new())
    }

    fn drain_end_caller_results(&mut self) -> Result<Vec<String>, WorkerError> {
        use std::sync::mpsc::TryRecvError;
        let mut finished = Vec::new();
        for (operation_id, pending) in &self.pending_end_caller {
            match pending.result_rx.try_recv() {
                Ok(result) => finished.push((operation_id.clone(), result)),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => finished.push((
                    operation_id.clone(),
                    Err(CompanionEndCallerFailure {
                        code: "radio_result_lost",
                        message: "the physical radio ended without a command result".into(),
                    }),
                )),
            }
        }
        let mut encoded = Vec::with_capacity(finished.len());
        for (operation_id, result) in finished {
            let Some(pending) = self.pending_end_caller.remove(&operation_id) else {
                continue;
            };
            let frame = match result {
                Ok(()) => end_caller_result(&pending.execute, EndCallerOutcome::Completed, None),
                Err(error) => end_caller_result(
                    &pending.execute,
                    EndCallerOutcome::Failed,
                    Some((error.code, error.message.as_str())),
                ),
            };
            frame
                .validate()
                .map_err(|_| WorkerError::reconnect("Companion caller-ending result is invalid"))?;
            encoded.push(serde_json::to_string(&frame).map_err(|_| {
                WorkerError::reconnect("Companion caller-ending result could not be encoded")
            })?);
        }
        Ok(encoded)
    }

    fn handle_claim_proposal(
        &mut self,
        notice: LeaseNotice,
        _media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        self.validate_notice(&notice, radio)?;
        let request_id = notice
            .request_id
            .clone()
            .ok_or_else(|| WorkerError::reconnect("Companion claim proposal omitted requestId"))?;
        match notice.lease.mode {
            LeaseMode::Consult if notice.lease.phase == LeasePhase::Prepared => {
                let remote = radio
                    .remote_media()
                    .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
                    .snapshot();
                let assistance = crate::assistance::global()
                    .pending_frame(&self.app_id)
                    .ok_or_else(|| {
                        WorkerError::reconnect(
                            "Private consultation requires a current Aokie assistance request",
                        )
                    })?;
                if !remote.consent.consult_enabled
                    || assistance.call_id != notice.lease.call_id
                    || assistance.call_epoch != notice.lease.call_epoch
                    || assistance.owner_epoch != notice.lease.owner_epoch
                    || assistance.switchboard_revision != radio.switchboard_revision()
                    || assistance.remote_revision != remote.remote_revision
                {
                    return Err(WorkerError::reconnect(
                        "Private consultation does not match the active assistance fence",
                    ));
                }
            }
            LeaseMode::Takeover if notice.lease.phase == LeasePhase::Prepared => {}
            _ => {
                return Err(WorkerError::reconnect(
                    "Companion claim proposal has an invalid lease phase",
                ))
            }
        }
        if self.prepared.is_some() {
            return Err(WorkerError::reconnect(
                "Companion gateway proposed a second consult/takeover claimant",
            ));
        }
        self.leases
            .insert(notice.lease.jti.clone(), notice.lease.clone());
        self.prepared = Some(PreparedTakeover {
            request_id,
            provisional: notice.lease,
            confirmed_owner_epoch: None,
            provisional_sdp_revision: 0,
            provisional_transport_generation: 0,
            decision_sent: false,
        });
        if let Some(prepared) = self.prepared.as_ref() {
            eprintln!(
                "[aokie-plugin][takeover] stage=claim_proposed app={} device={} call={} mode={:?} fence={} lease_jti={} rtc={}",
                self.app_id,
                prepared.provisional.device_id,
                prepared.provisional.call_id,
                prepared.provisional.mode,
                prepared.provisional.fence,
                prepared.provisional.jti,
                prepared.provisional.rtc_session_id
            );
        }
        Ok(Vec::new())
    }

    fn handle_lease_granted(
        &mut self,
        notice: LeaseNotice,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        self.validate_notice(&notice, radio)?;
        if notice.lease.mode != LeaseMode::Monitor || notice.lease.phase != LeasePhase::Active {
            return Err(WorkerError::reconnect(
                "Only active monitor leases may be granted directly",
            ));
        }
        self.leases.insert(notice.lease.jti.clone(), notice.lease);
        Ok(())
    }

    fn handle_lease_renewed(
        &mut self,
        notice: LeaseNotice,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        self.validate_notice(&notice, radio)?;
        let stable_id = notice.lease.lease_id.clone();
        self.leases.retain(|_, claims| claims.lease_id != stable_id);
        self.leases
            .insert(notice.lease.jti.clone(), notice.lease.clone());
        if let Some(route) = self
            .peers
            .values_mut()
            .find(|route| route.binding.lease_id.as_deref() == Some(stable_id.as_str()))
        {
            route.lease_jti = notice.lease.jti.clone();
            route.lease_ttl_ms = lease_ttl_ms(&notice.lease)?;
            media
                .renew_lease(route.binding.clone(), route.lease_ttl_ms)
                .map_err(|_| WorkerError::reconnect("Companion media lease renewal failed"))?;
        } else if let Some(prepared) = self
            .prepared
            .as_ref()
            .filter(|prepared| prepared.provisional.lease_id == stable_id)
        {
            let binding = binding_for_claims(&notice.lease);
            let _ = prepared;
            media
                .renew_lease(binding, lease_ttl_ms(&notice.lease)?)
                .map_err(|_| WorkerError::reconnect("Prepared media lease renewal failed"))?;
        }
        Ok(())
    }

    fn validate_notice(
        &self,
        notice: &LeaseNotice,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        let _ = (&notice.kind, notice.schema_version);
        if notice.app_id != self.app_id
            || notice.device_id != notice.lease.device_id
            || notice.lease.app_id != self.app_id
            || notice.lease.plugin_id != self.plugin_id
            || notice.lease.plugin_key_thumbprint != self.endpoint_authority.endpoint_key.thumbprint
            || !self
                .endpoint_authority
                .approved_mobile_keys
                .contains_key(&notice.lease.mobile_key_thumbprint)
            || notice.lease_token.is_empty()
            || notice.lease_token.len() > MAX_LEASE_TOKEN_BYTES
        {
            return Err(WorkerError::rebootstrap(
                "Companion lease notice identity is invalid",
            ));
        }
        let now = unix_now()?;
        notice
            .lease
            .validate(now)
            .map_err(|_| WorkerError::reconnect("Companion lease claims are invalid"))?;
        if notice.lease.expires_at > now.saturating_add(300) {
            return Err(WorkerError::reconnect(
                "Companion lease lifetime exceeds the local safety limit",
            ));
        }
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
            .snapshot();
        if remote.call_id.as_deref() != Some(notice.lease.call_id.as_str())
            || remote.call_epoch != notice.lease.call_epoch
            || remote.owner_epoch != notice.lease.owner_epoch
        {
            return Err(WorkerError::reconnect(
                "Companion lease does not match physical call epochs",
            ));
        }
        Ok(())
    }
}

fn end_caller_result(
    execute: &PluginEndCallerExecuteFrame,
    outcome: EndCallerOutcome,
    failure: Option<(&str, &str)>,
) -> PluginEndCallerResultFrame {
    PluginEndCallerResultFrame {
        kind: "end_caller_result".into(),
        schema_version: SCHEMA_VERSION,
        app_id: execute.app_id.clone(),
        operation_id: execute.operation_id.clone(),
        confirmation_id: execute.confirmation_id.clone(),
        device_id: execute.device_id.clone(),
        call_id: execute.call_id.clone(),
        call_epoch: execute.call_epoch,
        owner_epoch: execute.owner_epoch,
        switchboard_revision: execute.switchboard_revision,
        remote_revision: execute.remote_revision,
        lease_id: execute.lease_id.clone(),
        fence: execute.fence,
        outcome,
        code: failure.map(|(code, _)| code.to_owned()),
        message: failure.map(|(_, message)| message.to_owned()),
    }
}

fn encode_end_caller_failure(
    execute: &PluginEndCallerExecuteFrame,
    code: &str,
    message: &str,
) -> Result<String, WorkerError> {
    let frame = end_caller_result(execute, EndCallerOutcome::Failed, Some((code, message)));
    frame
        .validate()
        .map_err(|_| WorkerError::reconnect("Companion caller-ending failure is invalid"))?;
    serde_json::to_string(&frame)
        .map_err(|_| WorkerError::reconnect("Companion caller-ending failure could not be encoded"))
}

fn parse_gateway_frame<T>(encoded: &str) -> Result<T, WorkerError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(encoded)
        .map_err(|_| WorkerError::reconnect("Companion gateway frame has an invalid shape"))
}

fn sanitize_gateway_code(code: &str) -> String {
    let safe = code
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .take(80)
        .collect::<String>();
    if safe.is_empty() {
        "an error".into()
    } else {
        safe
    }
}

fn lease_ttl_ms(claims: &LeaseClaims) -> Result<u64, WorkerError> {
    let remaining = claims.expires_at.saturating_sub(unix_now()?);
    if remaining == 0 || remaining > 300 {
        return Err(WorkerError::expired("Companion media lease is expired"));
    }
    Ok(remaining.saturating_mul(1_000))
}

fn binding_for_claims(claims: &LeaseClaims) -> SessionBinding {
    let mode = match (claims.mode, claims.phase) {
        (LeaseMode::Monitor, _) => MediaMode::Monitor,
        (LeaseMode::Consult, LeasePhase::Prepared) => MediaMode::PreparedConsult,
        (LeaseMode::Consult, LeasePhase::Active) => MediaMode::Consult,
        (LeaseMode::Takeover, LeasePhase::Prepared) => MediaMode::PreparedTalk,
        (LeaseMode::Takeover, LeasePhase::Active) => MediaMode::Talk,
    };
    SessionBinding {
        rtc_session_id: claims.rtc_session_id.clone(),
        call_id: claims.call_id.clone(),
        call_epoch: claims.call_epoch,
        owner_epoch: claims.owner_epoch,
        device_id: claims.device_id.clone(),
        mode,
        lease_id: Some(claims.lease_id.clone()),
        fence: claims.fence,
    }
}

impl GatewaySession {
    fn handle_rtc_signal(
        &mut self,
        frame: PluginRtcSignalFrame,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        if frame.app_id != self.app_id || frame.plugin_id != self.plugin_id {
            return Err(WorkerError::rebootstrap(
                "Companion RTC signal crossed application identity",
            ));
        }
        let now = unix_now()?;
        let authentication = frame
            .signal
            .verify_endpoint_authentication(now)
            .map_err(|_| WorkerError::reconnect("Companion RTC endpoint signature is invalid"))?;
        if let Some(authentication) = authentication {
            let expected = self
                .leases
                .get(&frame.lease_jti)
                .or_else(|| self.prepared.as_ref().map(|prepared| &prepared.provisional))
                .ok_or_else(|| {
                    WorkerError::reconnect("Companion RTC signature has no exact lease context")
                })?;
            let supplied_key = match &frame.signal {
                RtcSignal::Offer { binding, .. } | RtcSignal::Answer { binding, .. } => {
                    &binding.endpoint_key
                }
                RtcSignal::Ice { envelope, .. } | RtcSignal::IceComplete { envelope } => {
                    &envelope.endpoint_key
                }
                RtcSignal::Close { .. } => unreachable!("authenticated signal is not close"),
            };
            let approved_key = self
                .endpoint_authority
                .approved_mobile_keys
                .get(authentication.holder_key_thumbprint())
                .ok_or_else(|| {
                    WorkerError::rebootstrap(
                        "Companion RTC signer is absent from the local owner-approved roster",
                    )
                })?;
            if supplied_key != approved_key
                || authentication.endpoint_role() != AdmissionRole::Mobile
                || authentication.endpoint_session_nonce() != expected.session_nonce
                || authentication.holder_key_thumbprint() != expected.mobile_key_thumbprint
                || authentication.peer_key_thumbprint() != expected.plugin_key_thumbprint
            {
                return Err(WorkerError::rebootstrap(
                    "Companion RTC signer does not match the explicit local roster and lease",
                ));
            }
            self.used_endpoint_jtis
                .retain(|_, expires_at| *expires_at > now);
            if self.used_endpoint_jtis.contains_key(authentication.jti()) {
                return Err(WorkerError::reconnect(
                    "Companion RTC endpoint signature was replayed",
                ));
            }
            if self.used_endpoint_jtis.len() >= MAX_USED_ENDPOINT_JTIS {
                return Err(WorkerError::reconnect(
                    "Companion RTC endpoint replay cache is exhausted",
                ));
            }
            self.used_endpoint_jtis
                .insert(authentication.jti().to_owned(), authentication.expires_at());
        }
        match frame.signal.clone() {
            RtcSignal::Offer { sdp, .. } => self.open_offer(frame, sdp, media, radio),
            RtcSignal::Ice {
                candidate,
                sdp_mid,
                sdp_m_line_index,
                ..
            } => {
                let route = self.route_for_frame(&frame)?;
                if route.sdp_revision != frame.sdp_revision
                    || route.transport_generation != frame.transport_generation
                {
                    return Err(WorkerError::reconnect(
                        "Companion ICE signal has a stale SDP generation",
                    ));
                }
                let sdp_mid = sdp_mid
                    .ok_or_else(|| WorkerError::reconnect("Companion ICE signal omitted sdpMid"))?;
                let index = sdp_m_line_index.ok_or_else(|| {
                    WorkerError::reconnect("Companion ICE signal omitted sdpMLineIndex")
                })?;
                media
                    .add_remote_ice(
                        &frame.rtc_session_id,
                        IceCandidateSignal {
                            sdp_mid,
                            sdp_mline_index: i32::from(index),
                            candidate,
                        },
                    )
                    .map_err(|error| {
                        eprintln!(
                            "[aokie-plugin][takeover] stage=remote_ice_rejected call={} owner_epoch={} fence={} rtc={} detail={}",
                            frame.call_id,
                            frame.owner_epoch,
                            frame.fence,
                            frame.rtc_session_id,
                            sanitize_status_message(&error)
                        );
                        WorkerError::reconnect(format!(
                            "Remote ICE candidate was rejected: {}",
                            sanitize_status_message(&error)
                        ))
                    })
            }
            RtcSignal::IceComplete { .. } => {
                let _ = self.route_for_frame(&frame)?;
                Ok(())
            }
            RtcSignal::Close { reason } => {
                let route = self.route_for_frame(&frame)?.binding.clone();
                if matches!(
                    route.mode,
                    MediaMode::PreparedConsult
                        | MediaMode::Consult
                        | MediaMode::PreparedTalk
                        | MediaMode::Talk
                ) {
                    let _ = media.revoke(&route, &reason);
                }
                let _ = media.close_peer(&frame.rtc_session_id, &reason);
                self.peers.remove(&frame.rtc_session_id);
                Ok(())
            }
            RtcSignal::Answer { .. } => Err(WorkerError::reconnect(
                "Companion endpoint sent an answer in the offer direction",
            )),
        }
    }

    fn open_offer(
        &mut self,
        frame: PluginRtcSignalFrame,
        sdp: String,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        let now = unix_now()?;
        let claims = if let Some(claims) = self.leases.get(&frame.lease_jti).cloned() {
            claims
        } else {
            let prepared = self.prepared.as_ref().ok_or_else(|| {
                WorkerError::reconnect("Active consult/takeover offer has no prepared claim")
            })?;
            let confirmed = prepared.confirmed_owner_epoch.ok_or_else(|| {
                WorkerError::reconnect("Active offer arrived before physical preparation")
            })?;
            let old = &prepared.provisional;
            if frame.device_id != old.device_id
                || frame.rtc_session_id != old.rtc_session_id
                || frame.call_id != old.call_id
                || frame.call_epoch != old.call_epoch
                || frame.owner_epoch != confirmed
                || frame.fence != old.fence
                || frame.sdp_revision <= prepared.provisional_sdp_revision
                || frame.transport_generation <= prepared.provisional_transport_generation
            {
                return Err(WorkerError::reconnect(
                    "Active offer does not advance the prepared binding",
                ));
            }
            let mut active = old.clone();
            active.owner_epoch = confirmed;
            active.phase = LeasePhase::Active;
            active.tracks = aokie_protocol::v2::tracks_for(active.mode, LeasePhase::Active);
            active.jti = frame.lease_jti.clone();
            active.expires_at = now.saturating_add(ACTIVE_LEASE_FALLBACK_TTL);
            active
        };
        if matches!(claims.mode, LeaseMode::Consult | LeaseMode::Takeover)
            && claims.phase == LeasePhase::Active
        {
            let prepared = self.prepared.as_ref().ok_or_else(|| {
                WorkerError::reconnect("Active consult/takeover offer has no prepared claim")
            })?;
            if prepared.confirmed_owner_epoch != Some(claims.owner_epoch)
                || frame.sdp_revision <= prepared.provisional_sdp_revision
                || frame.transport_generation <= prepared.provisional_transport_generation
            {
                return Err(WorkerError::reconnect(
                    "Active offer did not advance SDP and transport generations",
                ));
            }
        }
        if claims.jti != frame.lease_jti
            || claims.device_id != frame.device_id
            || claims.rtc_session_id != frame.rtc_session_id
            || claims.call_id != frame.call_id
            || claims.call_epoch != frame.call_epoch
            || claims.owner_epoch != frame.owner_epoch
            || claims.fence != frame.fence
        {
            return Err(WorkerError::reconnect(
                "Companion RTC offer does not match its lease binding",
            ));
        }
        claims
            .validate(now)
            .map_err(|_| WorkerError::reconnect("Companion RTC lease is invalid"))?;
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
            .snapshot();
        if remote.call_id.as_deref() != Some(claims.call_id.as_str())
            || remote.call_epoch != claims.call_epoch
            || remote.owner_epoch != claims.owner_epoch
        {
            return Err(WorkerError::reconnect(
                "Companion RTC offer is stale against physical call truth",
            ));
        }
        if let Some(existing) = self.peers.get(&frame.rtc_session_id) {
            let replacing_prepared = matches!(
                (existing.binding.mode, claims.mode, claims.phase),
                (
                    MediaMode::PreparedTalk,
                    LeaseMode::Takeover,
                    LeasePhase::Active
                ) | (
                    MediaMode::PreparedConsult,
                    LeaseMode::Consult,
                    LeasePhase::Active
                )
            );
            if !replacing_prepared {
                return Err(WorkerError::reconnect(
                    "Companion RTC session already has a native peer",
                ));
            }
            let _ = media.close_peer(&frame.rtc_session_id, "active_rebind");
            self.peers.remove(&frame.rtc_session_id);
        }
        let binding = binding_for_claims(&claims);
        let ttl = lease_ttl_ms(&claims)?;
        let offer = SdpSignal {
            kind: SdpSignalType::Offer,
            sdp,
        };
        offer
            .validate()
            .map_err(|_| WorkerError::reconnect("Companion SDP offer is invalid"))?;
        media
            .open_peer(OpenPeerRequest {
                binding: binding.clone(),
                offer,
                lease_ttl_ms: ttl,
                ice_servers: self.ice_servers.clone(),
                relay_only: self.relay_only,
            })
            .map_err(|error| {
                eprintln!(
                    "[aokie-plugin][takeover] stage=peer_open_failed call={} mode={:?} owner_epoch={} fence={} rtc={} detail={}",
                    binding.call_id,
                    binding.mode,
                    binding.owner_epoch,
                    binding.fence,
                    binding.rtc_session_id,
                    sanitize_status_message(&error)
                );
                WorkerError::reconnect(format!(
                    "Native Companion peer could not open: {}",
                    sanitize_status_message(&error)
                ))
            })?;
        eprintln!(
            "[aokie-plugin][takeover] stage=peer_opened call={} mode={:?} owner_epoch={} fence={} sdp={} generation={} rtc={}",
            binding.call_id,
            binding.mode,
            binding.owner_epoch,
            binding.fence,
            frame.sdp_revision,
            frame.transport_generation,
            binding.rtc_session_id
        );
        if matches!(
            binding.mode,
            MediaMode::PreparedTalk | MediaMode::PreparedConsult
        ) {
            let prepared = self
                .prepared
                .as_mut()
                .ok_or_else(|| WorkerError::reconnect("Prepared takeover context disappeared"))?;
            prepared.provisional_sdp_revision = frame.sdp_revision;
            prepared.provisional_transport_generation = frame.transport_generation;
        }
        self.leases.insert(claims.jti.clone(), claims);
        self.peers.insert(
            frame.rtc_session_id,
            PeerRoute {
                binding,
                lease_jti: frame.lease_jti,
                device_id: frame.device_id,
                sdp_revision: frame.sdp_revision,
                transport_generation: frame.transport_generation,
                lease_ttl_ms: ttl,
                connected: false,
                remote_audio_ready: false,
                transition_requested: false,
            },
        );
        Ok(())
    }

    fn route_for_frame(&self, frame: &PluginRtcSignalFrame) -> Result<&PeerRoute, WorkerError> {
        let route = self.peers.get(&frame.rtc_session_id).ok_or_else(|| {
            WorkerError::reconnect("Companion RTC signal names an unknown native peer")
        })?;
        if route.lease_jti != frame.lease_jti
            || route.device_id != frame.device_id
            || route.binding.call_id != frame.call_id
            || route.binding.call_epoch != frame.call_epoch
            || route.binding.owner_epoch != frame.owner_epoch
            || route.binding.fence != frame.fence
        {
            return Err(WorkerError::reconnect(
                "Companion RTC signal does not match the immutable peer binding",
            ));
        }
        Ok(route)
    }

    fn handle_lease_revoked(
        &mut self,
        notice: LeaseRevokedNotice,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        let _ = (&notice.kind, notice.schema_version);
        if notice.app_id != self.app_id {
            return Err(WorkerError::rebootstrap(
                "Companion revocation crossed application identity",
            ));
        }
        let route_id = self.peers.iter().find_map(|(id, route)| {
            (route.lease_jti == notice.lease_jti
                && route.device_id == notice.device_id
                && route.binding.lease_id.as_deref() == Some(notice.lease_id.as_str())
                && route.binding.call_id == notice.call_id
                && route.binding.call_epoch == notice.call_epoch
                && route.binding.fence == notice.fence)
                .then(|| id.clone())
        });
        if let Some(route_id) = route_id {
            if let Some(route) = self.peers.remove(&route_id) {
                if matches!(
                    route.binding.mode,
                    MediaMode::PreparedConsult
                        | MediaMode::Consult
                        | MediaMode::PreparedTalk
                        | MediaMode::Talk
                ) {
                    let _ = media.revoke(&route.binding, &notice.reason);
                }
                let _ = media.close_peer(&route_id, &notice.reason);
            }
        } else if let Some(prepared) = self.prepared.as_ref().filter(|prepared| {
            prepared.provisional.lease_id == notice.lease_id
                && prepared.provisional.device_id == notice.device_id
                && prepared.provisional.call_id == notice.call_id
                && prepared.provisional.call_epoch == notice.call_epoch
                && prepared.provisional.fence == notice.fence
        }) {
            let mut binding = binding_for_claims(&prepared.provisional);
            if let Some(owner_epoch) = prepared.confirmed_owner_epoch {
                binding.owner_epoch = owner_epoch;
                binding.mode = match prepared.provisional.mode {
                    LeaseMode::Consult => MediaMode::Consult,
                    LeaseMode::Takeover => MediaMode::Talk,
                    LeaseMode::Monitor => binding.mode,
                };
            }
            let _ = media.revoke(&binding, &notice.reason);
        }
        self.leases.remove(&notice.lease_jti);
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.provisional.lease_id == notice.lease_id)
        {
            self.prepared = None;
        }
        Ok(())
    }

    fn drain_media_events(
        &mut self,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let mut outbound = Vec::new();
        for event in media.drain_events(64) {
            eprintln!(
                "[aokie-plugin][takeover] stage=media_event call={} owner_epoch={} rtc={} event={}",
                event.call_id,
                event.owner_epoch,
                event.rtc_session_id,
                remote_media_event_kind(&event.kind)
            );
            match event.kind.clone() {
                RemoteMediaEventKind::TakeoverPrepared {
                    confirmed_owner_epoch,
                } => {
                    if let Some(encoded) =
                        self.complete_preparation(&event, confirmed_owner_epoch, media, radio)?
                    {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::ConsultPrepared {
                    confirmed_owner_epoch,
                } => {
                    if let Some(encoded) =
                        self.complete_preparation(&event, confirmed_owner_epoch, media, radio)?
                    {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::SdpAnswer { answer } => {
                    if answer.kind != SdpSignalType::Answer {
                        return Err(WorkerError::reconnect(
                            "Native media produced a non-answer SDP response",
                        ));
                    }
                    if let Some(route) = self.route_for_event(&event) {
                        outbound
                            .push(self.encode_rtc(route, OutboundRtcSignal::Answer(answer.sdp))?);
                    }
                }
                RemoteMediaEventKind::LocalIce { candidate } => {
                    if let Some(route) = self.route_for_event(&event) {
                        let index = u16::try_from(candidate.sdp_mline_index).map_err(|_| {
                            WorkerError::reconnect("Native ICE candidate index is invalid")
                        })?;
                        outbound.push(self.encode_rtc(
                            route,
                            OutboundRtcSignal::Ice {
                                candidate: candidate.candidate,
                                sdp_mid: candidate.sdp_mid,
                                sdp_m_line_index: index,
                            },
                        )?);
                    }
                }
                RemoteMediaEventKind::IceComplete => {
                    if let Some(route) = self.route_for_event(&event) {
                        outbound.push(self.encode_rtc(route, OutboundRtcSignal::IceComplete)?);
                    }
                }
                RemoteMediaEventKind::ConnectionState { state } => {
                    let failed = matches!(state.as_str(), "failed" | "disconnected" | "closed");
                    if let Some(route) = self.route_for_event_mut(&event) {
                        route.connected = state == "connected";
                    }
                    if failed {
                        if let Some(encoded) =
                            self.fail_peer(&event.rtc_session_id, "peer_connection_failed", media)?
                        {
                            outbound.push(encoded);
                        }
                    } else {
                        self.maybe_request_transition(&event.rtc_session_id, media)?;
                    }
                }
                RemoteMediaEventKind::RemoteAudioReady => {
                    if let Some(route) = self.route_for_event_mut(&event) {
                        route.remote_audio_ready = true;
                    }
                    self.maybe_request_transition(&event.rtc_session_id, media)?;
                }
                RemoteMediaEventKind::ProtocolViolation { .. }
                | RemoteMediaEventKind::Error { .. } => {
                    if let Some(encoded) =
                        self.fail_peer(&event.rtc_session_id, "media_protocol_failure", media)?
                    {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::ReturningToAokie { reason }
                | RemoteMediaEventKind::Closed { reason } => {
                    // Gateway-authoritative operator returns and remote Close
                    // signals remove `PeerRoute` before native media emits its
                    // terminal event.  A still-registered exact route therefore
                    // means Desktop/radio failed it locally (for example SCO
                    // vanished during PrepareHuman/EnterHuman).  Revoke that
                    // exact lease so a prepared/active takeover cannot remain
                    // renewable after Desktop has already returned to Aokie.
                    if let Some(encoded) = self.fail_route_if_current(&event, &reason, media)? {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::TakeoverPending
                | RemoteMediaEventKind::ConsultActive
                | RemoteMediaEventKind::HumanActive
                | RemoteMediaEventKind::AokieActive => {}
            }
        }
        Ok(outbound)
    }

    fn fail_route_if_current(
        &mut self,
        event: &RemoteMediaEvent,
        reason: &str,
        media: &RemoteMediaHandle,
    ) -> Result<Option<String>, WorkerError> {
        if self.route_for_event(event).is_none() {
            return Ok(None);
        }
        self.fail_peer(&event.rtc_session_id, reason, media)
    }

    fn route_for_event(&self, event: &RemoteMediaEvent) -> Option<&PeerRoute> {
        self.peers.get(&event.rtc_session_id).filter(|route| {
            route.binding.call_id == event.call_id
                && route.binding.call_epoch == event.call_epoch
                && route.binding.owner_epoch == event.owner_epoch
        })
    }

    fn route_for_event_mut(&mut self, event: &RemoteMediaEvent) -> Option<&mut PeerRoute> {
        self.peers.get_mut(&event.rtc_session_id).filter(|route| {
            route.binding.call_id == event.call_id
                && route.binding.call_epoch == event.call_epoch
                && route.binding.owner_epoch == event.owner_epoch
        })
    }

    fn maybe_request_transition(
        &mut self,
        rtc_session_id: &str,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        let action = self.peers.get(rtc_session_id).and_then(|route| {
            if route.transition_requested || !route.connected {
                return None;
            }
            match route.binding.mode {
                MediaMode::PreparedConsult => {
                    Some((0_u8, route.binding.clone(), route.lease_ttl_ms))
                }
                MediaMode::PreparedTalk => Some((1_u8, route.binding.clone(), route.lease_ttl_ms)),
                MediaMode::Consult if route.remote_audio_ready => {
                    Some((2_u8, route.binding.clone(), route.lease_ttl_ms))
                }
                MediaMode::Talk if route.remote_audio_ready => {
                    Some((3_u8, route.binding.clone(), route.lease_ttl_ms))
                }
                _ => None,
            }
        });
        let Some((action, binding, ttl)) = action else {
            return Ok(());
        };
        let action_name = match action {
            0 => "prepare_consult",
            1 => "prepare_takeover",
            2 => "enter_consult",
            _ => "enter_takeover",
        };
        eprintln!(
            "[aokie-plugin][takeover] stage=transition_requested action={} call={} owner_epoch={} fence={} rtc={}",
            action_name,
            binding.call_id,
            binding.owner_epoch,
            binding.fence,
            binding.rtc_session_id
        );
        let result = match action {
            0 => media.request_consult_hold(binding, ttl),
            1 => media.request_soft_hold(binding, ttl),
            2 => media.request_consult(binding, ttl),
            _ => media.request_takeover(binding, ttl),
        };
        result.map_err(|_| {
            WorkerError::reconnect(match action {
                0 => "Prepared private consultation could not enter software hold",
                1 => "Prepared Companion takeover could not enter soft hold",
                2 => "Active private consultation could not enter its isolated route",
                _ => "Active Companion takeover could not enter the radio route",
            })
        })?;
        if let Some(route) = self.peers.get_mut(rtc_session_id) {
            route.transition_requested = true;
        }
        Ok(())
    }

    fn complete_preparation(
        &mut self,
        event: &RemoteMediaEvent,
        confirmed_owner_epoch: u64,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Option<String>, WorkerError> {
        let prepared = match self.prepared.as_mut() {
            Some(prepared)
                if prepared.provisional.rtc_session_id == event.rtc_session_id
                    && prepared.provisional.call_id == event.call_id
                    && prepared.provisional.call_epoch == event.call_epoch
                    && prepared.provisional.owner_epoch == event.owner_epoch
                    && !prepared.decision_sent =>
            {
                prepared
            }
            _ => return Ok(None),
        };
        let remote = media.snapshot();
        if remote.owner_epoch != confirmed_owner_epoch
            || confirmed_owner_epoch <= prepared.provisional.owner_epoch
        {
            return Err(WorkerError::reconnect(
                "Physical owner epoch did not advance during soft hold",
            ));
        }
        let decision = PluginClaimDecisionFrame {
            kind: "claim_decision".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            request_id: prepared.request_id.clone(),
            device_id: prepared.provisional.device_id.clone(),
            call_id: prepared.provisional.call_id.clone(),
            call_epoch: prepared.provisional.call_epoch,
            fence: prepared.provisional.fence,
            accepted: true,
            media_ready: true,
            confirmed_owner_epoch,
            switchboard_revision: radio.switchboard_revision(),
            remote_revision: remote.remote_revision,
            reason: None,
        };
        decision.validate().map_err(|_| {
            WorkerError::reconnect("Prepared claim decision failed local validation")
        })?;
        prepared.confirmed_owner_epoch = Some(confirmed_owner_epoch);
        prepared.decision_sent = true;
        eprintln!(
            "[aokie-plugin][takeover] stage=preparation_complete app={} device={} call={} mode={:?} owner_epoch={} fence={} rtc={}",
            self.app_id,
            prepared.provisional.device_id,
            prepared.provisional.call_id,
            prepared.provisional.mode,
            confirmed_owner_epoch,
            prepared.provisional.fence,
            prepared.provisional.rtc_session_id
        );

        // DesktopPeer bindings are immutable. Close the receive-only peer;
        // the gateway rotates JTI/ownerEpoch and the Companion must send a
        // fresh higher-generation offer before any microphone track exists.
        let _ = media.close_peer(&event.rtc_session_id, "awaiting_active_rebind");
        self.peers.remove(&event.rtc_session_id);
        serde_json::to_string(&decision)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("Claim decision could not be encoded"))
    }

    fn encode_rtc(
        &self,
        route: &PeerRoute,
        outbound: OutboundRtcSignal,
    ) -> Result<String, WorkerError> {
        let now = unix_now()?;
        let lease = self.leases.get(&route.lease_jti).ok_or_else(|| {
            WorkerError::reconnect("Outbound RTC signal has no exact signed lease")
        })?;
        if lease.plugin_key_thumbprint != self.endpoint_authority.endpoint_key.thumbprint
            || !self
                .endpoint_authority
                .approved_mobile_keys
                .contains_key(&lease.mobile_key_thumbprint)
        {
            return Err(WorkerError::rebootstrap(
                "Outbound RTC lease keys do not match local endpoint policy",
            ));
        }
        let expires_at = lease.expires_at.min(now.saturating_add(30));
        if expires_at <= now {
            return Err(WorkerError::expired("Outbound RTC lease is expired"));
        }
        let signal = match outbound {
            OutboundRtcSignal::Answer(sdp) => {
                let claims = EndpointBindingClaims {
                    app_id: self.app_id.clone(),
                    plugin_id: self.plugin_id.clone(),
                    device_id: route.device_id.clone(),
                    rtc_session_id: route.binding.rtc_session_id.clone(),
                    endpoint_session_nonce: self.plugin_session_nonce.clone(),
                    lease_jti: route.lease_jti.clone(),
                    endpoint_role: AdmissionRole::Plugin,
                    holder_key_thumbprint: lease.plugin_key_thumbprint.clone(),
                    peer_key_thumbprint: lease.mobile_key_thumbprint.clone(),
                    call_id: route.binding.call_id.clone(),
                    call_epoch: route.binding.call_epoch,
                    owner_epoch: route.binding.owner_epoch,
                    fence: route.binding.fence,
                    sdp_revision: route.sdp_revision,
                    transport_generation: route.transport_generation,
                    dtls_fingerprint: sdp_dtls_fingerprint(&sdp).map_err(|_| {
                        WorkerError::reconnect("Native SDP answer omitted a valid DTLS fingerprint")
                    })?,
                    sdp_sha256: sdp_sha256(&sdp),
                    nonce: format!("rtc_nonce_{}", uuid::Uuid::new_v4().simple()),
                    jti: format!("rtc_jti_{}", uuid::Uuid::new_v4().simple()),
                    issued_at: now,
                    expires_at,
                };
                let signature = self
                    .endpoint_authority
                    .sign(&claims.signing_bytes().map_err(|_| {
                        WorkerError::reconnect("Outbound SDP binding could not be canonicalized")
                    })?);
                RtcSignal::Answer {
                    sdp,
                    binding: SignedEndpointBinding {
                        endpoint_key: self.endpoint_authority.endpoint_key.clone(),
                        claims,
                        signature,
                    },
                }
            }
            OutboundRtcSignal::Ice {
                candidate,
                sdp_mid,
                sdp_m_line_index,
            } => {
                let claims = TrickleCandidateClaims {
                    app_id: self.app_id.clone(),
                    plugin_id: self.plugin_id.clone(),
                    device_id: route.device_id.clone(),
                    rtc_session_id: route.binding.rtc_session_id.clone(),
                    endpoint_session_nonce: self.plugin_session_nonce.clone(),
                    lease_jti: route.lease_jti.clone(),
                    endpoint_role: AdmissionRole::Plugin,
                    holder_key_thumbprint: lease.plugin_key_thumbprint.clone(),
                    peer_key_thumbprint: lease.mobile_key_thumbprint.clone(),
                    call_id: route.binding.call_id.clone(),
                    call_epoch: route.binding.call_epoch,
                    owner_epoch: route.binding.owner_epoch,
                    fence: route.binding.fence,
                    sdp_revision: route.sdp_revision,
                    transport_generation: route.transport_generation,
                    candidate: Some(candidate.clone()),
                    sdp_mid: Some(sdp_mid.clone()),
                    sdp_m_line_index: Some(sdp_m_line_index),
                    end_of_candidates: false,
                    nonce: format!("rtc_nonce_{}", uuid::Uuid::new_v4().simple()),
                    jti: format!("rtc_jti_{}", uuid::Uuid::new_v4().simple()),
                    issued_at: now,
                    expires_at,
                };
                let signature = self
                    .endpoint_authority
                    .sign(&claims.signing_bytes().map_err(|_| {
                        WorkerError::reconnect("Outbound ICE envelope could not be canonicalized")
                    })?);
                RtcSignal::Ice {
                    candidate,
                    sdp_mid: Some(sdp_mid),
                    sdp_m_line_index: Some(sdp_m_line_index),
                    envelope: SignedTrickleCandidateEnvelope {
                        endpoint_key: self.endpoint_authority.endpoint_key.clone(),
                        claims,
                        signature,
                    },
                }
            }
            OutboundRtcSignal::IceComplete => {
                let claims = TrickleCandidateClaims {
                    app_id: self.app_id.clone(),
                    plugin_id: self.plugin_id.clone(),
                    device_id: route.device_id.clone(),
                    rtc_session_id: route.binding.rtc_session_id.clone(),
                    endpoint_session_nonce: self.plugin_session_nonce.clone(),
                    lease_jti: route.lease_jti.clone(),
                    endpoint_role: AdmissionRole::Plugin,
                    holder_key_thumbprint: lease.plugin_key_thumbprint.clone(),
                    peer_key_thumbprint: lease.mobile_key_thumbprint.clone(),
                    call_id: route.binding.call_id.clone(),
                    call_epoch: route.binding.call_epoch,
                    owner_epoch: route.binding.owner_epoch,
                    fence: route.binding.fence,
                    sdp_revision: route.sdp_revision,
                    transport_generation: route.transport_generation,
                    candidate: None,
                    sdp_mid: None,
                    sdp_m_line_index: None,
                    end_of_candidates: true,
                    nonce: format!("rtc_nonce_{}", uuid::Uuid::new_v4().simple()),
                    jti: format!("rtc_jti_{}", uuid::Uuid::new_v4().simple()),
                    issued_at: now,
                    expires_at,
                };
                let signature = self
                    .endpoint_authority
                    .sign(&claims.signing_bytes().map_err(|_| {
                        WorkerError::reconnect("Outbound ICE completion could not be canonicalized")
                    })?);
                RtcSignal::IceComplete {
                    envelope: SignedTrickleCandidateEnvelope {
                        endpoint_key: self.endpoint_authority.endpoint_key.clone(),
                        claims,
                        signature,
                    },
                }
            }
        };
        let frame = PluginRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            signal_id: format!("signal_{}", uuid::Uuid::new_v4().simple()),
            plugin_id: self.plugin_id.clone(),
            device_id: route.device_id.clone(),
            lease_jti: route.lease_jti.clone(),
            rtc_session_id: route.binding.rtc_session_id.clone(),
            sdp_revision: route.sdp_revision,
            transport_generation: route.transport_generation,
            call_id: route.binding.call_id.clone(),
            call_epoch: route.binding.call_epoch,
            owner_epoch: route.binding.owner_epoch,
            fence: route.binding.fence,
            signal,
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Outbound RTC signal is invalid"))?;
        serde_json::to_string(&frame)
            .map_err(|_| WorkerError::reconnect("Outbound RTC signal could not be encoded"))
    }

    fn fail_peer(
        &mut self,
        rtc_session_id: &str,
        reason: &str,
        media: &RemoteMediaHandle,
    ) -> Result<Option<String>, WorkerError> {
        let Some(route) = self.peers.remove(rtc_session_id) else {
            return Ok(None);
        };
        eprintln!(
            "[aokie-plugin][takeover] stage=peer_failed call={} mode={:?} owner_epoch={} fence={} rtc={} reason={}",
            route.binding.call_id,
            route.binding.mode,
            route.binding.owner_epoch,
            route.binding.fence,
            rtc_session_id,
            reason
        );
        let lease_id = route.binding.lease_id.clone().ok_or_else(|| {
            WorkerError::reconnect("Failed media route omitted stable lease identity")
        })?;
        if matches!(
            route.binding.mode,
            MediaMode::PreparedConsult
                | MediaMode::Consult
                | MediaMode::PreparedTalk
                | MediaMode::Talk
        ) {
            let _ = media.revoke(&route.binding, reason);
        }
        let _ = media.close_peer(rtc_session_id, reason);

        // Refuse any late active rebind immediately; waiting for the gateway's
        // revocation echo would leave a window in which a failed prepared Talk
        // route could be reopened with otherwise valid, still-live claims.
        self.leases.retain(|_, claims| claims.lease_id != lease_id);
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.provisional.lease_id == lease_id)
        {
            self.prepared = None;
        }
        let frame = PluginLeaseRevokeFrame {
            kind: "plugin_lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: route.device_id,
            lease_id,
            lease_jti: route.lease_jti,
            call_id: route.binding.call_id,
            call_epoch: route.binding.call_epoch,
            fence: route.binding.fence,
            reason: reason.into(),
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Media revocation frame is invalid"))?;
        serde_json::to_string(&frame)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("Media revocation could not be encoded"))
    }
}
