//! Self-hostable Aokie Companion signalling/control gateway.
//!
//! The gateway owns client-visible stream sequencing and never becomes the
//! physical call authority. One authenticated Desktop publisher supplies
//! snapshots; authenticated Companion devices receive them and may send
//! revision-fenced commands back to that publisher. Audio and durable business
//! records are deliberately outside this crate.

pub mod v2;

use aokie_protocol::{
    CallSnapshot, CommandAck, CommandEnvelope, CommandError, CommandType, ProtocolErrorCode,
    SyncReadyFrame, MAX_JSON_SAFE_INTEGER, SCHEMA_VERSION,
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch, Mutex, OwnedSemaphorePermit, Semaphore};

const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const OUTBOUND_CAPACITY: usize = 32;
const REPLAY_CAPACITY: usize = 128;
const PENDING_COMMAND_CAPACITY: usize = 128;
const ACK_CACHE_CAPACITY: usize = 256;
const IDEMPOTENCY_INDEX_CAPACITY: usize = ACK_CACHE_CAPACITY + PENDING_COMMAND_CAPACITY;
const MAX_APPS: usize = 1024;
const MAX_MOBILES_PER_APP: usize = 16;
const MAX_ADMISSIONS: usize = 4096;
const DESKTOP_INBOUND_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionRole {
    Desktop,
    Mobile,
    Plugin,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdmissionSpec {
    token: String,
    role: AdmissionRole,
    app_id: String,
    subject_id: String,
    #[serde(default)]
    grants: Vec<CommandType>,
    #[serde(default)]
    scopes: Vec<aokie_protocol::v2::Grant>,
}

#[derive(Clone, Debug)]
struct Admission {
    role: AdmissionRole,
    app_id: String,
    subject_id: String,
    grants: Vec<CommandType>,
    scopes: Vec<aokie_protocol::v2::Grant>,
}

#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub bind: SocketAddr,
    admissions: Vec<AdmissionSpec>,
    lease_hmac_secret: Option<Vec<u8>>,
    admission_hmac_secret: Option<Vec<u8>>,
    allow_static_v2_admissions: bool,
}

impl GatewayConfig {
    pub fn from_env() -> Result<Self, String> {
        let bind = std::env::var("AOKIE_GATEWAY_BIND")
            .unwrap_or_else(|_| "127.0.0.1:18787".to_string())
            .parse::<SocketAddr>()
            .map_err(|_| "AOKIE_GATEWAY_BIND must be an IP socket address".to_string())?;
        if !bind.ip().is_loopback()
            && std::env::var("AOKIE_GATEWAY_ALLOW_PUBLIC_HTTP").as_deref() != Ok("1")
        {
            return Err(
                "non-loopback plain HTTP is refused; keep the listener private behind TLS or explicitly set AOKIE_GATEWAY_ALLOW_PUBLIC_HTTP=1"
                    .into(),
            );
        }
        let admissions: Vec<AdmissionSpec> = match std::env::var("AOKIE_GATEWAY_ADMISSIONS") {
            Ok(encoded) => serde_json::from_str(&encoded)
                .map_err(|_| "AOKIE_GATEWAY_ADMISSIONS is invalid JSON".to_string())?,
            Err(_) => Vec::new(),
        };
        let lease_hmac_secret = std::env::var("AOKIE_GATEWAY_LEASE_HMAC_SECRET")
            .ok()
            .map(String::into_bytes);
        if lease_hmac_secret
            .as_ref()
            .is_some_and(|secret| !(32..=4096).contains(&secret.len()))
        {
            return Err(
                "AOKIE_GATEWAY_LEASE_HMAC_SECRET must be 32..4096 bytes when configured".into(),
            );
        }
        let admission_hmac_secret = std::env::var("AOKIE_GATEWAY_ADMISSION_HMAC_SECRET")
            .ok()
            .map(String::into_bytes);
        if admission_hmac_secret
            .as_ref()
            .is_some_and(|secret| !(32..=4096).contains(&secret.len()))
        {
            return Err(
                "AOKIE_GATEWAY_ADMISSION_HMAC_SECRET must be 32..4096 bytes when configured".into(),
            );
        }
        let allow_static_v2_admissions =
            std::env::var("AOKIE_GATEWAY_V2_ALLOW_STATIC_ADMISSIONS").as_deref() == Ok("1");
        if admissions.is_empty() && admission_hmac_secret.is_none() {
            return Err(
                "configure AOKIE_GATEWAY_ADMISSIONS or AOKIE_GATEWAY_ADMISSION_HMAC_SECRET; no anonymous admission exists"
                    .into(),
            );
        }
        Ok(Self {
            bind,
            admissions,
            lease_hmac_secret,
            admission_hmac_secret,
            allow_static_v2_admissions,
        })
    }

    #[cfg(test)]
    fn for_tests(admissions: Vec<AdmissionSpec>) -> Self {
        Self {
            bind: "127.0.0.1:0".parse().expect("test address"),
            admissions,
            lease_hmac_secret: Some(b"test-only-v2-lease-secret-32-bytes-minimum".to_vec()),
            admission_hmac_secret: Some(b"test-only-v2-admission-secret-32-bytes-minimum".to_vec()),
            allow_static_v2_admissions: true,
        }
    }
}

#[derive(Clone)]
struct AdmissionRegistry {
    by_token_hash: Arc<HashMap<[u8; 32], Admission>>,
}

impl AdmissionRegistry {
    fn new(specs: Vec<AdmissionSpec>) -> Result<Self, String> {
        if specs.len() > MAX_ADMISSIONS {
            return Err("gateway admission capacity is exceeded".into());
        }
        let mut by_token_hash = HashMap::new();
        for spec in specs {
            if spec.token.len() < 16 || spec.token.len() > 16 * 1024 {
                return Err("gateway admission tokens must be 16..16384 bytes".into());
            }
            if !safe_id(&spec.app_id) || !safe_id(&spec.subject_id) {
                return Err("gateway admission identities are invalid".into());
            }
            let digest = token_hash(&spec.token);
            if by_token_hash
                .insert(
                    digest,
                    Admission {
                        role: spec.role,
                        app_id: spec.app_id,
                        subject_id: spec.subject_id,
                        grants: spec.grants,
                        scopes: spec.scopes,
                    },
                )
                .is_some()
            {
                return Err("gateway admission tokens must be unique".into());
            }
        }
        Ok(Self {
            by_token_hash: Arc::new(by_token_hash),
        })
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<Admission, StatusCode> {
        let bearer = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let admission = self
            .by_token_hash
            .get(&token_hash(bearer))
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let app_id = header_id(headers, "x-aokie-app-id")?;
        let subject_id = header_id(headers, "x-aokie-device-id")?;
        if app_id != admission.app_id || subject_id != admission.subject_id {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(admission)
    }
}

fn header_id<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, StatusCode> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| safe_id(value))
        .ok_or(StatusCode::BAD_REQUEST)
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[derive(Clone)]
pub struct Gateway {
    inner: Arc<GatewayInner>,
}

struct GatewayInner {
    admissions: AdmissionRegistry,
    apps: Mutex<HashMap<String, AppSession>>,
    v2: v2::V2Gateway,
}

#[derive(Clone)]
struct Peer {
    connection_id: String,
    tx: mpsc::Sender<Message>,
    fenced: watch::Sender<bool>,
}

impl Peer {
    fn fence(&self) {
        self.fenced.send_replace(true);
        let _ = self.tx.try_send(Message::Close(None));
    }
}

struct PendingCommand {
    device_id: String,
    timeout_cancel: Option<oneshot::Sender<()>>,
}

impl PendingCommand {
    fn cancel_timeout(&mut self) {
        if let Some(cancel) = self.timeout_cancel.take() {
            let _ = cancel.send(());
        }
    }
}

struct IdempotencyRecord {
    device_id: String,
    command_id: String,
    fingerprint: [u8; 32],
}

enum IdempotencyDecision {
    New,
    Retry,
    Conflict,
}

struct AppSession {
    stream_nonce: String,
    sequence: u64,
    latest_frame: Option<String>,
    latest_snapshot: Option<CallSnapshot>,
    replay: VecDeque<(u64, String)>,
    desktop: Option<Peer>,
    mobiles: HashMap<String, Peer>,
    pending_commands: HashMap<String, PendingCommand>,
    acknowledgement_cache: HashMap<String, (String, String)>,
    acknowledgement_order: VecDeque<String>,
    idempotency_index: HashMap<String, IdempotencyRecord>,
    idempotency_by_command: HashMap<String, String>,
    timeout_slots: Arc<Semaphore>,
}

impl AppSession {
    fn new() -> Self {
        Self {
            stream_nonce: new_stream_nonce(),
            sequence: 0,
            latest_frame: None,
            latest_snapshot: None,
            replay: VecDeque::new(),
            desktop: None,
            mobiles: HashMap::new(),
            pending_commands: HashMap::new(),
            acknowledgement_cache: HashMap::new(),
            acknowledgement_order: VecDeque::new(),
            idempotency_index: HashMap::new(),
            idempotency_by_command: HashMap::new(),
            timeout_slots: Arc::new(Semaphore::new(PENDING_COMMAND_CAPACITY)),
        }
    }

    fn record_frame(&mut self, frame: String) {
        self.latest_frame = Some(frame.clone());
        self.replay.push_back((self.sequence, frame));
        while self.replay.len() > REPLAY_CAPACITY {
            self.replay.pop_front();
        }
    }

    fn frames_for_resume(&self, nonce: Option<&str>, sequence: u64) -> Vec<String> {
        let Some(latest_frame) = self.latest_frame.as_ref() else {
            return Vec::new();
        };
        if nonce == Some(self.stream_nonce.as_str()) && sequence < self.sequence {
            let frames = self
                .replay
                .iter()
                .filter(|(candidate, _)| *candidate > sequence)
                .map(|(_, frame)| frame.clone())
                .collect::<Vec<_>>();
            if !frames.is_empty() && frames.len() <= OUTBOUND_CAPACITY {
                return frames;
            }
        }
        // A reconnect at the current cursor still needs a fresh authoritative
        // proof before the client re-enables controls. Every replay entry is a
        // full authoritative state, so a replay larger than the bounded
        // outbound queue safely converges directly to the latest frame.
        vec![latest_frame.clone()]
    }

    fn invalidate_authority(&mut self, message: &str) -> AuthorityInvalidation {
        self.stream_nonce = new_stream_nonce();
        self.sequence = 0;
        self.latest_frame = None;
        self.latest_snapshot = None;
        self.replay.clear();
        // Command idempotency survives publisher epochs. Without this cache,
        // a mobile retry after replacement could execute a command that the
        // prior publisher had already accepted or that this gateway rejected.

        let pending = std::mem::take(&mut self.pending_commands);
        let mut rejections = Vec::with_capacity(pending.len());
        for (command_id, mut pending) in pending {
            pending.cancel_timeout();
            let acknowledgement =
                rejected_ack(&command_id, ProtocolErrorCode::EndpointUnreachable, message);
            let encoded = wrapped_ack(&acknowledgement);
            if let Some(mobile) = self.mobiles.get(&pending.device_id).cloned() {
                rejections.push((mobile, encoded.clone()));
            }
            self.cache_ack(&command_id, &pending.device_id, encoded);
        }

        AuthorityInvalidation {
            mobiles: self.mobiles.drain().map(|(_, peer)| peer).collect(),
            rejections,
        }
    }

    fn cache_ack(&mut self, command_id: &str, device_id: &str, encoded: String) {
        if !self.acknowledgement_cache.contains_key(command_id) {
            self.acknowledgement_order.push_back(command_id.to_string());
        }
        self.acknowledgement_cache
            .insert(command_id.to_string(), (device_id.to_string(), encoded));
        while self.acknowledgement_order.len() > ACK_CACHE_CAPACITY {
            self.evict_oldest_ack();
        }
    }

    fn bind_idempotency(
        &mut self,
        device_id: &str,
        command: &CommandEnvelope,
    ) -> IdempotencyDecision {
        let fingerprint = command_fingerprint(command);
        if let Some(existing) = self.idempotency_index.get(&command.idempotency_key) {
            return if existing.device_id == device_id
                && existing.command_id == command.command_id
                && existing.fingerprint == fingerprint
            {
                IdempotencyDecision::Retry
            } else {
                IdempotencyDecision::Conflict
            };
        }

        if self
            .idempotency_by_command
            .get(&command.command_id)
            .is_some_and(|key| key != &command.idempotency_key)
            || self.acknowledgement_cache.contains_key(&command.command_id)
            || self.pending_commands.contains_key(&command.command_id)
        {
            return IdempotencyDecision::Conflict;
        }

        while self.idempotency_index.len() >= IDEMPOTENCY_INDEX_CAPACITY {
            if !self.evict_oldest_ack() {
                return IdempotencyDecision::Conflict;
            }
        }
        self.idempotency_by_command
            .insert(command.command_id.clone(), command.idempotency_key.clone());
        self.idempotency_index.insert(
            command.idempotency_key.clone(),
            IdempotencyRecord {
                device_id: device_id.to_string(),
                command_id: command.command_id.clone(),
                fingerprint,
            },
        );
        IdempotencyDecision::New
    }

    fn evict_oldest_ack(&mut self) -> bool {
        while let Some(oldest) = self.acknowledgement_order.pop_front() {
            if self.acknowledgement_cache.remove(&oldest).is_some() {
                if let Some(key) = self.idempotency_by_command.remove(&oldest) {
                    if self
                        .idempotency_index
                        .get(&key)
                        .is_some_and(|record| record.command_id == oldest)
                    {
                        self.idempotency_index.remove(&key);
                    }
                }
                return true;
            }
        }
        false
    }
}

struct AuthorityInvalidation {
    mobiles: Vec<Peer>,
    rejections: Vec<(Peer, String)>,
}

impl AuthorityInvalidation {
    fn dispatch(self) {
        for (peer, encoded) in self.rejections {
            let _ = peer.tx.try_send(Message::Text(encoded));
        }
        for peer in self.mobiles {
            peer.fence();
        }
    }
}

fn new_stream_nonce() -> String {
    format!("stream_{}", uuid::Uuid::new_v4().simple())
}

fn command_fingerprint(command: &CommandEnvelope) -> [u8; 32] {
    let canonical =
        serde_json::to_vec(command).expect("a validated command envelope always serializes");
    Sha256::digest(canonical).into()
}

fn record_desktop_activity(activity: &Option<watch::Sender<u64>>) {
    if let Some(activity) = activity {
        activity.send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MobileResume {
    kind: String,
    schema_version: u16,
    app_id: String,
    device_id: String,
    #[serde(default)]
    last_stream_nonce: Option<String>,
    #[serde(default)]
    last_sequence: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DesktopResume {
    kind: String,
    schema_version: u16,
    app_id: String,
    desktop_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DesktopIdle {
    kind: String,
    schema_version: u16,
    app_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DesktopAck {
    kind: String,
    schema_version: u16,
    app_id: String,
    device_id: String,
    payload: CommandAck,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WrappedCommand {
    kind: String,
    payload: CommandEnvelope,
}

impl Gateway {
    pub fn new(config: GatewayConfig) -> Result<Self, String> {
        let GatewayConfig {
            bind: _,
            admissions,
            lease_hmac_secret,
            admission_hmac_secret,
            allow_static_v2_admissions,
        } = config;
        Ok(Self {
            inner: Arc::new(GatewayInner {
                admissions: AdmissionRegistry::new(admissions)?,
                apps: Mutex::new(HashMap::new()),
                v2: v2::V2Gateway::new(
                    lease_hmac_secret,
                    admission_hmac_secret,
                    allow_static_v2_admissions,
                ),
            }),
        })
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/version", get(version))
            .route("/v1/realtime", get(realtime))
            .route("/v2/realtime", get(v2::realtime))
            .with_state(self.clone())
    }

    async fn health(&self) -> Health {
        let apps = self.inner.apps.lock().await;
        let v2 = self.inner.v2.stats().await;
        Health {
            status: "ok",
            apps: apps.len(),
            desktops: apps
                .values()
                .filter(|session| session.desktop.is_some())
                .count(),
            mobiles: apps.values().map(|session| session.mobiles.len()).sum(),
            media_bridge: false,
            v2_enabled: self.inner.v2.is_enabled(),
            v2_apps: v2.apps,
            v2_plugins: v2.plugins,
            v2_mobiles: v2.mobiles,
        }
    }

    async fn register_mobile(
        &self,
        admission: &Admission,
        hello: MobileResume,
        peer: Peer,
    ) -> Result<(), String> {
        if hello.kind != "resume"
            || hello.schema_version != SCHEMA_VERSION
            || hello.app_id != admission.app_id
            || hello.device_id != admission.subject_id
            || hello.last_sequence > MAX_JSON_SAFE_INTEGER
            || hello
                .last_stream_nonce
                .as_deref()
                .is_some_and(|nonce| !safe_id(nonce))
        {
            return Err("invalid mobile resume".into());
        }
        let mut apps = self.inner.apps.lock().await;
        let session = apps
            .get_mut(&admission.app_id)
            .ok_or_else(|| "Aokie Desktop authority is not connected".to_string())?;
        if session.desktop.is_none() {
            return Err("Aokie Desktop authority is not connected".into());
        }
        if session.latest_frame.is_none() {
            return Err("Aokie Desktop has not asserted an authoritative state".into());
        }
        let frames =
            session.frames_for_resume(hello.last_stream_nonce.as_deref(), hello.last_sequence);
        if !session.mobiles.contains_key(&admission.subject_id)
            && session.mobiles.len() >= MAX_MOBILES_PER_APP
        {
            return Err("gateway mobile capacity is exhausted".into());
        }
        for frame in frames {
            if peer.tx.try_send(Message::Text(frame)).is_err() {
                return Err("mobile initial state channel is unavailable".into());
            }
        }
        let replaced = session.mobiles.insert(admission.subject_id.clone(), peer);
        drop(apps);
        if let Some(replaced) = replaced {
            replaced.fence();
        }
        Ok(())
    }

    async fn register_desktop(
        &self,
        admission: &Admission,
        hello: DesktopResume,
        peer: Peer,
    ) -> Result<(), String> {
        if hello.kind != "desktop_resume"
            || hello.schema_version != SCHEMA_VERSION
            || hello.app_id != admission.app_id
            || hello.desktop_id != admission.subject_id
        {
            return Err("invalid desktop resume".into());
        }
        let mut apps = self.inner.apps.lock().await;
        if !apps.contains_key(&admission.app_id) && apps.len() >= MAX_APPS {
            return Err("gateway app capacity is exhausted".into());
        }
        let peer_connection_id = peer.connection_id.clone();
        let session = apps
            .entry(admission.app_id.clone())
            .or_insert_with(AppSession::new);
        let replaced = session.desktop.take();
        let invalidation = session.invalidate_authority(
            "Aokie Desktop publisher authority changed before acknowledging the command",
        );
        session.desktop = Some(peer);
        drop(apps);
        invalidation.dispatch();
        if let Some(replaced) =
            replaced.filter(|current| current.connection_id != peer_connection_id)
        {
            replaced.fence();
        }
        Ok(())
    }

    async fn monitor_desktop_inbound(
        &self,
        admission: Admission,
        connection_id: String,
        mut activity: watch::Receiver<u64>,
        timeout: Duration,
    ) {
        loop {
            match tokio::time::timeout(timeout, activity.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return,
                Err(_) => {
                    self.unregister(&admission, &connection_id).await;
                    return;
                }
            }
        }
    }

    async fn publish_snapshot(
        &self,
        admission: &Admission,
        connection_id: &str,
        mut payload: Value,
    ) -> Result<(), String> {
        let object = payload
            .as_object_mut()
            .ok_or_else(|| "desktop snapshot payload must be an object".to_string())?;
        for gateway_owned in ["appId", "streamNonce", "sequence"] {
            if object.contains_key(gateway_owned) {
                return Err(format!(
                    "desktop snapshot must omit gateway-owned {gateway_owned}"
                ));
            }
        }

        let (encoded, peers) = {
            let mut apps = self.inner.apps.lock().await;
            let session = apps
                .entry(admission.app_id.clone())
                .or_insert_with(AppSession::new);
            require_current_desktop(session, connection_id)?;
            session.sequence = next_sequence(session.sequence)?;
            object.insert("appId".into(), json!(admission.app_id));
            object.insert("streamNonce".into(), json!(session.stream_nonce));
            object.insert("sequence".into(), json!(session.sequence));
            let snapshot: CallSnapshot = serde_json::from_value(payload)
                .map_err(|_| "desktop snapshot does not match protocol v1".to_string())?;
            snapshot.validate().map_err(|error| error.to_string())?;
            let encoded = json!({ "kind": "snapshot", "payload": snapshot }).to_string();
            session.latest_snapshot = Some(snapshot);
            session.record_frame(encoded.clone());
            let peers = session
                .mobiles
                .iter()
                .map(|(device, peer)| (device.clone(), peer.clone()))
                .collect::<Vec<_>>();
            (encoded, peers)
        };
        self.fan_out(&admission.app_id, peers, encoded).await;
        Ok(())
    }

    async fn publish_idle(
        &self,
        admission: &Admission,
        connection_id: &str,
        frame: DesktopIdle,
    ) -> Result<(), String> {
        if frame.kind != "desktop_idle"
            || frame.schema_version != SCHEMA_VERSION
            || frame.app_id != admission.app_id
        {
            return Err("invalid desktop idle frame".into());
        }
        let (encoded, peers) = {
            let mut apps = self.inner.apps.lock().await;
            let session = apps
                .entry(admission.app_id.clone())
                .or_insert_with(AppSession::new);
            require_current_desktop(session, connection_id)?;
            session.sequence = next_sequence(session.sequence)?;
            session.latest_snapshot = None;
            let encoded =
                sync_ready_frame(&admission.app_id, &session.stream_nonce, session.sequence);
            session.record_frame(encoded.clone());
            let peers = session
                .mobiles
                .iter()
                .map(|(device, peer)| (device.clone(), peer.clone()))
                .collect::<Vec<_>>();
            (encoded, peers)
        };
        self.fan_out(&admission.app_id, peers, encoded).await;
        Ok(())
    }

    async fn handle_mobile_command(
        &self,
        admission: &Admission,
        connection_id: &str,
        encoded: &str,
    ) -> Result<(), String> {
        let wrapped: WrappedCommand = serde_json::from_str(encoded)
            .map_err(|_| "invalid mobile command envelope".to_string())?;
        if wrapped.kind != "command" {
            return Err("invalid mobile command kind".into());
        }
        wrapped
            .payload
            .validate()
            .map_err(|error| error.to_string())?;
        let command_id = wrapped.payload.command_id.clone();

        enum Outcome {
            Send(Peer, String, oneshot::Receiver<()>, OwnedSemaphorePermit),
            Reply(Peer, String),
            Pending,
        }
        let outcome = {
            let mut apps = self.inner.apps.lock().await;
            let session = apps
                .get_mut(&admission.app_id)
                .ok_or_else(|| "realtime app session is unavailable".to_string())?;
            let mobile = session
                .mobiles
                .get(&admission.subject_id)
                .filter(|peer| peer.connection_id == connection_id)
                .cloned()
                .ok_or_else(|| "mobile session is no longer current".to_string())?;

            match session.bind_idempotency(&admission.subject_id, &wrapped.payload) {
                IdempotencyDecision::Conflict => {
                    let ack = rejected_ack(
                        &command_id,
                        ProtocolErrorCode::CommandFailed,
                        "idempotency key or command identifier conflicts with an earlier command",
                    );
                    Outcome::Reply(mobile, wrapped_ack(&ack))
                }
                IdempotencyDecision::Retry => {
                    if let Some((cached_device, cached)) =
                        session.acknowledgement_cache.get(&command_id)
                    {
                        if cached_device == &admission.subject_id {
                            Outcome::Reply(mobile, cached.clone())
                        } else {
                            let ack = rejected_ack(
                                &command_id,
                                ProtocolErrorCode::CommandFailed,
                                "idempotency record belongs to another device",
                            );
                            Outcome::Reply(mobile, wrapped_ack(&ack))
                        }
                    } else if session.pending_commands.contains_key(&command_id) {
                        Outcome::Pending
                    } else {
                        let ack = rejected_ack(
                            &command_id,
                            ProtocolErrorCode::CommandFailed,
                            "the prior idempotent command result is no longer available",
                        );
                        Outcome::Reply(mobile, wrapped_ack(&ack))
                    }
                }
                IdempotencyDecision::New
                    if !admission.grants.contains(&wrapped.payload.command_type) =>
                {
                    let ack = rejected_ack(
                        &command_id,
                        ProtocolErrorCode::Forbidden,
                        "this admission does not grant the requested command",
                    );
                    let reply = wrapped_ack(&ack);
                    session.cache_ack(&command_id, &admission.subject_id, reply.clone());
                    Outcome::Reply(mobile, reply)
                }
                IdempotencyDecision::New
                    if session.pending_commands.len() >= PENDING_COMMAND_CAPACITY =>
                {
                    let ack = rejected_ack(
                        &command_id,
                        ProtocolErrorCode::CommandFailed,
                        "gateway command capacity is exhausted",
                    );
                    let reply = wrapped_ack(&ack);
                    session.cache_ack(&command_id, &admission.subject_id, reply.clone());
                    Outcome::Reply(mobile, reply)
                }
                IdempotencyDecision::New => {
                    if let Some(desktop) = session.desktop.clone() {
                        if let Ok(timeout_permit) =
                            session.timeout_slots.clone().try_acquire_owned()
                        {
                            let (timeout_cancel, timeout_cancelled) = oneshot::channel();
                            session.pending_commands.insert(
                                command_id.clone(),
                                PendingCommand {
                                    device_id: admission.subject_id.clone(),
                                    timeout_cancel: Some(timeout_cancel),
                                },
                            );
                            let forwarded = json!({
                                "kind": "command",
                                "schemaVersion": SCHEMA_VERSION,
                                "appId": admission.app_id,
                                "deviceId": admission.subject_id,
                                "payload": wrapped.payload,
                            })
                            .to_string();
                            Outcome::Send(desktop, forwarded, timeout_cancelled, timeout_permit)
                        } else {
                            let ack = rejected_ack(
                                &command_id,
                                ProtocolErrorCode::CommandFailed,
                                "gateway command timeout capacity is exhausted",
                            );
                            let reply = wrapped_ack(&ack);
                            session.cache_ack(&command_id, &admission.subject_id, reply.clone());
                            Outcome::Reply(mobile, reply)
                        }
                    } else {
                        let ack = rejected_ack(
                            &command_id,
                            ProtocolErrorCode::EndpointUnreachable,
                            "Aokie Desktop is not connected",
                        );
                        let reply = wrapped_ack(&ack);
                        session.cache_ack(&command_id, &admission.subject_id, reply.clone());
                        Outcome::Reply(mobile, reply)
                    }
                }
            }
        };

        match outcome {
            Outcome::Send(peer, message, timeout_cancelled, timeout_permit) => {
                let timeout_gateway = self.clone();
                let timeout_app_id = admission.app_id.clone();
                let timeout_command_id = command_id.clone();
                tokio::spawn(async move {
                    let _timeout_permit = timeout_permit;
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {
                            timeout_gateway
                                .reject_pending(
                                    &timeout_app_id,
                                    &timeout_command_id,
                                    ProtocolErrorCode::CommandFailed,
                                    "desktop command acknowledgement timed out",
                                )
                                .await;
                        }
                        _ = timeout_cancelled => {}
                    }
                });
                if peer.tx.try_send(Message::Text(message)).is_err() {
                    self.reject_pending(
                        &admission.app_id,
                        &command_id,
                        ProtocolErrorCode::EndpointUnreachable,
                        "Aokie Desktop command channel is unavailable",
                    )
                    .await;
                }
            }
            Outcome::Reply(peer, message) => {
                self.send_mobile(&admission.app_id, &admission.subject_id, peer, message)
                    .await;
            }
            Outcome::Pending => {}
        }
        Ok(())
    }

    async fn route_desktop_ack(
        &self,
        admission: &Admission,
        connection_id: &str,
        frame: DesktopAck,
    ) -> Result<(), String> {
        if frame.kind != "desktop_command_ack"
            || frame.schema_version != SCHEMA_VERSION
            || frame.app_id != admission.app_id
            || !safe_id(&frame.device_id)
        {
            return Err("invalid desktop command acknowledgement".into());
        }
        frame
            .payload
            .validate()
            .map_err(|error| error.to_string())?;
        let command_id = frame.payload.command_id.clone();
        let encoded = wrapped_ack(&frame.payload);
        let mobile = {
            let mut apps = self.inner.apps.lock().await;
            let session = apps
                .get_mut(&admission.app_id)
                .ok_or_else(|| "realtime app session is unavailable".to_string())?;
            require_current_desktop(session, connection_id)?;
            let pending = session.pending_commands.get(&command_id).ok_or_else(|| {
                "desktop acknowledgement does not match a pending command".to_string()
            })?;
            if pending.device_id != frame.device_id {
                return Err("desktop acknowledgement targeted the wrong device".into());
            }
            let mut pending = session
                .pending_commands
                .remove(&command_id)
                .expect("pending command was checked above");
            pending.cancel_timeout();
            session.cache_ack(&command_id, &frame.device_id, encoded.clone());
            session.mobiles.get(&frame.device_id).cloned()
        };
        if let Some(mobile) = mobile {
            self.send_mobile(&admission.app_id, &frame.device_id, mobile, encoded)
                .await;
        }
        Ok(())
    }

    async fn reject_pending(
        &self,
        app_id: &str,
        command_id: &str,
        code: ProtocolErrorCode,
        message: &str,
    ) {
        let routed = {
            let mut apps = self.inner.apps.lock().await;
            let Some(session) = apps.get_mut(app_id) else {
                return;
            };
            let Some(mut pending) = session.pending_commands.remove(command_id) else {
                return;
            };
            pending.cancel_timeout();
            let ack = rejected_ack(command_id, code, message);
            let encoded = wrapped_ack(&ack);
            session.cache_ack(command_id, &pending.device_id, encoded.clone());
            let device_id = pending.device_id.clone();
            session
                .mobiles
                .get(&device_id)
                .cloned()
                .map(|peer| (device_id, peer, encoded))
        };
        if let Some((device_id, peer, encoded)) = routed {
            self.send_mobile(app_id, &device_id, peer, encoded).await;
        }
    }

    async fn send_mobile(&self, app_id: &str, device_id: &str, peer: Peer, encoded: String) {
        if peer.tx.try_send(Message::Text(encoded)).is_ok() {
            return;
        }
        let mut apps = self.inner.apps.lock().await;
        let Some(session) = apps.get_mut(app_id) else {
            peer.fence();
            return;
        };
        if session
            .mobiles
            .get(device_id)
            .is_some_and(|current| current.connection_id == peer.connection_id)
        {
            session.mobiles.remove(device_id);
        }
        peer.fence();
    }

    async fn fan_out(&self, app_id: &str, peers: Vec<(String, Peer)>, encoded: String) {
        let failed = peers
            .into_iter()
            .filter_map(|(device_id, peer)| {
                peer.tx
                    .try_send(Message::Text(encoded.clone()))
                    .err()
                    .map(|_| (device_id, peer.connection_id))
            })
            .collect::<Vec<_>>();
        if failed.is_empty() {
            return;
        }
        let mut apps = self.inner.apps.lock().await;
        if let Some(session) = apps.get_mut(app_id) {
            for (device_id, connection_id) in failed {
                if session
                    .mobiles
                    .get(&device_id)
                    .is_some_and(|peer| peer.connection_id == connection_id)
                {
                    if let Some(peer) = session.mobiles.remove(&device_id) {
                        peer.fence();
                    }
                }
            }
        }
    }

    async fn unregister(&self, admission: &Admission, connection_id: &str) {
        let (invalidation, desktop_to_fence) = {
            let mut apps = self.inner.apps.lock().await;
            let Some(session) = apps.get_mut(&admission.app_id) else {
                return;
            };
            match admission.role {
                AdmissionRole::Desktop => {
                    if session
                        .desktop
                        .as_ref()
                        .is_some_and(|peer| peer.connection_id == connection_id)
                    {
                        let desktop = session.desktop.take();
                        (
                            Some(session.invalidate_authority(
                                "Aokie Desktop disconnected before acknowledging the command",
                            )),
                            desktop,
                        )
                    } else {
                        (None, None)
                    }
                }
                AdmissionRole::Mobile => {
                    if session
                        .mobiles
                        .get(&admission.subject_id)
                        .is_some_and(|peer| peer.connection_id == connection_id)
                    {
                        session.mobiles.remove(&admission.subject_id);
                    }
                    (None, None)
                }
                AdmissionRole::Plugin => (None, None),
            }
        };
        if let Some(invalidation) = invalidation {
            invalidation.dispatch();
        }
        if let Some(desktop) = desktop_to_fence {
            desktop.fence();
        }
    }
}

fn next_sequence(sequence: u64) -> Result<u64, String> {
    let next = sequence
        .checked_add(1)
        .ok_or_else(|| "gateway stream sequence exhausted".to_string())?;
    if next > MAX_JSON_SAFE_INTEGER {
        return Err("gateway stream sequence exhausted".into());
    }
    Ok(next)
}

fn require_current_desktop(session: &AppSession, connection_id: &str) -> Result<(), String> {
    if session
        .desktop
        .as_ref()
        .is_some_and(|peer| peer.connection_id == connection_id)
    {
        Ok(())
    } else {
        Err("desktop publisher session is no longer current".into())
    }
}

fn sync_ready_frame(app_id: &str, stream_nonce: &str, sequence: u64) -> String {
    let frame = SyncReadyFrame {
        kind: "sync_ready".into(),
        schema_version: SCHEMA_VERSION,
        app_id: app_id.to_string(),
        stream_nonce: stream_nonce.to_string(),
        sequence,
    };
    debug_assert!(frame.validate().is_ok());
    serde_json::to_string(&frame).expect("validated sync-ready frame serializes")
}

fn wrapped_ack(ack: &CommandAck) -> String {
    json!({ "kind": "command_ack", "payload": ack }).to_string()
}

fn rejected_ack(command_id: &str, code: ProtocolErrorCode, message: &str) -> CommandAck {
    CommandAck {
        command_id: command_id.to_string(),
        accepted: false,
        error: Some(CommandError {
            code,
            message: message.to_string(),
        }),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Health {
    status: &'static str,
    apps: usize,
    desktops: usize,
    mobiles: usize,
    media_bridge: bool,
    v2_enabled: bool,
    v2_apps: usize,
    v2_plugins: usize,
    v2_mobiles: usize,
}

async fn health(State(gateway): State<Gateway>) -> Json<Health> {
    Json(gateway.health().await)
}

async fn version(State(gateway): State<Gateway>) -> Json<Value> {
    Json(json!({
        "service": "aokie-realtime",
        "version": env!("CARGO_PKG_VERSION"),
        "schemaVersion": SCHEMA_VERSION,
        "mediaBridge": false,
        "realtimeSchemas": [1, 2],
        "v2Enabled": gateway.inner.v2.is_enabled(),
        "v2DynamicAdmission": gateway.inner.v2.has_dynamic_admission(),
        "v2StaticAdmissionFallback": gateway.inner.v2.allows_static_admission(),
        "signallingOnly": true,
    }))
}

async fn realtime(
    State(gateway): State<Gateway>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let admission = match gateway.inner.admissions.authenticate(&headers) {
        Ok(admission) => admission,
        Err(status) => return status.into_response(),
    };
    upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(gateway, admission, socket))
}

async fn handle_socket(gateway: Gateway, admission: Admission, socket: WebSocket) {
    let connection_id = format!("conn_{}", uuid::Uuid::new_v4().simple());
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
    let peer = Peer {
        connection_id: connection_id.clone(),
        tx: tx.clone(),
        fenced,
    };

    let first = match tokio::time::timeout(std::time::Duration::from_secs(10), reader.next()).await
    {
        Ok(Some(Ok(Message::Text(text)))) if text.len() <= MAX_MESSAGE_BYTES => text,
        _ => {
            let _ = tx.try_send(Message::Close(None));
            writer_task.abort();
            return;
        }
    };

    let registered = match admission.role {
        AdmissionRole::Mobile => {
            let hello = serde_json::from_str::<MobileResume>(&first)
                .map_err(|_| "invalid mobile resume".to_string());
            match hello {
                Ok(hello) => {
                    gateway
                        .register_mobile(&admission, hello, peer.clone())
                        .await
                }
                Err(error) => Err(error),
            }
        }
        AdmissionRole::Desktop => {
            let hello = serde_json::from_str::<DesktopResume>(&first)
                .map_err(|_| "invalid desktop resume".to_string());
            match hello {
                Ok(hello) => {
                    gateway
                        .register_desktop(&admission, hello, peer.clone())
                        .await
                }
                Err(error) => Err(error),
            }
        }
        AdmissionRole::Plugin => Err("plugin admissions are only valid on /v2/realtime".into()),
    };
    if registered.is_err() {
        let _ = tx.try_send(Message::Close(None));
        writer_task.abort();
        return;
    }

    let (desktop_activity, desktop_liveness_task) = if admission.role == AdmissionRole::Desktop {
        let (activity_tx, activity_rx) = watch::channel(0_u64);
        let liveness_gateway = gateway.clone();
        let liveness_admission = admission.clone();
        let liveness_connection_id = connection_id.clone();
        let task = tokio::spawn(async move {
            liveness_gateway
                .monitor_desktop_inbound(
                    liveness_admission,
                    liveness_connection_id,
                    activity_rx,
                    DESKTOP_INBOUND_TIMEOUT,
                )
                .await;
        });
        (Some(activity_tx), Some(task))
    } else {
        (None, None)
    };

    loop {
        let incoming = tokio::select! {
            changed = fence_rx.changed() => {
                let _ = changed;
                break;
            }
            incoming = reader.next() => incoming,
        };
        let Some(incoming) = incoming else {
            break;
        };
        match incoming {
            Ok(Message::Text(text)) if text.len() <= MAX_MESSAGE_BYTES => {
                let result = match admission.role {
                    AdmissionRole::Mobile => {
                        gateway
                            .handle_mobile_command(&admission, &connection_id, &text)
                            .await
                    }
                    AdmissionRole::Desktop => {
                        handle_desktop_message(&gateway, &admission, &connection_id, &text).await
                    }
                    AdmissionRole::Plugin => {
                        Err("plugin admissions are only valid on /v2/realtime".into())
                    }
                };
                if result.is_err() {
                    let _ = tx.try_send(Message::Close(None));
                    break;
                }
                record_desktop_activity(&desktop_activity);
            }
            Ok(Message::Ping(bytes)) => {
                if tx.try_send(Message::Pong(bytes)).is_err() {
                    break;
                }
                record_desktop_activity(&desktop_activity);
            }
            Ok(Message::Pong(_)) => record_desktop_activity(&desktop_activity),
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {
                let _ = tx.try_send(Message::Close(None));
                break;
            }
        }
    }
    if let Some(task) = desktop_liveness_task {
        task.abort();
    }
    gateway.unregister(&admission, &connection_id).await;
    drop(peer);
    drop(tx);
    if tokio::time::timeout(std::time::Duration::from_secs(1), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
    }
}

async fn handle_desktop_message(
    gateway: &Gateway,
    admission: &Admission,
    connection_id: &str,
    encoded: &str,
) -> Result<(), String> {
    let value: Value =
        serde_json::from_str(encoded).map_err(|_| "invalid desktop message".to_string())?;
    match value.get("kind").and_then(Value::as_str) {
        Some("desktop_snapshot") => {
            let object = value
                .as_object()
                .ok_or_else(|| "invalid desktop snapshot wrapper".to_string())?;
            if object
                .keys()
                .any(|key| !matches!(key.as_str(), "kind" | "payload"))
            {
                return Err("desktop snapshot wrapper has unknown fields".into());
            }
            let payload = object
                .get("payload")
                .cloned()
                .ok_or_else(|| "desktop snapshot payload is missing".to_string())?;
            gateway
                .publish_snapshot(admission, connection_id, payload)
                .await
        }
        Some("desktop_idle") => {
            let frame: DesktopIdle = serde_json::from_value(value)
                .map_err(|_| "invalid desktop idle frame".to_string())?;
            gateway.publish_idle(admission, connection_id, frame).await
        }
        Some("desktop_command_ack") => {
            let frame: DesktopAck = serde_json::from_value(value)
                .map_err(|_| "invalid desktop acknowledgement".to_string())?;
            gateway
                .route_desktop_ack(admission, connection_id, frame)
                .await
        }
        _ => Err("unsupported desktop message kind".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(
        token: &str,
        role: AdmissionRole,
        app: &str,
        subject: &str,
        grants: Vec<CommandType>,
    ) -> AdmissionSpec {
        AdmissionSpec {
            token: token.to_string(),
            role,
            app_id: app.to_string(),
            subject_id: subject.to_string(),
            grants,
            scopes: vec![],
        }
    }

    fn gateway() -> Gateway {
        Gateway::new(GatewayConfig::for_tests(vec![
            spec(
                "mobile_token_1234567890",
                AdmissionRole::Mobile,
                "app_a",
                "device_a",
                vec![CommandType::TakeoverClaim],
            ),
            spec(
                "desktop_token_123456789",
                AdmissionRole::Desktop,
                "app_a",
                "desktop_a",
                vec![],
            ),
        ]))
        .expect("gateway")
    }

    fn mobile_admission() -> Admission {
        mobile_admission_for("device_a")
    }

    fn mobile_admission_for(device_id: &str) -> Admission {
        Admission {
            role: AdmissionRole::Mobile,
            app_id: "app_a".into(),
            subject_id: device_id.into(),
            grants: vec![CommandType::TakeoverClaim],
            scopes: vec![],
        }
    }

    fn desktop_admission() -> Admission {
        Admission {
            role: AdmissionRole::Desktop,
            app_id: "app_a".into(),
            subject_id: "desktop_a".into(),
            grants: vec![],
            scopes: vec![],
        }
    }

    fn peer(id: &str) -> (Peer, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel(OUTBOUND_CAPACITY);
        let (fenced, _fence_rx) = watch::channel(false);
        (
            Peer {
                connection_id: id.into(),
                tx,
                fenced,
            },
            rx,
        )
    }

    async fn receive_json(rx: &mut mpsc::Receiver<Message>) -> Value {
        let Message::Text(encoded) = rx.recv().await.expect("expected outbound message") else {
            panic!("expected outbound text message")
        };
        serde_json::from_str(&encoded).expect("outbound JSON")
    }

    fn takeover_command() -> CommandEnvelope {
        serde_json::from_str(include_str!(
            "../../../docs/contracts/fixtures/aokie-companion-takeover-command.v1.json"
        ))
        .expect("takeover command fixture")
    }

    fn wrapped_command(command: &CommandEnvelope) -> String {
        json!({"kind":"command","payload":command}).to_string()
    }

    async fn gateway_with_idle_desktop() -> (Gateway, mpsc::Receiver<Message>) {
        let gateway = gateway();
        let (desktop, desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_1",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .unwrap();
        (gateway, desktop_rx)
    }

    fn fixture_without_gateway_fields() -> Value {
        let mut value: Value = serde_json::from_str(include_str!(
            "../../../docs/contracts/fixtures/aokie-companion-live-snapshot.v1.json"
        ))
        .expect("fixture");
        let object = value.as_object_mut().expect("fixture object");
        object.remove("appId");
        object.remove("streamNonce");
        object.remove("sequence");
        value
    }

    #[test]
    fn admissions_are_token_app_device_and_role_bound() {
        let registry = AdmissionRegistry::new(vec![spec(
            "mobile_token_1234567890",
            AdmissionRole::Mobile,
            "app_a",
            "device_a",
            vec![],
        )])
        .expect("registry");
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer mobile_token_1234567890".parse().unwrap(),
        );
        headers.insert("x-aokie-app-id", "app_a".parse().unwrap());
        headers.insert("x-aokie-device-id", "device_a".parse().unwrap());
        assert_eq!(
            registry.authenticate(&headers).expect("admission").role,
            AdmissionRole::Mobile
        );
        headers.insert("x-aokie-app-id", "app_b".parse().unwrap());
        assert_eq!(
            registry.authenticate(&headers).unwrap_err(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn idle_sync_and_snapshot_are_gateway_sequenced_and_replayed() {
        let gateway = gateway();
        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        {
            let apps = gateway.inner.apps.lock().await;
            let session = apps.get("app_a").unwrap();
            assert!(session.latest_frame.is_none());
            assert!(session.replay.is_empty());
            assert_eq!(session.sequence, 0);
        }
        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_1",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .expect("desktop idle assertion");
        let (mobile, mut mobile_rx) = peer("mobile_1");
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .expect("mobile registration");
        let Message::Text(initial) = mobile_rx.recv().await.unwrap() else {
            panic!("expected initial authoritative frame")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&initial).unwrap()["kind"],
            "sync_ready"
        );
        gateway
            .publish_snapshot(
                &desktop_admission(),
                "desktop_1",
                fixture_without_gateway_fields(),
            )
            .await
            .expect("snapshot");
        let apps = gateway.inner.apps.lock().await;
        let session = apps.get("app_a").unwrap();
        let snapshot = session.latest_snapshot.as_ref().unwrap();
        assert_eq!(snapshot.app_id, "app_a");
        assert_eq!(snapshot.sequence, 2);
        assert_eq!(snapshot.stream_nonce, session.stream_nonce);
        let replay = session.frames_for_resume(Some(&session.stream_nonce), 1);
        assert_eq!(replay.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&replay[0]).unwrap()["kind"],
            "snapshot"
        );
    }

    #[tokio::test]
    async fn mobile_registration_requires_a_desktop_authoritative_frame() {
        let gateway = gateway();
        let (mobile, _mobile_rx) = peer("mobile_1");
        let error = gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap_err();
        assert!(error.contains("authority is not connected"));
        assert!(gateway.inner.apps.lock().await.is_empty());

        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        let (mobile, _mobile_rx) = peer("mobile_2");
        let error = gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap_err();
        assert!(error.contains("has not asserted"));
        assert!(gateway.inner.apps.lock().await["app_a"].mobiles.is_empty());
    }

    #[tokio::test]
    async fn admission_grants_are_enforced_before_desktop_routing() {
        let gateway = gateway();
        let mut admission = mobile_admission();
        admission.grants.clear();
        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_1",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .unwrap();
        let (mobile, mut mobile_rx) = peer("mobile_1");
        gateway
            .register_mobile(
                &admission,
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");
        let command: Value = serde_json::from_str(include_str!(
            "../../../docs/contracts/fixtures/aokie-companion-takeover-command.v1.json"
        ))
        .unwrap();
        gateway
            .handle_mobile_command(
                &admission,
                "mobile_1",
                &json!({"kind":"command","payload":command}).to_string(),
            )
            .await
            .unwrap();
        let Message::Text(ack) = mobile_rx.recv().await.unwrap() else {
            panic!("expected text acknowledgement")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&ack).unwrap()["payload"]["error"]["code"],
            "forbidden"
        );
    }

    #[tokio::test]
    async fn commands_and_acknowledgements_route_only_to_the_origin_device() {
        let gateway = gateway();
        let (mobile, mut mobile_rx) = peer("mobile_1");
        let (desktop, mut desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_1",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .unwrap();
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");
        let command: Value = serde_json::from_str(include_str!(
            "../../../docs/contracts/fixtures/aokie-companion-takeover-command.v1.json"
        ))
        .unwrap();
        let command_id = command["commandId"].as_str().unwrap().to_string();
        gateway
            .handle_mobile_command(
                &mobile_admission(),
                "mobile_1",
                &json!({"kind":"command","payload":command}).to_string(),
            )
            .await
            .unwrap();
        let Message::Text(forwarded) = desktop_rx.recv().await.unwrap() else {
            panic!("expected forwarded command")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&forwarded).unwrap()["deviceId"],
            "device_a"
        );
        let wrong_device_ack = CommandAck {
            command_id: command_id.clone(),
            accepted: true,
            error: None,
        };
        assert!(gateway
            .route_desktop_ack(
                &desktop_admission(),
                "desktop_1",
                DesktopAck {
                    kind: "desktop_command_ack".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_other".into(),
                    payload: wrong_device_ack,
                },
            )
            .await
            .unwrap_err()
            .contains("wrong device"));
        assert!(gateway.inner.apps.lock().await["app_a"]
            .pending_commands
            .contains_key(&command_id));
        let ack = CommandAck {
            command_id: command_id.clone(),
            accepted: true,
            error: None,
        };
        gateway
            .route_desktop_ack(
                &desktop_admission(),
                "desktop_1",
                DesktopAck {
                    kind: "desktop_command_ack".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    payload: ack,
                },
            )
            .await
            .unwrap();
        let Message::Text(routed) = mobile_rx.recv().await.unwrap() else {
            panic!("expected routed acknowledgement")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&routed).unwrap()["payload"]["commandId"],
            command_id
        );
    }

    #[tokio::test]
    async fn idempotency_key_forwards_only_one_exact_command_identity() {
        let (gateway, mut desktop_rx) = gateway_with_idle_desktop().await;
        let (mobile, mut mobile_rx) = peer("mobile_1");
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");

        let command = takeover_command();
        gateway
            .handle_mobile_command(&mobile_admission(), "mobile_1", &wrapped_command(&command))
            .await
            .unwrap();
        assert!(matches!(desktop_rx.recv().await, Some(Message::Text(_))));

        gateway
            .handle_mobile_command(&mobile_admission(), "mobile_1", &wrapped_command(&command))
            .await
            .unwrap();
        assert!(matches!(
            desktop_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            mobile_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let mut different_id = command.clone();
        different_id.command_id = "cmd_takeover_different".into();
        gateway
            .handle_mobile_command(
                &mobile_admission(),
                "mobile_1",
                &wrapped_command(&different_id),
            )
            .await
            .unwrap();
        assert_eq!(
            receive_json(&mut mobile_rx).await["payload"]["error"]["code"],
            "command_failed"
        );
        assert!(matches!(
            desktop_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let mut different_content = command.clone();
        different_content.expected_remote_revision += 1;
        gateway
            .handle_mobile_command(
                &mobile_admission(),
                "mobile_1",
                &wrapped_command(&different_content),
            )
            .await
            .unwrap();
        assert_eq!(
            receive_json(&mut mobile_rx).await["payload"]["error"]["code"],
            "command_failed"
        );
        assert!(matches!(
            desktop_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        let apps = gateway.inner.apps.lock().await;
        let session = &apps["app_a"];
        assert_eq!(session.pending_commands.len(), 1);
        assert_eq!(session.idempotency_index.len(), 1);
        assert_eq!(session.idempotency_by_command.len(), 1);
    }

    #[tokio::test]
    async fn idempotency_key_is_device_bound() {
        let (gateway, mut desktop_rx) = gateway_with_idle_desktop().await;
        let (mobile_a, mut mobile_a_rx) = peer("mobile_a");
        gateway
            .register_mobile(
                &mobile_admission_for("device_a"),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile_a,
            )
            .await
            .unwrap();
        let (mobile_b, mut mobile_b_rx) = peer("mobile_b");
        gateway
            .register_mobile(
                &mobile_admission_for("device_b"),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_b".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile_b,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_a_rx).await["kind"], "sync_ready");
        assert_eq!(receive_json(&mut mobile_b_rx).await["kind"], "sync_ready");

        let command = takeover_command();
        gateway
            .handle_mobile_command(
                &mobile_admission_for("device_a"),
                "mobile_a",
                &wrapped_command(&command),
            )
            .await
            .unwrap();
        assert!(matches!(desktop_rx.recv().await, Some(Message::Text(_))));
        gateway
            .handle_mobile_command(
                &mobile_admission_for("device_b"),
                "mobile_b",
                &wrapped_command(&command),
            )
            .await
            .unwrap();
        assert_eq!(
            receive_json(&mut mobile_b_rx).await["payload"]["error"]["code"],
            "command_failed"
        );
        assert!(matches!(
            desktop_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let session = &gateway.inner.apps.lock().await["app_a"];
        assert_eq!(
            session
                .idempotency_index
                .get(&command.idempotency_key)
                .unwrap()
                .device_id,
            "device_a"
        );
    }

    #[test]
    fn idempotency_eviction_stays_bounded_and_aligned_with_ack_retention() {
        let mut session = AppSession::new();
        let command_for = |index: usize| {
            let mut command = takeover_command();
            command.command_id = format!("cmd_{index}");
            command.idempotency_key = format!("mobile:device_a:cmd_{index}");
            command
        };

        for index in 0..ACK_CACHE_CAPACITY {
            let command = command_for(index);
            assert!(matches!(
                session.bind_idempotency("device_a", &command),
                IdempotencyDecision::New
            ));
            session.cache_ack(&command.command_id, "device_a", format!("ack_{index}"));
        }
        for index in ACK_CACHE_CAPACITY..IDEMPOTENCY_INDEX_CAPACITY {
            let command = command_for(index);
            assert!(matches!(
                session.bind_idempotency("device_a", &command),
                IdempotencyDecision::New
            ));
            session.pending_commands.insert(
                command.command_id,
                PendingCommand {
                    device_id: "device_a".into(),
                    timeout_cancel: None,
                },
            );
        }
        assert_eq!(session.acknowledgement_cache.len(), ACK_CACHE_CAPACITY);
        assert_eq!(session.pending_commands.len(), PENDING_COMMAND_CAPACITY);
        assert_eq!(session.idempotency_index.len(), IDEMPOTENCY_INDEX_CAPACITY);

        let newest = command_for(IDEMPOTENCY_INDEX_CAPACITY);
        assert!(matches!(
            session.bind_idempotency("device_a", &newest),
            IdempotencyDecision::New
        ));
        session.cache_ack(&newest.command_id, "device_a", "newest_ack".into());

        assert_eq!(session.acknowledgement_cache.len(), ACK_CACHE_CAPACITY);
        assert_eq!(session.pending_commands.len(), PENDING_COMMAND_CAPACITY);
        assert_eq!(session.idempotency_index.len(), IDEMPOTENCY_INDEX_CAPACITY);
        assert_eq!(
            session.idempotency_by_command.len(),
            IDEMPOTENCY_INDEX_CAPACITY
        );
        assert!(!session.acknowledgement_cache.contains_key("cmd_0"));
        assert!(!session.idempotency_by_command.contains_key("cmd_0"));
        assert!(!session
            .idempotency_index
            .contains_key("mobile:device_a:cmd_0"));
        assert!(session
            .idempotency_index
            .contains_key(&newest.idempotency_key));
    }

    #[tokio::test]
    async fn mobile_disconnect_keeps_pending_until_late_desktop_ack() {
        let (gateway, mut desktop_rx) = gateway_with_idle_desktop().await;
        let initial_nonce = gateway.inner.apps.lock().await["app_a"]
            .stream_nonce
            .clone();
        let (mobile, mut mobile_rx) = peer("mobile_1");
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");
        let command = takeover_command();
        let command_id = command.command_id.clone();
        gateway
            .handle_mobile_command(&mobile_admission(), "mobile_1", &wrapped_command(&command))
            .await
            .unwrap();
        assert!(matches!(desktop_rx.recv().await, Some(Message::Text(_))));

        gateway.unregister(&mobile_admission(), "mobile_1").await;
        {
            let apps = gateway.inner.apps.lock().await;
            let session = &apps["app_a"];
            assert!(session.mobiles.is_empty());
            assert!(session.pending_commands.contains_key(&command_id));
            assert_eq!(session.stream_nonce, initial_nonce);
            assert!(session.desktop.is_some());
        }
        gateway
            .route_desktop_ack(
                &desktop_admission(),
                "desktop_1",
                DesktopAck {
                    kind: "desktop_command_ack".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    payload: CommandAck {
                        command_id: command_id.clone(),
                        accepted: true,
                        error: None,
                    },
                },
            )
            .await
            .unwrap();
        let apps = gateway.inner.apps.lock().await;
        let session = &apps["app_a"];
        assert!(!session.pending_commands.contains_key(&command_id));
        assert!(session.acknowledgement_cache.contains_key(&command_id));
        assert_eq!(session.stream_nonce, initial_nonce);
        assert!(session.desktop.is_some());
        assert!(session.latest_frame.is_some());
    }

    #[tokio::test]
    async fn command_timeout_slots_are_bounded_and_ack_cancels_promptly() {
        let (gateway, mut desktop_rx) = gateway_with_idle_desktop().await;
        let (mobile, mut mobile_rx) = peer("mobile_1");
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");
        let command = takeover_command();
        let command_id = command.command_id.clone();
        gateway
            .handle_mobile_command(&mobile_admission(), "mobile_1", &wrapped_command(&command))
            .await
            .unwrap();
        assert!(matches!(desktop_rx.recv().await, Some(Message::Text(_))));

        let timeout_slots = {
            let apps = gateway.inner.apps.lock().await;
            let session = &apps["app_a"];
            assert_eq!(
                session.timeout_slots.available_permits(),
                PENDING_COMMAND_CAPACITY - 1
            );
            assert!(session.pending_commands[&command_id]
                .timeout_cancel
                .is_some());
            session.timeout_slots.clone()
        };
        gateway
            .route_desktop_ack(
                &desktop_admission(),
                "desktop_1",
                DesktopAck {
                    kind: "desktop_command_ack".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    payload: CommandAck {
                        command_id,
                        accepted: true,
                        error: None,
                    },
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while timeout_slots.available_permits() != PENDING_COMMAND_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("acknowledgement should cancel its timeout task");

        let mut permits = Vec::with_capacity(PENDING_COMMAND_CAPACITY);
        for _ in 0..PENDING_COMMAND_CAPACITY {
            permits.push(timeout_slots.clone().try_acquire_owned().unwrap());
        }
        assert!(timeout_slots.clone().try_acquire_owned().is_err());
        drop(permits);
        assert_eq!(timeout_slots.available_permits(), PENDING_COMMAND_CAPACITY);
    }

    #[tokio::test]
    async fn mobile_replay_is_queued_before_an_immediate_new_snapshot() {
        let gateway = gateway();
        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_snapshot(
                &desktop_admission(),
                "desktop_1",
                fixture_without_gateway_fields(),
            )
            .await
            .unwrap();
        let (mobile, mut mobile_rx) = peer("mobile_1");
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        gateway
            .publish_snapshot(
                &desktop_admission(),
                "desktop_1",
                fixture_without_gateway_fields(),
            )
            .await
            .unwrap();

        let first = receive_json(&mut mobile_rx).await;
        let second = receive_json(&mut mobile_rx).await;
        assert_eq!(first["payload"]["sequence"], 1);
        assert_eq!(second["payload"]["sequence"], 2);
    }

    #[tokio::test]
    async fn desktop_inbound_activity_resets_deadline_then_expiry_fences_authority() {
        let (gateway, _desktop_rx) = gateway_with_idle_desktop().await;
        let (mobile, mut mobile_rx) = peer("mobile_1");
        let mobile_fence = mobile.fenced.subscribe();
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");

        let (activity_tx, activity_rx) = watch::channel(0_u64);
        let monitor_gateway = gateway.clone();
        let monitor = tokio::spawn(async move {
            monitor_gateway
                .monitor_desktop_inbound(
                    desktop_admission(),
                    "desktop_1".into(),
                    activity_rx,
                    Duration::from_millis(200),
                )
                .await;
        });
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            activity_tx.send_modify(|generation| *generation += 1);
        }
        {
            let apps = gateway.inner.apps.lock().await;
            assert!(apps["app_a"].desktop.is_some());
            assert_eq!(apps["app_a"].mobiles.len(), 1);
        }

        tokio::time::timeout(Duration::from_secs(1), monitor)
            .await
            .expect("liveness monitor should expire")
            .expect("liveness monitor task");
        let apps = gateway.inner.apps.lock().await;
        let session = &apps["app_a"];
        assert!(session.desktop.is_none());
        assert!(session.mobiles.is_empty());
        assert!(session.latest_frame.is_none());
        drop(apps);
        assert!(*mobile_fence.borrow());
        assert!(matches!(mobile_rx.recv().await, Some(Message::Close(_))));
    }

    #[tokio::test]
    async fn desktop_disconnect_invalidates_authority_and_fences_mobiles() {
        let gateway = gateway();
        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_1",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .unwrap();
        let old_nonce = gateway.inner.apps.lock().await["app_a"]
            .stream_nonce
            .clone();

        let (mobile, mut mobile_rx) = peer("mobile_1");
        let mobile_fence = mobile.fenced.subscribe();
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");

        gateway.unregister(&desktop_admission(), "desktop_1").await;

        let apps = gateway.inner.apps.lock().await;
        let session = &apps["app_a"];
        assert!(session.desktop.is_none());
        assert!(session.mobiles.is_empty());
        assert!(session.latest_frame.is_none());
        assert!(session.latest_snapshot.is_none());
        assert!(session.replay.is_empty());
        assert!(session.pending_commands.is_empty());
        assert_eq!(session.sequence, 0);
        assert_ne!(session.stream_nonce, old_nonce);
        drop(apps);
        assert!(*mobile_fence.borrow());
        assert!(matches!(mobile_rx.recv().await, Some(Message::Close(_))));

        let (mobile, _mobile_rx) = peer("mobile_2");
        assert!(gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap_err()
            .contains("authority is not connected"));
    }

    #[tokio::test]
    async fn desktop_replacement_rejects_pending_and_starts_a_clean_epoch() {
        let gateway = gateway();
        let (desktop, mut desktop_rx) = peer("desktop_1");
        let desktop_fence = desktop.fenced.subscribe();
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_snapshot(
                &desktop_admission(),
                "desktop_1",
                fixture_without_gateway_fields(),
            )
            .await
            .unwrap();
        let old_nonce = gateway.inner.apps.lock().await["app_a"]
            .stream_nonce
            .clone();

        let (mobile, mut mobile_rx) = peer("mobile_1");
        let mobile_fence = mobile.fenced.subscribe();
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "snapshot");
        let command: Value = serde_json::from_str(include_str!(
            "../../../docs/contracts/fixtures/aokie-companion-takeover-command.v1.json"
        ))
        .unwrap();
        let command_id = command["commandId"].as_str().unwrap().to_string();
        gateway
            .handle_mobile_command(
                &mobile_admission(),
                "mobile_1",
                &json!({"kind":"command","payload":command}).to_string(),
            )
            .await
            .unwrap();
        assert!(matches!(desktop_rx.recv().await, Some(Message::Text(_))));

        let (replacement, mut replacement_rx) = peer("desktop_2");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                replacement,
            )
            .await
            .unwrap();

        let apps = gateway.inner.apps.lock().await;
        let session = &apps["app_a"];
        assert_eq!(session.desktop.as_ref().unwrap().connection_id, "desktop_2");
        assert!(session.mobiles.is_empty());
        assert!(session.latest_frame.is_none());
        assert!(session.latest_snapshot.is_none());
        assert!(session.replay.is_empty());
        assert!(session.pending_commands.is_empty());
        assert_eq!(session.sequence, 0);
        assert_ne!(session.stream_nonce, old_nonce);
        assert!(session.acknowledgement_cache.contains_key(&command_id));
        drop(apps);

        assert!(*desktop_fence.borrow());
        assert!(*mobile_fence.borrow());
        let Message::Text(rejected) = mobile_rx.recv().await.unwrap() else {
            panic!("expected pending command rejection")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&rejected).unwrap()["payload"]["error"]["code"],
            "endpoint_unreachable"
        );
        assert!(matches!(mobile_rx.recv().await, Some(Message::Close(_))));
        assert!(matches!(desktop_rx.recv().await, Some(Message::Close(_))));

        let (mobile, _mobile_rx) = peer("mobile_2");
        assert!(gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap_err()
            .contains("has not asserted"));

        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_2",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .unwrap();
        let (mobile, mut retry_rx) = peer("mobile_3");
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut retry_rx).await["kind"], "sync_ready");
        let retry_command: Value = serde_json::from_str(include_str!(
            "../../../docs/contracts/fixtures/aokie-companion-takeover-command.v1.json"
        ))
        .unwrap();
        gateway
            .handle_mobile_command(
                &mobile_admission(),
                "mobile_3",
                &json!({"kind":"command","payload":retry_command}).to_string(),
            )
            .await
            .unwrap();
        let Message::Text(retry_rejection) = retry_rx.recv().await.unwrap() else {
            panic!("expected cached pending command rejection")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&retry_rejection).unwrap()["payload"]["error"]["code"],
            "endpoint_unreachable"
        );
        assert!(matches!(
            replacement_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn oversized_resume_replay_converges_to_the_latest_snapshot() {
        let gateway = gateway();
        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        for _ in 0..(OUTBOUND_CAPACITY + 8) {
            gateway
                .publish_snapshot(
                    &desktop_admission(),
                    "desktop_1",
                    fixture_without_gateway_fields(),
                )
                .await
                .unwrap();
        }
        let apps = gateway.inner.apps.lock().await;
        let session = &apps["app_a"];
        let nonce = session.stream_nonce.clone();
        assert_eq!(session.sequence, (OUTBOUND_CAPACITY + 8) as u64);
        assert_eq!(
            session
                .frames_for_resume(Some(&nonce), session.sequence - OUTBOUND_CAPACITY as u64)
                .len(),
            OUTBOUND_CAPACITY
        );
        drop(apps);

        let (mobile, mut mobile_rx) = peer("mobile_1");
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: Some(nonce),
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        let Message::Text(latest) = mobile_rx.recv().await.unwrap() else {
            panic!("expected latest authoritative frame")
        };
        let latest: Value = serde_json::from_str(&latest).unwrap();
        assert_eq!(latest["kind"], "snapshot");
        assert_eq!(
            latest["payload"]["sequence"],
            (OUTBOUND_CAPACITY + 8) as u64
        );
    }

    #[tokio::test]
    async fn initial_delivery_failure_removes_the_mobile_registration() {
        let gateway = gateway();
        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_1",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .unwrap();
        let (mobile, mobile_rx) = peer("mobile_1");
        drop(mobile_rx);
        assert!(gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .is_err());
        assert!(gateway.inner.apps.lock().await["app_a"].mobiles.is_empty());
    }

    #[tokio::test]
    async fn full_fan_out_queue_fences_and_removes_the_mobile() {
        let gateway = gateway();
        let (desktop, _desktop_rx) = peer("desktop_1");
        gateway
            .register_desktop(
                &desktop_admission(),
                DesktopResume {
                    kind: "desktop_resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    desktop_id: "desktop_a".into(),
                },
                desktop,
            )
            .await
            .unwrap();
        gateway
            .publish_idle(
                &desktop_admission(),
                "desktop_1",
                DesktopIdle {
                    kind: "desktop_idle".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                },
            )
            .await
            .unwrap();

        let (mobile, mut mobile_rx) = peer("mobile_1");
        let mobile_tx = mobile.tx.clone();
        let mobile_fence = mobile.fenced.subscribe();
        gateway
            .register_mobile(
                &mobile_admission(),
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");
        for _ in 0..OUTBOUND_CAPACITY {
            mobile_tx.try_send(Message::Ping(Vec::new())).unwrap();
        }
        gateway
            .publish_snapshot(
                &desktop_admission(),
                "desktop_1",
                fixture_without_gateway_fields(),
            )
            .await
            .unwrap();
        assert!(*mobile_fence.borrow());
        assert!(gateway.inner.apps.lock().await["app_a"].mobiles.is_empty());
    }

    #[tokio::test]
    async fn full_direct_ack_queue_fences_and_removes_the_mobile() {
        let (gateway, _desktop_rx) = gateway_with_idle_desktop().await;
        let mut admission = mobile_admission();
        admission.grants.clear();
        let (mobile, mut mobile_rx) = peer("mobile_1");
        let mobile_tx = mobile.tx.clone();
        let mobile_fence = mobile.fenced.subscribe();
        gateway
            .register_mobile(
                &admission,
                MobileResume {
                    kind: "resume".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    device_id: "device_a".into(),
                    last_stream_nonce: None,
                    last_sequence: 0,
                },
                mobile,
            )
            .await
            .unwrap();
        assert_eq!(receive_json(&mut mobile_rx).await["kind"], "sync_ready");
        for _ in 0..OUTBOUND_CAPACITY {
            mobile_tx.try_send(Message::Ping(Vec::new())).unwrap();
        }
        gateway
            .handle_mobile_command(
                &admission,
                "mobile_1",
                &wrapped_command(&takeover_command()),
            )
            .await
            .unwrap();
        assert!(*mobile_fence.borrow());
        assert!(gateway.inner.apps.lock().await["app_a"].mobiles.is_empty());
    }
}
