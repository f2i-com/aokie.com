//! V2 Companion signalling gateway.
//!
//! This module is deliberately signalling-only.  SDP and ICE are routed to a
//! single authenticated endpoint; RTP/SRTP never enters this service.

use super::{header_id, Admission, AdmissionRegistry, AdmissionRole, Gateway, MAX_MESSAGE_BYTES};
use aokie_protocol::v2::{
    parse_mobile_frame, parse_plugin_frame, tracks_for, AdmissionClaims,
    AdmissionRole as TokenAdmissionRole, AuthoritativeCallSnapshot, EndCallerChallengeFrame,
    EndCallerOutcome, EndpointChallengeFrame, EndpointPublicKey, Grant, LeaseClaims,
    LeaseHeartbeatFrame, LeaseMode, LeasePhase, LeaseRequestFrame, LeaseRevokeFrame,
    MobileAssistanceAnswerFrame, MobileEndCallerChallengeRequestFrame, MobileEndCallerConfirmFrame,
    MobileHello, MobileIdleSyncFrame, MobileInbound, MobileOfferAnswerFrame, MobileOfferSurface,
    MobileRtcSignalFrame, MobileSnapshotFrame, PendingMobileOfferClaims,
    PluginAssistanceAnswerFrame, PluginAssistanceRequestFrame, PluginClaimDecisionFrame,
    PluginEndCallerExecuteFrame, PluginEndCallerResultFrame, PluginHello, PluginIdleFrame,
    PluginInbound, PluginLeaseRevokeFrame, PluginRtcSignalFrame, PluginSnapshotFrame,
    ProjectedCallSnapshot, RtcSignal, ServiceMode, SignedHelloProof, SignedPendingMobileOffer,
    TelephonyState, ENDPOINT_PROOF_MAX_LIFETIME, LEASE_AUDIENCE, MAX_SAFE_INTEGER,
    MOBILE_OFFER_MAX_LIFETIME, SCHEMA_VERSION,
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, watch, Mutex};

const OUTBOUND_CAPACITY: usize = 32;
const MAX_APPS: usize = 1024;
const MAX_MOBILES_PER_APP: usize = 16;
const MAX_LEASES_PER_APP: usize = 64;
const IDEMPOTENCY_CAPACITY: usize = 512;
const SIGNAL_DEDUPE_CAPACITY: usize = 1024;
const GENERAL_RATE_LIMIT: u32 = 240;
const RTC_RATE_LIMIT: u32 = 120;
const RATE_WINDOW: Duration = Duration::from_secs(10);
const LEASE_TTL: u64 = 20;
const MAX_ADMISSION_TTL: u64 = 300;
const MAX_CLAIMED_ADMISSIONS: usize = 4096;
const MAX_USED_ENDPOINT_JTIS: usize = 16_384;
const MAX_ASSISTANCE_TTL: u64 = 300;
const END_CALLER_CONFIRM_TTL: u64 = 12;
const END_CALLER_HISTORY_CAPACITY: usize = 128;
const ADMISSION_TOKEN_PREFIX: &str = "aokie-adm-v2";
const MOBILE_OFFER_TOKEN_PREFIX: &str = "aokie-offer-v2";

#[derive(Clone)]
pub(crate) struct V2Gateway {
    signer: Option<LeaseSigner>,
    admission_signer: Option<AdmissionTokenSigner>,
    allow_static_admissions: bool,
    claimed_admissions: Arc<Mutex<HashMap<String, ClaimedAdmission>>>,
    used_endpoint_jtis: Arc<Mutex<HashMap<String, u64>>>,
    apps: Arc<Mutex<HashMap<String, V2App>>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct V2Stats {
    pub apps: usize,
    pub plugins: usize,
    pub mobiles: usize,
}

impl V2Gateway {
    pub(crate) fn new(
        secret: Option<Vec<u8>>,
        admission_secret: Option<Vec<u8>>,
        allow_static_admissions: bool,
    ) -> Self {
        Self {
            signer: secret.map(LeaseSigner::new),
            admission_signer: admission_secret
                .and_then(|secret| AdmissionTokenSigner::new(secret).ok()),
            allow_static_admissions,
            claimed_admissions: Arc::new(Mutex::new(HashMap::new())),
            used_endpoint_jtis: Arc::new(Mutex::new(HashMap::new())),
            apps: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.signer.is_some() && self.admission_signer.is_some()
    }

    pub(crate) fn has_dynamic_admission(&self) -> bool {
        self.admission_signer.is_some()
    }

    pub(crate) fn allows_static_admission(&self) -> bool {
        false
    }

    pub(crate) async fn stats(&self) -> V2Stats {
        let apps = self.apps.lock().await;
        V2Stats {
            apps: apps.len(),
            plugins: apps.values().filter(|app| app.plugin.is_some()).count(),
            mobiles: apps.values().map(|app| app.mobiles.len()).sum(),
        }
    }

    fn signer(&self) -> Result<&LeaseSigner, GatewayError> {
        self.signer
            .as_ref()
            .ok_or_else(|| GatewayError::fatal("v2_disabled", "v2 lease signing is not configured"))
    }

    fn authenticate(
        &self,
        headers: &HeaderMap,
        static_registry: &AdmissionRegistry,
    ) -> Result<AuthenticatedAdmission, StatusCode> {
        let bearer = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if bearer.starts_with("aokie-adm-v2.") {
            let claims = self
                .admission_signer
                .as_ref()
                .ok_or(StatusCode::UNAUTHORIZED)?
                .verify(
                    bearer,
                    unix_now().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                )
                .map_err(|_| StatusCode::UNAUTHORIZED)?;
            if header_id(headers, "x-aokie-app-id")? != claims.app_id
                || header_id(headers, "x-aokie-device-id")? != claims.subject_id
            {
                return Err(StatusCode::FORBIDDEN);
            }
            let role = match claims.role {
                TokenAdmissionRole::Mobile => AdmissionRole::Mobile,
                TokenAdmissionRole::Plugin => AdmissionRole::Plugin,
            };
            return Ok(AuthenticatedAdmission {
                admission: Admission {
                    role,
                    app_id: claims.app_id,
                    subject_id: claims.subject_id,
                    grants: vec![],
                    scopes: claims.scopes,
                },
                jti: Some(claims.jti),
                exp: Some(claims.exp),
                holder_key_thumbprint: claims.holder_key_thumbprint,
                expected_peer_key_thumbprint: claims.expected_peer_key_thumbprint,
                approved_peer_key_thumbprints: claims.approved_peer_key_thumbprints,
                peer_roster_revision: claims.peer_roster_revision,
                peer_roster_hash: claims.peer_roster_hash,
            });
        }
        let _ = (static_registry, self.allow_static_admissions);
        // Endpoint thumbprints and the plugin's roster commitment must be
        // issuer-bound. Legacy static bearer records cannot express those
        // claims and are therefore never accepted on v2.
        Err(StatusCode::UNAUTHORIZED)
    }

    async fn claim_admission(
        &self,
        jti: Option<&str>,
        exp: Option<u64>,
        _connection_id: &str,
    ) -> Result<(), GatewayError> {
        let (Some(jti), Some(exp)) = (jti, exp) else {
            return Ok(());
        };
        let now = unix_now()?;
        let mut claimed = self.claimed_admissions.lock().await;
        claimed.retain(|_, binding| binding.exp > now);
        if claimed.contains_key(jti) {
            return Err(GatewayError::fatal(
                "admission_replay",
                "admission token was already used",
            ));
        }
        if claimed.len() >= MAX_CLAIMED_ADMISSIONS {
            return Err(GatewayError::fatal(
                "capacity",
                "unexpired admission capacity is exhausted",
            ));
        }
        claimed.insert(jti.to_owned(), ClaimedAdmission { exp });
        Ok(())
    }

    async fn release_admission(&self, jti: Option<&str>, _connection_id: &str) {
        let Some(_jti) = jti else { return };
        // A signed admission is one-use, not merely one-live-socket-at-a-time.
        // Retain its JTI until expiry so a disconnect, malformed hello or
        // forced fence cannot turn the same bearer into a fresh admission.
        let now = unix_now().unwrap_or_default();
        self.claimed_admissions
            .lock()
            .await
            .retain(|_, binding| binding.exp > now);
    }

    async fn claim_endpoint_jti(&self, jti: &str, exp: u64) -> Result<(), GatewayError> {
        let now = unix_now()?;
        let mut used = self.used_endpoint_jtis.lock().await;
        used.retain(|_, expires_at| *expires_at > now);
        if used.contains_key(jti) {
            return Err(GatewayError::fatal(
                "endpoint_replay",
                "endpoint proof JTI was already used",
            ));
        }
        if used.len() >= MAX_USED_ENDPOINT_JTIS {
            return Err(GatewayError::fatal(
                "capacity",
                "endpoint replay cache capacity is exhausted",
            ));
        }
        used.insert(jti.to_owned(), exp);
        Ok(())
    }
}

struct AuthenticatedAdmission {
    admission: Admission,
    jti: Option<String>,
    exp: Option<u64>,
    holder_key_thumbprint: String,
    expected_peer_key_thumbprint: Option<String>,
    approved_peer_key_thumbprints: Vec<String>,
    peer_roster_revision: Option<u64>,
    peer_roster_hash: Option<String>,
}

struct ClaimedAdmission {
    exp: u64,
}

#[derive(Clone)]
struct LeaseSigner {
    secret: Arc<Vec<u8>>,
}

/// HMAC-SHA256 admission-token codec shared by the gateway and a managed or
/// self-hosted FormLogic issuer.  The secret is never placed in a token.
#[derive(Clone)]
pub struct AdmissionTokenSigner {
    secret: Arc<Vec<u8>>,
}

impl AdmissionTokenSigner {
    pub fn new(secret: impl Into<Vec<u8>>) -> Result<Self, String> {
        let secret = secret.into();
        if !(32..=4096).contains(&secret.len()) {
            return Err("admission HMAC secret must be 32..4096 bytes".into());
        }
        Ok(Self {
            secret: Arc::new(secret),
        })
    }

    pub fn issue(&self, claims: &AdmissionClaims, now: u64) -> Result<String, String> {
        claims.validate(now).map_err(|error| error.to_string())?;
        if claims.exp.saturating_sub(now) > MAX_ADMISSION_TTL {
            return Err("admission lifetime exceeds 300 seconds".into());
        }
        let payload = serde_json::to_vec(claims).map_err(|_| "cannot encode admission claims")?;
        let signature = hmac_sha256(&self.secret, &payload);
        Ok(format!(
            "{ADMISSION_TOKEN_PREFIX}.{}.{}",
            hex(&payload),
            hex(&signature)
        ))
    }

    pub fn verify(&self, token: &str, now: u64) -> Result<AdmissionClaims, String> {
        let mut parts = token.split('.');
        if parts.next() != Some(ADMISSION_TOKEN_PREFIX) {
            return Err("invalid admission token prefix".into());
        }
        let payload = parts
            .next()
            .and_then(unhex)
            .ok_or_else(|| "invalid admission token payload".to_string())?;
        let supplied = parts
            .next()
            .and_then(unhex)
            .ok_or_else(|| "invalid admission token signature".to_string())?;
        if parts.next().is_some() || supplied.len() != 32 {
            return Err("invalid admission token format".into());
        }
        let expected = hmac_sha256(&self.secret, &payload);
        if !constant_time_eq(&expected, &supplied) {
            return Err("invalid admission token signature".into());
        }
        let claims: AdmissionClaims =
            serde_json::from_slice(&payload).map_err(|_| "invalid admission claims")?;
        claims.validate(now).map_err(|error| error.to_string())?;
        if claims.exp.saturating_sub(now) > MAX_ADMISSION_TTL {
            return Err("admission lifetime exceeds 300 seconds".into());
        }
        Ok(claims)
    }
}

impl LeaseSigner {
    fn new(secret: Vec<u8>) -> Self {
        Self {
            secret: Arc::new(secret),
        }
    }

    fn sign(&self, claims: &LeaseClaims) -> Result<String, GatewayError> {
        let payload = serde_json::to_vec(claims)
            .map_err(|_| GatewayError::fatal("internal", "cannot encode lease"))?;
        let signature = hmac_sha256(&self.secret, &payload);
        Ok(format!("v2.{}.{}", hex(&payload), hex(&signature)))
    }

    fn verify(&self, token: &str, now: u64) -> Result<LeaseClaims, GatewayError> {
        let mut parts = token.split('.');
        if parts.next() != Some("v2") {
            return Err(GatewayError::nonfatal(
                "invalid_lease",
                "lease format is invalid",
            ));
        }
        let payload = parts
            .next()
            .and_then(unhex)
            .ok_or_else(|| GatewayError::nonfatal("invalid_lease", "lease payload is invalid"))?;
        let supplied = parts
            .next()
            .and_then(unhex)
            .ok_or_else(|| GatewayError::nonfatal("invalid_lease", "lease signature is invalid"))?;
        if parts.next().is_some() || supplied.len() != 32 {
            return Err(GatewayError::nonfatal(
                "invalid_lease",
                "lease format is invalid",
            ));
        }
        let expected = hmac_sha256(&self.secret, &payload);
        if !constant_time_eq(&expected, &supplied) {
            return Err(GatewayError::nonfatal(
                "invalid_lease",
                "lease signature is invalid",
            ));
        }
        let claims: LeaseClaims = serde_json::from_slice(&payload)
            .map_err(|_| GatewayError::nonfatal("invalid_lease", "lease claims are invalid"))?;
        claims.validate(now).map_err(|_| {
            GatewayError::nonfatal("invalid_lease", "lease claims are invalid or expired")
        })?;
        if claims.expires_at.saturating_sub(now) > LEASE_TTL {
            return Err(GatewayError::nonfatal(
                "invalid_lease",
                "lease lifetime is invalid",
            ));
        }
        Ok(claims)
    }

    fn sign_mobile_offer(&self, claims: &PendingMobileOfferClaims) -> Result<String, GatewayError> {
        let payload = serde_json::to_vec(claims)
            .map_err(|_| GatewayError::fatal("internal", "cannot encode mobile offer"))?;
        let signature = hmac_sha256(&self.secret, &payload);
        Ok(format!(
            "{MOBILE_OFFER_TOKEN_PREFIX}.{}.{}",
            hex(&payload),
            hex(&signature)
        ))
    }

    fn verify_mobile_offer(
        &self,
        token: &str,
        now: u64,
    ) -> Result<PendingMobileOfferClaims, GatewayError> {
        let mut parts = token.split('.');
        if parts.next() != Some(MOBILE_OFFER_TOKEN_PREFIX) {
            return Err(GatewayError::nonfatal(
                "invalid_offer",
                "mobile offer format is invalid",
            ));
        }
        let payload = parts
            .next()
            .and_then(unhex)
            .ok_or_else(|| GatewayError::nonfatal("invalid_offer", "mobile offer is invalid"))?;
        let supplied = parts
            .next()
            .and_then(unhex)
            .ok_or_else(|| GatewayError::nonfatal("invalid_offer", "mobile offer is invalid"))?;
        if parts.next().is_some() || supplied.len() != 32 {
            return Err(GatewayError::nonfatal(
                "invalid_offer",
                "mobile offer format is invalid",
            ));
        }
        let expected = hmac_sha256(&self.secret, &payload);
        if !constant_time_eq(&expected, &supplied) {
            return Err(GatewayError::nonfatal(
                "invalid_offer",
                "mobile offer signature is invalid",
            ));
        }
        let claims: PendingMobileOfferClaims = serde_json::from_slice(&payload)
            .map_err(|_| GatewayError::nonfatal("invalid_offer", "mobile offer is invalid"))?;
        claims.validate(now).map_err(|_| {
            GatewayError::nonfatal("invalid_offer", "mobile offer is invalid or stale")
        })?;
        Ok(claims)
    }
}

fn hmac_sha256(secret: &[u8], payload: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = [0_u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= key[index];
        outer_pad[index] ^= key[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(payload);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn constant_time_eq(expected: &[u8], supplied: &[u8]) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    expected
        .iter()
        .zip(supplied)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn unhex(encoded: &str) -> Option<Vec<u8>> {
    if encoded.len() % 2 != 0 || encoded.len() > 128 * 1024 {
        return None;
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

struct V2App {
    plugin: Option<V2Peer>,
    plugin_id: Option<String>,
    approved_mobile_key_thumbprints: HashSet<String>,
    peer_roster_revision: Option<u64>,
    peer_roster_hash: Option<String>,
    mobiles: HashMap<String, V2Peer>,
    snapshot: Option<AuthoritativeCallSnapshot>,
    idle: Option<IdleAuthority>,
    sequence: u64,
    next_fence: u64,
    claim: Option<TalkClaim>,
    leases: HashMap<String, LeaseRecord>,
    idempotency: HashMap<(String, String), IdempotencyRecord>,
    idempotency_order: VecDeque<(String, String)>,
    signals: HashSet<(String, String)>,
    signal_order: VecDeque<(String, String)>,
    assistance: Option<AssistanceRecord>,
    end_caller_challenges: HashMap<String, EndCallerChallengeRecord>,
    used_end_caller_confirmations: HashSet<String>,
    used_end_caller_order: VecDeque<String>,
    end_caller_operation: Option<EndCallerOperation>,
    pending_mobile_offers: HashMap<String, SignedPendingMobileOffer>,
    accepted_mobile_offers: HashMap<String, PendingMobileOfferClaims>,
    mobile_offer_winners: HashMap<String, String>,
}

struct IdleAuthority {
    assertion: PluginIdleFrame,
    prior_fence: Option<AuthoritativeFence>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthoritativeFence {
    call_id: String,
    call_epoch: u64,
    owner_epoch: u64,
    switchboard_revision: u64,
    remote_revision: u64,
}

impl From<&AuthoritativeCallSnapshot> for AuthoritativeFence {
    fn from(snapshot: &AuthoritativeCallSnapshot) -> Self {
        Self {
            call_id: snapshot.call_id.clone(),
            call_epoch: snapshot.call_epoch,
            owner_epoch: snapshot.owner_epoch,
            switchboard_revision: snapshot.switchboard_revision,
            remote_revision: snapshot.remote_revision,
        }
    }
}

impl Default for V2App {
    fn default() -> Self {
        Self {
            plugin: None,
            plugin_id: None,
            approved_mobile_key_thumbprints: HashSet::new(),
            peer_roster_revision: None,
            peer_roster_hash: None,
            mobiles: HashMap::new(),
            snapshot: None,
            idle: None,
            sequence: 0,
            next_fence: 0,
            claim: None,
            leases: HashMap::new(),
            idempotency: HashMap::new(),
            idempotency_order: VecDeque::new(),
            signals: HashSet::new(),
            signal_order: VecDeque::new(),
            assistance: None,
            end_caller_challenges: HashMap::new(),
            used_end_caller_confirmations: HashSet::new(),
            used_end_caller_order: VecDeque::new(),
            end_caller_operation: None,
            pending_mobile_offers: HashMap::new(),
            accepted_mobile_offers: HashMap::new(),
            mobile_offer_winners: HashMap::new(),
        }
    }
}

impl V2App {
    fn remember_signal(&mut self, sender: &str, signal_id: &str) -> bool {
        let key = (sender.to_owned(), signal_id.to_owned());
        if self.signals.contains(&key) {
            return false;
        }
        self.signals.insert(key.clone());
        self.signal_order.push_back(key);
        while self.signal_order.len() > SIGNAL_DEDUPE_CAPACITY {
            if let Some(oldest) = self.signal_order.pop_front() {
                self.signals.remove(&oldest);
            }
        }
        true
    }

    fn cached(&self, device_id: &str, key: &str, fingerprint: [u8; 32]) -> Cached {
        match self
            .idempotency
            .get(&(device_id.to_owned(), key.to_owned()))
        {
            Some(record) if record.fingerprint == fingerprint => {
                Cached::Replay(record.response.clone())
            }
            Some(_) => Cached::Conflict,
            None => Cached::Miss,
        }
    }

    fn cache(&mut self, device_id: &str, key: &str, fingerprint: [u8; 32], response: String) {
        let cache_key = (device_id.to_owned(), key.to_owned());
        if !self.idempotency.contains_key(&cache_key) {
            self.idempotency_order.push_back(cache_key.clone());
        }
        self.idempotency.insert(
            cache_key,
            IdempotencyRecord {
                fingerprint,
                response,
            },
        );
        while self.idempotency_order.len() > IDEMPOTENCY_CAPACITY {
            if let Some(oldest) = self.idempotency_order.pop_front() {
                self.idempotency.remove(&oldest);
            }
        }
    }

    fn next_talk_fence(&mut self) -> Result<u64, GatewayError> {
        self.next_fence = self
            .next_fence
            .checked_add(1)
            .filter(|fence| *fence <= MAX_SAFE_INTEGER)
            .ok_or_else(|| GatewayError::fatal("fence_exhausted", "talk fence exhausted"))?;
        Ok(self.next_fence)
    }

    fn remember_used_end_caller_confirmation(&mut self, confirmation_id: String) {
        if self
            .used_end_caller_confirmations
            .insert(confirmation_id.clone())
        {
            self.used_end_caller_order.push_back(confirmation_id);
        }
        while self.used_end_caller_order.len() > END_CALLER_HISTORY_CAPACITY {
            if let Some(oldest) = self.used_end_caller_order.pop_front() {
                self.used_end_caller_confirmations.remove(&oldest);
            }
        }
    }
}

struct V2Peer {
    connection_id: String,
    session_nonce: String,
    endpoint_key: EndpointPublicKey,
    grants: Vec<Grant>,
    tx: mpsc::Sender<Message>,
    fenced: watch::Sender<bool>,
    general_rate: RateWindow,
    rtc_rate: RateWindow,
}

impl V2Peer {
    fn new(
        connection_id: String,
        session_nonce: String,
        endpoint_key: EndpointPublicKey,
        grants: Vec<Grant>,
        tx: mpsc::Sender<Message>,
        fenced: watch::Sender<bool>,
    ) -> Self {
        Self {
            connection_id,
            session_nonce,
            endpoint_key,
            grants,
            tx,
            fenced,
            general_rate: RateWindow::new(),
            rtc_rate: RateWindow::new(),
        }
    }

    fn fence(&self) {
        self.fenced.send_replace(true);
        let _ = self.tx.try_send(Message::Close(None));
    }

    fn send(&self, encoded: String) -> Result<(), GatewayError> {
        self.tx
            .try_send(Message::Text(encoded))
            .map_err(|_| GatewayError::fatal("peer_unavailable", "peer queue is unavailable"))
    }

    fn has(&self, grant: Grant) -> bool {
        self.grants.contains(&grant)
    }

    fn admit_general(&mut self) -> Result<(), GatewayError> {
        if self.general_rate.admit(GENERAL_RATE_LIMIT) {
            Ok(())
        } else {
            Err(GatewayError::fatal(
                "rate_limited",
                "inbound frame rate exceeded",
            ))
        }
    }

    fn admit_rtc(&mut self) -> Result<(), GatewayError> {
        if self.rtc_rate.admit(RTC_RATE_LIMIT) {
            Ok(())
        } else {
            Err(GatewayError::fatal(
                "rate_limited",
                "RTC signalling rate exceeded",
            ))
        }
    }
}

struct RateWindow {
    started: Instant,
    count: u32,
}

impl RateWindow {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            count: 0,
        }
    }

    fn admit(&mut self, limit: u32) -> bool {
        if self.started.elapsed() >= RATE_WINDOW {
            self.started = Instant::now();
            self.count = 0;
        }
        if self.count >= limit {
            return false;
        }
        self.count += 1;
        true
    }
}

struct LeaseRecord {
    claims: LeaseClaims,
    provisional: bool,
    mobile_signal: SignalProgress,
    plugin_signal: SignalProgress,
}

#[derive(Default)]
struct SignalProgress {
    sdp_revision: u64,
    transport_generation: u64,
}

struct TalkClaim {
    request_id: String,
    device_id: String,
    mode: LeaseMode,
    call_id: String,
    call_epoch: u64,
    fence: u64,
    lease_jti: String,
    active: bool,
}

struct IdempotencyRecord {
    fingerprint: [u8; 32],
    response: String,
}

struct AssistanceRecord {
    request: PluginAssistanceRequestFrame,
    fingerprint: [u8; 32],
    answered: bool,
}

struct EndCallerChallengeRecord {
    frame: EndCallerChallengeFrame,
    lease_jti: String,
}

struct EndCallerOperation {
    execute: PluginEndCallerExecuteFrame,
    idempotency_key: String,
    fingerprint: [u8; 32],
}

enum Cached {
    Replay(String),
    Conflict,
    Miss,
}

#[derive(Debug)]
struct GatewayError {
    code: &'static str,
    message: &'static str,
    fatal: bool,
}

impl GatewayError {
    fn fatal(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            fatal: true,
        }
    }

    fn nonfatal(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            fatal: false,
        }
    }
}

pub(crate) async fn realtime(
    State(gateway): State<Gateway>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !gateway.inner.v2.is_enabled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let authenticated = match gateway
        .inner
        .v2
        .authenticate(&headers, &gateway.inner.admissions)
    {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if !matches!(
        authenticated.admission.role,
        AdmissionRole::Mobile | AdmissionRole::Plugin
    ) {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(gateway, authenticated, socket))
}

async fn handle_socket(gateway: Gateway, authenticated: AuthenticatedAdmission, socket: WebSocket) {
    let admission = authenticated.admission;
    let admission_jti = authenticated.jti;
    let admission_exp = authenticated.exp;
    let holder_key_thumbprint = authenticated.holder_key_thumbprint;
    let expected_peer_key_thumbprint = authenticated.expected_peer_key_thumbprint;
    let approved_peer_key_thumbprints = authenticated.approved_peer_key_thumbprints;
    let peer_roster_revision = authenticated.peer_roster_revision;
    let peer_roster_hash = authenticated.peer_roster_hash;
    let connection_id = format!("v2_conn_{}", uuid::Uuid::new_v4().simple());
    let (mut writer, mut reader) = socket.split();
    let (tx, mut outbound) = mpsc::channel::<Message>(OUTBOUND_CAPACITY);
    let (fenced, mut fence_rx) = watch::channel(false);
    let writer_fence = fenced.clone();
    let mut writer_task = tokio::spawn(async move {
        while let Some(message) = outbound.recv().await {
            if writer.send(message).await.is_err() {
                writer_fence.send_replace(true);
                break;
            }
        }
    });
    if let Err(error) = gateway
        .inner
        .v2
        .claim_admission(admission_jti.as_deref(), admission_exp, &connection_id)
        .await
    {
        send_error(&tx, &error, None);
        let _ = tx.try_send(Message::Close(None));
        writer_task.abort();
        return;
    }
    let now = match unix_now() {
        Ok(now) => now,
        Err(error) => {
            send_error(&tx, &error, None);
            let _ = tx.try_send(Message::Close(None));
            writer_task.abort();
            return;
        }
    };
    let Some(admission_jti_value) = admission_jti.as_deref() else {
        let error = GatewayError::fatal(
            "endpoint_binding_required",
            "v2 requires a signed endpoint-bound admission",
        );
        send_error(&tx, &error, None);
        let _ = tx.try_send(Message::Close(None));
        writer_task.abort();
        return;
    };
    let challenge = EndpointChallengeFrame {
        kind: "endpoint_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: admission.app_id.clone(),
        subject_id: admission.subject_id.clone(),
        role: match admission.role {
            AdmissionRole::Mobile => TokenAdmissionRole::Mobile,
            AdmissionRole::Plugin => TokenAdmissionRole::Plugin,
            AdmissionRole::Desktop => unreachable!("v2 desktop was rejected"),
        },
        connection_id: connection_id.clone(),
        challenge_nonce: format!("challenge_{}", uuid::Uuid::new_v4().simple()),
        admission_jti: admission_jti_value.to_owned(),
        holder_key_thumbprint,
        expected_peer_key_thumbprint,
        approved_peer_key_thumbprints,
        peer_roster_revision,
        peer_roster_hash,
        expires_at: admission_exp
            .unwrap_or(now + ENDPOINT_PROOF_MAX_LIFETIME)
            .min(now + ENDPOINT_PROOF_MAX_LIFETIME),
    };
    if challenge.validate(now).is_err() {
        let error =
            GatewayError::fatal("invalid_admission", "admission endpoint policy is invalid");
        send_error(&tx, &error, None);
        let _ = tx.try_send(Message::Close(None));
        writer_task.abort();
        return;
    }
    if tx
        .try_send(Message::Text(match serialize(&challenge) {
            Ok(encoded) => encoded,
            Err(error) => {
                send_error(&tx, &error, None);
                writer_task.abort();
                return;
            }
        }))
        .is_err()
    {
        writer_task.abort();
        return;
    }
    let admission_expiry = wait_for_admission_expiry(admission_exp);
    tokio::pin!(admission_expiry);

    let first_result = tokio::select! {
        _ = &mut admission_expiry => {
            let error = GatewayError::fatal(
                "admission_expired",
                "admission authority expired; request a fresh admission",
            );
            send_error(&tx, &error, None);
            let _ = tx.try_send(Message::Close(None));
            gateway
                .inner
                .v2
                .release_admission(admission_jti.as_deref(), &connection_id)
                .await;
            drop(tx);
            if tokio::time::timeout(Duration::from_secs(1), &mut writer_task)
                .await
                .is_err()
            {
                writer_task.abort();
            }
            return;
        }
        first = tokio::time::timeout(Duration::from_secs(10), reader.next()) => first,
    };
    let first = match first_result {
        Ok(Some(Ok(Message::Text(text)))) if text.len() <= MAX_MESSAGE_BYTES => text,
        _ => {
            let _ = tx.try_send(Message::Close(None));
            writer_task.abort();
            gateway
                .inner
                .v2
                .release_admission(admission_jti.as_deref(), &connection_id)
                .await;
            return;
        }
    };

    let registration = match admission.role {
        AdmissionRole::Mobile => match parse_mobile_frame(&first) {
            Ok(MobileInbound::Hello(hello)) => {
                if let Err(error) = verify_hello_proof(&hello.endpoint_proof, &challenge) {
                    Err(error)
                } else if let Err(error) = gateway
                    .inner
                    .v2
                    .claim_endpoint_jti(
                        &hello.endpoint_proof.claims.jti,
                        hello.endpoint_proof.claims.expires_at,
                    )
                    .await
                {
                    Err(error)
                } else {
                    register_mobile(
                        &gateway,
                        &admission,
                        &connection_id,
                        hello,
                        &challenge,
                        tx.clone(),
                        fenced.clone(),
                    )
                    .await
                }
            }
            _ => Err(GatewayError::fatal(
                "invalid_hello",
                "mobile hello is required",
            )),
        },
        AdmissionRole::Plugin => match parse_plugin_frame(&first) {
            Ok(PluginInbound::Hello(hello)) => {
                if let Err(error) = verify_hello_proof(&hello.endpoint_proof, &challenge) {
                    Err(error)
                } else if let Err(error) = gateway
                    .inner
                    .v2
                    .claim_endpoint_jti(
                        &hello.endpoint_proof.claims.jti,
                        hello.endpoint_proof.claims.expires_at,
                    )
                    .await
                {
                    Err(error)
                } else {
                    register_plugin(
                        &gateway,
                        &admission,
                        &connection_id,
                        hello,
                        &challenge,
                        tx.clone(),
                        fenced.clone(),
                    )
                    .await
                }
            }
            _ => Err(GatewayError::fatal(
                "invalid_hello",
                "plugin hello is required",
            )),
        },
        AdmissionRole::Desktop => Err(GatewayError::fatal("wrong_role", "desktop role is v1 only")),
    };
    if let Err(error) = registration {
        send_error(&tx, &error, None);
        let _ = tx.try_send(Message::Close(None));
        writer_task.abort();
        gateway
            .inner
            .v2
            .release_admission(admission_jti.as_deref(), &connection_id)
            .await;
        return;
    }

    loop {
        let incoming = tokio::select! {
            _ = &mut admission_expiry => {
                let error = GatewayError::fatal(
                    "admission_expired",
                    "admission authority expired; request a fresh admission",
                );
                send_error(&tx, &error, None);
                let _ = tx.try_send(Message::Close(None));
                break;
            }
            changed = fence_rx.changed() => {
                let _ = changed;
                break;
            }
            incoming = reader.next() => incoming,
        };
        let Some(incoming) = incoming else { break };
        let result = match incoming {
            Ok(Message::Text(text)) if text.len() <= MAX_MESSAGE_BYTES => match admission.role {
                AdmissionRole::Mobile => {
                    handle_mobile(&gateway, &admission, &connection_id, &text).await
                }
                AdmissionRole::Plugin => {
                    handle_plugin(&gateway, &admission, &connection_id, &text).await
                }
                AdmissionRole::Desktop => {
                    Err(GatewayError::fatal("wrong_role", "desktop role is v1 only"))
                }
            },
            Ok(Message::Ping(bytes)) => tx
                .try_send(Message::Pong(bytes))
                .map_err(|_| GatewayError::fatal("peer_unavailable", "peer queue is unavailable")),
            Ok(Message::Pong(_)) => Ok(()),
            Ok(Message::Close(_)) | Err(_) => break,
            _ => Err(GatewayError::fatal(
                "invalid_frame",
                "text frames are required",
            )),
        };
        if let Err(error) = result {
            send_error(&tx, &error, None);
            if error.fatal {
                let _ = tx.try_send(Message::Close(None));
                break;
            }
        }
    }

    unregister(&gateway, &admission, &connection_id).await;
    gateway
        .inner
        .v2
        .release_admission(admission_jti.as_deref(), &connection_id)
        .await;
    drop(tx);
    if tokio::time::timeout(Duration::from_secs(1), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
    }
}

fn verify_hello_proof(
    proof: &SignedHelloProof,
    challenge: &EndpointChallengeFrame,
) -> Result<(), GatewayError> {
    proof.verify(unix_now()?).map_err(|_| {
        GatewayError::fatal(
            "invalid_endpoint_proof",
            "endpoint hello proof is invalid or stale",
        )
    })?;
    let claims = &proof.claims;
    if claims.app_id != challenge.app_id
        || claims.subject_id != challenge.subject_id
        || claims.role != challenge.role
        || claims.connection_id != challenge.connection_id
        || claims.challenge_nonce != challenge.challenge_nonce
        || claims.admission_jti != challenge.admission_jti
        || claims.holder_key_thumbprint != challenge.holder_key_thumbprint
        || claims.expected_peer_key_thumbprint != challenge.expected_peer_key_thumbprint
        || claims.approved_peer_key_thumbprints != challenge.approved_peer_key_thumbprints
        || claims.peer_roster_revision != challenge.peer_roster_revision
        || claims.peer_roster_hash != challenge.peer_roster_hash
        || claims.expires_at > challenge.expires_at
    {
        return Err(GatewayError::fatal(
            "endpoint_substitution",
            "endpoint hello proof does not match its server challenge",
        ));
    }
    Ok(())
}

async fn register_plugin(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    hello: PluginHello,
    challenge: &EndpointChallengeFrame,
    tx: mpsc::Sender<Message>,
    fenced: watch::Sender<bool>,
) -> Result<(), GatewayError> {
    hello
        .validate()
        .map_err(|_| GatewayError::fatal("invalid_hello", "plugin hello is invalid"))?;
    if hello.app_id != admission.app_id || hello.plugin_id != admission.subject_id {
        return Err(GatewayError::fatal(
            "identity_mismatch",
            "plugin identity is not bound to admission",
        ));
    }
    let mut apps = gateway.inner.v2.apps.lock().await;
    if !apps.contains_key(&admission.app_id) && apps.len() >= MAX_APPS {
        return Err(GatewayError::fatal(
            "capacity",
            "application capacity is exhausted",
        ));
    }
    let app = apps.entry(admission.app_id.clone()).or_default();
    if let Some(old) = app.plugin.take() {
        old.fence();
    }
    for (_, mobile) in app.mobiles.drain() {
        mobile.fence();
    }
    app.snapshot = None;
    app.idle = None;
    app.sequence = 0;
    app.claim = None;
    app.leases.clear();
    app.idempotency.clear();
    app.idempotency_order.clear();
    app.signals.clear();
    app.signal_order.clear();
    app.assistance = None;
    app.end_caller_challenges.clear();
    app.used_end_caller_confirmations.clear();
    app.used_end_caller_order.clear();
    app.end_caller_operation = None;
    app.pending_mobile_offers.clear();
    app.accepted_mobile_offers.clear();
    app.mobile_offer_winners.clear();
    app.approved_mobile_key_thumbprints.clear();
    app.peer_roster_revision = None;
    app.peer_roster_hash = None;
    app.plugin_id = Some(admission.subject_id.clone());
    app.plugin = Some(V2Peer::new(
        connection_id.to_owned(),
        hello.session_nonce,
        hello.endpoint_proof.endpoint_key,
        vec![],
        tx,
        fenced,
    ));
    app.approved_mobile_key_thumbprints = challenge
        .approved_peer_key_thumbprints
        .iter()
        .cloned()
        .collect();
    app.peer_roster_revision = challenge.peer_roster_revision;
    app.peer_roster_hash = challenge.peer_roster_hash.clone();
    Ok(())
}

async fn register_mobile(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    hello: MobileHello,
    challenge: &EndpointChallengeFrame,
    tx: mpsc::Sender<Message>,
    fenced: watch::Sender<bool>,
) -> Result<(), GatewayError> {
    hello
        .validate()
        .map_err(|_| GatewayError::fatal("invalid_hello", "mobile hello is invalid"))?;
    if hello.app_id != admission.app_id || hello.device_id != admission.subject_id {
        return Err(GatewayError::fatal(
            "identity_mismatch",
            "mobile identity is not bound to admission",
        ));
    }
    if !admission.scopes.contains(&Grant::StateRead) {
        return Err(GatewayError::fatal(
            "forbidden",
            "state_read scope is required",
        ));
    }
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = apps.get_mut(&admission.app_id).ok_or_else(|| {
        GatewayError::fatal("endpoint_unavailable", "Aokie plugin is not connected")
    })?;
    if app.plugin.is_none() {
        return Err(GatewayError::fatal(
            "endpoint_unavailable",
            "Aokie plugin is not connected",
        ));
    }
    if app.snapshot.is_none() && app.idle.is_none() {
        return Err(GatewayError::fatal(
            "endpoint_unavailable",
            "Aokie plugin has not published authoritative state",
        ));
    }
    let plugin = app.plugin.as_ref().expect("checked plugin");
    if challenge.expected_peer_key_thumbprint.as_deref()
        != Some(plugin.endpoint_key.thumbprint.as_str())
        || !app
            .approved_mobile_key_thumbprints
            .contains(&hello.endpoint_proof.endpoint_key.thumbprint)
    {
        return Err(GatewayError::fatal(
            "unapproved_endpoint",
            "mobile endpoint key is not in the active owner-approved roster",
        ));
    }
    if !app.mobiles.contains_key(&admission.subject_id) && app.mobiles.len() >= MAX_MOBILES_PER_APP
    {
        return Err(GatewayError::fatal(
            "capacity",
            "mobile capacity is exhausted",
        ));
    }
    recover_device(
        app,
        &admission.app_id,
        &admission.subject_id,
        "session_replaced",
    );
    let peer = V2Peer::new(
        connection_id.to_owned(),
        hello.session_nonce,
        hello.endpoint_proof.endpoint_key,
        admission.scopes.clone(),
        tx,
        fenced,
    );
    if let Some(old) = app.mobiles.insert(admission.subject_id.clone(), peer) {
        old.fence();
    }
    refresh_pending_mobile_offers(
        app,
        &admission.app_id,
        gateway.inner.v2.signer()?,
        unix_now()?,
    )?;
    let peer = app
        .mobiles
        .get(&admission.subject_id)
        .expect("inserted peer");
    peer.send(authoritative_sync(app, &admission.app_id, peer)?)?;
    if peer.has(Grant::AssistanceRead) {
        if let Some(assistance) = app.assistance.as_ref().filter(|record| !record.answered) {
            peer.send(serialize(&assistance.request)?)?;
        }
    }
    Ok(())
}

async fn handle_mobile(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    encoded: &str,
) -> Result<(), GatewayError> {
    let frame = parse_mobile_frame(encoded).map_err(|_| {
        GatewayError::fatal(
            "invalid_frame",
            "mobile frame is invalid for this direction",
        )
    })?;
    if matches!(frame, MobileInbound::Hello(_)) {
        return Err(GatewayError::fatal(
            "invalid_frame",
            "hello cannot be repeated",
        ));
    }
    match frame {
        MobileInbound::OfferAnswer(frame) => {
            handle_mobile_offer_answer(gateway, admission, connection_id, frame).await
        }
        MobileInbound::LeaseRequest(frame) => {
            handle_lease_request(gateway, admission, connection_id, frame).await
        }
        MobileInbound::LeaseHeartbeat(frame) => {
            handle_lease_heartbeat(gateway, admission, connection_id, frame).await
        }
        MobileInbound::LeaseRevoke(frame) => {
            handle_mobile_revoke(gateway, admission, connection_id, frame).await
        }
        MobileInbound::RtcSignal(frame) => {
            handle_mobile_rtc(gateway, admission, connection_id, frame).await
        }
        MobileInbound::AssistanceAnswer(frame) => {
            handle_assistance_answer(gateway, admission, connection_id, frame).await
        }
        MobileInbound::EndCallerChallengeRequest(frame) => {
            handle_end_caller_challenge_request(gateway, admission, connection_id, frame).await
        }
        MobileInbound::EndCallerConfirm(frame) => {
            handle_end_caller_confirm(gateway, admission, connection_id, frame).await
        }
        MobileInbound::Hello(_) => unreachable!(),
    }
}

async fn handle_plugin(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    encoded: &str,
) -> Result<(), GatewayError> {
    let frame = parse_plugin_frame(encoded).map_err(|_| {
        GatewayError::fatal(
            "invalid_frame",
            "plugin frame is invalid for this direction",
        )
    })?;
    if matches!(frame, PluginInbound::Hello(_)) {
        return Err(GatewayError::fatal(
            "invalid_frame",
            "hello cannot be repeated",
        ));
    }
    match frame {
        PluginInbound::Idle(frame) => handle_idle(gateway, admission, connection_id, frame).await,
        PluginInbound::Snapshot(frame) => {
            handle_snapshot(gateway, admission, connection_id, frame).await
        }
        PluginInbound::ClaimDecision(frame) => {
            handle_claim_decision(gateway, admission, connection_id, frame).await
        }
        PluginInbound::RtcSignal(frame) => {
            handle_plugin_rtc(gateway, admission, connection_id, frame).await
        }
        PluginInbound::LeaseRevoke(frame) => {
            handle_plugin_revoke(gateway, admission, connection_id, frame).await
        }
        PluginInbound::AssistanceRequest(frame) => {
            handle_assistance_request(gateway, admission, connection_id, frame).await
        }
        PluginInbound::EndCallerResult(frame) => {
            handle_end_caller_result(gateway, admission, connection_id, frame).await
        }
        PluginInbound::Hello(_) => unreachable!(),
    }
}

async fn handle_idle(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: PluginIdleFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_plugin_mut(&mut apps, admission, connection_id)?;
    app.plugin
        .as_mut()
        .expect("checked plugin")
        .admit_general()?;

    // A duplicate idle assertion on the same authenticated, ordered socket is
    // state-idempotent. It must not manufacture sequence churn or reopen any
    // authority that the first transition already revoked.
    if app.snapshot.is_none() {
        if let Some(idle) = app.idle.as_mut() {
            idle.assertion = frame;
            return Ok(());
        }
    }

    // Reserve the publication sequence before revoking anything.  If the
    // JSON-safe counter is exhausted, the caller will fence this socket and
    // unregister it; leaving the current authority intact until then avoids
    // an unpublishable half-idle state.
    let next_sequence = next_authoritative_sequence(app.sequence)?;
    let prior_fence = app.snapshot.as_ref().map(AuthoritativeFence::from);
    clear_call_authority(app, &admission.app_id, "authoritative_idle");
    app.sequence = next_sequence;
    app.idle = Some(IdleAuthority {
        assertion: frame,
        prior_fence,
    });
    broadcast_idle_sync(app, &admission.app_id)
}

fn next_authoritative_sequence(current: u64) -> Result<u64, GatewayError> {
    current
        .checked_add(1)
        .filter(|sequence| *sequence <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            GatewayError::fatal("sequence_exhausted", "authoritative sequence exhausted")
        })
}

fn snapshot_regresses(previous: &AuthoritativeFence, next: &AuthoritativeCallSnapshot) -> bool {
    next.call_epoch < previous.call_epoch
        || (next.call_epoch == previous.call_epoch
            && (next.owner_epoch < previous.owner_epoch
                || next.switchboard_revision < previous.switchboard_revision
                || next.remote_revision < previous.remote_revision))
        || (next.call_id != previous.call_id && next.call_epoch <= previous.call_epoch)
}

fn clear_call_authority(app: &mut V2App, app_id: &str, reason: &str) {
    revoke_all(app, app_id, reason);
    app.snapshot = None;
    app.claim = None;
    app.assistance = None;
    app.end_caller_challenges.clear();
    app.end_caller_operation = None;
    app.pending_mobile_offers.clear();
    app.accepted_mobile_offers.clear();
    app.mobile_offer_winners.clear();
    let stale_replay = json!({
        "kind": "error",
        "schemaVersion": SCHEMA_VERSION,
        "appId": app_id,
        "requestId": null,
        "code": "stale_authority",
        "message": "the idempotent request belongs to a call that is no longer authoritative"
    })
    .to_string();
    for record in app.idempotency.values_mut() {
        // Keep the key/fingerprint high-water so a reused key remains either
        // an exact safe replay or a conflict, but never replay a lease or
        // other positive call authority after the physical endpoint is idle.
        record.response = stale_replay.clone();
    }
    // Signal dedupe, used end-caller confirmations and the monotonic talk
    // fence are replay high-waters. They deliberately survive an idle
    // transition even though every live call authority is gone.
}

async fn handle_snapshot(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: PluginSnapshotFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_plugin_mut(&mut apps, admission, connection_id)?;
    app.plugin
        .as_mut()
        .expect("checked plugin")
        .admit_general()?;
    let regresses = app.snapshot.as_ref().is_some_and(|previous| {
        snapshot_regresses(&AuthoritativeFence::from(previous), &frame.snapshot)
    }) || app
        .idle
        .as_ref()
        .and_then(|idle| idle.prior_fence.as_ref())
        .is_some_and(|tombstone| frame.snapshot.call_epoch <= tombstone.call_epoch);
    if regresses {
        return Err(GatewayError::fatal(
            "stale_snapshot",
            "plugin snapshot regressed",
        ));
    }
    // Preflight the publication fence before applying revocations or
    // replacing endpoint truth.  Sequence exhaustion must never leave a
    // state change that no mobile can observe.
    let next_sequence = next_authoritative_sequence(app.sequence)?;
    if let Some(previous) = &app.snapshot {
        if previous.call_id != frame.snapshot.call_id
            || previous.call_epoch != frame.snapshot.call_epoch
            || previous.owner_epoch != frame.snapshot.owner_epoch
            || previous.switchboard_revision != frame.snapshot.switchboard_revision
            || previous.remote_revision != frame.snapshot.remote_revision
            || previous.telephony_state != frame.snapshot.telephony_state
            || previous.service_mode != frame.snapshot.service_mode
            || previous.remote_consent != frame.snapshot.remote_consent
        {
            app.end_caller_challenges.clear();
        }
    }
    let epoch_changed = app.snapshot.as_ref().is_some_and(|previous| {
        previous.call_id != frame.snapshot.call_id
            || previous.call_epoch != frame.snapshot.call_epoch
            || previous.owner_epoch != frame.snapshot.owner_epoch
    });
    if epoch_changed || matches!(frame.snapshot.telephony_state, TelephonyState::Ended) {
        revoke_all(app, &admission.app_id, "authoritative_epoch_changed");
        app.assistance = None;
        app.pending_mobile_offers.clear();
        app.accepted_mobile_offers.clear();
        app.mobile_offer_winners.clear();
    }
    let revoked_by_policy: Vec<_> = app
        .leases
        .iter()
        .filter(|(_, lease)| !frame.snapshot.remote_consent.allows(lease.claims.mode))
        .map(|(jti, _)| jti.clone())
        .collect();
    for jti in revoked_by_policy {
        let _ = revoke_one(
            app,
            &admission.app_id,
            &jti,
            "remote_consent_not_current",
            true,
        );
    }
    if !frame.snapshot.remote_consent.enabled
        || !frame.snapshot.remote_consent.acknowledged
        || !frame.snapshot.remote_consent.assistance_enabled
    {
        app.assistance = None;
    }
    app.sequence = next_sequence;
    app.idle = None;
    app.snapshot = Some(frame.snapshot);
    refresh_pending_mobile_offers(
        app,
        &admission.app_id,
        gateway.inner.v2.signer()?,
        unix_now()?,
    )?;

    let device_ids: Vec<_> = app.mobiles.keys().cloned().collect();
    let mut failed = Vec::new();
    for device_id in device_ids {
        let Some(peer) = app.mobiles.get(&device_id) else {
            continue;
        };
        let encoded = projected_snapshot(app, &admission.app_id, peer)?;
        if peer.send(encoded).is_err() {
            failed.push(device_id);
        }
    }
    for device_id in failed {
        if let Some(peer) = app.mobiles.remove(&device_id) {
            peer.fence();
        }
        recover_device(app, &admission.app_id, &device_id, "outbound_queue_failed");
    }
    Ok(())
}

fn refresh_pending_mobile_offers(
    app: &mut V2App,
    app_id: &str,
    signer: &LeaseSigner,
    now: u64,
) -> Result<(), GatewayError> {
    let Some(snapshot) = app.snapshot.as_ref() else {
        app.pending_mobile_offers.clear();
        return Ok(());
    };
    if matches!(snapshot.telephony_state, TelephonyState::Ended) {
        app.pending_mobile_offers.clear();
        return Ok(());
    }
    let mobiles = &app.mobiles;
    app.pending_mobile_offers.retain(|_, signed| {
        signed.offer.expires_at > now
            && signed.offer.app_id == app_id
            && signed.offer.call_id == snapshot.call_id
            && signed.offer.call_epoch == snapshot.call_epoch
            && signed.offer.owner_epoch == snapshot.owner_epoch
            && signed.offer.switchboard_revision == snapshot.switchboard_revision
            && signed.offer.remote_revision == snapshot.remote_revision
            && signed.offer.required_consent_policy_id == snapshot.remote_consent.policy_id
            && signed.offer.required_consent_policy_version
                == snapshot.remote_consent.policy_version
            && snapshot.remote_consent.allows(signed.offer.offered_mode)
            && mobiles
                .get(&signed.offer.target_device_id)
                .is_some_and(|peer| {
                    peer.endpoint_key.thumbprint == signed.offer.target_holder_key_thumbprint
                        && signed
                            .offer
                            .required_grants
                            .iter()
                            .all(|grant| peer.has(*grant))
                })
    });
    for (device_id, peer) in &app.mobiles {
        for (mode, grant, surface) in [
            (
                LeaseMode::Monitor,
                Grant::Monitor,
                MobileOfferSurface::InApp,
            ),
            (
                LeaseMode::Consult,
                Grant::Consult,
                MobileOfferSurface::InApp,
            ),
            (
                LeaseMode::Consult,
                Grant::Consult,
                MobileOfferSurface::VoiceSystemUi,
            ),
            (
                LeaseMode::Takeover,
                Grant::Takeover,
                MobileOfferSurface::InApp,
            ),
            (
                LeaseMode::Takeover,
                Grant::Takeover,
                MobileOfferSurface::VoiceSystemUi,
            ),
        ] {
            if !peer.has(Grant::StateRead)
                || !peer.has(Grant::RtcSignal)
                || !peer.has(grant)
                || !snapshot.remote_consent.allows(mode)
            {
                continue;
            }
            let opportunity_material = format!(
                "{app_id}\0{}\0{}\0{:?}",
                snapshot.call_id, snapshot.owner_epoch, mode
            );
            let opportunity_id = format!(
                "opportunity_{}",
                &hex(&Sha256::digest(opportunity_material.as_bytes()))[..24]
            );
            if app.mobile_offer_winners.contains_key(&opportunity_id)
                || app.pending_mobile_offers.values().any(|signed| {
                    signed.offer.opportunity_id == opportunity_id
                        && signed.offer.target_device_id == *device_id
                        && signed.offer.target_holder_key_thumbprint == peer.endpoint_key.thumbprint
                        && signed.offer.surface == surface
                })
            {
                continue;
            }
            let claims = PendingMobileOfferClaims {
                offer_id: format!("offer_{}", uuid::Uuid::new_v4().simple()),
                opportunity_id,
                target_device_id: device_id.clone(),
                target_holder_key_thumbprint: peer.endpoint_key.thumbprint.clone(),
                offered_mode: mode,
                surface,
                app_id: app_id.into(),
                call_id: snapshot.call_id.clone(),
                call_epoch: snapshot.call_epoch,
                owner_epoch: snapshot.owner_epoch,
                switchboard_revision: snapshot.switchboard_revision,
                remote_revision: snapshot.remote_revision,
                required_consent_policy_id: snapshot.remote_consent.policy_id.clone(),
                required_consent_policy_version: snapshot.remote_consent.policy_version,
                required_grants: vec![Grant::StateRead, Grant::RtcSignal, grant],
                issued_at: now,
                expires_at: now + MOBILE_OFFER_MAX_LIFETIME,
                jti: format!("offer_jti_{}", uuid::Uuid::new_v4().simple()),
            };
            claims.validate(now).map_err(|_| {
                GatewayError::fatal("internal", "generated mobile offer is invalid")
            })?;
            let signed = SignedPendingMobileOffer {
                offer_token: signer.sign_mobile_offer(&claims)?,
                offer: claims.clone(),
            };
            app.pending_mobile_offers.insert(claims.jti, signed);
        }
    }
    Ok(())
}

async fn handle_mobile_offer_answer(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: MobileOfferAnswerFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let fingerprint = fingerprint(&frame)?;
    let now = unix_now()?;
    let verified = gateway
        .inner
        .v2
        .signer()?
        .verify_mobile_offer(&frame.offer_token, now)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    app.mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile")
        .admit_general()?;
    match app.cached(&admission.subject_id, &frame.idempotency_key, fingerprint) {
        Cached::Replay(response) => return app.mobiles[&admission.subject_id].send(response),
        Cached::Conflict => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "idempotency_conflict",
                "offer answer idempotency key was reused",
                Some(&frame.request_id),
            )
        }
        Cached::Miss => {}
    }
    let peer = &app.mobiles[&admission.subject_id];
    let pending = app
        .pending_mobile_offers
        .get(&frame.offer_jti)
        .ok_or_else(|| {
            GatewayError::nonfatal("stale_offer", "mobile offer is no longer pending")
        })?;
    if pending.offer != verified
        || pending.offer_token != frame.offer_token
        || frame.offer_id != verified.offer_id
        || frame.offer_jti != verified.jti
        || frame.target_device_id != admission.subject_id
        || frame.target_device_id != verified.target_device_id
        || frame.target_holder_key_thumbprint != peer.endpoint_key.thumbprint
        || frame.target_holder_key_thumbprint != verified.target_holder_key_thumbprint
        || frame.offered_mode != verified.offered_mode
        || frame.call_id != verified.call_id
        || frame.call_epoch != verified.call_epoch
        || frame.owner_epoch != verified.owner_epoch
    {
        return Err(GatewayError::fatal(
            "offer_substitution",
            "mobile answer does not match the exact targeted offer",
        ));
    }
    let snapshot = app
        .snapshot
        .as_ref()
        .ok_or_else(|| GatewayError::nonfatal("stale_offer", "call state is unavailable"))?;
    if snapshot.call_id != verified.call_id
        || snapshot.call_epoch != verified.call_epoch
        || snapshot.owner_epoch != verified.owner_epoch
        || snapshot.switchboard_revision != verified.switchboard_revision
        || snapshot.remote_revision != verified.remote_revision
        || snapshot.remote_consent.policy_id != verified.required_consent_policy_id
        || snapshot.remote_consent.policy_version != verified.required_consent_policy_version
        || !snapshot.remote_consent.allows(verified.offered_mode)
        || verified
            .required_grants
            .iter()
            .any(|grant| !peer.has(*grant))
    {
        return Err(GatewayError::nonfatal(
            "stale_offer",
            "mobile offer no longer matches call, consent, or grants",
        ));
    }
    if app
        .mobile_offer_winners
        .contains_key(&verified.opportunity_id)
    {
        return Err(GatewayError::nonfatal(
            "offer_already_answered",
            "another approved endpoint already answered this offer",
        ));
    }
    // Acceptance changes the projected authoritative state. Reserve its
    // sequence before consuming the one-use offer, caching a positive result,
    // or notifying the mobile.
    let next_sequence = next_authoritative_sequence(app.sequence)?;
    // The app mutex is the first-winner linearization point.
    app.mobile_offer_winners
        .insert(verified.opportunity_id.clone(), verified.jti.clone());
    app.pending_mobile_offers
        .retain(|_, signed| signed.offer.opportunity_id != verified.opportunity_id);
    app.accepted_mobile_offers
        .insert(verified.jti.clone(), verified.clone());
    let response = response_json(
        "mobile_offer_accepted",
        &admission.app_id,
        json!({
            "requestId": frame.request_id,
            "offerId": verified.offer_id,
            "offerJti": verified.jti,
            "offeredMode": verified.offered_mode,
            "accepted": true
        }),
    )?;
    app.mobiles[&admission.subject_id].send(response.clone())?;
    app.cache(
        &admission.subject_id,
        &frame.idempotency_key,
        fingerprint,
        response,
    );
    app.sequence = next_sequence;
    broadcast_projected_snapshots(app, &admission.app_id)?;
    Ok(())
}

async fn handle_lease_request(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: LeaseRequestFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let fingerprint = fingerprint(&frame)?;
    let now = unix_now()?;
    let signer = gateway.inner.v2.signer()?.clone();
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    app.mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile")
        .admit_general()?;
    match app.cached(&admission.subject_id, &frame.idempotency_key, fingerprint) {
        Cached::Replay(response) => {
            return app.mobiles[&admission.subject_id].send(response);
        }
        Cached::Conflict => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "idempotency_conflict",
                "idempotency key was reused with a different request",
                Some(&frame.request_id),
            );
        }
        Cached::Miss => {}
    }
    ensure_grants(&app.mobiles[&admission.subject_id], frame.mode)?;
    let accepted_offer = app
        .accepted_mobile_offers
        .get(&frame.accepted_offer_jti)
        .cloned()
        .ok_or_else(|| {
            GatewayError::nonfatal(
                "offer_answer_required",
                "lease request requires a previously accepted one-use mobile offer",
            )
        })?;
    if accepted_offer.offer_id != frame.accepted_offer_id
        || accepted_offer.jti != frame.accepted_offer_jti
        || accepted_offer.target_device_id != admission.subject_id
        || accepted_offer.target_holder_key_thumbprint
            != app.mobiles[&admission.subject_id].endpoint_key.thumbprint
        || accepted_offer.offered_mode != frame.mode
        || accepted_offer.app_id != frame.app_id
        || accepted_offer.call_id != frame.call_id
        || accepted_offer.call_epoch != frame.expected_call_epoch
        || accepted_offer.owner_epoch != frame.expected_owner_epoch
        || accepted_offer.switchboard_revision != frame.expected_switchboard_revision
        || accepted_offer.remote_revision != frame.expected_remote_revision
        || accepted_offer.expires_at <= now
    {
        return Err(GatewayError::fatal(
            "offer_substitution",
            "lease request does not match its accepted mobile offer",
        ));
    }
    let snapshot = app
        .snapshot
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "call state is unavailable"))?;
    if snapshot.call_id != frame.call_id
        || snapshot.call_epoch != frame.expected_call_epoch
        || snapshot.owner_epoch != frame.expected_owner_epoch
        || snapshot.switchboard_revision != frame.expected_switchboard_revision
        || snapshot.remote_revision != frame.expected_remote_revision
    {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "stale_state",
            "claim epochs or revisions are stale",
            Some(&frame.request_id),
        );
    }
    if matches!(snapshot.service_mode, ServiceMode::Ended)
        || matches!(snapshot.telephony_state, TelephonyState::Ended)
    {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "call_ended",
            "call has ended",
            Some(&frame.request_id),
        );
    }
    if !snapshot.remote_consent.allows(frame.mode) {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "consent_required",
            "the current remote disclosure policy does not allow this media mode",
            Some(&frame.request_id),
        );
    }
    prune_expired(app, now, &admission.app_id);
    if app.leases.len() >= MAX_LEASES_PER_APP {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "capacity",
            "lease capacity is exhausted",
            Some(&frame.request_id),
        );
    }
    if app.leases.values().any(|lease| {
        lease.claims.device_id == admission.subject_id
            && lease.claims.rtc_session_id == frame.rtc_session_id
    }) {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "rtc_session_conflict",
            "rtcSessionId is already bound to a live lease",
            Some(&frame.request_id),
        );
    }

    let fence = if matches!(frame.mode, LeaseMode::Takeover) {
        if app.claim.is_some() {
            let response = response_json(
                "claim_rejected",
                &admission.app_id,
                json!({
                    "requestId": frame.request_id,
                    "code": "claim_already_won",
                    "message": "another endpoint already owns the provisional claim"
                }),
            )?;
            app.mobiles[&admission.subject_id].send(response.clone())?;
            app.cache(
                &admission.subject_id,
                &frame.idempotency_key,
                fingerprint,
                response,
            );
            return Ok(());
        }
        app.next_talk_fence()?
    } else {
        if matches!(frame.mode, LeaseMode::Consult) && app.claim.is_some() {
            let response = response_json(
                "claim_rejected",
                &admission.app_id,
                json!({
                    "requestId": frame.request_id,
                    "code": "claim_already_won",
                    "message": "another endpoint already owns the provisional claim"
                }),
            )?;
            app.mobiles[&admission.subject_id].send(response.clone())?;
            app.cache(
                &admission.subject_id,
                &frame.idempotency_key,
                fingerprint,
                response,
            );
            return Ok(());
        }
        0
    };
    let session_nonce = app.mobiles[&admission.subject_id].session_nonce.clone();
    let plugin_id = app.plugin_id.clone().ok_or_else(|| {
        GatewayError::fatal("endpoint_unavailable", "plugin identity is unavailable")
    })?;
    let plugin_key_thumbprint = app
        .plugin
        .as_ref()
        .expect("checked plugin")
        .endpoint_key
        .thumbprint
        .clone();
    let mobile_key_thumbprint = app.mobiles[&admission.subject_id]
        .endpoint_key
        .thumbprint
        .clone();
    let phase = if matches!(frame.mode, LeaseMode::Monitor) {
        LeasePhase::Active
    } else {
        LeasePhase::Prepared
    };
    let claims = new_claims(
        &admission.app_id,
        &plugin_id,
        &admission.subject_id,
        &frame.call_id,
        frame.expected_call_epoch,
        frame.expected_owner_epoch,
        frame.mode,
        phase,
        fence,
        &session_nonce,
        &frame.rtc_session_id,
        &plugin_key_thumbprint,
        &mobile_key_thumbprint,
        now,
    );
    app.accepted_mobile_offers.remove(&frame.accepted_offer_jti);
    let token = signer.sign(&claims)?;
    let provisional = !matches!(frame.mode, LeaseMode::Monitor);
    app.leases.insert(
        claims.jti.clone(),
        LeaseRecord {
            claims: claims.clone(),
            provisional,
            mobile_signal: SignalProgress::default(),
            plugin_signal: SignalProgress::default(),
        },
    );
    if provisional {
        app.claim = Some(TalkClaim {
            request_id: frame.request_id.clone(),
            device_id: admission.subject_id.clone(),
            mode: frame.mode,
            call_id: frame.call_id.clone(),
            call_epoch: frame.expected_call_epoch,
            fence,
            lease_jti: claims.jti.clone(),
            active: false,
        });
    }
    let response = response_json(
        if provisional {
            "claim_provisional"
        } else {
            "lease_granted"
        },
        &admission.app_id,
        json!({
            "requestId": frame.request_id,
            "leaseToken": token,
            "lease": claims,
            "provisional": provisional
        }),
    )?;
    app.mobiles[&admission.subject_id].send(response.clone())?;
    app.cache(
        &admission.subject_id,
        &frame.idempotency_key,
        fingerprint,
        response,
    );

    let notice = response_json(
        if provisional {
            "claim_proposal"
        } else {
            "lease_granted"
        },
        &admission.app_id,
        json!({
            "requestId": frame.request_id,
            "deviceId": admission.subject_id,
            "leaseToken": token,
            "lease": claims
        }),
    )?;
    app.plugin
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "plugin is unavailable"))?
        .send(notice)?;
    Ok(())
}

async fn handle_claim_decision(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: PluginClaimDecisionFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let signer = gateway.inner.v2.signer()?.clone();
    let now = unix_now()?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_plugin_mut(&mut apps, admission, connection_id)?;
    app.plugin
        .as_mut()
        .expect("checked plugin")
        .admit_general()?;
    let claim = app
        .claim
        .as_ref()
        .ok_or_else(|| GatewayError::nonfatal("stale_claim", "claim no longer exists"))?;
    if claim.request_id != frame.request_id
        || claim.device_id != frame.device_id
        || claim.call_id != frame.call_id
        || claim.call_epoch != frame.call_epoch
        || claim.fence != frame.fence
        || claim.active
    {
        return Err(GatewayError::nonfatal(
            "stale_claim",
            "claim decision does not match current claim",
        ));
    }
    let old_jti = claim.lease_jti.clone();
    let claim_mode = claim.mode;
    if !frame.accepted {
        app.leases.remove(&old_jti);
        app.claim = None;
        send_to_mobile(
            app,
            &frame.device_id,
            response_json(
                "claim_rejected",
                &admission.app_id,
                json!({
                    "requestId": frame.request_id,
                    "code": "endpoint_rejected",
                    "message": frame.reason.unwrap_or_else(|| "Aokie endpoint rejected the claim".into())
                }),
            )?,
        )?;
        return Ok(());
    }
    let snapshot = app
        .snapshot
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "call state is unavailable"))?;
    if frame.confirmed_owner_epoch <= snapshot.owner_epoch
        || frame.switchboard_revision < snapshot.switchboard_revision
        || frame.remote_revision < snapshot.remote_revision
    {
        return Err(GatewayError::fatal(
            "unsafe_ack",
            "accepted claim did not advance the physical owner epoch",
        ));
    }
    let stale_leases: Vec<_> = app
        .leases
        .keys()
        .filter(|jti| jti.as_str() != old_jti)
        .cloned()
        .collect();
    for jti in stale_leases {
        let _ = revoke_one(app, &admission.app_id, &jti, "owner_epoch_advanced", true);
    }
    let snapshot = app
        .snapshot
        .as_mut()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "call state is unavailable"))?;
    snapshot.owner_epoch = frame.confirmed_owner_epoch;
    snapshot.switchboard_revision = frame.switchboard_revision;
    snapshot.remote_revision = frame.remote_revision;
    snapshot.service_mode = match claim_mode {
        // The decision proves Desktop entered software hold and authorises a
        // fresh bidirectional consult offer; only the later authoritative
        // plugin snapshot may announce that isolated media is active.
        LeaseMode::Consult => ServiceMode::ConsultPending,
        LeaseMode::Takeover => ServiceMode::HumanActive,
        LeaseMode::Monitor => unreachable!(),
    };

    let previous = app.leases.remove(&old_jti).ok_or_else(|| {
        GatewayError::nonfatal("stale_claim", "provisional lease no longer exists")
    })?;
    let mut claims = previous.claims;
    claims.owner_epoch = frame.confirmed_owner_epoch;
    claims.phase = LeasePhase::Active;
    claims.tracks = tracks_for(claims.mode, LeasePhase::Active);
    claims.expires_at = now + LEASE_TTL;
    claims.jti = new_jti();
    let token = signer.sign(&claims)?;
    app.leases.insert(
        claims.jti.clone(),
        LeaseRecord {
            claims: claims.clone(),
            provisional: false,
            mobile_signal: previous.mobile_signal,
            plugin_signal: previous.plugin_signal,
        },
    );
    let claim = app.claim.as_mut().expect("claim remains");
    claim.active = true;
    claim.lease_jti = claims.jti.clone();
    send_to_mobile(
        app,
        &frame.device_id,
        response_json(
            "claim_active",
            &admission.app_id,
            json!({
                "requestId": frame.request_id,
                "leaseToken": token,
                "lease": claims,
                "provisional": false
            }),
        )?,
    )
}

async fn handle_lease_heartbeat(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: LeaseHeartbeatFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let fingerprint = fingerprint(&frame)?;
    let now = unix_now()?;
    let signer = gateway.inner.v2.signer()?.clone();
    let verified = signer.verify(&frame.lease_token, now)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    app.mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile")
        .admit_general()?;
    match app.cached(&admission.subject_id, &frame.idempotency_key, fingerprint) {
        Cached::Replay(response) => return app.mobiles[&admission.subject_id].send(response),
        Cached::Conflict => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "idempotency_conflict",
                "idempotency key was reused",
                Some(&frame.request_id),
            );
        }
        Cached::Miss => {}
    }
    validate_lease_binding(app, admission, &verified)?;
    let old = app
        .leases
        .remove(&verified.jti)
        .ok_or_else(|| GatewayError::nonfatal("invalid_lease", "lease was revoked"))?;
    let mut renewed = old.claims;
    renewed.expires_at = now + LEASE_TTL;
    renewed.jti = new_jti();
    let token = signer.sign(&renewed)?;
    app.leases.insert(
        renewed.jti.clone(),
        LeaseRecord {
            claims: renewed.clone(),
            provisional: old.provisional,
            mobile_signal: old.mobile_signal,
            plugin_signal: old.plugin_signal,
        },
    );
    if let Some(claim) = app
        .claim
        .as_mut()
        .filter(|claim| claim.lease_jti == verified.jti)
    {
        claim.lease_jti = renewed.jti.clone();
    }
    let response = response_json(
        "lease_renewed",
        &admission.app_id,
        json!({"requestId":frame.request_id,"leaseToken":token,"lease":renewed}),
    )?;
    app.mobiles[&admission.subject_id].send(response.clone())?;
    app.cache(
        &admission.subject_id,
        &frame.idempotency_key,
        fingerprint,
        response,
    );
    if let Some(plugin) = &app.plugin {
        plugin.send(response_json(
            "lease_renewed",
            &admission.app_id,
            json!({"deviceId":admission.subject_id,"leaseToken":token,"lease":renewed}),
        )?)?;
    }
    Ok(())
}

async fn handle_mobile_revoke(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: LeaseRevokeFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let fingerprint = fingerprint(&frame)?;
    let now = unix_now()?;
    let claims = gateway.inner.v2.signer()?.verify(&frame.lease_token, now)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    app.mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile")
        .admit_general()?;
    match app.cached(&admission.subject_id, &frame.idempotency_key, fingerprint) {
        Cached::Replay(response) => return app.mobiles[&admission.subject_id].send(response),
        Cached::Conflict => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "idempotency_conflict",
                "idempotency key was reused",
                Some(&frame.request_id),
            )
        }
        Cached::Miss => {}
    }
    validate_lease_binding(app, admission, &claims)?;
    revoke_one(app, &admission.app_id, &claims.jti, &frame.reason, true)?;
    let response = response_json(
        "lease_revoked",
        &admission.app_id,
        json!({"requestId":frame.request_id,"leaseId":claims.lease_id,"leaseJti":claims.jti,"reason":frame.reason}),
    )?;
    app.mobiles[&admission.subject_id].send(response.clone())?;
    app.cache(
        &admission.subject_id,
        &frame.idempotency_key,
        fingerprint,
        response,
    );
    Ok(())
}

async fn handle_plugin_revoke(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: PluginLeaseRevokeFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_plugin_mut(&mut apps, admission, connection_id)?;
    app.plugin
        .as_mut()
        .expect("checked plugin")
        .admit_general()?;
    let lease = app
        .leases
        .get(&frame.lease_jti)
        .ok_or_else(|| GatewayError::nonfatal("stale_lease", "lease no longer exists"))?;
    if lease.claims.device_id != frame.device_id
        || lease.claims.lease_id != frame.lease_id
        || lease.claims.call_id != frame.call_id
        || lease.claims.call_epoch != frame.call_epoch
        || lease.claims.fence != frame.fence
    {
        return Err(GatewayError::fatal(
            "epoch_mismatch",
            "lease revocation does not match current epochs",
        ));
    }
    let device_id = lease.claims.device_id.clone();
    revoke_one(
        app,
        &admission.app_id,
        &frame.lease_jti,
        &frame.reason,
        false,
    )?;
    send_to_mobile(
        app,
        &device_id,
        response_json(
            "lease_revoked",
            &admission.app_id,
            json!({"leaseId":frame.lease_id,"leaseJti":frame.lease_jti,"reason":frame.reason}),
        )?,
    )
}

async fn handle_assistance_request(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: PluginAssistanceRequestFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let now = unix_now()?;
    frame.validate(now).map_err(|_| {
        GatewayError::fatal(
            "invalid_assistance",
            "assistance request failed contract validation",
        )
    })?;
    if frame.expires_at.saturating_sub(now) > MAX_ASSISTANCE_TTL {
        return Err(GatewayError::fatal(
            "invalid_assistance",
            "assistance request lifetime exceeds five minutes",
        ));
    }
    let request_fingerprint = fingerprint(&frame)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_plugin_mut(&mut apps, admission, connection_id)?;
    app.plugin
        .as_mut()
        .expect("checked plugin")
        .admit_general()?;
    let snapshot = app
        .snapshot
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "call state is unavailable"))?;
    if frame.call_id != snapshot.call_id
        || frame.call_epoch != snapshot.call_epoch
        || frame.owner_epoch != snapshot.owner_epoch
        || frame.switchboard_revision != snapshot.switchboard_revision
        || frame.remote_revision != snapshot.remote_revision
    {
        return Err(GatewayError::nonfatal(
            "stale_state",
            "assistance request does not match authoritative call revisions",
        ));
    }
    if !snapshot.remote_consent.enabled
        || !snapshot.remote_consent.acknowledged
        || !snapshot.remote_consent.assistance_enabled
    {
        return Err(GatewayError::nonfatal(
            "consent_required",
            "current remote consent does not allow assistance",
        ));
    }
    if let Some(current) = app.assistance.as_ref() {
        if current.request.request_id == frame.request_id {
            return if current.fingerprint == request_fingerprint {
                Ok(())
            } else {
                Err(GatewayError::fatal(
                    "request_conflict",
                    "assistance requestId was reused with different content",
                ))
            };
        }
        if !current.answered && current.request.expires_at > now {
            return Err(GatewayError::nonfatal(
                "assistance_pending",
                "another assistance request is still pending",
            ));
        }
    }

    let encoded = serialize(&frame)?;
    let mut failed = Vec::new();
    for (device_id, peer) in &app.mobiles {
        if peer.has(Grant::AssistanceRead) && peer.send(encoded.clone()).is_err() {
            failed.push(device_id.clone());
        }
    }
    for device_id in failed {
        if let Some(peer) = app.mobiles.remove(&device_id) {
            peer.fence();
        }
        recover_device(app, &admission.app_id, &device_id, "outbound_queue_failed");
    }
    app.assistance = Some(AssistanceRecord {
        request: frame,
        fingerprint: request_fingerprint,
        answered: false,
    });
    Ok(())
}

async fn handle_assistance_answer(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: MobileAssistanceAnswerFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let request_fingerprint = fingerprint(&frame)?;
    let now = unix_now()?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    let peer = app
        .mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile");
    peer.admit_general()?;
    if !peer.has(Grant::AssistanceRespond) {
        return Err(GatewayError::fatal(
            "forbidden",
            "assistance_respond scope is required",
        ));
    }
    match app.cached(
        &admission.subject_id,
        &frame.idempotency_key,
        request_fingerprint,
    ) {
        Cached::Replay(response) => return app.mobiles[&admission.subject_id].send(response),
        Cached::Conflict => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "idempotency_conflict",
                "idempotency key was reused with a different answer",
                Some(&frame.request_id),
            )
        }
        Cached::Miss => {}
    }
    let request = app
        .assistance
        .as_ref()
        .ok_or_else(|| GatewayError::nonfatal("stale_assistance", "assistance request is gone"))?;
    if request.answered {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "already_answered",
            "assistance request already has its one permitted answer",
            Some(&frame.request_id),
        );
    }
    if request.request.expires_at <= now {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "assistance_expired",
            "assistance request expired",
            Some(&frame.request_id),
        );
    }
    let snapshot = app
        .snapshot
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "call state is unavailable"))?;
    if !snapshot.remote_consent.enabled
        || !snapshot.remote_consent.acknowledged
        || !snapshot.remote_consent.assistance_enabled
    {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "consent_required",
            "current remote consent does not allow assistance",
            Some(&frame.request_id),
        );
    }
    let expected = &request.request;
    if frame.request_id != expected.request_id
        || frame.call_id != expected.call_id
        || frame.call_epoch != expected.call_epoch
        || frame.owner_epoch != expected.owner_epoch
        || frame.switchboard_revision != expected.switchboard_revision
        || frame.remote_revision != expected.remote_revision
        || snapshot.call_id != expected.call_id
        || snapshot.call_epoch != expected.call_epoch
        || snapshot.owner_epoch != expected.owner_epoch
        || snapshot.switchboard_revision != expected.switchboard_revision
        || snapshot.remote_revision != expected.remote_revision
    {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "stale_state",
            "assistance answer does not match current call revisions",
            Some(&frame.request_id),
        );
    }
    let routed = PluginAssistanceAnswerFrame {
        kind: "assistance_answer".into(),
        schema_version: SCHEMA_VERSION,
        app_id: admission.app_id.clone(),
        device_id: admission.subject_id.clone(),
        request_id: frame.request_id.clone(),
        answer_id: frame.answer_id.clone(),
        call_id: frame.call_id.clone(),
        call_epoch: frame.call_epoch,
        owner_epoch: frame.owner_epoch,
        switchboard_revision: frame.switchboard_revision,
        remote_revision: frame.remote_revision,
        answer: frame.answer,
    };
    routed.validate().map_err(|_| {
        GatewayError::fatal("invalid_assistance", "routed assistance answer is invalid")
    })?;
    app.plugin
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "plugin is unavailable"))?
        .send(serialize(&routed)?)?;
    app.assistance
        .as_mut()
        .expect("validated assistance")
        .answered = true;
    let response = response_json(
        "assistance_answer_accepted",
        &admission.app_id,
        json!({
            "requestId": frame.request_id,
            "answerId": frame.answer_id,
            "accepted": true
        }),
    )?;
    app.mobiles[&admission.subject_id].send(response.clone())?;
    app.cache(
        &admission.subject_id,
        &frame.idempotency_key,
        request_fingerprint,
        response.clone(),
    );
    Ok(())
}

#[derive(Clone, Copy)]
struct EndCallerStateError {
    code: &'static str,
    message: &'static str,
}

struct EndCallerFenceInput<'a> {
    call_id: &'a str,
    call_epoch: u64,
    owner_epoch: u64,
    switchboard_revision: u64,
    remote_revision: u64,
    fence: u64,
}

fn validate_end_caller_state(
    app: &V2App,
    admission: &Admission,
    claims: &LeaseClaims,
    expected: EndCallerFenceInput<'_>,
) -> Result<(), EndCallerStateError> {
    let peer = app
        .mobiles
        .get(&admission.subject_id)
        .ok_or(EndCallerStateError {
            code: "stale_session",
            message: "the mobile session is no longer current",
        })?;
    if !peer.has(Grant::EndCaller) {
        return Err(EndCallerStateError {
            code: "end_caller_denied",
            message: "end_caller scope is required",
        });
    }
    if !peer.has(Grant::Takeover) {
        return Err(EndCallerStateError {
            code: "end_caller_denied",
            message: "caller ending requires the takeover scope",
        });
    }
    if claims.app_id != admission.app_id
        || app.plugin_id.as_deref() != Some(claims.plugin_id.as_str())
        || claims.device_id != admission.subject_id
        || claims.session_nonce != peer.session_nonce
        || claims.mode != LeaseMode::Takeover
        || claims.phase != LeasePhase::Active
        || claims.fence == 0
    {
        return Err(EndCallerStateError {
            code: "not_active_takeover_owner",
            message: "only the exact active takeover owner may end the caller call",
        });
    }
    let Some(lease) = app.leases.get(&claims.jti) else {
        return Err(EndCallerStateError {
            code: "stale_lease",
            message: "the takeover lease is no longer current",
        });
    };
    if lease.provisional || lease.claims != *claims {
        return Err(EndCallerStateError {
            code: "stale_lease",
            message: "the takeover lease no longer matches gateway state",
        });
    }
    let Some(claim) = app.claim.as_ref() else {
        return Err(EndCallerStateError {
            code: "not_active_takeover_owner",
            message: "the active takeover claim is no longer current",
        });
    };
    if !claim.active
        || claim.mode != LeaseMode::Takeover
        || claim.device_id != admission.subject_id
        || claim.call_id != claims.call_id
        || claim.call_epoch != claims.call_epoch
        || claim.fence != claims.fence
        || claim.lease_jti != claims.jti
    {
        return Err(EndCallerStateError {
            code: "not_active_takeover_owner",
            message: "another endpoint or call owns the active takeover fence",
        });
    }
    let Some(snapshot) = app.snapshot.as_ref() else {
        return Err(EndCallerStateError {
            code: "endpoint_unavailable",
            message: "authoritative call state is unavailable",
        });
    };
    if matches!(snapshot.telephony_state, TelephonyState::Ended)
        || matches!(snapshot.service_mode, ServiceMode::Ended)
    {
        return Err(EndCallerStateError {
            code: "call_ended",
            message: "the caller call has already ended",
        });
    }
    if snapshot.call_id != expected.call_id
        || snapshot.call_epoch != expected.call_epoch
        || snapshot.owner_epoch != expected.owner_epoch
        || snapshot.switchboard_revision != expected.switchboard_revision
        || snapshot.remote_revision != expected.remote_revision
        || claims.call_id != expected.call_id
        || claims.call_epoch != expected.call_epoch
        || claims.owner_epoch != expected.owner_epoch
        || claims.fence != expected.fence
    {
        return Err(EndCallerStateError {
            code: "stale_end_caller_state",
            message: "caller-ending epochs or revisions are stale",
        });
    }
    if snapshot.telephony_state != TelephonyState::Active
        || snapshot.service_mode != ServiceMode::HumanActive
    {
        return Err(EndCallerStateError {
            code: "not_active_takeover_owner",
            message: "the selected device is not the current active caller owner",
        });
    }
    if !snapshot.remote_consent.allows(LeaseMode::Takeover) {
        return Err(EndCallerStateError {
            code: "consent_required",
            message: "current remote consent no longer permits takeover control",
        });
    }
    Ok(())
}

async fn handle_end_caller_challenge_request(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: MobileEndCallerChallengeRequestFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let request_fingerprint = fingerprint(&frame)?;
    let now = unix_now()?;
    let verified = gateway.inner.v2.signer()?.verify(&frame.lease_token, now);
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    app.mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile")
        .admit_general()?;
    match app.cached(
        &admission.subject_id,
        &frame.idempotency_key,
        request_fingerprint,
    ) {
        Cached::Replay(response) => return app.mobiles[&admission.subject_id].send(response),
        Cached::Conflict => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "idempotency_conflict",
                "idempotency key was reused with a different caller-ending request",
                Some(&frame.request_id),
            )
        }
        Cached::Miss => {}
    }
    let claims = match verified {
        Ok(claims) => claims,
        Err(_) => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "stale_lease",
                "the takeover lease is invalid or expired",
                Some(&frame.request_id),
            )
        }
    };
    if let Err(error) = validate_end_caller_state(
        app,
        admission,
        &claims,
        EndCallerFenceInput {
            call_id: &frame.call_id,
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            fence: frame.fence,
        },
    ) {
        return send_mobile_error(
            app,
            &admission.subject_id,
            error.code,
            error.message,
            Some(&frame.request_id),
        );
    }
    if app.end_caller_operation.is_some() {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "end_caller_pending",
            "a caller-ending operation is already pending",
            Some(&frame.request_id),
        );
    }
    let confirmation_id = format!("end_confirm_{}", uuid::Uuid::new_v4().simple());
    let challenge = EndCallerChallengeFrame {
        kind: "end_caller_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: admission.app_id.clone(),
        request_id: frame.request_id.clone(),
        confirmation_id: confirmation_id.clone(),
        nonce: format!("nonce_{}", uuid::Uuid::new_v4().simple()),
        device_id: admission.subject_id.clone(),
        call_id: claims.call_id.clone(),
        call_epoch: claims.call_epoch,
        owner_epoch: claims.owner_epoch,
        switchboard_revision: frame.switchboard_revision,
        remote_revision: frame.remote_revision,
        lease_id: claims.lease_id.clone(),
        fence: claims.fence,
        expires_at: now + END_CALLER_CONFIRM_TTL,
    };
    challenge.validate(now).map_err(|_| {
        GatewayError::fatal("invalid_end_caller", "caller-ending challenge is invalid")
    })?;
    let encoded = serialize(&challenge)?;
    app.end_caller_challenges.retain(|_, current| {
        current.frame.expires_at > now && current.frame.device_id != admission.subject_id
    });
    app.end_caller_challenges.insert(
        confirmation_id,
        EndCallerChallengeRecord {
            frame: challenge,
            lease_jti: claims.jti,
        },
    );
    app.mobiles[&admission.subject_id].send(encoded.clone())?;
    app.cache(
        &admission.subject_id,
        &frame.idempotency_key,
        request_fingerprint,
        encoded,
    );
    Ok(())
}

async fn handle_end_caller_confirm(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: MobileEndCallerConfirmFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let request_fingerprint = fingerprint(&frame)?;
    let now = unix_now()?;
    let verified = gateway.inner.v2.signer()?.verify(&frame.lease_token, now);
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    app.mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile")
        .admit_general()?;
    match app.cached(
        &admission.subject_id,
        &frame.idempotency_key,
        request_fingerprint,
    ) {
        Cached::Replay(response) => return app.mobiles[&admission.subject_id].send(response),
        Cached::Conflict => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "idempotency_conflict",
                "idempotency key was reused with a different confirmation",
                Some(&frame.request_id),
            )
        }
        Cached::Miss => {}
    }
    if app
        .used_end_caller_confirmations
        .contains(&frame.confirmation_id)
    {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "confirmation_used",
            "the caller-ending confirmation was already consumed",
            Some(&frame.request_id),
        );
    }
    let Some(challenge) = app.end_caller_challenges.get(&frame.confirmation_id) else {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "confirmation_stale",
            "the caller-ending confirmation is no longer current",
            Some(&frame.request_id),
        );
    };
    if challenge.frame.expires_at <= now {
        app.end_caller_challenges.remove(&frame.confirmation_id);
        app.remember_used_end_caller_confirmation(frame.confirmation_id.clone());
        return send_mobile_error(
            app,
            &admission.subject_id,
            "confirmation_expired",
            "the caller-ending confirmation expired",
            Some(&frame.request_id),
        );
    }
    let challenge_matches = challenge.frame.nonce == frame.nonce
        && challenge.frame.device_id == admission.subject_id
        && challenge.frame.call_id == frame.call_id
        && challenge.frame.call_epoch == frame.call_epoch
        && challenge.frame.owner_epoch == frame.owner_epoch
        && challenge.frame.switchboard_revision == frame.switchboard_revision
        && challenge.frame.remote_revision == frame.remote_revision
        && challenge.frame.fence == frame.fence;
    let challenge_lease_jti = challenge.lease_jti.clone();
    let challenge_lease_id = challenge.frame.lease_id.clone();
    if !challenge_matches {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "confirmation_denied",
            "the confirmation nonce or exact caller-ending fence does not match",
            Some(&frame.request_id),
        );
    }
    let claims = match verified {
        Ok(claims)
            if claims.jti == challenge_lease_jti && claims.lease_id == challenge_lease_id =>
        {
            claims
        }
        _ => {
            return send_mobile_error(
                app,
                &admission.subject_id,
                "stale_lease",
                "the exact takeover lease changed before confirmation",
                Some(&frame.request_id),
            )
        }
    };
    if let Err(error) = validate_end_caller_state(
        app,
        admission,
        &claims,
        EndCallerFenceInput {
            call_id: &frame.call_id,
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            fence: frame.fence,
        },
    ) {
        return send_mobile_error(
            app,
            &admission.subject_id,
            error.code,
            error.message,
            Some(&frame.request_id),
        );
    }
    if app.end_caller_operation.is_some() {
        return send_mobile_error(
            app,
            &admission.subject_id,
            "end_caller_pending",
            "a caller-ending operation is already pending",
            Some(&frame.request_id),
        );
    }
    let execute = PluginEndCallerExecuteFrame {
        kind: "end_caller_execute".into(),
        schema_version: SCHEMA_VERSION,
        app_id: admission.app_id.clone(),
        operation_id: format!("end_op_{}", uuid::Uuid::new_v4().simple()),
        confirmation_id: frame.confirmation_id.clone(),
        device_id: admission.subject_id.clone(),
        call_id: frame.call_id.clone(),
        call_epoch: frame.call_epoch,
        owner_epoch: frame.owner_epoch,
        switchboard_revision: frame.switchboard_revision,
        remote_revision: frame.remote_revision,
        lease_id: claims.lease_id.clone(),
        lease_jti: claims.jti.clone(),
        fence: frame.fence,
    };
    execute.validate().map_err(|_| {
        GatewayError::fatal("invalid_end_caller", "caller-ending relay frame is invalid")
    })?;
    if app.plugin.is_none() {
        return Err(GatewayError::fatal(
            "endpoint_unavailable",
            "plugin is unavailable",
        ));
    }
    // Consume before relay while the app mutex is held.  No second socket or
    // concurrent confirm can win the same nonce.
    app.end_caller_challenges.remove(&frame.confirmation_id);
    app.remember_used_end_caller_confirmation(frame.confirmation_id.clone());
    app.plugin
        .as_ref()
        .expect("checked plugin")
        .send(serialize(&execute)?)?;
    let response = response_json(
        "end_caller_submitted",
        &admission.app_id,
        json!({
            "requestId": frame.request_id,
            "operationId": execute.operation_id,
            "confirmationId": execute.confirmation_id,
            "accepted": true
        }),
    )?;
    app.cache(
        &admission.subject_id,
        &frame.idempotency_key,
        request_fingerprint,
        response.clone(),
    );
    app.end_caller_operation = Some(EndCallerOperation {
        execute,
        idempotency_key: frame.idempotency_key,
        fingerprint: request_fingerprint,
    });
    app.mobiles[&admission.subject_id].send(response)
}

async fn handle_end_caller_result(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: PluginEndCallerResultFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_plugin_mut(&mut apps, admission, connection_id)?;
    app.plugin
        .as_mut()
        .expect("checked plugin")
        .admit_general()?;
    let Some(operation) = app.end_caller_operation.as_ref() else {
        return Err(GatewayError::nonfatal(
            "stale_end_caller_result",
            "caller-ending operation is no longer pending",
        ));
    };
    let expected = &operation.execute;
    if frame.operation_id != expected.operation_id
        || frame.confirmation_id != expected.confirmation_id
        || frame.device_id != expected.device_id
        || frame.call_id != expected.call_id
        || frame.call_epoch != expected.call_epoch
        || frame.owner_epoch != expected.owner_epoch
        || frame.switchboard_revision != expected.switchboard_revision
        || frame.remote_revision != expected.remote_revision
        || frame.lease_id != expected.lease_id
        || frame.fence != expected.fence
    {
        return Err(GatewayError::fatal(
            "end_caller_result_mismatch",
            "plugin caller-ending result crossed an operation fence",
        ));
    }
    let operation = app.end_caller_operation.take().expect("checked operation");
    let encoded = serialize(&frame)?;
    let mobile_result = send_to_mobile(app, &frame.device_id, encoded.clone());
    app.cache(
        &frame.device_id,
        &operation.idempotency_key,
        operation.fingerprint,
        encoded,
    );
    if frame.outcome == EndCallerOutcome::Completed {
        // The plugin has atomically validated the physical call and issued
        // the safe radio hangup.  Close the talk route immediately; the next
        // authoritative snapshot/call.ended remains cellular completion.
        let _ = revoke_one(
            app,
            &admission.app_id,
            &operation.execute.lease_jti,
            "caller_end_command_completed",
            true,
        );
    }
    mobile_result
}

async fn handle_mobile_rtc(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: MobileRtcSignalFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let now = unix_now()?;
    let authentication = frame
        .signal
        .verify_endpoint_authentication(now)
        .map_err(|_| {
            GatewayError::fatal(
                "invalid_endpoint_signature",
                "mobile RTC endpoint signature is invalid or stale",
            )
        })?;
    if let Some(authentication) = authentication {
        gateway
            .inner
            .v2
            .claim_endpoint_jti(authentication.jti(), authentication.expires_at())
            .await?;
    }
    let claims = gateway.inner.v2.signer()?.verify(&frame.lease_token, now)?;
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_mobile_app_mut(&mut apps, admission, connection_id)?;
    let peer = app
        .mobiles
        .get_mut(&admission.subject_id)
        .expect("checked mobile");
    peer.admit_general()?;
    peer.admit_rtc()?;
    if !peer.has(Grant::RtcSignal) {
        return Err(GatewayError::fatal(
            "forbidden",
            "rtc_signal scope is required",
        ));
    }
    let peer_session_nonce = peer.session_nonce.clone();
    validate_lease_binding(app, admission, &claims)?;
    if frame.plugin_id != claims.plugin_id
        || frame.device_id != claims.device_id
        || frame.lease_jti != claims.jti
        || frame.call_id != claims.call_id
        || frame.rtc_session_id != claims.rtc_session_id
        || frame.call_epoch != claims.call_epoch
        || frame.owner_epoch != claims.owner_epoch
        || frame.fence != claims.fence
    {
        return Err(GatewayError::fatal(
            "epoch_mismatch",
            "RTC signal does not match lease epochs",
        ));
    }
    if let Some(authentication) = authentication {
        if authentication.endpoint_role() != TokenAdmissionRole::Mobile
            || authentication.endpoint_session_nonce() != peer_session_nonce
            || authentication.holder_key_thumbprint() != claims.mobile_key_thumbprint
            || authentication.peer_key_thumbprint() != claims.plugin_key_thumbprint
        {
            return Err(GatewayError::fatal(
                "endpoint_substitution",
                "mobile RTC signature does not match the admitted lease endpoints",
            ));
        }
    }
    advance_signal(
        &mut app
            .leases
            .get_mut(&claims.jti)
            .expect("validated lease")
            .mobile_signal,
        &frame.signal,
        frame.sdp_revision,
        frame.transport_generation,
    )?;
    let sender = format!("mobile:{}", admission.subject_id);
    if !app.remember_signal(&sender, &frame.signal_id) {
        return Ok(());
    }
    let routed = PluginRtcSignalFrame {
        kind: "rtc_signal".into(),
        schema_version: SCHEMA_VERSION,
        app_id: admission.app_id.clone(),
        signal_id: frame.signal_id,
        plugin_id: claims.plugin_id,
        device_id: admission.subject_id.clone(),
        lease_jti: claims.jti,
        rtc_session_id: frame.rtc_session_id,
        sdp_revision: frame.sdp_revision,
        transport_generation: frame.transport_generation,
        call_id: frame.call_id,
        call_epoch: frame.call_epoch,
        owner_epoch: frame.owner_epoch,
        fence: frame.fence,
        signal: frame.signal,
    };
    app.plugin
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "plugin is unavailable"))?
        .send(serialize(&routed)?)
}

async fn handle_plugin_rtc(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    frame: PluginRtcSignalFrame,
) -> Result<(), GatewayError> {
    require_app(&frame.app_id, admission)?;
    let now = unix_now()?;
    let authentication = frame
        .signal
        .verify_endpoint_authentication(now)
        .map_err(|_| {
            GatewayError::fatal(
                "invalid_endpoint_signature",
                "plugin RTC endpoint signature is invalid or stale",
            )
        })?;
    if let Some(authentication) = authentication {
        gateway
            .inner
            .v2
            .claim_endpoint_jti(authentication.jti(), authentication.expires_at())
            .await?;
    }
    let mut apps = gateway.inner.v2.apps.lock().await;
    let app = current_plugin_mut(&mut apps, admission, connection_id)?;
    let plugin = app.plugin.as_mut().expect("checked plugin");
    plugin.admit_general()?;
    plugin.admit_rtc()?;
    let plugin_session_nonce = plugin.session_nonce.clone();
    {
        let lease = app
            .leases
            .get_mut(&frame.lease_jti)
            .ok_or_else(|| GatewayError::nonfatal("stale_lease", "lease no longer exists"))?;
        if lease.claims.device_id != frame.device_id
            || lease.claims.plugin_id != frame.plugin_id
            || lease.claims.rtc_session_id != frame.rtc_session_id
            || lease.claims.call_id != frame.call_id
            || lease.claims.call_epoch != frame.call_epoch
            || lease.claims.owner_epoch != frame.owner_epoch
            || lease.claims.fence != frame.fence
        {
            return Err(GatewayError::fatal(
                "epoch_mismatch",
                "RTC signal does not match lease epochs",
            ));
        }
        if lease.claims.expires_at <= now {
            return Err(GatewayError::nonfatal("stale_lease", "lease expired"));
        }
        if let Some(authentication) = authentication {
            if authentication.endpoint_role() != TokenAdmissionRole::Plugin
                || authentication.endpoint_session_nonce() != plugin_session_nonce
                || authentication.holder_key_thumbprint() != lease.claims.plugin_key_thumbprint
                || authentication.peer_key_thumbprint() != lease.claims.mobile_key_thumbprint
            {
                return Err(GatewayError::fatal(
                    "endpoint_substitution",
                    "plugin RTC signature does not match the admitted lease endpoints",
                ));
            }
        }
        advance_signal(
            &mut lease.plugin_signal,
            &frame.signal,
            frame.sdp_revision,
            frame.transport_generation,
        )?;
    }
    let sender = format!("plugin:{}", admission.subject_id);
    if !app.remember_signal(&sender, &frame.signal_id) {
        return Ok(());
    }
    send_to_mobile(
        app,
        &frame.device_id,
        response_json(
            "rtc_signal",
            &admission.app_id,
            json!({
                "signalId": frame.signal_id,
                "pluginId": frame.plugin_id,
                "deviceId": frame.device_id,
                "leaseJti": frame.lease_jti,
                "rtcSessionId": frame.rtc_session_id,
                "sdpRevision": frame.sdp_revision,
                "transportGeneration": frame.transport_generation,
                "callId": frame.call_id,
                "callEpoch": frame.call_epoch,
                "ownerEpoch": frame.owner_epoch,
                "fence": frame.fence,
                "signal": frame.signal
            }),
        )?,
    )
}

async fn unregister(gateway: &Gateway, admission: &Admission, connection_id: &str) {
    let mut apps = gateway.inner.v2.apps.lock().await;
    let Some(app) = apps.get_mut(&admission.app_id) else {
        return;
    };
    match admission.role {
        AdmissionRole::Mobile => {
            if app
                .mobiles
                .get(&admission.subject_id)
                .is_some_and(|peer| peer.connection_id == connection_id)
            {
                app.mobiles.remove(&admission.subject_id);
                recover_device(
                    app,
                    &admission.app_id,
                    &admission.subject_id,
                    "endpoint_disconnected",
                );
            }
        }
        AdmissionRole::Plugin => {
            if app
                .plugin
                .as_ref()
                .is_some_and(|peer| peer.connection_id == connection_id)
            {
                app.plugin = None;
                app.plugin_id = None;
                app.approved_mobile_key_thumbprints.clear();
                app.peer_roster_revision = None;
                app.peer_roster_hash = None;
                for (_, peer) in app.mobiles.drain() {
                    peer.fence();
                }
                app.snapshot = None;
                app.idle = None;
                app.sequence = 0;
                app.claim = None;
                app.leases.clear();
                app.idempotency.clear();
                app.idempotency_order.clear();
                app.signals.clear();
                app.signal_order.clear();
                app.assistance = None;
                app.end_caller_challenges.clear();
                app.used_end_caller_confirmations.clear();
                app.used_end_caller_order.clear();
                app.end_caller_operation = None;
                app.pending_mobile_offers.clear();
                app.accepted_mobile_offers.clear();
                app.mobile_offer_winners.clear();
            }
        }
        AdmissionRole::Desktop => {}
    }
}

fn authoritative_sync(app: &V2App, app_id: &str, peer: &V2Peer) -> Result<String, GatewayError> {
    match (app.snapshot.as_ref(), app.idle.as_ref()) {
        (Some(_), None) => projected_snapshot(app, app_id, peer),
        (None, Some(_)) => idle_sync(app, app_id, peer),
        _ => Err(GatewayError::fatal(
            "endpoint_unavailable",
            "authoritative plugin state is unavailable or inconsistent",
        )),
    }
}

fn idle_sync(app: &V2App, app_id: &str, peer: &V2Peer) -> Result<String, GatewayError> {
    if app.snapshot.is_some() || app.idle.is_none() {
        return Err(GatewayError::fatal(
            "endpoint_unavailable",
            "authoritative idle state is unavailable",
        ));
    }
    let frame = MobileIdleSyncFrame {
        kind: "idle_sync".into(),
        schema_version: SCHEMA_VERSION,
        app_id: app_id.to_owned(),
        sequence: app.sequence,
        grants: peer.grants.clone(),
    };
    frame.validate().map_err(|_| {
        GatewayError::fatal("invalid_projection", "mobile idle projection is invalid")
    })?;
    serialize(&frame)
}

fn broadcast_idle_sync(app: &mut V2App, app_id: &str) -> Result<(), GatewayError> {
    let device_ids = app.mobiles.keys().cloned().collect::<Vec<_>>();
    let mut failed = Vec::new();
    for device_id in device_ids {
        let Some(peer) = app.mobiles.get(&device_id) else {
            continue;
        };
        let encoded = idle_sync(app, app_id, peer)?;
        if peer.send(encoded).is_err() {
            failed.push(device_id);
        }
    }
    for device_id in failed {
        if let Some(peer) = app.mobiles.remove(&device_id) {
            peer.fence();
        }
        recover_device(app, app_id, &device_id, "outbound_queue_failed");
    }
    Ok(())
}

fn projected_snapshot(app: &V2App, app_id: &str, peer: &V2Peer) -> Result<String, GatewayError> {
    let snapshot = app
        .snapshot
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "call state is unavailable"))?;
    let projected = ProjectedCallSnapshot {
        call_id: snapshot.call_id.clone(),
        call_epoch: snapshot.call_epoch,
        owner_epoch: snapshot.owner_epoch,
        switchboard_revision: snapshot.switchboard_revision,
        remote_revision: snapshot.remote_revision,
        telephony_state: snapshot.telephony_state.clone(),
        service_mode: snapshot.service_mode.clone(),
        media_state: snapshot.media_state.clone(),
        remote_capabilities: snapshot.remote_capabilities.clone(),
        secondary_call_policy: snapshot.secondary_call_policy,
        secondary_call: snapshot.secondary_call.clone(),
        remote_consent: snapshot.remote_consent.clone(),
        caller: peer
            .has(Grant::CallerRead)
            .then(|| snapshot.caller.clone())
            .flatten(),
        captions: (peer.has(Grant::CaptionsRead)
            && snapshot.remote_consent.enabled
            && snapshot.remote_consent.acknowledged
            && snapshot.remote_consent.captions_enabled)
            .then(|| snapshot.captions.clone()),
        participants: projected_participants(app, app_id, peer),
        audio_levels: peer
            .has(Grant::AudioLevelsRead)
            .then(|| snapshot.audio_levels.clone())
            .flatten(),
        pending_mobile_offers: app
            .mobiles
            .iter()
            .find_map(|(device_id, candidate)| {
                (candidate.connection_id == peer.connection_id).then_some(device_id)
            })
            .map(|device_id| {
                let mut offers = app
                    .pending_mobile_offers
                    .values()
                    .filter(|signed| signed.offer.target_device_id == *device_id)
                    .cloned()
                    .collect::<Vec<_>>();
                offers.sort_by(|left, right| left.offer.offer_id.cmp(&right.offer.offer_id));
                offers
            })
            .unwrap_or_default(),
        occurred_at: snapshot.occurred_at.clone(),
    };
    projected.validate().map_err(|_| {
        GatewayError::fatal(
            "invalid_projection",
            "mobile snapshot projection is invalid",
        )
    })?;
    serialize(&MobileSnapshotFrame {
        kind: "snapshot".into(),
        schema_version: SCHEMA_VERSION,
        app_id: app_id.to_owned(),
        sequence: app.sequence,
        grants: peer.grants.clone(),
        snapshot: projected,
    })
}

fn broadcast_projected_snapshots(app: &mut V2App, app_id: &str) -> Result<(), GatewayError> {
    let device_ids = app.mobiles.keys().cloned().collect::<Vec<_>>();
    let mut failed = Vec::new();
    for device_id in device_ids {
        let Some(peer) = app.mobiles.get(&device_id) else {
            continue;
        };
        let encoded = projected_snapshot(app, app_id, peer)?;
        if peer.send(encoded).is_err() {
            failed.push(device_id);
        }
    }
    for device_id in failed {
        if let Some(peer) = app.mobiles.remove(&device_id) {
            peer.fence();
        }
        recover_device(app, app_id, &device_id, "outbound_queue_failed");
    }
    Ok(())
}

fn projected_participants(
    app: &V2App,
    app_id: &str,
    viewer: &V2Peer,
) -> Vec<aokie_protocol::v2::ParticipantPresence> {
    use aokie_protocol::v2::{ParticipantMode, ParticipantPresence, ParticipantState};
    if !viewer.has(Grant::ParticipantsRead) {
        return Vec::new();
    }
    let reveal_identity = viewer.has(Grant::ParticipantIdentityRead);
    let mut participants = app
        .mobiles
        .keys()
        .map(|device_id| {
            let lease = app
                .leases
                .values()
                .filter(|record| record.claims.device_id == *device_id)
                .max_by_key(|record| record.claims.expires_at);
            let (mode, state) = lease.map_or(
                (ParticipantMode::Observer, ParticipantState::Connected),
                |record| {
                    let mode = match record.claims.mode {
                        LeaseMode::Monitor => ParticipantMode::Observer,
                        LeaseMode::Consult => ParticipantMode::Advisor,
                        LeaseMode::Takeover => ParticipantMode::Talker,
                    };
                    let state = match record.claims.phase {
                        LeasePhase::Prepared => ParticipantState::Prepared,
                        LeasePhase::Active => ParticipantState::Active,
                    };
                    (mode, state)
                },
            );
            let opaque = Sha256::digest(format!("{app_id}\0{device_id}").as_bytes());
            ParticipantPresence {
                participant_id: format!("participant_{}", &hex(&opaque)[..24]),
                mode,
                state,
                subject_id: reveal_identity.then(|| device_id.clone()),
                display_label: None,
            }
        })
        .collect::<Vec<_>>();
    participants.sort_by(|left, right| left.participant_id.cmp(&right.participant_id));
    participants
}

fn new_claims(
    app_id: &str,
    plugin_id: &str,
    device_id: &str,
    call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
    mode: LeaseMode,
    phase: LeasePhase,
    fence: u64,
    session_nonce: &str,
    rtc_session_id: &str,
    plugin_key_thumbprint: &str,
    mobile_key_thumbprint: &str,
    now: u64,
) -> LeaseClaims {
    LeaseClaims {
        aud: LEASE_AUDIENCE.into(),
        app_id: app_id.into(),
        plugin_id: plugin_id.into(),
        device_id: device_id.into(),
        plugin_key_thumbprint: plugin_key_thumbprint.into(),
        mobile_key_thumbprint: mobile_key_thumbprint.into(),
        call_id: call_id.into(),
        call_epoch,
        owner_epoch,
        mode,
        phase,
        tracks: tracks_for(mode, phase),
        expires_at: now + LEASE_TTL,
        lease_id: new_lease_id(),
        jti: new_jti(),
        fence,
        session_nonce: session_nonce.into(),
        rtc_session_id: rtc_session_id.into(),
    }
}

fn new_jti() -> String {
    format!("lease_{}", uuid::Uuid::new_v4().simple())
}

fn new_lease_id() -> String {
    format!("media_{}", uuid::Uuid::new_v4().simple())
}

fn validate_lease_binding(
    app: &V2App,
    admission: &Admission,
    claims: &LeaseClaims,
) -> Result<(), GatewayError> {
    let peer = app
        .mobiles
        .get(&admission.subject_id)
        .ok_or_else(|| GatewayError::fatal("stale_session", "mobile session is not current"))?;
    if claims.app_id != admission.app_id
        || app.plugin_id.as_deref() != Some(&claims.plugin_id)
        || claims.device_id != admission.subject_id
        || claims.session_nonce != peer.session_nonce
        || claims.mobile_key_thumbprint != peer.endpoint_key.thumbprint
        || app
            .plugin
            .as_ref()
            .is_none_or(|plugin| claims.plugin_key_thumbprint != plugin.endpoint_key.thumbprint)
    {
        return Err(GatewayError::fatal(
            "lease_binding",
            "lease is bound to another session",
        ));
    }
    let record = app
        .leases
        .get(&claims.jti)
        .ok_or_else(|| GatewayError::nonfatal("invalid_lease", "lease was revoked"))?;
    if record.claims != *claims {
        return Err(GatewayError::fatal(
            "lease_binding",
            "lease does not match gateway state",
        ));
    }
    let snapshot = app
        .snapshot
        .as_ref()
        .ok_or_else(|| GatewayError::fatal("endpoint_unavailable", "call state is unavailable"))?;
    if snapshot.call_id != claims.call_id
        || snapshot.call_epoch != claims.call_epoch
        || snapshot.owner_epoch != claims.owner_epoch
    {
        return Err(GatewayError::nonfatal(
            "stale_lease",
            "lease epochs are stale",
        ));
    }
    Ok(())
}

fn ensure_grants(peer: &V2Peer, mode: LeaseMode) -> Result<(), GatewayError> {
    let mode_grant = match mode {
        LeaseMode::Monitor => Grant::Monitor,
        LeaseMode::Consult => Grant::Consult,
        LeaseMode::Takeover => Grant::Takeover,
    };
    if peer.has(mode_grant) && peer.has(Grant::RtcSignal) {
        Ok(())
    } else {
        Err(GatewayError::fatal(
            "forbidden",
            "required media scopes are missing",
        ))
    }
}

fn advance_signal(
    progress: &mut SignalProgress,
    signal: &RtcSignal,
    sdp_revision: u64,
    transport_generation: u64,
) -> Result<(), GatewayError> {
    match signal {
        RtcSignal::Offer { .. } | RtcSignal::Answer { .. } => {
            let expected_revision = progress.sdp_revision.checked_add(1).ok_or_else(|| {
                GatewayError::fatal("revision_exhausted", "SDP revision exhausted")
            })?;
            let generation_valid = if progress.transport_generation == 0 {
                transport_generation == 1
            } else {
                transport_generation == progress.transport_generation
                    || transport_generation == progress.transport_generation + 1
            };
            if sdp_revision != expected_revision || !generation_valid {
                return Err(GatewayError::fatal(
                    "signal_order",
                    "SDP revision or transport generation is not the next monotonic value",
                ));
            }
            progress.sdp_revision = sdp_revision;
            progress.transport_generation = transport_generation;
            Ok(())
        }
        RtcSignal::Ice { .. } | RtcSignal::IceComplete { .. } | RtcSignal::Close { .. } => {
            if progress.sdp_revision == 0
                || sdp_revision != progress.sdp_revision
                || transport_generation != progress.transport_generation
            {
                return Err(GatewayError::fatal(
                    "signal_order",
                    "ICE/close must match the current SDP and transport generation",
                ));
            }
            Ok(())
        }
    }
}

fn current_plugin_mut<'a>(
    apps: &'a mut HashMap<String, V2App>,
    admission: &Admission,
    connection_id: &str,
) -> Result<&'a mut V2App, GatewayError> {
    let app = apps.get_mut(&admission.app_id).ok_or_else(|| {
        GatewayError::fatal("stale_session", "application session is unavailable")
    })?;
    if !app
        .plugin
        .as_ref()
        .is_some_and(|peer| peer.connection_id == connection_id)
    {
        return Err(GatewayError::fatal(
            "stale_session",
            "plugin session is not current",
        ));
    }
    Ok(app)
}

fn current_mobile_app_mut<'a>(
    apps: &'a mut HashMap<String, V2App>,
    admission: &Admission,
    connection_id: &str,
) -> Result<&'a mut V2App, GatewayError> {
    let app = apps.get_mut(&admission.app_id).ok_or_else(|| {
        GatewayError::fatal("stale_session", "application session is unavailable")
    })?;
    if !app
        .mobiles
        .get(&admission.subject_id)
        .is_some_and(|peer| peer.connection_id == connection_id)
    {
        return Err(GatewayError::fatal(
            "stale_session",
            "mobile session is not current",
        ));
    }
    Ok(app)
}

fn require_app(app_id: &str, admission: &Admission) -> Result<(), GatewayError> {
    if app_id == admission.app_id {
        Ok(())
    } else {
        Err(GatewayError::fatal(
            "identity_mismatch",
            "frame app does not match admission",
        ))
    }
}

fn recover_device(app: &mut V2App, app_id: &str, device_id: &str, reason: &str) {
    let revoked: Vec<_> = app
        .leases
        .iter()
        .filter(|(_, record)| record.claims.device_id == device_id)
        .map(|(jti, _)| jti.clone())
        .collect();
    for jti in revoked {
        let _ = revoke_one(app, app_id, &jti, reason, true);
    }
}

fn revoke_all(app: &mut V2App, app_id: &str, reason: &str) {
    let leases: Vec<_> = app.leases.keys().cloned().collect();
    for jti in leases {
        let _ = revoke_one(app, app_id, &jti, reason, true);
    }
    app.claim = None;
}

fn revoke_one(
    app: &mut V2App,
    app_id: &str,
    jti: &str,
    reason: &str,
    notify_plugin: bool,
) -> Result<(), GatewayError> {
    let Some(record) = app.leases.remove(jti) else {
        return Err(GatewayError::nonfatal(
            "stale_lease",
            "lease no longer exists",
        ));
    };
    app.end_caller_challenges
        .retain(|_, challenge| challenge.lease_jti != jti);
    if app
        .end_caller_operation
        .as_ref()
        .is_some_and(|operation| operation.execute.lease_jti == jti)
    {
        let operation = app.end_caller_operation.take().expect("checked operation");
        let failed = PluginEndCallerResultFrame {
            kind: "end_caller_result".into(),
            schema_version: SCHEMA_VERSION,
            app_id: app_id.to_owned(),
            operation_id: operation.execute.operation_id.clone(),
            confirmation_id: operation.execute.confirmation_id.clone(),
            device_id: operation.execute.device_id.clone(),
            call_id: operation.execute.call_id.clone(),
            call_epoch: operation.execute.call_epoch,
            owner_epoch: operation.execute.owner_epoch,
            switchboard_revision: operation.execute.switchboard_revision,
            remote_revision: operation.execute.remote_revision,
            lease_id: operation.execute.lease_id.clone(),
            fence: operation.execute.fence,
            outcome: EndCallerOutcome::Failed,
            code: Some("end_caller_canceled".into()),
            message: Some("the takeover lease was revoked before caller ending completed".into()),
        };
        let encoded = serialize(&failed)?;
        if app.mobiles.contains_key(&operation.execute.device_id) {
            send_to_mobile(app, &operation.execute.device_id, encoded.clone())?;
        }
        app.cache(
            &operation.execute.device_id,
            &operation.idempotency_key,
            operation.fingerprint,
            encoded,
        );
    }
    if app
        .claim
        .as_ref()
        .is_some_and(|claim| claim.lease_jti == jti)
    {
        app.claim = None;
    }
    if notify_plugin {
        if let Some(plugin) = &app.plugin {
            plugin.send(response_json(
                "lease_revoked",
                app_id,
                json!({
                    "deviceId": record.claims.device_id,
                    "leaseId": record.claims.lease_id,
                    "leaseJti": record.claims.jti,
                    "callId": record.claims.call_id,
                    "callEpoch": record.claims.call_epoch,
                    "fence": record.claims.fence,
                    "reason": reason
                }),
            )?)?;
        }
    }
    Ok(())
}

fn prune_expired(app: &mut V2App, now: u64, app_id: &str) {
    let expired: Vec<_> = app
        .leases
        .iter()
        .filter(|(_, record)| record.claims.expires_at <= now)
        .map(|(jti, _)| jti.clone())
        .collect();
    for jti in expired {
        let _ = revoke_one(app, app_id, &jti, "lease_expired", true);
    }
}

fn send_to_mobile(app: &V2App, device_id: &str, encoded: String) -> Result<(), GatewayError> {
    app.mobiles
        .get(device_id)
        .ok_or_else(|| {
            GatewayError::nonfatal("endpoint_unavailable", "mobile endpoint is unavailable")
        })?
        .send(encoded)
}

fn send_mobile_error(
    app: &V2App,
    device_id: &str,
    code: &'static str,
    message: &'static str,
    request_id: Option<&str>,
) -> Result<(), GatewayError> {
    let encoded = serde_json::to_string(&json!({
        "kind":"error",
        "schemaVersion":SCHEMA_VERSION,
        "code":code,
        "message":message,
        "requestId":request_id
    }))
    .map_err(|_| GatewayError::fatal("internal", "cannot encode error"))?;
    send_to_mobile(app, device_id, encoded)
}

fn send_error(tx: &mpsc::Sender<Message>, error: &GatewayError, request_id: Option<&str>) {
    if let Ok(encoded) = serde_json::to_string(&json!({
        "kind":"error",
        "schemaVersion":SCHEMA_VERSION,
        "code":error.code,
        "message":error.message,
        "requestId":request_id
    })) {
        let _ = tx.try_send(Message::Text(encoded));
    }
}

fn response_json(kind: &str, app_id: &str, fields: Value) -> Result<String, GatewayError> {
    let mut object = fields
        .as_object()
        .cloned()
        .ok_or_else(|| GatewayError::fatal("internal", "response fields are invalid"))?;
    object.insert("kind".into(), Value::String(kind.into()));
    object.insert("schemaVersion".into(), Value::from(SCHEMA_VERSION));
    object.insert("appId".into(), Value::String(app_id.into()));
    serde_json::to_string(&object)
        .map_err(|_| GatewayError::fatal("internal", "cannot encode response"))
}

fn serialize<T: Serialize>(value: &T) -> Result<String, GatewayError> {
    serde_json::to_string(value).map_err(|_| GatewayError::fatal("internal", "cannot encode frame"))
}

fn fingerprint<T: Serialize>(value: &T) -> Result<[u8; 32], GatewayError> {
    let encoded = serde_json::to_vec(value)
        .map_err(|_| GatewayError::fatal("internal", "cannot encode request"))?;
    Ok(Sha256::digest(encoded).into())
}

fn unix_now() -> Result<u64, GatewayError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| GatewayError::fatal("clock_invalid", "system clock is before Unix epoch"))
}

fn admission_expiry_delay(exp: Option<u64>, now: u64) -> Option<Duration> {
    exp.map(|expires_at| Duration::from_secs(expires_at.saturating_sub(now)))
}

async fn wait_for_admission_expiry(exp: Option<u64>) {
    let Some(delay) = admission_expiry_delay(exp, unix_now().unwrap_or(u64::MAX)) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::time::sleep(delay).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AdmissionSpec, GatewayConfig};
    use aokie_protocol::v2::{
        AdmissionClaims, AdmissionRole as TokenRole, CarrierHoldEvidence, EndpointBindingClaims,
        MediaState, RemoteCapabilities, RemoteConsentPolicy, RtcSignal, SecondaryCallObservation,
        SecondaryCallPolicy, SignedEndpointBinding, SignedTrickleCandidateEnvelope,
        TrickleCandidateClaims, ADMISSION_AUDIENCE,
    };

    fn test_endpoint_key(seed: u8) -> EndpointPublicKey {
        EndpointPublicKey::from_ed25519_bytes(&[seed; 32])
    }

    fn plugin_key() -> EndpointPublicKey {
        test_endpoint_key(3)
    }

    fn mobile_key() -> EndpointPublicKey {
        test_endpoint_key(7)
    }

    fn dummy_binding(role: TokenRole, revision: u64, generation: u64) -> SignedEndpointBinding {
        let endpoint_key = match role {
            TokenRole::Plugin => plugin_key(),
            TokenRole::Mobile => mobile_key(),
        };
        SignedEndpointBinding {
            claims: EndpointBindingClaims {
                app_id: "app_a".into(),
                plugin_id: "plugin_a".into(),
                device_id: "device_a".into(),
                rtc_session_id: "rtc_a".into(),
                endpoint_session_nonce: "test_session".into(),
                lease_jti: "lease_a".into(),
                endpoint_role: role,
                holder_key_thumbprint: endpoint_key.thumbprint.clone(),
                peer_key_thumbprint: "peer_thumbprint".into(),
                call_id: "call_a".into(),
                call_epoch: 7,
                owner_epoch: 3,
                fence: 9,
                sdp_revision: revision,
                transport_generation: generation,
                dtls_fingerprint: "sha-256 AA".into(),
                sdp_sha256: "sdp_hash".into(),
                nonce: format!("nonce_{revision}_{generation}"),
                jti: format!("jti_{revision}_{generation}"),
                issued_at: 1,
                expires_at: 2,
            },
            endpoint_key,
            signature: "A".repeat(86),
        }
    }

    fn dummy_candidate(
        candidate: Option<&str>,
        revision: u64,
        generation: u64,
    ) -> SignedTrickleCandidateEnvelope {
        let endpoint_key = mobile_key();
        SignedTrickleCandidateEnvelope {
            claims: TrickleCandidateClaims {
                app_id: "app_a".into(),
                plugin_id: "plugin_a".into(),
                device_id: "device_a".into(),
                rtc_session_id: "rtc_a".into(),
                endpoint_session_nonce: "test_session".into(),
                lease_jti: "lease_a".into(),
                endpoint_role: TokenRole::Mobile,
                holder_key_thumbprint: endpoint_key.thumbprint.clone(),
                peer_key_thumbprint: "peer_thumbprint".into(),
                call_id: "call_a".into(),
                call_epoch: 7,
                owner_epoch: 3,
                fence: 9,
                sdp_revision: revision,
                transport_generation: generation,
                candidate: candidate.map(str::to_owned),
                sdp_mid: candidate.map(|_| "0".into()),
                sdp_m_line_index: candidate.map(|_| 0),
                end_of_candidates: candidate.is_none(),
                nonce: format!("candidate_nonce_{revision}_{generation}"),
                jti: format!("candidate_jti_{revision}_{generation}"),
                issued_at: 1,
                expires_at: 2,
            },
            endpoint_key,
            signature: "A".repeat(86),
        }
    }

    fn snapshot(owner_epoch: u64) -> AuthoritativeCallSnapshot {
        AuthoritativeCallSnapshot {
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch,
            switchboard_revision: 11,
            remote_revision: 13,
            telephony_state: TelephonyState::Active,
            service_mode: ServiceMode::AokieActive,
            media_state: MediaState::Ready,
            remote_capabilities: RemoteCapabilities {
                software_hold: true,
                carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                secondary_call_observation: SecondaryCallObservation::Unknown,
                voice_consult: true,
                takeover: true,
            },
            secondary_call_policy: SecondaryCallPolicy::Normal,
            secondary_call: None,
            remote_consent: RemoteConsentPolicy {
                policy_id: "aokie_remote_access".into(),
                policy_version: 3,
                enabled: true,
                acknowledged: true,
                acknowledged_at: Some("2026-07-15T00:00:00Z".into()),
                expires_at: None,
                captions_enabled: true,
                assistance_enabled: true,
                monitor_enabled: true,
                consult_enabled: true,
                takeover_enabled: true,
            },
            caller: Some(aokie_protocol::v2::CallerProjection {
                label: Some("Private caller".into()),
                masked_number: Some("***123".into()),
            }),
            captions: vec![aokie_protocol::v2::Caption {
                caption_id: "caption_a".into(),
                speaker: "caller".into(),
                text: "hello".into(),
                occurred_at: "2026-07-16T00:00:00Z".into(),
                final_text: true,
            }],
            audio_levels: None,
            occurred_at: "2026-07-16T00:00:00Z".into(),
        }
    }

    fn peer(id: &str, nonce: &str, grants: Vec<Grant>) -> (V2Peer, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (fenced, _) = watch::channel(false);
        let key = if id.contains("plugin") {
            plugin_key()
        } else {
            mobile_key()
        };
        (
            V2Peer::new(id.into(), nonce.into(), key, grants, tx, fenced),
            rx,
        )
    }

    fn app_with_peers() -> (V2App, mpsc::Receiver<Message>, mpsc::Receiver<Message>) {
        let (plugin, plugin_rx) = peer("plugin_conn", "plugin_nonce", vec![]);
        let (mobile, mobile_rx) = peer(
            "mobile_conn",
            "mobile_nonce",
            vec![
                Grant::StateRead,
                Grant::CallerRead,
                Grant::CaptionsRead,
                Grant::AssistanceRead,
                Grant::AssistanceRespond,
                Grant::Monitor,
                Grant::Consult,
                Grant::Takeover,
                Grant::EndCaller,
                Grant::RtcSignal,
            ],
        );
        let mut app = V2App::default();
        app.plugin = Some(plugin);
        app.plugin_id = Some("plugin_a".into());
        app.mobiles.insert("device_a".into(), mobile);
        app.snapshot = Some(snapshot(3));
        app.sequence = 1;
        (app, plugin_rx, mobile_rx)
    }

    fn admission(device_id: &str) -> Admission {
        Admission {
            role: AdmissionRole::Mobile,
            app_id: "app_a".into(),
            subject_id: device_id.into(),
            grants: vec![],
            scopes: vec![
                Grant::StateRead,
                Grant::AssistanceRead,
                Grant::AssistanceRespond,
                Grant::Monitor,
                Grant::Consult,
                Grant::Takeover,
                Grant::EndCaller,
                Grant::RtcSignal,
            ],
        }
    }

    fn plugin_admission() -> Admission {
        Admission {
            role: AdmissionRole::Plugin,
            app_id: "app_a".into(),
            subject_id: "plugin_a".into(),
            grants: vec![],
            scopes: vec![],
        }
    }

    fn gateway() -> Gateway {
        Gateway::new(GatewayConfig::for_tests(vec![AdmissionSpec {
            token: "static_mobile_token_123456789".into(),
            role: AdmissionRole::Mobile,
            app_id: "app_a".into(),
            subject_id: "device_a".into(),
            grants: vec![],
            scopes: admission("device_a").scopes,
        }]))
        .unwrap()
    }

    async fn active_end_caller_gateway() -> (
        Gateway,
        String,
        mpsc::Receiver<Message>,
        mpsc::Receiver<Message>,
    ) {
        let gateway = gateway();
        let (mut app, plugin_rx, mobile_rx) = app_with_peers();
        let now = unix_now().unwrap();
        let claims = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Takeover,
            LeasePhase::Active,
            9,
            "mobile_nonce",
            "rtc_end",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            now,
        );
        let token = gateway.inner.v2.signer().unwrap().sign(&claims).unwrap();
        app.snapshot.as_mut().unwrap().service_mode = ServiceMode::HumanActive;
        app.snapshot.as_mut().unwrap().media_state = MediaState::Active;
        app.claim = Some(TalkClaim {
            request_id: "takeover_request".into(),
            device_id: "device_a".into(),
            mode: LeaseMode::Takeover,
            call_id: "call_a".into(),
            call_epoch: 7,
            fence: 9,
            lease_jti: claims.jti.clone(),
            active: true,
        });
        app.leases.insert(
            claims.jti.clone(),
            LeaseRecord {
                claims,
                provisional: false,
                mobile_signal: SignalProgress::default(),
                plugin_signal: SignalProgress::default(),
            },
        );
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .insert("app_a".into(), app);
        (gateway, token, plugin_rx, mobile_rx)
    }

    fn end_challenge_request(token: &str, suffix: &str) -> MobileEndCallerChallengeRequestFrame {
        MobileEndCallerChallengeRequestFrame {
            kind: "end_caller_challenge_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: format!("end_prepare_{suffix}"),
            idempotency_key: format!("end-prepare-{suffix}"),
            lease_token: token.into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 3,
            switchboard_revision: 11,
            remote_revision: 13,
            fence: 9,
        }
    }

    fn end_confirm(
        token: &str,
        challenge: &EndCallerChallengeFrame,
        suffix: &str,
    ) -> MobileEndCallerConfirmFrame {
        MobileEndCallerConfirmFrame {
            kind: "end_caller_confirm".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: format!("end_confirm_{suffix}"),
            idempotency_key: format!("end-confirm-{suffix}"),
            confirmation_id: challenge.confirmation_id.clone(),
            nonce: challenge.nonce.clone(),
            lease_token: token.into(),
            call_id: challenge.call_id.clone(),
            call_epoch: challenge.call_epoch,
            owner_epoch: challenge.owner_epoch,
            switchboard_revision: challenge.switchboard_revision,
            remote_revision: challenge.remote_revision,
            fence: challenge.fence,
        }
    }

    async fn text_json(rx: &mut mpsc::Receiver<Message>) -> Value {
        let Message::Text(encoded) = rx.recv().await.expect("expected text frame") else {
            panic!("expected text frame")
        };
        serde_json::from_str(&encoded).unwrap()
    }

    fn assert_no_end_execute(rx: &mut mpsc::Receiver<Message>) {
        while let Ok(message) = rx.try_recv() {
            if let Message::Text(encoded) = message {
                let value: Value = serde_json::from_str(&encoded).unwrap();
                assert_ne!(value["kind"], "end_caller_execute");
            }
        }
    }

    #[tokio::test]
    async fn authenticated_idle_clears_call_authority_and_retains_replay_fences() {
        let gateway = gateway();
        let (mut app, mut plugin_rx, mut mobile_rx) = app_with_peers();
        let now = unix_now().unwrap();
        let lease = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Takeover,
            LeasePhase::Active,
            17,
            "mobile_nonce",
            "rtc_before_idle",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            now,
        );
        let lease_jti = lease.jti.clone();
        let lease_id = lease.lease_id.clone();
        app.claim = Some(TalkClaim {
            request_id: "claim_before_idle".into(),
            device_id: "device_a".into(),
            mode: LeaseMode::Takeover,
            call_id: "call_a".into(),
            call_epoch: 7,
            fence: 17,
            lease_jti: lease_jti.clone(),
            active: true,
        });
        app.leases.insert(
            lease_jti.clone(),
            LeaseRecord {
                claims: lease,
                provisional: false,
                mobile_signal: SignalProgress {
                    sdp_revision: 4,
                    transport_generation: 2,
                },
                plugin_signal: SignalProgress {
                    sdp_revision: 4,
                    transport_generation: 2,
                },
            },
        );
        app.assistance = Some(AssistanceRecord {
            request: PluginAssistanceRequestFrame {
                kind: "assistance_request".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "assistance_event_before_idle".into(),
                request_id: "assistance_before_idle".into(),
                call_id: "call_a".into(),
                call_epoch: 7,
                owner_epoch: 3,
                switchboard_revision: 11,
                remote_revision: 13,
                question: "Can you help?".into(),
                context: None,
                expires_at: now + 60,
            },
            fingerprint: [5; 32],
            answered: false,
        });
        app.end_caller_challenges.insert(
            "confirmation_before_idle".into(),
            EndCallerChallengeRecord {
                frame: EndCallerChallengeFrame {
                    kind: "end_caller_challenge".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    request_id: "end_request_before_idle".into(),
                    confirmation_id: "confirmation_before_idle".into(),
                    nonce: "confirmation_nonce_before_idle".into(),
                    device_id: "device_a".into(),
                    call_id: "call_a".into(),
                    call_epoch: 7,
                    owner_epoch: 3,
                    switchboard_revision: 11,
                    remote_revision: 13,
                    lease_id: lease_id.clone(),
                    fence: 17,
                    expires_at: now + 10,
                },
                lease_jti: lease_jti.clone(),
            },
        );
        app.end_caller_operation = Some(EndCallerOperation {
            execute: PluginEndCallerExecuteFrame {
                kind: "end_caller_execute".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                operation_id: "end_operation_before_idle".into(),
                confirmation_id: "confirmation_before_idle".into(),
                device_id: "device_a".into(),
                call_id: "call_a".into(),
                call_epoch: 7,
                owner_epoch: 3,
                switchboard_revision: 11,
                remote_revision: 13,
                lease_id,
                lease_jti: lease_jti.clone(),
                fence: 17,
            },
            idempotency_key: "end-confirm-before-idle".into(),
            fingerprint: [6; 32],
        });
        refresh_pending_mobile_offers(&mut app, "app_a", gateway.inner.v2.signer().unwrap(), now)
            .unwrap();
        let accepted = app
            .pending_mobile_offers
            .values()
            .next()
            .expect("a live call creates an offer")
            .offer
            .clone();
        app.accepted_mobile_offers
            .insert(accepted.jti.clone(), accepted.clone());
        app.mobile_offer_winners
            .insert(accepted.opportunity_id, accepted.jti);
        app.cache(
            "device_a",
            "old-idempotency-key",
            [9; 32],
            "cached-result".into(),
        );
        assert!(app.remember_signal("mobile:device_a", "signal_before_idle"));
        app.remember_used_end_caller_confirmation("confirmation_used_before_idle".into());
        app.next_fence = 41;
        app.approved_mobile_key_thumbprints
            .insert(mobile_key().thumbprint);
        app.peer_roster_revision = Some(9);
        app.peer_roster_hash = Some("owner_approved_roster_hash".into());
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .insert("app_a".into(), app);

        handle_idle(
            &gateway,
            &plugin_admission(),
            "plugin_conn",
            PluginIdleFrame {
                kind: "plugin_idle".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "idle_after_call_a".into(),
            },
        )
        .await
        .unwrap();

        // Revoking an in-flight caller-ending operation emits its terminal
        // failure first.  The idle synchronization follows only after every
        // gateway authority slot has been cleared under the app mutex.
        let end_caller_result = text_json(&mut mobile_rx).await;
        assert_eq!(end_caller_result["kind"], "end_caller_result");
        assert_eq!(end_caller_result["outcome"], "failed");
        let idle_json = text_json(&mut mobile_rx).await;
        let idle: MobileIdleSyncFrame = serde_json::from_value(idle_json).unwrap();
        idle.validate().unwrap();
        assert_eq!(idle.sequence, 2);
        assert!(idle.grants.contains(&Grant::StateRead));

        {
            let apps = gateway.inner.v2.apps.lock().await;
            let app = &apps["app_a"];
            assert!(app.snapshot.is_none());
            assert!(app.claim.is_none());
            assert!(app.leases.is_empty());
            assert!(app.assistance.is_none());
            assert!(app.end_caller_challenges.is_empty());
            assert!(app.end_caller_operation.is_none());
            assert!(app.pending_mobile_offers.is_empty());
            assert!(app.accepted_mobile_offers.is_empty());
            assert!(app.mobile_offer_winners.is_empty());
            assert!(app.plugin.is_some());
            assert!(app.mobiles.contains_key("device_a"));
            assert_eq!(app.plugin_id.as_deref(), Some("plugin_a"));
            assert!(app
                .approved_mobile_key_thumbprints
                .contains(&mobile_key().thumbprint));
            assert_eq!(app.peer_roster_revision, Some(9));
            assert_eq!(
                app.peer_roster_hash.as_deref(),
                Some("owner_approved_roster_hash")
            );
            assert_eq!(app.next_fence, 41);
            let Cached::Replay(stale_replay) =
                app.cached("device_a", "old-idempotency-key", [9; 32])
            else {
                panic!("the idempotency high-water must survive idle");
            };
            let stale_replay: Value = serde_json::from_str(&stale_replay).unwrap();
            assert_eq!(stale_replay["code"], "stale_authority");
            assert!(app
                .signals
                .contains(&("mobile:device_a".into(), "signal_before_idle".into())));
            assert!(app
                .used_end_caller_confirmations
                .contains("confirmation_used_before_idle"));
            let tombstone = app
                .idle
                .as_ref()
                .and_then(|idle| idle.prior_fence.as_ref())
                .expect("idle must retain the prior call fence");
            assert_eq!(tombstone.call_id, "call_a");
            assert_eq!(tombstone.call_epoch, 7);
            assert_eq!(tombstone.owner_epoch, 3);
            assert_eq!(tombstone.switchboard_revision, 11);
            assert_eq!(tombstone.remote_revision, 13);
        }
        let lease_revoked = text_json(&mut plugin_rx).await;
        assert_eq!(lease_revoked["kind"], "lease_revoked");
        assert_eq!(lease_revoked["leaseJti"], lease_jti);

        // Duplicate assertions are idempotent and cannot advance authority.
        handle_idle(
            &gateway,
            &plugin_admission(),
            "plugin_conn",
            PluginIdleFrame {
                kind: "plugin_idle".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "idle_duplicate".into(),
            },
        )
        .await
        .unwrap();
        assert!(mobile_rx.try_recv().is_err());
        assert_eq!(gateway.inner.v2.apps.lock().await["app_a"].sequence, 2);

        for call_epoch in [7, 6] {
            let mut stale = snapshot(3);
            stale.call_epoch = call_epoch;
            let error = handle_snapshot(
                &gateway,
                &plugin_admission(),
                "plugin_conn",
                PluginSnapshotFrame {
                    kind: "plugin_snapshot".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    event_id: format!("stale_{call_epoch}"),
                    snapshot: stale,
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, "stale_snapshot");
        }
        assert!(gateway.inner.v2.apps.lock().await["app_a"]
            .snapshot
            .is_none());

        let mut next_call = snapshot(0);
        next_call.call_id = "call_b".into();
        next_call.call_epoch = 8;
        next_call.switchboard_revision = 0;
        next_call.remote_revision = 0;
        handle_snapshot(
            &gateway,
            &plugin_admission(),
            "plugin_conn",
            PluginSnapshotFrame {
                kind: "plugin_snapshot".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "snapshot_call_b".into(),
                snapshot: next_call,
            },
        )
        .await
        .unwrap();
        let resumed = text_json(&mut mobile_rx).await;
        assert_eq!(resumed["kind"], "snapshot");
        assert_eq!(resumed["sequence"], 3);
        let apps = gateway.inner.v2.apps.lock().await;
        assert!(apps["app_a"].idle.is_none());
        assert_eq!(apps["app_a"].snapshot.as_ref().unwrap().call_id, "call_b");
    }

    #[tokio::test]
    async fn sequence_exhaustion_cannot_apply_an_unpublishable_state_change() {
        let gateway = gateway();
        let (mut app, mut plugin_rx, mut mobile_rx) = app_with_peers();
        app.sequence = MAX_SAFE_INTEGER;
        let now = unix_now().unwrap();
        let lease = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Takeover,
            LeasePhase::Active,
            23,
            "mobile_nonce",
            "rtc_at_sequence_ceiling",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            now,
        );
        let lease_jti = lease.jti.clone();
        app.claim = Some(TalkClaim {
            request_id: "claim_at_sequence_ceiling".into(),
            device_id: "device_a".into(),
            mode: LeaseMode::Takeover,
            call_id: "call_a".into(),
            call_epoch: 7,
            fence: 23,
            lease_jti: lease_jti.clone(),
            active: true,
        });
        app.leases.insert(
            lease_jti.clone(),
            LeaseRecord {
                claims: lease,
                provisional: false,
                mobile_signal: SignalProgress::default(),
                plugin_signal: SignalProgress::default(),
            },
        );
        refresh_pending_mobile_offers(&mut app, "app_a", gateway.inner.v2.signer().unwrap(), now)
            .unwrap();
        let offer = app
            .pending_mobile_offers
            .values()
            .find(|signed| signed.offer.offered_mode == LeaseMode::Takeover)
            .cloned()
            .expect("takeover offer");
        let offer_jti = offer.offer.jti.clone();
        let opportunity_id = offer.offer.opportunity_id.clone();
        let answer = MobileOfferAnswerFrame {
            kind: "mobile_offer_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: "answer_at_sequence_ceiling".into(),
            idempotency_key: "answer-at-sequence-ceiling".into(),
            offer_id: offer.offer.offer_id.clone(),
            offer_jti: offer.offer.jti.clone(),
            offer_token: offer.offer_token,
            target_device_id: "device_a".into(),
            target_holder_key_thumbprint: offer.offer.target_holder_key_thumbprint,
            offered_mode: offer.offer.offered_mode,
            call_id: offer.offer.call_id,
            call_epoch: offer.offer.call_epoch,
            owner_epoch: offer.offer.owner_epoch,
        };
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .insert("app_a".into(), app);

        let idle_error = handle_idle(
            &gateway,
            &plugin_admission(),
            "plugin_conn",
            PluginIdleFrame {
                kind: "plugin_idle".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "idle_at_sequence_ceiling".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(idle_error.code, "sequence_exhausted");

        let snapshot_error = handle_snapshot(
            &gateway,
            &plugin_admission(),
            "plugin_conn",
            PluginSnapshotFrame {
                kind: "plugin_snapshot".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "snapshot_at_sequence_ceiling".into(),
                snapshot: snapshot(4),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(snapshot_error.code, "sequence_exhausted");

        let offer_error =
            handle_mobile_offer_answer(&gateway, &admission("device_a"), "mobile_conn", answer)
                .await
                .unwrap_err();
        assert_eq!(offer_error.code, "sequence_exhausted");

        let apps = gateway.inner.v2.apps.lock().await;
        let app = &apps["app_a"];
        assert_eq!(app.sequence, MAX_SAFE_INTEGER);
        assert!(app.idle.is_none());
        assert_eq!(app.snapshot.as_ref().unwrap().owner_epoch, 3);
        assert!(app.claim.is_some());
        assert!(app.leases.contains_key(&lease_jti));
        assert!(app.pending_mobile_offers.contains_key(&offer_jti));
        assert!(!app.mobile_offer_winners.contains_key(&opportunity_id));
        assert!(app.accepted_mobile_offers.is_empty());
        assert!(!app
            .idempotency
            .contains_key(&("device_a".into(), "answer-at-sequence-ceiling".into())));
        assert!(mobile_rx.try_recv().is_err());
        assert!(plugin_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn mobile_registration_requires_and_receives_typed_idle_authority() {
        let gateway = gateway();
        let (plugin, _plugin_rx) = peer("plugin_conn", "plugin_nonce", vec![]);
        let endpoint_key = mobile_key();
        let mut app = V2App::default();
        app.plugin = Some(plugin);
        app.plugin_id = Some("plugin_a".into());
        app.approved_mobile_key_thumbprints
            .insert(endpoint_key.thumbprint.clone());
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .insert("app_a".into(), app);

        let proof = SignedHelloProof {
            claims: aokie_protocol::v2::HelloProofClaims {
                app_id: "app_a".into(),
                subject_id: "device_a".into(),
                role: TokenRole::Mobile,
                connection_id: "mobile_conn".into(),
                challenge_nonce: "challenge_device_a".into(),
                admission_jti: "admission_device_a".into(),
                session_nonce: "session_device_a".into(),
                holder_key_thumbprint: endpoint_key.thumbprint.clone(),
                expected_peer_key_thumbprint: Some(plugin_key().thumbprint.clone()),
                approved_peer_key_thumbprints: vec![],
                peer_roster_revision: None,
                peer_roster_hash: None,
                nonce: "nonce_device_a".into(),
                jti: "proof_device_a".into(),
                issued_at: 1,
                expires_at: 2,
            },
            endpoint_key: endpoint_key.clone(),
            signature: "A".repeat(86),
        };
        let hello = MobileHello {
            kind: "mobile_hello".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            session_nonce: "session_device_a".into(),
            endpoint_proof: proof,
        };
        let challenge = EndpointChallengeFrame {
            kind: "endpoint_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            subject_id: "device_a".into(),
            role: TokenRole::Mobile,
            connection_id: "mobile_conn".into(),
            challenge_nonce: "challenge_device_a".into(),
            admission_jti: "admission_device_a".into(),
            holder_key_thumbprint: endpoint_key.thumbprint,
            expected_peer_key_thumbprint: Some(plugin_key().thumbprint),
            approved_peer_key_thumbprints: vec![],
            peer_roster_revision: None,
            peer_roster_hash: None,
            expires_at: unix_now().unwrap() + 20,
        };
        let (tx, mut rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (fenced, _) = watch::channel(false);

        let unavailable = register_mobile(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            hello.clone(),
            &challenge,
            tx.clone(),
            fenced.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(unavailable.code, "endpoint_unavailable");

        {
            let mut apps = gateway.inner.v2.apps.lock().await;
            let app = apps.get_mut("app_a").unwrap();
            app.sequence = 1;
            app.idle = Some(IdleAuthority {
                assertion: PluginIdleFrame {
                    kind: "plugin_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    event_id: "initial_idle".into(),
                },
                prior_fence: None,
            });
        }
        register_mobile(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            hello,
            &challenge,
            tx,
            fenced,
        )
        .await
        .unwrap();
        let idle: MobileIdleSyncFrame = serde_json::from_value(text_json(&mut rx).await).unwrap();
        idle.validate().unwrap();
        assert_eq!(idle.app_id, "app_a");
        assert_eq!(idle.sequence, 1);
        assert!(idle.grants.contains(&Grant::StateRead));
        assert_eq!(gateway.inner.v2.stats().await.mobiles, 1);
    }

    #[test]
    fn hmac_tokens_reject_tampering_expiry_and_wrong_tracks() {
        let signer = LeaseSigner::new(b"0123456789abcdef0123456789abcdef".to_vec());
        let claims = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Takeover,
            LeasePhase::Prepared,
            1,
            "mobile_nonce",
            "rtc_a",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            100,
        );
        let token = signer.sign(&claims).unwrap();
        assert_eq!(signer.verify(&token, 101).unwrap(), claims);
        let mut tampered = token.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
        assert!(signer
            .verify(std::str::from_utf8(&tampered).unwrap(), 101)
            .is_err());
        assert!(signer
            .sign(&claims)
            .and_then(|token| signer.verify(&token, 121))
            .is_err());

        let mut unsafe_claims = claims;
        unsafe_claims.tracks = tracks_for(LeaseMode::Takeover, LeasePhase::Active);
        let unsafe_token = signer.sign(&unsafe_claims).unwrap();
        assert!(signer.verify(&unsafe_token, 101).is_err());
    }

    #[test]
    fn signed_mobile_offer_is_targeted_and_expires_fail_closed() {
        let gateway = gateway();
        let (mut app, _, _) = app_with_peers();
        refresh_pending_mobile_offers(&mut app, "app_a", gateway.inner.v2.signer().unwrap(), 100)
            .unwrap();
        let signed = app
            .pending_mobile_offers
            .values()
            .next()
            .expect("offer generated");
        assert_eq!(
            gateway
                .inner
                .v2
                .signer()
                .unwrap()
                .verify_mobile_offer(&signed.offer_token, 101)
                .unwrap(),
            signed.offer
        );
        assert!(gateway
            .inner
            .v2
            .signer()
            .unwrap()
            .verify_mobile_offer(&signed.offer_token, signed.offer.expires_at)
            .is_err());
    }

    #[test]
    fn signed_offer_surfaces_cover_in_app_and_native_without_duplicate_authority() {
        let gateway = gateway();
        let signer = gateway.inner.v2.signer().unwrap();
        let (mut app, _, _) = app_with_peers();
        refresh_pending_mobile_offers(&mut app, "app_a", signer, 100).unwrap();

        let offers_for = |app: &V2App, mode: LeaseMode| {
            app.pending_mobile_offers
                .values()
                .filter(|signed| {
                    signed.offer.target_device_id == "device_a" && signed.offer.offered_mode == mode
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        let monitor = offers_for(&app, LeaseMode::Monitor);
        let consult = offers_for(&app, LeaseMode::Consult);
        let takeover = offers_for(&app, LeaseMode::Takeover);
        assert_eq!(monitor.len(), 1);
        assert_eq!(monitor[0].offer.surface, MobileOfferSurface::InApp);
        for offers in [&consult, &takeover] {
            assert_eq!(offers.len(), 2);
            assert!(offers
                .iter()
                .any(|signed| signed.offer.surface == MobileOfferSurface::InApp));
            assert!(offers
                .iter()
                .any(|signed| signed.offer.surface == MobileOfferSurface::VoiceSystemUi));
            assert_eq!(
                offers[0].offer.opportunity_id,
                offers[1].offer.opportunity_id
            );
            assert_ne!(offers[0].offer.offer_id, offers[1].offer.offer_id);
            assert_ne!(offers[0].offer.jti, offers[1].offer.jti);
        }
        assert_eq!(app.pending_mobile_offers.len(), 5);

        let retained_jtis = app
            .pending_mobile_offers
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        refresh_pending_mobile_offers(&mut app, "app_a", signer, 101).unwrap();
        assert_eq!(app.pending_mobile_offers.len(), 5);
        assert_eq!(
            retained_jtis,
            app.pending_mobile_offers
                .keys()
                .cloned()
                .collect::<HashSet<_>>()
        );

        let snapshot = app.snapshot.as_mut().expect("active snapshot");
        snapshot.switchboard_revision += 1;
        snapshot.remote_revision += 1;
        snapshot.remote_consent.policy_version += 1;
        refresh_pending_mobile_offers(&mut app, "app_a", signer, 102).unwrap();
        let revised_jtis = app
            .pending_mobile_offers
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        assert_eq!(revised_jtis.len(), 5);
        assert!(retained_jtis.is_disjoint(&revised_jtis));
        let revised_snapshot = app.snapshot.as_ref().expect("active snapshot");
        assert!(app.pending_mobile_offers.values().all(|signed| {
            signed.offer.switchboard_revision == revised_snapshot.switchboard_revision
                && signed.offer.remote_revision == revised_snapshot.remote_revision
                && signed.offer.required_consent_policy_version
                    == revised_snapshot.remote_consent.policy_version
        }));

        let rotated_key = test_endpoint_key(8);
        app.mobiles
            .get_mut("device_a")
            .expect("mobile peer")
            .endpoint_key = rotated_key.clone();
        refresh_pending_mobile_offers(&mut app, "app_a", signer, 103).unwrap();
        assert_eq!(app.pending_mobile_offers.len(), 5);
        assert!(app
            .pending_mobile_offers
            .values()
            .all(|signed| { signed.offer.target_holder_key_thumbprint == rotated_key.thumbprint }));
    }

    #[test]
    fn admission_tokens_reject_tampering_expiry_and_wrong_audience() {
        let signer =
            AdmissionTokenSigner::new(b"0123456789abcdef0123456789abcdef".to_vec()).unwrap();
        let claims = AdmissionClaims {
            aud: ADMISSION_AUDIENCE.into(),
            app_id: "app_a".into(),
            subject_id: "device_a".into(),
            role: TokenRole::Mobile,
            holder_key_thumbprint: mobile_key().thumbprint,
            expected_peer_key_thumbprint: Some(plugin_key().thumbprint),
            approved_peer_key_thumbprints: vec![],
            peer_roster_revision: None,
            peer_roster_hash: None,
            scopes: vec![Grant::StateRead, Grant::Monitor],
            exp: 200,
            jti: "admission_a".into(),
        };
        let token = signer.issue(&claims, 100).unwrap();
        assert_eq!(signer.verify(&token, 101).unwrap(), claims);
        assert!(signer.verify(&token, 200).is_err());

        let mut tampered = token.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
        assert!(signer
            .verify(std::str::from_utf8(&tampered).unwrap(), 101)
            .is_err());

        let mut wrong_audience = claims;
        wrong_audience.aud = "another-service".into();
        assert!(signer.issue(&wrong_audience, 100).is_err());
    }

    #[tokio::test]
    async fn admission_jti_is_single_use_until_its_expiry() {
        let gateway = V2Gateway::new(
            Some(b"0123456789abcdef0123456789abcdef".to_vec()),
            Some(b"abcdef0123456789abcdef0123456789".to_vec()),
            false,
        );
        gateway
            .claim_admission(
                Some("admission_a"),
                Some(unix_now().unwrap() + 30),
                "conn_1",
            )
            .await
            .unwrap();
        let replay = gateway
            .claim_admission(
                Some("admission_a"),
                Some(unix_now().unwrap() + 30),
                "conn_2",
            )
            .await
            .unwrap_err();
        assert_eq!(replay.code, "admission_replay");
        gateway
            .release_admission(Some("admission_a"), "conn_1")
            .await;
        let disconnected_replay = gateway
            .claim_admission(
                Some("admission_a"),
                Some(unix_now().unwrap() + 30),
                "conn_2",
            )
            .await
            .unwrap_err();
        assert_eq!(disconnected_replay.code, "admission_replay");

        gateway
            .claimed_admissions
            .lock()
            .await
            .get_mut("admission_a")
            .unwrap()
            .exp = 0;
        gateway
            .claim_admission(
                Some("admission_a"),
                Some(unix_now().unwrap() + 30),
                "conn_3",
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn endpoint_proof_jti_is_one_use_until_expiry() {
        let gateway = V2Gateway::new(
            Some(b"0123456789abcdef0123456789abcdef".to_vec()),
            Some(b"abcdef0123456789abcdef0123456789".to_vec()),
            false,
        );
        let expires_at = unix_now().unwrap() + 30;
        gateway
            .claim_endpoint_jti("endpoint_jti_a", expires_at)
            .await
            .unwrap();
        let replay = gateway
            .claim_endpoint_jti("endpoint_jti_a", expires_at)
            .await
            .unwrap_err();
        assert_eq!(replay.code, "endpoint_replay");
    }

    #[tokio::test]
    async fn plugin_roster_admits_two_distinct_mobile_keys_and_rejects_an_unapproved_third() {
        let gateway = gateway();
        let (plugin, _plugin_rx) = peer("plugin_conn", "plugin_nonce", vec![]);
        let first_key = test_endpoint_key(21);
        let second_key = test_endpoint_key(22);
        let third_key = test_endpoint_key(23);
        let mut app = V2App::default();
        app.plugin = Some(plugin);
        app.plugin_id = Some("plugin_a".into());
        app.snapshot = Some(snapshot(3));
        app.sequence = 1;
        app.approved_mobile_key_thumbprints =
            [first_key.thumbprint.clone(), second_key.thumbprint.clone()]
                .into_iter()
                .collect();
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .insert("app_a".into(), app);

        let register = |device_id: &str, connection_id: &str, key: EndpointPublicKey| {
            let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
            let (fenced, _) = watch::channel(false);
            let proof = SignedHelloProof {
                claims: aokie_protocol::v2::HelloProofClaims {
                    app_id: "app_a".into(),
                    subject_id: device_id.into(),
                    role: TokenRole::Mobile,
                    connection_id: connection_id.into(),
                    challenge_nonce: format!("challenge_{device_id}"),
                    admission_jti: format!("admission_{device_id}"),
                    session_nonce: format!("session_{device_id}"),
                    holder_key_thumbprint: key.thumbprint.clone(),
                    expected_peer_key_thumbprint: Some(plugin_key().thumbprint),
                    approved_peer_key_thumbprints: vec![],
                    peer_roster_revision: None,
                    peer_roster_hash: None,
                    nonce: format!("nonce_{device_id}"),
                    jti: format!("proof_{device_id}"),
                    issued_at: 1,
                    expires_at: 2,
                },
                endpoint_key: key.clone(),
                signature: "A".repeat(86),
            };
            let hello = MobileHello {
                kind: "mobile_hello".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                device_id: device_id.into(),
                session_nonce: format!("session_{device_id}"),
                endpoint_proof: proof,
            };
            let challenge = EndpointChallengeFrame {
                kind: "endpoint_challenge".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                subject_id: device_id.into(),
                role: TokenRole::Mobile,
                connection_id: connection_id.into(),
                challenge_nonce: format!("challenge_{device_id}"),
                admission_jti: format!("admission_{device_id}"),
                holder_key_thumbprint: key.thumbprint,
                expected_peer_key_thumbprint: Some(plugin_key().thumbprint),
                approved_peer_key_thumbprints: vec![],
                peer_roster_revision: None,
                peer_roster_hash: None,
                expires_at: unix_now().unwrap() + 20,
            };
            (hello, challenge, tx, fenced, rx)
        };

        let (hello, challenge, tx, fenced, _first_rx) = register("device_a", "conn_a", first_key);
        register_mobile(
            &gateway,
            &admission("device_a"),
            "conn_a",
            hello,
            &challenge,
            tx,
            fenced,
        )
        .await
        .unwrap();
        let (hello, challenge, tx, fenced, _second_rx) = register("device_b", "conn_b", second_key);
        register_mobile(
            &gateway,
            &admission("device_b"),
            "conn_b",
            hello,
            &challenge,
            tx,
            fenced,
        )
        .await
        .unwrap();
        let (hello, challenge, tx, fenced, _third_rx) = register("device_c", "conn_c", third_key);
        let rejected = register_mobile(
            &gateway,
            &admission("device_c"),
            "conn_c",
            hello,
            &challenge,
            tx,
            fenced,
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.code, "unapproved_endpoint");
        let cross_device_offer = gateway.inner.v2.apps.lock().await["app_a"]
            .pending_mobile_offers
            .values()
            .find(|signed| {
                signed.offer.target_device_id == "device_a"
                    && signed.offer.offered_mode == LeaseMode::Takeover
            })
            .cloned()
            .expect("device A has a takeover offer");
        let cross_device = MobileOfferAnswerFrame {
            kind: "mobile_offer_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: "cross_device_answer".into(),
            idempotency_key: "cross-device-answer-key".into(),
            offer_id: cross_device_offer.offer.offer_id.clone(),
            offer_jti: cross_device_offer.offer.jti.clone(),
            offer_token: cross_device_offer.offer_token,
            target_device_id: cross_device_offer.offer.target_device_id.clone(),
            target_holder_key_thumbprint: cross_device_offer.offer.target_holder_key_thumbprint,
            offered_mode: cross_device_offer.offer.offered_mode,
            call_id: cross_device_offer.offer.call_id,
            call_epoch: cross_device_offer.offer.call_epoch,
            owner_epoch: cross_device_offer.offer.owner_epoch,
        };
        let cross_device_rejection =
            handle_mobile_offer_answer(&gateway, &admission("device_b"), "conn_b", cross_device)
                .await
                .unwrap_err();
        assert_eq!(cross_device_rejection.code, "offer_substitution");
        assert_eq!(gateway.inner.v2.apps.lock().await["app_a"].mobiles.len(), 2);
    }

    #[test]
    fn dynamic_admission_deadline_is_bounded_and_static_admission_has_none() {
        assert_eq!(
            admission_expiry_delay(Some(130), 100),
            Some(Duration::from_secs(30))
        );
        assert_eq!(admission_expiry_delay(Some(100), 100), Some(Duration::ZERO));
        assert_eq!(admission_expiry_delay(Some(99), 100), Some(Duration::ZERO));
        assert_eq!(admission_expiry_delay(None, 100), None);
    }

    #[tokio::test]
    async fn dynamic_admission_expiry_fires_while_static_admission_remains_open() {
        let now = unix_now().unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            wait_for_admission_expiry(Some(now)),
        )
        .await
        .is_ok());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), wait_for_admission_expiry(None))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn renewal_rotates_jti_but_preserves_native_peer_identity_and_rejects_old_token() {
        let gateway = gateway();
        let (mut app, mut plugin_rx, mut mobile_rx) = app_with_peers();
        let now = unix_now().unwrap();
        let claims = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Takeover,
            LeasePhase::Active,
            9,
            "mobile_nonce",
            "rtc_a",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            now,
        );
        let old_token = gateway.inner.v2.signer().unwrap().sign(&claims).unwrap();
        let old_jti = claims.jti.clone();
        let stable_lease_id = claims.lease_id.clone();
        app.leases.insert(
            claims.jti.clone(),
            LeaseRecord {
                claims,
                provisional: false,
                mobile_signal: SignalProgress::default(),
                plugin_signal: SignalProgress::default(),
            },
        );
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .insert("app_a".into(), app);

        handle_lease_heartbeat(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            LeaseHeartbeatFrame {
                kind: "lease_heartbeat".into(),
                schema_version: 2,
                app_id: "app_a".into(),
                request_id: "heartbeat_a".into(),
                idempotency_key: "heartbeat-key-a".into(),
                lease_token: old_token.clone(),
            },
        )
        .await
        .unwrap();
        let Message::Text(response) = mobile_rx.recv().await.unwrap() else {
            panic!("expected renewal")
        };
        let response: Value = serde_json::from_str(&response).unwrap();
        let renewed: LeaseClaims = serde_json::from_value(response["lease"].clone()).unwrap();
        assert_eq!(renewed.lease_id, stable_lease_id);
        assert_eq!(renewed.rtc_session_id, "rtc_a");
        assert_eq!(renewed.owner_epoch, 3);
        assert_eq!(renewed.fence, 9);
        assert_ne!(renewed.jti, old_jti);
        let Message::Text(_) = plugin_rx.recv().await.unwrap() else {
            panic!("expected plugin renewal notice")
        };

        let rejected = handle_lease_heartbeat(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            LeaseHeartbeatFrame {
                kind: "lease_heartbeat".into(),
                schema_version: 2,
                app_id: "app_a".into(),
                request_id: "heartbeat_old".into(),
                idempotency_key: "heartbeat-key-old".into(),
                lease_token: old_token,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.code, "invalid_lease");
    }

    #[test]
    fn projection_removes_caller_and_captions_without_scopes() {
        let (mut app, _, _) = app_with_peers();
        let (restricted, _) = peer("restricted", "nonce", vec![Grant::StateRead]);
        let encoded = projected_snapshot(&app, "app_a", &restricted).unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert!(value["snapshot"].get("caller").is_none());
        assert!(value["snapshot"].get("captions").is_none());

        let full = app.mobiles.remove("device_a").unwrap();
        let encoded = projected_snapshot(&app, "app_a", &full).unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["snapshot"]["caller"]["maskedNumber"], "***123");
        assert_eq!(value["snapshot"]["captions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn first_winner_fence_is_positive_and_monotonic() {
        let mut app = V2App::default();
        assert_eq!(app.next_talk_fence().unwrap(), 1);
        assert_eq!(app.next_talk_fence().unwrap(), 2);
        app.next_fence = MAX_SAFE_INTEGER;
        assert!(app.next_talk_fence().is_err());
    }

    #[test]
    fn consult_is_fence_zero_and_takeover_prepared_cannot_transmit_to_pstn() {
        let now = unix_now().unwrap();
        let consult = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Consult,
            LeasePhase::Prepared,
            0,
            "mobile_nonce",
            "rtc_consult",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            now,
        );
        consult.validate(now).unwrap();
        assert_eq!(consult.fence, 0);
        assert!(!consult
            .tracks
            .contains(&aokie_protocol::v2::MediaTrack::PstnOut));

        let takeover = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Takeover,
            LeasePhase::Prepared,
            1,
            "mobile_nonce",
            "rtc_takeover",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            now,
        );
        takeover.validate(now).unwrap();
        assert_eq!(takeover.phase, LeasePhase::Prepared);
        assert!(!takeover
            .tracks
            .contains(&aokie_protocol::v2::MediaTrack::PstnOut));
    }

    #[test]
    fn rtc_revisions_and_transport_generations_are_strictly_monotonic() {
        let mut progress = SignalProgress::default();
        advance_signal(
            &mut progress,
            &RtcSignal::Offer {
                sdp: "v=0".into(),
                binding: dummy_binding(TokenRole::Mobile, 1, 1),
            },
            1,
            1,
        )
        .unwrap();
        advance_signal(
            &mut progress,
            &RtcSignal::Ice {
                candidate: "candidate:1".into(),
                sdp_mid: Some("0".into()),
                sdp_m_line_index: Some(0),
                envelope: dummy_candidate(Some("candidate:1"), 1, 1),
            },
            1,
            1,
        )
        .unwrap();
        advance_signal(
            &mut progress,
            &RtcSignal::IceComplete {
                envelope: dummy_candidate(None, 1, 1),
            },
            1,
            1,
        )
        .unwrap();
        assert!(advance_signal(
            &mut progress,
            &RtcSignal::Offer {
                sdp: "v=0".into(),
                binding: dummy_binding(TokenRole::Mobile, 1, 1),
            },
            1,
            1,
        )
        .is_err());
        advance_signal(
            &mut progress,
            &RtcSignal::Offer {
                sdp: "v=0".into(),
                binding: dummy_binding(TokenRole::Mobile, 2, 2),
            },
            2,
            2,
        )
        .unwrap();
        assert!(advance_signal(
            &mut progress,
            &RtcSignal::IceComplete {
                envelope: dummy_candidate(None, 1, 1),
            },
            1,
            1,
        )
        .is_err());
    }

    #[tokio::test]
    async fn concurrent_mobile_offer_answers_have_exactly_one_winner_and_cancel_losers() {
        let gateway = gateway();
        let (mut app, _plugin_rx, mut first_rx) = app_with_peers();
        let (second, mut second_rx) = peer(
            "mobile_conn_2",
            "mobile_nonce_2",
            admission("device_b").scopes,
        );
        app.mobiles.insert("device_b".into(), second);
        refresh_pending_mobile_offers(
            &mut app,
            "app_a",
            gateway.inner.v2.signer().unwrap(),
            unix_now().unwrap(),
        )
        .unwrap();
        let offer_for = |device: &str| {
            app.pending_mobile_offers
                .values()
                .find(|signed| {
                    signed.offer.target_device_id == device
                        && signed.offer.offered_mode == LeaseMode::Takeover
                })
                .cloned()
                .expect("targeted takeover offer")
        };
        let first_offer = offer_for("device_a");
        let second_offer = offer_for("device_b");
        assert_eq!(
            first_offer.offer.opportunity_id,
            second_offer.offer.opportunity_id
        );
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .insert("app_a".into(), app);
        let answer = |device: &str, signed: SignedPendingMobileOffer| MobileOfferAnswerFrame {
            kind: "mobile_offer_answer".into(),
            schema_version: 2,
            app_id: "app_a".into(),
            request_id: format!("answer_{device}"),
            idempotency_key: format!("answer-key-{device}"),
            offer_id: signed.offer.offer_id.clone(),
            offer_jti: signed.offer.jti.clone(),
            offer_token: signed.offer_token,
            target_device_id: device.into(),
            target_holder_key_thumbprint: signed.offer.target_holder_key_thumbprint,
            offered_mode: signed.offer.offered_mode,
            call_id: signed.offer.call_id,
            call_epoch: signed.offer.call_epoch,
            owner_epoch: signed.offer.owner_epoch,
        };
        let first_admission = admission("device_a");
        let second_admission = admission("device_b");
        let (first, second) = tokio::join!(
            handle_mobile_offer_answer(
                &gateway,
                &first_admission,
                "mobile_conn",
                answer("device_a", first_offer)
            ),
            handle_mobile_offer_answer(
                &gateway,
                &second_admission,
                "mobile_conn_2",
                answer("device_b", second_offer)
            )
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let mut kinds = Vec::new();
        for receiver in [&mut first_rx, &mut second_rx] {
            while let Ok(Message::Text(encoded)) = receiver.try_recv() {
                kinds.push(
                    serde_json::from_str::<Value>(&encoded).unwrap()["kind"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| kind.as_str() == "mobile_offer_accepted")
                .count(),
            1,
            "exactly one endpoint receives acceptance"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| kind.as_str() == "snapshot")
                .count(),
            2,
            "both endpoints receive the cancellation projection"
        );
        let apps = gateway.inner.v2.apps.lock().await;
        let app = &apps["app_a"];
        assert_eq!(app.mobile_offer_winners.len(), 1);
        assert_eq!(app.accepted_mobile_offers.len(), 1);
        assert!(!app
            .pending_mobile_offers
            .values()
            .any(|signed| { signed.offer.offered_mode == LeaseMode::Takeover }));
    }

    #[test]
    fn signal_dedupe_is_bounded_and_sender_specific() {
        let mut app = V2App::default();
        assert!(app.remember_signal("mobile:a", "signal_1"));
        assert!(!app.remember_signal("mobile:a", "signal_1"));
        assert!(app.remember_signal("plugin:a", "signal_1"));
        for index in 0..=SIGNAL_DEDUPE_CAPACITY {
            app.remember_signal("mobile:a", &format!("signal_{index}"));
        }
        assert!(app.signals.len() <= SIGNAL_DEDUPE_CAPACITY);
    }

    #[test]
    fn disconnect_recovery_revokes_only_that_devices_leases_and_claim() {
        let (mut app, mut plugin_rx, _) = app_with_peers();
        let claims = new_claims(
            "app_a",
            "plugin_a",
            "device_a",
            "call_a",
            7,
            3,
            LeaseMode::Takeover,
            LeasePhase::Prepared,
            9,
            "mobile_nonce",
            "rtc_a",
            &plugin_key().thumbprint,
            &mobile_key().thumbprint,
            unix_now().unwrap(),
        );
        app.claim = Some(TalkClaim {
            request_id: "request_a".into(),
            device_id: "device_a".into(),
            mode: LeaseMode::Takeover,
            call_id: "call_a".into(),
            call_epoch: 7,
            fence: 9,
            lease_jti: claims.jti.clone(),
            active: false,
        });
        app.leases.insert(
            claims.jti.clone(),
            LeaseRecord {
                claims,
                provisional: true,
                mobile_signal: SignalProgress::default(),
                plugin_signal: SignalProgress::default(),
            },
        );
        recover_device(&mut app, "app_a", "device_a", "endpoint_disconnected");
        assert!(app.leases.is_empty());
        assert!(app.claim.is_none());
        let Message::Text(notice) = plugin_rx.try_recv().unwrap() else {
            panic!("expected revocation notice")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&notice).unwrap()["kind"],
            "lease_revoked"
        );
    }

    #[test]
    fn queue_backpressure_is_fail_closed() {
        let (peer, _rx) = peer("mobile", "nonce", vec![]);
        for _ in 0..OUTBOUND_CAPACITY {
            peer.tx.try_send(Message::Ping(vec![])).unwrap();
        }
        assert!(peer.send("{}".into()).is_err());
    }

    #[test]
    fn rtc_payload_remains_targeted_and_never_contains_media_bytes() {
        let routed = PluginRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: 2,
            app_id: "app_a".into(),
            signal_id: "signal_a".into(),
            plugin_id: "plugin_a".into(),
            device_id: "device_a".into(),
            lease_jti: "lease_a".into(),
            rtc_session_id: "rtc_a".into(),
            sdp_revision: 1,
            transport_generation: 1,
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 3,
            fence: 9,
            signal: RtcSignal::Offer {
                sdp: "v=0".into(),
                binding: dummy_binding(TokenRole::Plugin, 1, 1),
            },
        };
        let encoded = serialize(&routed).unwrap();
        assert!(encoded.contains("deviceId"));
        assert!(!encoded.contains("pcm"));
        assert!(!encoded.contains("rtp"));
    }

    #[tokio::test]
    async fn end_caller_confirmation_is_one_use_idempotent_and_relays_once_under_concurrency() {
        let (gateway, token, mut plugin_rx, mut mobile_rx) = active_end_caller_gateway().await;
        handle_end_caller_challenge_request(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            end_challenge_request(&token, "concurrent"),
        )
        .await
        .unwrap();
        let challenge: EndCallerChallengeFrame =
            serde_json::from_value(text_json(&mut mobile_rx).await).unwrap();
        let confirm = end_confirm(&token, &challenge, "concurrent");
        let mobile_admission = admission("device_a");
        let (first, second) = tokio::join!(
            handle_end_caller_confirm(&gateway, &mobile_admission, "mobile_conn", confirm.clone(),),
            handle_end_caller_confirm(&gateway, &mobile_admission, "mobile_conn", confirm.clone(),)
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(
            text_json(&mut mobile_rx).await["kind"],
            "end_caller_submitted"
        );
        assert_eq!(
            text_json(&mut mobile_rx).await["kind"],
            "end_caller_submitted"
        );
        let execute = text_json(&mut plugin_rx).await;
        assert_eq!(execute["kind"], "end_caller_execute");
        assert_eq!(execute["deviceId"], "device_a");
        assert!(matches!(
            plugin_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let replay = end_confirm(&token, &challenge, "new-key-replay");
        handle_end_caller_confirm(&gateway, &admission("device_a"), "mobile_conn", replay)
            .await
            .unwrap();
        let error = text_json(&mut mobile_rx).await;
        assert_eq!(error["code"], "confirmation_used");
        assert_no_end_execute(&mut plugin_rx);
    }

    #[tokio::test]
    async fn end_caller_plugin_completion_is_exact_targeted_and_revokes_talk_route() {
        let (gateway, token, mut plugin_rx, mut mobile_rx) = active_end_caller_gateway().await;
        handle_end_caller_challenge_request(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            end_challenge_request(&token, "complete"),
        )
        .await
        .unwrap();
        let challenge: EndCallerChallengeFrame =
            serde_json::from_value(text_json(&mut mobile_rx).await).unwrap();
        handle_end_caller_confirm(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            end_confirm(&token, &challenge, "complete"),
        )
        .await
        .unwrap();
        assert_eq!(
            text_json(&mut mobile_rx).await["kind"],
            "end_caller_submitted"
        );
        let execute: PluginEndCallerExecuteFrame =
            serde_json::from_value(text_json(&mut plugin_rx).await).unwrap();
        let result = PluginEndCallerResultFrame {
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
            outcome: EndCallerOutcome::Completed,
            code: None,
            message: None,
        };
        handle_end_caller_result(&gateway, &plugin_admission(), "plugin_conn", result)
            .await
            .unwrap();
        let mobile_result = text_json(&mut mobile_rx).await;
        assert_eq!(mobile_result["kind"], "end_caller_result");
        assert_eq!(mobile_result["outcome"], "completed");
        assert_eq!(mobile_result["deviceId"], "device_a");
        let revoke = text_json(&mut plugin_rx).await;
        assert_eq!(revoke["kind"], "lease_revoked");
        assert_eq!(revoke["leaseJti"], execute.lease_jti);
        let apps = gateway.inner.v2.apps.lock().await;
        assert!(apps["app_a"].leases.is_empty());
        assert!(apps["app_a"].claim.is_none());
        assert!(apps["app_a"].end_caller_operation.is_none());
    }

    #[tokio::test]
    async fn end_caller_expired_confirmation_never_relays() {
        let (gateway, token, mut plugin_rx, mut mobile_rx) = active_end_caller_gateway().await;
        handle_end_caller_challenge_request(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            end_challenge_request(&token, "expired"),
        )
        .await
        .unwrap();
        let challenge: EndCallerChallengeFrame =
            serde_json::from_value(text_json(&mut mobile_rx).await).unwrap();
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .get_mut("app_a")
            .unwrap()
            .end_caller_challenges
            .get_mut(&challenge.confirmation_id)
            .unwrap()
            .frame
            .expires_at = unix_now().unwrap().saturating_sub(1);
        handle_end_caller_confirm(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            end_confirm(&token, &challenge, "expired"),
        )
        .await
        .unwrap();
        let error = text_json(&mut mobile_rx).await;
        assert_eq!(error["code"], "confirmation_expired");
        assert_no_end_execute(&mut plugin_rx);
    }

    #[tokio::test]
    async fn end_caller_wrong_device_cannot_use_anothers_live_takeover_token() {
        let (gateway, token, mut plugin_rx, _owner_rx) = active_end_caller_gateway().await;
        let (other, mut other_rx) = peer(
            "mobile_conn_b",
            "mobile_nonce_b",
            admission("device_b").scopes,
        );
        gateway
            .inner
            .v2
            .apps
            .lock()
            .await
            .get_mut("app_a")
            .unwrap()
            .mobiles
            .insert("device_b".into(), other);
        handle_end_caller_challenge_request(
            &gateway,
            &admission("device_b"),
            "mobile_conn_b",
            end_challenge_request(&token, "wrong-device"),
        )
        .await
        .unwrap();
        let error = text_json(&mut other_rx).await;
        assert_eq!(error["code"], "not_active_takeover_owner");
        assert_no_end_execute(&mut plugin_rx);
    }

    #[tokio::test]
    async fn end_caller_observer_advisor_wrong_owner_and_stale_fences_never_relay() {
        #[derive(Clone, Copy)]
        enum Scenario {
            Observer,
            Advisor,
            WrongOwner,
            CallEpoch,
            OwnerEpoch,
            Switchboard,
            Remote,
            AlreadyEnded,
        }
        for (scenario, expected) in [
            (Scenario::Observer, "end_caller_denied"),
            (Scenario::Advisor, "end_caller_denied"),
            (Scenario::WrongOwner, "not_active_takeover_owner"),
            (Scenario::CallEpoch, "stale_end_caller_state"),
            (Scenario::OwnerEpoch, "stale_end_caller_state"),
            (Scenario::Switchboard, "stale_end_caller_state"),
            (Scenario::Remote, "stale_end_caller_state"),
            (Scenario::AlreadyEnded, "call_ended"),
        ] {
            let (gateway, token, mut plugin_rx, mut mobile_rx) = active_end_caller_gateway().await;
            let mut request = end_challenge_request(&token, "denied");
            {
                let mut apps = gateway.inner.v2.apps.lock().await;
                let app = apps.get_mut("app_a").unwrap();
                match scenario {
                    Scenario::Observer => app
                        .mobiles
                        .get_mut("device_a")
                        .unwrap()
                        .grants
                        .retain(|grant| *grant != Grant::EndCaller),
                    Scenario::Advisor => app
                        .mobiles
                        .get_mut("device_a")
                        .unwrap()
                        .grants
                        .retain(|grant| *grant != Grant::Takeover),
                    Scenario::WrongOwner => {
                        app.claim.as_mut().unwrap().device_id = "device_other".into()
                    }
                    Scenario::CallEpoch => request.call_epoch += 1,
                    Scenario::OwnerEpoch => request.owner_epoch += 1,
                    Scenario::Switchboard => request.switchboard_revision += 1,
                    Scenario::Remote => request.remote_revision += 1,
                    Scenario::AlreadyEnded => {
                        let snapshot = app.snapshot.as_mut().unwrap();
                        snapshot.telephony_state = TelephonyState::Ended;
                        snapshot.service_mode = ServiceMode::Ended;
                    }
                }
            }
            handle_end_caller_challenge_request(
                &gateway,
                &admission("device_a"),
                "mobile_conn",
                request,
            )
            .await
            .unwrap();
            let error = text_json(&mut mobile_rx).await;
            assert_eq!(error["code"], expected, "scenario denied with wrong code");
            assert_no_end_execute(&mut plugin_rx);
        }
    }

    #[tokio::test]
    async fn return_to_aokie_wins_race_and_confirm_cannot_relay_hangup() {
        let (gateway, token, mut plugin_rx, mut mobile_rx) = active_end_caller_gateway().await;
        handle_end_caller_challenge_request(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            end_challenge_request(&token, "return-race"),
        )
        .await
        .unwrap();
        let challenge: EndCallerChallengeFrame =
            serde_json::from_value(text_json(&mut mobile_rx).await).unwrap();

        handle_mobile_revoke(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            LeaseRevokeFrame {
                kind: "lease_revoke".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                request_id: "return_request".into(),
                idempotency_key: "return-request-key".into(),
                lease_token: token.clone(),
                reason: "native_call_ui_ended".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(text_json(&mut mobile_rx).await["kind"], "lease_revoked");

        handle_end_caller_confirm(
            &gateway,
            &admission("device_a"),
            "mobile_conn",
            end_confirm(&token, &challenge, "after-return"),
        )
        .await
        .unwrap();
        let error = text_json(&mut mobile_rx).await;
        assert!(matches!(
            error["code"].as_str(),
            Some("confirmation_stale" | "stale_lease")
        ));
        assert_no_end_execute(&mut plugin_rx);
    }
}
