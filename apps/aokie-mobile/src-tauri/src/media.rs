use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "windows")]
use std::collections::HashSet;

use aokie_media::{
    CompanionPeer, IceCandidateSignal, IceServerConfig, MediaError, MediaMode, PeerEvent,
    PeerOptions, SdpSignal, SessionBinding,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{broadcast, Mutex, RwLock};

#[cfg(target_os = "android")]
use tauri::Manager;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_SESSION_LIFETIME: Duration = Duration::from_secs(5 * 60);
const PEER_EVENT_POLL: Duration = Duration::from_millis(50);
const MICROPHONE_PERMISSION_POLL: Duration = Duration::from_millis(500);
const LOCAL_PROOF_INTERVAL: Duration = Duration::from_secs(5);
const LOCAL_PROOF_TTL: chrono::Duration = chrono::Duration::milliseconds(7_500);
#[cfg(target_os = "windows")]
const SYSTEM_DEFAULT_AUDIO_DEVICE_ID: &str = "system_default";
#[cfg(not(target_os = "windows"))]
const SYSTEM_MANAGED_AUDIO_DEVICE_ID: &str = "system_managed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSession {
    pub app_id: String,
    pub stream_nonce: String,
    #[serde(flatten)]
    pub binding: SessionBinding,
    pub sdp_revision: u64,
    pub transport_generation: u64,
    pub expires_at: DateTime<Utc>,
}

impl MediaSession {
    fn validate(&self, transport: &TransportIdentity) -> Result<Duration, String> {
        if !safe_id(&self.app_id) || !safe_id(&self.stream_nonce) {
            return Err("native media authority contains an invalid identity".into());
        }
        self.binding.validate().map_err(|error| error.to_string())?;
        if self.app_id != transport.app_id || self.binding.device_id != transport.device_id {
            return Err("native media authority does not match the authenticated transport".into());
        }
        if self.sdp_revision == 0
            || self.sdp_revision > MAX_SAFE_INTEGER
            || self.transport_generation == 0
            || self.transport_generation > MAX_SAFE_INTEGER
        {
            return Err("native media revision is invalid".into());
        }
        remaining_lifetime(self.expires_at)
    }

    fn same_peer(&self, other: &Self) -> bool {
        self.app_id == other.app_id
            && self.stream_nonce == other.stream_nonce
            && self.binding == other.binding
            && self.sdp_revision == other.sdp_revision
            && self.transport_generation == other.transport_generation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TransportIdentity {
    app_id: String,
    device_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateOfferRequest {
    pub session: MediaSession,
    #[serde(default)]
    pub ice_servers: Vec<IceServerConfig>,
    #[serde(default)]
    pub relay_only: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOfferResponse {
    pub session: MediaSession,
    pub offer: SdpSignal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptAnswerRequest {
    pub session: MediaSession,
    pub answer: SdpSignal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddIceRequest {
    pub session: MediaSession,
    pub candidate: IceCandidateSignal,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRequest {
    pub session: MediaSession,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RevokeRequest {
    pub session: MediaSession,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectAudioDevicesRequest {
    pub input_id: String,
    pub output_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAudioDevice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAudioDevices {
    pub routing_policy: &'static str,
    pub input_devices: Vec<NativeAudioDevice>,
    pub output_devices: Vec<NativeAudioDevice>,
    pub selected_input_id: String,
    pub selected_output_id: String,
    pub state: &'static str,
    pub can_select: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MediaStatusEvent {
    pub(crate) session: MediaSession,
    pub(crate) phase: &'static str,
    pub(crate) microphone_active: bool,
    pub(crate) remote_audio_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LocalSignal {
    Offer { description: SdpSignal },
    Ice { candidate: IceCandidateSignal },
    IceComplete,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MediaSignalEvent {
    pub(crate) session: MediaSession,
    pub(crate) signal: LocalSignal,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalMediaProof {
    app_id: String,
    stream_nonce: String,
    rtc_session_id: String,
    call_id: String,
    call_epoch: u64,
    owner_epoch: u64,
    device_id: String,
    lease_id: String,
    mode: MediaMode,
    fence: u64,
    sdp_revision: u64,
    transport_generation: u64,
    expires_at: String,
    active: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveTransitionEvent {
    session: MediaSession,
    occurred_at: String,
}

#[derive(Default)]
struct ConnectionEvidence {
    connected: AtomicBool,
    remote_audio_ready: AtomicBool,
    microphone_active: AtomicBool,
    proof_active: AtomicBool,
    live_active: AtomicBool,
}

#[derive(Clone)]
struct ActiveMedia {
    generation: u64,
    session: Arc<RwLock<MediaSession>>,
    peer: Arc<Mutex<CompanionPeer>>,
    offer: SdpSignal,
    evidence: Arc<ConnectionEvidence>,
    signal_tx: broadcast::Sender<MediaSignalEvent>,
}

#[derive(Default)]
struct MediaSlot {
    transport: Option<TransportIdentity>,
    active: Option<ActiveMedia>,
    audio_selection: AudioSelection,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AudioSelection {
    input_id: Option<String>,
    output_id: Option<String>,
}

struct NativeMediaInner {
    slot: Mutex<MediaSlot>,
    lifecycle: Mutex<()>,
    next_generation: AtomicU64,
    platform_ready: AtomicBool,
    platform_init: Mutex<()>,
    signal_tx: broadcast::Sender<MediaSignalEvent>,
}

#[derive(Clone)]
pub struct NativeMediaState {
    inner: Arc<NativeMediaInner>,
}

impl Default for NativeMediaState {
    fn default() -> Self {
        let (signal_tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(NativeMediaInner {
                slot: Mutex::new(MediaSlot::default()),
                lifecycle: Mutex::new(()),
                next_generation: AtomicU64::new(0),
                platform_ready: AtomicBool::new(!cfg!(target_os = "android")),
                platform_init: Mutex::new(()),
                signal_tx,
            }),
        }
    }
}

impl NativeMediaState {
    pub(crate) fn subscribe_signals(&self) -> broadcast::Receiver<MediaSignalEvent> {
        self.inner.signal_tx.subscribe()
    }

    pub async fn ensure_platform_ready(&self, app: &AppHandle) -> bool {
        if self.inner.platform_ready.load(Ordering::Acquire) {
            return true;
        }
        let _guard = self.inner.platform_init.lock().await;
        if self.inner.platform_ready.load(Ordering::Acquire) {
            return true;
        }
        let ready = initialize_platform_audio(app).await.unwrap_or(false);
        self.inner.platform_ready.store(ready, Ordering::Release);
        ready
    }

    pub async fn bind_transport(
        &self,
        app: &AppHandle,
        app_id: String,
        device_id: String,
    ) -> Result<(), String> {
        if !safe_id(&app_id) || !safe_id(&device_id) {
            return Err("native media transport identity is invalid".into());
        }
        let _lifecycle = self.inner.lifecycle.lock().await;
        let replacement = TransportIdentity { app_id, device_id };
        let old = {
            let mut slot = self.inner.slot.lock().await;
            if slot.transport.as_ref() == Some(&replacement) {
                return Ok(());
            }
            slot.transport = Some(replacement);
            slot.active.take()
        };
        if let Some(active) = old {
            close_peer(
                Some(app),
                active,
                "transport_rebound",
                "authenticated transport changed",
            )
            .await;
        }
        Ok(())
    }

    pub async fn clear_transport(&self, app: Option<&AppHandle>) {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let old = {
            let mut slot = self.inner.slot.lock().await;
            slot.transport = None;
            slot.active.take()
        };
        if let Some(active) = old {
            close_peer(app, active, "closed", "authenticated transport closed").await;
        } else if let Some(app) = app {
            emit_local_proof_reset(app);
        }
    }

    async fn active_for(&self, expected: &MediaSession) -> Result<ActiveMedia, String> {
        let active = self
            .inner
            .slot
            .lock()
            .await
            .active
            .clone()
            .ok_or("native media peer is not open")?;
        let current = active.session.read().await;
        if &*current != expected {
            return Err("native media message is stale for the active peer".into());
        }
        remaining_lifetime(current.expires_at)?;
        drop(current);
        Ok(active)
    }

    async fn close_if_current(
        &self,
        app: &AppHandle,
        generation: u64,
        phase: &'static str,
        reason: &'static str,
    ) {
        let active = {
            let mut slot = self.inner.slot.lock().await;
            if slot
                .active
                .as_ref()
                .is_some_and(|active| active.generation == generation)
            {
                slot.active.take()
            } else {
                None
            }
        };
        if let Some(active) = active {
            close_peer(Some(app), active, phase, reason).await;
        }
    }
}

#[tauri::command]
pub async fn media_create_offer(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
    request: CreateOfferRequest,
) -> Result<CreateOfferResponse, String> {
    create_offer(&app, state.inner(), request).await
}

#[tauri::command]
pub async fn media_get_audio_devices(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
) -> Result<NativeAudioDevices, String> {
    get_audio_devices(&app, state.inner()).await
}

#[tauri::command]
pub async fn media_select_audio_devices(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
    request: SelectAudioDevicesRequest,
) -> Result<NativeAudioDevices, String> {
    select_audio_devices(&app, state.inner(), request).await
}

pub(crate) async fn get_audio_devices(
    app: &AppHandle,
    state: &NativeMediaState,
) -> Result<NativeAudioDevices, String> {
    let _lifecycle = state.inner.lifecycle.lock().await;
    let (active, selection) = {
        let slot = state.inner.slot.lock().await;
        (slot.active.clone(), slot.audio_selection.clone())
    };
    let route_state = match active.as_ref() {
        Some(active) if active.evidence.microphone_active.load(Ordering::Acquire) => {
            "microphone_active"
        }
        Some(_) => "media_active",
        None => "idle",
    };

    #[cfg(target_os = "windows")]
    {
        let _ = app;
        let devices = if let Some(active) = active {
            active
                .peer
                .lock()
                .await
                .audio_devices()
                .map_err(|error| error.to_string())?
        } else {
            aokie_media::enumerate_platform_audio_devices().map_err(|error| error.to_string())?
        };
        return Ok(build_selectable_audio_devices(
            devices,
            &selection,
            route_state,
        ));
    }

    #[cfg(target_os = "android")]
    {
        let _ = selection;
        return android_audio_devices(
            crate::android_runtime::audio_routes(app).await?,
            route_state,
        );
    }

    #[cfg(not(any(target_os = "windows", target_os = "android")))]
    {
        let _ = app;
        let _ = selection;
        Ok(NativeAudioDevices {
            routing_policy: "system_managed",
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            selected_input_id: SYSTEM_MANAGED_AUDIO_DEVICE_ID.into(),
            selected_output_id: SYSTEM_MANAGED_AUDIO_DEVICE_ID.into(),
            state: route_state,
            can_select: false,
        })
    }
}

pub(crate) async fn select_audio_devices(
    app: &AppHandle,
    state: &NativeMediaState,
    request: SelectAudioDevicesRequest,
) -> Result<NativeAudioDevices, String> {
    validate_audio_device_id(&request.input_id)?;
    validate_audio_device_id(&request.output_id)?;

    #[cfg(target_os = "android")]
    {
        if request.input_id != request.output_id {
            return Err(
                "Android communication routes bind microphone and playout to one system device"
                    .into(),
            );
        }
        let route_state = {
            let slot = state.inner.slot.lock().await;
            let active = slot
                .active
                .as_ref()
                .ok_or("communication routes can change only during an active media session")?;
            if active.evidence.microphone_active.load(Ordering::Acquire) {
                "microphone_active"
            } else {
                "media_active"
            }
        };
        return android_audio_devices(
            crate::android_runtime::select_audio_route(app, &request.output_id).await?,
            route_state,
        );
    }

    #[cfg(not(any(target_os = "windows", target_os = "android")))]
    {
        let _ = (app, state);
        return Err(
            "audio routing is managed by the operating-system call session on this platform".into(),
        );
    }

    #[cfg(target_os = "windows")]
    {
        let _ = app;
        let _lifecycle = state.inner.lifecycle.lock().await;
        if state.inner.slot.lock().await.active.is_some() {
            return Err(
                "audio endpoints can only change while native media is idle; leave the current audio session first"
                    .into(),
            );
        }
        let devices =
            aokie_media::enumerate_platform_audio_devices().map_err(|error| error.to_string())?;
        let selection = AudioSelection {
            input_id: resolve_audio_selection(
                &request.input_id,
                devices.recording.iter().map(|device| device.guid.as_str()),
                "microphone",
            )?,
            output_id: resolve_audio_selection(
                &request.output_id,
                devices.playout.iter().map(|device| device.guid.as_str()),
                "speaker",
            )?,
        };
        state.inner.slot.lock().await.audio_selection = selection.clone();
        Ok(build_selectable_audio_devices(devices, &selection, "idle"))
    }
}

pub(crate) async fn create_offer(
    app: &AppHandle,
    state: &NativeMediaState,
    request: CreateOfferRequest,
) -> Result<CreateOfferResponse, String> {
    if !state.ensure_platform_ready(app).await {
        return Err("native WebRTC platform audio initialization failed".into());
    }
    let _lifecycle = state.inner.lifecycle.lock().await;
    let (transport, audio_selection) = {
        let slot = state.inner.slot.lock().await;
        (
            slot.transport
                .clone()
                .ok_or("native media requires an authenticated realtime transport")?,
            slot.audio_selection.clone(),
        )
    };
    request.session.validate(&transport)?;

    let old = {
        let mut slot = state.inner.slot.lock().await;
        if let Some(active) = slot.active.as_ref() {
            let current = active.session.read().await;
            if *current == request.session {
                return Ok(CreateOfferResponse {
                    session: current.clone(),
                    offer: active.offer.clone(),
                });
            }
            validate_replacement(&current, &request.session)?;
        }
        slot.active.take()
    };
    if let Some(active) = old {
        close_peer(
            Some(app),
            active,
            "replaced",
            "a newer media authority replaced this peer",
        )
        .await;
    }

    let options = PeerOptions {
        ice_servers: request.ice_servers,
        relay_only: request.relay_only,
        recording_device_guid: audio_selection.input_id,
        playout_device_guid: audio_selection.output_id,
        ..PeerOptions::default()
    };
    crate::android_runtime::begin_communication_audio(app).await?;
    let (peer, offer) = match CompanionPeer::offer(request.session.binding.clone(), options).await {
        Ok(opened) => opened,
        Err(error) => {
            let _ = crate::android_runtime::end_communication_audio(app).await;
            return Err(error.to_string());
        }
    };
    let generation = next_generation(&state.inner.next_generation);
    let active = ActiveMedia {
        generation,
        session: Arc::new(RwLock::new(request.session.clone())),
        peer: Arc::new(Mutex::new(peer)),
        offer: offer.clone(),
        evidence: Arc::new(ConnectionEvidence::default()),
        signal_tx: state.inner.signal_tx.clone(),
    };
    {
        let mut slot = state.inner.slot.lock().await;
        if slot.transport.as_ref() != Some(&transport) {
            drop(slot);
            close_peer(None, active, "closed", "authenticated transport changed").await;
            return Err("authenticated transport changed while media was opening".into());
        }
        slot.active = Some(active.clone());
    }

    emit_status(app, &active, "offer_created", None).await;
    emit_signal(
        app,
        &active,
        LocalSignal::Offer {
            description: offer.clone(),
        },
    )
    .await;
    let watcher_state = state.clone();
    let watcher_app = app.clone();
    tauri::async_runtime::spawn(async move {
        watch_peer(watcher_state, watcher_app, active).await;
    });

    Ok(CreateOfferResponse {
        session: request.session,
        offer,
    })
}

#[tauri::command]
pub async fn media_accept_answer(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
    request: AcceptAnswerRequest,
) -> Result<(), String> {
    accept_answer(&app, state.inner(), request).await
}

pub(crate) async fn accept_answer(
    app: &AppHandle,
    state: &NativeMediaState,
    request: AcceptAnswerRequest,
) -> Result<(), String> {
    let active = state.active_for(&request.session).await?;
    active
        .peer
        .lock()
        .await
        .accept_answer(request.answer)
        .await
        .map_err(|error| error.to_string())?;
    emit_status(app, &active, "answer_applied", None).await;
    Ok(())
}

#[tauri::command]
pub async fn media_add_ice_candidate(
    state: State<'_, NativeMediaState>,
    request: AddIceRequest,
) -> Result<(), String> {
    add_ice_candidate(state.inner(), request).await
}

pub(crate) async fn add_ice_candidate(
    state: &NativeMediaState,
    request: AddIceRequest,
) -> Result<(), String> {
    let active = state.active_for(&request.session).await?;
    let result = active
        .peer
        .lock()
        .await
        .add_remote_candidate(request.candidate)
        .await
        .map_err(|error| error.to_string());
    result
}

#[tauri::command]
pub async fn media_arm_microphone(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
    request: SessionRequest,
) -> Result<(), String> {
    // This check deliberately precedes every Android permission operation.
    // A receive-only monitor can never create a mic track or open a prompt.
    if !request.session.binding.mode.needs_microphone() {
        return Err("monitor media is receive-only and cannot request the microphone".into());
    }
    let active = state.active_for(&request.session).await?;
    if request.session.binding.mode == MediaMode::Talk {
        // A fenced native Talk session is the first authoritative winner.
        // Reconcile the pre-WebView Android call surface before any permission
        // prompt or track can open. Foreground calls without a push offer
        // simply return false and continue through the same media path.
        crate::android_runtime::reconcile_offer(
            &app,
            &request.session.binding.call_id,
            request.session.binding.call_epoch,
            "won",
            "authoritative_talk_lease",
        )
        .await?;
    }
    if !active.peer.lock().await.is_connected() {
        emit_status(
            &app,
            &active,
            "connecting",
            Some("WebRTC is still connecting; microphone arm will be retried".into()),
        )
        .await;
        return Err("native WebRTC is still connecting; retry microphone arm".into());
    }
    if let Err(message) = ensure_microphone_permission(&app).await {
        state
            .close_if_current(
                &app,
                active.generation,
                "revoked",
                "microphone permission was denied",
            )
            .await;
        return Err(message);
    }
    let lifetime = remaining_lifetime(request.session.expires_at)?;
    let arm_result = {
        let peer = active.peer.lock().await;
        if !peer.is_connected() {
            drop(peer);
            emit_status(
                &app,
                &active,
                "connecting",
                Some("WebRTC is still connecting; microphone arm will be retried".into()),
            )
            .await;
            return Err("native WebRTC is still connecting; retry microphone arm".into());
        }
        peer.arm_microphone(&request.session.binding, lifetime)
    };
    if let Err(error) = arm_result {
        if matches!(
            error,
            MediaError::UnsafeTransition("microphone cannot open before WebRTC is connected")
        ) {
            emit_status(
                &app,
                &active,
                "connecting",
                Some("WebRTC is still connecting; microphone arm will be retried".into()),
            )
            .await;
            return Err("native WebRTC is still connecting; retry microphone arm".into());
        }
        state
            .close_if_current(
                &app,
                active.generation,
                "failed",
                "native microphone track could not be armed",
            )
            .await;
        return Err(error.to_string());
    }
    active
        .evidence
        .microphone_active
        .store(true, Ordering::Release);
    emit_status(&app, &active, "microphone_armed", None).await;
    Ok(())
}

#[tauri::command]
pub async fn media_disarm_microphone(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
    request: SessionRequest,
) -> Result<(), String> {
    let active = state.active_for(&request.session).await?;
    active.peer.lock().await.disarm_microphone();
    active
        .evidence
        .microphone_active
        .store(false, Ordering::Release);
    active.evidence.live_active.store(false, Ordering::Release);
    reset_proof_if_active(&app, &active.evidence);
    emit_status(&app, &active, "microphone_disarmed", None).await;
    Ok(())
}

#[tauri::command]
pub async fn media_renew_lease(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
    request: SessionRequest,
) -> Result<(), String> {
    renew_lease(&app, state.inner(), request).await
}

pub(crate) async fn renew_lease(
    app: &AppHandle,
    state: &NativeMediaState,
    request: SessionRequest,
) -> Result<(), String> {
    let active = {
        let active = state
            .inner
            .slot
            .lock()
            .await
            .active
            .clone()
            .ok_or("native media peer is not open")?;
        let transport = state
            .inner
            .slot
            .lock()
            .await
            .transport
            .clone()
            .ok_or("native media transport is not authenticated")?;
        request.session.validate(&transport)?;
        let current = active.session.read().await;
        if !current.same_peer(&request.session) || request.session.expires_at <= current.expires_at
        {
            return Err("native media lease renewal is stale or changes the peer binding".into());
        }
        drop(current);
        active
    };

    let lifetime = remaining_lifetime(request.session.expires_at)?;
    if active.evidence.microphone_active.load(Ordering::Acquire) {
        let peer = active.peer.lock().await;
        peer.renew_microphone_lease(&request.session.binding, lifetime)
            .map_err(|error| error.to_string())?;
    }
    *active.session.write().await = request.session;
    emit_status(app, &active, "lease_renewed", None).await;
    Ok(())
}

#[tauri::command]
pub async fn media_revoke(
    app: AppHandle,
    state: State<'_, NativeMediaState>,
    request: RevokeRequest,
) -> Result<(), String> {
    revoke(&app, state.inner(), request).await
}

pub(crate) async fn revoke(
    app: &AppHandle,
    state: &NativeMediaState,
    request: RevokeRequest,
) -> Result<(), String> {
    validate_reason(request.reason.as_deref())?;
    let _lifecycle = state.inner.lifecycle.lock().await;
    let active = {
        let mut slot = state.inner.slot.lock().await;
        let active = slot
            .active
            .as_ref()
            .ok_or("native media peer is not open")?;
        if *active.session.read().await != request.session {
            return Err("stale media revocation did not match the active peer".into());
        }
        slot.active.take()
    };
    if let Some(active) = active {
        close_peer(
            Some(app),
            active,
            "revoked",
            request
                .reason
                .as_deref()
                .unwrap_or("media authority revoked"),
        )
        .await;
    }
    Ok(())
}

#[tauri::command]
pub async fn media_close(app: AppHandle, state: State<'_, NativeMediaState>) -> Result<(), String> {
    close(&app, state.inner(), "media closed locally").await
}

pub(crate) async fn close(
    app: &AppHandle,
    state: &NativeMediaState,
    reason: &'static str,
) -> Result<(), String> {
    let _lifecycle = state.inner.lifecycle.lock().await;
    let active = state.inner.slot.lock().await.active.take();
    if let Some(active) = active {
        close_peer(Some(app), active, "closed", reason).await;
    } else {
        emit_local_proof_reset(app);
    }
    Ok(())
}

async fn watch_peer(state: NativeMediaState, app: AppHandle, active: ActiveMedia) {
    let mut next_proof = tokio::time::Instant::now();
    let mut next_microphone_permission_check = tokio::time::Instant::now();
    #[cfg(target_os = "android")]
    let mut next_audio_route_check = tokio::time::Instant::now();
    #[cfg(target_os = "android")]
    let mut last_audio_route_revision = 0_u64;
    loop {
        let session = active.session.read().await.clone();
        if remaining_lifetime(session.expires_at).is_err() {
            state
                .close_if_current(&app, active.generation, "expired", "media lease expired")
                .await;
            return;
        }

        let event = {
            let mut peer = active.peer.lock().await;
            tokio::time::timeout(PEER_EVENT_POLL, peer.next_event()).await
        };
        match event {
            Ok(Some(PeerEvent::LocalIce(candidate))) => {
                emit_signal(&app, &active, LocalSignal::Ice { candidate }).await;
            }
            Ok(Some(PeerEvent::IceComplete)) => {
                emit_signal(&app, &active, LocalSignal::IceComplete).await;
            }
            Ok(Some(PeerEvent::RemoteAudioReady)) => {
                active
                    .evidence
                    .remote_audio_ready
                    .store(true, Ordering::Release);
                emit_status(
                    &app,
                    &active,
                    remote_audio_status_phase(active.evidence.connected.load(Ordering::Acquire)),
                    None,
                )
                .await;
            }
            Ok(Some(PeerEvent::ConnectionState(connection))) => {
                let connected = connection == "connected";
                active
                    .evidence
                    .connected
                    .store(connected, Ordering::Release);
                if !connected {
                    active.peer.lock().await.disarm_microphone();
                    active
                        .evidence
                        .microphone_active
                        .store(false, Ordering::Release);
                    reset_proof_if_active(&app, &active.evidence);
                }
                emit_status(&app, &active, connection, None).await;
                if matches!(connection, "failed" | "closed") {
                    state
                        .close_if_current(
                            &app,
                            active.generation,
                            "failed",
                            "native WebRTC connection failed",
                        )
                        .await;
                    return;
                }
            }
            Ok(Some(PeerEvent::ProtocolViolation(reason))) => {
                state
                    .close_if_current(&app, active.generation, "failed", reason)
                    .await;
                return;
            }
            Ok(None) => {
                state
                    .close_if_current(
                        &app,
                        active.generation,
                        "closed",
                        "native WebRTC event stream ended",
                    )
                    .await;
                return;
            }
            Err(_) => {}
        }

        if active.evidence.microphone_active.load(Ordering::Acquire)
            && tokio::time::Instant::now() >= next_microphone_permission_check
        {
            match microphone_permission_granted(&app).await {
                Ok(true) => {
                    next_microphone_permission_check =
                        tokio::time::Instant::now() + MICROPHONE_PERMISSION_POLL;
                }
                Ok(false) | Err(_) => {
                    active.peer.lock().await.disarm_microphone();
                    active
                        .evidence
                        .microphone_active
                        .store(false, Ordering::Release);
                    reset_proof_if_active(&app, &active.evidence);
                    state
                        .close_if_current(
                            &app,
                            active.generation,
                            "revoked",
                            "microphone permission was revoked",
                        )
                        .await;
                    return;
                }
            }
        }

        let live_now = matches!(session.binding.mode, MediaMode::Talk | MediaMode::Consult)
            && active.evidence.connected.load(Ordering::Acquire)
            && active.evidence.remote_audio_ready.load(Ordering::Acquire)
            && active.evidence.microphone_active.load(Ordering::Acquire);
        let was_live = active.evidence.live_active.swap(live_now, Ordering::AcqRel);
        if live_now && !was_live {
            let _ = crate::android_runtime::signal_live_transition(&app).await;
            let _ = app.emit(
                "aokie-companion://live-transition",
                LiveTransitionEvent {
                    session: session.clone(),
                    occurred_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                },
            );
        }

        #[cfg(target_os = "android")]
        if tokio::time::Instant::now() >= next_audio_route_check {
            if let Ok(routes) = crate::android_runtime::audio_routes(&app).await {
                if routes.revision != last_audio_route_revision {
                    last_audio_route_revision = routes.revision;
                    let route_state = if active.evidence.microphone_active.load(Ordering::Acquire) {
                        "microphone_active"
                    } else {
                        "media_active"
                    };
                    if let Ok(devices) = android_audio_devices(routes, route_state) {
                        let _ = app.emit("aokie-companion://audio-routes", devices);
                    }
                }
            }
            next_audio_route_check = tokio::time::Instant::now() + Duration::from_millis(500);
        }

        if live_now && tokio::time::Instant::now() >= next_proof {
            emit_local_proof(&app, &session);
            active.evidence.proof_active.store(true, Ordering::Release);
            next_proof = tokio::time::Instant::now() + LOCAL_PROOF_INTERVAL;
        }

        let current = state.inner.slot.lock().await.active.clone();
        if current
            .as_ref()
            .is_none_or(|current| current.generation != active.generation)
        {
            return;
        }
    }
}

async fn close_peer(
    app: Option<&AppHandle>,
    active: ActiveMedia,
    phase: &'static str,
    reason: &str,
) {
    let session = active.session.read().await.clone();
    active.peer.lock().await.close();
    if let Some(app) = app {
        let _ = crate::android_runtime::end_communication_audio(app).await;
    }
    active
        .evidence
        .microphone_active
        .store(false, Ordering::Release);
    active.evidence.live_active.store(false, Ordering::Release);
    if let Some(app) = app {
        if phase != "replaced" && session.binding.mode == MediaMode::Talk {
            let _ = crate::android_runtime::reconcile_offer(
                app,
                &session.binding.call_id,
                session.binding.call_epoch,
                "cancel",
                phase,
            )
            .await;
        }
        reset_proof_if_active(app, &active.evidence);
        emit_status(app, &active, phase, Some(reason.to_string())).await;
    }
}

async fn emit_status(
    app: &AppHandle,
    active: &ActiveMedia,
    phase: &'static str,
    reason: Option<String>,
) {
    let session = active.session.read().await.clone();
    let microphone_active = active.evidence.microphone_active.load(Ordering::Acquire);
    let remote_audio_ready = active.evidence.remote_audio_ready.load(Ordering::Acquire);
    eprintln!(
        "[AokieCompanion][media] phase={} call={} rtc={} mode={:?} connected={} remote_audio_ready={} microphone_active={}{}",
        phase,
        session.binding.call_id,
        session.binding.rtc_session_id,
        session.binding.mode,
        active.evidence.connected.load(Ordering::Acquire),
        remote_audio_ready,
        microphone_active,
        reason
            .as_deref()
            .map(|value| format!(" reason={value}"))
            .unwrap_or_default(),
    );
    let _ = app.emit(
        "aokie-companion://media-state",
        MediaStatusEvent {
            session,
            phase,
            microphone_active,
            remote_audio_ready,
            reason,
        },
    );
}

fn remote_audio_status_phase(connected: bool) -> &'static str {
    if connected {
        "remote_audio_ready"
    } else {
        "connecting"
    }
}

async fn emit_signal(app: &AppHandle, active: &ActiveMedia, signal: LocalSignal) {
    let event = MediaSignalEvent {
        session: active.session.read().await.clone(),
        signal,
    };
    // Native transports subscribe here so SDP/ICE never need to round-trip
    // through an untrusted WebView before being fenced by the lease token.
    let _ = active.signal_tx.send(event.clone());
    let _ = app.emit("aokie-companion://media-signal", event);
}

fn emit_local_proof(app: &AppHandle, session: &MediaSession) {
    let now = Utc::now();
    let proof_expiry = std::cmp::min(session.expires_at, now + LOCAL_PROOF_TTL);
    let _ = app.emit(
        "aokie-companion://local-media",
        LocalMediaProof {
            app_id: session.app_id.clone(),
            stream_nonce: session.stream_nonce.clone(),
            rtc_session_id: session.binding.rtc_session_id.clone(),
            call_id: session.binding.call_id.clone(),
            call_epoch: session.binding.call_epoch,
            owner_epoch: session.binding.owner_epoch,
            device_id: session.binding.device_id.clone(),
            lease_id: session.binding.lease_id().unwrap_or_default().to_string(),
            mode: session.binding.mode,
            fence: session.binding.fence,
            sdp_revision: session.sdp_revision,
            transport_generation: session.transport_generation,
            expires_at: proof_expiry.to_rfc3339_opts(SecondsFormat::Millis, true),
            active: true,
        },
    );
}

fn emit_local_proof_reset(app: &AppHandle) {
    let _ = app.emit::<Option<LocalMediaProof>>("aokie-companion://local-media", None);
}

fn reset_proof_if_active(app: &AppHandle, evidence: &ConnectionEvidence) {
    if evidence.proof_active.swap(false, Ordering::AcqRel) {
        emit_local_proof_reset(app);
    }
}

fn validate_audio_device_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 1_024 || value.chars().any(char::is_control) {
        Err("audio endpoint identity is invalid".into())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "android")]
fn android_audio_devices(
    routes: crate::android_runtime::AndroidAudioRoutes,
    state: &'static str,
) -> Result<NativeAudioDevices, String> {
    let devices = routes
        .routes
        .into_iter()
        .map(|route| NativeAudioDevice {
            id: route.id,
            label: route.label,
        })
        .collect::<Vec<_>>();
    if routes.selected_id != SYSTEM_MANAGED_AUDIO_DEVICE_ID
        && !devices.iter().any(|device| device.id == routes.selected_id)
    {
        return Err("Android selected communication route disappeared".into());
    }
    Ok(NativeAudioDevices {
        routing_policy: "system_managed",
        input_devices: devices.clone(),
        output_devices: devices,
        selected_input_id: routes.selected_id.clone(),
        selected_output_id: routes.selected_id,
        state,
        can_select: routes.can_select && state != "idle",
    })
}

#[cfg(target_os = "windows")]
fn resolve_audio_selection<'a>(
    requested: &str,
    available: impl Iterator<Item = &'a str>,
    kind: &str,
) -> Result<Option<String>, String> {
    if requested == SYSTEM_DEFAULT_AUDIO_DEVICE_ID {
        return Ok(None);
    }
    if available.into_iter().any(|guid| guid == requested) {
        Ok(Some(requested.to_string()))
    } else {
        Err(format!("selected {kind} is no longer available"))
    }
}

#[cfg(target_os = "windows")]
fn build_selectable_audio_devices(
    devices: aokie_media::PlatformAudioDevices,
    selection: &AudioSelection,
    state: &'static str,
) -> NativeAudioDevices {
    let input_devices = native_audio_devices(devices.recording);
    let output_devices = native_audio_devices(devices.playout);
    NativeAudioDevices {
        routing_policy: "selectable",
        input_devices,
        output_devices,
        selected_input_id: selection
            .input_id
            .clone()
            .unwrap_or_else(|| SYSTEM_DEFAULT_AUDIO_DEVICE_ID.into()),
        selected_output_id: selection
            .output_id
            .clone()
            .unwrap_or_else(|| SYSTEM_DEFAULT_AUDIO_DEVICE_ID.into()),
        state,
        can_select: state == "idle",
    }
}

#[cfg(target_os = "windows")]
fn native_audio_devices(devices: Vec<aokie_media::PlatformAudioDevice>) -> Vec<NativeAudioDevice> {
    let mut seen = HashSet::new();
    let mut result = vec![NativeAudioDevice {
        id: SYSTEM_DEFAULT_AUDIO_DEVICE_ID.into(),
        label: "System default".into(),
    }];
    for device in devices {
        if validate_audio_device_id(&device.guid).is_err() || !seen.insert(device.guid.clone()) {
            continue;
        }
        let label: String = device
            .label
            .trim()
            .chars()
            .filter(|character| !character.is_control())
            .take(200)
            .collect();
        result.push(NativeAudioDevice {
            id: device.guid,
            label: if label.is_empty() {
                "Unnamed audio endpoint".into()
            } else {
                label
            },
        });
    }
    result
}

fn remaining_lifetime(expires_at: DateTime<Utc>) -> Result<Duration, String> {
    let remaining = expires_at
        .signed_duration_since(Utc::now())
        .to_std()
        .map_err(|_| "native media lease is expired".to_string())?;
    if remaining.is_zero() || remaining > MAX_SESSION_LIFETIME {
        return Err("native media lease lifetime is invalid".into());
    }
    Ok(remaining)
}

fn validate_replacement(current: &MediaSession, next: &MediaSession) -> Result<(), String> {
    if current.app_id != next.app_id || current.binding.device_id != next.binding.device_id {
        return Err("native media replacement changed its authenticated identity".into());
    }
    if current.binding.call_id == next.binding.call_id
        && current.binding.call_epoch == next.binding.call_epoch
    {
        if next.binding.owner_epoch < current.binding.owner_epoch {
            return Err("native media replacement regressed owner epoch".into());
        }
        if next.binding.owner_epoch == current.binding.owner_epoch
            && (next.transport_generation < current.transport_generation
                || (next.transport_generation == current.transport_generation
                    && next.sdp_revision <= current.sdp_revision))
        {
            return Err("native media replacement is stale".into());
        }
    } else if next.binding.call_epoch <= current.binding.call_epoch {
        return Err("native media replacement did not advance the call epoch".into());
    }
    Ok(())
}

fn validate_reason(reason: Option<&str>) -> Result<(), String> {
    if reason.is_some_and(|reason| {
        reason.len() > 200 || reason.chars().any(|character| character.is_control())
    }) {
        Err("native media revocation reason is invalid".into())
    } else {
        Ok(())
    }
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn next_generation(counter: &AtomicU64) -> u64 {
    loop {
        let current = counter.load(Ordering::Acquire);
        let next = current.wrapping_add(1).max(1);
        if counter
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return next;
        }
    }
}

#[cfg(not(target_os = "android"))]
async fn initialize_platform_audio(_app: &AppHandle) -> Result<bool, String> {
    Ok(true)
}

#[cfg(target_os = "android")]
async fn initialize_platform_audio(app: &AppHandle) -> Result<bool, String> {
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = (|| -> Result<bool, String> {
                    let vm = env.get_java_vm().map_err(|error| error.to_string())?;
                    let context = env
                        .call_method(
                            activity,
                            "getApplicationContext",
                            "()Landroid/content/Context;",
                            &[],
                        )
                        .map_err(|error| error.to_string())?
                        .l()
                        .map_err(|error| error.to_string())?;
                    Ok(libwebrtc::android::initialize_android_context(
                        &vm, &context,
                    ))
                })();
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android WebRTC initialization timed out".to_string())?
        .map_err(|_| "Android WebRTC initialization was cancelled".to_string())?
}

#[cfg(not(target_os = "android"))]
async fn ensure_microphone_permission(_app: &AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "android"))]
async fn microphone_permission_granted(_app: &AppHandle) -> Result<bool, String> {
    Ok(true)
}

#[cfg(target_os = "android")]
async fn ensure_microphone_permission(app: &AppHandle) -> Result<(), String> {
    const REQUEST_ID: i32 = 4_217;
    let initial =
        android_permission_call(app, "requestAokieMicrophonePermission", REQUEST_ID).await?;
    if initial > 0 {
        return Ok(());
    }
    if initial < 0 {
        return Err("microphone permission was denied".into());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let result =
            android_permission_call(app, "pollAokieMicrophonePermission", REQUEST_ID).await?;
        if result > 0 {
            return Ok(());
        }
        if result < 0 {
            return Err("microphone permission was denied".into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("microphone permission request timed out".into());
        }
    }
}

#[cfg(target_os = "android")]
async fn microphone_permission_granted(app: &AppHandle) -> Result<bool, String> {
    const REQUEST_ID: i32 = 4_217;
    Ok(android_permission_call(app, "pollAokieMicrophonePermission", REQUEST_ID).await? > 0)
}

#[cfg(target_os = "android")]
async fn android_permission_call(
    app: &AppHandle,
    method: &'static str,
    request_id: i32,
) -> Result<i32, String> {
    use jni::objects::JValue;
    use tokio::sync::oneshot;

    let window = app
        .get_webview_window("main")
        .ok_or("main Android webview is unavailable")?;
    let (send, receive) = oneshot::channel();
    window
        .with_webview(move |webview| {
            webview.jni_handle().exec(move |env, activity, _webview| {
                let result = env
                    .call_method(activity, method, "(I)I", &[JValue::Int(request_id)])
                    .and_then(|value| value.i())
                    .map_err(|error| error.to_string());
                let _ = send.send(result);
            });
        })
        .map_err(|error| error.to_string())?;
    tokio::time::timeout(Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Android permission bridge timed out".to_string())?
        .map_err(|_| "Android permission bridge was cancelled".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(mode: MediaMode, owner_epoch: u64, fence: u64) -> MediaSession {
        MediaSession {
            app_id: "app_test".into(),
            stream_nonce: "stream_test".into(),
            binding: SessionBinding {
                rtc_session_id: format!("rtc_{owner_epoch}_{fence}"),
                call_id: "call_test".into(),
                call_epoch: 4,
                owner_epoch,
                device_id: "device_test".into(),
                mode,
                lease_id: Some(format!("lease_{owner_epoch}_{fence}")),
                fence,
            },
            sdp_revision: 1,
            transport_generation: 1,
            expires_at: Utc::now() + chrono::Duration::seconds(30),
        }
    }

    #[test]
    fn transport_and_short_expiry_are_part_of_native_authority() {
        let transport = TransportIdentity {
            app_id: "app_test".into(),
            device_id: "device_test".into(),
        };
        assert!(session(MediaMode::Talk, 2, 3).validate(&transport).is_ok());
        let mut wrong_device = session(MediaMode::Talk, 2, 3);
        wrong_device.binding.device_id = "device_other".into();
        assert!(wrong_device.validate(&transport).is_err());
        let mut excessive = session(MediaMode::Talk, 2, 3);
        excessive.expires_at = Utc::now() + chrono::Duration::minutes(10);
        assert!(excessive.validate(&transport).is_err());
    }

    #[test]
    fn monitor_has_a_lease_but_never_a_microphone_or_fence() {
        let monitor = session(MediaMode::Monitor, 1, 0);
        assert!(monitor.binding.validate().is_ok());
        assert!(!monitor.binding.mode.needs_microphone());
        let mut unsafe_monitor = monitor;
        unsafe_monitor.binding.fence = 1;
        assert!(unsafe_monitor.binding.validate().is_err());
    }

    #[test]
    fn replacement_rejects_stale_owner_transport_and_sdp() {
        let current = session(MediaMode::Talk, 5, 7);
        let mut stale = current.clone();
        stale.binding.rtc_session_id = "rtc_stale".into();
        assert!(validate_replacement(&current, &stale).is_err());

        let mut ice_restart = current.clone();
        ice_restart.binding.rtc_session_id = "rtc_restart".into();
        ice_restart.sdp_revision = 2;
        assert!(validate_replacement(&current, &ice_restart).is_ok());

        let mut new_owner = current.clone();
        new_owner.binding.rtc_session_id = "rtc_new_owner".into();
        new_owner.binding.owner_epoch = 6;
        new_owner.binding.fence = 8;
        assert!(validate_replacement(&current, &new_owner).is_ok());
    }

    #[test]
    fn audio_endpoint_identity_is_bounded_and_printable() {
        assert!(validate_audio_device_id("{windows-endpoint-guid}").is_ok());
        assert!(validate_audio_device_id("speaker endpoint").is_ok());
        assert!(validate_audio_device_id("").is_err());
        assert!(validate_audio_device_id("speaker\nendpoint").is_err());
        assert!(validate_audio_device_id(&"x".repeat(1_025)).is_err());
    }

    #[test]
    fn early_remote_track_does_not_claim_microphone_arm_readiness() {
        assert_eq!(remote_audio_status_phase(false), "connecting");
        assert_eq!(remote_audio_status_phase(true), "remote_audio_ready");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn selection_requires_a_current_enumerated_guid_or_system_default() {
        let available = ["capture-one", "capture-two"];
        assert_eq!(
            resolve_audio_selection(
                SYSTEM_DEFAULT_AUDIO_DEVICE_ID,
                available.iter().copied(),
                "microphone"
            )
            .unwrap(),
            None
        );
        assert_eq!(
            resolve_audio_selection("capture-two", available.iter().copied(), "microphone")
                .unwrap(),
            Some("capture-two".into())
        );
        assert!(
            resolve_audio_selection("unplugged", available.iter().copied(), "microphone").is_err()
        );
    }
}
