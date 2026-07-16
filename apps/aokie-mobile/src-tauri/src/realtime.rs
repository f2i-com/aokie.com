use aokie_media::IceServerConfig;
use aokie_protocol::{
    CallSnapshot, CommandAck, CommandEnvelope, CommandType, SyncReadyFrame, MAX_JSON_SAFE_INTEGER,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio::time::Instant;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderValue, StatusCode},
        protocol::WebSocketConfig,
        Error as WebSocketError, Message,
    },
};
use url::Url;

use crate::managed_auth::ManagedAuthState;
use crate::media::NativeMediaState;
use crate::realtime_v2::V2State;

const MAX_REALTIME_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_PENDING_COMMANDS: usize = 8;
const COMMAND_SEND_TIMEOUT_SECS: u64 = 10;
const CONNECT_TIMEOUT_SECS: u64 = 10;
const INITIAL_SYNC_TIMEOUT_SECS: u64 = 15;
const CLIENT_PING_INTERVAL_SECS: u64 = 20;
const PONG_TIMEOUT_SECS: u64 = 10;
const INBOUND_FRESHNESS_TIMEOUT_SECS: u64 = 45;
const MAX_RECONNECT_DELAY_SECS: u64 = 20;
const RECONNECT_JITTER_DIVISOR: u64 = 4;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn initial_sync_timeout() -> Duration {
    Duration::from_secs(INITIAL_SYNC_TIMEOUT_SECS)
}

fn connect_timeout() -> Duration {
    Duration::from_secs(CONNECT_TIMEOUT_SECS)
}

fn client_ping_interval() -> Duration {
    Duration::from_secs(CLIENT_PING_INTERVAL_SECS)
}

fn pong_timeout() -> Duration {
    Duration::from_secs(PONG_TIMEOUT_SECS)
}

fn inbound_freshness_timeout() -> Duration {
    Duration::from_secs(INBOUND_FRESHNESS_TIMEOUT_SECS)
}

fn reconnect_base_delay_ms(attempt: u32) -> u64 {
    let seconds = (1_u64 << attempt.min(4)).min(MAX_RECONNECT_DELAY_SECS);
    seconds * 1_000
}

fn reconnect_jitter_window_ms(base_delay_ms: u64) -> u64 {
    let cap_ms = MAX_RECONNECT_DELAY_SECS * 1_000;
    (base_delay_ms / RECONNECT_JITTER_DIVISOR).min(cap_ms.saturating_sub(base_delay_ms))
}

fn reconnect_jitter_sample(device_id: &str, attempt: u32) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in device_id
        .as_bytes()
        .iter()
        .copied()
        .chain(attempt.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn reconnect_delay(attempt: u32, jitter_sample: u64) -> Duration {
    let base_delay_ms = reconnect_base_delay_ms(attempt);
    let jitter_window_ms = reconnect_jitter_window_ms(base_delay_ms);
    let jitter_ms = jitter_sample % (jitter_window_ms + 1);
    Duration::from_millis(base_delay_ms + jitter_ms)
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeConfig {
    pub(crate) gateway_url: String,
    pub(crate) app_id: String,
    pub(crate) device_id: String,
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) protocol_version: Option<u16>,
    #[serde(default)]
    pub(crate) session_nonce: Option<String>,
    #[serde(default)]
    pub(crate) ice_servers: Vec<IceServerConfig>,
    #[serde(default)]
    pub(crate) relay_only: bool,
    #[serde(default)]
    pub(crate) managed_deployment_id: Option<String>,
    #[serde(default)]
    pub(crate) managed_profile_id: Option<String>,
    #[serde(default)]
    last_sequence: Option<u64>,
    #[serde(default)]
    last_stream_nonce: Option<String>,
}

impl std::fmt::Debug for RealtimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeConfig")
            .field("gateway_url", &self.gateway_url)
            .field("app_id", &self.app_id)
            .field("device_id", &self.device_id)
            .field("access_token", &"[redacted]")
            .field("protocol_version", &self.protocol_version)
            .field("session_nonce", &self.session_nonce)
            .field("ice_server_count", &self.ice_servers.len())
            .field("relay_only", &self.relay_only)
            .field("managed_deployment_id", &self.managed_deployment_id)
            .field("managed_profile_id", &self.managed_profile_id)
            .field("last_sequence", &self.last_sequence)
            .field("last_stream_nonce", &self.last_stream_nonce)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TransportEvent<'a> {
    Transport { value: &'a str },
    Error { message: &'a str },
}

pub(crate) struct OutboundCommand {
    pub(crate) generation: u64,
    pub(crate) encoded: String,
    pub(crate) completion: oneshot::Sender<Result<(), String>>,
}

#[derive(Default)]
pub(crate) struct ConnectionSlot {
    pub(crate) generation: u64,
    pub(crate) sender: Option<mpsc::Sender<OutboundCommand>>,
}

#[derive(Default)]
pub struct RealtimeState {
    pub(crate) connection: Arc<Mutex<ConnectionSlot>>,
    task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    lifecycle: AsyncMutex<()>,
    pub(crate) v2: V2State,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ResumeCursor {
    stream_nonce: Option<String>,
    sequence: u64,
}

enum IncomingFrame {
    Authoritative { stream_nonce: String, sequence: u64 },
    Other,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncReadyPayload {
    stream_nonce: String,
    sequence: u64,
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn validate_config(config: &RealtimeConfig) -> Result<Url, String> {
    if !safe_id(&config.app_id) || !safe_id(&config.device_id) {
        return Err("realtime identity is invalid".into());
    }
    let managed = match (
        config.managed_deployment_id.as_deref(),
        config.managed_profile_id.as_deref(),
    ) {
        (Some(deployment_id), Some(profile_id)) => {
            if !safe_id(deployment_id) || !safe_id(profile_id) {
                return Err("managed deployment or profile identity is invalid".into());
            }
            true
        }
        (None, None) => false,
        _ => {
            return Err("managed realtime identity requires both deployment and profile IDs".into())
        }
    };
    if (!managed && (config.access_token.len() < 16 || config.access_token.len() > 16 * 1024))
        || (managed && !config.access_token.is_empty())
    {
        return Err("realtime access token is invalid".into());
    }
    if config
        .last_stream_nonce
        .as_deref()
        .is_some_and(|nonce| !safe_id(nonce))
    {
        return Err("realtime stream nonce is invalid".into());
    }
    if config
        .session_nonce
        .as_deref()
        .is_some_and(|nonce| !safe_id(nonce))
    {
        return Err("realtime session nonce is invalid".into());
    }
    IceServerConfig::validate_all(&config.ice_servers).map_err(|error| error.to_string())?;
    if config.last_sequence.unwrap_or(0) > 0 && config.last_stream_nonce.is_none() {
        return Err("realtime resume sequence requires a stream nonce".into());
    }
    if config.last_sequence.unwrap_or(0) > MAX_JSON_SAFE_INTEGER {
        return Err("realtime resume sequence is not JSON-safe".into());
    }
    let url = Url::parse(&config.gateway_url).map_err(|_| "invalid realtime URL".to_string())?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    let managed_beta_loopback = cfg!(feature = "managed-beta-local")
        && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
        && url.scheme() == "ws";
    if url.scheme() != "wss"
        && !(cfg!(debug_assertions) && loopback && url.scheme() == "ws")
        && !managed_beta_loopback
    {
        return Err("realtime URL must use wss".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err("realtime URL contains unsupported components".into());
    }
    let protocol_version = selected_protocol(config, &url)?;
    let expected_path = if protocol_version == 2 {
        "/v2/realtime"
    } else {
        "/v1/realtime"
    };
    if !url.path().ends_with(expected_path) {
        return Err(format!(
            "realtime protocol v{protocol_version} requires {expected_path}"
        ));
    }
    if protocol_version == 2
        && (config.last_sequence.unwrap_or(0) != 0 || config.last_stream_nonce.is_some())
    {
        return Err("protocol v2 does not accept a protocol-v1 resume cursor".into());
    }
    if managed && protocol_version != 2 {
        return Err("managed admission requires realtime protocol v2".into());
    }
    Ok(url)
}

pub(crate) fn selected_protocol(config: &RealtimeConfig, url: &Url) -> Result<u16, String> {
    let inferred = if url.path().ends_with("/v2/realtime") {
        2
    } else {
        1
    };
    let selected = config.protocol_version.unwrap_or(inferred);
    if !matches!(selected, 1 | 2) {
        return Err(format!("unsupported realtime protocol v{selected}"));
    }
    Ok(selected)
}

fn authorization_header(access_token: &str) -> Result<HeaderValue, String> {
    let mut value = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|_| "realtime token contains invalid bytes".to_string())?;
    value.set_sensitive(true);
    Ok(value)
}

fn url_with_cursor(base: &Url, cursor: &ResumeCursor) -> Url {
    let retained_pairs: Vec<(String, String)> = base
        .query_pairs()
        .filter(|(key, _)| key != "lastSequence" && key != "lastStreamNonce")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut url = base.clone();
    url.set_query(None);
    let mut query = url.query_pairs_mut();
    for (key, value) in retained_pairs {
        query.append_pair(&key, &value);
    }
    query.append_pair("lastSequence", &cursor.sequence.to_string());
    if let Some(stream_nonce) = &cursor.stream_nonce {
        query.append_pair("lastStreamNonce", stream_nonce);
    }
    drop(query);
    url
}

fn update_cursor(
    cursor: &Mutex<ResumeCursor>,
    stream_nonce: &str,
    sequence: u64,
    allow_stream_reset: bool,
) -> Result<ResumeCursor, String> {
    if !safe_id(stream_nonce) {
        return Err("gateway snapshot has an invalid stream nonce".into());
    }
    let mut cursor = cursor
        .lock()
        .map_err(|_| "realtime cursor is unavailable".to_string())?;
    match cursor.stream_nonce.as_deref() {
        Some(current_nonce) if current_nonce == stream_nonce => {
            if sequence < cursor.sequence {
                return Err("gateway authoritative sequence regressed within its stream".into());
            }
            cursor.sequence = sequence;
        }
        Some(_) if !allow_stream_reset => {
            return Err("gateway stream nonce changed during an active session".into());
        }
        None if !allow_stream_reset => {
            return Err("realtime active stream was not established".into());
        }
        _ => {
            cursor.stream_nonce = Some(stream_nonce.to_string());
            cursor.sequence = sequence;
        }
    }
    Ok(cursor.clone())
}

fn accept_initial_cursor(
    cursor: &Mutex<ResumeCursor>,
    stream_nonce: &str,
    sequence: u64,
) -> Result<ResumeCursor, String> {
    update_cursor(cursor, stream_nonce, sequence, true)
}

fn advance_active_cursor(
    cursor: &Mutex<ResumeCursor>,
    stream_nonce: &str,
    sequence: u64,
) -> Result<ResumeCursor, String> {
    update_cursor(cursor, stream_nonce, sequence, false)
}

fn current_cursor(cursor: &Mutex<ResumeCursor>) -> Result<ResumeCursor, String> {
    cursor
        .lock()
        .map(|cursor| cursor.clone())
        .map_err(|_| "realtime cursor is unavailable".to_string())
}

fn next_generation(generation: u64) -> u64 {
    let next = generation.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

pub(crate) fn activate_session(
    connection: &Mutex<ConnectionSlot>,
) -> Result<(u64, mpsc::Receiver<OutboundCommand>), String> {
    let (sender, receiver) = mpsc::channel(MAX_PENDING_COMMANDS);
    let mut slot = connection
        .lock()
        .map_err(|_| "realtime state is unavailable".to_string())?;
    slot.generation = next_generation(slot.generation);
    let generation = slot.generation;
    slot.sender = Some(sender);
    Ok((generation, receiver))
}

pub(crate) fn invalidate_session(
    connection: &Mutex<ConnectionSlot>,
    expected_generation: Option<u64>,
) -> Result<(), String> {
    let mut slot = connection
        .lock()
        .map_err(|_| "realtime state is unavailable".to_string())?;
    if expected_generation.is_none() || expected_generation == Some(slot.generation) {
        slot.sender = None;
        slot.generation = next_generation(slot.generation);
    }
    Ok(())
}

pub(crate) fn session_is_current(connection: &Mutex<ConnectionSlot>, generation: u64) -> bool {
    connection
        .lock()
        .map(|slot| slot.generation == generation && slot.sender.is_some())
        .unwrap_or(false)
}

pub(crate) fn emit_transport(app: &AppHandle, event: TransportEvent<'_>) {
    let _ = app.emit("aokie-companion://transport", event);
}

fn validate_snapshot(snapshot: &CallSnapshot, expected_app_id: &str) -> Result<(), String> {
    snapshot.validate().map_err(|error| error.to_string())?;
    if snapshot.app_id != expected_app_id {
        return Err("gateway snapshot belongs to a different app".into());
    }
    if !safe_id(&snapshot.stream_nonce) {
        return Err("gateway snapshot has an invalid stream nonce".into());
    }
    Ok(())
}

fn parse_sync_ready(value: Value, expected_app_id: &str) -> Result<SyncReadyFrame, String> {
    let frame: SyncReadyFrame = serde_json::from_value(value)
        .map_err(|_| "gateway sync_ready frame is invalid".to_string())?;
    frame
        .validate()
        .map_err(|error| format!("gateway sync_ready frame is invalid: {error}"))?;
    if frame.app_id != expected_app_id {
        return Err("gateway sync_ready frame belongs to a different app".into());
    }
    Ok(frame)
}

fn validate_active_stream(
    stream_nonce: &str,
    active_stream_nonce: Option<&str>,
) -> Result<(), String> {
    if active_stream_nonce.is_some_and(|active| active != stream_nonce) {
        return Err("gateway stream nonce changed during an active session".into());
    }
    Ok(())
}

fn emit_incoming(
    app: &AppHandle,
    expected_app_id: &str,
    active_stream_nonce: Option<&str>,
    text: &str,
) -> Result<IncomingFrame, String> {
    if text.len() > MAX_REALTIME_MESSAGE_BYTES {
        return Err("realtime message exceeded the size limit".into());
    }
    let value: Value =
        serde_json::from_str(text).map_err(|_| "gateway sent invalid JSON".to_string())?;
    let kind = value.get("kind").and_then(Value::as_str);
    match kind {
        Some("snapshot") => {
            let snapshot_value = value
                .get("payload")
                .cloned()
                .ok_or("snapshot payload is missing")?;
            let snapshot: CallSnapshot = serde_json::from_value(snapshot_value)
                .map_err(|_| "gateway snapshot does not match protocol v1".to_string())?;
            validate_snapshot(&snapshot, expected_app_id)?;
            validate_active_stream(&snapshot.stream_nonce, active_stream_nonce)?;
            let sequence = snapshot.sequence;
            let stream_nonce = snapshot.stream_nonce.clone();
            // Protocol v1 has no device/session field in a snapshot. The authenticated
            // socket binds that context; appId is the strongest in-frame binding available.
            app.emit("aokie-companion://snapshot", snapshot)
                .map_err(|_| "could not deliver snapshot to the local UI".to_string())?;
            Ok(IncomingFrame::Authoritative {
                stream_nonce,
                sequence,
            })
        }
        Some("sync_ready") => {
            let frame = parse_sync_ready(value, expected_app_id)?;
            validate_active_stream(&frame.stream_nonce, active_stream_nonce)?;
            let payload = SyncReadyPayload {
                stream_nonce: frame.stream_nonce.clone(),
                sequence: frame.sequence,
            };
            app.emit("aokie-companion://sync-ready", payload)
                .map_err(|_| "could not deliver sync readiness to the local UI".to_string())?;
            Ok(IncomingFrame::Authoritative {
                stream_nonce: frame.stream_nonce,
                sequence: frame.sequence,
            })
        }
        Some("command_ack") => {
            let payload = value
                .get("payload")
                .cloned()
                .ok_or("command ack payload is missing")?;
            let acknowledgement: CommandAck = serde_json::from_value(payload)
                .map_err(|_| "gateway command acknowledgement is invalid".to_string())?;
            acknowledgement
                .validate()
                .map_err(|error| error.to_string())?;
            app.emit("aokie-companion://command-ack", acknowledgement)
                .map_err(|_| "could not deliver command acknowledgement".to_string())?;
            Ok(IncomingFrame::Other)
        }
        Some("ping") => Ok(IncomingFrame::Other),
        _ if value.get("schemaVersion").is_some() && value.get("callId").is_some() => {
            let snapshot: CallSnapshot = serde_json::from_value(value)
                .map_err(|_| "gateway snapshot does not match protocol v1".to_string())?;
            validate_snapshot(&snapshot, expected_app_id)?;
            validate_active_stream(&snapshot.stream_nonce, active_stream_nonce)?;
            let sequence = snapshot.sequence;
            let stream_nonce = snapshot.stream_nonce.clone();
            // See the wrapped snapshot branch for the v1 device/session binding boundary.
            app.emit("aokie-companion://snapshot", snapshot)
                .map_err(|_| "could not deliver snapshot to the local UI".to_string())?;
            Ok(IncomingFrame::Authoritative {
                stream_nonce,
                sequence,
            })
        }
        _ => Err("gateway sent an unsupported message kind".into()),
    }
}

#[tauri::command]
pub async fn realtime_connect(
    app: AppHandle,
    state: State<'_, RealtimeState>,
    media: State<'_, NativeMediaState>,
    managed_auth: State<'_, ManagedAuthState>,
    peer_trust: State<'_, crate::peer_trust::PeerTrustState>,
    mut config: RealtimeConfig,
) -> Result<(), String> {
    inject_debug_loopback_token(&mut config)?;
    let base_url = validate_config(&config)?;
    let protocol_version = selected_protocol(&config, &base_url)?;

    if protocol_version == 2 {
        let _lifecycle = state.lifecycle.lock().await;
        disconnect_locked(&state).await?;
        media
            .bind_transport(&app, config.app_id.clone(), config.device_id.clone())
            .await?;
        let task = crate::realtime_v2::spawn(
            app,
            Arc::clone(&state.connection),
            state.v2.clone(),
            media.inner().clone(),
            managed_auth.inner().clone(),
            peer_trust.inner().clone(),
            config,
        )?;
        *state
            .task
            .lock()
            .map_err(|_| "realtime state is unavailable")? = Some(task);
        return Ok(());
    }

    // Validate all protocol-v1 authentication material before reporting a
    // successful command return.
    let authorization = authorization_header(&config.access_token)?;
    let app_header = HeaderValue::from_str(&config.app_id)
        .map_err(|_| "realtime app identity is invalid".to_string())?;
    let device_header = HeaderValue::from_str(&config.device_id)
        .map_err(|_| "realtime device identity is invalid".to_string())?;

    let app_id = config.app_id;
    let device_id = config.device_id;
    let cursor = Arc::new(Mutex::new(ResumeCursor {
        stream_nonce: config.last_stream_nonce,
        sequence: config.last_sequence.unwrap_or(0),
    }));

    let _lifecycle = state.lifecycle.lock().await;
    disconnect_locked(&state).await?;
    media
        .bind_transport(&app, app_id.clone(), device_id.clone())
        .await?;
    let connection = Arc::clone(&state.connection);
    let task_app = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        let mut reconnect_attempt = 0_u32;
        loop {
            let status = if reconnect_attempt == 0 {
                "connecting"
            } else {
                "reconnecting"
            };
            emit_transport(&task_app, TransportEvent::Transport { value: status });

            let request_cursor = match current_cursor(&cursor) {
                Ok(cursor) => cursor,
                Err(message) => {
                    emit_transport(&task_app, TransportEvent::Error { message: &message });
                    return;
                }
            };
            let request_url = url_with_cursor(&base_url, &request_cursor);
            let mut request = match request_url.as_str().into_client_request() {
                Ok(request) => request,
                Err(_) => {
                    emit_transport(
                        &task_app,
                        TransportEvent::Error {
                            message: "could not create realtime request",
                        },
                    );
                    return;
                }
            };
            request
                .headers_mut()
                .insert("authorization", authorization.clone());
            request
                .headers_mut()
                .insert("x-aokie-app-id", app_header.clone());
            request
                .headers_mut()
                .insert("x-aokie-device-id", device_header.clone());

            let websocket_config = WebSocketConfig::default()
                .max_message_size(Some(MAX_REALTIME_MESSAGE_BYTES))
                .max_frame_size(Some(MAX_REALTIME_MESSAGE_BYTES));
            match tokio::time::timeout(
                connect_timeout(),
                connect_async_with_config(request, Some(websocket_config), false),
            )
            .await
            {
                Ok(Ok((socket, _response))) => {
                    let (mut writer, mut reader) = socket.split();
                    let hello = json!({
                        "kind": "resume",
                        "schemaVersion": 1,
                        "appId": app_id,
                        "deviceId": device_id,
                        "lastStreamNonce": request_cursor.stream_nonce,
                        "lastSequence": request_cursor.sequence,
                    });
                    if !matches!(
                        tokio::time::timeout(
                            Duration::from_secs(COMMAND_SEND_TIMEOUT_SECS),
                            writer.send(Message::Text(hello.to_string().into())),
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        emit_transport(
                            &task_app,
                            TransportEvent::Error {
                                message: "realtime handshake failed",
                            },
                        );
                    } else {
                        let sync_expires_at = Instant::now() + initial_sync_timeout();
                        let sync_deadline = tokio::time::sleep_until(sync_expires_at);
                        tokio::pin!(sync_deadline);
                        let initial_sync = loop {
                            tokio::select! {
                                _ = &mut sync_deadline => {
                                    break Err("realtime authoritative sync timed out".to_string());
                                }
                                inbound = reader.next() => {
                                    match inbound {
                                        Some(Ok(Message::Text(text))) => {
                                            match emit_incoming(&task_app, &app_id, None, &text) {
                                                Ok(IncomingFrame::Authoritative { stream_nonce, sequence }) => {
                                                    match accept_initial_cursor(&cursor, &stream_nonce, sequence) {
                                                        Ok(_) => break Ok(()),
                                                        Err(message) => break Err(message),
                                                    }
                                                }
                                                Ok(IncomingFrame::Other) => {}
                                                Err(message) => break Err(message),
                                            }
                                        }
                                        Some(Ok(Message::Ping(bytes))) => {
                                            if !matches!(
                                                tokio::time::timeout_at(
                                                    sync_expires_at,
                                                    writer.send(Message::Pong(bytes)),
                                                )
                                                .await,
                                                Ok(Ok(()))
                                            ) {
                                                break Err("realtime sync heartbeat failed".into());
                                            }
                                        }
                                        Some(Ok(Message::Pong(_))) => {}
                                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                                            break Err("realtime closed before authoritative sync".into());
                                        }
                                        Some(Ok(_)) => {
                                            break Err("gateway sent an unsupported realtime frame".into());
                                        }
                                    }
                                }
                            }
                        };

                        match initial_sync {
                            Err(message) => {
                                emit_transport(
                                    &task_app,
                                    TransportEvent::Error { message: &message },
                                );
                            }
                            Ok(()) => {
                                // A successful protocol-level authoritative sync, rather than
                                // a bare TCP/TLS/WebSocket upgrade, establishes recovery.
                                reconnect_attempt = 0;
                                let active_stream_nonce =
                                    match current_cursor(&cursor).and_then(|cursor| {
                                        cursor.stream_nonce.ok_or_else(|| {
                                            "realtime authoritative sync omitted its stream nonce"
                                                .to_string()
                                        })
                                    }) {
                                        Ok(stream_nonce) => stream_nonce,
                                        Err(message) => {
                                            emit_transport(
                                                &task_app,
                                                TransportEvent::Error { message: &message },
                                            );
                                            return;
                                        }
                                    };
                                let (generation, mut receiver) = match activate_session(&connection)
                                {
                                    Ok(session) => session,
                                    Err(message) => {
                                        emit_transport(
                                            &task_app,
                                            TransportEvent::Error { message: &message },
                                        );
                                        return;
                                    }
                                };
                                emit_transport(
                                    &task_app,
                                    TransportEvent::Transport { value: "connected" },
                                );
                                let freshness = tokio::time::sleep(inbound_freshness_timeout());
                                tokio::pin!(freshness);
                                let mut client_ping = tokio::time::interval(client_ping_interval());
                                client_ping.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Delay,
                                );
                                client_ping.tick().await;
                                let pong_deadline = tokio::time::sleep(pong_timeout());
                                tokio::pin!(pong_deadline);
                                let mut awaiting_pong = false;
                                loop {
                                    tokio::select! {
                                        _ = &mut freshness => {
                                            emit_transport(
                                                &task_app,
                                                TransportEvent::Error {
                                                    message: "realtime inbound heartbeat timed out",
                                                },
                                            );
                                            break;
                                        }
                                        _ = &mut pong_deadline, if awaiting_pong => {
                                            emit_transport(
                                                &task_app,
                                                TransportEvent::Error {
                                                    message: "realtime heartbeat pong timed out",
                                                },
                                            );
                                            break;
                                        }
                                        _ = client_ping.tick(), if !awaiting_pong => {
                                            let ping_result = tokio::time::timeout(
                                                Duration::from_secs(COMMAND_SEND_TIMEOUT_SECS),
                                                writer.send(Message::Ping(Default::default())),
                                            )
                                            .await;
                                            if !matches!(ping_result, Ok(Ok(()))) {
                                                emit_transport(
                                                    &task_app,
                                                    TransportEvent::Error {
                                                        message: "realtime client heartbeat failed",
                                                    },
                                                );
                                                break;
                                            }
                                            awaiting_pong = true;
                                            pong_deadline
                                                .as_mut()
                                                .reset(Instant::now() + pong_timeout());
                                        }
                                        outbound = receiver.recv() => {
                                            let Some(outbound) = outbound else { break; };
                                            if outbound.generation != generation
                                                || !session_is_current(&connection, generation)
                                            {
                                                let _ = outbound.completion.send(Err(
                                                    "realtime session changed before command delivery".into(),
                                                ));
                                                break;
                                            }
                                            let result = tokio::time::timeout(
                                                Duration::from_secs(COMMAND_SEND_TIMEOUT_SECS),
                                                writer.send(Message::Text(outbound.encoded.into())),
                                            )
                                            .await;
                                            match result {
                                                Ok(Ok(())) => {
                                                    let _ = outbound.completion.send(Ok(()));
                                                }
                                                _ => {
                                                    let _ = outbound.completion.send(Err(
                                                        "realtime command could not be delivered".into(),
                                                    ));
                                                    break;
                                                }
                                            }
                                        }
                                        inbound = reader.next() => {
                                            match inbound {
                                                Some(Ok(Message::Text(text))) => {
                                                    freshness.as_mut().reset(
                                                        Instant::now() + inbound_freshness_timeout(),
                                                    );
                                                    match emit_incoming(
                                                        &task_app,
                                                        &app_id,
                                                        Some(&active_stream_nonce),
                                                        &text,
                                                    ) {
                                                        Ok(IncomingFrame::Authoritative {
                                                            stream_nonce,
                                                            sequence,
                                                        }) => {
                                                            if let Err(message) = advance_active_cursor(
                                                                &cursor,
                                                                &stream_nonce,
                                                                sequence,
                                                            ) {
                                                                emit_transport(
                                                                    &task_app,
                                                                    TransportEvent::Error {
                                                                        message: &message,
                                                                    },
                                                                );
                                                                break;
                                                            }
                                                        }
                                                        Ok(IncomingFrame::Other) => {}
                                                        Err(message) => {
                                                            emit_transport(
                                                                &task_app,
                                                                TransportEvent::Error {
                                                                    message: &message,
                                                                },
                                                            );
                                                            break;
                                                        }
                                                    }
                                                }
                                                Some(Ok(Message::Ping(bytes))) => {
                                                    freshness.as_mut().reset(
                                                        Instant::now() + inbound_freshness_timeout(),
                                                    );
                                                    if !matches!(
                                                        tokio::time::timeout(
                                                            Duration::from_secs(
                                                                COMMAND_SEND_TIMEOUT_SECS,
                                                            ),
                                                            writer.send(Message::Pong(bytes)),
                                                        )
                                                        .await,
                                                        Ok(Ok(()))
                                                    ) {
                                                        break;
                                                    }
                                                }
                                                Some(Ok(Message::Pong(_))) => {
                                                    freshness.as_mut().reset(
                                                        Instant::now() + inbound_freshness_timeout(),
                                                    );
                                                    awaiting_pong = false;
                                                    client_ping.reset();
                                                }
                                                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                                                Some(Ok(_)) => {
                                                    emit_transport(
                                                        &task_app,
                                                        TransportEvent::Error {
                                                            message: "gateway sent an unsupported realtime frame",
                                                        },
                                                    );
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Err(message) =
                                    invalidate_session(&connection, Some(generation))
                                {
                                    emit_transport(
                                        &task_app,
                                        TransportEvent::Error { message: &message },
                                    );
                                    return;
                                }
                            }
                        }
                    }
                }
                Ok(Err(error)) => {
                    let authentication_expired = matches!(
                        &error,
                        WebSocketError::Http(response)
                            if matches!(response.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
                    );
                    if authentication_expired {
                        emit_transport(
                            &task_app,
                            TransportEvent::Error {
                                message: "realtime authentication expired",
                            },
                        );
                        emit_transport(&task_app, TransportEvent::Transport { value: "offline" });
                        return;
                    }
                    emit_transport(
                        &task_app,
                        TransportEvent::Error {
                            message: "realtime endpoint is unavailable",
                        },
                    );
                }
                Err(_) => {
                    emit_transport(
                        &task_app,
                        TransportEvent::Error {
                            message: "realtime connection attempt timed out",
                        },
                    );
                }
            }

            reconnect_attempt = reconnect_attempt.saturating_add(1);
            emit_transport(&task_app, TransportEvent::Transport { value: "offline" });
            let delay = reconnect_delay(
                reconnect_attempt,
                reconnect_jitter_sample(&device_id, reconnect_attempt),
            );
            tokio::time::sleep(delay).await;
        }
    });
    *state
        .task
        .lock()
        .map_err(|_| "realtime state is unavailable")? = Some(task);
    Ok(())
}

fn inject_debug_loopback_token(config: &mut RealtimeConfig) -> Result<(), String> {
    if config.managed_deployment_id.is_some()
        || !config.access_token.is_empty()
        || !cfg!(debug_assertions)
    {
        return Ok(());
    }
    let url = Url::parse(&config.gateway_url).map_err(|_| "invalid realtime URL".to_string())?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if url.scheme() != "ws" || !loopback {
        return Ok(());
    }
    config.access_token = std::env::var("AOKIE_COMPANION_LOCAL_TOKEN")
        .map_err(|_| "local gateway token is not configured for this debug process".to_string())?;
    Ok(())
}

async fn disconnect_locked(state: &RealtimeState) -> Result<(), String> {
    invalidate_session(&state.connection, None)?;
    let task = state
        .task
        .lock()
        .map_err(|_| "realtime state is unavailable")?
        .take();
    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
    state.v2.reset().await;
    Ok(())
}

#[tauri::command]
pub async fn realtime_disconnect(
    app: AppHandle,
    state: State<'_, RealtimeState>,
    media: State<'_, NativeMediaState>,
) -> Result<(), String> {
    let _lifecycle = state.lifecycle.lock().await;
    let result = disconnect_locked(&state).await;
    media.clear_transport(Some(&app)).await;
    result
}

#[tauri::command]
pub async fn realtime_send(
    state: State<'_, RealtimeState>,
    command: CommandEnvelope,
) -> Result<(), String> {
    command.validate().map_err(|error| error.to_string())?;
    validate_supported_command_type(&command.command_type)?;
    let encoded = serde_json::to_string(&json!({ "kind": "command", "payload": command }))
        .map_err(|_| "could not encode realtime command".to_string())?;
    if encoded.len() > MAX_REALTIME_MESSAGE_BYTES {
        return Err("realtime command exceeded the size limit".into());
    }

    enqueue_encoded(&state, encoded).await
}

fn validate_supported_command_type(command_type: &CommandType) -> Result<(), String> {
    if matches!(command_type, CommandType::ConsultClaim) {
        Err(
            "private voice consult is unavailable until Desktop provides a verified isolated audio route"
                .into(),
        )
    } else {
        Ok(())
    }
}

pub(crate) async fn enqueue_encoded(state: &RealtimeState, encoded: String) -> Result<(), String> {
    if encoded.len() > MAX_REALTIME_MESSAGE_BYTES {
        return Err("realtime message exceeded the size limit".into());
    }
    let (generation, sender) = {
        let slot = state
            .connection
            .lock()
            .map_err(|_| "realtime state is unavailable")?;
        let sender = slot
            .sender
            .clone()
            .ok_or("realtime is offline or reconnecting")?;
        (slot.generation, sender)
    };
    let (completion, delivered) = oneshot::channel();
    let outbound = OutboundCommand {
        generation,
        encoded,
        completion,
    };
    match sender.try_send(outbound) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            return Err("realtime command capacity is exhausted".into());
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            let _ = invalidate_session(&state.connection, Some(generation));
            return Err("realtime is offline or reconnecting".into());
        }
    }

    delivered
        .await
        .map_err(|_| "realtime session ended before command delivery".to_string())?
}

#[cfg(test)]
mod tests {
    use super::{
        accept_initial_cursor, activate_session, advance_active_cursor, authorization_header,
        client_ping_interval, connect_timeout, current_cursor, inbound_freshness_timeout,
        initial_sync_timeout, invalidate_session, parse_sync_ready, pong_timeout,
        reconnect_base_delay_ms, reconnect_delay, reconnect_jitter_sample,
        reconnect_jitter_window_ms, session_is_current, url_with_cursor, validate_active_stream,
        validate_config, validate_snapshot, validate_supported_command_type, ConnectionSlot,
        OutboundCommand, RealtimeConfig, RealtimeState, ResumeCursor, MAX_PENDING_COMMANDS,
        MAX_RECONNECT_DELAY_SECS,
    };
    use aokie_protocol::{
        CallSnapshot, CommandAck, CommandError, CommandType, ProtocolErrorCode,
        MAX_JSON_SAFE_INTEGER,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use std::time::Duration;
    use tokio::sync::oneshot;
    use url::Url;

    #[test]
    fn remote_realtime_requires_wss_and_redacts_tokens_from_debug() {
        let secure = RealtimeConfig {
            gateway_url: "wss://realtime.example.com/v1/realtime".into(),
            app_id: "app_test".into(),
            device_id: "device_test".into(),
            access_token: "secret-token-value".into(),
            protocol_version: None,
            session_nonce: None,
            ice_servers: Vec::new(),
            relay_only: false,
            managed_deployment_id: None,
            managed_profile_id: None,
            last_sequence: Some(42),
            last_stream_nonce: Some("stream-test".into()),
        };
        assert!(validate_config(&secure).is_ok());
        assert!(authorization_header(&secure.access_token).is_ok());
        assert!(!format!("{secure:?}").contains("secret-token-value"));

        let unscoped_resume = RealtimeConfig {
            last_stream_nonce: None,
            ..secure.clone()
        };
        assert!(validate_config(&unscoped_resume).is_err());

        let insecure = RealtimeConfig {
            gateway_url: "ws://realtime.example.com/v1/realtime".into(),
            ..secure
        };
        assert!(validate_config(&insecure).is_err());
    }

    #[test]
    fn managed_realtime_requires_paired_profile_and_deployment_bindings() {
        let managed = RealtimeConfig {
            gateway_url: "wss://issuer-a.example/v2/realtime".into(),
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            access_token: String::new(),
            protocol_version: Some(2),
            session_nonce: None,
            ice_servers: Vec::new(),
            relay_only: false,
            managed_deployment_id: Some("deployment_shared".into()),
            managed_profile_id: Some("profile_issuer_a_deployment_shared_app_a".into()),
            last_sequence: None,
            last_stream_nonce: None,
        };
        assert!(validate_config(&managed).is_ok());

        let missing_profile = RealtimeConfig {
            managed_profile_id: None,
            ..managed.clone()
        };
        assert!(validate_config(&missing_profile).is_err());

        let profile_without_deployment = RealtimeConfig {
            managed_deployment_id: None,
            ..managed.clone()
        };
        assert!(validate_config(&profile_without_deployment).is_err());

        let malformed_profile = RealtimeConfig {
            managed_profile_id: Some("profile with spaces".into()),
            ..managed
        };
        assert!(validate_config(&malformed_profile).is_err());
    }

    #[test]
    fn private_consult_claim_is_locked_but_safe_release_remains_available() {
        assert!(validate_supported_command_type(&CommandType::ConsultClaim).is_err());
        assert!(validate_supported_command_type(&CommandType::ConsultEnd).is_ok());
    }

    #[test]
    fn resume_url_replaces_stale_nonce_scoped_cursor_and_preserves_other_query_fields() {
        let base = Url::parse(
            "wss://realtime.example.com/v1/realtime?tenant=acme&lastSequence=12&lastStreamNonce=old",
        )
        .expect("base URL parses");
        let resumed = url_with_cursor(
            &base,
            &ResumeCursor {
                stream_nonce: Some("stream-new".into()),
                sequence: 84,
            },
        );
        let pairs: Vec<_> = resumed.query_pairs().collect();
        assert!(pairs
            .iter()
            .any(|pair| pair == &("tenant".into(), "acme".into())));
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key == "lastSequence")
                .map(|(_, value)| value.as_ref())
                .collect::<Vec<_>>(),
            vec!["84"]
        );
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key == "lastStreamNonce")
                .map(|(_, value)| value.as_ref())
                .collect::<Vec<_>>(),
            vec!["stream-new"]
        );
    }

    #[test]
    fn initial_sync_can_reset_cursor_but_active_stream_nonce_change_is_fatal() {
        let cursor = Mutex::new(ResumeCursor {
            stream_nonce: Some("stream-a".into()),
            sequence: 84,
        });
        assert!(advance_active_cursor(&cursor, "stream-a", 80).is_err());
        assert_eq!(current_cursor(&cursor).expect("cursor reads").sequence, 84);
        advance_active_cursor(&cursor, "stream-a", 90).expect("same stream cursor advances");
        assert_eq!(current_cursor(&cursor).expect("cursor reads").sequence, 90);

        assert!(advance_active_cursor(&cursor, "stream-b", 2).is_err());
        assert!(validate_active_stream("stream-b", Some("stream-a")).is_err());
        assert_eq!(
            current_cursor(&cursor)
                .expect("cursor remains fenced")
                .stream_nonce
                .as_deref(),
            Some("stream-a")
        );

        let replaced = accept_initial_cursor(&cursor, "stream-b", 2)
            .expect("a new initial sync may replace an old stream cursor");
        assert_eq!(replaced.stream_nonce.as_deref(), Some("stream-b"));
        assert_eq!(replaced.sequence, 2);
    }

    #[test]
    fn sync_ready_is_strict_typed_json_safe_and_app_bound() {
        let valid = json!({
            "kind": "sync_ready",
            "schemaVersion": 1,
            "appId": "app_test",
            "streamNonce": "stream-test",
            "sequence": 42,
        });
        let parsed = parse_sync_ready(valid.clone(), "app_test").expect("sync_ready validates");
        assert_eq!(parsed.stream_nonce, "stream-test");
        assert_eq!(parsed.sequence, 42);

        assert!(parse_sync_ready(valid.clone(), "app_other").is_err());

        let mut unknown_field = valid.clone();
        unknown_field["unexpected"] = json!(true);
        assert!(parse_sync_ready(unknown_field, "app_test").is_err());

        let mut unsafe_nonce = valid.clone();
        unsafe_nonce["streamNonce"] = json!("unsafe nonce");
        assert!(parse_sync_ready(unsafe_nonce, "app_test").is_err());

        let mut unsafe_sequence = valid.clone();
        unsafe_sequence["sequence"] = json!(MAX_JSON_SAFE_INTEGER + 1);
        assert!(parse_sync_ready(unsafe_sequence, "app_test").is_err());

        let mut wrong_type = valid;
        wrong_type["sequence"] = json!("42");
        assert!(parse_sync_ready(wrong_type, "app_test").is_err());

        let wrong_kind = json!({
            "kind": "ready",
            "schemaVersion": 1,
            "appId": "app_test",
            "streamNonce": "stream-test",
            "sequence": 42,
        });
        assert!(parse_sync_ready(wrong_kind, "app_test").is_err());

        let wrong_schema = json!({
            "kind": "sync_ready",
            "schemaVersion": 2,
            "appId": "app_test",
            "streamNonce": "stream-test",
            "sequence": 42,
        });
        assert!(parse_sync_ready(wrong_schema, "app_test").is_err());
    }

    #[test]
    fn session_invalidation_closes_the_old_bounded_queue() {
        let connection = Mutex::new(ConnectionSlot::default());
        let (generation, mut receiver) = activate_session(&connection).expect("session activates");
        invalidate_session(&connection, Some(generation)).expect("session invalidates");
        assert!(receiver.try_recv().is_err());
        assert!(connection.lock().expect("connection lock").sender.is_none());
    }

    #[test]
    fn pre_sync_state_has_no_available_command_sender() {
        let state = RealtimeState::default();
        let generation = {
            let slot = state.connection.lock().expect("connection lock");
            assert!(slot.sender.is_none());
            slot.generation
        };
        assert!(!session_is_current(&state.connection, generation));
    }

    #[test]
    fn connect_sync_and_heartbeat_timings_are_bounded() {
        assert!(connect_timeout() > Duration::ZERO);
        assert!(connect_timeout() <= initial_sync_timeout());
        assert!(initial_sync_timeout() > Duration::ZERO);
        assert!(pong_timeout() < client_ping_interval());
        assert!(client_ping_interval() < inbound_freshness_timeout());
        assert!(inbound_freshness_timeout() > initial_sync_timeout());
    }

    #[test]
    fn reconnect_backoff_has_deterministic_bounded_jitter() {
        assert_eq!(reconnect_base_delay_ms(0), 1_000);
        assert_eq!(reconnect_base_delay_ms(1), 2_000);
        assert_eq!(reconnect_base_delay_ms(4), 16_000);
        assert_eq!(reconnect_base_delay_ms(u32::MAX), 16_000);
        assert_eq!(reconnect_jitter_window_ms(2_000), 500);
        assert_eq!(reconnect_jitter_window_ms(16_000), 4_000);

        assert_eq!(reconnect_delay(1, 0), Duration::from_millis(2_000));
        assert_eq!(reconnect_delay(1, 500), Duration::from_millis(2_500));
        assert_eq!(reconnect_delay(1, 501), Duration::from_millis(2_000));
        assert_eq!(
            reconnect_delay(u32::MAX, 4_000),
            Duration::from_secs(MAX_RECONNECT_DELAY_SECS)
        );

        let sample = reconnect_jitter_sample("device_test", 3);
        assert_eq!(sample, reconnect_jitter_sample("device_test", 3));
        assert_ne!(sample, reconnect_jitter_sample("device_other", 3));
        assert_ne!(sample, reconnect_jitter_sample("device_test", 4));

        for attempt in 0..=32 {
            let delay = reconnect_delay(attempt, reconnect_jitter_sample("device_test", attempt));
            assert!(delay >= Duration::from_millis(reconnect_base_delay_ms(attempt)));
            assert!(delay <= Duration::from_secs(MAX_RECONNECT_DELAY_SECS));
        }
    }

    #[tokio::test]
    async fn lifecycle_mutex_serializes_superseding_operations() {
        let state = Arc::new(RealtimeState::default());
        let first = state.lifecycle.lock().await;
        let acquired = Arc::new(AtomicBool::new(false));
        let second_state = Arc::clone(&state);
        let second_acquired = Arc::clone(&acquired);
        let (attempted, started) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = attempted.send(());
            let _guard = second_state.lifecycle.lock().await;
            second_acquired.store(true, Ordering::Release);
        });

        started.await.expect("second operation starts");
        tokio::task::yield_now().await;
        assert!(!acquired.load(Ordering::Acquire));
        drop(first);
        task.await.expect("second operation completes");
        assert!(acquired.load(Ordering::Acquire));
    }

    #[test]
    fn stale_sender_cannot_cross_a_session_generation() {
        let connection = Mutex::new(ConnectionSlot::default());
        let (generation, mut receiver) = activate_session(&connection).expect("session activates");
        let stale_sender = connection
            .lock()
            .expect("connection lock")
            .sender
            .clone()
            .expect("active sender");
        invalidate_session(&connection, Some(generation)).expect("session invalidates");

        let (completion, _delivered) = oneshot::channel();
        stale_sender
            .try_send(OutboundCommand {
                generation,
                encoded: "{}".into(),
                completion,
            })
            .expect("a stale clone can only reach its old session queue");
        let stale = receiver
            .try_recv()
            .expect("old receiver observes stale item");
        assert_eq!(stale.generation, generation);
        assert!(!session_is_current(&connection, stale.generation));
    }

    #[test]
    fn outbound_queue_has_a_hard_capacity() {
        let connection = Mutex::new(ConnectionSlot::default());
        let (generation, _receiver) = activate_session(&connection).expect("session activates");
        let sender = connection
            .lock()
            .expect("connection lock")
            .sender
            .clone()
            .expect("active sender");
        for _ in 0..MAX_PENDING_COMMANDS {
            let (completion, _delivered) = oneshot::channel();
            sender
                .try_send(OutboundCommand {
                    generation,
                    encoded: "{}".into(),
                    completion,
                })
                .expect("queue has remaining capacity");
        }
        let (completion, _delivered) = oneshot::channel();
        assert!(sender
            .try_send(OutboundCommand {
                generation,
                encoded: "{}".into(),
                completion,
            })
            .is_err());
    }

    #[test]
    fn authoritative_snapshots_are_bound_to_the_configured_app() {
        let snapshot: CallSnapshot = serde_json::from_str(include_str!(
            "../../../../docs/contracts/fixtures/aokie-companion-live-snapshot.v1.json"
        ))
        .expect("canonical snapshot parses");
        assert!(validate_snapshot(&snapshot, "app_coastal_auto").is_ok());
        assert!(validate_snapshot(&snapshot, "app_other").is_err());
    }

    #[test]
    fn command_acknowledgements_are_consistent_and_typed() {
        let accepted = CommandAck {
            command_id: "cmd-1".into(),
            accepted: true,
            error: None,
        };
        assert!(accepted.validate().is_ok());

        let rejected = CommandAck {
            command_id: "cmd-2".into(),
            accepted: false,
            error: Some(CommandError {
                code: ProtocolErrorCode::StaleOwnerEpoch,
                message: "ownership changed".into(),
            }),
        };
        assert!(rejected.validate().is_ok());

        let inconsistent = CommandAck {
            command_id: "cmd-3".into(),
            accepted: true,
            error: rejected.error,
        };
        assert!(inconsistent.validate().is_err());
    }
}
