//! Authenticated v2 Companion signalling connection.
//!
//! The implementation lives in this plugin so media control remains next to
//! the physical radio truth.  Only SDP/ICE and epoch-bound lease transitions
//! cross the WebSocket; PCM remains inside native WebRTC tracks.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aokie_media::{
    IceCandidateSignal, IceServerConfig, MediaMode, SdpSignal, SdpSignalType, SessionBinding,
};
use aokie_protocol::v2::{
    peer_roster_hash, sdp_dtls_fingerprint, sdp_sha256, tracks_for, AdmissionRole,
    AuthoritativeCallSnapshot, CallerProjection, Caption, CarrierHoldEvidence, EndCallerOutcome,
    EndpointBindingClaims, EndpointChallengeFrame, EndpointKeyAlgorithm, EndpointPublicKey, Grant,
    HelloProofClaims, LeaseClaims, LeaseHeartbeatFrame, LeaseMode, LeasePhase, LeaseRequestFrame,
    LeaseRevokeFrame, MediaState, MobileHello, MobileOfferAnswerFrame, MobileOfferSurface,
    MobileRtcSignalFrame, PendingMobileOfferClaims, PluginAssistanceAnswerFrame,
    PluginAssistanceRequestFrame, PluginClaimDecisionFrame, PluginClaimRejectedFrame,
    PluginEndCallerExecuteFrame, PluginEndCallerResultFrame, PluginHello, PluginIdleFrame,
    PluginLeaseRevokeFrame, PluginLeaseStatus, PluginLeaseStatusFrame, PluginOfferAcceptedFrame,
    PluginRtcSignalFrame, PluginSnapshotFrame, RemoteCapabilities, RemoteConsentPolicy, RtcSignal,
    SecondaryCallObservation, SecondaryCallPolicy, ServiceMode as ProtocolServiceMode,
    SignedEndpointBinding, SignedHelloProof, SignedPendingMobileOffer,
    SignedTrickleCandidateEnvelope, TelephonyState, TrickleCandidateClaims, V2ProtocolError,
    LEASE_AUDIENCE, MAX_LEASE_TOKEN_BYTES, MAX_PENDING_MOBILE_OFFERS, SCHEMA_VERSION,
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
/// Begin replacing an admission while this much of its already safety-bounded
/// lifetime remains. The current carrier stays authoritative throughout the
/// overlap, so broker and endpoint-challenge latency cannot stop lease
/// heartbeats from reaching the session loop.
const ADMISSION_ROTATION_OVERLAP: Duration = Duration::from_secs(30);
/// A replacement carrier opens away from the authority loop, but it is still
/// bounded: an attempt that cannot prove its endpoint before this window must
/// yield to a retry while the predecessor is still inside its safe lifetime.
const ADMISSION_TRANSPORT_OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const ADMISSION_ROTATION_RETRY_DELAY: Duration = Duration::from_secs(1);
// plugin.init's compact bootstrap intentionally omits token expiry. Consume it
// once and rotate through the Desktop broker quickly rather than assuming the
// gateway's maximum admission lifetime.
const DEFAULT_BOOTSTRAP_LIFETIME: Duration = Duration::from_secs(45);
const MAX_BACKOFF: Duration = Duration::from_secs(20);
const ACTIVE_LEASE_FALLBACK_TTL: u64 = 20;
const MAX_USED_ENDPOINT_JTIS: usize = 4_096;
const ADMISSION_SAFETY_MARGIN_SECONDS: u64 = 10;
/// Desktop's token for the hosted-relay carrier in `supportedTransports`.
/// It compares the string exactly, so this is a shared wire constant.
const RELAY_TRANSPORT: &str = "relay";
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

#[derive(Clone)]
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
    /// Present only when the admission advertised the hosted relay AND every
    /// advertised URL passed [`normalize_relay_url`]. `None` selects the
    /// WebSocket gateway, which stays the default transport.
    relay: Option<RelayEndpoints>,
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
            .field("transport", &self.transport_label())
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
            // plugin.init's compact bootstrap carries no transport
            // advertisement; the first brokered admission refresh decides.
            relay: None,
        }))
    }

    fn transport_label(&self) -> &'static str {
        if self.relay.is_some() {
            "relay"
        } else {
            "websocket"
        }
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

/// FormLogic-hosted relay transport advertised alongside the WebSocket
/// gateway. Absent (the default) keeps the untouched WebSocket path, so
/// withdrawing the member server-side reverts the transport with no rebuild.
///
/// ⚠️ Deliberately NOT `deny_unknown_fields`, unlike every security-bearing
/// document around it. This is an additive transport hint: a server that later
/// advertises another member (the long-poll fallback the relay controller
/// already serves is the obvious next one) must leave this build using the
/// three URLs it does understand, not lose the whole admission over a member
/// it was never taught.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RelayEndpoints {
    pub(crate) challenge_url: String,
    pub(crate) frames_url: String,
    pub(crate) stream_url: String,
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
    /// Tolerated ahead of the Desktop projection that forwards it: this
    /// decoder is `deny_unknown_fields`, so the member has to be accepted
    /// before it can ever arrive.
    ///
    /// Held as a raw value rather than a typed member ON PURPOSE. Decoding it
    /// inline would make a malformed or reshaped advertisement fail the whole
    /// admission — a `rebootstrap` that takes the entire Companion surface
    /// down — when the transport is additive and the correct answer is to keep
    /// using the WebSocket gateway. [`usable_relay_endpoints`] owns that
    /// decision, so every rejection lands on the same degrade path.
    #[serde(default)]
    relay: Option<Value>,
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
            relay: self.relay.and_then(usable_relay_endpoints),
        })
    }
}

/// Accept an advertised relay only when it decodes to the shape this build
/// understands, every URL is safe, AND all three share one origin. A rejected
/// advertisement degrades to the WebSocket gateway rather than failing the
/// admission: the transport is additive, and refusing the whole admission over
/// it would take the Companion surface down harder than simply not adopting
/// the new path.
fn usable_relay_endpoints(advertisement: Value) -> Option<RelayEndpoints> {
    let relay: RelayEndpoints = match serde_json::from_value(advertisement) {
        Ok(relay) => relay,
        Err(_) => {
            eprintln!(
                "[aokie-plugin][companion] stage=relay_advertisement_rejected transport=websocket detail=The relay advertisement is not the shape this build understands"
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
            Err(error) => {
                eprintln!(
                    "[aokie-plugin][companion] stage=relay_advertisement_rejected transport=websocket detail={}",
                    sanitize_status_message(&error.message)
                );
                return None;
            }
        }
    }
    if origins.windows(2).any(|pair| pair[0] != pair[1]) {
        eprintln!(
            "[aokie-plugin][companion] stage=relay_advertisement_rejected transport=websocket detail=Companion relay URLs span more than one origin"
        );
        return None;
    }
    Some(relay)
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
pub(crate) enum WorkerErrorKind {
    Reconnect,
    Expired,
    AdmissionRefresh,
    Rebootstrap,
}

/// Whether a carrier actually accepted a frame for delivery.
///
/// `Dropped` is deliberately non-fatal: relay backpressure must not tear down
/// a live call. It is still distinct from success because a consult/takeover
/// claim may only be armed after its provisional grant was accepted by the
/// carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportDelivery {
    Delivered,
    Dropped,
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
pub(crate) struct WorkerError {
    pub(crate) kind: WorkerErrorKind,
    pub(crate) message: String,
}

impl WorkerError {
    pub(crate) fn reconnect(message: impl Into<String>) -> Self {
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

    pub(crate) fn rebootstrap(message: impl Into<String>) -> Self {
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
    let params = admission_request_params(app_id, plugin_id, &endpoint_authority)?;
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

/// The `companion.admission` RPC request. Pure so the transport negotiation
/// below can be locked in a test — Desktop strips the relay advertisement from
/// every admission until the plugin asks for it, so this request IS the switch
/// that activates the hosted carrier.
fn admission_request_params(
    app_id: Option<&str>,
    plugin_id: &str,
    endpoint_authority: &EndpointAuthority,
) -> Result<serde_json::Map<String, Value>, WorkerError> {
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
    // Desktop NEGOTIATES the optional `relay` member rather than deploy-ordering
    // it: it forwards the advertisement only to a build that asks, so a plugin
    // that predates the transport keeps the exact pre-relay wire shape whichever
    // side upgrades first. Asking is therefore what activates the carrier — the
    // member is stripped from every admission until this is sent.
    //
    // Gated with the carrier itself: a non-voice build cannot open a relay
    // channel, so it must not ask for endpoints it would only log and ignore.
    if cfg!(feature = "voice") {
        params.insert(
            "supportedTransports".into(),
            Value::Array(vec![Value::String(RELAY_TRANSPORT.into())]),
        );
    }
    Ok(params)
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

/// Relay endpoints are ordinary HTTP resources, so they cannot share
/// [`normalize_gateway_url`]: that one forces `wss` and rewrites the path to
/// `/v2/realtime`, which would destroy a mailbox URL. The path here is
/// authoritative and preserved exactly as advertised.
///
/// ⚠️ The `http` exception exists because the current deployment serves
/// `http://formlogic.local` with its API on `http://api.formlogic.local`:
/// neither presents a certificate, so requiring `https` would make the hosted
/// relay unreachable on the very install it was built for. It means the
/// admission bearer rides plaintext over the LAN, which is why the exception
/// is confined to loopback / `.local` hosts on managed-beta builds.
fn normalize_relay_url(raw: &str, label: &str) -> Result<Url, WorkerError> {
    let invalid =
        |detail: &str| WorkerError::rebootstrap(format!("Companion relay {label} {detail}"));
    let url = Url::parse(raw).map_err(|_| invalid("is not an absolute URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("must not contain credentials"));
    }
    if url.fragment().is_some() {
        return Err(invalid("must not contain a fragment"));
    }
    let secure = url.scheme() == "https";
    let local_plaintext = cfg!(feature = "managed-beta-driver")
        && url.scheme() == "http"
        && url.host_str().is_some_and(is_local_network_host);
    if !secure && !local_plaintext {
        return Err(invalid(
            "must use https (managed-beta builds may use http on a loopback or .local host)",
        ));
    }
    Ok(url)
}

/// Loopback plus the mDNS `.local` names the desktop install actually serves.
fn is_local_network_host(host: &str) -> bool {
    is_loopback_host(host) || host.to_ascii_lowercase().ends_with(".local")
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

/// The relay's party identifier for a Companion endpoint key.
///
/// Lives here rather than in the voice-gated carrier so the session (which
/// compiles unconditionally) and [`crate::companion_relay`] cannot drift: the
/// carrier's approved-party set, its greeting book and the session's re-greet
/// signal must all name a party the same way or the greeting is retired for a
/// party that does not exist.
pub(crate) fn relay_party(endpoint_key_thumbprint: &str) -> String {
    format!("mobile:{endpoint_key_thumbprint}")
}

/// Whether this build may act as the lease authority on the relay carrier.
///
/// Set `AOKIE_RELAY_LEASE_AUTHORITY=0` to switch the whole path off on a
/// running install without a rebuild: no offers are published and every arm
/// beyond the authenticated hello goes back to being dropped, which is exactly
/// how the relay behaved before. Anything else, including the variable being
/// absent, leaves it on.
fn relay_lease_authority_enabled() -> bool {
    !std::env::var("AOKIE_RELAY_LEASE_AUTHORITY").is_ok_and(|value| value.trim() == "0")
}

fn relay_mode_grant(mode: LeaseMode) -> Grant {
    match mode {
        LeaseMode::Monitor => Grant::Monitor,
        LeaseMode::Consult => Grant::Consult,
        LeaseMode::Takeover => Grant::Takeover,
    }
}

/// Authority required for every relay media operation.
///
/// These grants come only from FormLogic's authenticated outer envelope or a
/// previously verified hello carrying that metadata. A frame's own `mode`,
/// `requiredGrants`, or any other peer-controlled field never supplies one.
fn relay_grants_allow_mode(grants: &HashSet<Grant>, mode: LeaseMode) -> bool {
    grants.contains(&Grant::StateRead)
        && grants.contains(&Grant::RtcSignal)
        && grants.contains(&relay_mode_grant(mode))
        // A takeover claimant must already be authorized to return the
        // caller to Aokie.  Granting seizure without its mandatory failback
        // operation would turn later policy narrowing or media failure into
        // dead air. Exact-holder revoke remains identity-fenced separately.
        && (mode != LeaseMode::Takeover || grants.contains(&Grant::ResumeAokie))
}

/// Compare two bearer tokens without leaking their divergence point.
///
/// These are secrets the plugin minted and the peer presents back, so the
/// comparison is the authentication step; a short-circuiting `==` would let a
/// peer recover a token byte by byte from timing.
fn tokens_match(minted: &str, presented: &str) -> bool {
    let minted = minted.as_bytes();
    let presented = presented.as_bytes();
    if minted.len() != presented.len() {
        return false;
    }
    minted
        .iter()
        .zip(presented)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
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
    remote_microphone_ready: bool,
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
    /// Relay offers are minted against the physical switchboard revision.
    /// Socket leases predate that dialect and leave this unset.
    expected_switchboard_revision: Option<u64>,
    confirmed_owner_epoch: Option<u64>,
    provisional_sdp_revision: u64,
    provisional_transport_generation: u64,
    decision_sent: bool,
    /// Non-renewable bound between delivery of the active lease and opening
    /// its replacement peer. Heartbeats may extend the lease itself, but can
    /// never monopolize the claimant slot (or, for Consult, suppress Aokie)
    /// without a usable media path.
    active_rebind_deadline: Option<Instant>,
}

/// A Companion that proved itself over the relay, remembered by device.
///
/// The two recorded facts come from the VERIFIED hello proof, never from a
/// self-asserted field: the thumbprint the signature actually proved, and the
/// mobile's own session nonce. The nonce is not bookkeeping — every later RTC
/// signal is checked against `LeaseClaims::session_nonce`, so a lease minted
/// with the wrong one silently rejects every signal the device sends.
#[derive(Clone)]
struct RelayPeer {
    holder_key_thumbprint: String,
    session_nonce: String,
    /// Exact server-authenticated admission scopes carried by this peer's
    /// verified hello. Payload fields never contribute authority.
    grants: HashSet<Grant>,
}

/// An offer this plugin minted, kept so a device cannot redeem one it invented.
#[derive(Clone)]
struct MintedOffer {
    claims: PendingMobileOfferClaims,
    token: String,
    /// Set when the device answered it. An accepted offer is never replaced by
    /// a fresher mint, because the device is mid-redemption against this exact
    /// `offerId`.
    accepted: bool,
}

/// A lease this plugin minted, kept so a device cannot present one it invented.
///
/// Only what is needed to recognise the lease later and address its holder; the
/// claims themselves live in the session's own lease book, which stays the
/// authority on whether a lease is still alive.
#[derive(Clone)]
struct RelayLease {
    lease_id: String,
    device_id: String,
    request_id: String,
    token: String,
    current_jti: String,
    phase: LeasePhase,
    status: PluginLeaseStatus,
    mode: LeaseMode,
}

/// The exact receive-only RTC binding that was superseded when a PREPARED
/// consult/takeover became ACTIVE.
///
/// Relay delivery is at-least-once and trickled ICE can overtake the ACTIVE
/// lease status.  Keeping this binding briefly lets the plugin recognise that
/// exact, already-retired generation and discard it without presenting it to
/// the native peer.  It is deliberately not a lease alias: only RTC signalling
/// consults it, every binding field and the bearer token must match, and it can
/// never renew, revoke, route media, or mutate caller ownership.
#[derive(Clone)]
struct RetiredPreparedRtcBinding {
    claims: LeaseClaims,
    token: String,
    sdp_revision: u64,
    transport_generation: u64,
    expires_at: Instant,
}

/// A consult/takeover claim that has been minted but deliberately NOT armed.
///
/// See [`GatewaySession::finish_relay_delivery`] for why the arming waits.
struct DeferredPrepare {
    encoded: String,
    notice: LeaseNotice,
    expected_switchboard_revision: u64,
    lease_id: String,
    device_id: String,
    request_id: String,
    offer_id: String,
    offer: MintedOffer,
    replay_key: String,
}

/// A relay lease-status transition waiting for the carrier's delivery result.
///
/// Authority is either committed on `Delivered` or rolled back on `Dropped`;
/// HTTP backpressure therefore stays non-fatal without becoming permission to
/// extend or arm authority the device never learned about.
enum PendingRelayStatus {
    MonitorGrant {
        encoded: String,
        notice: LeaseNotice,
        lease_id: String,
        offer_id: String,
        offer: MintedOffer,
        replay_key: String,
    },
    Renewal {
        encoded: String,
        notice: LeaseNotice,
        lease_id: String,
        replay_key: String,
    },
    Active {
        encoded: String,
        lease_id: String,
        claims: LeaseClaims,
        token: String,
    },
}

impl PendingRelayStatus {
    fn encoded(&self) -> &str {
        match self {
            Self::MonitorGrant { encoded, .. }
            | Self::Renewal { encoded, .. }
            | Self::Active { encoded, .. } => encoded,
        }
    }

    fn lease_id(&self) -> &str {
        match self {
            Self::MonitorGrant { lease_id, .. }
            | Self::Renewal { lease_id, .. }
            | Self::Active { lease_id, .. } => lease_id,
        }
    }
}

#[derive(Clone)]
enum RelayReplayResult {
    OfferAccepted {
        encoded: String,
        mode: LeaseMode,
    },
    LeaseStatus {
        lease_id: String,
    },
    /// A terminal claim decision after its one-shot offer was consumed.
    /// Remembering the exact encoded refusal is what makes relay delivery
    /// retries converge instead of falling through to a now-missing offer.
    Rejected {
        encoded: String,
    },
    /// A native RTC failure both rejects the provoking signal and revokes the
    /// exact lease. Relay egress posts these frames separately, so retries must
    /// reproduce both byte-for-byte without executing the teardown twice.
    TerminalRtcFailure {
        revocation: String,
        rejection: String,
    },
    RtcAccepted {
        mode: LeaseMode,
    },
    /// A valid signal for the just-retired PREPARED generation was consumed as
    /// a no-op.  Its replay lifetime is the tombstone lifetime, not the normal
    /// two-minute operation ledger lifetime.
    RetiredPreparedRtcDropped {
        lease_id: String,
        mode: LeaseMode,
        expires_at: Instant,
    },
    HeartbeatStatus {
        lease_id: String,
    },
}

#[derive(Clone)]
struct RelayReplay {
    fingerprint: String,
    device_id: String,
    result: RelayReplayResult,
    seen_at: Instant,
}

/// One exact terminal lease notice still owed to a relay Companion.
///
/// The lease has already been retired locally; this is delivery bookkeeping
/// only and must never execute that teardown again.  `encoded` is retained
/// byte-for-byte because the mobile's completed-revocation tombstone makes an
/// exact duplicate safe, while synthesising a later notice could change the
/// JTI/fence that proves which authority ended.
struct PendingRelayRevocation {
    encoded: String,
    device_id: String,
    registered_at: Instant,
    next_attempt_at: Instant,
    expires_at: Instant,
}

/// Relay peers remembered per session. One owner rarely approves more.
const MAX_RELAY_PEERS: usize = 16;
/// Terminal notices may briefly outlive the leases/peers they retire. Keep
/// enough room for every admitted peer plus overlap, but never let a broken
/// relay grow an unbounded egress ledger.
const MAX_PENDING_RELAY_REVOCATIONS: usize = MAX_RELAY_PEERS * 2;
/// Relay delivery has its own bounded retry/backoff. Sending only the oldest
/// debt per gateway tick prevents a full ledger from delaying lease expiry,
/// heartbeats and caller-state reconciliation for many seconds.
const MAX_PENDING_RELAY_REVOCATIONS_PER_POLL: usize = 1;
/// Companion lease expiry (or a fresh-session snapshot after reconnect) is
/// the bounded fallback. Exact revokes get a retry window long enough to cross
/// transient relay backpressure, without posting four times a second forever
/// to an unavailable phone.
const PENDING_RELAY_REVOCATION_TTL: Duration = Duration::from_secs(30);
/// Live minted offers held at once: at most one published per (device, mode),
/// plus superseded ones kept until expiry so an in-flight answer still resolves.
const MAX_RELAY_OFFERS: usize = 32;
/// Concurrent plugin-minted leases. `claimant_busy` keeps the real number at 1;
/// the cap is what makes that structural.
const MAX_RELAY_LEASES: usize = 4;
/// Lifetime of a minted offer. Under [`MOBILE_OFFER_MAX_LIFETIME`], and long
/// enough that an offer published at the slowest snapshot cadence is still
/// answerable when it arrives.
const RELAY_OFFER_TTL: u64 = 25;
/// Remaining offer life below which a fresh one is minted for that (device,
/// mode). Keeps every published offer comfortably answerable.
const RELAY_OFFER_REFRESH_MARGIN: u64 = 10;
/// Lifetime of a prepared consult/takeover lease.
///
/// Deliberately short AND non-renewable: a prepare that cannot complete inside
/// this window must hand the caller back to the AI, and refusing renewal is
/// what makes a stuck prepare impossible to keep alive. This is the largest
/// caller-visible dead-air window the relay path can produce.
const RELAY_PREPARED_LEASE_TTL: u64 = 15;
/// Lifetime of an active lease, extended by each heartbeat. Far inside the
/// 300-second local safety cap.
const RELAY_ACTIVE_LEASE_TTL: u64 = 20;
/// A prepared takeover leaves Aokie serving the caller, so the first-use
/// microphone permission prompt gets a humane window without risking silence.
const RELAY_TAKEOVER_ACTIVE_REBIND_TIMEOUT: Duration = Duration::from_secs(30);
/// Private consult deliberately holds Aokie away from the caller. Bound the
/// active-peer rebind much more tightly so a missing offer cannot become an
/// indefinitely renewable silent hold.
const RELAY_CONSULT_ACTIVE_REBIND_TIMEOUT: Duration = Duration::from_secs(8);
/// Rolling window and budget for control requests, per device.
const RELAY_REQUEST_WINDOW: Duration = Duration::from_secs(10);
const RELAY_REQUEST_BUDGET: u32 = 10;
/// RTC trickle is naturally bursty (offer plus a set of gathered candidates),
/// so it has an independent bounded lane.  Sharing the ten-request control
/// lane made one healthy offer + lease + candidate burst deterministically
/// throttle its own ACTIVE rebind.
const RELAY_RTC_SIGNAL_BUDGET: u32 = 32;
/// At-least-once relay ordering can deliver the final PREPARED candidates just
/// after the ACTIVE token/JTI rotation.  Ten seconds covers that reordering
/// without turning the old bearer into long-lived authority.
const RETIRED_PREPARED_RTC_TTL: Duration = Duration::from_secs(10);
/// How long a replay result is remembered, and how many at once.
const RELAY_REPLAY_TTL: Duration = Duration::from_secs(120);
const MAX_RELAY_REPLAYS: usize = 256;

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
    relay_snapshot_event_id: Option<String>,
    relay_snapshot_delivered_devices: HashSet<String>,
    pending_end_caller: HashMap<String, PendingEndCaller>,
    /// Unhandled relay frame kinds already reported, so a Companion emitting one
    /// on a timer cannot wrap the bounded log ring during a call. Bounded, and
    /// keyed by kind so a genuinely NEW kind is still surfaced once.
    dropped_relay_kinds: HashSet<String>,
    /// When a refused relay hello was last reported, and how many refusals were
    /// held back since. Every refusal is a security event worth recording, and
    /// an approved-yet-misbehaving device retries on a timer, so refusals are
    /// RATE-limited rather than capped: bounded tightly enough that they cannot
    /// wrap the log ring during a call, but never permanently silent.
    relay_hello_rejection_logged_at: Option<Instant>,
    relay_hello_rejections_suppressed: u32,
    /// A relay party that must be greeted again before the next publish,
    /// carried from [`Self::accept_mobile_hello`] out to the carrier that owns
    /// the greeting book and can fetch a fresh challenge. Set only for a hello
    /// whose proof VERIFIED, so an unauthenticated frame can never provoke an
    /// extra signed hello.
    relay_regreet_party: Option<String>,
    /// A device route proved by the hello currently being handled.
    ///
    /// The carrier consumes this immediately after the session returns. Route
    /// ownership must never be learned from a frame's self-asserted `deviceId`:
    /// only the hello proof binds a device to an approved endpoint party.
    relay_verified_route: Option<(String, String)>,

    // --- Relay lease authority -------------------------------------------
    //
    // On the socket a trusted gateway minted, signed and fenced every lease,
    // and this plugin only ever CONSUMED the result — which is why
    // `validate_notice` checks identity and epochs but never the lease token.
    // The relay has no such authority, so the plugin takes the role itself:
    // it mints offers and leases from live radio truth and remembers exactly
    // what it minted. That is what replaces the missing signature check. A
    // peer can then only ever ask; anything it did not receive from us here
    // is unrecognised, and refusing is free.
    /// True once per session when the feature is enabled. Read from the
    /// environment at construction so the whole path can be turned off on a
    /// running install without a rebuild.
    relay_authority_enabled: bool,
    /// Whether the CURRENT transport is the relay, refreshed each loop turn.
    /// Offers are published and claim decisions are self-consumed only here.
    relay_carrier: bool,
    relay_peers: HashMap<String, RelayPeer>,
    relay_offers: HashMap<String, MintedOffer>,
    relay_leases: HashMap<String, RelayLease>,
    deferred_prepare: Option<DeferredPrepare>,
    pending_relay_status: Option<PendingRelayStatus>,
    pending_relay_revocations: HashMap<String, PendingRelayRevocation>,
    /// Strictly increasing per session, so a replayed older takeover fence can
    /// never look current.
    next_takeover_fence: u64,
    relay_request_budget: HashMap<String, (Instant, u32)>,
    relay_rtc_signal_budget: HashMap<String, (Instant, u32)>,
    retired_prepared_rtc: HashMap<String, RetiredPreparedRtcBinding>,
    relay_replays: HashMap<String, RelayReplay>,
}

/// Distinct unhandled relay kinds reported per session.
const MAX_REPORTED_RELAY_KINDS: usize = 16;
/// Shortest gap between two reported relay-hello refusals. Long enough that a
/// device retrying on a timer cannot crowd the log ring during a call, short
/// enough that a systematic refusal stays visible for as long as it persists.
const RELAY_HELLO_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(60);

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
            relay_snapshot_event_id: None,
            relay_snapshot_delivered_devices: HashSet::new(),
            pending_end_caller: HashMap::new(),
            dropped_relay_kinds: HashSet::new(),
            relay_hello_rejection_logged_at: None,
            relay_hello_rejections_suppressed: 0,
            relay_regreet_party: None,
            relay_verified_route: None,
            relay_authority_enabled: relay_lease_authority_enabled(),
            relay_carrier: false,
            relay_peers: HashMap::new(),
            relay_offers: HashMap::new(),
            relay_leases: HashMap::new(),
            deferred_prepare: None,
            pending_relay_status: None,
            pending_relay_revocations: HashMap::new(),
            next_takeover_fence: 1,
            relay_request_budget: HashMap::new(),
            relay_rtc_signal_budget: HashMap::new(),
            retired_prepared_rtc: HashMap::new(),
            relay_replays: HashMap::new(),
        }
    }

    /// The relay party owed a fresh greeting, consumed once.
    fn take_relay_regreet_party(&mut self) -> Option<String> {
        self.relay_regreet_party.take()
    }

    /// The verified device/party route produced by the last admitted hello.
    fn take_relay_verified_route(&mut self) -> Option<(String, String)> {
        self.relay_verified_route.take()
    }

    /// Force the current authoritative state to be published on the next loop
    /// turn. Called once when a mobile hello is admitted and again after its
    /// asynchronous fresh plugin proof is installed: state sent while the
    /// challenge was in flight cannot substitute for state BEHIND that proof.
    fn rearm_authoritative_publication(&mut self) {
        self.authoritative_idle = false;
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.relay_snapshot_event_id = None;
        self.relay_snapshot_delivered_devices.clear();
        self.last_assistance_request_sent = None;
        self.next_snapshot_poll = Instant::now();
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
        self.relay_snapshot_event_id = None;
        self.relay_snapshot_delivered_devices.clear();
        // Endpoint-session rotation invalidates every signature context the
        // retired PREPARED generation carried.  It must not survive merely as
        // a bearer-token match.
        self.retired_prepared_rtc.clear();
        self.relay_replays.retain(|_, replay| {
            !matches!(
                &replay.result,
                RelayReplayResult::RetiredPreparedRtcDropped { .. }
            )
        });
        Ok(())
    }

    fn apply_admission_rotation(
        &mut self,
        credentials: &SessionCredentials,
        plugin_session_nonce: String,
        preserve_continuity: bool,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        if preserve_continuity {
            return self.rotate_credentials(credentials, plugin_session_nonce);
        }
        // Sequence numbers, routes and lease heartbeats from another relay
        // mailbox cannot prove authority here. Return the caller first, then
        // replace every logical-session registry in one assignment.
        media.fail_closed_all("gateway_admission_domain_changed");
        *self = Self::new(credentials, plugin_session_nonce);
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

    fn test_plugin_revocation(
        session: &GatewaySession,
        device_id: &str,
        lease_id: &str,
    ) -> (PluginLeaseRevokeFrame, String) {
        let frame = PluginLeaseRevokeFrame {
            kind: "plugin_lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: session.app_id.clone(),
            device_id: device_id.into(),
            lease_id: lease_id.into(),
            lease_jti: format!("{lease_id}_jti"),
            call_id: "call_a".into(),
            call_epoch: 7,
            fence: 9,
            reason: "terminal_rtc_failure".into(),
        };
        frame.validate().unwrap();
        let encoded = serde_json::to_string(&frame).unwrap();
        (frame, encoded)
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

    #[test]
    fn pending_revokes_follow_admission_continuity_but_not_a_new_mobile_session_or_domain() {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        let mut session = GatewaySession::new(&credentials, "plugin_session_old".into());
        session.relay_carrier = true;
        let (_, encoded) = test_plugin_revocation(&session, "device_a", "lease_continuity");
        session.prepare_relay_delivery(&encoded);
        assert_eq!(session.pending_relay_revocations.len(), 1);

        let refreshed = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();
        session
            .rotate_credentials(&refreshed, "plugin_session_rotated".into())
            .unwrap();
        assert!(session
            .pending_relay_revocations
            .get("lease_continuity")
            .is_some_and(|pending| pending.encoded == encoded));

        let media = RemoteMediaHandle::spawn().unwrap();
        session.revoke_relay_device_authority("device_a", &full_relay_grants(), true, &media);
        assert!(session.pending_relay_revocations.is_empty());

        session.prepare_relay_delivery(&encoded);
        assert_eq!(session.pending_relay_revocations.len(), 1);
        session
            .apply_admission_rotation(
                &refreshed,
                "plugin_session_new_domain".into(),
                false,
                &media,
            )
            .unwrap();
        assert!(session.pending_relay_revocations.is_empty());
    }

    #[test]
    fn pending_revoke_retry_is_fair_when_the_oldest_delivery_drops_again() {
        let mut harness = RelayHarness::new();
        let (_, first) =
            test_plugin_revocation(&harness.session, &harness.device_id, "lease_retry_a");
        let (_, second) =
            test_plugin_revocation(&harness.session, &harness.device_id, "lease_retry_b");
        harness.session.prepare_relay_delivery(&first);
        harness.session.prepare_relay_delivery(&second);

        let due_at = Instant::now();
        {
            let first_pending = harness
                .session
                .pending_relay_revocations
                .get_mut("lease_retry_a")
                .unwrap();
            first_pending.registered_at = due_at;
            first_pending.next_attempt_at = due_at;
            let second_pending = harness
                .session
                .pending_relay_revocations
                .get_mut("lease_retry_b")
                .unwrap();
            second_pending.registered_at = due_at + Duration::from_nanos(1);
            second_pending.next_attempt_at = due_at;
        }

        let first_attempt = harness.session.due_pending_relay_revocations(due_at);
        assert_eq!(first_attempt, vec![first.clone()]);
        assert_eq!(
            harness
                .session
                .pending_relay_revocations
                .get("lease_retry_b")
                .unwrap()
                .next_attempt_at,
            due_at,
            "only the selected revoke advances"
        );
        harness.session.finish_relay_delivery(
            &first,
            TransportDelivery::Dropped,
            &harness.media,
            &harness.radio,
        );

        let second_attempt = harness
            .session
            .due_pending_relay_revocations(Instant::now());
        assert_eq!(second_attempt, vec![second]);
    }

    #[test]
    fn pending_revoke_ledger_evicts_oldest_at_its_cap_and_prunes_its_ttl() {
        let mut session = test_gateway_session();
        session.relay_carrier = true;
        let base = Instant::now();
        for index in 0..(MAX_PENDING_RELAY_REVOCATIONS + 3) {
            let lease_id = format!("lease_bounded_{index:02}");
            let (frame, encoded) = test_plugin_revocation(&session, "device_a", &lease_id);
            session.register_pending_relay_revocation(
                &frame,
                &encoded,
                base + Duration::from_nanos(index as u64),
            );
        }
        assert_eq!(
            session.pending_relay_revocations.len(),
            MAX_PENDING_RELAY_REVOCATIONS
        );
        assert!(!session
            .pending_relay_revocations
            .contains_key("lease_bounded_00"));
        assert!(session
            .pending_relay_revocations
            .contains_key("lease_bounded_03"));

        assert!(session
            .due_pending_relay_revocations(
                base + PENDING_RELAY_REVOCATION_TTL + Duration::from_secs(1)
            )
            .is_empty());
        assert!(session.pending_relay_revocations.is_empty());
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
            expected_switchboard_revision: None,
            confirmed_owner_epoch: (phase == LeasePhase::Active).then_some(claims.owner_epoch),
            provisional_sdp_revision: 1,
            provisional_transport_generation: 1,
            decision_sent: phase == LeasePhase::Active,
            active_rebind_deadline: None,
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
                remote_microphone_ready: phase == LeasePhase::Active,
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

    /// A real `mobile_hello`: signed by `signing_key` over the same
    /// domain-separated claims the Companion signs, in the mobile peer-policy
    /// shape (an expected peer, and no roster members at all).
    fn mobile_hello(
        session: &GatewaySession,
        signing_key: &SigningKey,
        device_id: &str,
        jti: &str,
        expected_peer_thumbprint: &str,
    ) -> String {
        mobile_hello_with_nonce(
            session,
            signing_key,
            device_id,
            jti,
            expected_peer_thumbprint,
            &format!("mobile_session_{device_id}"),
        )
    }

    fn mobile_hello_with_nonce(
        session: &GatewaySession,
        signing_key: &SigningKey,
        device_id: &str,
        jti: &str,
        expected_peer_thumbprint: &str,
        session_nonce: &str,
    ) -> String {
        let endpoint_key =
            EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
        let now = unix_now().unwrap();
        let claims = HelloProofClaims {
            app_id: session.app_id.clone(),
            subject_id: device_id.to_owned(),
            role: AdmissionRole::Mobile,
            connection_id: "relay_c0ffee".into(),
            challenge_nonce: "challenge_abc123".into(),
            admission_jti: "jti_abc123".into(),
            session_nonce: session_nonce.to_owned(),
            holder_key_thumbprint: endpoint_key.thumbprint.clone(),
            expected_peer_key_thumbprint: Some(expected_peer_thumbprint.to_owned()),
            approved_peer_key_thumbprints: Vec::new(),
            peer_roster_revision: None,
            peer_roster_hash: None,
            nonce: format!("hello_nonce_{jti}"),
            jti: jti.to_owned(),
            issued_at: now,
            expires_at: now + 30,
        };
        let signature = URL_SAFE_NO_PAD.encode(
            signing_key
                .sign(&claims.signing_bytes().expect("claims canonicalize"))
                .to_bytes(),
        );
        let hello = MobileHello {
            kind: "mobile_hello".into(),
            schema_version: SCHEMA_VERSION,
            app_id: session.app_id.clone(),
            device_id: device_id.to_owned(),
            session_nonce: session_nonce.to_owned(),
            endpoint_proof: SignedHelloProof {
                endpoint_key,
                claims,
                signature,
            },
        };
        // Internal consistency only; whether the SIGNER is approved is exactly
        // what the plugin decides.
        hello.validate().expect("test mobile hello is well formed");
        serde_json::to_string(&hello).expect("test mobile hello encodes")
    }

    /// The approved Companion's key, as `test_authority` minted it.
    fn approved_mobile_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[10; 32])
    }

    fn approved_mobile_party() -> String {
        relay_party(
            &EndpointPublicKey::from_ed25519_bytes(
                &approved_mobile_signing_key().verifying_key().to_bytes(),
            )
            .thumbprint,
        )
    }

    fn full_relay_grants() -> HashSet<Grant> {
        HashSet::from([
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Monitor,
            Grant::Consult,
            Grant::Takeover,
            Grant::ResumeAokie,
        ])
    }

    fn monitor_relay_grants() -> HashSet<Grant> {
        HashSet::from([Grant::StateRead, Grant::RtcSignal, Grant::Monitor])
    }

    fn takeover_relay_grants() -> HashSet<Grant> {
        HashSet::from([
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Takeover,
            Grant::ResumeAokie,
        ])
    }

    // --- Relay lease authority ------------------------------------------
    //
    // The plugin is the authority on this carrier, so these drive it exactly
    // the way a Companion does: over `handle_relay_peer_frame`, with the
    // roster party the carrier reports, and never by reaching into session
    // state to install something the mint would not have produced.

    /// A live call with an approved Companion greeted on the relay.
    struct RelayHarness {
        session: GatewaySession,
        media: RemoteMediaHandle,
        radio: crate::radio::RadioHandle,
        status: Arc<crate::radio::RadioStatus>,
        _control_rx: std::sync::mpsc::Receiver<crate::radio::RadioControl>,
        device_id: String,
        party: String,
        grants: HashSet<Grant>,
    }

    impl RelayHarness {
        fn new() -> Self {
            Self::with_consent(consenting_gate())
        }

        fn with_consent(gate: crate::remote_media::RemoteConsentGate) -> Self {
            Self::with_consent_and_grants(gate, full_relay_grants())
        }

        fn with_grants(grants: HashSet<Grant>) -> Self {
            Self::with_consent_and_grants(consenting_gate(), grants)
        }

        fn with_consent_and_grants(
            gate: crate::remote_media::RemoteConsentGate,
            grants: HashSet<Grant>,
        ) -> Self {
            let media = RemoteMediaHandle::spawn().unwrap();
            media.set_remote_consent(gate);
            media.observe_physical_call(Some("call_a"), true);
            let (radio, control_rx, status) =
                crate::radio::RadioHandle::test_handle_with_media(media.clone());
            *status.current_call_id.lock().unwrap() = Some("call_a".into());
            status
                .call_active
                .store(true, std::sync::atomic::Ordering::Relaxed);

            let mut session = test_gateway_session();
            session.relay_carrier = true;
            let device_id = "device_a".to_string();
            let peer = plugin_thumbprint(&session);
            let hello = mobile_hello(
                &session,
                &approved_mobile_signing_key(),
                &device_id,
                "hello_jti_authority",
                &peer,
            );
            let party = approved_mobile_party();
            session
                .accept_mobile_hello(&hello, Some(&party), Some(&device_id), &grants, &media)
                .unwrap();
            Self {
                session,
                media,
                radio,
                status,
                _control_rx: control_rx,
                device_id,
                party,
                grants,
            }
        }

        /// Move the physical call, keeping radio and media truth in step the
        /// way the real radio does.
        fn move_call(&mut self, call_id: &str) {
            self.media.observe_physical_call(Some(call_id), true);
            *self.status.current_call_id.lock().unwrap() = Some(call_id.into());
        }

        /// Free the claimant slot, as a revoke or a failed route does.
        fn release_claimant(&mut self) {
            self.session.relay_leases.clear();
            self.session.leases.clear();
            self.session.prepared = None;
            self.session.deferred_prepare = None;
            self.session.pending_relay_status = None;
        }

        /// Publish a snapshot and return the offers it carried.
        ///
        /// Publication is normally edge-triggered on a state change or the
        /// ten-second refresh; forcing it here is what that refresh does, and it
        /// keeps these tests about the offers rather than about the cadence.
        fn publish_offers(&mut self) -> Vec<SignedPendingMobileOffer> {
            self.session.last_snapshot_fingerprint = None;
            self.session.last_snapshot_sent = None;
            let encoded = self
                .session
                .snapshot_frame(&self.radio)
                .expect("snapshot builds")
                .expect("a live call publishes a snapshot");
            serde_json::from_str::<PluginSnapshotFrame>(&encoded)
                .expect("snapshot decodes")
                .snapshot
                .pending_mobile_offers
        }

        fn offer_for(&mut self, mode: LeaseMode) -> SignedPendingMobileOffer {
            self.publish_offers()
                .into_iter()
                .find(|offer| offer.offer.offered_mode == mode)
                .unwrap_or_else(|| panic!("an offer for {mode:?} is published"))
        }

        /// Drive one peer frame the way the worker loop does.
        fn post(&mut self, encoded: &str) -> Vec<String> {
            self.post_as(encoded, Some(&self.party.clone()))
        }

        fn post_as(&mut self, encoded: &str, party: Option<&str>) -> Vec<String> {
            let grants = self.grants.clone();
            self.post_as_with_grants(encoded, party, &grants)
        }

        fn post_with_grants(
            &mut self,
            encoded: &str,
            authenticated_grants: &HashSet<Grant>,
        ) -> Vec<String> {
            self.post_as_with_grants(encoded, Some(&self.party.clone()), authenticated_grants)
        }

        fn post_as_with_grants(
            &mut self,
            encoded: &str,
            party: Option<&str>,
            authenticated_grants: &HashSet<Grant>,
        ) -> Vec<String> {
            self.session
                .handle_relay_peer_frame(
                    encoded,
                    party,
                    Some(&self.device_id),
                    authenticated_grants,
                    &self.media,
                    &self.radio,
                )
                .expect("relay peer traffic never terminates the session")
        }

        /// Settle the exact frame the transport just attempted, as the worker
        /// loop does after every relay POST.
        fn settle(&mut self, frames: &[String], delivery: TransportDelivery) {
            let encoded = frames.first().expect("a relay frame was returned");
            self.session.prepare_relay_delivery(encoded);
            self.session
                .finish_relay_delivery(encoded, delivery, &self.media, &self.radio);
        }

        fn answer(&mut self, offer: &SignedPendingMobileOffer, request: &str) -> Vec<String> {
            let frame = offer_answer(offer, &self.device_id, request);
            self.post(&frame)
        }

        /// Answer an offer and request the lease it allows, in one step.
        fn claim(&mut self, mode: LeaseMode, request: &str) -> Vec<String> {
            let offer = self.offer_for(mode);
            let accepted = self.answer(&offer, request);
            assert!(!accepted.is_empty(), "the offer answer is accepted");
            let frame = lease_request(&offer, request, "rtc_a");
            self.post(&frame)
        }

        fn granted(&self, frames: &[String]) -> PluginLeaseStatusFrame {
            let encoded = frames.first().expect("a lease status frame was returned");
            serde_json::from_str(encoded)
                .unwrap_or_else(|_| panic!("expected a lease status, got {encoded}"))
        }

        fn rejection(&self, frames: &[String]) -> PluginClaimRejectedFrame {
            frames
                .iter()
                .find_map(|encoded| serde_json::from_str(encoded).ok())
                .unwrap_or_else(|| panic!("expected a refusal, got {frames:?}"))
        }
    }

    fn activate_takeover_without_replacement_peer(
        harness: &mut RelayHarness,
        request_id: &str,
    ) -> PluginLeaseStatusFrame {
        let provisional_frames = harness.claim(LeaseMode::Takeover, request_id);
        let provisional = harness.granted(&provisional_frames);
        harness.settle(&provisional_frames, TransportDelivery::Delivered);
        let provisional_binding = binding_for_claims(&provisional.lease);
        harness
            .media
            .install_test_prepared_peer(provisional_binding.clone(), 10_000)
            .unwrap();
        harness
            .media
            .ack_prepare_human(&provisional_binding)
            .unwrap();
        let active_frames = harness
            .session
            .drain_media_events(&harness.media, &harness.radio)
            .unwrap();
        let active_encoded = active_frames
            .iter()
            .find(|encoded| {
                serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                    .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
            })
            .expect("preparation emits an active lease")
            .clone();
        let active = serde_json::from_str(&active_encoded).unwrap();
        harness.session.finish_relay_delivery(
            &active_encoded,
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        active
    }

    /// Exercise the same PREPARED -> ACTIVE delivery rotation as production,
    /// while marking the prepared peer's negotiated revision/generation.  The
    /// native test seam does not negotiate SDP itself, so the two values are
    /// supplied explicitly here.
    fn activate_takeover_with_retired_prepared_rtc(
        harness: &mut RelayHarness,
        request_id: &str,
    ) -> (PluginLeaseStatusFrame, PluginLeaseStatusFrame) {
        let provisional_frames = harness.claim(LeaseMode::Takeover, request_id);
        let provisional = harness.granted(&provisional_frames);
        harness.settle(&provisional_frames, TransportDelivery::Delivered);
        let prepared = harness
            .session
            .prepared
            .as_mut()
            .expect("the provisional grant is armed");
        prepared.provisional_sdp_revision = 1;
        prepared.provisional_transport_generation = 1;
        let provisional_binding = binding_for_claims(&provisional.lease);
        harness
            .media
            .install_test_prepared_peer(provisional_binding.clone(), 10_000)
            .unwrap();
        harness
            .media
            .ack_prepare_human(&provisional_binding)
            .unwrap();
        let active_frames = harness
            .session
            .drain_media_events(&harness.media, &harness.radio)
            .unwrap();
        let active_encoded = active_frames
            .iter()
            .find(|encoded| {
                serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                    .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
            })
            .expect("preparation emits an active lease")
            .clone();
        let active = serde_json::from_str(&active_encoded).unwrap();
        harness.session.finish_relay_delivery(
            &active_encoded,
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert!(harness
            .session
            .retired_prepared_rtc
            .contains_key(&provisional.lease.lease_id));
        (provisional, active)
    }

    fn signed_mobile_ice(
        lease: &LeaseClaims,
        lease_token: &str,
        signal_id: &str,
        endpoint_jti: &str,
        sdp_revision: u64,
        transport_generation: u64,
    ) -> String {
        let signing_key = approved_mobile_signing_key();
        let endpoint_key =
            EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
        let now = unix_now().unwrap();
        let candidate = "candidate:1 1 UDP 2122260223 192.0.2.1 54321 typ host".to_string();
        let claims = TrickleCandidateClaims {
            app_id: lease.app_id.clone(),
            plugin_id: lease.plugin_id.clone(),
            device_id: lease.device_id.clone(),
            rtc_session_id: lease.rtc_session_id.clone(),
            endpoint_session_nonce: lease.session_nonce.clone(),
            lease_jti: lease.jti.clone(),
            endpoint_role: AdmissionRole::Mobile,
            holder_key_thumbprint: lease.mobile_key_thumbprint.clone(),
            peer_key_thumbprint: lease.plugin_key_thumbprint.clone(),
            call_id: lease.call_id.clone(),
            call_epoch: lease.call_epoch,
            owner_epoch: lease.owner_epoch,
            fence: lease.fence,
            sdp_revision,
            transport_generation,
            candidate: Some(candidate.clone()),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
            end_of_candidates: false,
            nonce: format!("candidate_nonce_{endpoint_jti}"),
            jti: endpoint_jti.into(),
            issued_at: now,
            expires_at: now + 30,
        };
        let signature = URL_SAFE_NO_PAD.encode(
            signing_key
                .sign(
                    &claims
                        .signing_bytes()
                        .expect("candidate claims canonicalize"),
                )
                .to_bytes(),
        );
        serde_json::to_string(&MobileRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: lease.app_id.clone(),
            signal_id: signal_id.into(),
            plugin_id: lease.plugin_id.clone(),
            device_id: lease.device_id.clone(),
            lease_token: lease_token.into(),
            lease_jti: lease.jti.clone(),
            rtc_session_id: lease.rtc_session_id.clone(),
            sdp_revision,
            transport_generation,
            call_id: lease.call_id.clone(),
            call_epoch: lease.call_epoch,
            owner_epoch: lease.owner_epoch,
            fence: lease.fence,
            signal: RtcSignal::Ice {
                candidate,
                sdp_mid: Some("0".into()),
                sdp_m_line_index: Some(0),
                envelope: SignedTrickleCandidateEnvelope {
                    endpoint_key,
                    claims,
                    signature,
                },
            },
        })
        .expect("signed mobile ICE encodes")
    }

    fn consenting_gate() -> crate::remote_media::RemoteConsentGate {
        crate::remote_media::RemoteConsentGate {
            enabled: true,
            acknowledged: true,
            acknowledged_at: Some("2026-07-18T00:00:00Z".into()),
            expires_at: None,
            captions_enabled: true,
            assistance_enabled: true,
            monitor_enabled: true,
            consult_enabled: true,
            takeover_enabled: true,
            ..Default::default()
        }
    }

    fn offer_answer(offer: &SignedPendingMobileOffer, device_id: &str, request_id: &str) -> String {
        serde_json::to_string(&MobileOfferAnswerFrame {
            kind: "mobile_offer_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: offer.offer.app_id.clone(),
            request_id: request_id.into(),
            idempotency_key: format!("idem_answer_{request_id}"),
            offer_id: offer.offer.offer_id.clone(),
            offer_jti: offer.offer.jti.clone(),
            offer_token: offer.offer_token.clone(),
            target_device_id: device_id.into(),
            target_holder_key_thumbprint: offer.offer.target_holder_key_thumbprint.clone(),
            offered_mode: offer.offer.offered_mode,
            call_id: offer.offer.call_id.clone(),
            call_epoch: offer.offer.call_epoch,
            owner_epoch: offer.offer.owner_epoch,
        })
        .expect("offer answer encodes")
    }

    fn lease_request(
        offer: &SignedPendingMobileOffer,
        request_id: &str,
        rtc_session_id: &str,
    ) -> String {
        serde_json::to_string(&LeaseRequestFrame {
            kind: "lease_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: offer.offer.app_id.clone(),
            request_id: request_id.into(),
            idempotency_key: format!("idem_lease_{request_id}"),
            call_id: offer.offer.call_id.clone(),
            expected_call_epoch: offer.offer.call_epoch,
            expected_owner_epoch: offer.offer.owner_epoch,
            expected_switchboard_revision: offer.offer.switchboard_revision,
            expected_remote_revision: offer.offer.remote_revision,
            mode: offer.offer.offered_mode,
            rtc_session_id: rtc_session_id.into(),
            accepted_offer_id: offer.offer.offer_id.clone(),
            accepted_offer_jti: offer.offer.jti.clone(),
        })
        .expect("lease request encodes")
    }

    #[test]
    fn unadmitted_relay_kinds_still_drop_silently() {
        // The gateway-dialect lifecycle notices are the ones that would let an
        // approved-but-untrusted Companion declare its own takeover with a
        // fence of its choosing. Admitting any of them is the single change
        // that would make this whole path unsafe.
        let mut harness = RelayHarness::new();
        for kind in [
            "claim_proposal",
            "lease_granted",
            "lease_renewed",
            "lease_revoked",
            "claim_decision",
            "assistance_answer",
            "end_caller_challenge_request",
            "end_caller_confirm",
        ] {
            let encoded = json!({"kind": kind, "schemaVersion": SCHEMA_VERSION, "appId": "app_a"})
                .to_string();
            assert!(
                harness.post(&encoded).is_empty(),
                "{kind} must not be actionable over the relay"
            );
        }
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert!(harness.session.deferred_prepare.is_none());
        assert!(harness.session.leases.is_empty());
    }

    #[test]
    fn every_relay_refusal_returns_ok_and_leaves_no_residue() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Takeover);
        let mut forged = offer.clone();
        forged.offer_token = "forged-token".into();

        let hostile = [
            // Malformed, for every admitted kind.
            json!({"kind": "mobile_offer_answer", "schemaVersion": SCHEMA_VERSION}).to_string(),
            json!({"kind": "lease_request", "schemaVersion": SCHEMA_VERSION}).to_string(),
            json!({"kind": "rtc_signal", "schemaVersion": SCHEMA_VERSION}).to_string(),
            json!({"kind": "lease_heartbeat", "schemaVersion": SCHEMA_VERSION}).to_string(),
            json!({"kind": "lease_revoke", "schemaVersion": SCHEMA_VERSION}).to_string(),
            // A wrong schema version, which the socket treats as fatal.
            json!({"kind": "lease_request", "schemaVersion": 9999}).to_string(),
            // A forged token against a real offer.
            offer_answer(&forged, &harness.device_id, "request_forged"),
            // An offer this plugin never issued.
            offer_answer(
                &SignedPendingMobileOffer {
                    offer: PendingMobileOfferClaims {
                        offer_id: "offer_invented".into(),
                        ..offer.offer.clone()
                    },
                    offer_token: offer.offer_token.clone(),
                },
                &harness.device_id,
                "request_invented",
            ),
            // A lease request naming an offer that was never answered.
            lease_request(&offer, "request_unanswered", "rtc_unanswered"),
            // A heartbeat and a revoke for a lease that does not exist.
            json!({
                "kind": "lease_heartbeat",
                "schemaVersion": SCHEMA_VERSION,
                "appId": "app_a",
                "requestId": "request_ghost",
                "idempotencyKey": "idem_ghost",
                "leaseToken": "not-a-lease"
            })
            .to_string(),
            json!({
                "kind": "lease_revoke",
                "schemaVersion": SCHEMA_VERSION,
                "appId": "app_a",
                "requestId": "request_ghost2",
                "idempotencyKey": "idem_ghost2",
                "leaseToken": "not-a-lease",
                "reason": "cleanup"
            })
            .to_string(),
            "{ this is not json".to_string(),
        ];

        for encoded in &hostile {
            // Returning Ok is the contract: over this carrier the sender is an
            // approved-but-untrusted peer, so one malformed frame must never
            // cost a session that is carrying a live call.
            assert!(harness
                .session
                .handle_relay_peer_frame(
                    encoded,
                    Some(&harness.party.clone()),
                    Some(&harness.device_id),
                    &harness.grants,
                    &harness.media,
                    &harness.radio
                )
                .is_ok());
            // ...and nothing it sent may leave authority behind.
            assert!(harness.session.relay_leases.is_empty(), "{encoded}");
            assert!(harness.session.leases.is_empty(), "{encoded}");
            assert!(harness.session.prepared.is_none(), "{encoded}");
            assert!(harness.session.deferred_prepare.is_none(), "{encoded}");
            assert!(harness.session.peers.is_empty(), "{encoded}");
        }
    }

    #[test]
    fn a_burned_offer_cannot_be_replayed() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Monitor);

        let exact_answer = offer_answer(&offer, &harness.device_id, "request_first");
        let accepted = harness.post(&exact_answer);
        assert!(!accepted.is_empty());
        assert_eq!(
            harness.post(&exact_answer),
            accepted,
            "an ambiguous offer-acceptance commit replays its acceptance"
        );
        // Answering twice is a replay: the same invitation must not be able to
        // start a second claim.
        let refused = harness.answer(&offer, "request_second");
        assert_eq!(harness.rejection(&refused).code, "offer_replayed");

        // Redeeming spends it outright, so even the first answerer cannot go
        // round again on the same offer.
        let granted = harness.post(&lease_request(&offer, "request_first", "rtc_a"));
        assert!(!granted.is_empty());
        let refused = harness.post(&lease_request(&offer, "request_third", "rtc_b"));
        assert!(refused.is_empty(), "a spent offer resolves to nobody");
    }

    #[test]
    fn an_offer_for_another_device_cannot_be_answered() {
        // Even if another approved Companion obtains this frame (for example
        // from a stale pre-projection client or copied logs), only the party
        // and authenticated subject that proved the target device may act on
        // its offer.
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Takeover);
        let frame = offer_answer(&offer, &harness.device_id, "request_impostor");

        let refused = harness.post_as(&frame, Some("mobile:some-other-approved-device"));
        assert_eq!(harness.rejection(&refused).code, "device_unknown");
        assert!(harness.session.relay_leases.is_empty());

        // A carrier that cannot say who sent a frame cannot authorise one
        // either: absent identity fails closed rather than open.
        let refused = harness.post_as(&frame, None);
        assert_eq!(harness.rejection(&refused).code, "device_unknown");

        // The rightful holder is unaffected.
        assert!(!harness.answer(&offer, "request_rightful").is_empty());
    }

    #[test]
    fn monitor_only_admission_never_receives_a_takeover_offer() {
        let mut harness = RelayHarness::with_grants(monitor_relay_grants());
        let offers = harness.publish_offers();

        assert!(
            !offers.is_empty(),
            "state+rtc+monitor still receives its permitted invitation"
        );
        assert!(offers
            .iter()
            .all(|offer| offer.offer.offered_mode == LeaseMode::Monitor));
    }

    #[test]
    fn takeover_requires_resume_aokie_but_exact_holder_can_still_revoke_after_narrowing() {
        let without_resume = HashSet::from([Grant::StateRead, Grant::RtcSignal, Grant::Takeover]);
        let mut narrowed = RelayHarness::with_grants(without_resume.clone());
        assert!(
            narrowed.publish_offers().is_empty(),
            "a device unable to execute mandatory failback receives no takeover invitation"
        );

        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Takeover);
        assert!(offer.offer.required_grants.contains(&Grant::ResumeAokie));
        let refused = harness.post_with_grants(
            &offer_answer(&offer, &harness.device_id, "request_missing_resume"),
            &without_resume,
        );
        assert_eq!(harness.rejection(&refused).code, "grant_required");

        // Revoke is strictly de-escalating and remains available to the exact
        // authenticated holder even when current admission no longer grants
        // the mode (or ResumeAokie). It must never be trapped behind the gate
        // whose purpose is to prevent gaining authority.
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_revoke_narrowed");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);
        let revoke = json!({
            "kind": "lease_revoke",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_revoke_narrowed_now",
            "idempotencyKey": "idem_revoke_narrowed_now",
            "leaseToken": status.lease_token,
            "reason": "permission_removed"
        })
        .to_string();
        assert!(harness
            .session
            .handle_relay_peer_frame(
                &revoke,
                Some(&harness.party),
                Some(&harness.device_id),
                &without_resume,
                &harness.media,
                &harness.radio,
            )
            .unwrap()
            .is_empty());
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
    }

    #[test]
    fn authenticated_grants_not_payload_mode_control_takeover_redemption() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Takeover);
        let monitor_grants = monitor_relay_grants();

        // Even changing the peer-controlled offeredMode to one the admission
        // does grant cannot downgrade the authority check: the plugin uses the
        // mode in the offer it signed.
        let mut spoofed_answer: Value = serde_json::from_str(&offer_answer(
            &offer,
            &harness.device_id,
            "request_grant_spoof",
        ))
        .unwrap();
        spoofed_answer["offeredMode"] = json!("monitor");
        let refused = harness.post_with_grants(&spoofed_answer.to_string(), &monitor_grants);
        assert_eq!(harness.rejection(&refused).code, "grant_required");
        assert!(
            !harness
                .session
                .relay_offers
                .contains_key(&offer.offer.offer_id),
            "current admission narrowing retires the now-unauthorized offer"
        );

        // Exercise the lease request independently with a fresh, fully
        // admitted peer. Broadening is intentionally re-hello-only, so the
        // narrowed session above must not silently regain Takeover here.
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Takeover);
        let accepted = harness.answer(&offer, "request_grant_spoof");
        assert!(!accepted.is_empty());

        // The lease request is checked independently. Its payload can also
        // claim monitor, but the accepted signed offer remains takeover.
        let mut spoofed_request: Value = serde_json::from_str(&lease_request(
            &offer,
            "request_grant_spoof",
            "rtc_grant_spoof",
        ))
        .unwrap();
        spoofed_request["mode"] = json!("monitor");
        let refused = harness.post_with_grants(&spoofed_request.to_string(), &monitor_grants);
        assert_eq!(harness.rejection(&refused).code, "grant_required");
        assert!(harness.session.relay_leases.is_empty());
    }

    #[test]
    fn rtc_and_heartbeat_recheck_and_apply_current_authenticated_grants() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_grant_rechecks");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);

        let missing_rtc = HashSet::from([Grant::StateRead, Grant::Monitor]);
        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_grant_rechecks_beat",
            "idempotencyKey": "idem_grant_rechecks_beat",
            "leaseToken": status.lease_token
        })
        .to_string();
        let refused = harness.post_with_grants(&heartbeat, &missing_rtc);
        assert_eq!(harness.rejection(&refused).code, "grant_required");
        assert!(
            harness.session.relay_leases.is_empty(),
            "losing a grant on this frame revokes the active lease immediately"
        );
        assert!(harness.session.leases.is_empty());

        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_grant_rechecks_rtc");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);
        let missing_mode = HashSet::from([Grant::StateRead, Grant::RtcSignal]);
        let rtc = serde_json::to_string(&MobileRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: status.lease.app_id.clone(),
            signal_id: "signal_grant_recheck".into(),
            plugin_id: status.lease.plugin_id.clone(),
            device_id: status.lease.device_id.clone(),
            lease_token: status.lease_token.clone(),
            lease_jti: status.lease.jti.clone(),
            rtc_session_id: status.lease.rtc_session_id.clone(),
            sdp_revision: 1,
            transport_generation: 1,
            call_id: status.lease.call_id.clone(),
            call_epoch: status.lease.call_epoch,
            owner_epoch: status.lease.owner_epoch,
            fence: status.lease.fence,
            signal: RtcSignal::Close {
                reason: "done".into(),
            },
        })
        .unwrap();
        let refused = harness.post_with_grants(&rtc, &missing_mode);
        assert_eq!(harness.rejection(&refused).code, "grant_required");
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
    }

    #[test]
    fn grant_narrowing_during_a_prepared_takeover_immediately_returns_the_caller() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_takeover_narrowed");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);
        assert!(harness.session.prepared.is_some());
        assert_eq!(harness.session.leases.len(), 1);

        // This exact frame's authenticated admission has lost Takeover. The
        // plugin must unwind the prepared/soft-hold path before even deciding
        // whether a heartbeat would otherwise be renewable.
        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_takeover_narrowed_beat",
            "idempotencyKey": "idem_takeover_narrowed_beat",
            "leaseToken": status.lease_token
        })
        .to_string();
        let without_takeover = monitor_relay_grants();
        let refused = harness.post_with_grants(&heartbeat, &without_takeover);
        assert_eq!(harness.rejection(&refused).code, "grant_required");
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert!(harness.session.deferred_prepare.is_none());
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive,
            "permission loss must never leave a caller in soft hold"
        );
    }

    #[test]
    fn lease_request_that_misses_physical_epochs_is_refused_stale_call() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Takeover);
        assert!(!harness.answer(&offer, "request_stale").is_empty());

        // The call moves under the claim, exactly as a real one does.
        harness.move_call("call_b");

        let request = lease_request(&offer, "request_stale", "rtc_a");
        let refused = harness.post(&request);
        assert_eq!(harness.rejection(&refused).code, "stale_call");
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.deferred_prepare.is_none());
        assert_eq!(
            harness.post(&request),
            refused,
            "a dropped terminal refusal replays byte-for-byte after the offer was spent"
        );
    }

    #[test]
    fn takeover_without_consent_is_refused_consent_required() {
        let mut gate = consenting_gate();
        gate.takeover_enabled = false;
        let mut harness = RelayHarness::with_consent(gate);

        // Withdrawn consent stops the invitation being made at all...
        assert!(harness
            .publish_offers()
            .iter()
            .all(|offer| offer.offer.offered_mode != LeaseMode::Takeover));

        // ...and a request built from an offer minted while consent still held
        // is refused on its own merits, because consent is re-read at claim
        // time rather than trusted from the offer.
        let monitor = harness.offer_for(LeaseMode::Monitor);
        let mut escalated = monitor.clone();
        escalated.offer.offered_mode = LeaseMode::Takeover;
        assert!(!harness.answer(&monitor, "request_escalate").is_empty());
        let refused = harness.post(&lease_request(&escalated, "request_escalate", "rtc_a"));
        // The accepted offer allowed monitor, so the escalation is refused
        // before consent is even consulted.
        assert_eq!(harness.rejection(&refused).code, "mode_unavailable");
        assert!(harness.session.relay_leases.is_empty());
    }

    #[test]
    fn consult_without_a_matching_assistance_fence_is_refused() {
        // A private consultation exists to answer a question Aokie asked. With
        // no current assistance request there is nothing to consult about.
        let mut harness = RelayHarness::new();
        assert!(harness
            .publish_offers()
            .iter()
            .all(|offer| offer.offer.offered_mode != LeaseMode::Consult));

        let monitor = harness.offer_for(LeaseMode::Monitor);
        let mut consult = monitor.clone();
        consult.offer.offered_mode = LeaseMode::Consult;
        assert!(!harness.answer(&monitor, "request_consult").is_empty());
        let refused = harness.post(&lease_request(&consult, "request_consult", "rtc_a"));
        assert_eq!(harness.rejection(&refused).code, "mode_unavailable");
        assert!(harness.session.relay_leases.is_empty());
    }

    #[test]
    fn a_second_claimant_is_refused_claimant_busy() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_first");
        assert_eq!(
            harness.granted(&granted).status,
            PluginLeaseStatus::Provisional
        );

        // Two prepared claims would race for the same physical route, and the
        // loser's soft hold would sit on a caller the winner is already taking.
        let second = harness.offer_for(LeaseMode::Monitor);
        assert!(!harness.answer(&second, "request_second").is_empty());
        let refused = harness.post(&lease_request(&second, "request_second", "rtc_b"));
        assert_eq!(harness.rejection(&refused).code, "claimant_busy");
        assert_eq!(harness.session.relay_leases.len(), 1);
    }

    #[test]
    fn minted_takeover_leases_carry_a_strictly_increasing_positive_fence() {
        let mut harness = RelayHarness::new();
        let first = harness.claim(LeaseMode::Takeover, "request_fence_1");
        let first = harness.granted(&first).lease;
        assert!(first.fence > 0, "a talk-capable lease needs a real fence");

        // Clear the claimant slot the way a revoke does, then claim again.
        harness.release_claimant();

        let second = harness.claim(LeaseMode::Takeover, "request_fence_2");
        let second = harness.granted(&second).lease;
        assert!(
            second.fence > first.fence,
            "a replayed older fence must never look current: {} then {}",
            first.fence,
            second.fence
        );

        // Non-talk modes carry no fence at all, which is what stops one being
        // mistaken for caller-bound authority.
        harness.release_claimant();
        let monitor = harness.claim(LeaseMode::Monitor, "request_fence_3");
        assert_eq!(harness.granted(&monitor).lease.fence, 0);
    }

    #[test]
    fn minted_leases_pass_their_own_validate_and_never_exceed_the_local_cap() {
        let mut harness = RelayHarness::new();
        let now = unix_now().unwrap();

        let prepared = harness.claim(LeaseMode::Takeover, "request_ttl_1");
        let prepared = harness.granted(&prepared);
        prepared
            .lease
            .validate(now)
            .expect("a minted lease survives the checks a received one faces");
        assert_eq!(prepared.status, PluginLeaseStatus::Provisional);
        assert_eq!(prepared.lease.phase, LeasePhase::Prepared);
        // Short on purpose: this is the bound on how long a caller can sit in
        // soft hold waiting for a handover that is not arriving.
        assert!(prepared.lease.expires_at <= now + RELAY_PREPARED_LEASE_TTL);
        assert!(prepared.lease.expires_at <= now + 300, "local safety cap");
        assert_eq!(prepared.lease.session_nonce, "mobile_session_device_a");
        assert_eq!(
            prepared.lease.plugin_key_thumbprint,
            harness.session.endpoint_authority.endpoint_key.thumbprint
        );

        harness.release_claimant();

        let monitor = harness.claim(LeaseMode::Monitor, "request_ttl_2");
        let monitor = harness.granted(&monitor);
        monitor.lease.validate(now).expect("monitor lease is valid");
        assert_eq!(monitor.status, PluginLeaseStatus::Granted);
        assert_eq!(monitor.lease.phase, LeasePhase::Active);
        assert!(monitor.lease.expires_at <= now + RELAY_ACTIVE_LEASE_TTL);
    }

    #[test]
    fn prepared_leases_are_never_renewable() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_renew");
        let status = harness.granted(&granted);
        let token = status.lease_token.clone();
        harness.settle(&granted, TransportDelivery::Delivered);

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_renew_beat",
            "idempotencyKey": "idem_renew_beat",
            "leaseToken": token
        })
        .to_string();

        // Refusing renewal is what makes a stuck prepare impossible to hold
        // open: it must complete inside its short life or hand the caller back.
        let refused = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&refused).code, "phase_not_renewable");

        let stored = harness
            .session
            .relay_leases
            .values()
            .next()
            .expect("the prepared lease is still recorded");
        assert_eq!(stored.phase, LeasePhase::Prepared);
        assert_eq!(stored.current_jti, status.lease.jti);
    }

    #[test]
    fn silent_prepared_lease_expiry_retires_media_and_frees_the_claimant() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_silent_prepare_expiry");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);
        assert!(harness.session.prepared.is_some());

        assert_eq!(
            harness
                .session
                .expire_relay_leases(status.lease.expires_at, &harness.media),
            1
        );
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive
        );

        let replacement = harness.claim(
            LeaseMode::Takeover,
            "request_silent_prepare_expiry_replacement",
        );
        assert_eq!(
            harness.granted(&replacement).status,
            PluginLeaseStatus::Provisional
        );
    }

    #[test]
    fn a_deferred_prepare_is_not_armed_until_the_grant_frame_is_sent() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_defer");
        assert_eq!(
            harness.granted(&granted).status,
            PluginLeaseStatus::Provisional
        );

        // The grant has been BUILT but not sent. Nothing may be armed yet: an
        // armed claim puts the plugin on the path to soft-holding a live caller
        // on behalf of a device that has not been told it won, so a relay stall
        // here would silence the caller for a handover nobody is completing.
        assert!(harness.session.prepared.is_none());
        assert!(harness.session.deferred_prepare.is_some());
        assert!(harness.session.leases.is_empty());

        // The worker loop commits only after the transport accepted the frame.
        harness.session.finish_relay_delivery(
            &granted[0],
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert!(harness.session.deferred_prepare.is_none());
        assert!(
            harness.session.prepared.is_some(),
            "the claim arms once the device has actually been told"
        );
        assert_eq!(harness.session.leases.len(), 1);
    }

    #[test]
    fn a_deferred_prepare_cannot_arm_after_the_switchboard_revision_moves() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_defer_switch");
        let expected = harness
            .session
            .deferred_prepare
            .as_ref()
            .expect("the provisional grant is delivery-gated")
            .expected_switchboard_revision;
        harness.status.switchboard_revision.store(
            expected.saturating_add(1),
            std::sync::atomic::Ordering::Relaxed,
        );

        harness.session.finish_relay_delivery(
            &granted[0],
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert!(harness.session.deferred_prepare.is_none());
        assert!(harness.session.prepared.is_none());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
    }

    #[test]
    fn a_dropped_provisional_grant_never_arms_or_blocks_the_caller() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_dropped_grant");
        assert_eq!(
            harness.granted(&granted).status,
            PluginLeaseStatus::Provisional
        );
        let lease_id = harness
            .session
            .deferred_prepare
            .as_ref()
            .expect("the provisional grant is waiting on delivery")
            .lease_id
            .clone();

        // A terminal relay 429 and a missing target are non-fatal carrier
        // drops. Neither is permission to soft-hold a live caller.
        harness.session.finish_relay_delivery(
            &granted[0],
            TransportDelivery::Dropped,
            &harness.media,
            &harness.radio,
        );

        assert!(harness.session.deferred_prepare.is_none());
        assert!(harness.session.prepared.is_none());
        assert!(harness.session.leases.is_empty());
        assert!(!harness.session.relay_leases.contains_key(&lease_id));
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive,
            "the caller never left the receptionist"
        );
    }

    #[test]
    fn a_dropped_monitor_grant_leaves_no_hidden_authority_and_can_retry() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Monitor);
        assert!(!harness.answer(&offer, "request_monitor_drop").is_empty());
        let request = lease_request(&offer, "request_monitor_drop", "rtc_monitor_drop");
        let granted = harness.post(&request);
        assert_eq!(harness.granted(&granted).status, PluginLeaseStatus::Granted);
        assert!(harness.session.leases.is_empty());
        assert_eq!(harness.session.relay_leases.len(), 1);

        harness.session.finish_relay_delivery(
            &granted[0],
            TransportDelivery::Dropped,
            &harness.media,
            &harness.radio,
        );

        assert!(harness.session.pending_relay_status.is_none());
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        let retry = harness.post(&request);
        assert!(
            !retry.is_empty(),
            "the accepted offer is restored for the exact retry"
        );
        assert_eq!(harness.session.relay_leases.len(), 1);
    }

    #[test]
    fn a_dropped_relay_snapshot_is_retried_at_normal_poll_cadence() {
        let mut harness = RelayHarness::new();
        harness.session.last_snapshot_fingerprint = None;
        harness.session.last_snapshot_sent = None;
        let encoded = harness
            .session
            .snapshot_frame(&harness.radio)
            .expect("snapshot builds")
            .expect("live call publishes a snapshot");
        assert!(harness.session.last_snapshot_fingerprint.is_some());
        assert!(harness.session.last_snapshot_sent.is_some());

        let settled_at = Instant::now();
        harness.session.finish_relay_delivery(
            &encoded,
            TransportDelivery::Dropped,
            &harness.media,
            &harness.radio,
        );

        assert!(harness.session.last_snapshot_fingerprint.is_none());
        assert!(harness.session.last_snapshot_sent.is_none());
        assert!(
            harness.session.next_snapshot_poll > settled_at,
            "a dropped receipt is owed again without a hot retry loop"
        );
        assert!(
            harness.session.next_snapshot_poll
                <= settled_at + SNAPSHOT_POLL + Duration::from_millis(20)
        );
    }

    #[test]
    fn relay_snapshots_are_targeted_and_projected_from_each_devices_grants() {
        let mut harness = RelayHarness::new();
        harness.session.last_snapshot_fingerprint = None;
        harness.session.last_snapshot_sent = None;
        let encoded = harness
            .session
            .snapshot_frame(&harness.radio)
            .expect("snapshot builds")
            .expect("live call publishes a snapshot");
        let mut raw: PluginSnapshotFrame =
            serde_json::from_str(&encoded).expect("raw snapshot decodes");
        assert!(
            raw.device_id.is_none(),
            "the internal snapshot is untargeted"
        );
        assert!(
            !raw.snapshot.pending_mobile_offers.is_empty(),
            "the raw relay snapshot carries invitations for projection"
        );
        raw.snapshot.caller = Some(CallerProjection {
            label: Some("Alice".into()),
            masked_number: Some("*******5678".into()),
        });
        raw.snapshot.captions = vec![Caption {
            caption_id: "caption_a".into(),
            speaker: "caller".into(),
            text: "my private appointment details".into(),
            occurred_at: "2026-07-19T00:00:00Z".into(),
            final_text: true,
        }];
        let encoded = serde_json::to_string(&raw).unwrap();

        // State alone can render the call shell, but it cannot reveal caller
        // identity, captions, audio telemetry or invitations to claim media.
        harness
            .session
            .relay_peers
            .get_mut(&harness.device_id)
            .unwrap()
            .grants = HashSet::from([Grant::StateRead]);
        let projected = harness.session.relay_project_snapshot(&encoded).unwrap();
        assert_eq!(projected.len(), 1);
        let redacted: PluginSnapshotFrame = serde_json::from_str(&projected[0]).unwrap();
        assert_eq!(
            redacted.device_id.as_deref(),
            Some(harness.device_id.as_str())
        );
        assert!(redacted.snapshot.caller.is_none());
        assert!(redacted.snapshot.captions.is_empty());
        assert!(redacted.snapshot.audio_levels.is_none());
        assert!(redacted.snapshot.pending_mobile_offers.is_empty());

        // Adding only the monitor/caller/caption grants reveals those exact
        // fields and only monitor invitations. Takeover and consult offers for
        // the same device remain outside this projection.
        harness
            .session
            .relay_peers
            .get_mut(&harness.device_id)
            .unwrap()
            .grants = HashSet::from([
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Monitor,
            Grant::CallerRead,
            Grant::CaptionsRead,
        ]);
        let projected = harness.session.relay_project_snapshot(&encoded).unwrap();
        let permitted: PluginSnapshotFrame = serde_json::from_str(&projected[0]).unwrap();
        assert_eq!(permitted.snapshot.caller, raw.snapshot.caller);
        assert_eq!(permitted.snapshot.captions, raw.snapshot.captions);
        assert!(
            !permitted.snapshot.pending_mobile_offers.is_empty(),
            "the authorized monitor invitation remains"
        );
        assert!(permitted
            .snapshot
            .pending_mobile_offers
            .iter()
            .all(|offer| offer.offer.target_device_id == harness.device_id
                && offer.offer.offered_mode == LeaseMode::Monitor));
    }

    #[test]
    fn relay_assistance_waits_for_every_eligible_devices_current_snapshot_and_retries_drops() {
        let mut harness = RelayHarness::new();
        let grants = HashSet::from([Grant::StateRead, Grant::AssistanceRead]);
        harness
            .session
            .relay_peers
            .get_mut(&harness.device_id)
            .unwrap()
            .grants = grants.clone();
        harness.session.relay_peers.insert(
            "device_b".into(),
            RelayPeer {
                holder_key_thumbprint: "thumbprint_b".into(),
                session_nonce: "mobile_session_b".into(),
                grants,
            },
        );
        harness.session.relay_snapshot_event_id = Some("snapshot_current".into());
        assert!(!harness.session.relay_assistance_snapshot_ready());
        harness
            .session
            .relay_snapshot_delivered_devices
            .insert(harness.device_id.clone());
        assert!(
            !harness.session.relay_assistance_snapshot_ready(),
            "one successful projection cannot expose context before the other party has state"
        );
        harness
            .session
            .relay_snapshot_delivered_devices
            .insert("device_b".into());
        assert!(harness.session.relay_assistance_snapshot_ready());

        let encoded = json!({
            "kind": "assistance_request",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "eventId": "assistance_event_a",
            "requestId": "assistance_request_a",
            "callId": "call_a",
            "callEpoch": 1,
            "ownerEpoch": 0,
            "switchboardRevision": 0,
            "remoteRevision": 1,
            "question": "Can the manager help?",
            "expiresAt": unix_now().unwrap() + 60
        })
        .to_string();
        harness.session.finish_relay_delivery(
            &encoded,
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert_eq!(
            harness.session.last_assistance_request_sent.as_deref(),
            Some("assistance_request_a")
        );
        harness.session.finish_relay_delivery(
            &encoded,
            TransportDelivery::Dropped,
            &harness.media,
            &harness.radio,
        );
        assert!(
            harness.session.last_assistance_request_sent.is_none(),
            "a dropped aggregate delivery remains owed and retryable"
        );
    }

    #[test]
    fn an_rtc_signal_whose_lease_token_was_not_minted_here_is_refused() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_rtc");
        let status = harness.granted(&granted);

        let plugin_id = harness.session.plugin_id.clone();
        let device_id = harness.device_id.clone();
        let signal = |lease_token: &str, lease_jti: &str| {
            json!({
                "kind": "rtc_signal",
                "schemaVersion": SCHEMA_VERSION,
                "appId": "app_a",
                "signalId": "signal_a",
                "pluginId": plugin_id,
                "deviceId": device_id,
                "leaseToken": lease_token,
                "leaseJti": lease_jti,
                "rtcSessionId": "rtc_a",
                "sdpRevision": 1,
                "transportGeneration": 1,
                "callId": "call_a",
                "callEpoch": status.lease.call_epoch,
                "ownerEpoch": status.lease.owner_epoch,
                "fence": status.lease.fence,
                "signal": {"type": "close", "reason": "done"}
            })
            .to_string()
        };

        // A token this plugin never minted names no lease it will honour.
        let refused = harness.post(&signal("forged-token", &status.lease.jti));
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");

        // The right token against the wrong lease identity is equally unknown.
        let refused = harness.post(&signal(&status.lease_token, "leasejti_invented"));
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");

        // ...and no peer was opened by any of it.
        assert!(harness.session.peers.is_empty());
    }

    #[test]
    fn an_exact_rtc_redelivery_is_a_noop_but_changed_content_is_rejected() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_rtc_replay");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);

        // A close is enough to exercise the real accepted-signal path without
        // asking the native test peer to negotiate SDP. The first copy removes
        // the exact route; the second can only succeed through replay state.
        let binding = binding_for_claims(&status.lease);
        harness.session.peers.insert(
            status.lease.rtc_session_id.clone(),
            PeerRoute {
                binding,
                lease_jti: status.lease.jti.clone(),
                device_id: status.lease.device_id.clone(),
                sdp_revision: 1,
                transport_generation: 1,
                lease_ttl_ms: 20_000,
                connected: true,
                remote_audio_ready: false,
                remote_microphone_ready: false,
                transition_requested: false,
            },
        );
        let signal = |reason: &str| {
            serde_json::to_string(&MobileRtcSignalFrame {
                kind: "rtc_signal".into(),
                schema_version: SCHEMA_VERSION,
                app_id: status.lease.app_id.clone(),
                signal_id: "signal_replayed_close".into(),
                plugin_id: status.lease.plugin_id.clone(),
                device_id: status.lease.device_id.clone(),
                lease_token: status.lease_token.clone(),
                lease_jti: status.lease.jti.clone(),
                rtc_session_id: status.lease.rtc_session_id.clone(),
                sdp_revision: 1,
                transport_generation: 1,
                call_id: status.lease.call_id.clone(),
                call_epoch: status.lease.call_epoch,
                owner_epoch: status.lease.owner_epoch,
                fence: status.lease.fence,
                signal: RtcSignal::Close {
                    reason: reason.into(),
                },
            })
            .expect("RTC close encodes")
        };

        let exact = signal("done");
        assert!(harness.post(&exact).is_empty());
        assert!(harness.session.peers.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert!(
            harness.post(&exact).is_empty(),
            "the exact redelivery is accepted without running Close twice"
        );

        let changed = harness.post(&signal("different reason"));
        assert_eq!(harness.rejection(&changed).code, "duplicate_signal");
        assert!(harness.session.peers.is_empty());

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_after_rtc_close",
            "idempotencyKey": "idem_after_rtc_close",
            "leaseToken": status.lease_token
        })
        .to_string();
        let after_close = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&after_close).code, "lease_unknown");
        let replacement = harness.claim(LeaseMode::Monitor, "request_after_close_replacement");
        assert_eq!(
            harness.granted(&replacement).status,
            PluginLeaseStatus::Granted
        );
    }

    #[test]
    fn terminal_rtc_failure_retires_prepared_takeover_and_replays_the_refusal() {
        let mut harness = RelayHarness::new();
        let provisional_frames = harness.claim(LeaseMode::Takeover, "request_rtc_terminal");
        let provisional = harness.granted(&provisional_frames);
        harness.settle(&provisional_frames, TransportDelivery::Delivered);
        assert!(harness.session.prepared.is_some());

        let provisional_binding = binding_for_claims(&provisional.lease);
        harness
            .media
            .install_test_prepared_peer(provisional_binding.clone(), 10_000)
            .unwrap();
        harness
            .media
            .ack_prepare_human(&provisional_binding)
            .unwrap();
        let active_frames = harness
            .session
            .drain_media_events(&harness.media, &harness.radio)
            .unwrap();
        let active_encoded = active_frames
            .iter()
            .find(|encoded| {
                serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                    .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
            })
            .unwrap()
            .clone();
        let status: PluginLeaseStatusFrame = serde_json::from_str(&active_encoded).unwrap();
        harness.session.finish_relay_delivery(
            &active_encoded,
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert_eq!(status.lease.phase, LeasePhase::Active);

        // An active-lease Close before the exact native peer exists reaches
        // the authenticated RTC handler but cannot be applied. That is
        // terminal for this lease:
        // it must not remain heartbeat-renewable after its media transaction
        // failed.
        let signal = serde_json::to_string(&MobileRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: status.lease.app_id.clone(),
            signal_id: "signal_terminal_without_peer".into(),
            plugin_id: status.lease.plugin_id.clone(),
            device_id: status.lease.device_id.clone(),
            lease_token: status.lease_token.clone(),
            lease_jti: status.lease.jti.clone(),
            rtc_session_id: status.lease.rtc_session_id.clone(),
            sdp_revision: 2,
            transport_generation: 2,
            call_id: status.lease.call_id.clone(),
            call_epoch: status.lease.call_epoch,
            owner_epoch: status.lease.owner_epoch,
            fence: status.lease.fence,
            signal: RtcSignal::Close {
                reason: "permission_failed".into(),
            },
        })
        .unwrap();
        let refused = harness.post(&signal);
        assert_eq!(refused.len(), 2);
        assert!(serde_json::from_str::<PluginLeaseRevokeFrame>(&refused[0]).is_ok());
        assert!(serde_json::from_str::<PluginClaimRejectedFrame>(&refused[1]).is_ok());
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");
        let revocations = refused
            .iter()
            .filter_map(|encoded| serde_json::from_str::<PluginLeaseRevokeFrame>(encoded).ok())
            .collect::<Vec<_>>();
        assert_eq!(revocations.len(), 1);
        let revocation = &revocations[0];
        assert_eq!(revocation.device_id, status.lease.device_id);
        assert_eq!(revocation.lease_id, status.lease.lease_id);
        assert_eq!(revocation.lease_jti, status.lease.jti);
        assert_eq!(revocation.call_id, status.lease.call_id);
        assert_eq!(revocation.call_epoch, status.lease.call_epoch);
        assert_eq!(revocation.fence, status.lease.fence);
        assert_eq!(revocation.reason, "terminal_rtc_failure");

        let replay = harness.post(&signal);
        assert_eq!(replay, refused);
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive
        );
    }

    #[test]
    fn terminal_rtc_failure_retries_dropped_exact_revoke_egress_until_delivered() {
        let mut harness = RelayHarness::new();
        let provisional_frames = harness.claim(LeaseMode::Takeover, "request_exact_rtc_failure");
        let provisional = harness.granted(&provisional_frames);
        harness.settle(&provisional_frames, TransportDelivery::Delivered);
        let binding = binding_for_claims(&provisional.lease);
        harness
            .media
            .install_test_prepared_peer(binding.clone(), 10_000)
            .unwrap();
        harness.session.peers.insert(
            provisional.lease.rtc_session_id.clone(),
            PeerRoute {
                binding,
                lease_jti: provisional.lease.jti.clone(),
                device_id: provisional.lease.device_id.clone(),
                sdp_revision: 1,
                transport_generation: 1,
                lease_ttl_ms: 10_000,
                connected: false,
                remote_audio_ready: false,
                remote_microphone_ready: false,
                transition_requested: false,
            },
        );
        harness
            .media
            .close_peer(
                &provisional.lease.rtc_session_id,
                "simulate signalling endpoint loss",
            )
            .unwrap();
        let signal = signed_mobile_ice(
            &provisional.lease,
            &provisional.lease_token,
            "signal_exact_rtc_failure",
            "candidate_jti_exact_rtc_failure",
            1,
            1,
        );

        // The terminal transition executes once and returns the exact revoke
        // first. Model the relay rejecting that outbound POST: no inbound
        // redelivery from the phone is involved in the retry below.
        let failed = harness.post(&signal);
        assert_eq!(failed.len(), 2);
        assert!(serde_json::from_str::<PluginLeaseRevokeFrame>(&failed[0]).is_ok());
        assert!(serde_json::from_str::<PluginClaimRejectedFrame>(&failed[1]).is_ok());
        assert_eq!(harness.rejection(&failed).code, "lease_unknown");
        let revocations = failed
            .iter()
            .filter_map(|encoded| serde_json::from_str::<PluginLeaseRevokeFrame>(encoded).ok())
            .collect::<Vec<_>>();
        assert_eq!(revocations.len(), 1);
        let revocation = &revocations[0];
        assert_eq!(revocation.device_id, provisional.lease.device_id);
        assert_eq!(revocation.lease_id, provisional.lease.lease_id);
        assert_eq!(revocation.lease_jti, provisional.lease.jti);
        assert_eq!(revocation.call_id, provisional.lease.call_id);
        assert_eq!(revocation.call_epoch, provisional.lease.call_epoch);
        assert_eq!(revocation.fence, provisional.lease.fence);
        assert_eq!(revocation.reason, "terminal_rtc_failure");
        assert!(harness.session.peers.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());

        harness.settle(&failed, TransportDelivery::Dropped);
        assert_eq!(harness.session.pending_relay_revocations.len(), 1);
        let due = harness
            .session
            .due_pending_relay_revocations(Instant::now() + SNAPSHOT_POLL);
        assert_eq!(due, vec![failed[0].clone()]);
        harness.session.prepare_relay_delivery(&due[0]);
        harness.session.finish_relay_delivery(
            &due[0],
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert!(harness.session.pending_relay_revocations.is_empty());

        // A later inbound redelivery is still answered byte-for-byte without
        // re-running teardown. If that replayed revoke is itself dropped, the
        // same egress ledger re-arms it and clears only on exact delivery.
        let replay = harness.post(&signal);
        assert_eq!(replay, failed);
        assert!(harness.session.peers.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
        harness.settle(&replay, TransportDelivery::Dropped);
        let replay_due = harness
            .session
            .due_pending_relay_revocations(Instant::now() + SNAPSHOT_POLL);
        assert_eq!(replay_due, vec![failed[0].clone()]);
        harness.session.prepare_relay_delivery(&replay_due[0]);
        harness.session.finish_relay_delivery(
            &replay_due[0],
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert!(harness.session.pending_relay_revocations.is_empty());
    }

    #[test]
    fn terminal_rtc_failure_never_revokes_a_foreign_peer_named_by_the_frame() {
        let mut harness = RelayHarness::new();
        let provisional_frames = harness.claim(LeaseMode::Takeover, "request_foreign_rtc_failure");
        let provisional = harness.granted(&provisional_frames);
        harness.settle(&provisional_frames, TransportDelivery::Delivered);

        let foreign_rtc = "rtc_foreign".to_string();
        let mut foreign_binding = binding_for_claims(&provisional.lease);
        foreign_binding.rtc_session_id = foreign_rtc.clone();
        foreign_binding.device_id = "device_foreign".into();
        foreign_binding.lease_id = Some("lease_foreign".into());
        harness.session.peers.insert(
            foreign_rtc.clone(),
            PeerRoute {
                binding: foreign_binding,
                lease_jti: "lease_jti_foreign".into(),
                device_id: "device_foreign".into(),
                sdp_revision: 1,
                transport_generation: 1,
                lease_ttl_ms: 10_000,
                connected: true,
                remote_audio_ready: true,
                remote_microphone_ready: false,
                transition_requested: false,
            },
        );
        let signal = serde_json::to_string(&MobileRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: provisional.lease.app_id.clone(),
            signal_id: "signal_foreign_rtc_failure".into(),
            plugin_id: provisional.lease.plugin_id.clone(),
            device_id: provisional.lease.device_id.clone(),
            lease_token: provisional.lease_token.clone(),
            lease_jti: provisional.lease.jti.clone(),
            rtc_session_id: foreign_rtc.clone(),
            sdp_revision: 1,
            transport_generation: 1,
            call_id: provisional.lease.call_id.clone(),
            call_epoch: provisional.lease.call_epoch,
            owner_epoch: provisional.lease.owner_epoch,
            fence: provisional.lease.fence,
            signal: RtcSignal::Close {
                reason: "foreign route probe".into(),
            },
        })
        .unwrap();

        let failed = harness.post(&signal);
        assert_eq!(failed.len(), 2);
        assert!(serde_json::from_str::<PluginLeaseRevokeFrame>(&failed[0]).is_ok());
        assert!(serde_json::from_str::<PluginClaimRejectedFrame>(&failed[1]).is_ok());
        assert_eq!(harness.rejection(&failed).code, "lease_unknown");
        let revocation = failed
            .iter()
            .find_map(|encoded| serde_json::from_str::<PluginLeaseRevokeFrame>(encoded).ok())
            .expect("the authenticated lease gets an exact terminal notice");
        assert_eq!(revocation.device_id, provisional.lease.device_id);
        assert_eq!(revocation.lease_id, provisional.lease.lease_id);
        assert_eq!(revocation.lease_jti, provisional.lease.jti);
        assert_ne!(revocation.device_id, "device_foreign");
        assert_ne!(revocation.lease_id, "lease_foreign");
        assert!(
            harness.session.peers.contains_key(&foreign_rtc),
            "an authenticated device can retire only its recognised lease, never a peer named by untrusted rtcSessionId"
        );
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
    }

    #[test]
    fn exact_late_prepared_ice_is_replay_fenced_and_cannot_touch_the_active_lease() {
        let mut harness = RelayHarness::new();
        let (prepared, active) =
            activate_takeover_with_retired_prepared_rtc(&mut harness, "request_late_prepared_ice");
        let current_before = harness
            .session
            .relay_leases
            .get(&active.lease.lease_id)
            .expect("the active stable lease is current")
            .clone();
        let active_claims_before = harness
            .session
            .leases
            .get(&active.lease.jti)
            .expect("active authority is committed")
            .clone();
        let peer_count_before = harness.session.peers.len();
        let signal = signed_mobile_ice(
            &prepared.lease,
            &prepared.lease_token,
            "signal_late_prepared_ice",
            "candidate_jti_late_prepared_ice",
            1,
            1,
        );

        assert!(
            harness.post(&signal).is_empty(),
            "an exact valid late candidate is consumed as a no-op"
        );
        assert_eq!(
            harness
                .session
                .relay_leases
                .get(&active.lease.lease_id)
                .expect("the current lease survives")
                .current_jti,
            current_before.current_jti
        );
        assert_eq!(
            harness
                .session
                .relay_leases
                .get(&active.lease.lease_id)
                .expect("the current lease survives")
                .token,
            current_before.token
        );
        assert_eq!(
            harness.session.leases.get(&active.lease.jti),
            Some(&active_claims_before)
        );
        assert_eq!(harness.session.leases.len(), 1);
        assert_eq!(harness.session.peers.len(), peer_count_before);
        let charged = harness
            .session
            .relay_rtc_signal_budget
            .get(&harness.device_id)
            .expect("the first late signal used the RTC lane")
            .1;
        assert!(
            harness.post(&signal).is_empty(),
            "an at-least-once redelivery is also a no-op"
        );
        assert_eq!(
            harness
                .session
                .relay_rtc_signal_budget
                .get(&harness.device_id)
                .unwrap()
                .1,
            charged,
            "replay lookup happens before the RTC lane is charged"
        );

        let revoke = json!({
            "kind": "lease_revoke",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_late_prepared_ice_revoke",
            "idempotencyKey": "idem_late_prepared_ice_revoke",
            "leaseToken": active.lease_token,
            "reason": "operator_return"
        })
        .to_string();
        assert!(harness.post(&revoke).is_empty());
        assert!(
            harness.session.retired_prepared_rtc.is_empty(),
            "stable-lease revoke also clears its old-generation tombstone"
        );
        assert!(harness.session.relay_replays.values().all(|replay| {
            !matches!(
                &replay.result,
                RelayReplayResult::RetiredPreparedRtcDropped { .. }
            )
        }));
    }

    #[test]
    fn altered_or_expired_prepared_rtc_bindings_are_never_tombstone_authorized() {
        let mut harness = RelayHarness::new();
        let (prepared, active) = activate_takeover_with_retired_prepared_rtc(
            &mut harness,
            "request_retired_binding_limits",
        );
        let current_jti = active.lease.jti.clone();
        let current_token = active.lease_token.clone();

        // Even the approved mobile signer cannot widen the remembered route:
        // a changed owner epoch is a different binding and remains unknown.
        let mut altered = prepared.lease.clone();
        altered.owner_epoch = altered.owner_epoch.saturating_add(1);
        let refused = harness.post(&signed_mobile_ice(
            &altered,
            &prepared.lease_token,
            "signal_altered_prepared_ice",
            "candidate_jti_altered_prepared_ice",
            1,
            1,
        ));
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");
        assert_eq!(
            harness
                .session
                .relay_leases
                .get(&active.lease.lease_id)
                .unwrap()
                .current_jti,
            current_jti
        );

        harness
            .session
            .retired_prepared_rtc
            .get_mut(&prepared.lease.lease_id)
            .expect("the exact old generation is remembered")
            .expires_at = Instant::now();
        let refused = harness.post(&signed_mobile_ice(
            &prepared.lease,
            &prepared.lease_token,
            "signal_expired_prepared_ice",
            "candidate_jti_expired_prepared_ice",
            1,
            1,
        ));
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");
        assert!(harness.session.retired_prepared_rtc.is_empty());
        let current = harness
            .session
            .relay_leases
            .get(&active.lease.lease_id)
            .expect("rejecting the old binding leaves ACTIVE current");
        assert_eq!(current.current_jti, current_jti);
        assert_eq!(current.token, current_token);
        assert!(harness.session.leases.contains_key(&active.lease.jti));
    }

    #[test]
    fn active_rebind_deadline_is_not_renewable_and_frees_the_claimant() {
        let mut harness = RelayHarness::new();
        let active = activate_takeover_without_replacement_peer(
            &mut harness,
            "request_active_rebind_timeout",
        );
        let deadline = harness
            .session
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.active_rebind_deadline)
            .expect("active delivery starts a bounded replacement-peer window");

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_active_rebind_timeout_beat",
            "idempotencyKey": "idem_active_rebind_timeout_beat",
            "leaseToken": active.lease_token
        })
        .to_string();
        let renewed = harness.post(&heartbeat);
        assert_eq!(harness.granted(&renewed).status, PluginLeaseStatus::Renewed);
        harness.settle(&renewed, TransportDelivery::Delivered);
        assert_eq!(
            harness
                .session
                .prepared
                .as_ref()
                .and_then(|prepared| prepared.active_rebind_deadline),
            Some(deadline),
            "heartbeats cannot extend the media handoff deadline"
        );

        assert!(harness
            .session
            .expire_unbound_active_rebind(deadline + Duration::from_millis(1), &harness.media,));
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive
        );
        let refused = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");

        let replacement = harness.claim(
            LeaseMode::Takeover,
            "request_active_rebind_timeout_replacement",
        );
        assert_eq!(
            harness.granted(&replacement).status,
            PluginLeaseStatus::Provisional
        );
    }

    #[test]
    fn media_reconciliation_retires_authority_when_the_terminal_event_is_not_drained() {
        let mut harness = RelayHarness::new();
        let active =
            activate_takeover_without_replacement_peer(&mut harness, "request_terminal_event_drop");
        let binding = binding_for_claims(&active.lease);
        harness
            .media
            .revoke(&binding, "simulated_terminal_event_drop")
            .expect("native media returns safely even if its event is not drained");
        assert_eq!(harness.session.relay_leases.len(), 1);

        assert_eq!(
            harness
                .session
                .reconcile_relay_media_authority(&harness.media),
            1
        );
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.prepared.is_none());

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_terminal_event_drop_beat",
            "idempotencyKey": "idem_terminal_event_drop_beat",
            "leaseToken": active.lease_token
        })
        .to_string();
        let refused = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");
    }

    #[test]
    fn talk_readiness_timeout_matches_active_route_and_makes_heartbeat_terminal() {
        let mut harness = RelayHarness::new();
        let provisional_frames = harness.claim(LeaseMode::Takeover, "request_ready_timeout");
        let provisional = harness.granted(&provisional_frames);
        harness.settle(&provisional_frames, TransportDelivery::Delivered);

        let provisional_binding = binding_for_claims(&provisional.lease);
        harness
            .media
            .install_test_prepared_peer(provisional_binding.clone(), 10_000)
            .unwrap();
        harness
            .media
            .ack_prepare_human(&provisional_binding)
            .expect("test radio ACKs the non-mutating prepared Talk transition");
        let active_frames = harness
            .session
            .drain_media_events(&harness.media, &harness.radio)
            .unwrap();
        let active_encoded = active_frames
            .iter()
            .find(|encoded| {
                serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                    .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
            })
            .expect("preparation emits an active lease status")
            .clone();
        let active: PluginLeaseStatusFrame = serde_json::from_str(&active_encoded).unwrap();
        harness.session.finish_relay_delivery(
            &active_encoded,
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );

        let active_binding = binding_for_claims(&active.lease);
        harness.session.peers.insert(
            active.lease.rtc_session_id.clone(),
            PeerRoute {
                binding: active_binding.clone(),
                lease_jti: active.lease.jti.clone(),
                device_id: active.lease.device_id.clone(),
                sdp_revision: 2,
                transport_generation: 2,
                lease_ttl_ms: 20_000,
                connected: true,
                remote_audio_ready: true,
                remote_microphone_ready: false,
                transition_requested: false,
            },
        );
        let timeout = RemoteMediaEvent {
            sequence: 1,
            rtc_session_id: active_binding.rtc_session_id.clone(),
            call_id: active_binding.call_id.clone(),
            call_epoch: active_binding.call_epoch,
            owner_epoch: active_binding.owner_epoch,
            kind: RemoteMediaEventKind::Closed {
                reason: "talk_readiness_timeout".into(),
            },
        };
        assert!(
            harness
                .session
                .fail_route_if_current(&timeout, "talk_readiness_timeout", &harness.media)
                .unwrap()
                .is_some(),
            "the exact post-rotation binding retires the live gateway route"
        );
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive
        );

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_ready_timeout_beat",
            "idempotencyKey": "idem_ready_timeout_beat",
            "leaseToken": active.lease_token
        })
        .to_string();
        let refused = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");
    }

    #[test]
    fn a_heartbeat_retry_with_the_previous_token_replays_current_renewal() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_heartbeat_replay");
        let original = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_heartbeat_replay_beat",
            "idempotencyKey": "idem_heartbeat_replay_beat",
            "leaseToken": original.lease_token
        })
        .to_string();
        let renewed = harness.post(&heartbeat);
        let renewed_status = harness.granted(&renewed);
        assert_eq!(renewed_status.status, PluginLeaseStatus::Renewed);
        assert_ne!(renewed_status.lease_token, original.lease_token);
        assert_ne!(renewed_status.lease.jti, original.lease.jti);
        harness.settle(&renewed, TransportDelivery::Delivered);

        // The retry still carries the token that was current when the request
        // was first made. Its idempotency record names the stable lease, so it
        // receives the current renewal rather than minting another rotation.
        assert_eq!(harness.post(&heartbeat), renewed);
        assert_eq!(harness.session.leases.len(), 1);
        assert!(harness
            .session
            .leases
            .contains_key(&renewed_status.lease.jti));

        let mut changed: Value = serde_json::from_str(&heartbeat).unwrap();
        changed["requestId"] = Value::String("request_changed".into());
        let refused = harness.post(&changed.to_string());
        assert_eq!(harness.rejection(&refused).code, "duplicate_request");
    }

    #[test]
    fn a_dropped_renewal_does_not_extend_authority_and_can_be_retried() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_renewal_drop");
        let original = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);
        let original_entry = harness
            .session
            .relay_leases
            .values()
            .next()
            .expect("monitor relay lease exists")
            .clone();
        let original_claims = harness
            .session
            .leases
            .get(&original_entry.current_jti)
            .expect("monitor authority is committed")
            .clone();

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_renewal_drop_beat",
            "idempotencyKey": "idem_renewal_drop_beat",
            "leaseToken": original.lease_token
        })
        .to_string();
        let renewed = harness.post(&heartbeat);
        assert_eq!(harness.granted(&renewed).status, PluginLeaseStatus::Renewed);
        harness.settle(&renewed, TransportDelivery::Dropped);

        let after = harness
            .session
            .relay_leases
            .values()
            .next()
            .expect("the previous monitor lease remains current");
        assert_eq!(after.token, original_entry.token);
        assert_eq!(after.current_jti, original_entry.current_jti);
        assert_eq!(after.status, original_entry.status);
        assert_eq!(
            harness
                .session
                .leases
                .get(&after.current_jti)
                .expect("the old authority remains")
                .expires_at,
            original_claims.expires_at,
            "a status the device never received cannot extend its authority"
        );
        assert!(harness.session.pending_relay_status.is_none());

        let retry = harness.post(&heartbeat);
        assert_eq!(harness.granted(&retry).status, PluginLeaseStatus::Renewed);
        harness.settle(&retry, TransportDelivery::Dropped);
    }

    #[test]
    fn an_expired_current_lease_cannot_be_renewed_or_resurrected() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_expired_renewal");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);
        let current_jti = harness
            .session
            .relay_leases
            .values()
            .next()
            .expect("monitor lease exists")
            .current_jti
            .clone();
        harness
            .session
            .leases
            .get_mut(&current_jti)
            .expect("monitor authority exists")
            .expires_at = unix_now().unwrap();

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_expired_renewal_beat",
            "idempotencyKey": "idem_expired_renewal_beat",
            "leaseToken": status.lease_token
        })
        .to_string();
        let refused = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&refused).code, "lease_expired");
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.pending_relay_status.is_none());
        let again = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&again).code, "lease_unknown");
    }

    #[test]
    fn relay_replay_state_is_strictly_bounded() {
        let mut harness = RelayHarness::new();
        for index in 0..(MAX_RELAY_REPLAYS + 32) {
            harness.session.relay_record_replay(
                format!("test_replay_{index}"),
                format!("fingerprint_{index}"),
                harness.device_id.clone(),
                RelayReplayResult::RtcAccepted {
                    mode: LeaseMode::Monitor,
                },
            );
        }
        assert_eq!(harness.session.relay_replays.len(), MAX_RELAY_REPLAYS);
        assert!(!harness
            .session
            .relay_replay_has_room("one_more_distinct_operation"));
        assert!(harness.session.relay_replay_has_room("test_replay_0"));
    }

    #[test]
    fn a_dropped_active_status_revokes_the_provisional_authority() {
        let mut harness = RelayHarness::new();
        let provisional = harness.claim(LeaseMode::Takeover, "request_active_drop");
        let provisional_status = harness.granted(&provisional);
        harness.settle(&provisional, TransportDelivery::Delivered);

        let lease_id = provisional_status.lease.lease_id.clone();
        let mut active = provisional_status.lease.clone();
        active.phase = LeasePhase::Active;
        active.tracks = tracks_for(active.mode, LeasePhase::Active);
        active.owner_epoch = active.owner_epoch.saturating_add(1);
        active.jti = "leasejti_active_delivery_drop".into();
        active.expires_at = unix_now().unwrap() + RELAY_ACTIVE_LEASE_TTL;
        let token = harness
            .session
            .endpoint_authority
            .sign(&active.signing_bytes().expect("active claims canonicalize"));
        let encoded = harness
            .session
            .relay_lease_status(
                PluginLeaseStatus::Active,
                &harness.device_id,
                "request_active_drop",
                &token,
                active.clone(),
                unix_now().unwrap(),
            )
            .into_iter()
            .next()
            .expect("active status encodes");
        let prepared = harness
            .session
            .prepared
            .as_mut()
            .expect("the delivered provisional grant is armed");
        prepared.confirmed_owner_epoch = Some(active.owner_epoch);
        prepared.decision_sent = true;
        harness.session.pending_relay_status = Some(PendingRelayStatus::Active {
            encoded: encoded.clone(),
            lease_id,
            claims: active,
            token,
        });

        harness.session.finish_relay_delivery(
            &encoded,
            TransportDelivery::Dropped,
            &harness.media,
            &harness.radio,
        );
        assert!(harness.session.pending_relay_status.is_none());
        assert!(harness.session.prepared.is_none());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_leases.is_empty());
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive,
            "an undelivered active grant cannot strand caller ownership"
        );
    }

    #[test]
    fn the_kill_switch_disables_offers_and_every_admitted_arm() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Takeover);

        // Flipping the switch is a complete revert to the behaviour that
        // shipped before this path existed.
        harness.session.relay_authority_enabled = false;

        assert!(
            harness.publish_offers().is_empty(),
            "no offer means the Companion's own selection refuses before it sends"
        );
        for encoded in [
            offer_answer(&offer, &harness.device_id, "request_off"),
            lease_request(&offer, "request_off", "rtc_a"),
        ] {
            assert!(harness.post(&encoded).is_empty());
        }
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.deferred_prepare.is_none());

        // The authenticated hello still works, so a Companion can still join
        // and watch: only the authority is withdrawn.
        let peer = plugin_thumbprint(&harness.session);
        let hello = mobile_hello(
            &harness.session,
            &approved_mobile_signing_key(),
            "device_a",
            "hello_jti_switch",
            &peer,
        );
        assert!(harness.post(&hello).is_empty());
        assert!(harness.session.relay_peers.contains_key("device_a"));
        // The re-greet signal is set only by a hello whose proof VERIFIED, so
        // it proves the hello was actually processed rather than merely
        // tolerated.
        assert!(harness.session.take_relay_regreet_party().is_some());
    }

    #[test]
    fn offers_are_published_only_for_the_relay_carrier_and_a_live_call() {
        let mut harness = RelayHarness::new();
        assert!(!harness.publish_offers().is_empty());

        // The socket carrier has a gateway to mint these, and a snapshot
        // carrying ours there would be a second authority.
        harness.session.relay_carrier = false;
        assert!(harness.publish_offers().is_empty());
    }

    #[test]
    fn switchboard_transition_publishes_state_without_media_offers_then_reopens() {
        let mut harness = RelayHarness::new();
        *harness.status.switch_in_flight.lock().unwrap() =
            Some(("test_switch".into(), Instant::now()));
        assert!(
            harness.publish_offers().is_empty(),
            "no caller-seizing invitation exists while CHLD topology is unsettled"
        );

        *harness.status.switch_in_flight.lock().unwrap() = None;
        assert!(
            !harness.publish_offers().is_empty(),
            "settling the switch triggers a fresh offer-bearing projection"
        );
    }

    #[test]
    fn a_republished_offer_keeps_its_identity_until_the_call_state_moves() {
        let mut harness = RelayHarness::new();
        let first = harness.offer_for(LeaseMode::Takeover);

        // Re-minting per publish would hand the Companion a new offerId every
        // poll and turn an answer already in flight into a stale one.
        let again = harness.offer_for(LeaseMode::Takeover);
        assert_eq!(again.offer.offer_id, first.offer.offer_id);
        assert_eq!(again.offer_token, first.offer_token);

        // A real transition invalidates it, because the Companion filters an
        // offer against the exact snapshot it arrived in.
        harness.move_call("call_b");
        let moved = harness.offer_for(LeaseMode::Takeover);
        assert_ne!(moved.offer.offer_id, first.offer.offer_id);
        assert_eq!(moved.offer.call_id, "call_b");
    }

    #[test]
    fn a_revoked_lease_stops_being_recognised_at_once() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_revoke");
        let status = harness.granted(&granted);
        harness.settle(&granted, TransportDelivery::Delivered);
        assert_eq!(harness.session.relay_leases.len(), 1);
        quiesce_publication(&mut harness.session);

        let revoke = json!({
            "kind": "lease_revoke",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_revoke_now",
            "idempotencyKey": "idem_revoke_now",
            "leaseToken": status.lease_token,
            "reason": "handing back"
        })
        .to_string();
        assert!(harness.post(&revoke).is_empty());

        // Both books must forget it together: a token still recognised by the
        // relay registry could be heartbeated back to life after the session
        // already withdrew the authority.
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.last_snapshot_fingerprint.is_none());
        assert!(harness.session.last_snapshot_sent.is_none());
        assert!(harness.session.next_snapshot_poll <= Instant::now());

        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_revoked_beat",
            "idempotencyKey": "idem_revoked_beat",
            "leaseToken": status.lease_token
        })
        .to_string();
        let refused = harness.post(&heartbeat);
        assert_eq!(harness.rejection(&refused).code, "lease_unknown");
    }

    #[test]
    fn a_redelivered_claim_cannot_mint_a_second_lease() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Monitor);
        assert!(!harness.answer(&offer, "request_dupe").is_empty());
        let request = lease_request(&offer, "request_dupe", "rtc_a");

        let first = harness.post(&request);
        assert!(!first.is_empty());
        harness.session.finish_relay_delivery(
            &first[0],
            TransportDelivery::Delivered,
            &harness.media,
            &harness.radio,
        );
        assert_eq!(harness.session.relay_leases.len(), 1);

        // The carrier can redeliver after an ambiguous commit. Replay the
        // current grant byte-for-byte; never mint a second lease.
        assert_eq!(harness.post(&request), first);
        assert_eq!(harness.session.relay_leases.len(), 1);
    }

    #[test]
    fn a_flooding_device_is_refused_before_it_can_drive_minting() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Monitor);

        let mut refusals = 0;
        for index in 0..(RELAY_REQUEST_BUDGET + 4) {
            let frame = offer_answer(&offer, &harness.device_id, &format!("request_{index}"));
            let response = harness.post(&frame);
            if response
                .first()
                .and_then(|encoded| serde_json::from_str::<PluginClaimRejectedFrame>(encoded).ok())
                .is_some_and(|refusal| refusal.code == "rate_limited")
            {
                refusals += 1;
            }
        }
        assert!(
            refusals > 0,
            "a roster member that loops must not drive signing at whatever rate it likes"
        );
        assert!(harness.session.relay_leases.is_empty());
    }

    #[test]
    fn a_normal_claim_and_candidate_burst_use_independent_device_lanes() {
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Monitor);
        let answer = offer_answer(&offer, &harness.device_id, "request_lane_split");
        assert!(!harness.post(&answer).is_empty());
        let request = lease_request(&offer, "request_lane_split", "rtc_lane_split");
        assert!(!harness.post(&request).is_empty());
        assert_eq!(
            harness
                .session
                .relay_request_budget
                .get(&harness.device_id)
                .expect("offer + lease spend the control lane")
                .1,
            2
        );
        assert!(harness.session.relay_rtc_signal_budget.is_empty());

        // The live incident contained sixteen inbound SDP/ICE signals across
        // the prepared and active negotiations.  That healthy burst must not
        // consume (or be consumed by) the offer/lease control allowance.
        for _ in 0..16 {
            assert!(!harness.session.relay_rtc_over_budget(&harness.device_id));
        }
        assert_eq!(
            harness
                .session
                .relay_request_budget
                .get(&harness.device_id)
                .unwrap()
                .1,
            2
        );
        assert!(
            !harness.session.relay_over_budget(&harness.device_id),
            "a later control remains inside its own allowance"
        );

        let control_before_replay = harness
            .session
            .relay_request_budget
            .get(&harness.device_id)
            .unwrap()
            .1;
        assert!(!harness.post(&answer).is_empty());
        assert_eq!(
            harness
                .session
                .relay_request_budget
                .get(&harness.device_id)
                .unwrap()
                .1,
            control_before_replay,
            "control replays are resolved before charging too"
        );
    }

    #[test]
    fn rtc_trickle_is_bounded_per_device_without_spending_control_budget() {
        let mut harness = RelayHarness::new();
        for _ in 0..RELAY_RTC_SIGNAL_BUDGET {
            assert!(!harness.session.relay_rtc_over_budget(&harness.device_id));
        }
        assert!(harness.session.relay_rtc_over_budget(&harness.device_id));
        assert!(
            harness.session.relay_request_budget.is_empty(),
            "RTC floods cannot exhaust the control lane"
        );
        assert!(
            !harness.session.relay_rtc_over_budget("device_b"),
            "one device cannot spend another device's RTC allowance"
        );
    }

    #[test]
    fn one_device_cannot_spend_another_devices_request_budget() {
        // The budget is per-device state, so it may only be spent by a claim
        // whose sender has been identified. Otherwise an approved Companion
        // could name a rival in `targetDeviceId`, exhaust its allowance, and
        // have the rival's own claims refused as flooding — locking a device
        // out of taking over a live call it is entitled to take.
        let mut harness = RelayHarness::new();
        let offer = harness.offer_for(LeaseMode::Monitor);
        let frame = offer_answer(&offer, &harness.device_id, "request_flood");

        for _ in 0..(RELAY_REQUEST_BUDGET * 3) {
            let refused = harness.post_as(&frame, Some("mobile:some-other-approved-device"));
            assert_eq!(harness.rejection(&refused).code, "device_unknown");
        }

        // The rightful holder still has its full allowance.
        assert!(
            !harness.answer(&offer, "request_rightful").is_empty(),
            "an impostor's traffic must not consume the real device's budget"
        );
    }

    fn plugin_thumbprint(session: &GatewaySession) -> String {
        session.endpoint_authority.endpoint_key.thumbprint.clone()
    }

    fn accept_test_mobile_hello(
        session: &mut GatewaySession,
        encoded: &str,
        party: Option<&str>,
    ) -> Result<(), WorkerError> {
        let media = RemoteMediaHandle::spawn().unwrap();
        let subject = serde_json::from_str::<MobileHello>(encoded)
            .ok()
            .map(|hello| hello.device_id);
        session.accept_mobile_hello(
            encoded,
            party,
            subject.as_deref(),
            &full_relay_grants(),
            &media,
        )
    }

    /// Leave publication in the state a quiet, already-running session reaches:
    /// idle asserted once, nothing due for a minute.
    fn quiesce_publication(session: &mut GatewaySession) {
        session.authoritative_idle = true;
        session.last_snapshot_fingerprint = Some("stale-fingerprint".into());
        session.last_snapshot_sent = Some(Instant::now());
        session.next_snapshot_poll = Instant::now() + Duration::from_secs(60);
    }

    fn publication_is_quiesced(session: &GatewaySession) -> bool {
        session.authoritative_idle
            && session.last_snapshot_fingerprint.is_some()
            && session.last_snapshot_sent.is_some()
            && session.next_snapshot_poll > Instant::now()
    }

    #[test]
    fn approved_relay_hello_rearms_authoritative_publication_for_the_party_that_joined() {
        let mut session = test_gateway_session();
        let peer = plugin_thumbprint(&session);
        let encoded = mobile_hello(
            &session,
            &approved_mobile_signing_key(),
            "device_a",
            "hello_jti_1",
            &peer,
        );
        quiesce_publication(&mut session);
        session.last_assistance_request_sent = Some("assistance_pending".into());
        session.relay_snapshot_event_id = Some("snapshot_old".into());
        session
            .relay_snapshot_delivered_devices
            .insert("device_a".into());
        let party = approved_mobile_party();

        accept_test_mobile_hello(&mut session, &encoded, Some(&party))
            .expect("an approved Companion hello is admitted");

        // The carrier already registered the sender as a destination; this is
        // the half that makes the plugin actually speak to it.
        assert!(!session.authoritative_idle);
        assert!(session.last_snapshot_fingerprint.is_none());
        assert!(session.last_snapshot_sent.is_none());
        assert!(session.next_snapshot_poll <= Instant::now());

        // The behaviour that matters on a quiet line: authoritative state is
        // published again rather than waiting for the next call.
        assert!(session.idle_transition_frame().unwrap().is_some());

        // A fresh verified party is owed a new projected snapshot before the
        // still-pending assistance context can be delivered to it.
        assert!(session.last_assistance_request_sent.is_none());
        assert!(session.relay_snapshot_event_id.is_none());
        assert!(session.relay_snapshot_delivered_devices.is_empty());
    }

    #[test]
    fn a_verified_hello_without_state_read_is_not_admitted() {
        let mut session = test_gateway_session();
        let peer = plugin_thumbprint(&session);
        let encoded = mobile_hello(
            &session,
            &approved_mobile_signing_key(),
            "device_a",
            "hello_jti_no_state",
            &peer,
        );
        let media = RemoteMediaHandle::spawn().unwrap();
        let refusal = session
            .accept_mobile_hello(
                &encoded,
                Some(&approved_mobile_party()),
                Some("device_a"),
                &HashSet::from([Grant::RtcSignal, Grant::Monitor]),
                &media,
            )
            .unwrap_err();

        assert!(refusal.message.contains("state access"), "{refusal:?}");
        assert!(session.relay_peers.is_empty());
        assert!(session.take_relay_verified_route().is_none());
    }

    #[test]
    fn a_verified_rehello_without_state_read_revokes_existing_takeover_authority() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Takeover, "request_state_removed");
        harness.settle(&granted, TransportDelivery::Delivered);
        assert!(harness.session.prepared.is_some());
        assert_eq!(harness.session.leases.len(), 1);

        let peer = plugin_thumbprint(&harness.session);
        let encoded = mobile_hello(
            &harness.session,
            &approved_mobile_signing_key(),
            &harness.device_id,
            "hello_jti_state_removed",
            &peer,
        );
        let grants_without_state = HashSet::from([Grant::RtcSignal, Grant::Takeover]);
        let refusal = harness
            .session
            .accept_mobile_hello(
                &encoded,
                Some(&harness.party),
                Some(&harness.device_id),
                &grants_without_state,
                &harness.media,
            )
            .unwrap_err();

        assert!(refusal.message.contains("state access"), "{refusal:?}");
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.prepared.is_none());
        assert!(harness.session.deferred_prepare.is_none());
        assert!(harness
            .session
            .relay_peers
            .get(&harness.device_id)
            .is_some_and(|peer| peer.grants.is_empty()));
        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive
        );
    }

    #[test]
    fn a_signed_hello_cannot_invent_a_device_id_outside_its_authenticated_subject() {
        let mut session = test_gateway_session();
        let peer = plugin_thumbprint(&session);
        let encoded = mobile_hello(
            &session,
            &approved_mobile_signing_key(),
            "device_squatted",
            "hello_jti_subject_mismatch",
            &peer,
        );
        quiesce_publication(&mut session);
        let media = RemoteMediaHandle::spawn().unwrap();

        let refusal = session
            .accept_mobile_hello(
                &encoded,
                Some(&approved_mobile_party()),
                Some("device_authenticated"),
                &full_relay_grants(),
                &media,
            )
            .unwrap_err();

        assert!(
            refusal.message.contains("authenticated admission"),
            "{refusal:?}"
        );
        assert!(session.relay_peers.is_empty());
        assert!(session.take_relay_verified_route().is_none());
        assert!(publication_is_quiesced(&session));
    }

    #[test]
    fn same_session_grant_narrowing_revokes_only_unauthorized_modes() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_narrow_grants");
        harness.settle(&granted, TransportDelivery::Delivered);
        let peer = plugin_thumbprint(&harness.session);
        let signing_key = approved_mobile_signing_key();

        // A byte-equivalent admission on the same mobile session is a
        // re-introduction only; it must not touch existing authority.
        let unchanged = mobile_hello(
            &harness.session,
            &signing_key,
            &harness.device_id,
            "hello_jti_grants_unchanged",
            &peer,
        );
        harness
            .session
            .accept_mobile_hello(
                &unchanged,
                Some(&harness.party),
                Some(&harness.device_id),
                &full_relay_grants(),
                &harness.media,
            )
            .unwrap();
        assert_eq!(harness.session.relay_leases.len(), 1);
        assert_eq!(harness.session.leases.len(), 1);

        // Removing unrelated modes still leaves the authorized monitor alone.
        let still_monitor = mobile_hello(
            &harness.session,
            &signing_key,
            &harness.device_id,
            "hello_jti_grants_monitor_only",
            &peer,
        );
        harness
            .session
            .accept_mobile_hello(
                &still_monitor,
                Some(&harness.party),
                Some(&harness.device_id),
                &monitor_relay_grants(),
                &harness.media,
            )
            .unwrap();
        assert_eq!(harness.session.relay_leases.len(), 1);
        assert_eq!(harness.session.leases.len(), 1);

        // Once Monitor itself disappears the exact monitor lease is revoked;
        // a newly granted Takeover scope cannot preserve another mode.
        let no_monitor = mobile_hello(
            &harness.session,
            &signing_key,
            &harness.device_id,
            "hello_jti_grants_no_monitor",
            &peer,
        );
        harness
            .session
            .accept_mobile_hello(
                &no_monitor,
                Some(&harness.party),
                Some(&harness.device_id),
                &takeover_relay_grants(),
                &harness.media,
            )
            .unwrap();
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
    }

    #[test]
    fn a_new_mobile_session_nonce_retires_all_prior_device_authority() {
        let mut harness = RelayHarness::new();
        let granted = harness.claim(LeaseMode::Monitor, "request_nonce_rotation");
        harness.settle(&granted, TransportDelivery::Delivered);
        let peer = plugin_thumbprint(&harness.session);
        let rotated = mobile_hello_with_nonce(
            &harness.session,
            &approved_mobile_signing_key(),
            &harness.device_id,
            "hello_jti_nonce_rotated",
            &peer,
            "mobile_session_device_a_rotated",
        );

        harness
            .session
            .accept_mobile_hello(
                &rotated,
                Some(&harness.party),
                Some(&harness.device_id),
                &full_relay_grants(),
                &harness.media,
            )
            .unwrap();
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        assert_eq!(
            harness
                .session
                .relay_peers
                .get(&harness.device_id)
                .unwrap()
                .session_nonce,
            "mobile_session_device_a_rotated"
        );
    }

    #[test]
    fn an_admitted_relay_hello_makes_the_carrier_greet_that_party_again() {
        let mut session = test_gateway_session();
        let peer = plugin_thumbprint(&session);
        let signing_key = approved_mobile_signing_key();
        let thumbprint =
            EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes())
                .thumbprint;
        let party = relay_party(&thumbprint);

        let hello = mobile_hello(&session, &signing_key, "device_a", "hello_jti_greet", &peer);
        accept_test_mobile_hello(&mut session, &hello, Some(&party))
            .expect("an approved Companion hello is admitted");

        assert_eq!(
            session.take_relay_verified_route(),
            Some(("device_a".to_string(), party.clone())),
            "only the verified hello creates the carrier's targeted route"
        );

        // Re-arming publication alone is not enough to go live. The Companion
        // DROPS authoritative state from a peer whose endpoint key it has not
        // seen proved, and it learns that proof only from our `plugin_hello` —
        // which the carrier prepends once per party per plugin session. A
        // Companion that restarts as a fresh process is the SAME party (its
        // endpoint key is on disk) but has lost the proof, so without retiring
        // its greeting it would receive every re-armed frame and drop all of
        // them until this plugin session ends.
        assert_eq!(
            session.take_relay_regreet_party().as_deref(),
            Some(relay_party(&thumbprint).as_str())
        );
        // Consumed once: the carrier re-greets on the next send, and a stale
        // signal would make every later publish carry a redundant hello.
        assert!(session.take_relay_regreet_party().is_none());
    }

    #[test]
    fn a_refused_relay_hello_never_makes_the_plugin_reissue_its_hello() {
        let mut session = test_gateway_session();
        let peer = plugin_thumbprint(&session);

        // Correctly signed, just not by a key the owner approved.
        let media = RemoteMediaHandle::spawn().unwrap();
        let (radio, _control_rx) = crate::radio::RadioHandle::test_handle();
        session
            .handle_relay_peer_frame(
                &mobile_hello(
                    &session,
                    &SigningKey::from_bytes(&[11; 32]),
                    "device_intruder",
                    "hello_jti_greet_intruder",
                    &peer,
                ),
                None,
                Some("device_intruder"),
                &full_relay_grants(),
                &media,
                &radio,
            )
            .expect("a forged hello never terminates the session");

        // The re-greet is driven by a proof that VERIFIED, so an unapproved
        // party cannot make the plugin reissue anything on demand.
        assert!(session.take_relay_regreet_party().is_none());
    }

    #[test]
    fn a_replayed_relay_hello_is_refused_and_republishes_nothing() {
        let mut session = test_gateway_session();
        let peer = plugin_thumbprint(&session);
        let encoded = mobile_hello(
            &session,
            &approved_mobile_signing_key(),
            "device_a",
            "hello_jti_replay",
            &peer,
        );
        let party = approved_mobile_party();
        accept_test_mobile_hello(&mut session, &encoded, Some(&party)).unwrap();

        quiesce_publication(&mut session);
        let refusal = accept_test_mobile_hello(&mut session, &encoded, Some(&party)).unwrap_err();
        // Pin WHICH gate refused it: a replay must be caught by the jti cache,
        // not incidentally by an expired signature window.
        assert!(refusal.message.contains("replayed"), "{refusal:?}");
        assert!(publication_is_quiesced(&session));
    }

    #[test]
    fn a_relay_hello_signed_outside_the_owner_approved_roster_is_refused() {
        let mut session = test_gateway_session();
        let peer = plugin_thumbprint(&session);
        // Internally consistent and correctly signed — just not by a key the
        // owner approved.
        let encoded = mobile_hello(
            &session,
            &SigningKey::from_bytes(&[11; 32]),
            "device_intruder",
            "hello_jti_2",
            &peer,
        );
        quiesce_publication(&mut session);

        let refusal =
            accept_test_mobile_hello(&mut session, &encoded, Some("mobile:intruder")).unwrap_err();
        // The proof itself is valid; it is the ROSTER that refuses it. Pinning
        // the reason keeps this from passing on a malformed-signature accident.
        assert!(
            refusal.message.contains("owner-approved roster"),
            "{refusal:?}"
        );
        assert!(publication_is_quiesced(&session));
    }

    #[test]
    fn a_relay_hello_addressed_to_another_plugin_endpoint_is_refused() {
        let mut session = test_gateway_session();
        let elsewhere = EndpointPublicKey::from_ed25519_bytes(
            &SigningKey::from_bytes(&[12; 32]).verifying_key().to_bytes(),
        );
        let encoded = mobile_hello(
            &session,
            &approved_mobile_signing_key(),
            "device_a",
            "hello_jti_3",
            &elsewhere.thumbprint,
        );
        quiesce_publication(&mut session);

        let party = approved_mobile_party();
        let refusal = accept_test_mobile_hello(&mut session, &encoded, Some(&party)).unwrap_err();
        // This signer IS on the roster, so only the peer-binding check can
        // refuse it — which is the point of the check.
        assert!(
            refusal.message.contains("different plugin endpoint"),
            "{refusal:?}"
        );
        assert!(publication_is_quiesced(&session));
    }

    #[test]
    fn unhandled_relay_frames_are_dropped_without_touching_the_session() {
        let mut session = test_gateway_session();
        let media = RemoteMediaHandle::spawn().unwrap();
        let (radio, _control_rx) = crate::radio::RadioHandle::test_handle();
        let grants = full_relay_grants();
        quiesce_publication(&mut session);

        // Kinds this carrier still has no authority to act on: the assistance
        // answer and the caller-ending pair parse plugin-dialect twins only a
        // gateway produces.
        for encoded in [
            json!({"kind": "assistance_answer", "schemaVersion": SCHEMA_VERSION}).to_string(),
            json!({"kind": "end_caller_confirm", "schemaVersion": SCHEMA_VERSION}).to_string(),
            "{ this is not json".to_string(),
        ] {
            assert!(session
                .handle_relay_peer_frame(&encoded, None, None, &grants, &media, &radio)
                .expect("relay peer traffic never terminates the session")
                .is_empty());
        }
        assert!(publication_is_quiesced(&session));

        // Reported once per distinct kind, so a Companion emitting one on a
        // timer cannot wrap the log ring during a call.
        assert!(session
            .handle_relay_peer_frame(
                &"{ this is not json".to_string(),
                None,
                None,
                &grants,
                &media,
                &radio,
            )
            .is_ok());
        assert_eq!(session.dropped_relay_kinds.len(), 3);
    }

    #[test]
    fn the_relay_carrier_never_lets_peer_traffic_terminate_a_session_the_socket_still_refuses() {
        let media = RemoteMediaHandle::spawn().unwrap();
        let (radio, _control_rx) = crate::radio::RadioHandle::test_handle();
        let peer = plugin_thumbprint(&test_gateway_session());
        let grants = full_relay_grants();
        let hostile = [
            json!({"kind": "lease_request", "schemaVersion": SCHEMA_VERSION}).to_string(),
            json!({"kind": "error", "schemaVersion": SCHEMA_VERSION, "code": "boom", "message": "x"})
                .to_string(),
            json!({"kind": "claim_proposal", "schemaVersion": 9999}).to_string(),
            "{ this is not json".to_string(),
        ];

        for encoded in &hostile {
            // The carrier that carries untrusted peers: refuse the frame, keep
            // the session.
            let mut relay_session = test_gateway_session();
            assert!(relay_session
                .handle_inbound(
                    encoded,
                    &media,
                    &radio,
                    true,
                    Some("mobile:unknown"),
                    Some("device_unknown"),
                    Some(&grants),
                )
                .is_ok());

            // The carrier that carries trusted gateway infrastructure: a
            // violation still means the session is broken. Unchanged.
            let mut socket_session = test_gateway_session();
            assert!(socket_session
                .handle_inbound(encoded, &media, &radio, false, None, None, None)
                .is_err());
        }

        // The one actionable kind, on each carrier: admitted over the relay,
        // and still an unsupported frame on the socket, where a real gateway
        // never sends it.
        let mut relay_session = test_gateway_session();
        let hello = mobile_hello(
            &relay_session,
            &approved_mobile_signing_key(),
            "device_a",
            "hello_jti_4",
            &peer,
        );
        quiesce_publication(&mut relay_session);
        let party = approved_mobile_party();
        assert!(relay_session
            .handle_inbound(
                &hello,
                &media,
                &radio,
                true,
                Some(&party),
                Some("device_a"),
                Some(&grants),
            )
            .is_ok());
        assert!(!publication_is_quiesced(&relay_session));

        let mut socket_session = test_gateway_session();
        assert!(socket_session
            .handle_inbound(&hello, &media, &radio, false, None, None, None)
            .is_err());
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
    fn relay_urls_keep_their_path_and_refuse_unsafe_advertisements() {
        let frames = normalize_relay_url(
            "https://api.example.test/api/aokie-companion/relay/frames",
            "framesUrl",
        )
        .unwrap();
        // The mailbox path is authoritative: normalize_gateway_url's
        // /v2/realtime rewrite would destroy it.
        assert_eq!(frames.path(), "/api/aokie-companion/relay/frames");
        assert_ne!(frames.path(), "/v2/realtime");

        assert!(
            normalize_relay_url("https://api.example.test/relay?since=4", "streamUrl").is_ok(),
            "an existing query is not a reason to refuse the endpoint"
        );
        assert!(
            normalize_relay_url("https://user:pass@api.example.test/relay", "framesUrl").is_err()
        );
        assert!(normalize_relay_url("https://api.example.test/relay#part", "framesUrl").is_err());
        assert!(normalize_relay_url("/api/aokie-companion/relay/frames", "framesUrl").is_err());
        assert!(normalize_relay_url("wss://api.example.test/relay", "framesUrl").is_err());
        assert!(normalize_relay_url("http://public.example.test/relay", "framesUrl").is_err());

        // The live install serves plain http on .local names, so managed-beta
        // builds accept exactly those and nothing wider.
        let local = normalize_relay_url("http://api.formlogic.local/api/relay/frames", "framesUrl");
        let loopback = normalize_relay_url("http://127.0.0.1:17872/api/relay/frames", "framesUrl");
        if cfg!(feature = "managed-beta-driver") {
            assert!(local.is_ok());
            assert!(loopback.is_ok());
            assert_eq!(local.unwrap().path(), "/api/relay/frames");
        } else {
            assert!(local.is_err());
            assert!(loopback.is_err());
        }
    }

    #[test]
    fn admission_tolerates_the_relay_member_and_defaults_to_the_socket() {
        let authority = test_authority();

        let socket_only = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        assert!(
            socket_only.relay.is_none(),
            "an admission without the member keeps the untouched WebSocket path"
        );
        assert_eq!(socket_only.transport_label(), "websocket");

        let mut advertised = admission_value("app_a", "aokie", &authority);
        advertised["relay"] = json!({
            "challengeUrl": "https://api.example.test/api/aokie-companion/relay/challenge",
            "framesUrl": "https://api.example.test/api/aokie-companion/relay/frames",
            "streamUrl": "https://api.example.test/api/aokie-companion/relay/stream"
        });
        let response: AdmissionResponse =
            serde_json::from_value(advertised.clone()).expect("the relay member is tolerated");
        let credentials = response
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        let relay = credentials.relay.as_ref().expect("relay endpoints survive");
        assert_eq!(
            relay.stream_url,
            "https://api.example.test/api/aokie-companion/relay/stream"
        );
        assert_eq!(credentials.transport_label(), "relay");
        assert!(!format!("{credentials:?}").contains("aokie-adm-v2.secret-value"));

        // A split-origin or unsafe advertisement degrades to the socket
        // instead of failing the admission the live line depends on.
        let mut split = advertised.clone();
        split["relay"]["streamUrl"] = json!("https://elsewhere.example.test/relay/stream");
        let degraded: AdmissionResponse = serde_json::from_value(split).unwrap();
        assert!(degraded
            .into_credentials(None, "aokie", authority.clone())
            .unwrap()
            .relay
            .is_none());

        // A server that grows the advertisement keeps this build on the relay:
        // the transport hint is additive, so an unknown member is ignored
        // rather than failing the admission the live line depends on.
        let mut grown = advertised.clone();
        grown["relay"]["pollUrl"] =
            json!("https://api.example.test/api/aokie-companion/relay/frames");
        let tolerated: AdmissionResponse =
            serde_json::from_value(grown).expect("an added relay member is not fatal");
        assert!(tolerated
            .into_credentials(None, "aokie", authority.clone())
            .unwrap()
            .relay
            .is_some());

        // A reshaped advertisement this build cannot use degrades to the
        // socket — it must never cost the whole admission.
        let mut reshaped = advertised.clone();
        reshaped["relay"] = json!({"framesUrl": "https://api.example.test/relay/frames"});
        let degraded: AdmissionResponse =
            serde_json::from_value(reshaped).expect("a reshaped relay member is not fatal");
        assert!(degraded
            .into_credentials(None, "aokie", authority.clone())
            .unwrap()
            .relay
            .is_none());

        // The admission document itself stays strict: tolerance is scoped to
        // the additive transport hint, not to the security envelope.
        let mut unknown = advertised;
        unknown["mailboxUrl"] = json!("https://api.example.test/relay/mailbox");
        assert!(serde_json::from_value::<AdmissionResponse>(unknown).is_err());
    }

    #[test]
    fn the_admission_request_asks_for_the_relay_carrier_this_build_can_actually_open() {
        let authority = test_authority();
        let params = admission_request_params(Some("app_a"), "aokie", &authority).unwrap();

        // Desktop forwards the optional `relay` member ONLY to a build that
        // declares it, and strips it otherwise. Without this the carrier is
        // unreachable: every admission arrives relay-less and the plugin sits
        // on the WebSocket gateway forever, looking exactly like a backend that
        // never advertised.
        let declared = params.get("supportedTransports");
        if cfg!(feature = "voice") {
            assert_eq!(
                declared,
                Some(&json!([RELAY_TRANSPORT])),
                "the relay carrier only activates when the plugin asks for it"
            );
        } else {
            assert!(
                declared.is_none(),
                "a build with no relay carrier must keep the pre-relay request shape"
            );
        }

        // The rest of the request is the pre-relay contract, unchanged.
        assert_eq!(params.get("pluginId"), Some(&json!("aokie")));
        assert_eq!(params.get("appId"), Some(&json!("app_a")));
        assert_eq!(
            params.get("peerRosterHash"),
            Some(&json!(authority.roster_hash))
        );
        assert!(admission_request_params(None, "aokie", &authority)
            .unwrap()
            .get("appId")
            .is_none());
    }

    #[test]
    fn endpoint_hello_is_identical_whichever_transport_fetched_the_challenge() {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        let now = unix_now().unwrap();
        let challenge = EndpointChallengeFrame {
            kind: "endpoint_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            subject_id: "aokie".into(),
            role: AdmissionRole::Plugin,
            connection_id: "relay_c0ffee".into(),
            challenge_nonce: "challenge_abc123".into(),
            admission_jti: "jti_abc123".into(),
            holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
            expected_peer_key_thumbprint: None,
            approved_peer_key_thumbprints: authority.approved_thumbprints(),
            peer_roster_revision: Some(authority.roster_revision),
            peer_roster_hash: Some(authority.roster_hash.clone()),
            expires_at: now + 30,
        };

        let (socket_hello, socket_nonce) = endpoint_hello(&challenge, &credentials, now).unwrap();
        let (relay_hello, relay_nonce) = endpoint_hello(&challenge, &credentials, now).unwrap();

        // Only the per-connection nonces differ; everything the gateway binds
        // the connection to comes from the challenge itself.
        assert_ne!(socket_nonce, relay_nonce);
        assert_eq!(socket_hello.app_id, relay_hello.app_id);
        assert_eq!(socket_hello.plugin_id, relay_hello.plugin_id);
        for hello in [&socket_hello, &relay_hello] {
            hello.validate().unwrap();
            let claims = &hello.endpoint_proof.claims;
            assert_eq!(claims.connection_id, challenge.connection_id);
            assert_eq!(claims.challenge_nonce, challenge.challenge_nonce);
            assert_eq!(claims.admission_jti, challenge.admission_jti);
            assert_eq!(claims.expires_at, challenge.expires_at);
            assert_eq!(
                claims.approved_peer_key_thumbprints,
                authority.approved_thumbprints()
            );
            assert!(claims.expected_peer_key_thumbprint.is_none());
        }

        // A challenge minted for another identity is refused on either carrier.
        let mut foreign = challenge.clone();
        foreign.app_id = "app_b".into();
        assert!(endpoint_hello(&foreign, &credentials, now).is_err());

        // The plugin role must never be handed a mobile's peer expectation.
        let mut peered = challenge;
        peered.expected_peer_key_thumbprint = Some("mobile-thumbprint".into());
        assert!(endpoint_hello(&peered, &credentials, now).is_err());
    }

    #[test]
    fn a_relay_regreeting_refreshes_an_expired_proof_without_rotating_the_session() {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        let refresh_now = unix_now().unwrap();
        let original_now = refresh_now.saturating_sub(40);
        let logical_session = "plugin_session_live_takeover";
        let original_challenge = EndpointChallengeFrame {
            kind: "endpoint_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            subject_id: "aokie".into(),
            role: AdmissionRole::Plugin,
            connection_id: "relay_original".into(),
            challenge_nonce: "challenge_original".into(),
            admission_jti: "admission_original".into(),
            holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
            expected_peer_key_thumbprint: None,
            approved_peer_key_thumbprints: authority.approved_thumbprints(),
            peer_roster_revision: Some(authority.roster_revision),
            peer_roster_hash: Some(authority.roster_hash.clone()),
            expires_at: original_now + 30,
        };
        let original = endpoint_hello_for_session(
            &original_challenge,
            &credentials,
            original_now,
            logical_session,
        )
        .unwrap();
        assert_eq!(
            original.endpoint_proof.verify(refresh_now),
            Err(V2ProtocolError::Expired),
            "the cached proof reproduces the live failure after its 30-second window"
        );

        let fresh_challenge = EndpointChallengeFrame {
            connection_id: "relay_refreshed".into(),
            challenge_nonce: "challenge_refreshed".into(),
            admission_jti: "admission_refreshed".into(),
            expires_at: refresh_now + 30,
            ..original_challenge
        };
        let refreshed = endpoint_hello_for_session(
            &fresh_challenge,
            &credentials,
            refresh_now,
            logical_session,
        )
        .unwrap();

        refreshed.validate().unwrap();
        refreshed.endpoint_proof.verify(refresh_now).unwrap();
        assert_eq!(refreshed.session_nonce, logical_session);
        assert_eq!(
            refreshed.endpoint_proof.claims.session_nonce,
            logical_session
        );
        assert_eq!(
            refreshed.endpoint_proof.claims.connection_id,
            "relay_refreshed"
        );
        assert_ne!(
            refreshed.endpoint_proof.claims.jti,
            original.endpoint_proof.claims.jti
        );
    }

    #[cfg(feature = "voice")]
    #[tokio::test]
    async fn refreshed_relay_greeting_is_sent_before_the_rearmed_state() {
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        let now = unix_now().unwrap();
        let logical_session = "plugin_session_live_takeover";
        let original_now = now.saturating_sub(40);
        let original_challenge = EndpointChallengeFrame {
            kind: "endpoint_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            subject_id: "aokie".into(),
            role: AdmissionRole::Plugin,
            connection_id: "relay_original".into(),
            challenge_nonce: "challenge_original".into(),
            admission_jti: "admission_original".into(),
            holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
            expected_peer_key_thumbprint: None,
            approved_peer_key_thumbprints: authority.approved_thumbprints(),
            peer_roster_revision: Some(authority.roster_revision),
            peer_roster_hash: Some(authority.roster_hash.clone()),
            expires_at: original_now + 30,
        };
        let original = endpoint_hello_for_session(
            &original_challenge,
            &credentials,
            original_now,
            logical_session,
        )
        .unwrap();
        let original_jti = original.endpoint_proof.claims.jti.clone();

        let fresh_challenge = EndpointChallengeFrame {
            connection_id: "relay_refreshed".into(),
            challenge_nonce: "challenge_refreshed".into(),
            admission_jti: "admission_refreshed".into(),
            expires_at: now + 30,
            ..original_challenge
        };
        let challenge_response = fresh_challenge.clone();
        let posts = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured_posts = posts.clone();
        let router = axum::Router::new()
            .route(
                "/challenge",
                axum::routing::get(move || {
                    let challenge = challenge_response.clone();
                    async move { axum::Json(challenge) }
                }),
            )
            .route(
                "/frames",
                axum::routing::get(|| async { axum::Json(json!({"frames": [], "lastSeq": 0})) })
                    .post(move |axum::Json(body): axum::Json<Value>| {
                        let captured_posts = captured_posts.clone();
                        async move {
                            captured_posts.lock().unwrap().push(body);
                            axum::http::StatusCode::OK
                        }
                    }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let endpoints = RelayEndpoints {
            challenge_url: format!("http://{address}/challenge"),
            frames_url: format!("http://{address}/frames"),
            stream_url: format!("http://{address}/stream"),
        };
        let (mut channel, _) = crate::companion_relay::RelayChannel::connect(
            &endpoints,
            &credentials.token,
            &credentials.app_id,
            &credentials.plugin_id,
            authority.approved_thumbprints(),
        )
        .await
        .unwrap();
        channel.arm(serde_json::to_string(&original).unwrap());
        let party = relay_party(&authority.approved_thumbprints()[0]);
        channel.authorize_route("device_a", &party, &HashSet::from([Grant::StateRead]));
        let mut transport = GatewayTransport::Relay(channel);
        let state = "{\"kind\":\"plugin_snapshot\",\"deviceId\":\"device_a\"}";

        // Establish the bug's starting point: this party was greeted while the
        // original proof was current, but the cached document is now expired.
        transport.send_text(state).await.unwrap();
        posts.lock().unwrap().clear();

        let mut tasks = RelayGreetingTasks::default();
        tasks.schedule(
            party.clone(),
            transport.regreeting_request().unwrap(),
            credentials.clone(),
            logical_session.into(),
        );
        let greeting = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(result) = tasks.take_finished().await.into_iter().next() {
                    break result.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fresh challenge completes");
        assert!(transport.install_regreeting(greeting));
        transport.send_text(state).await.unwrap();

        let posted = posts.lock().unwrap();
        assert_eq!(posted.len(), 1);
        let frames = posted[0]["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2, "fresh hello must precede re-armed state");
        let hello: PluginHello = serde_json::from_value(frames[0].clone()).unwrap();
        hello.endpoint_proof.verify(unix_now().unwrap()).unwrap();
        assert_eq!(hello.session_nonce, logical_session);
        assert_eq!(hello.endpoint_proof.claims.session_nonce, logical_session);
        assert_eq!(
            hello.endpoint_proof.claims.connection_id,
            fresh_challenge.connection_id
        );
        assert_ne!(hello.endpoint_proof.claims.jti, original_jti);
        assert_eq!(frames[1]["kind"], "plugin_snapshot");
        server.abort();
    }

    #[test]
    fn admission_domain_change_returns_caller_and_drops_all_lease_continuity() {
        let mut harness = RelayHarness::new();
        let active =
            activate_takeover_without_replacement_peer(&mut harness, "request_domain_change_reset");
        assert_eq!(active.lease.phase, LeasePhase::Active);
        assert_eq!(harness.session.relay_leases.len(), 1);
        let authority = harness.session.endpoint_authority.clone();
        let refreshed = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();

        harness
            .session
            .apply_admission_rotation(
                &refreshed,
                "plugin_session_new_domain".into(),
                false,
                &harness.media,
            )
            .unwrap();

        assert_eq!(
            harness.media.snapshot().service_mode,
            LocalServiceMode::AokieActive
        );
        assert_eq!(
            harness.session.plugin_session_nonce,
            "plugin_session_new_domain"
        );
        assert!(harness.session.relay_leases.is_empty());
        assert!(harness.session.leases.is_empty());
        assert!(harness.session.relay_peers.is_empty());
        assert!(harness.session.peers.is_empty());
    }

    #[tokio::test]
    async fn delayed_admission_rotation_keeps_heartbeats_ahead_of_lease_expiry() {
        let mut harness = RelayHarness::new();
        let active = activate_takeover_without_replacement_peer(
            &mut harness,
            "request_admission_rotation_nonblocking",
        );
        assert_eq!(active.lease.phase, LeasePhase::Active);
        let logical_session = harness.session.plugin_session_nonce.clone();

        // Put the predecessor lease one second from expiry. The delayed
        // replacement represents the broker + endpoint challenge/open that
        // used to run synchronously on this same authority path.
        let now = unix_now().unwrap();
        harness
            .session
            .leases
            .get_mut(&active.lease.jti)
            .expect("the active lease is current")
            .expires_at = now + 1;
        let host_rpc = HostRpc::new();
        let (request_id, _line, response) = host_rpc.begin("companion.admission", json!({}));
        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();
        let mut rotation = AdmissionRotationTask {
            host_rpc: host_rpc.clone(),
            generation: 0,
            phase: Some(AdmissionRotationPhase::Broker(PendingAdmissionBroker {
                request_id,
                response,
                started_at: Instant::now(),
                expected_app_id: credentials.app_id.clone(),
                plugin_id: credentials.plugin_id.clone(),
                endpoint_authority: credentials.endpoint_authority.clone(),
                status: Arc::new(Mutex::new(GatewayStatusSnapshot::starting())),
                attempt: 0,
                predecessor_domain: AdmissionCarrierDomain::WebSocket,
            })),
        };
        assert!(rotation.is_pending());

        let polled_at = Instant::now();
        assert!(rotation.take_finished().await.is_none());
        assert!(
            polled_at.elapsed() < Duration::from_millis(50),
            "polling an unfinished rotation must not inherit its delay"
        );

        // The heartbeat is handled while the replacement remains in flight,
        // rotating the lease beyond its old deadline. Sweeping at a synthetic
        // time after that old deadline therefore keeps authority alive. The
        // former inline refresh could not read this frame before the sweep.
        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "heartbeat_during_admission_rotation",
            "idempotencyKey": "idem_heartbeat_during_admission_rotation",
            "leaseToken": active.lease_token
        })
        .to_string();
        let renewed_frames = harness.post(&heartbeat);
        let renewed = harness.granted(&renewed_frames);
        assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
        assert_eq!(renewed.lease.phase, LeasePhase::Active);
        harness.settle(&renewed_frames, TransportDelivery::Delivered);
        assert!(rotation.is_pending());
        assert_eq!(
            harness.session.expire_relay_leases(now + 2, &harness.media),
            0,
            "the queued heartbeat must renew before the old lease deadline is swept"
        );
        assert_eq!(harness.session.relay_leases.len(), 1);
        assert!(harness.session.leases.contains_key(&renewed.lease.jti));
        assert_eq!(harness.session.plugin_session_nonce, logical_session);

        assert!(host_rpc.try_route_response(&json!({
            "id": request_id,
            "error": {"code": -32000, "message": "synthetic broker refusal"}
        })));
        let failure = match rotation
            .take_finished()
            .await
            .expect("the broker response completes the rotation")
        {
            Ok(_) => panic!("the synthetic broker refusal unexpectedly opened a transport"),
            Err(error) => error,
        };
        assert_eq!(failure.kind, WorkerErrorKind::Rebootstrap);
        assert_eq!(harness.session.relay_leases.len(), 1);
        assert!(harness.session.leases.contains_key(&renewed.lease.jti));
    }

    #[cfg(feature = "voice")]
    #[tokio::test]
    async fn delayed_real_relay_rotation_opens_off_the_authority_path() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let authority = test_authority();
        let now = unix_now().unwrap();
        let challenge = EndpointChallengeFrame {
            kind: "endpoint_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            subject_id: "aokie".into(),
            role: AdmissionRole::Plugin,
            connection_id: "relay_admission_rotation".into(),
            challenge_nonce: "challenge_admission_rotation".into(),
            admission_jti: "admission_rotation".into(),
            holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
            expected_peer_key_thumbprint: None,
            approved_peer_key_thumbprints: authority.approved_thumbprints(),
            peer_roster_revision: Some(authority.roster_revision),
            peer_roster_hash: Some(authority.roster_hash.clone()),
            expires_at: now + 30,
        };
        let tail_requests = Arc::new(AtomicUsize::new(0));
        let seen_tail_requests = tail_requests.clone();
        let router = axum::Router::new()
            .route(
                "/challenge",
                axum::routing::get(move || {
                    let challenge = challenge.clone();
                    async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        axum::Json(challenge)
                    }
                }),
            )
            .route(
                "/frames",
                axum::routing::get(move || {
                    seen_tail_requests.fetch_add(1, Ordering::SeqCst);
                    async { axum::Json(json!({"frames": [], "lastSeq": 0})) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let endpoints = RelayEndpoints {
            challenge_url: format!("http://{address}/challenge"),
            frames_url: format!("http://{address}/frames"),
            stream_url: format!("http://{address}/stream"),
        };
        let mut credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();
        // Local test servers deliberately bypass production URL admission;
        // the production decoder's HTTPS/managed-beta policy has its own
        // contract tests. Everything from replacement open onward is real.
        credentials.relay = Some(endpoints);

        let host_rpc = HostRpc::new();
        let status = Arc::new(Mutex::new(GatewayStatusSnapshot {
            configured: true,
            connected: true,
            phase: GatewayConnectionPhase::Connected,
            reconnect_attempt: 0,
            last_error: None,
            changed_at: aokie_core::events::now_iso8601(),
        }));
        let rotation_credentials = credentials.clone();
        let delayed_open = tokio::spawn(async move {
            let (transport, plugin_session_nonce) =
                GatewayTransport::open_replacement(&rotation_credentials, &status, 0, true).await?;
            Ok(OpenedAdmissionRotation {
                generation: 7,
                credentials: rotation_credentials,
                transport,
                plugin_session_nonce,
                preserve_continuity: true,
            })
        });
        let mut rotation = AdmissionRotationTask::from_opening_for_test(host_rpc, 7, delayed_open);
        tokio::task::yield_now().await;
        assert!(rotation.take_finished().await.is_none());
        assert!(rotation.is_pending());

        let mut harness = RelayHarness::new();
        let active = activate_takeover_without_replacement_peer(
            &mut harness,
            "request_real_admission_rotation_nonblocking",
        );
        harness
            .session
            .leases
            .get_mut(&active.lease.jti)
            .unwrap()
            .expires_at = now + 1;
        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "heartbeat_during_real_admission_open",
            "idempotencyKey": "idem_heartbeat_during_real_admission_open",
            "leaseToken": active.lease_token
        })
        .to_string();
        let renewed_frames = harness.post(&heartbeat);
        let renewed = harness.granted(&renewed_frames);
        harness.settle(&renewed_frames, TransportDelivery::Delivered);
        assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
        assert_eq!(
            harness.session.expire_relay_leases(now + 2, &harness.media),
            0
        );
        assert!(rotation.is_pending());

        let opened = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(result) = rotation.take_finished().await {
                    break match result {
                        Ok(opened) => opened,
                        Err(error) => panic!("replacement failed: {}", error.message),
                    };
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the delayed replacement opens");
        assert_eq!(opened.generation, 7);
        assert!(opened.transport.is_relay());
        assert_eq!(
            tail_requests.load(Ordering::SeqCst),
            0,
            "a same-relay replacement inherits the predecessor cursor instead of walking the tail"
        );
        assert_eq!(harness.session.relay_leases.len(), 1);
        assert!(harness.session.leases.contains_key(&renewed.lease.jti));
        opened.transport.close().await;
        server.abort();
    }

    #[tokio::test]
    async fn websocket_rotation_defers_the_fencing_hello_until_atomic_handoff() {
        enum OldSocketCommand {
            Frame(String),
            Close,
        }

        let authority = test_authority();
        let now = unix_now().unwrap();
        let challenge = |suffix: &str| EndpointChallengeFrame {
            kind: "endpoint_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            subject_id: "aokie".into(),
            role: AdmissionRole::Plugin,
            connection_id: format!("ws_rotation_{suffix}"),
            challenge_nonce: format!("challenge_ws_rotation_{suffix}"),
            admission_jti: format!("admission_ws_rotation_{suffix}"),
            holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
            expected_peer_key_thumbprint: None,
            approved_peer_key_thumbprints: authority.approved_thumbprints(),
            peer_roster_revision: Some(authority.roster_revision),
            peer_roster_hash: Some(authority.roster_hash.clone()),
            expires_at: now + 30,
        };
        let old_challenge = challenge("old");
        let replacement_challenge = challenge("replacement");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let address = listener.local_addr().expect("listener address");
        let (old_commands, mut old_command_rx) =
            tokio::sync::mpsc::unbounded_channel::<OldSocketCommand>();
        let fence_old = old_commands.clone();
        let (old_ready_tx, old_ready_rx) = tokio::sync::oneshot::channel();
        let (new_hello_tx, mut new_hello_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (old_stream, _) = listener.accept().await.unwrap();
            let mut old_socket = tokio_tungstenite::accept_async(old_stream).await.unwrap();
            old_socket
                .send(Message::Text(
                    serde_json::to_string(&old_challenge).unwrap().into(),
                ))
                .await
                .unwrap();
            let old_hello = old_socket.next().await.unwrap().unwrap();
            assert!(matches!(old_hello, Message::Text(_)));
            let _ = old_ready_tx.send(());
            let old_writer = tokio::spawn(async move {
                while let Some(command) = old_command_rx.recv().await {
                    match command {
                        OldSocketCommand::Frame(encoded) => {
                            old_socket
                                .send(Message::Text(encoded.into()))
                                .await
                                .unwrap();
                        }
                        OldSocketCommand::Close => {
                            let _ = old_socket.send(Message::Close(None)).await;
                            break;
                        }
                    }
                }
            });

            let (replacement_stream, _) = listener.accept().await.unwrap();
            let mut replacement_socket = tokio_tungstenite::accept_async(replacement_stream)
                .await
                .unwrap();
            replacement_socket
                .send(Message::Text(
                    serde_json::to_string(&replacement_challenge)
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            let replacement_hello = replacement_socket.next().await.unwrap().unwrap();
            let Message::Text(encoded) = replacement_hello else {
                panic!("replacement endpoint proof is a text frame");
            };
            let hello: PluginHello = serde_json::from_str(encoded.as_str()).unwrap();
            assert_eq!(hello.kind, "plugin_hello");
            // This is the v2 gateway's same-plugin behaviour: accepting the
            // replacement hello immediately fences the predecessor.
            fence_old.send(OldSocketCommand::Close).unwrap();
            let _ = new_hello_tx.send(());
            while let Some(message) = replacement_socket.next().await {
                if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
            }
            let _ = old_writer.await;
        });

        let mut credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority)
            .unwrap();
        credentials.endpoint =
            Url::parse(&format!("ws://{address}/v2/realtime")).expect("local ws URL");
        credentials.relay = None;
        credentials.relay_only = false;
        let status = Arc::new(Mutex::new(GatewayStatusSnapshot::starting()));
        let (mut current, current_nonce) = GatewayTransport::open(&credentials, &status, 0)
            .await
            .unwrap();
        old_ready_rx.await.unwrap();

        let (mut replacement, replacement_nonce) =
            GatewayTransport::open_replacement(&credentials, &status, 0, false)
                .await
                .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(75), &mut new_hello_rx)
                .await
                .is_err(),
            "background open must not send the hello that fences the live predecessor"
        );

        let mut harness = RelayHarness::new();
        harness.session.plugin_session_nonce = current_nonce;
        let active =
            activate_takeover_without_replacement_peer(&mut harness, "request_ws_rotation_handoff");
        let heartbeat = json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "heartbeat_before_ws_handoff",
            "idempotencyKey": "idem_heartbeat_before_ws_handoff",
            "leaseToken": active.lease_token
        })
        .to_string();
        old_commands
            .send(OldSocketCommand::Frame(heartbeat))
            .unwrap();
        let inbound = current
            .recv_text(Duration::from_secs(1))
            .await
            .unwrap()
            .expect("the predecessor still carries its heartbeat");
        let renewed_frames = harness.post(&inbound);
        let renewed = harness.granted(&renewed_frames);
        harness.settle(&renewed_frames, TransportDelivery::Delivered);
        assert_eq!(renewed.status, PluginLeaseStatus::Renewed);

        replacement.activate_replacement().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), &mut new_hello_rx)
            .await
            .expect("the replacement hello reaches the gateway")
            .unwrap();
        harness
            .session
            .rotate_credentials(&credentials, replacement_nonce)
            .unwrap();
        let mut fenced_predecessor = std::mem::replace(&mut current, replacement);
        assert!(current.adopt_routing_from(&mut fenced_predecessor));
        assert!(
            fenced_predecessor
                .recv_text(Duration::from_secs(1))
                .await
                .is_err(),
            "the gateway really fenced the old socket after the committed hello"
        );
        assert_eq!(harness.session.relay_leases.len(), 1);
        assert!(harness.session.leases.contains_key(&renewed.lease.jti));

        current.close().await;
        drop(old_commands);
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("test gateway shuts down")
            .unwrap();
    }

    #[cfg(feature = "voice")]
    #[tokio::test]
    async fn delayed_or_failed_regreeting_never_stalls_active_takeover_heartbeats() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let authority = test_authority();
        let credentials = admission("app_a", "aokie", &authority)
            .into_credentials(None, "aokie", authority.clone())
            .unwrap();
        let now = unix_now().unwrap();
        let challenge = EndpointChallengeFrame {
            kind: "endpoint_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            subject_id: "aokie".into(),
            role: AdmissionRole::Plugin,
            connection_id: "relay_delayed".into(),
            challenge_nonce: "challenge_delayed".into(),
            admission_jti: "admission_delayed".into(),
            holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
            expected_peer_key_thumbprint: None,
            approved_peer_key_thumbprints: authority.approved_thumbprints(),
            peer_roster_revision: Some(authority.roster_revision),
            peer_roster_hash: Some(authority.roster_hash.clone()),
            expires_at: now + 30,
        };
        let challenge_requests = Arc::new(AtomicUsize::new(0));
        let seen_challenges = challenge_requests.clone();
        let challenge_response = challenge.clone();
        let router = axum::Router::new()
            .route(
                "/challenge",
                axum::routing::get(move || {
                    let request = seen_challenges.fetch_add(1, Ordering::SeqCst);
                    let challenge = challenge_response.clone();
                    async move {
                        use axum::response::IntoResponse;
                        if request == 0 {
                            // Initial channel open is unrelated to per-party
                            // re-greeting and completes immediately.
                            return axum::Json(challenge).into_response();
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        if request == 1 {
                            axum::Json(challenge).into_response()
                        } else {
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                        }
                    }
                }),
            )
            .route(
                "/frames",
                axum::routing::get(|| async { axum::Json(json!({"frames": [], "lastSeq": 0})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let endpoints = RelayEndpoints {
            challenge_url: format!("http://{address}/challenge"),
            frames_url: format!("http://{address}/frames"),
            stream_url: format!("http://{address}/stream"),
        };
        let (channel, _) = crate::companion_relay::RelayChannel::connect(
            &endpoints,
            &credentials.token,
            &credentials.app_id,
            &credentials.plugin_id,
            authority.approved_thumbprints(),
        )
        .await
        .unwrap();
        let transport = GatewayTransport::Relay(channel);

        let mut harness = RelayHarness::new();
        let active = activate_takeover_without_replacement_peer(
            &mut harness,
            "request_regreeting_nonblocking",
        );
        assert_eq!(active.lease.phase, LeasePhase::Active);
        let logical_session = harness.session.plugin_session_nonce.clone();
        let party = harness.party.clone();
        let heartbeat = |request_id: &str, token: &str| {
            json!({
                "kind": "lease_heartbeat",
                "schemaVersion": SCHEMA_VERSION,
                "appId": "app_a",
                "requestId": request_id,
                "idempotencyKey": format!("idem_{request_id}"),
                "leaseToken": token
            })
            .to_string()
        };

        let mut tasks = RelayGreetingTasks::default();
        tasks.schedule(
            party.clone(),
            transport.regreeting_request().unwrap(),
            credentials.clone(),
            logical_session.clone(),
        );
        // A repeat hello coalesces rather than spawning an unbounded second
        // challenge request for the same roster party.
        tasks.schedule(
            party.clone(),
            transport.regreeting_request().unwrap(),
            credentials.clone(),
            logical_session.clone(),
        );
        tokio::task::yield_now().await;
        assert!(tasks.is_pending(&party));
        assert!(tasks.take_finished().await.is_empty());

        // The delayed HTTP request is still in flight, but the exact active
        // takeover heartbeat rotates normally on the authority path.
        let renewed_frames =
            harness.post(&heartbeat("heartbeat_during_delay", &active.lease_token));
        let renewed = harness.granted(&renewed_frames);
        assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
        assert_eq!(renewed.lease.phase, LeasePhase::Active);
        harness.settle(&renewed_frames, TransportDelivery::Delivered);
        assert_eq!(harness.session.plugin_session_nonce, logical_session);
        assert_eq!(harness.session.relay_leases.len(), 1);

        let fresh = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(result) = tasks.take_finished().await.into_iter().next() {
                    break result.expect("the delayed refresh succeeds");
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delayed refresh completes");
        assert_eq!(fresh.plugin_session_nonce, logical_session);

        // The next attempt is delayed and then fails. It is equally isolated:
        // a second heartbeat advances while it is pending, and the error owns
        // no session/media state it could revoke.
        tasks.schedule(
            party.clone(),
            transport.regreeting_request().unwrap(),
            credentials,
            logical_session.clone(),
        );
        tokio::task::yield_now().await;
        assert!(tasks.is_pending(&party));
        let renewed_again_frames = harness.post(&heartbeat(
            "heartbeat_during_failed_refresh",
            &renewed.lease_token,
        ));
        let renewed_again = harness.granted(&renewed_again_frames);
        assert_eq!(renewed_again.status, PluginLeaseStatus::Renewed);
        harness.settle(&renewed_again_frames, TransportDelivery::Delivered);

        let failure = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(result) = tasks.take_finished().await.into_iter().next() {
                    break result.expect_err("the second challenge is refused");
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("failed refresh completes");
        assert_eq!(failure.kind, WorkerErrorKind::Reconnect);
        assert_eq!(harness.session.plugin_session_nonce, logical_session);
        assert_eq!(harness.session.relay_leases.len(), 1);
        assert!(harness
            .session
            .leases
            .contains_key(&renewed_again.lease.jti));
        assert_eq!(
            harness
                .session
                .expire_relay_leases(unix_now().unwrap(), &harness.media),
            0,
            "challenge failure cannot expire or revoke active authority"
        );
        server.abort();
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

/// One session, two possible carriers. Every [`GatewaySession`] method already
/// speaks text in and text out, so the protocol itself is transport-blind: the
/// WebSocket gateway remains the default and the FormLogic-hosted relay is
/// selected only when an admission advertises it.
///
/// Exactly one of these exists per session, so the size gap between the
/// carriers buys nothing worth boxing the socket the live path runs on.
#[allow(clippy::large_enum_variant)]
enum GatewayTransport {
    WebSocket(WebSocketTransport),
    #[cfg(feature = "voice")]
    Relay(crate::companion_relay::RelayChannel),
}

/// Carrier namespace whose authenticated connection state can survive an
/// admission rotation. WebSocket continuity keeps the gateway's established
/// behaviour. Relay continuity is narrower: its numeric cursor is meaningful
/// only inside one exact mailbox domain.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AdmissionCarrierDomain {
    WebSocket,
    #[cfg(feature = "voice")]
    Relay(crate::companion_relay::RelayCursorDomain),
}

impl AdmissionCarrierDomain {
    fn for_credentials(credentials: &SessionCredentials) -> Result<Self, WorkerError> {
        #[cfg(feature = "voice")]
        if let Some(endpoints) = credentials.relay.as_ref() {
            let domain = crate::companion_relay::RelayCursorDomain::from_endpoints(
                endpoints,
                &credentials.app_id,
                &credentials.plugin_id,
            )
            .ok_or_else(|| WorkerError::rebootstrap("Companion relay cursor domain is invalid"))?;
            return Ok(Self::Relay(domain));
        }
        #[cfg(not(feature = "voice"))]
        let _ = credentials;
        Ok(Self::WebSocket)
    }

    fn inherits_relay_cursor(&self) -> bool {
        #[cfg(feature = "voice")]
        {
            return matches!(self, Self::Relay(_));
        }
        #[cfg(not(feature = "voice"))]
        {
            false
        }
    }
}

/// The WebSocket carrier owns its own heartbeat bookkeeping so an admission
/// rotation replaces the ping schedule together with the socket it belongs to.
struct WebSocketTransport {
    socket: GatewaySocket,
    next_ping: Instant,
    awaiting_pong: Option<Instant>,
    /// Replacement sockets finish their endpoint challenge in the background
    /// but do not send this hello until the authority loop is ready to swap.
    /// Sending it earlier lets the gateway fence the predecessor before the
    /// loop has taken ownership of the replacement.
    pending_hello: Option<String>,
}

/// A freshly challenged hello prepared away from the call-authority loop.
#[cfg(feature = "voice")]
#[derive(Debug)]
struct FreshRelayGreeting {
    channel_id: u64,
    party: String,
    plugin_session_nonce: String,
    encoded_hello: String,
}

/// Bounded, per-party greeting refresh work.
///
/// Fetching the relay challenge may consume the full HTTP timeout. These tasks
/// own clone-only request/signing inputs and never borrow [`GatewayTransport`],
/// so `run_socket` continues receiving lease heartbeats and reconciling media
/// authority while the request is delayed. A repeated hello for the same party
/// coalesces onto its existing task.
#[cfg(feature = "voice")]
#[derive(Default)]
struct RelayGreetingTasks {
    by_party: HashMap<String, tokio::task::JoinHandle<Result<FreshRelayGreeting, WorkerError>>>,
}

#[cfg(feature = "voice")]
impl RelayGreetingTasks {
    fn schedule(
        &mut self,
        party: String,
        request: crate::companion_relay::RelayGreetingRequest,
        credentials: SessionCredentials,
        plugin_session_nonce: String,
    ) {
        if self.by_party.contains_key(&party) {
            return;
        }
        let task_party = party.clone();
        let channel_id = request.channel_id();
        let handle = tokio::spawn(async move {
            let challenge = request.fetch_challenge().await?;
            let hello = endpoint_hello_for_session(
                &challenge,
                &credentials,
                unix_now()?,
                &plugin_session_nonce,
            )?;
            let encoded_hello = serde_json::to_string(&hello)
                .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?;
            Ok(FreshRelayGreeting {
                channel_id,
                party: task_party,
                plugin_session_nonce,
                encoded_hello,
            })
        });
        self.by_party.insert(party, handle);
    }

    /// Drain only tasks Tokio already marks complete. Awaiting one of these
    /// cannot inherit the challenge timeout; work still in flight remains in
    /// the map and the authority loop proceeds immediately.
    async fn take_finished(&mut self) -> Vec<Result<FreshRelayGreeting, WorkerError>> {
        let finished = self
            .by_party
            .iter()
            .filter_map(|(party, task)| task.is_finished().then(|| party.clone()))
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(finished.len());
        for party in finished {
            let Some(task) = self.by_party.remove(&party) else {
                continue;
            };
            results.push(match task.await {
                Ok(result) => result,
                Err(_) => Err(WorkerError::reconnect(
                    "Companion relay greeting refresh task stopped",
                )),
            });
        }
        results
    }

    fn abort_all(&mut self) {
        for (_, task) in self.by_party.drain() {
            task.abort();
        }
    }

    #[cfg(test)]
    fn is_pending(&self, party: &str) -> bool {
        self.by_party.contains_key(party)
    }
}

#[cfg(feature = "voice")]
impl Drop for RelayGreetingTasks {
    fn drop(&mut self) {
        self.abort_all();
    }
}

/// A fully authenticated replacement prepared without borrowing the carrier
/// or session whose authority it will supersede.
struct OpenedAdmissionRotation {
    generation: u64,
    credentials: SessionCredentials,
    transport: GatewayTransport,
    plugin_session_nonce: String,
    preserve_continuity: bool,
}

struct PendingAdmissionBroker {
    request_id: u64,
    response: std::sync::mpsc::Receiver<crate::host_rpc::HostResult>,
    started_at: Instant,
    expected_app_id: String,
    plugin_id: String,
    endpoint_authority: Arc<EndpointAuthority>,
    status: Arc<Mutex<GatewayStatusSnapshot>>,
    attempt: u32,
    predecessor_domain: AdmissionCarrierDomain,
}

enum AdmissionRotationPhase {
    Broker(PendingAdmissionBroker),
    Opening(tokio::task::JoinHandle<Result<OpenedAdmissionRotation, WorkerError>>),
}

/// One overlapping admission replacement.
///
/// The Desktop RPC receiver is polled with `try_recv`, and endpoint opening is
/// owned by a Tokio task. Neither phase can wait on the sole session loop, so
/// the predecessor continues reading heartbeats and renewing active authority
/// until the replacement is complete. At most one generation is in flight.
struct AdmissionRotationTask {
    host_rpc: Arc<HostRpc>,
    generation: u64,
    phase: Option<AdmissionRotationPhase>,
}

impl AdmissionRotationTask {
    fn begin(
        host_rpc: Arc<HostRpc>,
        generation: u64,
        credentials: &SessionCredentials,
        status: Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
        predecessor_domain: AdmissionCarrierDomain,
    ) -> Result<Self, WorkerError> {
        let params = admission_request_params(
            Some(&credentials.app_id),
            &credentials.plugin_id,
            &credentials.endpoint_authority,
        )?;
        let (request_id, line, response) =
            host_rpc.begin("companion.admission", Value::Object(params));
        let mut sink = StdoutSink::new();
        if sink.send_line(&line).is_err() {
            host_rpc.forget(request_id);
            return Err(WorkerError::rebootstrap(
                "Desktop admission broker is unavailable",
            ));
        }
        Ok(Self {
            host_rpc,
            generation,
            phase: Some(AdmissionRotationPhase::Broker(PendingAdmissionBroker {
                request_id,
                response,
                started_at: Instant::now(),
                expected_app_id: credentials.app_id.clone(),
                plugin_id: credentials.plugin_id.clone(),
                endpoint_authority: credentials.endpoint_authority.clone(),
                status,
                attempt,
                predecessor_domain,
            })),
        })
    }

    /// Return only a completed result. Pending broker and transport work is
    /// observed, never awaited, by the authority loop.
    async fn take_finished(&mut self) -> Option<Result<OpenedAdmissionRotation, WorkerError>> {
        let broker_result = match self.phase.as_mut() {
            Some(AdmissionRotationPhase::Broker(pending)) => {
                if pending.started_at.elapsed() >= ADMISSION_RPC_TIMEOUT {
                    Some(Err(WorkerError::rebootstrap(
                        "Desktop admission refresh timed out",
                    )))
                } else {
                    match pending.response.try_recv() {
                        Ok(Ok(value)) => Some(Ok(value)),
                        Ok(Err(_)) => Some(Err(WorkerError::rebootstrap(
                            "Desktop rejected Companion admission refresh",
                        ))),
                        Err(std::sync::mpsc::TryRecvError::Empty) => None,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(
                            WorkerError::rebootstrap("Desktop admission refresh stopped"),
                        )),
                    }
                }
            }
            _ => None,
        };

        if let Some(result) = broker_result {
            let Some(AdmissionRotationPhase::Broker(pending)) = self.phase.take() else {
                return Some(Err(WorkerError::reconnect(
                    "Companion admission rotation lost its broker state",
                )));
            };
            if result.is_err() {
                self.host_rpc.forget(pending.request_id);
            }
            let value = match result {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            let response: AdmissionResponse = match serde_json::from_value(value) {
                Ok(response) => response,
                Err(_) => {
                    return Some(Err(WorkerError::rebootstrap(
                        "Desktop returned an invalid Companion admission response",
                    )))
                }
            };
            let credentials = match response.into_credentials(
                Some(&pending.expected_app_id),
                &pending.plugin_id,
                pending.endpoint_authority,
            ) {
                Ok(credentials) => credentials,
                Err(error) => return Some(Err(error)),
            };
            let replacement_domain = match AdmissionCarrierDomain::for_credentials(&credentials) {
                Ok(domain) => domain,
                Err(error) => return Some(Err(error)),
            };
            let preserve_continuity = pending.predecessor_domain == replacement_domain;
            let inherit_relay_cursor =
                preserve_continuity && pending.predecessor_domain.inherits_relay_cursor();
            let generation = self.generation;
            let handle = tokio::spawn(async move {
                let opened = tokio::time::timeout(
                    ADMISSION_TRANSPORT_OPEN_TIMEOUT,
                    GatewayTransport::open_replacement(
                        &credentials,
                        &pending.status,
                        pending.attempt,
                        inherit_relay_cursor,
                    ),
                )
                .await
                .map_err(|_| {
                    WorkerError::reconnect(
                        "Companion replacement transport did not open before its deadline",
                    )
                })??;
                Ok(OpenedAdmissionRotation {
                    generation,
                    credentials,
                    transport: opened.0,
                    plugin_session_nonce: opened.1,
                    preserve_continuity,
                })
            });
            self.phase = Some(AdmissionRotationPhase::Opening(handle));
            return None;
        }

        let opening_finished = matches!(
            self.phase.as_ref(),
            Some(AdmissionRotationPhase::Opening(task)) if task.is_finished()
        );
        if !opening_finished {
            return None;
        }
        let Some(AdmissionRotationPhase::Opening(task)) = self.phase.take() else {
            return Some(Err(WorkerError::reconnect(
                "Companion admission rotation lost its transport state",
            )));
        };
        Some(match task.await {
            Ok(result) => result,
            Err(_) => Err(WorkerError::reconnect(
                "Companion admission rotation task stopped",
            )),
        })
    }

    #[cfg(all(test, feature = "voice"))]
    fn from_opening_for_test(
        host_rpc: Arc<HostRpc>,
        generation: u64,
        task: tokio::task::JoinHandle<Result<OpenedAdmissionRotation, WorkerError>>,
    ) -> Self {
        Self {
            host_rpc,
            generation,
            phase: Some(AdmissionRotationPhase::Opening(task)),
        }
    }

    #[cfg(test)]
    fn is_pending(&self) -> bool {
        self.phase.is_some()
    }
}

impl Drop for AdmissionRotationTask {
    fn drop(&mut self) {
        match self.phase.take() {
            Some(AdmissionRotationPhase::Broker(pending)) => {
                self.host_rpc.forget(pending.request_id);
            }
            Some(AdmissionRotationPhase::Opening(task)) => task.abort(),
            None => {}
        }
    }
}

impl GatewayTransport {
    async fn open(
        credentials: &SessionCredentials,
        status: &Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
    ) -> Result<(Self, String), WorkerError> {
        Self::open_inner(credentials, status, attempt, false, false).await
    }

    async fn open_replacement(
        credentials: &SessionCredentials,
        status: &Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
        inherit_relay_cursor: bool,
    ) -> Result<(Self, String), WorkerError> {
        Self::open_inner(credentials, status, attempt, inherit_relay_cursor, true).await
    }

    async fn open_inner(
        credentials: &SessionCredentials,
        status: &Arc<Mutex<GatewayStatusSnapshot>>,
        attempt: u32,
        inherit_relay_cursor: bool,
        defer_websocket_hello: bool,
    ) -> Result<(Self, String), WorkerError> {
        #[cfg(feature = "voice")]
        if let Some(relay) = credentials.relay.as_ref() {
            let approved = credentials.endpoint_authority.approved_thumbprints();
            let (mut channel, challenge) = if inherit_relay_cursor {
                crate::companion_relay::RelayChannel::connect_replacement(
                    relay,
                    &credentials.token,
                    &credentials.app_id,
                    &credentials.plugin_id,
                    approved,
                )
                .await?
            } else {
                crate::companion_relay::RelayChannel::connect(
                    relay,
                    &credentials.token,
                    &credentials.app_id,
                    &credentials.plugin_id,
                    approved,
                )
                .await?
            };
            // The identical validation + signing the socket runs, so a relay
            // session proves the same endpoint identity from the same document.
            let (hello, session_nonce) = endpoint_hello(&challenge, credentials, unix_now()?)?;
            let encoded = serde_json::to_string(&hello)
                .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?;
            channel.arm(encoded);
            set_status(status, GatewayConnectionPhase::Connected, attempt, None);
            return Ok((Self::Relay(channel), session_nonce));
        }
        #[cfg(not(feature = "voice"))]
        if credentials.relay.is_some() {
            // The relay carrier rides reqwest, which only the voice build
            // pulls in. Say so once and use the proven WebSocket path rather
            // than pretending the advertisement was never made.
            eprintln!(
                "[aokie-plugin][companion] stage=relay_unavailable transport=websocket detail=The hosted relay transport requires the voice build"
            );
        }
        #[cfg(not(feature = "voice"))]
        let _ = inherit_relay_cursor;
        let (socket, session_nonce, pending_hello) =
            open_gateway_socket(credentials, status, attempt, defer_websocket_hello).await?;
        Ok((
            Self::WebSocket(WebSocketTransport {
                socket,
                next_ping: Instant::now() + PING_INTERVAL,
                awaiting_pong: None,
                pending_hello,
            }),
            session_nonce,
        ))
    }

    async fn send_text(&mut self, encoded: &str) -> Result<TransportDelivery, WorkerError> {
        match self {
            Self::WebSocket(transport) => transport
                .socket
                .send(Message::Text(encoded.into()))
                .await
                .map(|_| TransportDelivery::Delivered)
                .map_err(safe_ws_error),
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.send_text(encoded).await,
        }
    }

    /// `Ok(None)` means "nothing for the session this tick" — the carrier's own
    /// keepalive traffic never reaches the protocol layer.
    async fn recv_text(&mut self, tick: Duration) -> Result<Option<String>, WorkerError> {
        match self {
            Self::WebSocket(transport) => transport.recv_text(tick).await,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.recv_text(tick).await,
        }
    }

    async fn tick_heartbeat(&mut self) -> Result<(), WorkerError> {
        match self {
            Self::WebSocket(transport) => transport.tick_heartbeat().await,
            // The relay has no connection to keep warm: the server sends SSE
            // heartbeat comments and the reader tracks its own read idleness.
            #[cfg(feature = "voice")]
            Self::Relay(_) => Ok(()),
        }
    }

    /// Which carrier this session actually opened.
    ///
    /// Derived from the transport that was opened, never from
    /// `credentials.relay.is_some()`: the non-voice build advertises a relay in
    /// its admission request and still runs the socket, so the advertisement
    /// does not tell you which carrier is live.
    fn is_relay(&self) -> bool {
        match self {
            Self::WebSocket(_) => false,
            #[cfg(feature = "voice")]
            Self::Relay(_) => true,
        }
    }

    /// The roster party that posted the frame just returned by `recv_text`.
    ///
    /// `None` on the socket, where the gateway authenticated the connection and
    /// every frame on it was implicitly from that peer. On the relay any
    /// approved Companion can post to the same mailbox, so this is what lets
    /// the session tell one from another.
    fn last_inbound_party(&self) -> Option<&str> {
        match self {
            Self::WebSocket(_) => None,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.last_inbound_party(),
        }
    }

    /// Server-authenticated admission subject for the frame just returned.
    /// Socket gateway traffic already carries an authenticated connection
    /// identity and therefore has no relay envelope subject.
    fn last_inbound_subject(&self) -> Option<&str> {
        match self {
            Self::WebSocket(_) => None,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.last_inbound_subject(),
        }
    }

    /// Server-authenticated admission grants for the frame just returned.
    ///
    /// The socket gateway remains unchanged: it is the authority and exposes
    /// no relay envelope metadata, so this is `None` there.
    fn last_inbound_grants(&self) -> Option<&HashSet<Grant>> {
        match self {
            Self::WebSocket(_) => None,
            #[cfg(feature = "voice")]
            Self::Relay(channel) => Some(channel.last_inbound_grants()),
        }
    }

    /// Bind one relay device to the party its signed hello proved.
    fn authorize_relay_route(
        &mut self,
        device_id: &str,
        party: &str,
        authenticated_grants: &HashSet<Grant>,
    ) {
        match self {
            Self::WebSocket(_) => {}
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.authorize_route(device_id, party, authenticated_grants),
        }
        let _ = (device_id, party, authenticated_grants);
    }

    fn narrow_relay_route_grants(
        &mut self,
        device_id: &str,
        party: &str,
        authenticated_grants: &HashSet<Grant>,
    ) {
        match self {
            Self::WebSocket(_) => {}
            #[cfg(feature = "voice")]
            Self::Relay(channel) => {
                channel.narrow_route_grants(device_id, party, authenticated_grants)
            }
        }
        let _ = (device_id, party, authenticated_grants);
    }

    /// Clone-only input for a non-blocking relay greeting refresh.
    #[cfg(feature = "voice")]
    fn regreeting_request(&self) -> Option<crate::companion_relay::RelayGreetingRequest> {
        match self {
            Self::WebSocket(_) => None,
            Self::Relay(channel) => Some(channel.regreeting_request()),
        }
    }

    /// Install only into the relay channel that launched the refresh. An
    /// admission rotation replaces that channel and makes late work a no-op.
    #[cfg(feature = "voice")]
    fn install_regreeting(&mut self, greeting: FreshRelayGreeting) -> bool {
        match self {
            Self::WebSocket(_) => false,
            Self::Relay(channel) => channel.install_regreeting(
                greeting.channel_id,
                &greeting.party,
                greeting.encoded_hello,
            ),
        }
    }

    /// Preserve carrier-level continuity across an admission rotation.
    ///
    /// The socket needs nothing here — the gateway holds the routing and the
    /// predecessor stays live during the overlap. The relay has no such
    /// middleman: its replacement must inherit the read cursor and the learned
    /// device routes, or it re-reads frames the session already handled.
    fn admission_domain(&self) -> Result<AdmissionCarrierDomain, WorkerError> {
        match self {
            Self::WebSocket(_) => Ok(AdmissionCarrierDomain::WebSocket),
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel
                .cursor_domain()
                .map(AdmissionCarrierDomain::Relay)
                .ok_or_else(|| {
                    WorkerError::rebootstrap("Companion relay cursor domain is invalid")
                }),
        }
    }

    /// Commit a prepared replacement's identity proof. Relay hellos are
    /// cached and naturally go out with the next addressed post. WebSocket
    /// hellos fence the predecessor immediately, so they are deliberately
    /// withheld until this call runs on the authority loop immediately before
    /// the atomic transport swap.
    async fn activate_replacement(&mut self) -> Result<(), WorkerError> {
        match self {
            Self::WebSocket(transport) => {
                let Some(encoded) = transport.pending_hello.as_ref() else {
                    return Ok(());
                };
                transport
                    .socket
                    .send(Message::Text(encoded.clone().into()))
                    .await
                    .map_err(safe_ws_error)?;
                transport.pending_hello = None;
                Ok(())
            }
            #[cfg(feature = "voice")]
            Self::Relay(_) => Ok(()),
        }
    }

    fn adopt_routing_from(&mut self, previous: &mut Self) -> bool {
        #[cfg(feature = "voice")]
        {
            return match (self, previous) {
                (Self::Relay(next), Self::Relay(previous)) => next.adopt_routing_from(previous),
                (Self::WebSocket(_), Self::WebSocket(_)) => true,
                _ => false,
            };
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = (self, previous);
            true
        }
    }

    async fn close(self) {
        match self {
            Self::WebSocket(mut transport) => {
                let _ = transport.socket.send(Message::Close(None)).await;
            }
            #[cfg(feature = "voice")]
            Self::Relay(channel) => channel.close().await,
        }
    }
}

impl WebSocketTransport {
    async fn recv_text(&mut self, tick: Duration) -> Result<Option<String>, WorkerError> {
        match tokio::time::timeout(tick, self.socket.next()).await {
            Err(_) => Ok(None),
            Ok(Some(Ok(Message::Text(encoded)))) => Ok(Some(encoded.as_str().to_string())),
            Ok(Some(Ok(Message::Ping(payload)))) => {
                self.socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(safe_ws_error)?;
                Ok(None)
            }
            Ok(Some(Ok(Message::Pong(_)))) => {
                self.awaiting_pong = None;
                Ok(None)
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => Err(WorkerError::reconnect(
                "Companion gateway closed the socket",
            )),
            Ok(Some(Ok(_))) => Err(WorkerError::reconnect(
                "Companion gateway sent a non-text protocol frame",
            )),
            Ok(Some(Err(error))) => Err(safe_ws_error(error)),
        }
    }

    async fn tick_heartbeat(&mut self) -> Result<(), WorkerError> {
        if let Some(sent_at) = self.awaiting_pong {
            if sent_at.elapsed() >= PONG_TIMEOUT {
                return Err(WorkerError::reconnect(
                    "Companion gateway heartbeat timed out",
                ));
            }
        }
        if Instant::now() >= self.next_ping {
            self.socket
                .send(Message::Ping(Default::default()))
                .await
                .map_err(safe_ws_error)?;
            self.awaiting_pong = Some(Instant::now());
            self.next_ping = Instant::now() + PING_INTERVAL;
        }
        Ok(())
    }
}

async fn open_gateway_socket(
    credentials: &SessionCredentials,
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    attempt: u32,
    defer_hello: bool,
) -> Result<(GatewaySocket, String, Option<String>), WorkerError> {
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
    let (hello, session_nonce) = endpoint_hello(&challenge, credentials, now)?;
    let pending_hello = if defer_hello {
        Some(
            serde_json::to_string(&hello)
                .map_err(|_| WorkerError::reconnect("Companion frame could not be encoded"))?,
        )
    } else {
        send_json(&mut socket, &hello).await?;
        set_status(status, GatewayConnectionPhase::Connected, attempt, None);
        None
    };
    Ok((socket, session_nonce, pending_hello))
}

/// Validate an endpoint challenge against local identity and sign the plugin
/// hello it demands. Transport-free on purpose: the WebSocket gateway reads
/// its challenge off the socket and the hosted relay fetches the same document
/// over HTTP, and both must produce a byte-identical proof from it.
fn endpoint_hello(
    challenge: &EndpointChallengeFrame,
    credentials: &SessionCredentials,
    now: u64,
) -> Result<(PluginHello, String), WorkerError> {
    let session_nonce = format!("plugin_session_{}", uuid::Uuid::new_v4().simple());
    let hello = endpoint_hello_for_session(challenge, credentials, now, &session_nonce)?;
    Ok((hello, session_nonce))
}

/// Mint a fresh endpoint proof while retaining the logical plugin session.
///
/// Relay peers can re-introduce themselves long after the original hello's
/// short proof expired. Re-greeting must bind a fresh relay challenge to the
/// session nonce already carried by leases and RTC authentication; rotating
/// that nonce would instead fence the live session we are trying to preserve.
fn endpoint_hello_for_session(
    challenge: &EndpointChallengeFrame,
    credentials: &SessionCredentials,
    now: u64,
    session_nonce: &str,
) -> Result<PluginHello, WorkerError> {
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
    let proof_claims = HelloProofClaims {
        app_id: credentials.app_id.clone(),
        subject_id: credentials.plugin_id.clone(),
        role: AdmissionRole::Plugin,
        connection_id: challenge.connection_id.clone(),
        challenge_nonce: challenge.challenge_nonce.clone(),
        admission_jti: challenge.admission_jti.clone(),
        session_nonce: session_nonce.to_owned(),
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
        session_nonce: session_nonce.to_owned(),
        endpoint_proof: proof,
    };
    hello
        .validate()
        .map_err(|_| WorkerError::rebootstrap("Companion plugin hello is invalid"))?;
    Ok(hello)
}

fn admission_rotation_times(now: Instant, lifetime: Duration) -> (Instant, Instant) {
    let deadline = now + lifetime;
    let overlap = lifetime.min(ADMISSION_ROTATION_OVERLAP);
    let starts_at = deadline.checked_sub(overlap).unwrap_or(now);
    (starts_at, deadline)
}

async fn run_socket(
    mut credentials: SessionCredentials,
    host_rpc: &Arc<HostRpc>,
    radio: &RadioHandle,
    stop_rx: &mut watch::Receiver<bool>,
    status: &Arc<Mutex<GatewayStatusSnapshot>>,
    attempt: u32,
) -> Result<(), WorkerError> {
    let (mut transport, session_nonce) =
        GatewayTransport::open(&credentials, status, attempt).await?;
    let media = radio
        .remote_media()
        .ok_or_else(|| WorkerError::reconnect("Companion media endpoint is unavailable"))?;
    let mut session = GatewaySession::new(&credentials, session_nonce);
    let (mut admission_refresh_at, mut admission_deadline) =
        admission_rotation_times(Instant::now(), credentials.lifetime);
    let mut admission_retry_at = admission_refresh_at;
    let mut admission_generation = 0_u64;
    let mut admission_rotation: Option<AdmissionRotationTask> = None;
    let mut admission_rotation_error: Option<WorkerError> = None;
    let mut retiring_transport: Option<(GatewayTransport, Instant)> = None;
    #[cfg(feature = "voice")]
    let mut relay_greeting_tasks = RelayGreetingTasks::default();

    loop {
        if retiring_transport
            .as_ref()
            .is_some_and(|(_, deadline)| Instant::now() >= *deadline)
        {
            // Dropped rather than closed, exactly as before: the gateway has
            // already fenced this predecessor off the replacement hello, and
            // an explicit close frame here would be new behaviour on the
            // rotation path a live call depends on.
            retiring_transport.take();
        }
        if *stop_rx.borrow() {
            transport.close().await;
            if let Some((retiring, _)) = retiring_transport.take() {
                retiring.close().await;
            }
            return Ok(());
        }
        let rotation_result = match admission_rotation.as_mut() {
            Some(rotation) => rotation.take_finished().await,
            None => None,
        };
        if let Some(result) = rotation_result {
            admission_rotation.take();
            match result {
                Ok(opened) if opened.generation == admission_generation => {
                    let OpenedAdmissionRotation {
                        credentials: refreshed,
                        transport: mut replacement,
                        plugin_session_nonce: replacement_nonce,
                        preserve_continuity,
                        ..
                    } = opened;

                    // A WebSocket plugin_hello fences the predecessor at the
                    // gateway. Commit it only now, when this loop already owns
                    // the finished replacement and will not read/write the old
                    // socket again before swapping.
                    replacement.activate_replacement().await?;
                    session.apply_admission_rotation(
                        &refreshed,
                        replacement_nonce,
                        preserve_continuity,
                        media,
                    )?;

                    // Keep the authenticated predecessor alive briefly while
                    // the gateway consumes the replacement hello. The
                    // predecessor has continued reading and renewing leases
                    // for the whole broker/open overlap; only this atomic swap
                    // transfers its cursor and routes to the proven successor.
                    let mut predecessor = std::mem::replace(&mut transport, replacement);
                    if preserve_continuity && !transport.adopt_routing_from(&mut predecessor) {
                        media.fail_closed_all("gateway_admission_continuity_mismatch");
                        return Err(WorkerError::reconnect(
                            "Companion replacement transport changed continuity domains",
                        ));
                    }
                    retiring_transport.take();
                    retiring_transport =
                        Some((predecessor, Instant::now() + Duration::from_secs(2)));
                    #[cfg(feature = "voice")]
                    relay_greeting_tasks.abort_all();
                    credentials = refreshed;
                    admission_generation = admission_generation.saturating_add(1);
                    let times = admission_rotation_times(Instant::now(), credentials.lifetime);
                    admission_refresh_at = times.0;
                    admission_deadline = times.1;
                    admission_retry_at = admission_refresh_at;
                    admission_rotation_error = None;
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotated continuity={} transport={} app={} plugin={} active_peers={}",
                        if preserve_continuity { "preserved" } else { "reset" },
                        credentials.transport_label(),
                        credentials.app_id,
                        credentials.plugin_id,
                        session.peers.len()
                    );
                    continue;
                }
                Ok(opened) => {
                    // A newer generation won before this completion was
                    // observed. It owns no session state; close it and leave
                    // the current carrier authoritative.
                    opened.transport.close().await;
                    admission_rotation_error = Some(WorkerError::reconnect(
                        "A stale Companion admission replacement was discarded",
                    ));
                }
                Err(error) => {
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotation_deferred kind={} detail={}",
                        error.kind.label(),
                        sanitize_status_message(&error.message)
                    );
                    admission_rotation_error = Some(error);
                }
            }
            admission_retry_at = Instant::now() + ADMISSION_ROTATION_RETRY_DELAY;
        }

        let now = Instant::now();
        if now >= admission_deadline {
            admission_rotation.take();
            return Err(admission_rotation_error.take().unwrap_or_else(|| {
                WorkerError::expired(
                    "Companion admission could not rotate before its safe lifetime ended",
                )
            }));
        }
        if admission_rotation.is_none() && now >= admission_refresh_at && now >= admission_retry_at
        {
            let predecessor_domain = transport.admission_domain()?;
            match AdmissionRotationTask::begin(
                Arc::clone(host_rpc),
                admission_generation,
                &credentials,
                Arc::clone(status),
                attempt,
                predecessor_domain,
            ) {
                Ok(rotation) => {
                    admission_rotation = Some(rotation);
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotation_started continuity=overlap transport={} app={} plugin={}",
                        credentials.transport_label(),
                        credentials.app_id,
                        credentials.plugin_id
                    );
                }
                Err(error) => {
                    eprintln!(
                        "[aokie-plugin][companion] stage=admission_rotation_deferred kind={} detail={}",
                        error.kind.label(),
                        sanitize_status_message(&error.message)
                    );
                    admission_rotation_error = Some(error);
                    admission_retry_at = now + ADMISSION_ROTATION_RETRY_DELAY;
                }
            }
        }

        #[cfg(feature = "voice")]
        for completed in relay_greeting_tasks.take_finished().await {
            match completed {
                Ok(greeting) => {
                    if greeting.plugin_session_nonce == session.plugin_session_nonce
                        && transport.install_regreeting(greeting)
                    {
                        // State may already have been published while the HTTP
                        // challenge was in flight. Re-arm it now so the next
                        // publish is guaranteed to follow the fresh hello.
                        session.rearm_authoritative_publication();
                    } else {
                        // A logical-session or channel rotation won the race.
                        // Its own newly armed hello is authoritative; stale
                        // work is a contained no-op.
                    }
                }
                Err(error) => {
                    // A fresh peer greeting is recoverable signalling. Never
                    // turn a challenge outage into a live-call failback; a
                    // later verified mobile hello will schedule another try.
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_regreet_deferred kind={} detail={}",
                        error.kind.label(),
                        sanitize_status_message(&error.message)
                    );
                }
            }
        }

        // Which carrier is live decides two things the session cannot infer on
        // its own: whether it publishes its own offers, and whether it consumes
        // its own claim decision instead of addressing one to a gateway.
        session.relay_carrier = transport.is_relay();

        // Lease heartbeats prove only that the app process is alive. They do
        // not prove that an ACTIVE replacement peer ever opened, nor can a
        // dropped terminal event be allowed to leave gateway authority alive
        // after the media state has already returned the caller to Aokie.
        session.expire_relay_leases(unix_now()?, media);
        session.expire_unbound_active_rebind(Instant::now(), media);
        session.reconcile_relay_media_authority(media);

        let publication_due = Instant::now() >= session.next_snapshot_poll;
        if publication_due {
            session.next_snapshot_poll = Instant::now() + SNAPSHOT_POLL;
            // A terminal revoke is the exact proof that caller authority has
            // returned. Pay the oldest bounded delivery debt before every
            // ordinary egress lane, without re-running completed teardown.
            for encoded in session.due_pending_relay_revocations(Instant::now()) {
                session.prepare_relay_delivery(&encoded);
                let delivery = transport.send_text(&encoded).await?;
                session.finish_relay_delivery(&encoded, delivery, media, radio);
            }
        }
        for encoded in session.drain_end_caller_results()? {
            session.prepare_relay_delivery(&encoded);
            let delivery = transport.send_text(&encoded).await?;
            session.finish_relay_delivery(&encoded, delivery, media, radio);
        }
        for encoded in session.drain_media_events(media, radio)? {
            session.prepare_relay_delivery(&encoded);
            let delivery = transport.send_text(&encoded).await?;
            session.finish_relay_delivery(&encoded, delivery, media, radio);
        }
        if publication_due {
            for encoded in session.authoritative_state_frames(radio)? {
                session.prepare_relay_delivery(&encoded);
                let delivery = transport.send_text(&encoded).await?;
                session.finish_relay_delivery(&encoded, delivery, media, radio);
            }
            if let Some(encoded) = session.assistance_frame(radio)? {
                session.prepare_relay_delivery(&encoded);
                let delivery = transport.send_text(&encoded).await?;
                session.finish_relay_delivery(&encoded, delivery, media, radio);
            }
        }

        transport.tick_heartbeat().await?;

        let Some(encoded) = transport.recv_text(READ_TICK).await? else {
            continue;
        };
        let from_relay_peer = transport.is_relay();
        let inbound_party = transport.last_inbound_party().map(str::to_owned);
        let inbound_subject = transport.last_inbound_subject().map(str::to_owned);
        let inbound_grants = transport.last_inbound_grants().cloned();
        let inbound_kind = serde_json::from_str::<Envelope>(&encoded)
            .map(|frame| frame.kind)
            .unwrap_or_else(|_| "malformed".into());
        let outbound = session
            .handle_inbound(
                &encoded,
                media,
                radio,
                from_relay_peer,
                inbound_party.as_deref(),
                inbound_subject.as_deref(),
                inbound_grants.as_ref(),
            )
            .map_err(|error| {
                eprintln!(
                    "[aokie-plugin][companion] stage=inbound_rejected frame={} kind={} detail={}",
                    inbound_kind,
                    error.kind.label(),
                    sanitize_status_message(&error.message)
                );
                error
            })?;
        // Relay admission can narrow on any later authenticated frame. Keep
        // transport-level privacy projections in the same fail-closed state
        // before sending outbound assistance/caller context. Ordinary frames
        // may remove grants; only a newly verified hello may broaden them.
        if let (Some(device_id), Some(party), Some(grants)) = (
            inbound_subject.as_deref(),
            inbound_party.as_deref(),
            inbound_grants.as_ref(),
        ) {
            transport.narrow_relay_route_grants(device_id, party, grants);
        }
        // Route ownership is installed only after the mobile hello's endpoint
        // proof, owner roster membership and actual relay sender all agree.
        // Doing this before the response loop is what makes the very first
        // targeted acceptance/grant reach the device that proved it.
        if let Some((device_id, party)) = session.take_relay_verified_route() {
            transport.authorize_relay_route(
                &device_id,
                &party,
                inbound_grants.as_ref().unwrap_or(&HashSet::new()),
            );
        }
        // Before anything else goes out: a Companion that just proved itself
        // may have lost the proof WE gave it (a restarted process keeps its
        // on-disk endpoint key, so it is the same party, but its memory of our
        // hello is gone). Fetch a new challenge and replace the cached proof
        // before retiring its greeting mark, while the re-armed publish is
        // still one loop turn away, so the state it is about to receive arrives
        // behind a CURRENT hello it can verify.
        if let Some(party) = session.take_relay_regreet_party() {
            #[cfg(feature = "voice")]
            if let Some(request) = transport.regreeting_request() {
                relay_greeting_tasks.schedule(
                    party,
                    request,
                    credentials.clone(),
                    session.plugin_session_nonce.clone(),
                );
            }
            #[cfg(not(feature = "voice"))]
            let _ = party;
        }
        for encoded in outbound {
            session.prepare_relay_delivery(&encoded);
            let delivery = transport.send_text(&encoded).await?;
            session.finish_relay_delivery(&encoded, delivery, media, radio);
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
    /// Authoritative publication ready for the carrier that is actually live.
    ///
    /// The socket sends one full plugin snapshot to trusted gateway
    /// infrastructure, which performs per-peer projection. The dumb relay has
    /// no trusted translator, so the plugin produces one redacted, targeted
    /// snapshot per authenticated device before any bytes leave the Desktop.
    fn authoritative_state_frames(
        &mut self,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let Some(encoded) = self.authoritative_state_frame(radio)? else {
            return Ok(Vec::new());
        };
        if !self.relay_carrier
            || !serde_json::from_str::<Envelope>(&encoded)
                .is_ok_and(|frame| frame.kind == "plugin_snapshot")
        {
            return Ok(vec![encoded]);
        }
        self.relay_project_snapshot(&encoded)
    }

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

    /// Project one raw authoritative snapshot into device-addressed relay
    /// frames. This is the security boundary the old gateway supplied.
    fn relay_project_snapshot(&mut self, encoded: &str) -> Result<Vec<String>, WorkerError> {
        let frame: PluginSnapshotFrame = serde_json::from_str(encoded)
            .map_err(|_| WorkerError::reconnect("Relay snapshot could not be projected"))?;
        if self.relay_snapshot_event_id.as_deref() != Some(frame.event_id.as_str()) {
            self.relay_snapshot_event_id = Some(frame.event_id.clone());
            self.relay_snapshot_delivered_devices.clear();
        }
        let mut devices = self.relay_peers.keys().cloned().collect::<Vec<_>>();
        devices.sort();
        let mut projected = Vec::with_capacity(devices.len());
        for device_id in devices {
            let Some(peer) = self.relay_peers.get(&device_id) else {
                continue;
            };
            if !peer.grants.contains(&Grant::StateRead) {
                continue;
            }
            let mut snapshot = frame.snapshot.clone();
            if !peer.grants.contains(&Grant::CallerRead) {
                snapshot.caller = None;
            }
            let captions_permitted = peer.grants.contains(&Grant::CaptionsRead)
                && snapshot.remote_consent.enabled
                && snapshot.remote_consent.acknowledged
                && snapshot.remote_consent.captions_enabled;
            if !captions_permitted {
                snapshot.captions.clear();
            }
            if !peer.grants.contains(&Grant::AudioLevelsRead) {
                snapshot.audio_levels = None;
            }
            snapshot.pending_mobile_offers.retain(|offer| {
                offer.offer.target_device_id == device_id
                    && relay_grants_allow_mode(&peer.grants, offer.offer.offered_mode)
            });
            let targeted = PluginSnapshotFrame {
                kind: frame.kind.clone(),
                schema_version: frame.schema_version,
                app_id: frame.app_id.clone(),
                event_id: frame.event_id.clone(),
                device_id: Some(device_id),
                snapshot,
            };
            targeted.validate().map_err(|_| {
                WorkerError::reconnect("Projected relay snapshot failed local validation")
            })?;
            projected.push(serde_json::to_string(&targeted).map_err(|_| {
                WorkerError::reconnect("Projected relay snapshot could not be encoded")
            })?);
        }
        Ok(projected)
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
        let switch_in_flight = radio.switch_in_flight();
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
            pending_mobile_offers: Vec::new(),
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
            "switchInFlight": switch_in_flight,
        }))
        .map_err(|_| WorkerError::reconnect("Call snapshot fingerprint failed"))?;
        let unchanged = self.last_snapshot_fingerprint.as_deref() == Some(fingerprint.as_str());
        let refresh_due = self
            .last_snapshot_sent
            .is_none_or(|sent| sent.elapsed() >= SNAPSHOT_REFRESH);
        if unchanged && !refresh_due {
            return Ok(None);
        }
        // Attach offers only once this snapshot is definitely going out, and
        // AFTER the fingerprint is taken. The fingerprint deliberately ignores
        // offers: including them would make every offer refresh look like a
        // state change and republish the snapshot on every poll.
        //
        // The Companion filters an offer against the snapshot it arrived in, so
        // these are stamped with the fences of THIS frame rather than a re-read
        // of live state that may already have moved on.
        let snapshot = if switch_in_flight {
            // Publish state so the Companion can lock controls, but never mint
            // or re-publish a caller-seizing offer while CHLD topology is in
            // flight. Including this flag in the fingerprint above forces a
            // fresh offer-bearing snapshot as soon as the switch settles.
            snapshot
        } else {
            self.attach_pending_offers(snapshot, &remote)?
        };
        snapshot
            .validate()
            .map_err(|_| WorkerError::reconnect("Authoritative call snapshot is invalid"))?;
        let frame = PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            event_id: format!("snapshot_{}", uuid::Uuid::new_v4().simple()),
            device_id: None,
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

    /// Stamp this snapshot with the offers its recipients may act on.
    ///
    /// Empty on the socket carrier, where the gateway is the authority. Empty
    /// too when the kill switch is off, which is what makes
    /// `AOKIE_RELAY_LEASE_AUTHORITY=0` a complete revert: no offer means the
    /// Companion's own `select_mobile_offer` refuses before it sends anything.
    fn attach_pending_offers(
        &mut self,
        mut snapshot: AuthoritativeCallSnapshot,
        remote: &crate::remote_media::RemoteMediaSnapshot,
    ) -> Result<AuthoritativeCallSnapshot, WorkerError> {
        if !self.relay_authority_enabled || !self.relay_carrier || self.relay_peers.is_empty() {
            return Ok(snapshot);
        }
        // An offer is an invitation to seize a LIVE caller's audio, so it is
        // only ever made about a call that is genuinely up. `Ringing` has no
        // audio to take and `Ended` has no caller left.
        if !matches!(snapshot.telephony_state, TelephonyState::Active) {
            return Ok(snapshot);
        }
        let now = unix_now()?;
        self.relay_offers
            .retain(|_, offer| offer.claims.expires_at > now);

        // Consult is the one mode with a precondition beyond consent: it exists
        // to answer a question Aokie asked, so without a current assistance
        // request there is nothing to consult about — and `handle_claim_proposal`
        // would refuse the claim anyway.
        let assistance = crate::assistance::global().pending_frame(&self.app_id);
        let consult_available = assistance.is_some_and(|assistance| {
            assistance.call_id == snapshot.call_id
                && assistance.call_epoch == snapshot.call_epoch
                && assistance.owner_epoch == snapshot.owner_epoch
                && assistance.switchboard_revision == snapshot.switchboard_revision
                && assistance.remote_revision == snapshot.remote_revision
        });

        let mut devices = self.relay_peers.keys().cloned().collect::<Vec<_>>();
        devices.sort();
        let mut published: Vec<SignedPendingMobileOffer> = Vec::new();
        for device_id in devices {
            for mode in [LeaseMode::Monitor, LeaseMode::Consult, LeaseMode::Takeover] {
                if published.len() >= MAX_PENDING_MOBILE_OFFERS {
                    break;
                }
                if !self
                    .relay_peers
                    .get(&device_id)
                    .is_some_and(|peer| relay_grants_allow_mode(&peer.grants, mode))
                {
                    continue;
                }
                // Consent is re-read from the effective gate, so an expired or
                // withdrawn disclosure stops producing offers immediately. This
                // is a fast fail only: the authoritative check lives inside the
                // media state lock and runs again on every claim and every
                // caller-bound frame.
                let permitted = remote.consent.enabled
                    && remote.consent.acknowledged
                    && match mode {
                        LeaseMode::Monitor => remote.consent.monitor_enabled,
                        LeaseMode::Consult => remote.consent.consult_enabled && consult_available,
                        LeaseMode::Takeover => remote.consent.takeover_enabled,
                    };
                if !permitted {
                    continue;
                }
                if let Some(offer) = self.reusable_offer(&device_id, mode, &snapshot, now) {
                    published.push(offer);
                    continue;
                }
                if let Some(offer) = self.mint_offer(&device_id, mode, &snapshot, now)? {
                    published.push(offer);
                }
            }
        }
        snapshot.pending_mobile_offers = published;
        Ok(snapshot)
    }

    /// The live offer for this (device, mode) if it is still worth publishing.
    ///
    /// Reuse keeps the offer identity stable while nothing has changed, which
    /// matters because the fences move on real transitions rather than on a
    /// timer: re-minting per publish would hand the Companion a new `offerId`
    /// every poll and turn an answer already in flight into a stale one.
    fn reusable_offer(
        &self,
        device_id: &str,
        mode: LeaseMode,
        snapshot: &AuthoritativeCallSnapshot,
        now: u64,
    ) -> Option<SignedPendingMobileOffer> {
        self.relay_offers
            .values()
            .find(|offer| {
                offer.claims.target_device_id == device_id
                    && offer.claims.offered_mode == mode
                    && !offer.accepted
                    && offer.claims.expires_at > now.saturating_add(RELAY_OFFER_REFRESH_MARGIN)
                    && Self::offer_matches_snapshot(&offer.claims, snapshot)
            })
            .map(|offer| SignedPendingMobileOffer {
                offer: offer.claims.clone(),
                offer_token: offer.token.clone(),
            })
    }

    /// Whether an offer still describes exactly the call state being published.
    ///
    /// The Companion applies the same equality before it will answer, so an
    /// offer that drifts from its snapshot is unusable rather than merely
    /// stale — and re-publishing it would be dead weight in the frame.
    fn offer_matches_snapshot(
        claims: &PendingMobileOfferClaims,
        snapshot: &AuthoritativeCallSnapshot,
    ) -> bool {
        claims.call_id == snapshot.call_id
            && claims.call_epoch == snapshot.call_epoch
            && claims.owner_epoch == snapshot.owner_epoch
            && claims.switchboard_revision == snapshot.switchboard_revision
            && claims.remote_revision == snapshot.remote_revision
            && claims.required_consent_policy_id == snapshot.remote_consent.policy_id
            && claims.required_consent_policy_version == snapshot.remote_consent.policy_version
    }

    fn mint_offer(
        &mut self,
        device_id: &str,
        mode: LeaseMode,
        snapshot: &AuthoritativeCallSnapshot,
        now: u64,
    ) -> Result<Option<SignedPendingMobileOffer>, WorkerError> {
        let Some(peer) = self.relay_peers.get(device_id) else {
            return Ok(None);
        };
        if !relay_grants_allow_mode(&peer.grants, mode) {
            return Ok(None);
        }
        // Drop the superseded, un-answered offer for this (device, mode) so the
        // registry tracks live invitations rather than growing with history.
        // An ACCEPTED one is left alone: the device is mid-redemption against
        // that exact identity and dropping it would refuse a claim already in
        // flight.
        self.relay_offers.retain(|_, offer| {
            offer.accepted
                || offer.claims.target_device_id != device_id
                || offer.claims.offered_mode != mode
        });
        if self.relay_offers.len() >= MAX_RELAY_OFFERS {
            // Refuse to mint rather than evict: an entry still in here may be
            // the one a device is redeeming right now. The Companion simply
            // sees no offer this turn and the next publish makes room.
            return Ok(None);
        }
        let claims = PendingMobileOfferClaims {
            offer_id: format!("offer_{}", uuid::Uuid::new_v4().simple()),
            opportunity_id: format!("opportunity_{}", uuid::Uuid::new_v4().simple()),
            target_device_id: device_id.to_owned(),
            target_holder_key_thumbprint: peer.holder_key_thumbprint.clone(),
            offered_mode: mode,
            surface: MobileOfferSurface::InApp,
            app_id: self.app_id.clone(),
            call_id: snapshot.call_id.clone(),
            call_epoch: snapshot.call_epoch,
            owner_epoch: snapshot.owner_epoch,
            switchboard_revision: snapshot.switchboard_revision,
            remote_revision: snapshot.remote_revision,
            required_consent_policy_id: snapshot.remote_consent.policy_id.clone(),
            required_consent_policy_version: snapshot.remote_consent.policy_version,
            required_grants: {
                let mut grants = vec![Grant::StateRead, Grant::RtcSignal, relay_mode_grant(mode)];
                if mode == LeaseMode::Takeover {
                    grants.push(Grant::ResumeAokie);
                }
                grants
            },
            issued_at: now,
            expires_at: now.saturating_add(RELAY_OFFER_TTL),
            jti: format!("offerjti_{}", uuid::Uuid::new_v4().simple()),
        };
        // Validate what we are about to assert, exactly as the receiver will.
        // A malformed offer would be caught by the snapshot's own validate and
        // take the whole frame down with it — including the state an active
        // call depends on.
        if claims.validate(now).is_err() {
            return Ok(None);
        }
        let token = self.endpoint_authority.sign(
            &claims
                .signing_bytes()
                .map_err(|_| WorkerError::reconnect("Mobile offer could not be signed"))?,
        );
        let signed = SignedPendingMobileOffer {
            offer: claims.clone(),
            offer_token: token.clone(),
        };
        self.relay_offers.insert(
            claims.offer_id.clone(),
            MintedOffer {
                claims,
                token,
                accepted: false,
            },
        );
        Ok(Some(signed))
    }

    fn assistance_frame(&mut self, radio: &RadioHandle) -> Result<Option<String>, WorkerError> {
        let Some(frame) = crate::assistance::global().pending_frame(&self.app_id) else {
            self.last_assistance_request_sent = None;
            return Ok(None);
        };
        if self.last_assistance_request_sent.as_deref() == Some(frame.request_id.as_str()) {
            return Ok(None);
        }
        if self.relay_carrier && !self.relay_assistance_snapshot_ready() {
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
            return Ok(None);
        }
        if remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.call_epoch
            || remote.owner_epoch != frame.owner_epoch
            || radio.switchboard_revision() != frame.switchboard_revision
            || remote.remote_revision != frame.remote_revision
        {
            return Ok(None);
        }
        match frame.validate(unix_now()?) {
            Ok(()) => {}
            Err(V2ProtocolError::Expired) => return Ok(None),
            Err(_) => return Err(WorkerError::reconnect("assistance request is invalid")),
        }
        serde_json::to_string(&frame)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("assistance request could not be encoded"))
    }

    fn relay_assistance_snapshot_ready(&self) -> bool {
        if self.relay_snapshot_event_id.is_none() {
            return false;
        }
        let mut eligible = self.relay_peers.iter().filter(|(_, peer)| {
            peer.grants.contains(&Grant::StateRead) && peer.grants.contains(&Grant::AssistanceRead)
        });
        let Some((first_device, _)) = eligible.next() else {
            return false;
        };
        self.relay_snapshot_delivered_devices.contains(first_device)
            && eligible
                .all(|(device_id, _)| self.relay_snapshot_delivered_devices.contains(device_id))
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
        RemoteMediaEventKind::RemoteMicrophoneReady => "remote_microphone_ready",
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

#[derive(Clone, Deserialize)]
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
        from_relay_peer: bool,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: Option<&HashSet<Grant>>,
    ) -> Result<Vec<String>, WorkerError> {
        // A relay peer is not the gateway.
        //
        // On the socket every frame was minted by trusted gateway
        // infrastructure, so a protocol violation means this session is
        // genuinely broken and tearing it down is the correct fail-closed
        // response. On the relay the frame was posted by an
        // approved-but-untrusted Companion; honouring its content as a
        // lifecycle signal hands any roster member a one-frame kill switch.
        //
        // The relay therefore enters a narrow authenticated allowlist whose
        // handlers re-check party/device identity, current admission grants,
        // replay keys and immutable call/lease fences. Unknown kinds are
        // dropped before the trusted-gateway handlers below can mutate state.
        if from_relay_peer {
            return self.handle_relay_peer_frame(
                encoded,
                from_party,
                authenticated_subject,
                authenticated_grants.unwrap_or(&HashSet::new()),
                media,
                radio,
            );
        }
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
                self.handle_claim_proposal(notice, media, radio, None)
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

    /// Everything a Companion posts directly to this plugin over the relay.
    ///
    /// Returns `Ok` on every path — including malformed JSON, unknown kinds,
    /// forged proofs and replays — so peer traffic can never terminate the
    /// session. `WorkerError` is used inside only as a typed reason for the
    /// log; it never escapes.
    ///
    /// The actionable set beyond the hello exists because this plugin is the
    /// lease authority on this carrier: it mints what it later honours, so an
    /// unrecognised claim is not a protocol violation to fail on, it is simply
    /// something we never issued. Refusals go back in band as
    /// `plugin_claim_rejected` and cost the session nothing.
    fn handle_relay_peer_frame(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let kind = serde_json::from_str::<Envelope>(encoded)
            .map(|frame| frame.kind)
            .unwrap_or_else(|_| "malformed".into());
        if self.relay_authority_enabled {
            match kind.as_str() {
                "mobile_offer_answer" => {
                    return Ok(self.relay_offer_answer(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                    ));
                }
                "lease_request" => {
                    return Ok(self.relay_lease_request(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "rtc_signal" => {
                    return Ok(self.relay_rtc_signal(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "lease_heartbeat" => {
                    return Ok(self.relay_lease_heartbeat(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "lease_revoke" => {
                    return Ok(self.relay_lease_revoke(
                        encoded,
                        from_party,
                        authenticated_subject,
                        media,
                    ));
                }
                _ => {}
            }
        }
        if kind == "mobile_hello" {
            if let Err(error) = self.accept_mobile_hello(
                encoded,
                from_party,
                authenticated_subject,
                authenticated_grants,
                media,
            ) {
                // Rate-limited, never capped. A lifetime cap would go silent
                // after a handful of lines, and a SYSTEMATICALLY refused
                // Companion (clock skew past the signature window, a roster
                // that has not propagated, an assignment naming another
                // Desktop) retries on a timer — so the cap would be spent in
                // the first minute and every later refusal, including a
                // genuinely new one, would vanish. The plugin log would then
                // show nothing but `relay_no_destination`, which is exactly
                // what a Companion that never spoke at all looks like: the
                // undiagnosable deadlock this whole path exists to escape.
                let due = self
                    .relay_hello_rejection_logged_at
                    .is_none_or(|at| at.elapsed() >= RELAY_HELLO_REJECTION_LOG_INTERVAL);
                if due {
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_hello_rejected suppressed={} detail={}",
                        self.relay_hello_rejections_suppressed,
                        sanitize_status_message(&error.message)
                    );
                    self.relay_hello_rejection_logged_at = Some(Instant::now());
                    self.relay_hello_rejections_suppressed = 0;
                } else {
                    self.relay_hello_rejections_suppressed =
                        self.relay_hello_rejections_suppressed.saturating_add(1);
                }
            }
            return Ok(Vec::new());
        }
        // Nothing is lost by dropping the rest.
        //
        // The gateway-dialect lifecycle notices (`claim_proposal`,
        // `lease_granted`, `lease_renewed`, `lease_revoked`, `claim_decision`)
        // stay dropped ON PURPOSE and must never be admitted here: they assert
        // an authority this carrier has no one to exercise, so honouring one
        // would let an approved-but-untrusted Companion declare its own
        // takeover with a fence of its choosing. On this carrier the plugin
        // mints those itself, above, from live radio truth.
        //
        // `assistance_answer` and the caller-ending pair are dropped for a
        // duller reason: they parse plugin-dialect twins only the gateway
        // produces (MobileAssistanceAnswerFrame carries idempotencyKey where
        // the plugin twin wants deviceId; the caller-ending pair needs a
        // challenge issuer this plugin does not yet have). Both twins are
        // deny_unknown_fields, so the mobile shape cannot decode into the
        // plugin one even by accident. Neither is on the takeover path.
        // Keyed on the SANITIZED code, never the raw kind. A relay frame may be
        // just under the carrier's 1 MiB SSE ceiling and `kind` is peer-supplied
        // string content, so retaining raw kinds would hold up to
        // `MAX_REPORTED_RELAY_KINDS` megabyte-scale strings for the life of the
        // session inside the process that also runs the radio. Keying on what is
        // actually printed bounds retention to the 80-char sanitized form and
        // closes the matching throttle bypass, where distinct raw kinds sharing a
        // sanitized prefix each earned an identical log line.
        let code = sanitize_gateway_code(&kind);
        if self.dropped_relay_kinds.len() < MAX_REPORTED_RELAY_KINDS
            && self.dropped_relay_kinds.insert(code.clone())
        {
            eprintln!(
                "[aokie-plugin][companion] stage=relay_frame_dropped kind={code} detail=The frame is outside the authenticated relay action allowlist"
            );
        }
        Ok(Vec::new())
    }

    /// Admit an owner-approved Companion onto this relay session.
    ///
    /// The carrier already registered the sender as a publish destination when
    /// the frame arrived ([`crate::companion_relay::RelayChannel::learn_route`]),
    /// so this proves the hello and then RE-ARMS authoritative publication.
    /// The re-arm is the load-bearing half: publication is edge-triggered, so a
    /// Companion joining a quiet line would otherwise register successfully and
    /// then receive nothing until the next call.
    fn accept_mobile_hello(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        let hello: MobileHello = parse_gateway_frame(encoded)?;
        // Pins kind, schemaVersion, and that role/appId/subjectId/sessionNonce
        // agree with the proof's own claims.
        hello
            .validate()
            .map_err(|_| WorkerError::reconnect("Companion hello failed contract validation"))?;
        if hello.app_id != self.app_id {
            return Err(WorkerError::rebootstrap(
                "Companion hello crossed application identity",
            ));
        }
        let now = unix_now()?;
        // Signature over the domain-separated canonical claims, plus the
        // bounded signature window.
        hello
            .endpoint_proof
            .verify(now)
            .map_err(|_| WorkerError::reconnect("Companion hello endpoint signature is invalid"))?;
        let claims = &hello.endpoint_proof.claims;
        let approved = self
            .endpoint_authority
            .approved_mobile_keys
            .get(&claims.holder_key_thumbprint)
            .ok_or_else(|| {
                WorkerError::reconnect(
                    "Companion hello signer is absent from the owner-approved roster",
                )
            })?;
        if *approved != hello.endpoint_proof.endpoint_key {
            return Err(WorkerError::reconnect(
                "Companion hello key does not match the owner-approved roster entry",
            ));
        }
        // The mobile role always carries this (the protocol's peer policy makes
        // it mandatory), and the identity service sets it to the assigned
        // plugin's endpoint thumbprint — so it proves the hello was addressed to
        // THIS plugin rather than replayed from a session with another Desktop.
        if claims.expected_peer_key_thumbprint.as_deref()
            != Some(self.endpoint_authority.endpoint_key.thumbprint.as_str())
        {
            return Err(WorkerError::reconnect(
                "Companion hello addresses a different plugin endpoint",
            ));
        }
        let verified_party = relay_party(&claims.holder_key_thumbprint);
        if from_party != Some(verified_party.as_str()) {
            return Err(WorkerError::reconnect(
                "Companion hello arrived from a party other than its proved endpoint",
            ));
        }
        if authenticated_subject != Some(hello.device_id.as_str()) {
            return Err(WorkerError::reconnect(
                "Companion hello subject does not match its authenticated admission",
            ));
        }
        if !authenticated_grants.contains(&Grant::StateRead) {
            // The proof and outer sender identity are already established, so
            // this is an authoritative loss of access rather than an anonymous
            // frame. Revoke anything an older admission left behind before
            // refusing to re-admit the peer.
            self.revoke_relay_device_authority(&hello.device_id, &HashSet::new(), false, media);
            if let Some(peer) = self.relay_peers.get_mut(&hello.device_id) {
                peer.grants.clear();
            }
            return Err(WorkerError::reconnect(
                "Companion hello admission does not grant authoritative state access",
            ));
        }
        self.used_endpoint_jtis
            .retain(|_, expires_at| *expires_at > now);
        if self.used_endpoint_jtis.contains_key(&claims.jti) {
            return Err(WorkerError::reconnect("Companion hello was replayed"));
        }
        if self.used_endpoint_jtis.len() >= MAX_USED_ENDPOINT_JTIS {
            return Err(WorkerError::reconnect(
                "Companion hello replay cache is exhausted",
            ));
        }
        self.used_endpoint_jtis
            .insert(claims.jti.clone(), claims.expires_at);

        // Remember the party, now that its signature, roster membership and
        // addressing have all been proved. This is the ONLY place a relay peer
        // is learned, so every later mint is bound to an identity that got
        // through all of the checks above.
        //
        // A device that re-introduces itself REPLACES its entry. Before that
        // replacement, retire authority whose proof or admission no longer
        // supports it. A fresh session nonce fences every old lease; an
        // unchanged session only loses modes actually removed from its grants.
        if !self.relay_peers.contains_key(&hello.device_id)
            && self.relay_peers.len() >= MAX_RELAY_PEERS
        {
            return Err(WorkerError::reconnect(
                "Companion hello exceeds the relay peer limit for this session",
            ));
        }
        if let Some(previous) = self.relay_peers.get(&hello.device_id).cloned() {
            let session_changed = previous.session_nonce != claims.session_nonce
                || previous.holder_key_thumbprint != claims.holder_key_thumbprint;
            let grants_narrowed = previous
                .grants
                .iter()
                .any(|grant| !authenticated_grants.contains(grant));
            if session_changed || grants_narrowed {
                self.revoke_relay_device_authority(
                    &hello.device_id,
                    authenticated_grants,
                    session_changed,
                    media,
                );
            }
        }
        self.relay_peers.insert(
            hello.device_id.clone(),
            RelayPeer {
                holder_key_thumbprint: claims.holder_key_thumbprint.clone(),
                session_nonce: claims.session_nonce.clone(),
                grants: authenticated_grants.clone(),
            },
        );
        // Hand the carrier the route only after every proof above succeeded.
        // This intentionally REBINDS an existing entry: any older route was
        // learned by an earlier verified session, while an arbitrary frame is
        // never allowed to install one in the first place.
        self.relay_verified_route = Some((hello.device_id.clone(), verified_party.clone()));

        // Re-arm authoritative publication for the party that just joined:
        // clear the idle latch, clear the snapshot fingerprint and its refresh
        // clock, and mark the poll due so the next loop turn publishes.
        //
        // Assistance delivery is per current verified audience. A newly
        // admitted eligible device is owed the current projected snapshot and
        // then the still-pending request; both deliveries are idempotent.
        self.rearm_authoritative_publication();

        // Re-arming publication is only half of going live. The Companion
        // DROPS authoritative state from a peer that has not proved its
        // endpoint key, and it learns that proof from our `plugin_hello`, which
        // the carrier prepends exactly ONCE per party per plugin session. A
        // Companion that already greeted, then restarted as a fresh process,
        // has lost its in-memory proof while our greeting book still records it
        // as greeted — so it would receive every re-armed frame and drop every
        // one of them, for the rest of this plugin session.
        //
        // A verified `mobile_hello` IS the signal that a Companion has started
        // a session with us, so it asks the carrier to fetch a fresh challenge,
        // re-sign this logical plugin session and retire that party's greeting
        // mark. The party is derived from the thumbprint the signature just
        // proved, never from the carrier's routing header or the frame's
        // self-asserted `deviceId`.
        self.relay_regreet_party = Some(verified_party);
        eprintln!(
            "[aokie-plugin][companion] stage=relay_peer_hello device={} detail=An approved Companion joined this relay session and authoritative state was re-armed",
            sanitize_gateway_code(&hello.device_id)
        );
        Ok(())
    }

    /// A device redeeming one of the invitations this plugin published.
    ///
    /// Answering does not consume the offer: the lease request that follows
    /// still has to name it, and that is where the single use is spent. Keeping
    /// the binding alive between the two steps is what lets the request resolve
    /// which device is asking at all — `lease_request` carries no device id.
    fn relay_offer_answer(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileOfferAnswerFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let device_id = frame.target_device_id.as_str();
        let request_id = frame.request_id.as_str();
        // Identity BEFORE budget, in every arm. The budget is per-device state,
        // so spending it on an unauthenticated claim would let one approved
        // Companion exhaust another's allowance simply by naming it — and the
        // victim's own claims would then be refused as flooding.
        if !self.relay_sender_owns_device(from_party, authenticated_subject, device_id) {
            return self.relay_reject(
                device_id,
                request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        // Capture the mode from the offer WE minted before narrowing retires
        // offers the current admission no longer permits. The peer-controlled
        // `offeredMode` must never choose which grant is checked.
        let authoritative_mode = self
            .relay_offers
            .get(&frame.offer_id)
            .map(|offer| offer.claims.offered_mode)
            .unwrap_or(frame.offered_mode);
        self.relay_reconcile_frame_grants(device_id, authenticated_grants, media);
        if !self.relay_mode_is_authorized(device_id, authenticated_grants, authoritative_mode) {
            return self.relay_reject(
                device_id,
                request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        let replay_key = format!("offer\u{1f}{device_id}\u{1f}{}", frame.idempotency_key);
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    device_id,
                    request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::OfferAccepted {
                    encoded: response,
                    mode,
                } if self.relay_mode_is_authorized(device_id, authenticated_grants, mode) => {
                    vec![response]
                }
                RelayReplayResult::OfferAccepted { .. } => self.relay_reject(
                    device_id,
                    request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                ),
                _ => self.relay_reject(
                    device_id,
                    request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        if self.relay_over_budget(device_id) {
            return self.relay_reject(device_id, request_id, "rate_limited", "too many requests");
        }
        if !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                device_id,
                request_id,
                "rate_limited",
                "the replay ledger is full",
            );
        }
        let Ok(now) = unix_now() else {
            return self.relay_reject(device_id, request_id, "offer_unknown", "clock unavailable");
        };
        let Some(minted) = self.relay_offers.get(&frame.offer_id) else {
            return self.relay_reject(
                device_id,
                request_id,
                "offer_unknown",
                "this plugin did not issue that offer",
            );
        };
        let minted_device_id = minted.claims.target_device_id.clone();
        let minted_mode = minted.claims.offered_mode;
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &minted_device_id) {
            return self.relay_reject(
                &minted_device_id,
                request_id,
                "device_unknown",
                "this offer belongs to another authenticated device",
            );
        }
        if !self.relay_mode_is_authorized(&minted_device_id, authenticated_grants, minted_mode) {
            return self.relay_reject(
                &minted_device_id,
                request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        if minted.claims.expires_at <= now {
            self.relay_offers.remove(&frame.offer_id);
            return self.relay_reject(
                device_id,
                request_id,
                "offer_expired",
                "that offer has expired",
            );
        }
        if minted.accepted {
            return self.relay_reject(
                device_id,
                request_id,
                "offer_replayed",
                "that offer was already answered",
            );
        }
        // Every field is re-checked against what WE minted rather than trusted
        // from the frame, and the token comparison is constant time: the token
        // is the secret, and a byte-at-a-time timing leak would hand a peer the
        // ability to forge one.
        let matches = minted.claims.jti == frame.offer_jti
            && minted.claims.target_device_id == frame.target_device_id
            && minted.claims.target_holder_key_thumbprint == frame.target_holder_key_thumbprint
            && minted.claims.offered_mode == frame.offered_mode
            && minted.claims.call_id == frame.call_id
            && minted.claims.call_epoch == frame.call_epoch
            && minted.claims.owner_epoch == frame.owner_epoch
            && tokens_match(&minted.token, &frame.offer_token);
        if !matches {
            return self.relay_reject(
                device_id,
                request_id,
                "offer_unknown",
                "that answer does not match the offer this plugin issued",
            );
        }
        if let Some(minted) = self.relay_offers.get_mut(&frame.offer_id) {
            minted.accepted = true;
        }
        let accepted = PluginOfferAcceptedFrame {
            kind: "plugin_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: frame.target_device_id.clone(),
            request_id: frame.request_id.clone(),
            offer_id: frame.offer_id.clone(),
            offer_jti: frame.offer_jti.clone(),
            offered_mode: frame.offered_mode,
            accepted: true,
        };
        if accepted.validate().is_err() {
            return Vec::new();
        }
        let Ok(response) = serde_json::to_string(&accepted) else {
            return Vec::new();
        };
        self.relay_record_replay(
            replay_key,
            fingerprint,
            device_id.to_owned(),
            RelayReplayResult::OfferAccepted {
                encoded: response.clone(),
                mode: minted_mode,
            },
        );
        vec![response]
    }

    /// Mint a lease, or say plainly why not.
    ///
    /// This is the only place in the plugin that creates media authority, so
    /// every claim in the minted lease comes from live radio truth or from the
    /// proved peer identity — never from the requesting frame. The frame's own
    /// numbers are used for one thing only: to check that the device is asking
    /// about the call state it thinks it is.
    fn relay_lease_request(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<LeaseRequestFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let replay_key = format!(
            "lease\u{1f}{}\u{1f}{}",
            frame.accepted_offer_id, frame.idempotency_key
        );
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            let device_id = replay.device_id.clone();
            if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "device_unknown",
                    "this device has not introduced itself on this relay session",
                );
            }
            let replay_mode = match &replay.result {
                RelayReplayResult::LeaseStatus { lease_id } => {
                    self.relay_leases.get(lease_id).map(|entry| entry.mode)
                }
                _ => None,
            };
            self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::LeaseStatus { lease_id } => {
                    let Some(mode) = replay_mode else {
                        return self.relay_reject(
                            &device_id,
                            &frame.request_id,
                            "lease_unknown",
                            "that lease is no longer current",
                        );
                    };
                    if !self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) {
                        return self.relay_reject(
                            &device_id,
                            &frame.request_id,
                            "grant_required",
                            "the authenticated admission no longer grants this media mode",
                        );
                    }
                    self.relay_current_lease_status(&lease_id, &frame.request_id, media)
                        .unwrap_or_else(|| {
                            self.relay_reject(
                                &device_id,
                                &frame.request_id,
                                "lease_unknown",
                                "that lease is no longer current",
                            )
                        })
                }
                RelayReplayResult::Rejected { encoded } => vec![encoded],
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        // The request names no device, so the accepted offer is what identifies
        // the asker. An unknown or unanswered offer means we have no idea who
        // this is, and there is nothing to address a refusal to either.
        let Some(minted) = self.relay_offers.get(&frame.accepted_offer_id).cloned() else {
            return Vec::new();
        };
        if !minted.accepted || minted.claims.jti != frame.accepted_offer_jti {
            return Vec::new();
        }
        let device_id = minted.claims.target_device_id.clone();
        let offered_mode = minted.claims.offered_mode;
        let request_id = frame.request_id.clone();
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        if !self.relay_mode_is_authorized(&device_id, authenticated_grants, offered_mode) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        if self.relay_over_budget(&device_id) {
            return self.relay_reject(&device_id, &request_id, "rate_limited", "too many requests");
        }
        if !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "rate_limited",
                "the replay ledger is full",
            );
        }
        // Spend the offer here, whatever happens next. A refusal below is a
        // decision about this claim, and letting the same invitation be
        // redeemed again would turn one offer into an unbounded retry budget.
        let Some(spent_offer) = self.relay_offers.remove(&frame.accepted_offer_id) else {
            return Vec::new();
        };
        if frame.mode != offered_mode {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "that mode is not the one the accepted offer allowed",
            );
        }
        // One claimant at a time. Two prepared claims would race for the same
        // physical route, and the loser's soft hold would sit on a caller that
        // the winner is already taking.
        if self.prepared.is_some()
            || self.deferred_prepare.is_some()
            || self.pending_relay_status.is_some()
            || self.relay_leases.len() >= MAX_RELAY_LEASES
            || self
                .relay_leases
                .values()
                .any(|lease| lease.device_id == device_id)
        {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "claimant_busy",
                "another media claim is already in flight for this call",
            );
        }
        let Some(peer) = self.relay_peers.get(&device_id) else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        };
        let mobile_key_thumbprint = peer.holder_key_thumbprint.clone();
        let session_nonce = peer.session_nonce.clone();
        if !self
            .endpoint_authority
            .approved_mobile_keys
            .contains_key(&mobile_key_thumbprint)
        {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "device_unknown",
                "this device is no longer on the owner-approved roster",
            );
        }
        let Some(media) = radio.remote_media() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "stale_call",
                "there is no media endpoint to claim",
            );
        };
        let remote = media.snapshot();
        // The SAME predicate `validate_notice` applies to a gateway-minted
        // lease, applied before minting rather than after receiving.
        if remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.expected_call_epoch
            || remote.owner_epoch != frame.expected_owner_epoch
            || remote.remote_revision != frame.expected_remote_revision
            || radio.switchboard_revision() != frame.expected_switchboard_revision
        {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "stale_call",
                "the physical call moved on before this claim arrived",
            );
        }
        // A fast, honest refusal. The authoritative consent check runs inside
        // the media state lock on every transition, and again per caller-bound
        // frame; this one exists so a device is told why instead of watching a
        // claim die silently later.
        let consented = remote.consent.enabled
            && remote.consent.acknowledged
            && match frame.mode {
                LeaseMode::Monitor => remote.consent.monitor_enabled,
                LeaseMode::Consult => remote.consent.consult_enabled,
                LeaseMode::Takeover => remote.consent.takeover_enabled,
            };
        if !consented {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "consent_required",
                "current disclosure consent does not allow this mode",
            );
        }
        if matches!(frame.mode, LeaseMode::Consult) {
            let fenced = crate::assistance::global()
                .pending_frame(&self.app_id)
                .is_some_and(|assistance| {
                    assistance.call_id == frame.call_id
                        && assistance.call_epoch == remote.call_epoch
                        && assistance.owner_epoch == remote.owner_epoch
                        && assistance.switchboard_revision == radio.switchboard_revision()
                        && assistance.remote_revision == remote.remote_revision
                });
            if !fenced {
                return self.relay_recorded_rejection(
                    &replay_key,
                    &fingerprint,
                    &device_id,
                    &request_id,
                    "assistance_required",
                    "a private consultation needs a current Aokie assistance request",
                );
            }
        }
        let Ok(now) = unix_now() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "stale_call",
                "clock unavailable",
            );
        };
        let phase = if matches!(frame.mode, LeaseMode::Monitor) {
            LeasePhase::Active
        } else {
            LeasePhase::Prepared
        };
        let fence = if matches!(frame.mode, LeaseMode::Takeover) {
            let fence = self.next_takeover_fence;
            self.next_takeover_fence = self.next_takeover_fence.saturating_add(1);
            fence
        } else {
            0
        };
        let lease = LeaseClaims {
            aud: LEASE_AUDIENCE.into(),
            app_id: self.app_id.clone(),
            plugin_id: self.plugin_id.clone(),
            device_id: device_id.clone(),
            plugin_key_thumbprint: self.endpoint_authority.endpoint_key.thumbprint.clone(),
            mobile_key_thumbprint,
            // Physical truth, not the frame's copy of it. The two were just
            // proved equal, and taking the radio's own values means a lease can
            // never describe a call that does not exist.
            call_id: remote.call_id.clone().unwrap_or_default(),
            call_epoch: remote.call_epoch,
            owner_epoch: remote.owner_epoch,
            mode: frame.mode,
            phase,
            tracks: tracks_for(frame.mode, phase),
            expires_at: now.saturating_add(if matches!(phase, LeasePhase::Prepared) {
                RELAY_PREPARED_LEASE_TTL
            } else {
                RELAY_ACTIVE_LEASE_TTL
            }),
            lease_id: format!("lease_{}", uuid::Uuid::new_v4().simple()),
            jti: format!("leasejti_{}", uuid::Uuid::new_v4().simple()),
            fence,
            // The nonce the device proved in its hello. A lease carrying any
            // other value would reject every RTC signal that device sends.
            session_nonce,
            rtc_session_id: frame.rtc_session_id.clone(),
        };
        // Never emit a lease that would not survive the checks a received one
        // faces. Minting it does not make it valid.
        if lease.validate(now).is_err() {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "the requested lease could not be formed safely",
            );
        }
        let Ok(signing_bytes) = lease.signing_bytes() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "the requested lease could not be signed",
            );
        };
        let token = self.endpoint_authority.sign(&signing_bytes);
        let notice = LeaseNotice {
            kind: "lease_granted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.clone(),
            request_id: Some(request_id.clone()),
            lease_token: token.clone(),
            lease: lease.clone(),
        };
        let status = if matches!(phase, LeasePhase::Active) {
            PluginLeaseStatus::Granted
        } else {
            PluginLeaseStatus::Provisional
        };
        let frames =
            self.relay_lease_status(status, &device_id, &request_id, &token, lease.clone(), now);
        let Some(encoded_status) = frames.first().cloned() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "the lease status could not be encoded safely",
            );
        };
        self.relay_leases.insert(
            lease.lease_id.clone(),
            RelayLease {
                lease_id: lease.lease_id.clone(),
                device_id: device_id.clone(),
                request_id: request_id.clone(),
                token: token.clone(),
                current_jti: lease.jti.clone(),
                phase,
                status,
                mode: offered_mode,
            },
        );
        self.relay_record_replay(
            replay_key.clone(),
            fingerprint,
            device_id.clone(),
            RelayReplayResult::LeaseStatus {
                lease_id: lease.lease_id.clone(),
            },
        );
        if matches!(phase, LeasePhase::Active) {
            // Even monitor authority is committed only after delivery. A
            // dropped status must not leave a hidden lease blocking a retry.
            self.pending_relay_status = Some(PendingRelayStatus::MonitorGrant {
                encoded: encoded_status.clone(),
                notice,
                lease_id: lease.lease_id.clone(),
                offer_id: frame.accepted_offer_id.clone(),
                offer: spent_offer,
                replay_key,
            });
        } else {
            // Consult and takeover are minted here but NOT armed. Arming leads
            // to a soft hold on a live caller and therefore waits for delivery.
            self.deferred_prepare = Some(DeferredPrepare {
                encoded: encoded_status.clone(),
                notice: LeaseNotice {
                    kind: "claim_proposal".into(),
                    ..notice
                },
                expected_switchboard_revision: spent_offer.claims.switchboard_revision,
                lease_id: lease.lease_id.clone(),
                device_id: device_id.clone(),
                request_id: request_id.clone(),
                offer_id: frame.accepted_offer_id.clone(),
                offer: spent_offer,
                replay_key,
            });
        }
        eprintln!(
            "[aokie-plugin][takeover] stage=relay_lease_minted device={} call={} mode={:?} phase={:?} fence={} lease={} rtc={}",
            sanitize_gateway_code(&device_id),
            lease.call_id,
            lease.mode,
            lease.phase,
            lease.fence,
            lease.lease_id,
            lease.rtc_session_id
        );
        vec![encoded_status]
    }

    /// A device signalling SDP or ICE for a lease this plugin minted.
    ///
    /// The mobile dialect differs from the plugin's by exactly one field, the
    /// bearer `leaseToken`. Checking that token here and then handing the
    /// remaining fields to the unchanged handler is precisely the translation
    /// the gateway used to perform — every endpoint-signature, roster,
    /// session-nonce and replay check downstream stays untouched.
    fn relay_rtc_signal(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileRtcSignalFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err()
            || frame.app_id != self.app_id
            || frame.plugin_id != self.plugin_id
        {
            return Vec::new();
        }
        let device_id = frame.device_id.clone();
        // RTC signals carry no requestId; the signal id is what correlates a
        // refusal with what provoked it.
        let request_id = frame.signal_id.clone();
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        let recognised_before_narrowing = self
            .relay_leases
            .values()
            .find(|lease| {
                lease.device_id == device_id
                    && lease.current_jti == frame.lease_jti
                    && tokens_match(&lease.token, &frame.lease_token)
            })
            .cloned();
        let retired_before_narrowing = self.retired_prepared_rtc_for_frame(&frame);
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        let replay_key = format!("rtc\u{1f}{device_id}\u{1f}{}", frame.signal_id);
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &request_id,
                    "duplicate_signal",
                    "that signal id was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::RtcAccepted { mode }
                    if self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) =>
                {
                    Vec::new()
                }
                RelayReplayResult::RtcAccepted { .. } => self.relay_reject(
                    &device_id,
                    &request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                ),
                RelayReplayResult::RetiredPreparedRtcDropped {
                    mode, expires_at, ..
                } if expires_at > Instant::now()
                    && self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) =>
                {
                    Vec::new()
                }
                RelayReplayResult::RetiredPreparedRtcDropped { expires_at, .. }
                    if expires_at <= Instant::now() =>
                {
                    self.relay_replays.remove(&replay_key);
                    self.relay_reject(
                        &device_id,
                        &request_id,
                        "lease_unknown",
                        "that retired RTC generation has expired",
                    )
                }
                RelayReplayResult::RetiredPreparedRtcDropped { .. } => self.relay_reject(
                    &device_id,
                    &request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                ),
                RelayReplayResult::TerminalRtcFailure {
                    revocation,
                    rejection,
                } => vec![revocation, rejection],
                RelayReplayResult::Rejected { encoded } => vec![encoded],
                _ => self.relay_reject(
                    &device_id,
                    &request_id,
                    "duplicate_signal",
                    "that signal id belongs to another operation",
                ),
            };
        }
        if self.relay_rtc_over_budget(&device_id) {
            return self.relay_reject(&device_id, &request_id, "rate_limited", "too many requests");
        }
        if !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "rate_limited",
                "the replay ledger is full",
            );
        }
        if let Some(retired) = retired_before_narrowing {
            if !self.relay_mode_is_authorized(&device_id, authenticated_grants, retired.claims.mode)
            {
                return self.relay_reject(
                    &device_id,
                    &request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                );
            }
            if !self.authenticate_retired_prepared_rtc(&frame, &retired) {
                return self.relay_recorded_rejection(
                    &replay_key,
                    &fingerprint,
                    &device_id,
                    &request_id,
                    "lease_unknown",
                    "that retired RTC signal did not match its endpoint proof",
                );
            }
            self.relay_record_replay(
                replay_key,
                fingerprint,
                device_id.clone(),
                RelayReplayResult::RetiredPreparedRtcDropped {
                    lease_id: retired.claims.lease_id.clone(),
                    mode: retired.claims.mode,
                    expires_at: retired.expires_at,
                },
            );
            eprintln!(
                "[aokie-plugin][takeover] stage=retired_prepared_rtc_dropped device={} call={} lease={} rtc={} sdp={} generation={} detail=A late signal for the superseded receive-only generation was replay-fenced and ignored",
                sanitize_gateway_code(&device_id),
                retired.claims.call_id,
                retired.claims.lease_id,
                retired.claims.rtc_session_id,
                retired.sdp_revision,
                retired.transport_generation
            );
            return Vec::new();
        }
        let Some(recognised) = recognised_before_narrowing else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "that lease was not issued by this plugin",
            );
        };
        if !self.relay_mode_is_authorized(&device_id, authenticated_grants, recognised.mode) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        let routed = PluginRtcSignalFrame {
            kind: frame.kind,
            schema_version: frame.schema_version,
            app_id: frame.app_id,
            signal_id: frame.signal_id,
            plugin_id: frame.plugin_id,
            device_id: frame.device_id,
            lease_jti: frame.lease_jti,
            rtc_session_id: frame.rtc_session_id,
            sdp_revision: frame.sdp_revision,
            transport_generation: frame.transport_generation,
            call_id: frame.call_id,
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            fence: frame.fence,
            signal: frame.signal,
        };
        if routed.validate_routed_mobile().is_err() {
            return self.relay_recorded_terminal_rtc_failure(
                &recognised,
                &routed.rtc_session_id,
                "terminal_rtc_contract_failure",
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "that RTC signal failed contract validation",
                media,
            );
        }
        let rtc_session_id = routed.rtc_session_id.clone();
        match self.handle_rtc_signal(routed, media, radio) {
            Ok(()) => {
                self.relay_record_replay(
                    replay_key,
                    fingerprint,
                    device_id,
                    RelayReplayResult::RtcAccepted {
                        mode: recognised.mode,
                    },
                );
                Vec::new()
            }
            Err(error) => {
                // Once an authenticated signal reaches the native RTC path,
                // any failure is terminal for this exact peer/lease. Keeping
                // it heartbeat-renewable would leave a prepared or active
                // authority with no usable media path. ICE-order retries are
                // not currently classified as safe; candidates are accepted
                // only after the exact peer exists.
                self.relay_recorded_terminal_rtc_failure(
                    &recognised,
                    &rtc_session_id,
                    "terminal_rtc_failure",
                    &replay_key,
                    &fingerprint,
                    &device_id,
                    &request_id,
                    &error.message,
                    media,
                )
            }
        }
    }

    /// Extend an active lease.
    ///
    /// A PREPARED lease is deliberately not renewable. Its short life is the
    /// bound on how long a caller can sit in soft hold waiting for a handover
    /// that is not arriving, and a renewable prepare would let a stuck one be
    /// held open indefinitely.
    fn relay_lease_heartbeat(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<LeaseHeartbeatFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let expired_exact = unix_now().ok().and_then(|now| {
            self.relay_leases
                .values()
                .find(|entry| {
                    tokens_match(&entry.token, &frame.lease_token)
                        && self
                            .leases
                            .get(&entry.current_jti)
                            .is_none_or(|claims| claims.expires_at <= now)
                })
                .cloned()
        });
        if let Ok(now) = unix_now() {
            self.expire_relay_leases(now, media);
        }
        self.expire_unbound_active_rebind(Instant::now(), media);
        self.reconcile_relay_media_authority(media);
        if let Some(expired) = expired_exact {
            if !self.relay_sender_owns_device(from_party, authenticated_subject, &expired.device_id)
            {
                return self.relay_reject(
                    &expired.device_id,
                    &frame.request_id,
                    "device_unknown",
                    "this device has not introduced itself on this relay session",
                );
            }
            return self.relay_reject(
                &expired.device_id,
                &frame.request_id,
                "lease_expired",
                "that lease has expired and cannot be renewed",
            );
        }
        let replay_key = format!("heartbeat\u{1f}{}", frame.idempotency_key);
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            let device_id = replay.device_id.clone();
            if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "device_unknown",
                    "this device has not introduced itself on this relay session",
                );
            }
            let replay_mode = match &replay.result {
                RelayReplayResult::HeartbeatStatus { lease_id } => {
                    self.relay_leases.get(lease_id).map(|entry| entry.mode)
                }
                _ => None,
            };
            self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::HeartbeatStatus { lease_id } => {
                    let Some(mode) = replay_mode else {
                        return self.relay_reject(
                            &device_id,
                            &frame.request_id,
                            "lease_unknown",
                            "that lease is no longer current",
                        );
                    };
                    if !self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) {
                        return self.relay_reject(
                            &device_id,
                            &frame.request_id,
                            "grant_required",
                            "the authenticated admission no longer grants this media mode",
                        );
                    }
                    self.relay_current_lease_status(&lease_id, &frame.request_id, media)
                        .unwrap_or_else(|| {
                            self.relay_reject(
                                &device_id,
                                &frame.request_id,
                                "lease_unknown",
                                "that lease is no longer current",
                            )
                        })
                }
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        let Some(lease_id) = self.relay_lease_id_for_token(&frame.lease_token) else {
            // The outer relay subject still tells us which verified device to
            // address after a failed route's token has been retired. Return a
            // typed terminal answer so the client cannot mistake silence for
            // a renewable lease and keep heartbeating forever.
            let Some(device_id) = authenticated_subject.filter(|device_id| {
                self.relay_sender_owns_device(from_party, authenticated_subject, device_id)
            }) else {
                return Vec::new();
            };
            return self.relay_reject(
                device_id,
                &frame.request_id,
                "lease_unknown",
                "that lease is no longer current",
            );
        };
        let Some(entry) = self.relay_leases.get(&lease_id).cloned() else {
            return Vec::new();
        };
        let device_id = entry.device_id.clone();
        let request_id = frame.request_id.clone();
        let current_jti = entry.current_jti.clone();
        let phase = entry.phase;
        let mode = entry.mode;
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        if !self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        if self.relay_over_budget(&device_id) {
            return self.relay_reject(&device_id, &request_id, "rate_limited", "too many requests");
        }
        if !self.relay_replay_has_room(&replay_key) || self.pending_relay_status.is_some() {
            return self.relay_reject(
                &device_id,
                &request_id,
                "rate_limited",
                "another relay transition is still being delivered",
            );
        }
        if matches!(phase, LeasePhase::Prepared) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "phase_not_renewable",
                "a prepared claim must complete rather than be extended",
            );
        }
        // The session's own lease book is the authority on whether this lease
        // is still alive: a peer that failed, or was revoked, has already been
        // purged from it, and renewing on the strength of the relay registry
        // alone would resurrect authority that was deliberately withdrawn.
        let Some(live) = self.leases.get(&current_jti).cloned() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "that lease is no longer live on this session",
            );
        };
        let Ok(now) = unix_now() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "clock unavailable",
            );
        };
        if live.expires_at <= now {
            let expired = LeaseRevokedNotice {
                kind: "lease_revoked".into(),
                schema_version: SCHEMA_VERSION,
                app_id: self.app_id.clone(),
                device_id: device_id.clone(),
                lease_id: live.lease_id.clone(),
                lease_jti: live.jti.clone(),
                call_id: live.call_id.clone(),
                call_epoch: live.call_epoch,
                fence: live.fence,
                reason: "lease_expired".into(),
            };
            let _ = self.handle_lease_revoked(expired, media);
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_expired",
                "that lease expired before the heartbeat arrived",
            );
        }
        let mut renewed = live;
        renewed.jti = format!("leasejti_{}", uuid::Uuid::new_v4().simple());
        renewed.expires_at = now.saturating_add(RELAY_ACTIVE_LEASE_TTL);
        if renewed.validate(now).is_err() {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "the renewed lease could not be formed safely",
            );
        }
        let Ok(signing_bytes) = renewed.signing_bytes() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "the renewed lease could not be signed",
            );
        };
        let token = self.endpoint_authority.sign(&signing_bytes);
        let notice = LeaseNotice {
            kind: "lease_renewed".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.clone(),
            request_id: Some(request_id.clone()),
            lease_token: token.clone(),
            lease: renewed.clone(),
        };
        let frames = self.relay_lease_status(
            PluginLeaseStatus::Renewed,
            &device_id,
            &request_id,
            &token,
            renewed.clone(),
            now,
        );
        let Some(encoded_status) = frames.first().cloned() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "the renewed lease status could not be encoded safely",
            );
        };
        self.relay_record_replay(
            replay_key.clone(),
            fingerprint,
            device_id,
            RelayReplayResult::HeartbeatStatus {
                lease_id: lease_id.clone(),
            },
        );
        self.pending_relay_status = Some(PendingRelayStatus::Renewal {
            encoded: encoded_status.clone(),
            notice,
            lease_id,
            replay_key,
        });
        let _ = (media, radio);
        vec![encoded_status]
    }

    /// A device handing its lease back.
    ///
    /// Strictly de-escalating, and the shortest path from a live takeover back
    /// to the AI answering. Nothing here can extend authority.
    fn relay_lease_revoke(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        media: &RemoteMediaHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<LeaseRevokeFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let Some(lease_id) = self.relay_lease_id_for_token(&frame.lease_token) else {
            return Vec::new();
        };
        let Some(entry) = self.relay_leases.get(&lease_id) else {
            return Vec::new();
        };
        let device_id = entry.device_id.clone();
        let current_jti = entry.current_jti.clone();
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return Vec::new();
        }
        let Some(lease) = self.leases.get(&current_jti).cloned() else {
            // Already gone. Forget our copy and say nothing: the device is
            // asking for a state it is already in.
            self.retire_relay_lease(&lease_id);
            return Vec::new();
        };
        let notice = LeaseRevokedNotice {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id,
            lease_id: lease.lease_id.clone(),
            lease_jti: current_jti,
            call_id: lease.call_id.clone(),
            call_epoch: lease.call_epoch,
            fence: lease.fence,
            reason: frame.reason.clone(),
        };
        let result = self.handle_lease_revoked(notice, media);
        self.retire_relay_lease(&lease_id);
        match result {
            Ok(()) => {
                // The next authoritative Aokie-active snapshot is the mobile's
                // strong completion receipt. Force it on the next loop turn;
                // no separate optimistic revoke-ack frame is needed.
                self.last_snapshot_fingerprint = None;
                self.last_snapshot_sent = None;
                self.next_snapshot_poll = Instant::now();
            }
            Err(error) => {
                eprintln!(
                    "[aokie-plugin][companion] stage=relay_revoke_failed detail={}",
                    sanitize_status_message(&error.message)
                );
            }
        }
        Vec::new()
    }

    /// The lease this token belongs to, compared in constant time.
    fn relay_lease_id_for_token(&self, presented: &str) -> Option<String> {
        self.relay_leases
            .values()
            .find(|lease| tokens_match(&lease.token, presented))
            .map(|lease| lease.lease_id.clone())
    }

    /// Reconcile a verified re-hello with authority issued to its prior
    /// session/admission. Session rotation revokes everything for the device;
    /// same-session grant narrowing revokes only modes no longer authorized.
    fn revoke_relay_device_authority(
        &mut self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        session_changed: bool,
        media: &RemoteMediaHandle,
    ) {
        self.relay_offers.retain(|_, offer| {
            offer.claims.target_device_id != device_id
                || (!session_changed
                    && relay_grants_allow_mode(authenticated_grants, offer.claims.offered_mode))
        });
        let retiring = self
            .relay_leases
            .values()
            .filter(|lease| {
                lease.device_id == device_id
                    && (session_changed
                        || !relay_grants_allow_mode(authenticated_grants, lease.mode))
            })
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in retiring {
            self.revoke_relay_lease_by_id(
                &lease_id,
                if session_changed {
                    "mobile_session_rotated"
                } else {
                    "admission_grants_narrowed"
                },
                media,
            );
        }
        if session_changed {
            self.relay_replays
                .retain(|_, replay| replay.device_id != device_id);
            self.retired_prepared_rtc
                .retain(|_, retired| retired.claims.device_id != device_id);
            // The replacement mobile session did not hold the retired
            // session's in-memory lease. Its fresh authoritative snapshot is
            // the proof it needs; do not deliver terminal notices owed to the
            // predecessor endpoint session into the replacement.
            self.pending_relay_revocations
                .retain(|_, pending| pending.device_id != device_id);
        }
    }

    /// Revoke one plugin-minted relay lease even when it is between delivery
    /// phases. A committed claim takes the normal media-return path; an
    /// uncommitted monitor/provisional claim is simply retired because it never
    /// held caller authority.
    fn revoke_relay_lease_by_id(
        &mut self,
        lease_id: &str,
        reason: &str,
        media: &RemoteMediaHandle,
    ) {
        let entry = self.relay_leases.get(lease_id).cloned();
        let claims = entry
            .as_ref()
            .and_then(|entry| self.leases.get(&entry.current_jti))
            .cloned()
            .or_else(|| {
                self.leases
                    .values()
                    .find(|claims| claims.lease_id == lease_id)
                    .cloned()
            });
        if let Some(claims) = claims {
            let _ = self.handle_lease_revoked(
                LeaseRevokedNotice {
                    kind: "lease_revoked".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: self.app_id.clone(),
                    device_id: claims.device_id.clone(),
                    lease_id: claims.lease_id,
                    lease_jti: claims.jti,
                    call_id: claims.call_id,
                    call_epoch: claims.call_epoch,
                    fence: claims.fence,
                    reason: reason.into(),
                },
                media,
            );
            return;
        }

        // Registry/book skew must never make an exact stable lease
        // unrevocable. Close every matching route and, if the peer has not
        // opened yet, synthesize the binding from the prepared claim.
        let route_ids = self
            .peers
            .iter()
            .filter_map(|(id, route)| {
                (route.binding.lease_id.as_deref() == Some(lease_id)).then(|| id.clone())
            })
            .collect::<Vec<_>>();
        for route_id in route_ids {
            if let Some(route) = self.peers.remove(&route_id) {
                let _ = media.revoke(&route.binding, reason);
                let _ = media.close_peer(&route_id, reason);
            }
        }
        if let Some(prepared) = self
            .prepared
            .as_ref()
            .filter(|prepared| prepared.provisional.lease_id == lease_id)
        {
            let mut binding = binding_for_claims(&prepared.provisional);
            if let Some(owner_epoch) = prepared.confirmed_owner_epoch {
                binding.owner_epoch = owner_epoch;
                binding.mode = match prepared.provisional.mode {
                    LeaseMode::Consult => MediaMode::Consult,
                    LeaseMode::Takeover => MediaMode::Talk,
                    LeaseMode::Monitor => binding.mode,
                };
            }
            let _ = media.revoke(&binding, reason);
        }
        self.leases.retain(|_, claims| claims.lease_id != lease_id);
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.provisional.lease_id == lease_id)
        {
            self.prepared = None;
        }
        self.retire_relay_lease(lease_id);
    }

    /// Retire an ACTIVE consult/takeover lease whose replacement peer never
    /// opened. The deadline is deliberately separate from the renewable lease
    /// TTL: heartbeats prove that an app process is alive, not that a usable
    /// microphone/media path exists.
    fn expire_unbound_active_rebind(&mut self, now: Instant, media: &RemoteMediaHandle) -> bool {
        let Some((lease_id, mode, device_id)) = self.prepared.as_ref().and_then(|prepared| {
            prepared
                .active_rebind_deadline
                .filter(|deadline| *deadline <= now)
                .map(|_| {
                    (
                        prepared.provisional.lease_id.clone(),
                        prepared.provisional.mode,
                        prepared.provisional.device_id.clone(),
                    )
                })
        }) else {
            return false;
        };
        eprintln!(
            "[aokie-plugin][takeover] stage=active_rebind_timeout device={} mode={:?} detail=The active lease never opened its replacement media peer",
            sanitize_gateway_code(&device_id),
            mode
        );
        self.revoke_relay_lease_by_id(&lease_id, "active_peer_not_opened", media);
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.next_snapshot_poll = Instant::now();
        true
    }

    /// Enforce signed lease expiry even when a silent client sends neither a
    /// heartbeat nor a revoke. Without this sweep, a delivered PREPARED lease
    /// that never opens its first peer could occupy the single claimant slot
    /// long after its non-renewable token expired.
    fn expire_relay_leases(&mut self, now: u64, media: &RemoteMediaHandle) -> usize {
        let expired = self
            .relay_leases
            .values()
            .filter(|entry| {
                self.leases
                    .get(&entry.current_jti)
                    .is_none_or(|claims| claims.expires_at <= now)
            })
            .map(|entry| entry.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in &expired {
            self.revoke_relay_lease_by_id(lease_id, "lease_expired", media);
            // A missing session-book entry gives revoke_relay_lease_by_id no
            // claims to route through, so make the registry retirement
            // explicit as the final fail-closed step.
            self.retire_relay_lease(lease_id);
        }
        if !expired.is_empty() {
            self.last_snapshot_fingerprint = None;
            self.last_snapshot_sent = None;
            self.next_snapshot_poll = Instant::now();
        }
        expired.len()
    }

    /// Reconcile gateway authority against the media state, rather than
    /// relying exclusively on its bounded signalling/event queue. Terminal
    /// events are intentionally best-effort; if one is dropped behind ICE,
    /// the snapshot still proves that an ACTIVE talk/consult binding has
    /// disappeared and the corresponding lease must stop renewing.
    fn reconcile_relay_media_authority(&mut self, media: &RemoteMediaHandle) -> usize {
        let remote = media.snapshot();
        let stale = self
            .relay_leases
            .values()
            .filter(|entry| {
                entry.phase == LeasePhase::Active
                    && matches!(entry.mode, LeaseMode::Consult | LeaseMode::Takeover)
            })
            .filter(|entry| {
                self.leases.get(&entry.current_jti).is_none_or(|claims| {
                    remote.call_id.as_deref() != Some(claims.call_id.as_str())
                        || remote.call_epoch != claims.call_epoch
                        || remote.owner_epoch != claims.owner_epoch
                        || remote.talk_device_id.as_deref() != Some(claims.device_id.as_str())
                        || remote.talk_lease_id.as_deref() != Some(claims.lease_id.as_str())
                        || remote.talk_fence != claims.fence
                })
            })
            .map(|entry| entry.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in &stale {
            self.revoke_relay_lease_by_id(lease_id, "media_authority_disappeared", media);
            self.retire_relay_lease(lease_id);
        }
        if !stale.is_empty() {
            self.last_snapshot_fingerprint = None;
            self.last_snapshot_sent = None;
            self.next_snapshot_poll = Instant::now();
        }
        stale.len()
    }

    /// Forget a plugin-minted lease.
    ///
    /// Called wherever the session purges its own lease book, so the relay
    /// registry can never outlive the authority it records — a stale entry
    /// would let a heartbeat renew a lease that failing media already revoked.
    fn retire_relay_lease(&mut self, lease_id: &str) {
        self.relay_leases.remove(lease_id);
        self.retired_prepared_rtc.remove(lease_id);
        self.relay_replays.retain(|_, replay| {
            !matches!(
                &replay.result,
                RelayReplayResult::RetiredPreparedRtcDropped {
                    lease_id: retired_lease_id,
                    ..
                } if retired_lease_id == lease_id
            )
        });
        if self
            .deferred_prepare
            .as_ref()
            .is_some_and(|deferred| deferred.lease_id == lease_id)
        {
            self.deferred_prepare = None;
        }
        if self
            .pending_relay_status
            .as_ref()
            .is_some_and(|pending| pending.lease_id() == lease_id)
        {
            self.pending_relay_status = None;
        }
    }

    fn prune_retired_prepared_rtc(&mut self, now: Instant) {
        self.retired_prepared_rtc
            .retain(|_, retired| retired.expires_at > now);
    }

    /// Remember one superseded PREPARED generation as a drop-only binding.
    ///
    /// The stable lease cap is also the global tombstone cap, and only one
    /// generation per device is retained.  A claimant therefore cannot grow
    /// memory by repeatedly rotating generations or reconnecting.
    fn remember_retired_prepared_rtc(
        &mut self,
        claims: LeaseClaims,
        token: String,
        sdp_revision: u64,
        transport_generation: u64,
    ) {
        let now = Instant::now();
        self.prune_retired_prepared_rtc(now);
        let Ok(now_unix) = unix_now() else {
            return;
        };
        let remaining = claims.expires_at.saturating_sub(now_unix);
        if remaining == 0 {
            return;
        }
        let ttl = RETIRED_PREPARED_RTC_TTL.min(Duration::from_secs(remaining));
        self.retired_prepared_rtc.retain(|_, retired| {
            retired.claims.device_id != claims.device_id
                || retired.claims.lease_id == claims.lease_id
        });
        if self.retired_prepared_rtc.len() >= MAX_RELAY_LEASES
            && !self.retired_prepared_rtc.contains_key(&claims.lease_id)
        {
            if let Some(oldest) = self
                .retired_prepared_rtc
                .iter()
                .min_by_key(|(_, retired)| retired.expires_at)
                .map(|(lease_id, _)| lease_id.clone())
            {
                self.retired_prepared_rtc.remove(&oldest);
            }
        }
        self.retired_prepared_rtc.insert(
            claims.lease_id.clone(),
            RetiredPreparedRtcBinding {
                claims,
                token,
                sdp_revision,
                transport_generation,
                expires_at: now + ttl,
            },
        );
    }

    /// Return the exact retired PREPARED binding named by this frame.
    ///
    /// This is intentionally stricter than finding a stable lease: every
    /// immutable route field and the old bearer token must agree.  Matching
    /// only the lease id would turn the tombstone into an authority alias.
    fn retired_prepared_rtc_for_frame(
        &mut self,
        frame: &MobileRtcSignalFrame,
    ) -> Option<RetiredPreparedRtcBinding> {
        self.prune_retired_prepared_rtc(Instant::now());
        self.retired_prepared_rtc
            .values()
            .find(|retired| {
                matches!(
                    &frame.signal,
                    RtcSignal::Ice { .. } | RtcSignal::IceComplete { .. }
                ) && retired.claims.phase == LeasePhase::Prepared
                    && retired.claims.device_id == frame.device_id
                    && retired.claims.jti == frame.lease_jti
                    && retired.claims.rtc_session_id == frame.rtc_session_id
                    && retired.claims.call_id == frame.call_id
                    && retired.claims.call_epoch == frame.call_epoch
                    && retired.claims.owner_epoch == frame.owner_epoch
                    && retired.claims.fence == frame.fence
                    && retired.sdp_revision == frame.sdp_revision
                    && retired.transport_generation == frame.transport_generation
                    && tokens_match(&retired.token, &frame.lease_token)
            })
            .cloned()
    }

    /// Verify that an exact tombstone match was signed by the same approved
    /// mobile endpoint and session as the retired lease.  A bearer alone is
    /// insufficient even though the result will only be dropped.
    fn authenticate_retired_prepared_rtc(
        &mut self,
        frame: &MobileRtcSignalFrame,
        retired: &RetiredPreparedRtcBinding,
    ) -> bool {
        let Ok(now) = unix_now() else {
            return false;
        };
        let Ok(Some(authentication)) = frame.signal.verify_endpoint_authentication(now) else {
            return false;
        };
        let supplied_key = match &frame.signal {
            RtcSignal::Offer { binding, .. } | RtcSignal::Answer { binding, .. } => {
                &binding.endpoint_key
            }
            RtcSignal::Ice { envelope, .. } | RtcSignal::IceComplete { envelope } => {
                &envelope.endpoint_key
            }
            RtcSignal::Close { .. } => return false,
        };
        let Some(approved_key) = self
            .endpoint_authority
            .approved_mobile_keys
            .get(authentication.holder_key_thumbprint())
        else {
            return false;
        };
        let expected = &retired.claims;
        if supplied_key != approved_key
            || authentication.endpoint_role() != AdmissionRole::Mobile
            || authentication.endpoint_session_nonce() != expected.session_nonce
            || authentication.holder_key_thumbprint() != expected.mobile_key_thumbprint
            || authentication.peer_key_thumbprint() != expected.plugin_key_thumbprint
        {
            return false;
        }
        self.used_endpoint_jtis
            .retain(|_, expires_at| *expires_at > now);
        if self.used_endpoint_jtis.contains_key(authentication.jti())
            || self.used_endpoint_jtis.len() >= MAX_USED_ENDPOINT_JTIS
        {
            return false;
        }
        self.used_endpoint_jtis
            .insert(authentication.jti().to_owned(), authentication.expires_at());
        true
    }

    /// Encode a minted lease for the device that asked for it.
    fn relay_lease_status(
        &self,
        status: PluginLeaseStatus,
        device_id: &str,
        request_id: &str,
        token: &str,
        lease: LeaseClaims,
        now: u64,
    ) -> Vec<String> {
        let frame = PluginLeaseStatusFrame {
            kind: "plugin_lease_status".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.to_owned(),
            request_id: request_id.to_owned(),
            status,
            lease_token: token.to_owned(),
            lease,
        };
        if frame.validate(now).is_err() {
            return self.relay_reject(
                device_id,
                request_id,
                "mode_unavailable",
                "the minted lease failed its own contract validation",
            );
        }
        self.relay_encode(&frame)
    }

    /// Register an exact terminal revoke before asking the relay to carry it.
    ///
    /// This deliberately runs before `send_text`: a delivery result may be
    /// `Dropped`, and the terminal media transition that produced this frame
    /// has already happened.  Retrying the retained bytes is safe; rebuilding
    /// the transition is not.
    fn prepare_relay_delivery(&mut self, encoded: &str) {
        if !self.relay_carrier {
            return;
        }
        let Some(frame) = self.outbound_relay_revocation(encoded) else {
            return;
        };
        self.register_pending_relay_revocation(&frame, encoded, Instant::now());
    }

    fn outbound_relay_revocation(&self, encoded: &str) -> Option<PluginLeaseRevokeFrame> {
        serde_json::from_str::<PluginLeaseRevokeFrame>(encoded)
            .ok()
            .filter(|frame| frame.app_id == self.app_id && frame.validate().is_ok())
    }

    fn prune_pending_relay_revocations(&mut self, now: Instant) {
        self.pending_relay_revocations
            .retain(|_, pending| pending.expires_at > now);
    }

    fn register_pending_relay_revocation(
        &mut self,
        frame: &PluginLeaseRevokeFrame,
        encoded: &str,
        now: Instant,
    ) {
        self.prune_pending_relay_revocations(now);
        if self
            .pending_relay_revocations
            .get(&frame.lease_id)
            .is_some_and(|pending| pending.encoded == encoded)
        {
            return;
        }
        if self.pending_relay_revocations.len() >= MAX_PENDING_RELAY_REVOCATIONS
            && !self.pending_relay_revocations.contains_key(&frame.lease_id)
        {
            if let Some(oldest) = self
                .pending_relay_revocations
                .iter()
                .min_by_key(|(_, pending)| pending.registered_at)
                .map(|(lease_id, _)| lease_id.clone())
            {
                self.pending_relay_revocations.remove(&oldest);
            }
        }
        let next_attempt_at = now + SNAPSHOT_POLL;
        self.pending_relay_revocations.insert(
            frame.lease_id.clone(),
            PendingRelayRevocation {
                encoded: encoded.to_owned(),
                device_id: frame.device_id.clone(),
                registered_at: now,
                next_attempt_at,
                expires_at: now + PENDING_RELAY_REVOCATION_TTL,
            },
        );
        if self.next_snapshot_poll > next_attempt_at {
            self.next_snapshot_poll = next_attempt_at;
        }
    }

    /// Return due notices in first-registration order and move their next due
    /// time forward one normal publication tick. Delivery completion either
    /// clears the exact bytes or re-arms them; neither path touches authority.
    fn due_pending_relay_revocations(&mut self, now: Instant) -> Vec<String> {
        self.prune_pending_relay_revocations(now);
        let mut due_lease_ids = self
            .pending_relay_revocations
            .iter()
            .filter(|(_, pending)| pending.next_attempt_at <= now)
            .map(|(lease_id, pending)| {
                (
                    pending.next_attempt_at,
                    pending.registered_at,
                    lease_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        due_lease_ids.sort_by(|left, right| left.cmp(right));
        due_lease_ids
            .into_iter()
            .take(MAX_PENDING_RELAY_REVOCATIONS_PER_POLL)
            .filter_map(|(_, _, lease_id)| {
                self.pending_relay_revocations
                    .get_mut(&lease_id)
                    .map(|pending| {
                        pending.next_attempt_at = now + SNAPSHOT_POLL;
                        pending.encoded.clone()
                    })
            })
            .collect()
    }

    /// Commit or roll back the exact relay status frame just sent.
    fn finish_relay_delivery(
        &mut self,
        encoded: &str,
        delivery: TransportDelivery,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) {
        if self.relay_carrier {
            if let Some(frame) = self.outbound_relay_revocation(encoded) {
                match delivery {
                    TransportDelivery::Delivered => {
                        let exact = self
                            .pending_relay_revocations
                            .get(&frame.lease_id)
                            .is_some_and(|pending| pending.encoded == encoded);
                        if exact {
                            self.pending_relay_revocations.remove(&frame.lease_id);
                        }
                    }
                    TransportDelivery::Dropped => {
                        let now = Instant::now();
                        self.register_pending_relay_revocation(&frame, encoded, now);
                        if let Some(pending) = self
                            .pending_relay_revocations
                            .get_mut(&frame.lease_id)
                            .filter(|pending| pending.encoded == encoded)
                        {
                            pending.next_attempt_at = now + SNAPSHOT_POLL;
                            if self.next_snapshot_poll > pending.next_attempt_at {
                                self.next_snapshot_poll = pending.next_attempt_at;
                            }
                        }
                    }
                }
            }
        }
        if self.relay_carrier {
            if let Ok(frame) = serde_json::from_str::<PluginSnapshotFrame>(encoded) {
                if self.relay_snapshot_event_id.as_deref() == Some(frame.event_id.as_str()) {
                    if let Some(device_id) = frame.device_id {
                        match delivery {
                            TransportDelivery::Delivered => {
                                self.relay_snapshot_delivered_devices.insert(device_id);
                            }
                            TransportDelivery::Dropped => {
                                self.relay_snapshot_delivered_devices.remove(&device_id);
                            }
                        }
                    }
                }
            }
            if let Ok(frame) = serde_json::from_str::<PluginAssistanceRequestFrame>(encoded) {
                match delivery {
                    TransportDelivery::Delivered => {
                        self.last_assistance_request_sent = Some(frame.request_id);
                    }
                    TransportDelivery::Dropped => {
                        if self.last_assistance_request_sent.as_deref()
                            == Some(frame.request_id.as_str())
                        {
                            self.last_assistance_request_sent = None;
                        }
                    }
                }
            }
        }
        if self.relay_carrier
            && delivery == TransportDelivery::Dropped
            && serde_json::from_str::<Envelope>(encoded)
                .is_ok_and(|frame| frame.kind == "plugin_snapshot")
        {
            // A snapshot is authoritative state, and immediately after revoke
            // it is also the Companion's completion receipt. A terminal 429 or
            // missing target must make it owed again, but at normal poll cadence
            // so a relay outage cannot turn the radio process into a hot loop.
            self.last_snapshot_fingerprint = None;
            self.last_snapshot_sent = None;
            self.next_snapshot_poll = Instant::now() + SNAPSHOT_POLL;
        }
        if self
            .deferred_prepare
            .as_ref()
            .is_some_and(|pending| pending.encoded == encoded)
        {
            let deferred = self
                .deferred_prepare
                .take()
                .expect("checked deferred prepare");
            if delivery == TransportDelivery::Dropped {
                self.relay_leases.remove(&deferred.lease_id);
                self.relay_replays.remove(&deferred.replay_key);
                self.relay_offers.insert(deferred.offer_id, deferred.offer);
                eprintln!(
                    "[aokie-plugin][takeover] stage=relay_prepare_not_delivered device={} request={} detail=The provisional grant was dropped before delivery; the caller stayed with Aokie",
                    sanitize_gateway_code(&deferred.device_id),
                    sanitize_gateway_code(&deferred.request_id)
                );
                return;
            }
            let lease_id = deferred.lease_id.clone();
            let device_id = deferred.device_id.clone();
            let request_id = deferred.request_id.clone();
            if let Err(error) = self.handle_claim_proposal(
                deferred.notice,
                media,
                radio,
                Some(deferred.expected_switchboard_revision),
            ) {
                self.leases.retain(|_, claims| claims.lease_id != lease_id);
                self.prepared = None;
                self.relay_replays.remove(&deferred.replay_key);
                self.retire_relay_lease(&lease_id);
                eprintln!(
                    "[aokie-plugin][takeover] stage=relay_prepare_failed device={} request={} detail={}",
                    sanitize_gateway_code(&device_id),
                    sanitize_gateway_code(&request_id),
                    sanitize_status_message(&error.message)
                );
            }
            return;
        }

        if !self
            .pending_relay_status
            .as_ref()
            .is_some_and(|pending| pending.encoded() == encoded)
        {
            return;
        }
        let pending = self
            .pending_relay_status
            .take()
            .expect("checked relay status transition");
        match pending {
            PendingRelayStatus::MonitorGrant {
                notice,
                lease_id,
                offer_id,
                offer,
                replay_key,
                ..
            } => {
                if delivery == TransportDelivery::Dropped {
                    self.relay_leases.remove(&lease_id);
                    self.relay_replays.remove(&replay_key);
                    self.relay_offers.insert(offer_id, offer);
                    return;
                }
                if let Err(error) = self.handle_lease_granted(notice, radio) {
                    self.relay_replays.remove(&replay_key);
                    self.retire_relay_lease(&lease_id);
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_monitor_commit_failed detail={}",
                        sanitize_status_message(&error.message)
                    );
                }
            }
            PendingRelayStatus::Renewal {
                notice,
                lease_id,
                replay_key,
                ..
            } => {
                if delivery == TransportDelivery::Dropped {
                    self.relay_replays.remove(&replay_key);
                    return;
                }
                let renewed = notice.lease.clone();
                let token = notice.lease_token.clone();
                if let Err(error) = self.handle_lease_renewed(notice, media, radio) {
                    self.relay_replays.remove(&replay_key);
                    self.retire_relay_lease(&lease_id);
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_renewal_commit_failed detail={}",
                        sanitize_status_message(&error.message)
                    );
                    return;
                }
                if let Some(entry) = self.relay_leases.get_mut(&lease_id) {
                    entry.current_jti = renewed.jti;
                    entry.token = token;
                    entry.phase = renewed.phase;
                    entry.status = PluginLeaseStatus::Renewed;
                }
            }
            PendingRelayStatus::Active {
                lease_id,
                claims,
                token,
                ..
            } => {
                if delivery == TransportDelivery::Dropped {
                    if let Some(entry) = self.relay_leases.get(&lease_id).cloned() {
                        if let Some(current) = self.leases.get(&entry.current_jti).cloned() {
                            let _ = self.handle_lease_revoked(
                                LeaseRevokedNotice {
                                    kind: "lease_revoked".into(),
                                    schema_version: SCHEMA_VERSION,
                                    app_id: self.app_id.clone(),
                                    device_id: entry.device_id,
                                    lease_id: current.lease_id,
                                    lease_jti: current.jti,
                                    call_id: current.call_id,
                                    call_epoch: current.call_epoch,
                                    fence: current.fence,
                                    reason: "active_status_not_delivered".into(),
                                },
                                media,
                            );
                        } else {
                            self.retire_relay_lease(&lease_id);
                        }
                    }
                    return;
                }
                // Preserve only the exact PREPARED receive-only generation
                // that this delivered ACTIVE status supersedes.  This is a
                // short-lived drop-only tombstone, never an alternate live
                // token/JTI for the stable lease.
                let retired = self.relay_leases.get(&lease_id).and_then(|entry| {
                    self.prepared
                        .as_ref()
                        .filter(|prepared| prepared.provisional.lease_id == lease_id)
                        .filter(|prepared| {
                            entry.phase == LeasePhase::Prepared
                                && prepared.provisional.phase == LeasePhase::Prepared
                                && entry.current_jti == prepared.provisional.jti
                                && prepared.provisional_sdp_revision > 0
                                && prepared.provisional_transport_generation > 0
                        })
                        .map(|prepared| {
                            (
                                prepared.provisional.clone(),
                                entry.token.clone(),
                                prepared.provisional_sdp_revision,
                                prepared.provisional_transport_generation,
                            )
                        })
                });
                if let Some((claims, token, sdp_revision, transport_generation)) = retired {
                    self.remember_retired_prepared_rtc(
                        claims,
                        token,
                        sdp_revision,
                        transport_generation,
                    );
                }
                self.leases
                    .retain(|_, existing| existing.lease_id != lease_id);
                self.leases.insert(claims.jti.clone(), claims.clone());
                if let Some(entry) = self.relay_leases.get_mut(&lease_id) {
                    entry.current_jti = claims.jti;
                    entry.token = token;
                    entry.phase = LeasePhase::Active;
                    entry.status = PluginLeaseStatus::Active;
                }
                if let Some(prepared) = self
                    .prepared
                    .as_mut()
                    .filter(|prepared| prepared.provisional.lease_id == lease_id)
                {
                    prepared.active_rebind_deadline = match claims.mode {
                        LeaseMode::Consult => {
                            Some(Instant::now() + RELAY_CONSULT_ACTIVE_REBIND_TIMEOUT)
                        }
                        LeaseMode::Takeover => {
                            Some(Instant::now() + RELAY_TAKEOVER_ACTIVE_REBIND_TIMEOUT)
                        }
                        LeaseMode::Monitor => None,
                    };
                }
            }
        }
    }

    /// Encode one typed, in-band refusal.
    ///
    /// Returns an empty vec if the refusal itself cannot be built, because the
    /// alternative — propagating an error — would let a peer that sent
    /// something unencodable take down a session carrying a live call.
    fn relay_reject(
        &self,
        device_id: &str,
        request_id: &str,
        code: &str,
        message: &str,
    ) -> Vec<String> {
        let frame = PluginClaimRejectedFrame {
            kind: "plugin_claim_rejected".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.to_owned(),
            request_id: request_id.to_owned(),
            code: sanitize_gateway_code(code),
            message: sanitize_status_message(message),
        };
        eprintln!(
            "[aokie-plugin][companion] stage=relay_claim_rejected device={} code={} detail={}",
            sanitize_gateway_code(device_id),
            frame.code,
            frame.message
        );
        if frame.validate().is_err() {
            return Vec::new();
        }
        serde_json::to_string(&frame)
            .map(|encoded| vec![encoded])
            .unwrap_or_default()
    }

    /// Emit and remember a terminal refusal for an offer that was already
    /// consumed. The relay is at-least-once and a successful POST only means
    /// mailbox acceptance, so the peer must be able to repeat the same request
    /// and receive the byte-equivalent decision after a dropped first attempt.
    fn relay_recorded_rejection(
        &mut self,
        replay_key: &str,
        fingerprint: &str,
        device_id: &str,
        request_id: &str,
        code: &str,
        message: &str,
    ) -> Vec<String> {
        let frames = self.relay_reject(device_id, request_id, code, message);
        if let Some(encoded) = frames.first() {
            self.relay_record_replay(
                replay_key.to_owned(),
                fingerprint.to_owned(),
                device_id.to_owned(),
                RelayReplayResult::Rejected {
                    encoded: encoded.clone(),
                },
            );
        }
        frames
    }

    /// Fail one authenticated RTC operation without leaving either side with
    /// an ambiguous media lease.
    ///
    /// The generic claim rejection is retained for operation correlation, but
    /// it intentionally carries no lease identity and therefore cannot prove
    /// authority return to the Companion.  Pair the first refusal with an
    /// exact, fully-fenced plugin revocation. Relay egress posts each frame
    /// separately, so the replay result preserves both byte-for-byte with the
    /// authoritative revocation first. Replaying them does not re-run the
    /// terminal state transition; mobile applies duplicate revokes through its
    /// completed-revocation tombstone.
    #[allow(clippy::too_many_arguments)]
    fn relay_recorded_terminal_rtc_failure(
        &mut self,
        recognised: &RelayLease,
        rtc_session_id: &str,
        reason: &str,
        replay_key: &str,
        fingerprint: &str,
        device_id: &str,
        request_id: &str,
        message: &str,
        media: &RemoteMediaHandle,
    ) -> Vec<String> {
        let revocation = match self.fail_relay_rtc_authority(
            recognised,
            rtc_session_id,
            reason,
            media,
        ) {
            Ok(encoded) => encoded,
            Err(error) => {
                eprintln!(
                    "[aokie-plugin][takeover] stage=terminal_rtc_revoke_encode_failed rtc={} detail={}",
                    sanitize_gateway_code(rtc_session_id),
                    sanitize_status_message(&error.message)
                );
                None
            }
        };
        let rejection = self
            .relay_reject(device_id, request_id, "lease_unknown", message)
            .into_iter()
            .next();
        match (revocation, rejection) {
            (Some(revocation), Some(rejection)) => {
                self.relay_record_replay(
                    replay_key.to_owned(),
                    fingerprint.to_owned(),
                    device_id.to_owned(),
                    RelayReplayResult::TerminalRtcFailure {
                        revocation: revocation.clone(),
                        rejection: rejection.clone(),
                    },
                );
                vec![revocation, rejection]
            }
            (None, Some(rejection)) => {
                self.relay_record_replay(
                    replay_key.to_owned(),
                    fingerprint.to_owned(),
                    device_id.to_owned(),
                    RelayReplayResult::Rejected {
                        encoded: rejection.clone(),
                    },
                );
                vec![rejection]
            }
            (Some(revocation), None) => vec![revocation],
            (None, None) => Vec::new(),
        }
    }

    /// Retire only the lease authenticated before grant narrowing.  A supplied
    /// rtcSessionId can name another peer, so it is safe to call `fail_peer`
    /// only when that route also matches the recognised device, JTI and stable
    /// lease.  Otherwise revoke the recognised lease without touching the
    /// foreign route and construct its notice from the plugin-minted claims.
    fn fail_relay_rtc_authority(
        &mut self,
        recognised: &RelayLease,
        rtc_session_id: &str,
        reason: &str,
        media: &RemoteMediaHandle,
    ) -> Result<Option<String>, WorkerError> {
        let claims = self
            .leases
            .get(&recognised.current_jti)
            .filter(|claims| {
                claims.device_id == recognised.device_id
                    && claims.lease_id == recognised.lease_id
                    && claims.jti == recognised.current_jti
            })
            .cloned();
        let expected_binding = claims.as_ref().map(binding_for_claims);
        let exact_peer = self.peers.get(rtc_session_id).is_some_and(|route| {
            route.device_id == recognised.device_id
                && route.lease_jti == recognised.current_jti
                && route.binding.lease_id.as_deref() == Some(recognised.lease_id.as_str())
                && expected_binding
                    .as_ref()
                    .is_some_and(|binding| route.binding == *binding)
        });
        if exact_peer {
            return self.fail_peer(rtc_session_id, reason, media);
        }

        self.revoke_relay_lease_by_id(&recognised.lease_id, reason, media);
        claims
            .map(|claims| self.encode_failed_lease_revocation(&claims, reason))
            .transpose()
    }

    fn encode_failed_lease_revocation(
        &self,
        claims: &LeaseClaims,
        reason: &str,
    ) -> Result<String, WorkerError> {
        let frame = PluginLeaseRevokeFrame {
            kind: "plugin_lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: claims.device_id.clone(),
            lease_id: claims.lease_id.clone(),
            lease_jti: claims.jti.clone(),
            call_id: claims.call_id.clone(),
            call_epoch: claims.call_epoch,
            fence: claims.fence,
            reason: reason.into(),
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Media revocation frame is invalid"))?;
        serde_json::to_string(&frame)
            .map_err(|_| WorkerError::reconnect("Media revocation could not be encoded"))
    }

    /// Encode one outbound relay frame, dropping it rather than failing.
    fn relay_encode<T: Serialize>(&self, frame: &T) -> Vec<String> {
        serde_json::to_string(frame)
            .map(|encoded| vec![encoded])
            .unwrap_or_default()
    }

    /// Whether this device has already spent its control-request budget.
    ///
    /// A roster member that misbehaves — or simply loops — must not be able to
    /// drive minting, signing and radio snapshots at whatever rate it likes on
    /// the process that also runs the radio.
    fn relay_over_budget(&mut self, device_id: &str) -> bool {
        Self::relay_budget_over(
            &mut self.relay_request_budget,
            device_id,
            RELAY_REQUEST_BUDGET,
        )
    }

    /// Whether this device has already spent its independent RTC trickle
    /// budget.  Keeping this separate is essential: healthy ICE gathering is
    /// bursty, while offer/lease/heartbeat controls are not.
    fn relay_rtc_over_budget(&mut self, device_id: &str) -> bool {
        Self::relay_budget_over(
            &mut self.relay_rtc_signal_budget,
            device_id,
            RELAY_RTC_SIGNAL_BUDGET,
        )
    }

    fn relay_budget_over(
        budget: &mut HashMap<String, (Instant, u32)>,
        device_id: &str,
        limit: u32,
    ) -> bool {
        let now = Instant::now();
        budget.retain(|_, (started, _)| now.duration_since(*started) < RELAY_REQUEST_WINDOW);
        if budget.len() >= MAX_RELAY_PEERS && !budget.contains_key(device_id) {
            return true;
        }
        let entry = budget.entry(device_id.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= RELAY_REQUEST_WINDOW {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 > limit
    }

    fn relay_prune_replays(&mut self) {
        let now = Instant::now();
        self.relay_replays.retain(|_, replay| {
            now.duration_since(replay.seen_at) < RELAY_REPLAY_TTL
                && !matches!(
                    &replay.result,
                    RelayReplayResult::RetiredPreparedRtcDropped { expires_at, .. }
                        if *expires_at <= now
                )
        });
    }

    fn relay_replay(&mut self, key: &str) -> Option<RelayReplay> {
        self.relay_prune_replays();
        self.relay_replays.get(key).cloned()
    }

    fn relay_replay_has_room(&mut self, key: &str) -> bool {
        self.relay_prune_replays();
        self.relay_replays.contains_key(key) || self.relay_replays.len() < MAX_RELAY_REPLAYS
    }

    fn relay_record_replay(
        &mut self,
        key: String,
        fingerprint: String,
        device_id: String,
        result: RelayReplayResult,
    ) {
        self.relay_prune_replays();
        if self.relay_replays.len() < MAX_RELAY_REPLAYS || self.relay_replays.contains_key(&key) {
            self.relay_replays.insert(
                key,
                RelayReplay {
                    fingerprint,
                    device_id,
                    result,
                    seen_at: Instant::now(),
                },
            );
        }
    }

    /// Re-encode the authority this lease currently holds for an exact retry.
    fn relay_current_lease_status(
        &mut self,
        lease_id: &str,
        request_id: &str,
        media: &RemoteMediaHandle,
    ) -> Option<Vec<String>> {
        let entry = self.relay_leases.get(lease_id)?.clone();
        let claims = self.leases.get(&entry.current_jti)?.clone();
        let now = unix_now().ok()?;
        if claims.expires_at <= now {
            self.revoke_relay_lease_by_id(lease_id, "lease_expired", media);
            return None;
        }
        Some(self.relay_lease_status(
            entry.status,
            &entry.device_id,
            request_id,
            &entry.token,
            claims,
            now,
        ))
    }

    /// Whether the party that posted this frame is the device it claims to be.
    ///
    /// Snapshot projection prevents one device receiving another's offer, but
    /// identity is still enforced at the action boundary: copied/stale frames
    /// or a future projection regression cannot occupy the claimant slot and
    /// drive a soft hold on a live caller in someone else's name.
    fn relay_sender_owns_device(
        &self,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        device_id: &str,
    ) -> bool {
        let Some(peer) = self.relay_peers.get(device_id) else {
            return false;
        };
        authenticated_subject == Some(device_id)
            && match from_party {
                // Absent only on carriers with no per-party addressing, which are
                // exactly the carriers that never reach this code.
                None => false,
                Some(party) => party == relay_party(&peer.holder_key_thumbprint),
            }
    }

    /// Apply only NARROWING observed in this frame's authenticated admission.
    ///
    /// A re-hello is required to broaden authority, but waiting for a re-hello
    /// to notice revocation would let a custom client keep an active takeover
    /// until its lease TTL. Every proved device frame can safely reduce the
    /// stored set and immediately return unsupported caller routes to Aokie.
    fn relay_reconcile_frame_grants(
        &mut self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
    ) {
        let Some(previous) = self.relay_peers.get(device_id) else {
            return;
        };
        let narrowed = previous
            .grants
            .intersection(authenticated_grants)
            .copied()
            .collect::<HashSet<_>>();
        if narrowed == previous.grants {
            return;
        }
        self.revoke_relay_device_authority(device_id, &narrowed, false, media);
        if let Some(peer) = self.relay_peers.get_mut(device_id) {
            peer.grants = narrowed;
        }
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.next_snapshot_poll = Instant::now();
    }

    /// Require the intersection of the verified hello's exact admission and
    /// the authenticated metadata on THIS frame. A stale/broadened payload can
    /// therefore neither preserve nor manufacture authority.
    fn relay_mode_is_authorized(
        &self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        mode: LeaseMode,
    ) -> bool {
        self.relay_peers
            .get(device_id)
            .is_some_and(|peer| relay_grants_allow_mode(&peer.grants, mode))
            && relay_grants_allow_mode(authenticated_grants, mode)
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
        expected_switchboard_revision: Option<u64>,
    ) -> Result<Vec<String>, WorkerError> {
        self.validate_notice(&notice, radio)?;
        if radio.switch_in_flight()
            || expected_switchboard_revision
                .is_some_and(|revision| revision != radio.switchboard_revision())
        {
            return Err(WorkerError::reconnect(
                "Companion claim proposal crossed a physical switchboard transition",
            ));
        }
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
            expected_switchboard_revision,
            confirmed_owner_epoch: None,
            provisional_sdp_revision: 0,
            provisional_transport_generation: 0,
            decision_sent: false,
            active_rebind_deadline: None,
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
                let lease_id = route
                    .lease_id
                    .clone()
                    .ok_or_else(|| WorkerError::reconnect("Companion RTC route omitted leaseId"))?;
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
                // Close is a terminal media statement, not merely a peer
                // transport hint. Purge every JTI for the stable lease and
                // its claimant/relay registry now; the ensuing native terminal
                // event may be coalesced or dropped and must not be required to
                // make future heartbeats fail.
                self.leases.retain(|_, claims| claims.lease_id != lease_id);
                if self
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.provisional.lease_id == lease_id)
                {
                    self.prepared = None;
                }
                self.retire_relay_lease(&lease_id);
                self.last_snapshot_fingerprint = None;
                self.last_snapshot_sent = None;
                self.next_snapshot_poll = Instant::now();
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
        // Capture this BEFORE reading the public switch marker/revision. If a
        // CHLD wins after either read but before the media manager dequeues the
        // open, its dedicated epoch changes and the queued peer fails closed.
        // Capturing after admission would bless the already-started switch.
        let admitted_switch_epoch = media.capture_aokie_switch_epoch().map_err(|_| {
            WorkerError::reconnect("Companion media switchboard proof is unavailable")
        })?;
        if radio.switch_in_flight() {
            return Err(WorkerError::reconnect(
                "Companion media offer arrived while the physical switchboard was moving",
            ));
        }
        if let Some((lease_id, expected_revision)) = self.prepared.as_ref().and_then(|prepared| {
            prepared
                .expected_switchboard_revision
                .map(|revision| (prepared.provisional.lease_id.clone(), revision))
        }) {
            if radio.switchboard_revision() != expected_revision {
                self.revoke_relay_lease_by_id(&lease_id, "switchboard_changed_before_media", media);
                return Err(WorkerError::reconnect(
                    "Companion media offer crossed a physical switchboard transition",
                ));
            }
        }
        if let Some(lease_id) = self.prepared.as_ref().and_then(|prepared| {
            prepared
                .active_rebind_deadline
                .filter(|deadline| *deadline <= Instant::now())
                .map(|_| prepared.provisional.lease_id.clone())
        }) {
            self.revoke_relay_lease_by_id(&lease_id, "active_peer_not_opened", media);
            return Err(WorkerError::reconnect(
                "Active Companion media offer arrived after its bounded handoff window",
            ));
        }
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
                expected_switch_epoch: (binding.mode != MediaMode::Monitor)
                    .then_some(admitted_switch_epoch),
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
        if claims.phase == LeasePhase::Active {
            if let Some(prepared) = self
                .prepared
                .as_mut()
                .filter(|prepared| prepared.provisional.lease_id == claims.lease_id)
            {
                // The native media state now owns its own non-renewable PCM
                // readiness deadline. Retire the earlier "active status but
                // no replacement peer" guard so the two stages cannot race.
                prepared.active_rebind_deadline = None;
            }
        }
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
                remote_microphone_ready: false,
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
        // A revoked lease must leave the relay registry too, or its token would
        // still be recognised and a heartbeat could renew authority that was
        // just withdrawn.
        self.retire_relay_lease(&notice.lease_id);
        Ok(())
    }

    fn drain_media_events(
        &mut self,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let mut outbound = Vec::new();
        for event in media.drain_events(64) {
            let event_detail = match &event.kind {
                RemoteMediaEventKind::ProtocolViolation { message } => {
                    sanitize_status_message(message)
                }
                RemoteMediaEventKind::Error { operation, message } => format!(
                    "operation={} message={}",
                    sanitize_gateway_code(operation),
                    sanitize_status_message(message)
                ),
                RemoteMediaEventKind::ConnectionState { state } => {
                    format!("state={}", sanitize_gateway_code(state))
                }
                RemoteMediaEventKind::Closed { reason }
                | RemoteMediaEventKind::ReturningToAokie { reason } => {
                    format!("reason={}", sanitize_status_message(reason))
                }
                _ => "none".to_string(),
            };
            eprintln!(
                "[aokie-plugin][takeover] stage=media_event call={} owner_epoch={} rtc={} event={} detail={}",
                event.call_id,
                event.owner_epoch,
                event.rtc_session_id,
                remote_media_event_kind(&event.kind),
                event_detail,
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
                RemoteMediaEventKind::RemoteMicrophoneReady => {
                    if let Some(route) = self.route_for_event_mut(&event) {
                        // This event came from first decoded PCM on the exact
                        // direct WebRTC Talk peer. Its frame remains
                        // quarantined until the radio opens the route.
                        route.remote_microphone_ready = true;
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
                MediaMode::Consult if route.remote_audio_ready && route.remote_microphone_ready => {
                    Some((2_u8, route.binding.clone(), route.lease_ttl_ms))
                }
                MediaMode::Talk if route.remote_audio_ready && route.remote_microphone_ready => {
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
        // On the relay there is no gateway to address a decision to, and no one
        // else to promote the prepared claim. The plugin consumes its own
        // decision instead: it mints the ACTIVE lease from the epoch the radio
        // just confirmed and hands it to the device, which is what lets the
        // Companion send the second, microphone-bearing offer.
        //
        // Everything the socket path does either side of this stays: the
        // ownerEpoch must still have strictly advanced above, and the
        // receive-only peer is still closed below so no active binding can be
        // reached without a fresh higher-generation offer.
        let relay_status = if self.relay_carrier && self.relay_authority_enabled {
            let lease_id = prepared.provisional.lease_id.clone();
            let mut active = prepared.provisional.clone();
            active.owner_epoch = confirmed_owner_epoch;
            active.phase = LeasePhase::Active;
            active.tracks = tracks_for(active.mode, LeasePhase::Active);
            active.jti = format!("leasejti_{}", uuid::Uuid::new_v4().simple());
            active.expires_at = unix_now()?.saturating_add(RELAY_ACTIVE_LEASE_TTL);
            Some((lease_id, active))
        } else {
            None
        };
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
        if let Some((lease_id, active)) = relay_status {
            if self.pending_relay_status.is_some() {
                return Err(WorkerError::reconnect(
                    "Another relay lease status is still awaiting delivery",
                ));
            }
            let now = unix_now()?;
            let signing_bytes = active
                .signing_bytes()
                .map_err(|_| WorkerError::reconnect("Active lease could not be signed"))?;
            let token = self.endpoint_authority.sign(&signing_bytes);
            let (device_id, request_id) = match self.relay_leases.get(&lease_id) {
                Some(entry) => (entry.device_id.clone(), entry.request_id.clone()),
                // The registry lost this lease while the radio was preparing,
                // which means something already revoked it. Say nothing rather
                // than hand out authority the session no longer holds.
                None => return Ok(None),
            };
            let encoded = self
                .relay_lease_status(
                    PluginLeaseStatus::Active,
                    &device_id,
                    &request_id,
                    &token,
                    active.clone(),
                    now,
                )
                .into_iter()
                .next();
            if let Some(encoded) = encoded {
                self.pending_relay_status = Some(PendingRelayStatus::Active {
                    encoded: encoded.clone(),
                    lease_id,
                    claims: active,
                    token,
                });
                return Ok(Some(encoded));
            }
            return Ok(None);
        }
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
        // Same reasoning as the refusal above, for the relay's own registry: a
        // failed route's token must stop being recognised at once, so a
        // heartbeat cannot re-extend a lease whose media has already gone.
        self.retire_relay_lease(&lease_id);
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
