//! Native Companion media routing owned by the Aokie plugin.
//!
//! The public plugin protocol never carries PCM.  This module terminates one
//! native WebRTC peer per admitted Companion device, fans the current SCO RX
//! stream to monitor/talk peers through bounded queues, and exposes only
//! typed signalling and lease transitions to the connector adapter.
//!
//! There are deliberately two caller-transmit gates:
//!
//! * [`aokie_media::DesktopPeer`] quarantines decoded talk audio until its
//!   exact immutable binding has a current [`RoutePermit`].
//! * [`RemoteMediaState`] checks the same call/owner/lease/fence again when
//!   the radio thread consumes a decoded frame.
//!
//! A compromised/stale signalling task therefore cannot write to SCO merely
//! by retaining a decoder or a queue sender.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aokie_media::{
    DesktopPeer, IceCandidateSignal, IceServerConfig, MediaMode, OwnedAudioFrame, PeerEvent,
    PeerOptions, RoutePermit, SdpSignal, SessionBinding, MEDIA_SAMPLE_RATE_HZ,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub const MAX_REMOTE_PEERS: usize = 6;
pub const REMOTE_CONSENT_POLICY_ID: &str = "aokie_remote_access";
// Lifecycle commands must remain available while negotiation and caller audio
// are busy. ICE gets its own admitted-burst-sized lane; PCM is deliberately
// small and lossy because stale audio is less useful than the current frame.
const ACTOR_QUEUE_CAPACITY: usize = 12;
const REMOTE_ICE_QUEUE_CAPACITY: usize = 128;
const CALLER_PCM_QUEUE_CAPACITY: usize = 4;
const REMOTE_ICE_BATCH_SIZE: usize = 8;
const MANAGER_QUEUE_CAPACITY: usize = 32;
const ROUTED_AUDIO_FRAMES: usize = 16;
const EVENT_QUEUE_CAPACITY: usize = 128;
const MAX_LEASE_TTL: Duration = Duration::from_secs(5 * 60);
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
/// An active Talk peer gets only this bounded window to prove real microphone
/// PCM. Lease heartbeats deliberately cannot extend this preflight deadline.
const TALK_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);
/// Once the radio opens the exact caller route, real PCM must reach SCO very
/// quickly. Negotiation and permission prompts happen before this point.
const FIRST_TALK_PCM_TIMEOUT: Duration = Duration::from_secs(1);
/// An enabled WebRTC microphone produces frames even during silence. Losing
/// them for this long means the human path is no longer live, so Aokie resumes.
const ONGOING_TALK_PCM_TIMEOUT: Duration = Duration::from_secs(2);
/// Private consultation is isolated only after exact microphone PCM has been
/// proven. Once isolated, silence still produces WebRTC frames; losing them
/// must return the caller to Aokie just as aggressively as takeover.
const FIRST_CONSULT_PCM_TIMEOUT: Duration = Duration::from_secs(1);
const ONGOING_CONSULT_PCM_TIMEOUT: Duration = Duration::from_secs(2);
/// Hold a real endpoint sample briefly, then decay it linearly. If no PCM has
/// arrived by the stale bound the source is omitted rather than fabricated as
/// silent/connected.
const AUDIO_LEVEL_HOLD: Duration = Duration::from_millis(180);
const AUDIO_LEVEL_STALE: Duration = Duration::from_millis(1_500);

/// The radio-owned service state.  `HumanActive` is entered only after the
/// radio thread has flushed caller TX and acknowledged the pending permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceMode {
    AokieActive,
    SoftHold,
    ConsultPending,
    ConsultActive,
    HumanPending,
    HumanActive,
    ReturningToAokie,
    Recovering,
}

impl Default for ServiceMode {
    fn default() -> Self {
        Self::AokieActive
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMediaSnapshot {
    pub call_id: Option<String>,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub remote_revision: u64,
    pub service_mode: ServiceMode,
    pub peer_count: usize,
    pub talk_device_id: Option<String>,
    pub talk_lease_id: Option<String>,
    pub talk_fence: u64,
    /// True only after an authorized microphone frame for the exact active
    /// talk binding reached the caller-bound Bluetooth enqueue seam.
    pub talk_audio_forwarded: bool,
    pub microphone_muted: bool,
    pub radio_reserved: bool,
    pub dropped_sco_frames: u64,
    pub quarantined_talk_frames: u64,
    pub dropped_events: u64,
    pub consent: RemoteConsentGate,
    pub captions: Vec<RemoteCaption>,
    pub participants: Vec<RemoteParticipant>,
    pub audio_levels: Vec<RemoteAudioLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteParticipantState {
    Connected,
    Prepared,
    Active,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteParticipant {
    pub participant_id: String,
    pub device_id: String,
    pub mode: MediaMode,
    pub state: RemoteParticipantState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAudioLevelSource {
    Caller,
    Aokie,
    Companion,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteAudioLevel {
    pub source: RemoteAudioLevelSource,
    pub participant_id: Option<String>,
    pub level_permille: u16,
}

/// Exact proof that Aokie, rather than a pending/active Companion route, owns
/// one physical caller. Autonomous radio actions capture this fence before
/// starting and must re-present it while the media-state lock is held. A
/// takeover/consult claim (including a claim that is later returned) changes
/// at least the service state or dedicated action epoch (while a new physical
/// call changes the call epoch) and therefore fences a stale AI hangup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AokieOwnerFence {
    pub call_id: String,
    pub call_epoch: u64,
    pub action_epoch: u64,
}

/// Stronger fence for an Aokie-owned physical switchboard command. Prepared
/// or active non-monitor media makes this unavailable, and the owner epoch
/// pins the exact foreground leg while CHLD is issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AokieSwitchFence {
    pub owner: AokieOwnerFence,
    pub owner_epoch: u64,
    pub switch_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConsentGate {
    pub policy_id: String,
    pub policy_version: u32,
    pub enabled: bool,
    pub acknowledged: bool,
    pub acknowledged_at: Option<String>,
    pub expires_at: Option<String>,
    pub captions_enabled: bool,
    pub assistance_enabled: bool,
    pub monitor_enabled: bool,
    pub consult_enabled: bool,
    pub takeover_enabled: bool,
}

impl Default for RemoteConsentGate {
    fn default() -> Self {
        Self {
            policy_id: REMOTE_CONSENT_POLICY_ID.into(),
            policy_version: crate::consent::CURRENT_CONSENT_VERSION,
            enabled: false,
            acknowledged: false,
            acknowledged_at: None,
            expires_at: None,
            captions_enabled: false,
            assistance_enabled: false,
            monitor_enabled: false,
            consult_enabled: false,
            takeover_enabled: false,
        }
    }
}

impl RemoteConsentGate {
    pub(crate) fn is_current(&self) -> bool {
        self.enabled
            && self.acknowledged
            && self.expires_at.as_deref().is_none_or(|expiry| {
                chrono::DateTime::parse_from_rfc3339(expiry)
                    .is_ok_and(|expiry| expiry > chrono::Utc::now())
            })
    }

    fn allows(&self, mode: MediaMode) -> bool {
        self.is_current()
            && match mode {
                MediaMode::Monitor => self.monitor_enabled,
                MediaMode::PreparedTalk | MediaMode::Talk => self.takeover_enabled,
                MediaMode::PreparedConsult | MediaMode::Consult => self.consult_enabled,
            }
    }

    fn effective(&self) -> Self {
        let mut effective = self.clone();
        if !self.is_current() {
            effective.acknowledged = false;
            effective.acknowledged_at = None;
            effective.captions_enabled = false;
            effective.assistance_enabled = false;
            effective.monitor_enabled = false;
            effective.consult_enabled = false;
            effective.takeover_enabled = false;
        }
        effective
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCaption {
    pub caption_id: String,
    pub speaker: String,
    pub text: String,
    pub occurred_at: String,
    pub final_text: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenPeerRequest {
    pub binding: SessionBinding,
    pub offer: SdpSignal,
    pub lease_ttl_ms: u64,
    #[serde(default)]
    pub ice_servers: Vec<IceServerConfig>,
    #[serde(default)]
    pub relay_only: bool,
    /// Local-only proof captured before the gateway admits a non-monitor
    /// offer. A peer-controlled payload can never supply this value.
    #[serde(skip)]
    pub expected_switch_epoch: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPeerAccepted {
    pub rtc_session_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMediaEvent {
    pub sequence: u64,
    pub rtc_session_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    #[serde(flatten)]
    pub kind: RemoteMediaEventKind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteMediaEventKind {
    SdpAnswer {
        answer: SdpSignal,
    },
    LocalIce {
        candidate: IceCandidateSignal,
    },
    IceComplete,
    ConnectionState {
        state: String,
    },
    RemoteAudioReady,
    /// First decoded microphone PCM from the exact active Talk peer. The frame
    /// is still quarantined; GatewaySession uses this only to allow the final
    /// radio-owned handoff to begin.
    RemoteMicrophoneReady,
    ProtocolViolation {
        message: String,
    },
    TakeoverPending,
    /// The receive-only provisional peer is ready and the radio has entered
    /// a software hold.  The gateway may now rotate the lease into its active
    /// phase, but caller microphone audio is still impossible at this point.
    TakeoverPrepared {
        confirmed_owner_epoch: u64,
    },
    /// Desktop has flushed caller TX and entered software hold for a private
    /// consultation.  The provisional receive-only peer is closed and a
    /// fresh active consult lease/offer is required before microphone access.
    ConsultPrepared {
        confirmed_owner_epoch: u64,
    },
    ConsultActive,
    HumanActive,
    ReturningToAokie {
        reason: String,
    },
    AokieActive,
    Closed {
        reason: String,
    },
    Error {
        operation: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioTransition {
    PrepareHuman { binding: SessionBinding },
    PrepareConsult { binding: SessionBinding },
    EnterConsult { binding: SessionBinding },
    EnterHuman { binding: SessionBinding },
    ReturnToAokie { reason: String },
}

#[derive(Clone)]
pub struct RemoteMediaHandle {
    inner: Arc<RemoteMediaInner>,
}

struct RemoteMediaInner {
    state: Arc<Mutex<RemoteMediaState>>,
    manager_tx: mpsc::Sender<ManagerCommand>,
    talk_rx: Mutex<std_mpsc::Receiver<RoutedAudio>>,
    consult_rx: Mutex<std_mpsc::Receiver<RoutedAudio>>,
    event_rx: Mutex<std_mpsc::Receiver<RemoteMediaEvent>>,
    event_tx: std_mpsc::SyncSender<RemoteMediaEvent>,
    event_sequence: Arc<AtomicU64>,
    radio_reserved: AtomicBool,
    dropped_sco_frames: Arc<AtomicU64>,
    quarantined_talk_frames: Arc<AtomicU64>,
    dropped_events: Arc<AtomicU64>,
}

#[derive(Clone)]
struct RoutedAudio {
    binding: SessionBinding,
    frame: OwnedAudioFrame,
}

#[derive(Clone)]
struct PeerSlot {
    binding: SessionBinding,
    lease_expires_at: Instant,
    actor_tx: mpsc::Sender<ActorCommand>,
    remote_ice_tx: mpsc::Sender<IceCandidateSignal>,
    caller_pcm_tx: mpsc::Sender<Arc<Vec<i16>>>,
}

#[derive(Debug, Clone)]
struct CallRecord {
    call_epoch: u64,
    owner_epoch: u64,
}

#[derive(Clone)]
struct PendingRoute {
    permit: RoutePermit,
    transition_dispatched: bool,
    transfer_guard: Option<TransferActivationGuard>,
}

#[derive(Clone)]
struct TransferActivationGuard {
    request_id: String,
    offered_fence: crate::assistance::AssistanceCallFence,
    device_id: String,
    deadline: Instant,
}

fn bounded_transfer_deadline(
    setup_expires_at: u64,
    now_unix: u64,
    now_monotonic: Instant,
) -> Result<Instant, String> {
    let remaining = setup_expires_at
        .checked_sub(now_unix)
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| "accepted transfer setup deadline expired".to_string())?
        .min(crate::assistance::TRANSFER_SETUP_SECONDS);
    now_monotonic
        .checked_add(Duration::from_secs(remaining))
        .ok_or_else(|| "accepted transfer setup deadline is out of range".to_string())
}

#[derive(Clone)]
struct PendingPrepare {
    binding: SessionBinding,
    expires_at: Instant,
    transition_dispatched: bool,
}

#[derive(Clone)]
struct PreparedClaim {
    provisional_binding: SessionBinding,
    confirmed_owner_epoch: u64,
    expires_at: Instant,
    media_ready_deadline: Option<Instant>,
    active_media_binding: Option<SessionBinding>,
    last_media_received_at: Option<Instant>,
}

#[derive(Clone)]
struct PendingConsult {
    binding: SessionBinding,
    expires_at: Instant,
    transition_dispatched: bool,
    last_received_at: Option<Instant>,
}

#[derive(Clone)]
struct ActiveConsult {
    binding: SessionBinding,
    expires_at: Instant,
    activated_at: Instant,
    last_received_at: Option<Instant>,
}

struct ActiveRoute {
    permit: RoutePermit,
    activated_at: Instant,
    last_forwarded_at: Option<Instant>,
}

#[derive(Clone)]
struct ObservedAudioLevel {
    call_id: String,
    call_epoch: u64,
    participant_id: Option<String>,
    level_permille: u16,
    observed_at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallerOutputAttribution {
    Aokie,
    Companion,
}

struct RemoteMediaState {
    calls: HashMap<String, CallRecord>,
    call_order: VecDeque<String>,
    next_call_epoch: u64,
    /// Advances only when proven Companion PCM is about to reserve the caller.
    /// Unlike remote_revision, monitor/negotiation/heartbeat churn cannot
    /// spuriously cancel an in-flight Aokie reply.
    aokie_action_epoch: u64,
    /// Advances only for a physical switchboard command. Native peer opens
    /// capture this before asynchronous SDP work and must re-present it at
    /// slot insertion, making CHLD and non-monitor registration exclusive.
    aokie_switch_epoch: u64,
    remote_revision: u64,
    current_call_id: Option<String>,
    current_call_active: bool,
    service_mode: ServiceMode,
    peers: HashMap<String, PeerSlot>,
    pending_prepare: Option<PendingPrepare>,
    prepared_claim: Option<PreparedClaim>,
    pending_consult: Option<PendingConsult>,
    active_consult: Option<ActiveConsult>,
    pending_route: Option<PendingRoute>,
    active_route: Option<ActiveRoute>,
    talk_audio_binding: Option<SessionBinding>,
    microphone_mute_binding: Option<SessionBinding>,
    caller_audio_level: Option<ObservedAudioLevel>,
    aokie_audio_level: Option<ObservedAudioLevel>,
    companion_audio_levels: HashMap<String, ObservedAudioLevel>,
    return_binding: Option<SessionBinding>,
    return_reason: Option<String>,
    return_dispatched: bool,
    consent: RemoteConsentGate,
    captions: VecDeque<RemoteCaption>,
}

impl Default for RemoteMediaState {
    fn default() -> Self {
        Self {
            calls: HashMap::new(),
            call_order: VecDeque::new(),
            next_call_epoch: 0,
            aokie_action_epoch: 0,
            aokie_switch_epoch: 0,
            remote_revision: 0,
            current_call_id: None,
            current_call_active: false,
            service_mode: ServiceMode::AokieActive,
            peers: HashMap::new(),
            pending_prepare: None,
            prepared_claim: None,
            pending_consult: None,
            active_consult: None,
            pending_route: None,
            active_route: None,
            talk_audio_binding: None,
            microphone_mute_binding: None,
            caller_audio_level: None,
            aokie_audio_level: None,
            companion_audio_levels: HashMap::new(),
            return_binding: None,
            return_reason: None,
            return_dispatched: false,
            consent: RemoteConsentGate::default(),
            captions: VecDeque::new(),
        }
    }
}

enum ManagerCommand {
    Open {
        request: OpenPeerRequest,
        reply: std_mpsc::Sender<Result<OpenPeerAccepted, String>>,
    },
    Shutdown,
}

enum ActorCommand {
    Authorize(RoutePermit),
    Revoke,
    Close,
}

static ACTIVE_REMOTE_MEDIA: OnceLock<Mutex<Weak<RemoteMediaInner>>> = OnceLock::new();

impl RemoteMediaHandle {
    pub fn spawn() -> Result<Self, String> {
        let state = Arc::new(Mutex::new(RemoteMediaState::default()));
        let (manager_tx, manager_rx) = mpsc::channel(MANAGER_QUEUE_CAPACITY);
        let (talk_tx, talk_rx) = std_mpsc::sync_channel(ROUTED_AUDIO_FRAMES);
        let (consult_tx, consult_rx) = std_mpsc::sync_channel(ROUTED_AUDIO_FRAMES);
        let (event_tx, event_rx) = std_mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let event_sequence = Arc::new(AtomicU64::new(0));
        let dropped_sco_frames = Arc::new(AtomicU64::new(0));
        let quarantined_talk_frames = Arc::new(AtomicU64::new(0));
        let dropped_events = Arc::new(AtomicU64::new(0));

        let worker_state = state.clone();
        let worker_events = EventEmitter {
            tx: event_tx.clone(),
            sequence: event_sequence.clone(),
            dropped: dropped_events.clone(),
        };
        let worker_quarantined = quarantined_talk_frames.clone();
        std::thread::Builder::new()
            .name("aokie-companion-media".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!("[aokie-plugin] companion media runtime failed: {error}");
                        return;
                    }
                };
                runtime.block_on(manager_loop(
                    manager_rx,
                    worker_state,
                    talk_tx,
                    consult_tx,
                    worker_events,
                    worker_quarantined,
                ));
            })
            .map_err(|error| format!("spawn Companion media runtime: {error}"))?;

        let handle = Self {
            inner: Arc::new(RemoteMediaInner {
                state,
                manager_tx,
                talk_rx: Mutex::new(talk_rx),
                consult_rx: Mutex::new(consult_rx),
                event_rx: Mutex::new(event_rx),
                event_tx,
                event_sequence,
                radio_reserved: AtomicBool::new(false),
                dropped_sco_frames,
                quarantined_talk_frames,
                dropped_events,
            }),
        };
        let slot = ACTIVE_REMOTE_MEDIA.get_or_init(|| Mutex::new(Weak::new()));
        if let Ok(mut active) = slot.lock() {
            *active = Arc::downgrade(&handle.inner);
        }
        Ok(handle)
    }

    /// Observe physical truth from the radio thread.  A call epoch is stable
    /// for one call id even if the old call-session generation changes during
    /// a park/restore.  Switching foreground calls closes old media peers so
    /// a second caller can never inherit their route.
    pub fn observe_physical_call(&self, call_id: Option<&str>, active: bool) {
        let (actors, events) = {
            let Ok(mut state) = self.inner.state.lock() else {
                return;
            };
            state.observe_call(call_id, active)
        };
        for actor in actors {
            let _ = actor.try_send(ActorCommand::Revoke);
            let _ = actor.try_send(ActorCommand::Close);
        }
        for (binding, kind) in events {
            self.emit(binding, kind);
        }
        self.refresh_reserved();
    }

    pub fn snapshot(&self) -> RemoteMediaSnapshot {
        let Some(state) = self.inner.state.lock().ok() else {
            return RemoteMediaSnapshot {
                call_id: None,
                call_epoch: 0,
                owner_epoch: 0,
                remote_revision: 0,
                service_mode: ServiceMode::Recovering,
                peer_count: 0,
                talk_device_id: None,
                talk_lease_id: None,
                talk_fence: 0,
                talk_audio_forwarded: false,
                microphone_muted: false,
                radio_reserved: self.radio_reserved(),
                dropped_sco_frames: self.inner.dropped_sco_frames.load(Ordering::Relaxed),
                quarantined_talk_frames: self.inner.quarantined_talk_frames.load(Ordering::Relaxed),
                dropped_events: self.inner.dropped_events.load(Ordering::Relaxed),
                consent: RemoteConsentGate::default(),
                captions: Vec::new(),
                participants: Vec::new(),
                audio_levels: Vec::new(),
            };
        };
        let current = state.current_record();
        let call_id = state.current_call_id.clone();
        let call_epoch = current.map_or(0, |record| record.call_epoch);
        let owner_epoch = current.map_or(0, |record| record.owner_epoch);
        let binding = state
            .active_route
            .as_ref()
            .map(|route| &route.permit.binding)
            .or_else(|| state.active_consult.as_ref().map(|route| &route.binding))
            .or_else(|| {
                state
                    .pending_route
                    .as_ref()
                    .map(|route| &route.permit.binding)
            })
            .or_else(|| {
                state
                    .pending_consult
                    .as_ref()
                    .map(|pending| &pending.binding)
            })
            .or_else(|| {
                state
                    .pending_prepare
                    .as_ref()
                    .map(|pending| &pending.binding)
            })
            .or_else(|| {
                state
                    .prepared_claim
                    .as_ref()
                    .map(|prepared| &prepared.provisional_binding)
            })
            .cloned();
        let mut participants = state
            .peers
            .values()
            .map(|slot| RemoteParticipant {
                participant_id: companion_participant_id(&slot.binding),
                device_id: slot.binding.device_id.clone(),
                mode: slot.binding.mode,
                state: state.participant_state(&slot.binding),
            })
            .collect::<Vec<_>>();
        participants.sort_by(|left, right| left.participant_id.cmp(&right.participant_id));
        let audio_levels = state.current_audio_levels(Instant::now());
        let microphone_muted = state
            .microphone_mute_binding
            .as_ref()
            .is_some_and(|muted| state.binding_is_active_media_owner(muted, Instant::now()));
        RemoteMediaSnapshot {
            call_id,
            call_epoch,
            owner_epoch,
            remote_revision: state.remote_revision,
            service_mode: state.service_mode,
            peer_count: state.peers.len(),
            talk_device_id: binding.as_ref().map(|binding| binding.device_id.clone()),
            talk_lease_id: binding
                .as_ref()
                .and_then(|binding| binding.lease_id.clone()),
            talk_fence: binding.as_ref().map_or(0, |binding| binding.fence),
            talk_audio_forwarded: binding
                .as_ref()
                .is_some_and(|binding| state.talk_audio_binding.as_ref() == Some(binding)),
            microphone_muted,
            radio_reserved: state.radio_reserved(),
            dropped_sco_frames: self.inner.dropped_sco_frames.load(Ordering::Relaxed),
            quarantined_talk_frames: self.inner.quarantined_talk_frames.load(Ordering::Relaxed),
            dropped_events: self.inner.dropped_events.load(Ordering::Relaxed),
            consent: state.consent.effective(),
            captions: state.captions.iter().cloned().collect(),
            participants,
            audio_levels,
        }
    }

    /// Capture the exact current Aokie caller-owner fence. `None` is the
    /// fail-closed answer while the call is inactive or any Companion
    /// takeover/consult transition reserves the radio.
    pub fn aokie_owner_fence(&self) -> Option<AokieOwnerFence> {
        self.inner
            .state
            .lock()
            .ok()
            .and_then(|state| state.aokie_owner_fence())
    }

    /// Capture an exact foreground-leg fence only while no prepared or active
    /// Companion media claim can race a physical switchboard command.
    pub fn aokie_switch_fence(&self) -> Option<AokieSwitchFence> {
        self.inner
            .state
            .lock()
            .ok()
            .and_then(|state| state.aokie_switch_fence())
    }

    /// Capture the physical-switch epoch before the gateway makes its
    /// marker/revision admission decision. Non-monitor peer opening carries
    /// this proof through asynchronous SDP work and rechecks it both when the
    /// manager dequeues the request and when the native slot is inserted.
    pub fn capture_aokie_switch_epoch(&self) -> Result<u64, String> {
        self.inner
            .state
            .lock()
            .map(|state| state.aokie_switch_epoch)
            .map_err(|_| "remote media state poisoned".to_string())
    }

    /// Linearize one short CHLD/physical-switch command against media claims.
    /// The state lock is held only for the command send; callers must bump the
    /// public switchboard revision inside `action` before touching the phone,
    /// so any claim starting after the lock is released sees a stale offer.
    pub fn with_aokie_switch_owner<T>(
        &self,
        expected: &AokieSwitchFence,
        action: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?;
        if state.aokie_switch_fence().as_ref() != Some(expected) {
            return Err("the exact Aokie switchboard fence changed".into());
        }
        state.aokie_switch_epoch = state.aokie_switch_epoch.saturating_add(1).max(1);
        state.fence_autonomous_aokie_actions();
        Ok(action())
    }

    /// Execute one autonomous physical action only if the exact Aokie owner
    /// captured by the caller is still current. The state mutex remains held
    /// through `action`, atomically ordering it against a concurrent remote
    /// claim: whichever acquires the lock first owns the decision.
    pub fn with_aokie_owner<T>(
        &self,
        expected: &AokieOwnerFence,
        action: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?;
        if state.aokie_owner_fence().as_ref() != Some(expected) {
            return Err("the exact Aokie caller-owner fence changed".into());
        }
        Ok(action())
    }

    /// Linearize a short decision for an autonomous side effect whose actual
    /// I/O must not run while the media-state mutex is held (for example a
    /// durable host event). If this succeeds, the action won ownership before
    /// any later Companion claim and may finish after the lock is released.
    /// Advancing the dedicated epoch makes every other stale Aokie decision
    /// fail closed; it does not alter the externally published call epochs.
    pub fn linearize_aokie_action(&self, expected: &AokieOwnerFence) -> Result<(), String> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?;
        if state.aokie_owner_fence().as_ref() != Some(expected) {
            return Err("the exact Aokie caller-owner fence changed".into());
        }
        state.fence_autonomous_aokie_actions();
        Ok(())
    }

    /// Run one physical caller-ending action while the remote-owner mutex is
    /// held. This atomically orders an End caller command against Return to
    /// Aokie/revoke: whichever acquires the state first wins, and a return
    /// that already won makes the action impossible.
    pub fn with_active_talk_owner<T>(
        &self,
        call_id: &str,
        call_epoch: u64,
        owner_epoch: u64,
        remote_revision: u64,
        device_id: &str,
        lease_id: &str,
        fence: u64,
        action: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?;
        let binding = state
            .active_route
            .as_ref()
            .map(|route| route.permit.binding.clone())
            .ok_or_else(|| "there is no active Companion caller owner".to_string())?;
        let record = state
            .current_record()
            .ok_or_else(|| "physical call authority is unavailable".to_string())?;
        let consent = state.consent.effective();
        if state.current_call_id.as_deref() != Some(call_id)
            || record.call_epoch != call_epoch
            || record.owner_epoch != owner_epoch
            || state.remote_revision != remote_revision
            || state.service_mode != ServiceMode::HumanActive
            || binding.call_id != call_id
            || binding.call_epoch != call_epoch
            || binding.owner_epoch != owner_epoch
            || binding.device_id != device_id
            || binding.lease_id.as_deref() != Some(lease_id)
            || binding.fence != fence
            || !consent.enabled
            || !consent.acknowledged
            || !consent.takeover_enabled
        {
            return Err("the exact active Companion caller-owner fence changed".into());
        }
        if !state.allows_talk(&binding, Instant::now()) {
            return Err("the Companion caller-owner permit expired or was revoked".into());
        }
        Ok(action())
    }

    /// Replace the live, versioned remote disclosure gate. Revocation is
    /// immediate: any peer whose exact mode is no longer permitted is closed
    /// before a later PCM frame can be routed.
    pub fn set_remote_consent(&self, gate: RemoteConsentGate) {
        let revoked = {
            let Ok(mut state) = self.inner.state.lock() else {
                return;
            };
            state.consent = gate;
            // Decide whether caller audio was ever physically reserved before
            // removing the now-disallowed peer slots. Prepared Talk and its
            // active-Talk WebRTC preflight deliberately leave Aokie in charge;
            // revoking consent there must close/fence in place, not fabricate
            // a ReturnToAokie transition that suppresses the receptionist.
            let revoked_route = state
                .active_route
                .as_ref()
                .filter(|route| !state.consent.allows(route.permit.binding.mode))
                .map(|route| (route.permit.binding.clone(), true))
                .or_else(|| {
                    state
                        .active_consult
                        .as_ref()
                        .filter(|route| !state.consent.allows(route.binding.mode))
                        .map(|route| (route.binding.clone(), true))
                })
                .or_else(|| {
                    state
                        .pending_consult
                        .as_ref()
                        .filter(|pending| !state.consent.allows(pending.binding.mode))
                        .map(|pending| (pending.binding.clone(), false))
                })
                .or_else(|| {
                    state
                        .pending_route
                        .as_ref()
                        .filter(|route| !state.consent.allows(route.permit.binding.mode))
                        .map(|route| (route.permit.binding.clone(), true))
                })
                .or_else(|| {
                    state.pending_prepare.as_ref().and_then(|pending| {
                        (!state.consent.allows(pending.binding.mode))
                            .then(|| (pending.binding.clone(), false))
                    })
                })
                .or_else(|| {
                    state.prepared_claim.as_ref().and_then(|prepared| {
                        let binding = prepared
                            .active_media_binding
                            .as_ref()
                            .unwrap_or(&prepared.provisional_binding);
                        (!state.consent.allows(binding.mode)).then(|| (binding.clone(), false))
                    })
                });
            let revoked = state
                .peers
                .iter()
                .filter(|(_, slot)| !state.consent.allows(slot.binding.mode))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            let mut slots = Vec::new();
            for id in revoked {
                if let Some(slot) = state.peers.remove(&id) {
                    slots.push(slot);
                }
            }
            if let Some((binding, needs_return)) = revoked_route {
                state.active_route = None;
                state.active_consult = None;
                state.pending_consult = None;
                state.pending_route = None;
                state.pending_prepare = None;
                state.prepared_claim = None;
                state.talk_audio_binding = None;
                state.clear_binding_observations(&binding);
                state.return_dispatched = false;
                if needs_return {
                    state.return_binding = Some(binding);
                    state.return_reason = Some("remote_consent_revoked".into());
                    state.service_mode = ServiceMode::ReturningToAokie;
                } else {
                    state.return_binding = None;
                    state.return_reason = None;
                    state.fence_cancelled_binding(&binding);
                    state.service_mode = ServiceMode::AokieActive;
                }
                state.bump_revision();
            }
            slots
        };
        for slot in revoked {
            let _ = slot.actor_tx.try_send(ActorCommand::Revoke);
            let _ = slot.actor_tx.try_send(ActorCommand::Close);
            self.emit(
                slot.binding,
                RemoteMediaEventKind::Closed {
                    reason: "remote_consent_revoked".into(),
                },
            );
        }
        self.refresh_reserved();
    }

    pub fn open_peer(&self, request: OpenPeerRequest) -> Result<OpenPeerAccepted, String> {
        validate_ttl(request.lease_ttl_ms)?;
        if !self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?
            .consent
            .allows(request.binding.mode)
        {
            return Err("current remote consent does not permit this media mode".into());
        }
        IceServerConfig::validate_all(&request.ice_servers).map_err(|error| error.to_string())?;
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.inner
            .manager_tx
            .try_send(ManagerCommand::Open {
                request,
                reply: reply_tx,
            })
            .map_err(|error| format!("Companion media queue unavailable: {error}"))?;
        reply_rx
            .recv_timeout(OPEN_TIMEOUT)
            .map_err(|_| "timed out while creating native Desktop WebRTC answer".to_string())?
    }

    pub fn add_remote_ice(
        &self,
        rtc_session_id: &str,
        candidate: IceCandidateSignal,
    ) -> Result<(), String> {
        candidate.validate().map_err(|error| error.to_string())?;
        let remote_ice = self.remote_ice_for(rtc_session_id)?;
        remote_ice
            .try_send(candidate)
            .map_err(|error| format!("peer signalling queue unavailable: {error}"))
    }

    /// Reserve the caller route for a provisional takeover without opening a
    /// microphone path.  `PreparedTalk` is a receive-only WebRTC binding;
    /// this operation merely asks the radio thread to flush Aokie's queued TX
    /// and enter a fail-closed software hold.
    pub fn request_soft_hold(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
    ) -> Result<(), String> {
        let ttl = validate_ttl(lease_ttl_ms)?;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            if !state.consent.allows(MediaMode::PreparedTalk) {
                return Err("current remote consent does not permit takeover".into());
            }
            state.request_soft_hold(binding.clone(), ttl)?;
        }
        self.refresh_reserved();
        self.emit(binding, RemoteMediaEventKind::TakeoverPending);
        Ok(())
    }

    /// Verify a provisional, receive-only consultation without reserving the
    /// caller. The active lease and exact microphone PCM must both exist before
    /// the radio may isolate the receptionist from the caller.
    pub fn request_consult_hold(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
    ) -> Result<(), String> {
        let ttl = validate_ttl(lease_ttl_ms)?;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            if !state.consent.allows(MediaMode::PreparedConsult) {
                return Err("current remote consent does not permit private consultation".into());
            }
            state.request_soft_hold(binding.clone(), ttl)?;
        }
        self.refresh_reserved();
        self.emit(binding, RemoteMediaEventKind::TakeoverPending);
        Ok(())
    }

    /// Queue final consult isolation after the rotated lease has created a
    /// fresh bidirectional peer and exact microphone PCM has been observed.
    /// This never installs a caller RoutePermit.
    pub fn request_consult(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
    ) -> Result<(), String> {
        let ttl = validate_ttl(lease_ttl_ms)?;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            if !state.consent.allows(MediaMode::Consult) {
                return Err("current remote consent does not permit private consultation".into());
            }
            state.request_consult(binding.clone(), ttl)?;
        }
        self.refresh_reserved();
        Ok(())
    }

    pub fn active_consult_binding(&self) -> Option<SessionBinding> {
        self.inner.state.lock().ok().and_then(|state| {
            state
                .active_consult
                .as_ref()
                .map(|route| route.binding.clone())
        })
    }

    pub fn active_talk_binding(&self) -> Option<SessionBinding> {
        self.inner.state.lock().ok().and_then(|state| {
            state
                .active_route
                .as_ref()
                .map(|route| route.permit.binding.clone())
        })
    }

    /// Change the Desktop-owned microphone gate for one exact active media
    /// owner. The caller's switchboard revision is fenced by the gateway; the
    /// media revision is rechecked here while the same lock that gates PCM is
    /// held, then advanced as the authoritative acknowledgement.
    pub fn set_microphone_muted(
        &self,
        binding: &SessionBinding,
        expected_remote_revision: u64,
        muted: bool,
    ) -> Result<u64, String> {
        let revision = self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?
            .set_microphone_muted(binding, expected_remote_revision, muted, Instant::now())?;
        self.refresh_reserved();
        Ok(revision)
    }

    /// Linearize the final Desktop-to-caller write against mute authority.
    /// When this returns, a successful mute acknowledgement cannot race one
    /// more Bluetooth write that was already popped from the WebRTC queue.
    pub fn send_talk_pcm_if_unmuted<F>(
        &self,
        binding: &SessionBinding,
        samples: &[i16],
        send: F,
    ) -> bool
    where
        F: FnOnce() -> bool,
    {
        if samples.is_empty() {
            return false;
        }
        let (sent, first) = {
            let Ok(mut state) = self.inner.state.lock() else {
                return false;
            };
            let now = Instant::now();
            if !state.allows_talk(binding, now)
                || state.microphone_is_muted_for(binding, now)
                || !send()
            {
                return false;
            }
            let Some(route) = state.active_route.as_mut() else {
                return false;
            };
            if route.permit.binding != *binding {
                return false;
            }
            route.last_forwarded_at = Some(now);
            state.observe_companion_audio(binding, samples, now);
            let first = state.talk_audio_binding.as_ref() != Some(binding);
            if first {
                state.talk_audio_binding = Some(binding.clone());
                state.bump_revision();
            }
            (true, first)
        };
        if first {
            let (peak, level_permille) = pcm_level_summary(samples);
            eprintln!(
                "[aokie-plugin][takeover] stage=microphone_pcm_to_sco call={} owner_epoch={} rtc={} samples={} rate_level_permille={} peak={}",
                binding.call_id,
                binding.owner_epoch,
                binding.rtc_session_id,
                samples.len(),
                level_permille,
                peak,
            );
        }
        sent
    }

    /// Records the first exact caller-bound frame accepted by the Bluetooth
    /// runtime. This is deliberately later than "microphone armed" and later
    /// than WebRTC track negotiation, so protocol snapshots cannot claim an
    /// active talk media route before PCM reaches the Desktop-to-SCO seam.
    pub fn mark_talk_audio_forwarded(&self, binding: &SessionBinding, samples: &[i16]) -> bool {
        if samples.is_empty() {
            return false;
        }
        let first = {
            let Ok(mut state) = self.inner.state.lock() else {
                return false;
            };
            let now = Instant::now();
            if !state.allows_talk(binding, now) || state.microphone_is_muted_for(binding, now) {
                return false;
            }
            let Some(route) = state.active_route.as_mut() else {
                return false;
            };
            if route.permit.binding != *binding {
                return false;
            }
            route.last_forwarded_at = Some(now);
            state.observe_companion_audio(binding, samples, now);
            if state.talk_audio_binding.as_ref() == Some(binding) {
                false
            } else {
                state.talk_audio_binding = Some(binding.clone());
                state.bump_revision();
                true
            }
        };
        if first {
            let (peak, level_permille) = pcm_level_summary(samples);
            eprintln!(
                "[aokie-plugin][takeover] stage=microphone_pcm_to_sco call={} owner_epoch={} rtc={} samples={} rate_level_permille={} peak={}",
                binding.call_id,
                binding.owner_epoch,
                binding.rtc_session_id,
                samples.len(),
                level_permille,
                peak,
            );
        }
        first
    }

    pub fn end_active_consult(&self, reason: &str) -> Result<(), String> {
        let binding = self
            .active_consult_binding()
            .ok_or_else(|| "there is no active private consultation".to_string())?;
        self.revoke(&binding, reason)
    }

    /// Enter a fail-closed software hold.  No decoded microphone frame is
    /// caller-bound until the radio loop performs and acknowledges the
    /// physical flush via [`ack_enter_human`](Self::ack_enter_human).
    pub fn request_takeover(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
    ) -> Result<(), String> {
        self.request_takeover_with_guard(binding, lease_ttl_ms, None)
    }

    /// Request a takeover that is valid only for one accepted assistance
    /// transfer and only until its separate setup deadline. The deadline is
    /// converted once to monotonic time and travels with the pending route all
    /// the way to the radio ACK linearization point.
    pub fn request_transfer_takeover(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
        request_id: String,
        offered_fence: crate::assistance::AssistanceCallFence,
        device_id: String,
        setup_expires_at: u64,
    ) -> Result<(), String> {
        if binding.device_id != device_id
            || binding.call_id != offered_fence.call_id
            || binding.call_epoch != offered_fence.call_epoch
            || !crate::assistance::global().transfer_activation_is_current(
                &request_id,
                &offered_fence,
                &device_id,
            )
        {
            return Err("accepted transfer is no longer current before takeover setup".into());
        }
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock is before the Unix epoch".to_string())?
            .as_secs();
        let deadline = bounded_transfer_deadline(setup_expires_at, now_unix, Instant::now())?;
        let guard = TransferActivationGuard {
            request_id,
            offered_fence,
            device_id,
            deadline,
        };
        self.request_takeover_with_guard(binding, lease_ttl_ms, Some(guard))
    }

    fn request_takeover_with_guard(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
        transfer_guard: Option<TransferActivationGuard>,
    ) -> Result<(), String> {
        let ttl = validate_ttl(lease_ttl_ms)?;
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            if !state.consent.allows(MediaMode::Talk) {
                return Err("current remote consent does not permit takeover".into());
            }
            state.request_takeover_guarded(binding.clone(), ttl, transfer_guard)?;
        }
        self.refresh_reserved();
        self.emit(binding, RemoteMediaEventKind::TakeoverPending);
        Ok(())
    }

    pub fn renew_lease(&self, binding: SessionBinding, lease_ttl_ms: u64) -> Result<(), String> {
        let ttl = validate_ttl(lease_ttl_ms)?;
        let (actor, permit) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.renew(&binding, ttl)?
        };
        if let (Some(actor), Some(permit)) = (actor, permit) {
            // Reauthorization is deliberately revoke-then-open.  The outer
            // route is already updated, while the peer's inner gate never has
            // a window in which an expired permit remains accepted.
            let _ = actor.try_send(ActorCommand::Revoke);
            actor
                .try_send(ActorCommand::Authorize(permit))
                .map_err(|error| format!("peer route queue unavailable: {error}"))?;
        }
        self.refresh_reserved();
        Ok(())
    }

    pub fn revoke(&self, binding: &SessionBinding, reason: &str) -> Result<(), String> {
        let (actor, event_binding, needs_return, close_peer) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.request_revoke(binding, reason)?
        };
        if let Some(actor) = actor {
            let _ = actor.try_send(ActorCommand::Revoke);
            if close_peer {
                let _ = actor.try_send(ActorCommand::Close);
            }
        }
        self.refresh_reserved();
        if needs_return {
            self.emit(
                event_binding,
                RemoteMediaEventKind::ReturningToAokie {
                    reason: sanitize_reason(reason),
                },
            );
        } else {
            self.emit(
                event_binding,
                RemoteMediaEventKind::Closed {
                    reason: sanitize_reason(reason),
                },
            );
        }
        Ok(())
    }

    pub fn close_peer(&self, rtc_session_id: &str, reason: &str) -> Result<(), String> {
        let (slot, needs_return) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.close_peer(rtc_session_id, reason)?
        };
        let _ = slot.actor_tx.try_send(ActorCommand::Revoke);
        let _ = slot.actor_tx.try_send(ActorCommand::Close);
        self.refresh_reserved();
        self.emit(
            slot.binding.clone(),
            RemoteMediaEventKind::Closed {
                reason: sanitize_reason(reason),
            },
        );
        if needs_return {
            self.emit(
                slot.binding,
                RemoteMediaEventKind::ReturningToAokie {
                    reason: sanitize_reason(reason),
                },
            );
        }
        Ok(())
    }

    /// Revoke every peer and caller route immediately. Used whenever the
    /// authenticated signalling carrier is lost. A route that actually held
    /// caller audio stays reserved until its physical return flush ACK;
    /// receive-only prepared Talk is cancelled in place because Aokie never
    /// yielded the caller in that phase.
    pub fn fail_closed_all(&self, reason: &str) {
        let (slots, returning) = {
            let Ok(mut state) = self.inner.state.lock() else {
                self.inner.radio_reserved.store(true, Ordering::Release);
                return;
            };
            state.fail_closed_all(reason)
        };
        for slot in slots {
            let _ = slot.actor_tx.try_send(ActorCommand::Revoke);
            let _ = slot.actor_tx.try_send(ActorCommand::Close);
            self.emit(
                slot.binding,
                RemoteMediaEventKind::Closed {
                    reason: sanitize_reason(reason),
                },
            );
        }
        if let Some(binding) = returning {
            self.emit(
                binding,
                RemoteMediaEventKind::ReturningToAokie {
                    reason: sanitize_reason(reason),
                },
            );
        }
        self.refresh_reserved();
    }

    pub fn drain_events(&self, max: usize) -> Vec<RemoteMediaEvent> {
        let max = max.clamp(1, EVENT_QUEUE_CAPACITY);
        let Ok(receiver) = self.inner.event_rx.lock() else {
            return Vec::new();
        };
        (0..max).filter_map(|_| receiver.try_recv().ok()).collect()
    }

    /// Non-blocking SCO ingress.  Consult peers are intentionally absent:
    /// caller audio can never enter the private consult lane.
    pub fn try_push_sco(&self, samples: &[i16], sample_rate: u32) {
        self.inner.try_push_sco(samples, sample_rate);
    }

    /// Mirror only PCM that has reached the final caller-bound SCO TX seam.
    /// Listen-only monitor peers receive it; takeover/prepared peers never do,
    /// preventing local microphone echo and preserving the PstnIn-only talker
    /// contract.
    pub fn try_push_caller_output(&self, samples: &[i16], sample_rate: u32) {
        self.inner
            .try_push_caller_output(samples, sample_rate, CallerOutputAttribution::Aokie);
    }

    /// Mirror caller-bound owner PCM to listen-only peers without attributing
    /// it to Aokie. The exact Companion participant meter was already updated
    /// at the mute/lease-gated talk seam before this mirror is reached.
    pub fn try_mirror_companion_output(&self, samples: &[i16], sample_rate: u32) {
        self.inner
            .try_push_caller_output(samples, sample_rate, CallerOutputAttribution::Companion);
    }

    /// Internal-only consult consumer.  It is separate from the caller-bound
    /// queue and is never exposed through JSON/Tauri/connector IPC.
    pub fn try_recv_consult_pcm(&self) -> Option<OwnedAudioFrame> {
        loop {
            let routed = self
                .inner
                .consult_rx
                .try_lock()
                .ok()
                .and_then(|receiver| receiver.try_recv().ok())?;
            let allowed = self.inner.state.lock().ok().is_some_and(|mut state| {
                let now = Instant::now();
                if !state.allows_consult(&routed.binding, now)
                    || state.microphone_is_muted_for(&routed.binding, now)
                {
                    return false;
                }
                if let Some(active) = state.active_consult.as_mut() {
                    active.last_received_at = Some(now);
                }
                state.observe_companion_audio(&routed.binding, &routed.frame.samples, now);
                true
            });
            self.refresh_reserved();
            if allowed {
                return Some(routed.frame);
            }
            self.inner
                .quarantined_talk_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Send private Aokie audio only to the currently leased consult peer.
    /// This uses the peer's WebRTC source and has no route to SCO TX.
    pub fn try_push_consult_output(&self, samples: &[i16], sample_rate: u32) -> bool {
        if samples.is_empty() || sample_rate == 0 {
            return false;
        }
        let normalized = Arc::new(resample_mono(samples, sample_rate, MEDIA_SAMPLE_RATE_HZ));
        if normalized.is_empty() {
            return false;
        }
        let caller_pcm = {
            let Ok(state) = self.inner.state.try_lock() else {
                return false;
            };
            let Some(active) = state.active_consult.as_ref() else {
                return false;
            };
            if !state.allows_consult(&active.binding, Instant::now()) {
                return false;
            }
            state
                .peers
                .get(&active.binding.rtc_session_id)
                .map(|slot| slot.caller_pcm_tx.clone())
        };
        caller_pcm.is_some_and(|caller_pcm| caller_pcm.try_send(normalized).is_ok())
    }

    /// Pop one independently revalidated talk frame for the radio thread.
    pub fn try_recv_talk_pcm(&self) -> Option<OwnedAudioFrame> {
        loop {
            let routed = self
                .inner
                .talk_rx
                .try_lock()
                .ok()
                .and_then(|receiver| receiver.try_recv().ok())?;
            let allowed = self.inner.state.lock().ok().is_some_and(|mut state| {
                let now = Instant::now();
                state.allows_talk(&routed.binding, now)
                    && !state.microphone_is_muted_for(&routed.binding, now)
            });
            self.refresh_reserved();
            if allowed {
                return Some(routed.frame);
            }
            self.inner
                .quarantined_talk_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Consent expiry is a wall-clock transition, not a settings mutation, so
    /// no external config sync is guaranteed to call set_remote_consent at the
    /// instant it occurs. Re-run the same state-aware revocation at the radio
    /// tick seam: prepared Talk cancels in place, while consult/owned caller
    /// routes retain their physical ReturnToAokie handshake.
    fn reconcile_remote_consent_expiry(&self) {
        let expired_gate = self
            .inner
            .state
            .lock()
            .ok()
            .and_then(|state| (!state.consent.is_current()).then(|| state.consent.clone()));
        if let Some(gate) = expired_gate {
            self.set_remote_consent(gate);
        }
    }

    pub fn next_radio_transition(&self) -> Option<RadioTransition> {
        self.reconcile_remote_consent_expiry();
        let (transition, revoked, expired, return_notice, cancelled) = {
            let mut state = self.inner.state.lock().ok()?;
            let now = Instant::now();
            let expired = state.expire_peers(now);
            let (transition, revoked, return_notice, cancelled) = state.next_transition(now);
            (transition, revoked, expired, return_notice, cancelled)
        };
        for slot in expired {
            let _ = slot.actor_tx.try_send(ActorCommand::Revoke);
            let _ = slot.actor_tx.try_send(ActorCommand::Close);
            self.emit(
                slot.binding,
                RemoteMediaEventKind::Closed {
                    reason: "lease_expired".into(),
                },
            );
        }
        if let Some(actor) = revoked {
            let _ = actor.try_send(ActorCommand::Revoke);
        }
        if let Some((slot, reason)) = cancelled {
            let _ = slot.actor_tx.try_send(ActorCommand::Revoke);
            let _ = slot.actor_tx.try_send(ActorCommand::Close);
            self.emit(slot.binding, RemoteMediaEventKind::Closed { reason });
        }
        if let Some((binding, reason)) = return_notice {
            self.emit(binding, RemoteMediaEventKind::ReturningToAokie { reason });
        }
        self.refresh_reserved();
        transition
    }

    /// Physical ACK from the radio loop after flushing every queued Aokie
    /// sample.  Only this operation opens both transmit gates.
    pub fn ack_enter_human(&self, binding: &SessionBinding) -> Result<(), String> {
        let transfer_authorized = {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state
                .pending_route
                .as_ref()
                .filter(|pending| pending.permit.binding == *binding)
                .and_then(|pending| pending.transfer_guard.clone())
        }
        .is_none_or(|guard| {
            crate::assistance::global().transfer_activation_is_current(
                &guard.request_id,
                &guard.offered_fence,
                &guard.device_id,
            )
        });
        let (actor, permit) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.ack_enter(binding, Instant::now(), transfer_authorized)?
        };
        actor
            .try_send(ActorCommand::Authorize(permit))
            .map_err(|error| format!("peer route queue unavailable: {error}"))?;
        self.refresh_reserved();
        self.emit(binding.clone(), RemoteMediaEventKind::HumanActive);
        Ok(())
    }

    /// Physical ACK for the provisional receive-only soft hold.  This
    /// advances ownerEpoch for the gateway claim decision but deliberately
    /// does not create or install a RoutePermit.
    pub fn ack_prepare_human(&self, binding: &SessionBinding) -> Result<(), String> {
        let confirmed_owner_epoch = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.ack_prepare(binding, Instant::now())?
        };
        self.refresh_reserved();
        self.emit(
            binding.clone(),
            RemoteMediaEventKind::TakeoverPrepared {
                confirmed_owner_epoch,
            },
        );
        Ok(())
    }

    pub fn ack_prepare_consult(&self, binding: &SessionBinding) -> Result<(), String> {
        let confirmed_owner_epoch = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.ack_prepare(binding, Instant::now())?
        };
        self.refresh_reserved();
        self.emit(
            binding.clone(),
            RemoteMediaEventKind::ConsultPrepared {
                confirmed_owner_epoch,
            },
        );
        Ok(())
    }

    /// Physical ACK after the radio has flushed the last Aokie sample and
    /// isolated the caller. Only this operation makes private consult active.
    pub fn ack_enter_consult(&self, binding: &SessionBinding) -> Result<(), String> {
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.ack_enter_consult(binding, Instant::now())?;
        }
        self.refresh_reserved();
        self.emit(binding.clone(), RemoteMediaEventKind::ConsultActive);
        Ok(())
    }

    /// Physical ACK after flushing the human tail.  This advances ownerEpoch
    /// again before Aokie may speak, permanently fencing delayed talk frames.
    pub fn ack_return_to_aokie(&self) -> Result<(), String> {
        let binding = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.ack_return()?
        };
        self.refresh_reserved();
        self.emit(binding, RemoteMediaEventKind::AokieActive);
        Ok(())
    }

    pub fn radio_reserved(&self) -> bool {
        self.inner.radio_reserved.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn install_test_prepared_peer(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
    ) -> Result<(), String> {
        let ttl = validate_ttl(lease_ttl_ms)?;
        let (actor_tx, _actor_rx) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
        let (remote_ice_tx, _remote_ice_rx) = mpsc::channel(REMOTE_ICE_QUEUE_CAPACITY);
        let (caller_pcm_tx, _caller_pcm_rx) = mpsc::channel(CALLER_PCM_QUEUE_CAPACITY);
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?;
        state.insert_peer(PeerSlot {
            binding: binding.clone(),
            lease_expires_at: Instant::now() + ttl,
            actor_tx,
            remote_ice_tx,
            caller_pcm_tx,
        })?;
        state.request_soft_hold(binding, ttl)?;
        drop(state);
        self.refresh_reserved();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn install_test_active_talk_peer(
        &self,
        binding: SessionBinding,
        lease_ttl_ms: u64,
    ) -> Result<(), String> {
        let ttl = validate_ttl(lease_ttl_ms)?;
        let (actor_tx, _actor_rx) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
        let (remote_ice_tx, _remote_ice_rx) = mpsc::channel(REMOTE_ICE_QUEUE_CAPACITY);
        let (caller_pcm_tx, _caller_pcm_rx) = mpsc::channel(CALLER_PCM_QUEUE_CAPACITY);
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?;
        state.insert_peer(PeerSlot {
            binding: binding.clone(),
            lease_expires_at: Instant::now() + ttl,
            actor_tx,
            remote_ice_tx,
            caller_pcm_tx,
        })?;
        state.request_takeover(binding.clone(), ttl)?;
        let _ = state.ack_enter(&binding, Instant::now(), false)?;
        drop(state);
        self.refresh_reserved();
        Ok(())
    }

    fn remote_ice_for(
        &self,
        rtc_session_id: &str,
    ) -> Result<mpsc::Sender<IceCandidateSignal>, String> {
        self.inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?
            .peers
            .get(rtc_session_id)
            .map(|slot| slot.remote_ice_tx.clone())
            .ok_or_else(|| format!("unknown or closed rtcSessionId {rtc_session_id:?}"))
    }

    fn refresh_reserved(&self) {
        let reserved = self
            .inner
            .state
            .lock()
            .ok()
            .is_some_and(|state| state.radio_reserved());
        self.inner.radio_reserved.store(reserved, Ordering::Release);
    }

    fn emit(&self, binding: SessionBinding, kind: RemoteMediaEventKind) {
        EventEmitter {
            tx: self.inner.event_tx.clone(),
            sequence: self.inner.event_sequence.clone(),
            dropped: self.inner.dropped_events.clone(),
        }
        .emit(&binding, kind);
    }
}

impl Drop for RemoteMediaInner {
    fn drop(&mut self) {
        let _ = self.manager_tx.try_send(ManagerCommand::Shutdown);
        self.radio_reserved.store(false, Ordering::Release);
    }
}

fn caller_binding_is_current(
    consent: &RemoteConsentGate,
    binding: &SessionBinding,
    lease_expires_at: Instant,
    now: Instant,
    current_call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
) -> bool {
    lease_expires_at > now
        && binding.call_id == current_call_id
        && binding.call_epoch == call_epoch
        && binding.owner_epoch == owner_epoch
        && consent.allows(binding.mode)
}

fn routes_caller_ingress(
    consent: &RemoteConsentGate,
    slot: &PeerSlot,
    now: Instant,
    current_call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
) -> bool {
    caller_binding_is_current(
        consent,
        &slot.binding,
        slot.lease_expires_at,
        now,
        current_call_id,
        call_epoch,
        owner_epoch,
    ) && !matches!(
        slot.binding.mode,
        MediaMode::PreparedConsult | MediaMode::Consult
    )
}

fn routes_caller_output(
    consent: &RemoteConsentGate,
    slot: &PeerSlot,
    now: Instant,
    current_call_id: &str,
    call_epoch: u64,
    owner_epoch: u64,
) -> bool {
    caller_binding_is_current(
        consent,
        &slot.binding,
        slot.lease_expires_at,
        now,
        current_call_id,
        call_epoch,
        owner_epoch,
    ) && slot.binding.mode == MediaMode::Monitor
}

impl RemoteMediaInner {
    fn try_push_sco(&self, samples: &[i16], sample_rate: u32) {
        if samples.is_empty() || sample_rate == 0 {
            return;
        }
        let normalized = Arc::new(resample_mono(samples, sample_rate, MEDIA_SAMPLE_RATE_HZ));
        if normalized.is_empty() {
            return;
        }
        let Ok(mut state) = self.state.try_lock() else {
            self.dropped_sco_frames.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let now = Instant::now();
        let Some(current_id) = state.current_call_id.clone() else {
            return;
        };
        let Some(record) = state.current_record() else {
            return;
        };
        let call_epoch = record.call_epoch;
        let owner_epoch = record.owner_epoch;
        state.observe_caller_audio(&normalized, now);
        for slot in state.peers.values() {
            if !routes_caller_ingress(
                &state.consent,
                slot,
                now,
                &current_id,
                call_epoch,
                owner_epoch,
            ) {
                continue;
            }
            if slot.caller_pcm_tx.try_send(normalized.clone()).is_err() {
                self.dropped_sco_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn try_push_caller_output(
        &self,
        samples: &[i16],
        sample_rate: u32,
        attribution: CallerOutputAttribution,
    ) {
        if samples.is_empty() || sample_rate == 0 {
            return;
        }
        let normalized = Arc::new(resample_mono(samples, sample_rate, MEDIA_SAMPLE_RATE_HZ));
        if normalized.is_empty() {
            return;
        }
        let Ok(mut state) = self.state.try_lock() else {
            self.dropped_sco_frames.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let now = Instant::now();
        let Some(current_id) = state.current_call_id.clone() else {
            return;
        };
        let Some(record) = state.current_record() else {
            return;
        };
        let call_epoch = record.call_epoch;
        let owner_epoch = record.owner_epoch;
        if attribution == CallerOutputAttribution::Aokie {
            state.observe_aokie_audio(&normalized, now);
        }
        for slot in state.peers.values() {
            if !routes_caller_output(
                &state.consent,
                slot,
                now,
                &current_id,
                call_epoch,
                owner_epoch,
            ) {
                continue;
            }
            if slot.caller_pcm_tx.try_send(normalized.clone()).is_err() {
                self.dropped_sco_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Used by the voice playback seam, which can consume SCO frames while the
/// main radio loop is busy speaking.  The lookup and all downstream sends are
/// try-only; audio capture is never blocked by signalling or WebRTC.
pub fn try_capture_sco_globally(samples: &[i16], sample_rate: u32) {
    let Some(slot) = ACTIVE_REMOTE_MEDIA.get() else {
        return;
    };
    let Some(inner) = slot.try_lock().ok().and_then(|active| active.upgrade()) else {
        return;
    };
    inner.try_push_sco(samples, sample_rate);
}

pub fn try_capture_caller_output_globally(samples: &[i16], sample_rate: u32) {
    let Some(slot) = ACTIVE_REMOTE_MEDIA.get() else {
        return;
    };
    let Some(inner) = slot.try_lock().ok().and_then(|active| active.upgrade()) else {
        return;
    };
    inner.try_push_caller_output(samples, sample_rate, CallerOutputAttribution::Aokie);
}

/// Publish one finalized caption from the existing live STT/delivery source.
/// The lane is volatile and bounded; it is never written by the v2 transport.
pub fn publish_caption_globally(
    call_id: &str,
    turn_index: u32,
    speaker: &str,
    text: &str,
    occurred_at: &str,
) {
    let Some(slot) = ACTIVE_REMOTE_MEDIA.get() else {
        return;
    };
    let Some(inner) = slot.try_lock().ok().and_then(|active| active.upgrade()) else {
        return;
    };
    let Ok(mut state) = inner.state.try_lock() else {
        return;
    };
    if state.current_call_id.as_deref() != Some(call_id)
        || !state.consent.is_current()
        || !state.consent.captions_enabled
    {
        return;
    }
    let speaker: String = speaker.chars().take(40).collect();
    let text: String = text.trim().chars().take(2_000).collect();
    if speaker.trim().is_empty() || text.is_empty() {
        return;
    }
    let caption_id = format!(
        "caption_{}_{}",
        state.current_record().map_or(0, |r| r.call_epoch),
        turn_index
    );
    if let Some(existing) = state
        .captions
        .iter_mut()
        .find(|caption| caption.caption_id == caption_id)
    {
        existing.speaker = speaker;
        existing.text = text;
        existing.occurred_at = occurred_at.chars().take(64).collect();
        existing.final_text = true;
        return;
    }
    state.captions.push_back(RemoteCaption {
        caption_id,
        speaker,
        text,
        occurred_at: occurred_at.chars().take(64).collect(),
        final_text: true,
    });
    while state.captions.len() > aokie_protocol::v2::MAX_CAPTIONS {
        state.captions.pop_front();
    }
}

/// Chokepoint used by every Aokie/TTS caller-TX path.  Remote talk audio uses
/// the explicit routed queue and never calls this predicate.
pub fn human_reserves_radio_globally() -> bool {
    // The §12.3 synthetic-audio rig drives the REAL playback engine in an
    // otherwise radio-less test thread; without an opt-out, a PARALLEL
    // companion/takeover test holding the process-global ACTIVE_REMOTE_MEDIA
    // reservation would cancel the rig's playback mid-run (the 2026-07-19
    // flake: rig tests failing in batches only in some full-suite runs).
    #[cfg(test)]
    if test_rig_isolated() {
        return false;
    }
    ACTIVE_REMOTE_MEDIA
        .get()
        .and_then(|slot| slot.try_lock().ok())
        .and_then(|active| active.upgrade())
        .is_some_and(|inner| inner.radio_reserved.load(Ordering::Acquire))
}

#[cfg(test)]
thread_local! {
    static TEST_RIG_ISOLATED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: mark THIS thread as rig-isolated so process-global remote-media
/// gates read as unreserved. Set by the synthetic-audio rig's driver.
#[cfg(test)]
pub(crate) fn set_test_rig_isolated(isolated: bool) {
    TEST_RIG_ISOLATED.with(|c| c.set(isolated));
}

#[cfg(test)]
fn test_rig_isolated() -> bool {
    TEST_RIG_ISOLATED.with(|c| c.get())
}

impl RemoteMediaState {
    fn aokie_owner_fence(&self) -> Option<AokieOwnerFence> {
        if !self.current_call_active
            || self.service_mode != ServiceMode::AokieActive
            || self.radio_reserved()
        {
            return None;
        }
        let call_id = self.current_call_id.as_ref()?;
        let record = self.calls.get(call_id)?;
        Some(AokieOwnerFence {
            call_id: call_id.clone(),
            call_epoch: record.call_epoch,
            action_epoch: self.aokie_action_epoch,
        })
    }

    fn aokie_switch_fence(&self) -> Option<AokieSwitchFence> {
        let owner = self.aokie_owner_fence()?;
        // Monitor is observational. Every other peer/pending claim can become
        // a caller route and therefore excludes a concurrent CHLD topology
        // change, even before exact PCM is ready to reserve the radio.
        let media_claim_exists = self.pending_prepare.is_some()
            || self.prepared_claim.is_some()
            || self.pending_consult.is_some()
            || self.active_consult.is_some()
            || self.pending_route.is_some()
            || self.active_route.is_some()
            || self
                .peers
                .values()
                .any(|slot| slot.binding.mode != MediaMode::Monitor);
        if media_claim_exists {
            return None;
        }
        let owner_epoch = self.current_record()?.owner_epoch;
        Some(AokieSwitchFence {
            owner,
            owner_epoch,
            switch_epoch: self.aokie_switch_epoch,
        })
    }

    fn fence_autonomous_aokie_actions(&mut self) {
        self.aokie_action_epoch = self.aokie_action_epoch.saturating_add(1).max(1);
    }

    fn bump_revision(&mut self) {
        self.remote_revision = self.remote_revision.saturating_add(1).max(1);
    }

    fn current_record(&self) -> Option<&CallRecord> {
        self.current_call_id
            .as_ref()
            .and_then(|call_id| self.calls.get(call_id))
    }

    fn current_record_mut(&mut self) -> Option<&mut CallRecord> {
        let call_id = self.current_call_id.clone()?;
        self.calls.get_mut(&call_id)
    }

    fn participant_state(&self, binding: &SessionBinding) -> RemoteParticipantState {
        if self.binding_is_active_media_owner(binding, Instant::now()) {
            RemoteParticipantState::Active
        } else if binding.mode == MediaMode::Monitor {
            RemoteParticipantState::Connected
        } else {
            // A non-monitor peer is never represented as an active speaker
            // merely because its DTLS transport connected. Until the exact
            // radio-owned route is active it remains prepared.
            RemoteParticipantState::Prepared
        }
    }

    fn binding_is_active_media_owner(&self, binding: &SessionBinding, now: Instant) -> bool {
        if !self.current_call_active
            || self.current_call_id.as_deref() != Some(binding.call_id.as_str())
            || self.current_record().is_none_or(|record| {
                record.call_epoch != binding.call_epoch || record.owner_epoch != binding.owner_epoch
            })
        {
            return false;
        }
        let live_peer = self
            .peers
            .get(&binding.rtc_session_id)
            .is_some_and(|slot| slot.binding == *binding && slot.lease_expires_at > now);
        if !live_peer {
            return false;
        }
        match binding.mode {
            MediaMode::Talk => {
                self.service_mode == ServiceMode::HumanActive
                    && self.consent.allows(MediaMode::Talk)
                    && self.active_route.as_ref().is_some_and(|route| {
                        route.permit.binding == *binding
                            && route.permit.is_current_for(binding, now)
                    })
            }
            MediaMode::Consult => {
                self.service_mode == ServiceMode::ConsultActive
                    && self.consent.allows(MediaMode::Consult)
                    && self
                        .active_consult
                        .as_ref()
                        .is_some_and(|route| route.binding == *binding && route.expires_at > now)
            }
            MediaMode::Monitor | MediaMode::PreparedConsult | MediaMode::PreparedTalk => false,
        }
    }

    fn microphone_is_muted_for(&self, binding: &SessionBinding, now: Instant) -> bool {
        self.microphone_mute_binding.as_ref() == Some(binding)
            && self.binding_is_active_media_owner(binding, now)
    }

    fn set_microphone_muted(
        &mut self,
        binding: &SessionBinding,
        expected_remote_revision: u64,
        muted: bool,
        now: Instant,
    ) -> Result<u64, String> {
        if self.remote_revision != expected_remote_revision {
            return Err("remoteRevision changed before microphone mute was applied".into());
        }
        if !matches!(binding.mode, MediaMode::Consult | MediaMode::Talk)
            || !self.binding_is_active_media_owner(binding, now)
        {
            return Err(
                "only the exact active Companion media owner may change microphone mute".into(),
            );
        }
        let already = self.microphone_mute_binding.as_ref() == Some(binding);
        if already == muted {
            return Ok(self.remote_revision);
        }
        self.microphone_mute_binding = muted.then(|| binding.clone());
        // Buffered PCM received before the mute boundary is no longer
        // meaningful authority evidence after the gate closes.
        if muted {
            self.companion_audio_levels
                .remove(&companion_participant_id(binding));
        }
        self.bump_revision();
        Ok(self.remote_revision)
    }

    fn clear_binding_observations(&mut self, binding: &SessionBinding) {
        if self.microphone_mute_binding.as_ref() == Some(binding) {
            self.microphone_mute_binding = None;
        }
        self.companion_audio_levels
            .remove(&companion_participant_id(binding));
    }

    fn clear_all_media_observations(&mut self) {
        self.microphone_mute_binding = None;
        self.caller_audio_level = None;
        self.aokie_audio_level = None;
        self.companion_audio_levels.clear();
    }

    fn observe_caller_audio(&mut self, samples: &[i16], now: Instant) {
        let (Some(call_id), Some(record)) =
            (self.current_call_id.clone(), self.current_record().cloned())
        else {
            return;
        };
        let (_, level_permille) = pcm_level_summary(samples);
        self.caller_audio_level = Some(ObservedAudioLevel {
            call_id,
            call_epoch: record.call_epoch,
            participant_id: None,
            level_permille,
            observed_at: now,
        });
    }

    fn observe_aokie_audio(&mut self, samples: &[i16], now: Instant) {
        let (Some(call_id), Some(record)) =
            (self.current_call_id.clone(), self.current_record().cloned())
        else {
            return;
        };
        let (_, level_permille) = pcm_level_summary(samples);
        self.aokie_audio_level = Some(ObservedAudioLevel {
            call_id,
            call_epoch: record.call_epoch,
            participant_id: None,
            level_permille,
            observed_at: now,
        });
    }

    fn observe_companion_audio(&mut self, binding: &SessionBinding, samples: &[i16], now: Instant) {
        let participant_id = companion_participant_id(binding);
        let (_, level_permille) = pcm_level_summary(samples);
        self.companion_audio_levels.insert(
            participant_id.clone(),
            ObservedAudioLevel {
                call_id: binding.call_id.clone(),
                call_epoch: binding.call_epoch,
                participant_id: Some(participant_id),
                level_permille,
                observed_at: now,
            },
        );
    }

    fn current_audio_levels(&self, now: Instant) -> Vec<RemoteAudioLevel> {
        let Some(call_id) = self.current_call_id.as_deref() else {
            return Vec::new();
        };
        let Some(record) = self.current_record() else {
            return Vec::new();
        };
        let mut levels = Vec::new();
        if let Some(level) = self
            .caller_audio_level
            .as_ref()
            .and_then(|observed| observed_level_for_call(observed, call_id, record.call_epoch, now))
        {
            levels.push(RemoteAudioLevel {
                source: RemoteAudioLevelSource::Caller,
                participant_id: None,
                level_permille: level,
            });
        }
        if let Some(level) = self
            .aokie_audio_level
            .as_ref()
            .and_then(|observed| observed_level_for_call(observed, call_id, record.call_epoch, now))
        {
            levels.push(RemoteAudioLevel {
                source: RemoteAudioLevelSource::Aokie,
                participant_id: None,
                level_permille: level,
            });
        }
        for observed in self.companion_audio_levels.values() {
            let Some(level_permille) =
                observed_level_for_call(observed, call_id, record.call_epoch, now)
            else {
                continue;
            };
            let Some(participant_id) = observed.participant_id.clone() else {
                continue;
            };
            if !self.peers.values().any(|slot| {
                companion_participant_id(&slot.binding) == participant_id
                    && self.binding_is_active_media_owner(&slot.binding, now)
            }) {
                continue;
            }
            levels.push(RemoteAudioLevel {
                source: RemoteAudioLevelSource::Companion,
                participant_id: Some(participant_id),
                level_permille,
            });
        }
        levels.sort_by(|left, right| left.participant_id.cmp(&right.participant_id));
        levels
    }

    fn observe_call(
        &mut self,
        call_id: Option<&str>,
        active: bool,
    ) -> (
        Vec<mpsc::Sender<ActorCommand>>,
        Vec<(SessionBinding, RemoteMediaEventKind)>,
    ) {
        if self.current_call_id.as_deref() == call_id {
            if self.current_call_active != active {
                self.current_call_active = active;
                if !active {
                    self.clear_all_media_observations();
                }
                self.bump_revision();
            }
            return (Vec::new(), Vec::new());
        }

        let mut actors = Vec::new();
        let mut events = Vec::new();
        for (_, slot) in self.peers.drain() {
            actors.push(slot.actor_tx);
            events.push((
                slot.binding,
                RemoteMediaEventKind::Closed {
                    reason: "physical_call_changed".into(),
                },
            ));
        }
        let old_route = self
            .active_route
            .take()
            .map(|route| route.permit.binding)
            .or_else(|| self.active_consult.take().map(|route| route.binding))
            .or_else(|| self.pending_consult.take().map(|pending| pending.binding))
            .or_else(|| self.pending_route.take().map(|route| route.permit.binding))
            .or_else(|| self.pending_prepare.take().map(|pending| pending.binding))
            .or_else(|| {
                self.prepared_claim
                    .take()
                    .map(|prepared| prepared.provisional_binding)
            });
        self.return_binding = old_route;
        self.return_reason = self
            .return_binding
            .as_ref()
            .map(|_| "physical_call_changed".to_string());
        self.return_dispatched = false;
        self.current_call_id = call_id.map(str::to_string);
        self.captions.clear();
        self.clear_all_media_observations();
        self.current_call_active = active;

        if let Some(call_id) = call_id {
            if !self.calls.contains_key(call_id) {
                self.next_call_epoch = self.next_call_epoch.saturating_add(1).max(1);
                self.calls.insert(
                    call_id.to_string(),
                    CallRecord {
                        call_epoch: self.next_call_epoch,
                        owner_epoch: 0,
                    },
                );
                self.call_order.push_back(call_id.to_string());
                while self.call_order.len() > 16 {
                    if let Some(old) = self.call_order.pop_front() {
                        if self.current_call_id.as_deref() != Some(old.as_str()) {
                            self.calls.remove(&old);
                        }
                    }
                }
            }
        }
        self.service_mode = if self.return_binding.is_some() {
            ServiceMode::Recovering
        } else {
            ServiceMode::AokieActive
        };
        self.bump_revision();
        (actors, events)
    }

    fn validate_open(&self, binding: &SessionBinding) -> Result<(), String> {
        binding.validate().map_err(|error| error.to_string())?;
        if !self.consent.allows(binding.mode) {
            return Err("current remote consent does not permit this media mode".into());
        }
        if !self.current_call_active {
            return Err("the physical call is not active".into());
        }
        let current_id = self
            .current_call_id
            .as_deref()
            .ok_or_else(|| "there is no physical call".to_string())?;
        let record = self
            .current_record()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if binding.call_id != current_id || binding.call_epoch != record.call_epoch {
            return Err("callId/callEpoch does not match physical radio truth".into());
        }
        match binding.mode {
            MediaMode::Monitor | MediaMode::PreparedConsult | MediaMode::PreparedTalk => {
                if binding.owner_epoch != record.owner_epoch {
                    return Err("receive-only ownerEpoch is stale".into());
                }
                if matches!(
                    binding.mode,
                    MediaMode::PreparedTalk | MediaMode::PreparedConsult
                ) && (self.pending_prepare.is_some()
                    || self.prepared_claim.is_some()
                    || self.pending_consult.is_some()
                    || self.active_consult.is_some()
                    || self.pending_route.is_some()
                    || self.active_route.is_some()
                    || self.peers.values().any(|slot| {
                        matches!(
                            slot.binding.mode,
                            MediaMode::PreparedConsult
                                | MediaMode::Consult
                                | MediaMode::PreparedTalk
                                | MediaMode::Talk
                        )
                    }))
                {
                    return Err("a consult/takeover claimant already exists".into());
                }
            }
            MediaMode::Consult => {
                let prepared_matches = self.prepared_claim.as_ref().is_some_and(|prepared| {
                    prepared.provisional_binding.mode == MediaMode::PreparedConsult
                        && prepared.confirmed_owner_epoch == binding.owner_epoch
                        && prepared.provisional_binding.call_id == binding.call_id
                        && prepared.provisional_binding.call_epoch == binding.call_epoch
                        && prepared.provisional_binding.device_id == binding.device_id
                        && prepared.provisional_binding.rtc_session_id == binding.rtc_session_id
                        && prepared.provisional_binding.lease_id == binding.lease_id
                        && prepared.provisional_binding.fence == binding.fence
                });
                if !prepared_matches || binding.owner_epoch != record.owner_epoch {
                    return Err(
                        "active consult binding does not match the prepared hold epoch".into(),
                    );
                }
                if self.active_consult.is_some()
                    || self.pending_consult.is_some()
                    || self.pending_route.is_some()
                    || self.active_route.is_some()
                    || self
                        .peers
                        .values()
                        .any(|slot| slot.binding.mode == MediaMode::Consult)
                {
                    return Err("a private consultation is already active".into());
                }
            }
            MediaMode::Talk => {
                let prepared_matches = self.prepared_claim.as_ref().is_some_and(|prepared| {
                    prepared.confirmed_owner_epoch == binding.owner_epoch
                        && prepared.provisional_binding.call_id == binding.call_id
                        && prepared.provisional_binding.call_epoch == binding.call_epoch
                        && prepared.provisional_binding.device_id == binding.device_id
                        && prepared.provisional_binding.rtc_session_id == binding.rtc_session_id
                        && prepared.provisional_binding.lease_id == binding.lease_id
                        && prepared.provisional_binding.fence == binding.fence
                });
                if !prepared_matches || binding.owner_epoch != record.owner_epoch {
                    return Err(
                        "active talk binding does not match the prepared owner epoch".into(),
                    );
                }
                if self
                    .peers
                    .values()
                    .any(|slot| slot.binding.mode == MediaMode::Talk)
                    || self.pending_route.is_some()
                    || self.active_route.is_some()
                {
                    return Err("a talk claimant already exists".into());
                }
            }
        }
        if self.peers.len() >= MAX_REMOTE_PEERS {
            return Err(format!(
                "at most {MAX_REMOTE_PEERS} Companion peers are allowed"
            ));
        }
        if self.peers.contains_key(&binding.rtc_session_id) {
            return Err("rtcSessionId already exists".into());
        }
        Ok(())
    }

    fn reserve_open_after_switch_epoch(
        &self,
        binding: &SessionBinding,
        expected_switch_epoch: Option<u64>,
    ) -> Result<(), String> {
        self.validate_open(binding)?;
        if binding.mode != MediaMode::Monitor {
            let expected = expected_switch_epoch.ok_or_else(|| {
                "non-monitor media open omitted its physical switchboard proof".to_string()
            })?;
            if self.aokie_switch_epoch != expected {
                return Err(
                    "physical switchboard changed before the media manager admitted the peer"
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    fn insert_peer(&mut self, slot: PeerSlot) -> Result<(), String> {
        // Revalidate after the asynchronous SDP operation.  A call/owner
        // transition that raced answer creation makes the answer unusable.
        self.validate_open(&slot.binding)?;
        if matches!(slot.binding.mode, MediaMode::Consult | MediaMode::Talk) {
            let prepared = self
                .prepared_claim
                .as_mut()
                .ok_or_else(|| "active media peer has no prepared claim".to_string())?;
            // Separate from the renewable lease/peer expiry: a client cannot
            // heartbeat forever while declining permission or never sending
            // microphone PCM.
            prepared.media_ready_deadline = Some(Instant::now() + TALK_PREFLIGHT_TIMEOUT);
            // Timeout/revocation events must carry the exact post-rotation
            // Talk binding.  The provisional binding has the prior owner
            // epoch and is intentionally rejected by the gateway's route
            // fence.
            prepared.active_media_binding = Some(slot.binding.clone());
            prepared.last_media_received_at = None;
        }
        self.peers.insert(slot.binding.rtc_session_id.clone(), slot);
        self.bump_revision();
        Ok(())
    }

    fn insert_peer_after_switch_epoch(
        &mut self,
        slot: PeerSlot,
        expected_switch_epoch: Option<u64>,
    ) -> Result<(), String> {
        if expected_switch_epoch.is_some_and(|expected| self.aokie_switch_epoch != expected) {
            return Err(
                "physical switchboard changed while the media peer was opening".to_string(),
            );
        }
        self.insert_peer(slot)
    }

    fn request_soft_hold(
        &mut self,
        binding: SessionBinding,
        requested_ttl: Duration,
    ) -> Result<(), String> {
        if !matches!(
            binding.mode,
            MediaMode::PreparedConsult | MediaMode::PreparedTalk
        ) {
            return Err("only a prepared consult/talk peer may request a soft hold".into());
        }
        if !self.consent.allows(binding.mode) {
            return Err("current remote consent no longer permits this preparation".into());
        }
        self.validate_live_binding(&binding)?;
        let record = self
            .current_record()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if binding.owner_epoch != record.owner_epoch {
            return Err("prepared ownerEpoch is not current".into());
        }
        if self.pending_prepare.is_some()
            || self.prepared_claim.is_some()
            || self.pending_route.is_some()
            || self.active_route.is_some()
        {
            return Err("caller ownership is already pending, prepared, or active".into());
        }
        let slot = self
            .peers
            .get(&binding.rtc_session_id)
            .ok_or_else(|| "prepared peer is not open".to_string())?;
        if slot.binding != binding {
            return Err("soft-hold binding differs from the immutable peer binding".into());
        }
        let remaining = slot
            .lease_expires_at
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "prepared lease is expired".to_string())?;
        self.pending_prepare = Some(PendingPrepare {
            binding: binding.clone(),
            expires_at: Instant::now() + requested_ttl.min(remaining),
            transition_dispatched: false,
        });
        // Preparation is negotiation, not caller ownership. Keep the
        // receptionist live while the active lease and exact microphone path
        // are established. Only the final PCM-proven transition may reserve
        // the radio for either Talk or private Consult.
        self.service_mode = ServiceMode::AokieActive;
        self.bump_revision();
        Ok(())
    }

    fn request_consult(
        &mut self,
        binding: SessionBinding,
        requested_ttl: Duration,
    ) -> Result<(), String> {
        if binding.mode != MediaMode::Consult {
            return Err("only an active consult peer may enter private consultation".into());
        }
        if !self.consent.allows(MediaMode::Consult) {
            return Err("current remote consent no longer permits consultation".into());
        }
        self.validate_live_binding(&binding)?;
        let record = self
            .current_record()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if binding.owner_epoch != record.owner_epoch {
            return Err("consult ownerEpoch is not the prepared hold epoch".into());
        }
        if self.pending_consult.is_some()
            || self.active_consult.is_some()
            || self.pending_route.is_some()
            || self.active_route.is_some()
        {
            return Err("a consult/takeover route is already active".into());
        }
        let slot = self
            .peers
            .get(&binding.rtc_session_id)
            .ok_or_else(|| "consult peer is not open".to_string())?;
        if slot.binding != binding {
            return Err("consult binding differs from the immutable peer binding".into());
        }
        let prepared = self
            .prepared_claim
            .as_ref()
            .ok_or_else(|| "consultation has not completed receive-only preparation".to_string())?;
        if prepared.expires_at <= Instant::now()
            || prepared
                .media_ready_deadline
                .is_none_or(|deadline| deadline <= Instant::now())
            || prepared.active_media_binding.as_ref() != Some(&binding)
            || prepared.last_media_received_at.is_none()
            || prepared.provisional_binding.mode != MediaMode::PreparedConsult
            || prepared.confirmed_owner_epoch != binding.owner_epoch
            || prepared.provisional_binding.call_id != binding.call_id
            || prepared.provisional_binding.call_epoch != binding.call_epoch
            || prepared.provisional_binding.device_id != binding.device_id
            || prepared.provisional_binding.rtc_session_id != binding.rtc_session_id
            || prepared.provisional_binding.lease_id != binding.lease_id
            || prepared.provisional_binding.fence != binding.fence
        {
            return Err("active consult binding does not match the prepared claim".into());
        }
        let remaining = slot
            .lease_expires_at
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "consult lease is expired".to_string())?;
        self.pending_consult = Some(PendingConsult {
            binding,
            expires_at: Instant::now() + requested_ttl.min(remaining),
            transition_dispatched: false,
            last_received_at: prepared.last_media_received_at,
        });
        self.prepared_claim = None;
        // Exact decoded microphone PCM is now proven. Reserve new Aokie TX
        // while the radio performs the final flush/ACK on its next tick.
        self.fence_autonomous_aokie_actions();
        self.service_mode = ServiceMode::ConsultPending;
        self.bump_revision();
        Ok(())
    }

    fn request_takeover(
        &mut self,
        binding: SessionBinding,
        requested_ttl: Duration,
    ) -> Result<(), String> {
        self.request_takeover_guarded(binding, requested_ttl, None)
    }

    fn request_takeover_guarded(
        &mut self,
        binding: SessionBinding,
        requested_ttl: Duration,
        transfer_guard: Option<TransferActivationGuard>,
    ) -> Result<(), String> {
        if binding.mode != MediaMode::Talk {
            return Err("only a talk peer may request caller ownership".into());
        }
        if !self.consent.allows(MediaMode::Talk) {
            return Err("current remote consent no longer permits takeover".into());
        }
        self.validate_live_binding(&binding)?;
        let record = self
            .current_record()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if binding.owner_epoch != record.owner_epoch {
            return Err("talk ownerEpoch is not the prepared physical owner epoch".into());
        }
        if self.pending_route.is_some() || self.active_route.is_some() {
            return Err("caller ownership is already pending or active".into());
        }
        let slot = self
            .peers
            .get(&binding.rtc_session_id)
            .ok_or_else(|| "talk peer is not open".to_string())?;
        if slot.binding != binding {
            return Err("takeover binding differs from the immutable peer binding".into());
        }
        let prepared = self
            .prepared_claim
            .as_ref()
            .ok_or_else(|| "takeover has not completed its receive-only preparation".to_string())?;
        if prepared.expires_at <= Instant::now()
            || prepared
                .media_ready_deadline
                .is_none_or(|deadline| deadline <= Instant::now())
            || prepared.active_media_binding.as_ref() != Some(&binding)
            || prepared.confirmed_owner_epoch != binding.owner_epoch
            || prepared.provisional_binding.call_id != binding.call_id
            || prepared.provisional_binding.call_epoch != binding.call_epoch
            || prepared.provisional_binding.device_id != binding.device_id
            || prepared.provisional_binding.rtc_session_id != binding.rtc_session_id
            || prepared.provisional_binding.lease_id != binding.lease_id
            || prepared.provisional_binding.fence != binding.fence
        {
            return Err("active talk binding does not match the prepared claim".into());
        }
        let remaining = slot
            .lease_expires_at
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "talk lease is expired".to_string())?;
        let permit = RoutePermit::new(binding, requested_ttl.min(remaining))
            .map_err(|error| error.to_string())?;
        self.pending_route = Some(PendingRoute {
            permit,
            transition_dispatched: false,
            transfer_guard,
        });
        self.prepared_claim = None;
        self.fence_autonomous_aokie_actions();
        self.service_mode = ServiceMode::HumanPending;
        self.bump_revision();
        Ok(())
    }

    fn renew(
        &mut self,
        binding: &SessionBinding,
        ttl: Duration,
    ) -> Result<(Option<mpsc::Sender<ActorCommand>>, Option<RoutePermit>), String> {
        self.validate_live_binding(binding)?;
        let actor = if let Some(slot) = self.peers.get_mut(&binding.rtc_session_id) {
            if slot.binding != *binding {
                return Err("renew binding differs from the immutable peer binding".into());
            }
            slot.lease_expires_at = Instant::now() + ttl;
            Some(slot.actor_tx.clone())
        } else {
            None
        };
        let mut renewed_prepared = false;
        if let Some(prepared) = self.prepared_claim.as_mut() {
            let original = &prepared.provisional_binding;
            let same_stable_claim = original.call_id == binding.call_id
                && original.call_epoch == binding.call_epoch
                && original.device_id == binding.device_id
                && original.rtc_session_id == binding.rtc_session_id
                && original.lease_id == binding.lease_id
                && original.fence == binding.fence;
            if same_stable_claim {
                prepared.expires_at = Instant::now() + ttl;
                renewed_prepared = true;
            }
        }
        if actor.is_none() && !renewed_prepared {
            return Err("peer is not open".to_string());
        }
        let permit = if self
            .active_route
            .as_ref()
            .is_some_and(|route| route.permit.binding == *binding)
        {
            let permit =
                RoutePermit::new(binding.clone(), ttl).map_err(|error| error.to_string())?;
            if let Some(route) = self.active_route.as_mut() {
                // Renewal extends authority but cannot erase the Desktop-owned
                // no-first/ongoing-PCM watchdog evidence.
                route.permit = permit.clone();
            }
            Some(permit)
        } else if let Some(pending) = self.pending_route.as_mut() {
            if pending.permit.binding == *binding {
                pending.permit =
                    RoutePermit::new(binding.clone(), ttl).map_err(|error| error.to_string())?;
            }
            None
        } else {
            None
        };
        if let Some(active) = self.active_consult.as_mut() {
            if active.binding == *binding {
                active.expires_at = Instant::now() + ttl;
            }
        }
        if let Some(pending) = self.pending_consult.as_mut() {
            if pending.binding == *binding {
                pending.expires_at = Instant::now() + ttl;
            }
        }
        self.bump_revision();
        Ok((actor, permit))
    }

    fn request_revoke(
        &mut self,
        binding: &SessionBinding,
        reason: &str,
    ) -> Result<
        (
            Option<mpsc::Sender<ActorCommand>>,
            SessionBinding,
            bool,
            bool,
        ),
        String,
    > {
        self.validate_live_binding(binding)?;
        let active_or_pending_route = self
            .active_consult
            .as_ref()
            .is_some_and(|route| route.binding == *binding)
            || self
                .active_route
                .as_ref()
                .is_some_and(|route| route.permit.binding == *binding)
            || self
                .pending_route
                .as_ref()
                .is_some_and(|route| route.permit.binding == *binding);
        let pending_consult = self
            .pending_consult
            .as_ref()
            .is_some_and(|pending| pending.binding == *binding);
        let pending_prepare_mode = self
            .pending_prepare
            .as_ref()
            .filter(|pending| pending.binding == *binding)
            .map(|pending| pending.binding.mode);
        let prepared_mode = self.prepared_claim.as_ref().and_then(|prepared| {
            let original = &prepared.provisional_binding;
            let stable_match = original == binding
                || (original.call_id == binding.call_id
                    && original.call_epoch == binding.call_epoch
                    && original.device_id == binding.device_id
                    && original.rtc_session_id == binding.rtc_session_id
                    && original.lease_id == binding.lease_id
                    && original.fence == binding.fence);
            stable_match.then_some(original.mode)
        });
        let matches_route = active_or_pending_route
            || pending_consult
            || pending_prepare_mode.is_some()
            || prepared_mode.is_some();
        if !matches_route {
            return Err("binding does not own or await the caller route".into());
        }
        let needs_return = active_or_pending_route;
        let peer_binding = self
            .peers
            .get(&binding.rtc_session_id)
            .map(|slot| slot.binding.clone())
            .unwrap_or_else(|| binding.clone());
        let actor = if needs_return {
            self.peers
                .get(&binding.rtc_session_id)
                .map(|slot| slot.actor_tx.clone())
        } else {
            self.peers
                .remove(&binding.rtc_session_id)
                .map(|slot| slot.actor_tx)
        };
        self.pending_prepare = None;
        self.prepared_claim = None;
        self.pending_consult = None;
        self.active_consult = None;
        self.pending_route = None;
        self.active_route = None;
        self.talk_audio_binding = None;
        self.clear_binding_observations(binding);
        if needs_return {
            self.return_binding = Some(binding.clone());
            self.return_reason = Some(sanitize_reason(reason));
            self.return_dispatched = false;
            self.service_mode = ServiceMode::ReturningToAokie;
        } else {
            // Prepared/PCM-proven preflight never owned or muted the caller
            // route. Cancelling it must be an in-place peer retirement, not a
            // fabricated radio return/flush that interrupts the receptionist.
            self.fence_cancelled_binding(&peer_binding);
            self.service_mode = ServiceMode::AokieActive;
        }
        self.bump_revision();
        Ok((actor, peer_binding, needs_return, !needs_return))
    }

    fn close_peer(
        &mut self,
        rtc_session_id: &str,
        reason: &str,
    ) -> Result<(PeerSlot, bool), String> {
        let slot = self
            .peers
            .remove(rtc_session_id)
            .ok_or_else(|| format!("unknown or closed rtcSessionId {rtc_session_id:?}"))?;
        self.clear_binding_observations(&slot.binding);
        let active_route_needs_return = self
            .active_consult
            .as_ref()
            .is_some_and(|route| route.binding == slot.binding)
            || self
                .active_route
                .as_ref()
                .is_some_and(|route| route.permit.binding == slot.binding)
            || self
                .pending_route
                .as_ref()
                .is_some_and(|route| route.permit.binding == slot.binding);
        let pending_prepare_mode = self.pending_prepare.as_ref().and_then(|pending| {
            (pending.binding.rtc_session_id == rtc_session_id).then_some(pending.binding.mode)
        });
        let pending_consult = self
            .pending_consult
            .as_ref()
            .is_some_and(|pending| pending.binding.rtc_session_id == rtc_session_id);
        let preserves_prepared_claim = matches!(reason, "active_rebind" | "awaiting_active_rebind");
        let prepared_mode = (!preserves_prepared_claim)
            .then(|| {
                self.prepared_claim.as_ref().and_then(|prepared| {
                    (prepared.provisional_binding.rtc_session_id == rtc_session_id)
                        .then_some(prepared.provisional_binding.mode)
                })
            })
            .flatten();
        let needs_return = active_route_needs_return;
        let cancels_preflight =
            pending_consult || pending_prepare_mode.is_some() || prepared_mode.is_some();
        if pending_prepare_mode.is_some() {
            self.pending_prepare = None;
        }
        if prepared_mode.is_some() {
            self.prepared_claim = None;
        }
        if pending_consult {
            self.pending_consult = None;
        }
        if needs_return {
            self.pending_route = None;
            self.active_route = None;
            self.active_consult = None;
            self.clear_binding_observations(&slot.binding);
            self.return_binding = Some(slot.binding.clone());
            self.return_reason = Some(sanitize_reason(reason));
            self.return_dispatched = false;
            self.service_mode = ServiceMode::ReturningToAokie;
        } else if cancels_preflight {
            self.fence_cancelled_binding(&slot.binding);
            self.service_mode = ServiceMode::AokieActive;
        }
        self.bump_revision();
        Ok((slot, needs_return))
    }

    fn fail_closed_all(&mut self, reason: &str) -> (Vec<PeerSlot>, Option<SessionBinding>) {
        let slots = self.peers.drain().map(|(_, slot)| slot).collect::<Vec<_>>();
        self.clear_all_media_observations();
        let pending_prepare = self.pending_prepare.take();
        let prepared_claim = self.prepared_claim.take();
        let pending_consult = self.pending_consult.take();
        let cancelled_binding = pending_consult
            .as_ref()
            .map(|pending| pending.binding.clone())
            .or_else(|| {
                prepared_claim.as_ref().and_then(|prepared| {
                    prepared
                        .active_media_binding
                        .clone()
                        .or_else(|| Some(prepared.provisional_binding.clone()))
                })
            })
            .or_else(|| {
                pending_prepare
                    .as_ref()
                    .map(|pending| pending.binding.clone())
            });
        let returning = self
            .active_route
            .take()
            .map(|route| route.permit.binding)
            .or_else(|| self.active_consult.take().map(|route| route.binding))
            .or_else(|| self.pending_route.take().map(|route| route.permit.binding));
        if let Some(binding) = returning.clone() {
            self.return_binding = Some(binding);
            self.return_reason = Some(sanitize_reason(reason));
            self.return_dispatched = false;
            self.service_mode = ServiceMode::ReturningToAokie;
        } else if let Some(binding) = cancelled_binding {
            self.fence_cancelled_binding(&binding);
            self.service_mode = ServiceMode::AokieActive;
        }
        self.bump_revision();
        (slots, returning)
    }

    fn fence_cancelled_binding(&mut self, binding: &SessionBinding) {
        if self.current_call_id.as_deref() == Some(binding.call_id.as_str()) {
            if let Some(record) = self.current_record_mut() {
                if record.call_epoch == binding.call_epoch {
                    record.owner_epoch = record
                        .owner_epoch
                        .max(binding.owner_epoch)
                        .saturating_add(1);
                }
            }
        }
    }

    fn validate_live_binding(&self, binding: &SessionBinding) -> Result<(), String> {
        binding.validate().map_err(|error| error.to_string())?;
        let record = self
            .current_record()
            .ok_or_else(|| "there is no physical call".to_string())?;
        if self.current_call_id.as_deref() != Some(binding.call_id.as_str())
            || record.call_epoch != binding.call_epoch
        {
            return Err("call binding is stale".into());
        }
        Ok(())
    }

    fn next_transition(
        &mut self,
        now: Instant,
    ) -> (
        Option<RadioTransition>,
        Option<mpsc::Sender<ActorCommand>>,
        Option<(SessionBinding, String)>,
        Option<(PeerSlot, String)>,
    ) {
        let mut revoked = None;
        let mut return_notice = None;
        let mut cancelled = None;
        if let Some(active) = self.active_consult.as_ref() {
            let microphone_muted = self.microphone_mute_binding.as_ref() == Some(&active.binding);
            let peer_expired = self
                .peers
                .get(&active.binding.rtc_session_id)
                .is_none_or(|slot| slot.lease_expires_at <= now);
            let timeout_reason = if active.expires_at <= now || peer_expired {
                Some("consult_lease_expired")
            } else if !microphone_muted
                && active.last_received_at.is_none()
                && now.saturating_duration_since(active.activated_at) >= FIRST_CONSULT_PCM_TIMEOUT
            {
                Some("consult_audio_never_arrived")
            } else if !microphone_muted
                && active.last_received_at.is_some_and(|last| {
                    now.saturating_duration_since(last) >= ONGOING_CONSULT_PCM_TIMEOUT
                })
            {
                Some("consult_audio_stalled")
            } else {
                None
            };
            if let Some(reason) = timeout_reason {
                let binding = active.binding.clone();
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.active_consult = None;
                self.clear_binding_observations(&binding);
                let reason = reason.to_string();
                self.return_binding = Some(binding.clone());
                self.return_reason = Some(reason.clone());
                return_notice = Some((binding, reason));
                self.return_dispatched = false;
                self.service_mode = ServiceMode::ReturningToAokie;
                self.bump_revision();
            }
        }
        if let Some(active) = self.active_route.as_ref() {
            let microphone_muted =
                self.microphone_mute_binding.as_ref() == Some(&active.permit.binding);
            let peer_expired = self
                .peers
                .get(&active.permit.binding.rtc_session_id)
                .is_none_or(|slot| slot.lease_expires_at <= now);
            let timeout_reason =
                if !active.permit.is_current_for(&active.permit.binding, now) || peer_expired {
                    Some("lease_expired")
                } else if !microphone_muted
                    && active.last_forwarded_at.is_none()
                    && now.saturating_duration_since(active.activated_at) >= FIRST_TALK_PCM_TIMEOUT
                {
                    Some("talk_audio_never_arrived")
                } else if !microphone_muted
                    && active.last_forwarded_at.is_some_and(|last| {
                        now.saturating_duration_since(last) >= ONGOING_TALK_PCM_TIMEOUT
                    })
                {
                    Some("talk_audio_stalled")
                } else {
                    None
                };
            if let Some(reason) = timeout_reason {
                let binding = active.permit.binding.clone();
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.active_route = None;
                self.clear_binding_observations(&binding);
                let reason = reason.to_string();
                self.return_binding = Some(binding.clone());
                self.return_reason = Some(reason.clone());
                return_notice = Some((binding, reason));
                self.return_dispatched = false;
                self.service_mode = ServiceMode::ReturningToAokie;
                self.bump_revision();
            }
        }
        if let Some(pending) = self.pending_route.as_ref() {
            let peer_expired = self
                .peers
                .get(&pending.permit.binding.rtc_session_id)
                .is_none_or(|slot| slot.lease_expires_at <= now);
            let transfer_expired = pending
                .transfer_guard
                .as_ref()
                .is_some_and(|guard| guard.deadline <= now);
            if !pending.permit.is_current_for(&pending.permit.binding, now)
                || peer_expired
                || transfer_expired
            {
                let binding = pending.permit.binding.clone();
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.pending_route = None;
                let reason = if transfer_expired {
                    "transfer_expired_before_physical_ack"
                } else {
                    "lease_expired_before_physical_ack"
                }
                .to_string();
                self.return_binding = Some(binding.clone());
                self.return_reason = Some(reason.clone());
                return_notice = Some((binding, reason));
                self.return_dispatched = false;
                self.service_mode = ServiceMode::ReturningToAokie;
                self.bump_revision();
            }
        }
        if let Some(pending) = self.pending_consult.as_ref() {
            let peer_expired = self
                .peers
                .get(&pending.binding.rtc_session_id)
                .is_none_or(|slot| slot.lease_expires_at <= now);
            if pending.expires_at <= now || peer_expired {
                let binding = pending.binding.clone();
                self.pending_consult = None;
                let reason = "consult_expired_before_physical_ack".to_string();
                if let Some(slot) = self.peers.remove(&binding.rtc_session_id) {
                    cancelled = Some((slot, reason));
                }
                self.fence_cancelled_binding(&binding);
                self.service_mode = ServiceMode::AokieActive;
                self.bump_revision();
            }
        }
        if let Some(pending) = self.pending_prepare.as_ref() {
            let peer_expired = self
                .peers
                .get(&pending.binding.rtc_session_id)
                .is_none_or(|slot| slot.lease_expires_at <= now);
            if pending.expires_at <= now || peer_expired {
                let binding = pending.binding.clone();
                self.pending_prepare = None;
                let reason = "prepared_lease_expired".to_string();
                if let Some(slot) = self.peers.remove(&binding.rtc_session_id) {
                    cancelled = Some((slot, reason));
                }
                self.fence_cancelled_binding(&binding);
                self.service_mode = ServiceMode::AokieActive;
                self.bump_revision();
            }
        }
        if let Some(prepared) = self.prepared_claim.as_ref() {
            let reason = if prepared
                .media_ready_deadline
                .is_some_and(|deadline| deadline <= now)
            {
                Some("talk_readiness_timeout")
            } else if prepared.expires_at <= now {
                Some("prepared_lease_expired")
            } else {
                None
            };
            if let Some(reason) = reason {
                let provisional_mode = prepared.provisional_binding.mode;
                let binding = prepared
                    .active_media_binding
                    .clone()
                    .unwrap_or_else(|| prepared.provisional_binding.clone());
                self.prepared_claim = None;
                let reason = reason.to_string();
                if let Some(slot) = self.peers.remove(&binding.rtc_session_id) {
                    cancelled = Some((slot, reason));
                }
                let _ = provisional_mode;
                self.fence_cancelled_binding(&binding);
                self.service_mode = ServiceMode::AokieActive;
                self.bump_revision();
            }
        }
        if let Some(binding) = self.return_binding.clone() {
            if !self.return_dispatched {
                self.return_dispatched = true;
                return (
                    Some(RadioTransition::ReturnToAokie {
                        reason: self
                            .return_reason
                            .clone()
                            .unwrap_or_else(|| "route_revoked".into()),
                    }),
                    revoked,
                    return_notice,
                    cancelled,
                );
            }
            let _ = binding;
            return (None, revoked, return_notice, cancelled);
        }
        if let Some(pending) = self.pending_route.as_mut() {
            if !pending.transition_dispatched {
                pending.transition_dispatched = true;
                return (
                    Some(RadioTransition::EnterHuman {
                        binding: pending.permit.binding.clone(),
                    }),
                    revoked,
                    return_notice,
                    cancelled,
                );
            }
        }
        if let Some(pending) = self.pending_consult.as_mut() {
            if !pending.transition_dispatched {
                pending.transition_dispatched = true;
                return (
                    Some(RadioTransition::EnterConsult {
                        binding: pending.binding.clone(),
                    }),
                    revoked,
                    return_notice,
                    cancelled,
                );
            }
        }
        if let Some(pending) = self.pending_prepare.as_mut() {
            if !pending.transition_dispatched {
                pending.transition_dispatched = true;
                let transition = if pending.binding.mode == MediaMode::PreparedConsult {
                    RadioTransition::PrepareConsult {
                        binding: pending.binding.clone(),
                    }
                } else {
                    RadioTransition::PrepareHuman {
                        binding: pending.binding.clone(),
                    }
                };
                return (Some(transition), revoked, return_notice, cancelled);
            }
        }
        (None, revoked, return_notice, cancelled)
    }

    fn expire_peers(&mut self, now: Instant) -> Vec<PeerSlot> {
        let expired_ids = self
            .peers
            .iter()
            .filter_map(|(id, slot)| (slot.lease_expires_at <= now).then(|| id.clone()))
            .collect::<Vec<_>>();
        let expired = expired_ids
            .into_iter()
            .filter_map(|id| self.peers.remove(&id))
            .collect::<Vec<_>>();
        if !expired.is_empty() {
            self.bump_revision();
        }
        expired
    }

    fn ack_prepare(&mut self, binding: &SessionBinding, now: Instant) -> Result<u64, String> {
        self.validate_live_binding(binding)?;
        if !self.consent.allows(binding.mode) {
            return Err("remote consent expired before soft-hold ACK".into());
        }
        if !self.current_call_active {
            return Err("physical call ended before soft-hold ACK".into());
        }
        let pending = self
            .pending_prepare
            .take()
            .ok_or_else(|| "there is no pending soft hold".to_string())?;
        if pending.binding != *binding || pending.expires_at <= now {
            self.pending_prepare = Some(pending);
            return Err("pending soft-hold binding is stale or expired".into());
        }
        let record = self
            .current_record_mut()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if record.owner_epoch != binding.owner_epoch {
            self.pending_prepare = Some(pending);
            return Err("ownerEpoch changed before soft-hold ACK".into());
        }
        let confirmed_owner_epoch = record.owner_epoch.saturating_add(1);
        record.owner_epoch = confirmed_owner_epoch;
        self.prepared_claim = Some(PreparedClaim {
            provisional_binding: binding.clone(),
            confirmed_owner_epoch,
            expires_at: pending.expires_at,
            media_ready_deadline: None,
            active_media_binding: None,
            last_media_received_at: None,
        });
        // Owner epoch advances to fence the active lease, but caller audio
        // remains with Aokie until exact Consult/Talk PCM has been decoded.
        self.service_mode = ServiceMode::AokieActive;
        self.bump_revision();
        Ok(confirmed_owner_epoch)
    }

    fn ack_enter_consult(&mut self, binding: &SessionBinding, now: Instant) -> Result<(), String> {
        self.validate_live_binding(binding)?;
        if !self.consent.allows(MediaMode::Consult) {
            return Err("remote consent expired before consult ACK".into());
        }
        if !self.current_call_active {
            return Err("physical call ended before consult ACK".into());
        }
        let pending = self
            .pending_consult
            .take()
            .ok_or_else(|| "there is no pending private consultation".to_string())?;
        if pending.binding != *binding || pending.expires_at <= now {
            self.pending_consult = Some(pending);
            return Err("pending consult binding is stale or expired".into());
        }
        let record = self
            .current_record()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if binding.owner_epoch != record.owner_epoch {
            self.pending_consult = Some(pending);
            return Err("ownerEpoch changed before consult ACK".into());
        }
        let slot = self
            .peers
            .get(&binding.rtc_session_id)
            .ok_or_else(|| "consult peer closed before physical ACK".to_string())?;
        if slot.binding != *binding || slot.lease_expires_at <= now {
            self.pending_consult = Some(pending);
            return Err("consult peer changed or expired before physical ACK".into());
        }
        self.active_consult = Some(ActiveConsult {
            binding: binding.clone(),
            expires_at: pending.expires_at.min(slot.lease_expires_at),
            activated_at: now,
            last_received_at: pending.last_received_at,
        });
        self.service_mode = ServiceMode::ConsultActive;
        self.bump_revision();
        Ok(())
    }

    fn ack_enter(
        &mut self,
        binding: &SessionBinding,
        now: Instant,
        transfer_authorized: bool,
    ) -> Result<(mpsc::Sender<ActorCommand>, RoutePermit), String> {
        self.validate_live_binding(binding)?;
        if !self.consent.allows(MediaMode::Talk) {
            return Err("remote consent expired before takeover ACK".into());
        }
        if !self.current_call_active {
            return Err("physical call ended before takeover ACK".into());
        }
        let pending = self
            .pending_route
            .take()
            .ok_or_else(|| "there is no pending takeover".to_string())?;
        if pending.transfer_guard.as_ref().is_some_and(|guard| {
            !transfer_authorized
                || guard.deadline <= now
                || guard.device_id != binding.device_id
                || guard.offered_fence.call_id != binding.call_id
                || guard.offered_fence.call_epoch != binding.call_epoch
        }) {
            self.pending_route = Some(pending);
            return Err("accepted transfer expired or changed before physical ACK".into());
        }
        if pending.permit.binding != *binding || !pending.permit.is_current_for(binding, now) {
            self.pending_route = Some(pending);
            return Err("pending takeover binding is stale or expired".into());
        }
        let actor = self
            .peers
            .get(&binding.rtc_session_id)
            .ok_or_else(|| "talk peer closed before physical ACK".to_string())?
            .actor_tx
            .clone();
        let record = self
            .current_record()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if binding.owner_epoch != record.owner_epoch {
            return Err("ownerEpoch changed before physical ACK".into());
        }
        let permit = pending.permit.clone();
        self.active_route = Some(ActiveRoute {
            permit: pending.permit,
            activated_at: now,
            last_forwarded_at: None,
        });
        self.service_mode = ServiceMode::HumanActive;
        self.bump_revision();
        Ok((actor, permit))
    }

    fn ack_return(&mut self) -> Result<SessionBinding, String> {
        let binding = self
            .return_binding
            .take()
            .ok_or_else(|| "there is no pending return".to_string())?;
        if self.current_call_id.as_deref() == Some(binding.call_id.as_str()) {
            if let Some(record) = self.current_record_mut() {
                record.owner_epoch = record
                    .owner_epoch
                    .max(binding.owner_epoch)
                    .saturating_add(1);
            }
        }
        self.pending_route = None;
        self.active_route = None;
        self.active_consult = None;
        self.pending_consult = None;
        self.pending_prepare = None;
        self.prepared_claim = None;
        self.clear_binding_observations(&binding);
        self.return_reason = None;
        self.return_dispatched = false;
        self.service_mode = ServiceMode::AokieActive;
        self.bump_revision();
        Ok(binding)
    }

    fn allows_talk(&mut self, binding: &SessionBinding, now: Instant) -> bool {
        let Some(route) = self.active_route.as_ref() else {
            return false;
        };
        let allowed = self.consent.allows(MediaMode::Talk)
            && self.current_call_active
            && self.current_call_id.as_deref() == Some(binding.call_id.as_str())
            && self.current_record().is_some_and(|record| {
                record.call_epoch == binding.call_epoch && record.owner_epoch == binding.owner_epoch
            })
            && route.permit.is_current_for(binding, now)
            && self
                .peers
                .get(&binding.rtc_session_id)
                .is_some_and(|slot| slot.binding == *binding && slot.lease_expires_at > now);
        if !allowed && route.permit.expires_at <= now {
            let binding = route.permit.binding.clone();
            self.active_route = None;
            self.return_binding = Some(binding);
            self.return_reason = Some("lease_expired".into());
            self.return_dispatched = false;
            self.service_mode = ServiceMode::ReturningToAokie;
            self.bump_revision();
        }
        allowed
    }

    fn allows_consult(&self, binding: &SessionBinding, now: Instant) -> bool {
        let Some(active) = self.active_consult.as_ref() else {
            return false;
        };
        self.current_call_active
            && self.service_mode == ServiceMode::ConsultActive
            && self.consent.allows(MediaMode::Consult)
            && active.expires_at > now
            && active.binding == *binding
            && self.current_call_id.as_deref() == Some(binding.call_id.as_str())
            && self.current_record().is_some_and(|record| {
                record.call_epoch == binding.call_epoch && record.owner_epoch == binding.owner_epoch
            })
            && self
                .peers
                .get(&binding.rtc_session_id)
                .is_some_and(|slot| slot.binding == *binding && slot.lease_expires_at > now)
    }

    fn note_consult_pcm_received(&mut self, binding: &SessionBinding, now: Instant) {
        if binding.mode != MediaMode::Consult {
            return;
        }
        if let Some(prepared) = self.prepared_claim.as_mut() {
            if prepared.active_media_binding.as_ref() == Some(binding) {
                prepared.last_media_received_at = Some(now);
            }
        }
        if let Some(pending) = self.pending_consult.as_mut() {
            if pending.binding == *binding {
                pending.last_received_at = Some(now);
            }
        }
        if let Some(active) = self.active_consult.as_mut() {
            if active.binding == *binding {
                active.last_received_at = Some(now);
            }
        }
    }

    fn radio_reserved(&self) -> bool {
        matches!(
            self.service_mode,
            ServiceMode::SoftHold
                | ServiceMode::ConsultPending
                | ServiceMode::ConsultActive
                | ServiceMode::HumanPending
                | ServiceMode::HumanActive
                | ServiceMode::ReturningToAokie
                | ServiceMode::Recovering
        )
    }
}

#[derive(Clone)]
struct EventEmitter {
    tx: std_mpsc::SyncSender<RemoteMediaEvent>,
    sequence: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl EventEmitter {
    fn emit(&self, binding: &SessionBinding, kind: RemoteMediaEventKind) {
        let event = RemoteMediaEvent {
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
            rtc_session_id: binding.rtc_session_id.clone(),
            call_id: binding.call_id.clone(),
            call_epoch: binding.call_epoch,
            owner_epoch: binding.owner_epoch,
            kind,
        };
        if self.tx.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn manager_loop(
    mut receiver: mpsc::Receiver<ManagerCommand>,
    state: Arc<Mutex<RemoteMediaState>>,
    talk_tx: std_mpsc::SyncSender<RoutedAudio>,
    consult_tx: std_mpsc::SyncSender<RoutedAudio>,
    events: EventEmitter,
    quarantined: Arc<AtomicU64>,
) {
    while let Some(command) = receiver.recv().await {
        match command {
            ManagerCommand::Open { request, reply } => {
                let binding = request.binding.clone();
                let expected_switch_epoch = request.expected_switch_epoch;
                let reserved = state
                    .lock()
                    .map_err(|_| "remote media state poisoned".to_string())
                    .and_then(|state| {
                        state.reserve_open_after_switch_epoch(&binding, expected_switch_epoch)
                    });
                if let Err(error) = reserved {
                    let _ = reply.send(Err(error));
                    continue;
                }
                let options = PeerOptions {
                    ice_servers: request.ice_servers,
                    relay_only: request.relay_only,
                    ..PeerOptions::default()
                };
                match DesktopPeer::answer(binding.clone(), request.offer, options).await {
                    Ok((peer, answer)) => {
                        let (actor_tx, actor_rx) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
                        let (remote_ice_tx, remote_ice_rx) =
                            mpsc::channel(REMOTE_ICE_QUEUE_CAPACITY);
                        let (caller_pcm_tx, caller_pcm_rx) =
                            mpsc::channel(CALLER_PCM_QUEUE_CAPACITY);
                        let slot = PeerSlot {
                            binding: binding.clone(),
                            lease_expires_at: Instant::now()
                                + Duration::from_millis(request.lease_ttl_ms),
                            actor_tx: actor_tx.clone(),
                            remote_ice_tx,
                            caller_pcm_tx,
                        };
                        let inserted = state
                            .lock()
                            .map_err(|_| "remote media state poisoned".to_string())
                            .and_then(|mut state| {
                                state.insert_peer_after_switch_epoch(slot, expected_switch_epoch)
                            });
                        if let Err(error) = inserted {
                            peer.close();
                            let _ = reply.send(Err(error));
                            continue;
                        }
                        events.emit(
                            &binding,
                            RemoteMediaEventKind::SdpAnswer {
                                answer: answer.clone(),
                            },
                        );
                        let accepted = OpenPeerAccepted {
                            rtc_session_id: binding.rtc_session_id.clone(),
                            call_id: binding.call_id.clone(),
                            call_epoch: binding.call_epoch,
                            owner_epoch: binding.owner_epoch,
                        };
                        let _ = reply.send(Ok(accepted));
                        tokio::spawn(peer_actor(
                            peer,
                            actor_rx,
                            remote_ice_rx,
                            caller_pcm_rx,
                            state.clone(),
                            talk_tx.clone(),
                            consult_tx.clone(),
                            events.clone(),
                            quarantined.clone(),
                        ));
                    }
                    Err(error) => {
                        events.emit(
                            &binding,
                            RemoteMediaEventKind::Error {
                                operation: "answer".into(),
                                message: error.to_string(),
                            },
                        );
                        let _ = reply.send(Err(error.to_string()));
                    }
                }
            }
            ManagerCommand::Shutdown => break,
        }
    }
}

fn handle_actor_command(
    peer: &mut DesktopPeer,
    command: ActorCommand,
    binding: &SessionBinding,
    events: &EventEmitter,
) -> bool {
    match command {
        ActorCommand::Authorize(permit) => {
            if let Err(error) = peer.authorize_caller_transmit(permit) {
                events.emit(
                    binding,
                    RemoteMediaEventKind::Error {
                        operation: "authorize".into(),
                        message: error.to_string(),
                    },
                );
            }
        }
        ActorCommand::Revoke => peer.revoke_caller_transmit(),
        ActorCommand::Close => {
            peer.close();
            return true;
        }
    }
    false
}

fn drain_actor_commands(
    peer: &mut DesktopPeer,
    commands: &mut mpsc::Receiver<ActorCommand>,
    binding: &SessionBinding,
    events: &EventEmitter,
) -> (bool, bool) {
    let mut worked = false;
    while let Ok(command) = commands.try_recv() {
        worked = true;
        if handle_actor_command(peer, command, binding, events) {
            return (worked, true);
        }
    }
    if commands.is_closed() {
        peer.close();
        return (worked, true);
    }
    (worked, false)
}

async fn peer_actor(
    mut peer: DesktopPeer,
    mut commands: mpsc::Receiver<ActorCommand>,
    mut remote_ice: mpsc::Receiver<IceCandidateSignal>,
    mut caller_pcm: mpsc::Receiver<Arc<Vec<i16>>>,
    state: Arc<Mutex<RemoteMediaState>>,
    talk_tx: std_mpsc::SyncSender<RoutedAudio>,
    consult_tx: std_mpsc::SyncSender<RoutedAudio>,
    events: EventEmitter,
    quarantined: Arc<AtomicU64>,
) {
    let binding = peer.binding().clone();
    let mut microphone_pcm_observed = false;
    loop {
        let (mut worked, should_close) =
            drain_actor_commands(&mut peer, &mut commands, &binding, &events);
        if should_close {
            return;
        }

        for _ in 0..REMOTE_ICE_BATCH_SIZE {
            let (control_worked, should_close) =
                drain_actor_commands(&mut peer, &mut commands, &binding, &events);
            worked |= control_worked;
            if should_close {
                return;
            }
            let Ok(candidate) = remote_ice.try_recv() else {
                break;
            };
            worked = true;
            if let Err(error) = peer.add_remote_candidate(candidate).await {
                events.emit(
                    &binding,
                    RemoteMediaEventKind::Error {
                        operation: "remote_ice".into(),
                        message: error.to_string(),
                    },
                );
            }
        }

        let (control_worked, should_close) =
            drain_actor_commands(&mut peer, &mut commands, &binding, &events);
        worked |= control_worked;
        if should_close {
            return;
        }
        if let Ok(samples) = caller_pcm.try_recv() {
            worked = true;
            if let Err(error) = peer.push_caller_pcm(samples.as_slice()).await {
                events.emit(
                    &binding,
                    RemoteMediaEventKind::Error {
                        operation: "caller_pcm".into(),
                        message: error.to_string(),
                    },
                );
            }
        }

        let (control_worked, should_close) =
            drain_actor_commands(&mut peer, &mut commands, &binding, &events);
        worked |= control_worked;
        if should_close {
            return;
        }

        let received = match binding.mode {
            MediaMode::Talk => {
                tokio::time::timeout(Duration::from_millis(2), peer.recv_caller_microphone())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|frame| (frame, &talk_tx))
            }
            MediaMode::Consult => {
                tokio::time::timeout(Duration::from_millis(2), peer.recv_consult_microphone())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|frame| (frame, &consult_tx))
            }
            MediaMode::Monitor | MediaMode::PreparedConsult | MediaMode::PreparedTalk => None,
        };
        if let Some((frame, destination)) = received {
            worked = true;
            if binding.mode == MediaMode::Consult && !frame.samples.is_empty() {
                if let Ok(mut state) = state.lock() {
                    state.note_consult_pcm_received(&binding, Instant::now());
                }
            }
            if !microphone_pcm_observed {
                microphone_pcm_observed = true;
                let (peak, level_permille) = pcm_level_summary(&frame.samples);
                eprintln!(
                    "[aokie-plugin][takeover] stage=microphone_pcm_received call={} owner_epoch={} rtc={} samples={} sample_rate={} level_permille={} peak={}",
                    binding.call_id,
                    binding.owner_epoch,
                    binding.rtc_session_id,
                    frame.samples.len(),
                    frame.sample_rate,
                    level_permille,
                    peak,
                );
            }
            if destination
                .try_send(RoutedAudio {
                    binding: binding.clone(),
                    frame,
                })
                .is_err()
            {
                quarantined.fetch_add(1, Ordering::Relaxed);
            }
        }

        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(1), peer.next_event()).await
        {
            worked = true;
            let kind = match event {
                PeerEvent::LocalIce(candidate) => RemoteMediaEventKind::LocalIce { candidate },
                PeerEvent::IceComplete => RemoteMediaEventKind::IceComplete,
                PeerEvent::ConnectionState(state) => RemoteMediaEventKind::ConnectionState {
                    state: state.to_string(),
                },
                PeerEvent::RemoteAudioReady => RemoteMediaEventKind::RemoteAudioReady,
                PeerEvent::MicrophoneAuthorityReady => RemoteMediaEventKind::ProtocolViolation {
                    message: "Desktop received a Companion-only microphone authority event"
                        .to_string(),
                },
                PeerEvent::RemoteMicrophoneReady => RemoteMediaEventKind::RemoteMicrophoneReady,
                PeerEvent::ProtocolViolation(message) => RemoteMediaEventKind::ProtocolViolation {
                    message: message.to_string(),
                },
            };
            events.emit(&binding, kind);
        }
        if !worked {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

fn validate_ttl(ttl_ms: u64) -> Result<Duration, String> {
    let ttl = Duration::from_millis(ttl_ms);
    if ttl.is_zero() || ttl > MAX_LEASE_TTL {
        Err("leaseTtlMs must be between 1 and 300000".into())
    } else {
        Ok(ttl)
    }
}

fn sanitize_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        "requested".into()
    } else {
        trimmed.chars().take(160).collect()
    }
}

fn companion_participant_id(binding: &SessionBinding) -> String {
    // rtcSessionId is already a validated opaque identifier and changes with
    // each native endpoint. Reusing it avoids leaking the owner/device subject
    // while preserving an exact key for participant-bound audio levels.
    binding.rtc_session_id.clone()
}

fn observed_level_for_call(
    observed: &ObservedAudioLevel,
    call_id: &str,
    call_epoch: u64,
    now: Instant,
) -> Option<u16> {
    if observed.call_id != call_id || observed.call_epoch != call_epoch {
        return None;
    }
    let age = now.saturating_duration_since(observed.observed_at);
    if age > AUDIO_LEVEL_STALE {
        return None;
    }
    if age <= AUDIO_LEVEL_HOLD {
        return Some(observed.level_permille);
    }
    let fade = AUDIO_LEVEL_STALE.saturating_sub(AUDIO_LEVEL_HOLD);
    let remaining = AUDIO_LEVEL_STALE.saturating_sub(age);
    let scaled = u128::from(observed.level_permille).saturating_mul(remaining.as_millis())
        / fade.as_millis().max(1);
    Some(u16::try_from(scaled).unwrap_or(0))
}

fn pcm_level_summary(samples: &[i16]) -> (u16, u16) {
    if samples.is_empty() {
        return (0, 0);
    }
    let peak = samples
        .iter()
        .map(|sample| sample.unsigned_abs())
        .max()
        .unwrap_or(0);
    let mean_square = samples
        .iter()
        .map(|sample| {
            let sample = f64::from(*sample);
            sample * sample
        })
        .sum::<f64>()
        / samples.len() as f64;
    let rms = mean_square.sqrt();
    let level_permille = ((rms / f64::from(i16::MAX)) * 1_000.0)
        .round()
        .clamp(0.0, 1_000.0) as u16;
    (peak, level_permille)
}

/// Small deterministic mono resampler for the HFP rates used by the radio
/// (normally 8 kHz CVSD or 16 kHz mSBC).  It also handles arbitrary positive
/// rates for synthetic tests without retaining unbounded history.
pub fn resample_mono(samples: &[i16], from_hz: u32, to_hz: u32) -> Vec<i16> {
    if samples.is_empty() || from_hz == 0 || to_hz == 0 {
        return Vec::new();
    }
    if from_hz == to_hz {
        return samples.to_vec();
    }
    let output_len = ((samples.len() as u64 * to_hz as u64) / from_hz as u64).max(1) as usize;
    let mut output = Vec::with_capacity(output_len);
    for index in 0..output_len {
        let position = index as f64 * from_hz as f64 / to_hz as f64;
        let left = position.floor() as usize;
        let right = (left + 1).min(samples.len() - 1);
        let fraction = position - left as f64;
        let sample = samples[left.min(samples.len() - 1)] as f64 * (1.0 - fraction)
            + samples[right] as f64 * fraction;
        output.push(sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_deadline_conversion_caps_backward_wall_clock_steps() {
        let now = Instant::now();
        let deadline = bounded_transfer_deadline(u64::MAX, 1, now).unwrap();
        assert!(deadline >= now);
        assert!(
            deadline <= now + Duration::from_secs(crate::assistance::TRANSFER_SETUP_SECONDS),
            "a backward wall-clock step cannot extend the monotonic setup guard"
        );
        assert!(bounded_transfer_deadline(10, 10, now).is_err());
        assert!(bounded_transfer_deadline(9, 10, now).is_err());
    }

    #[test]
    fn actual_aokie_playout_pcm_drives_and_clears_its_meter() {
        let mut state = RemoteMediaState::default();
        state.observe_call(Some("call_a"), true);
        let now = Instant::now();
        state.observe_aokie_audio(&vec![12_000; 160], now);
        let levels = state.current_audio_levels(now);
        assert!(levels.iter().any(|level| {
            level.source == RemoteAudioLevelSource::Aokie
                && level.participant_id.is_none()
                && level.level_permille > 0
        }));
        assert!(state
            .current_audio_levels(now + AUDIO_LEVEL_STALE + Duration::from_millis(1))
            .iter()
            .all(|level| level.source != RemoteAudioLevelSource::Aokie));
        state.observe_aokie_audio(&vec![12_000; 160], now);
        state.observe_call(None, false);
        assert!(state.current_audio_levels(now).is_empty());
    }

    #[test]
    fn companion_caller_output_mirror_never_drives_the_aokie_meter() {
        let handle = RemoteMediaHandle::spawn().unwrap();
        handle.observe_physical_call(Some("call_a"), true);
        let pcm = vec![12_000; 160];

        handle.try_mirror_companion_output(&pcm, MEDIA_SAMPLE_RATE_HZ);
        assert!(handle
            .snapshot()
            .audio_levels
            .iter()
            .all(|level| level.source != RemoteAudioLevelSource::Aokie));

        handle.try_push_caller_output(&pcm, MEDIA_SAMPLE_RATE_HZ);
        assert!(handle.snapshot().audio_levels.iter().any(|level| {
            level.source == RemoteAudioLevelSource::Aokie && level.level_permille > 0
        }));
    }

    fn binding(mode: MediaMode, owner_epoch: u64, fence: u64, suffix: &str) -> SessionBinding {
        SessionBinding {
            rtc_session_id: format!("rtc_{suffix}"),
            call_id: "call_a".into(),
            call_epoch: 1,
            owner_epoch,
            device_id: format!("device_{suffix}"),
            mode,
            lease_id: Some(format!("lease_{suffix}")),
            fence,
        }
    }

    fn actor() -> mpsc::Sender<ActorCommand> {
        let (tx, _rx) = mpsc::channel(2);
        tx
    }

    fn remote_ice() -> mpsc::Sender<IceCandidateSignal> {
        let (tx, _rx) = mpsc::channel(2);
        tx
    }

    fn caller_pcm() -> mpsc::Sender<Arc<Vec<i16>>> {
        let (tx, _rx) = mpsc::channel(2);
        tx
    }

    fn insert(state: &mut RemoteMediaState, binding: SessionBinding, ttl: Duration) {
        state
            .insert_peer(PeerSlot {
                binding,
                lease_expires_at: Instant::now() + ttl,
                actor_tx: actor(),
                remote_ice_tx: remote_ice(),
                caller_pcm_tx: caller_pcm(),
            })
            .unwrap();
    }

    fn active_state() -> RemoteMediaState {
        let mut state = RemoteMediaState::default();
        state.consent = RemoteConsentGate {
            policy_id: REMOTE_CONSENT_POLICY_ID.into(),
            policy_version: crate::consent::CURRENT_CONSENT_VERSION,
            enabled: true,
            acknowledged: true,
            acknowledged_at: Some("2026-07-16T00:00:00Z".into()),
            expires_at: Some("2999-01-01T00:00:00Z".into()),
            captions_enabled: true,
            assistance_enabled: true,
            monitor_enabled: true,
            consult_enabled: true,
            takeover_enabled: true,
        };
        state.observe_call(Some("call_a"), true);
        state
    }

    #[test]
    fn autonomous_aokie_actions_require_the_exact_unchanged_owner_fence() {
        let handle = RemoteMediaHandle::spawn().unwrap();
        {
            let mut state = handle.inner.state.lock().unwrap();
            *state = active_state();
        }
        handle.refresh_reserved();

        let original = handle
            .aokie_owner_fence()
            .expect("active Aokie call has an owner fence");
        let mut ran = false;
        assert_eq!(
            handle.with_aokie_owner(&original, || {
                ran = true;
                7
            }),
            Ok(7)
        );
        assert!(ran);

        // Monitor peers, signalling, and heartbeats use remote_revision but do
        // not transfer the caller; they must not cut a healthy Aokie reply.
        {
            let mut state = handle.inner.state.lock().unwrap();
            state.bump_revision();
        }
        assert!(handle.with_aokie_owner(&original, || ()).is_ok());

        handle
            .linearize_aokie_action(&original)
            .expect("the first exact action wins its linearization point");
        let linearized = handle
            .aokie_owner_fence()
            .expect("linearization keeps Aokie as the current owner");
        assert_eq!(linearized.call_id, original.call_id);
        assert_eq!(linearized.call_epoch, original.call_epoch);
        assert_eq!(linearized.action_epoch, original.action_epoch + 1);
        assert!(handle.linearize_aokie_action(&original).is_err());

        // A later legitimate preparation starts from the fresh owner state;
        // the local action epoch is not a remote-media kill switch.
        {
            let mut state = handle.inner.state.lock().unwrap();
            let prepared = binding(MediaMode::PreparedTalk, 0, 11, "after_action");
            insert(&mut state, prepared.clone(), Duration::from_secs(5));
            state
                .request_soft_hold(prepared, Duration::from_secs(5))
                .expect("a later exact claim can still proceed");
            state.pending_prepare = None;
            state.peers.clear();
        }
        // Any pending/active remote service state fails closed.
        for mode in [
            ServiceMode::SoftHold,
            ServiceMode::ConsultPending,
            ServiceMode::ConsultActive,
            ServiceMode::HumanPending,
            ServiceMode::HumanActive,
            ServiceMode::ReturningToAokie,
            ServiceMode::Recovering,
        ] {
            {
                let mut state = handle.inner.state.lock().unwrap();
                state.service_mode = mode;
            }
            handle.refresh_reserved();
            assert!(handle.aokie_owner_fence().is_none(), "{mode:?}");
            let mut stale_ran = false;
            assert!(
                handle
                    .with_aokie_owner(&original, || stale_ran = true)
                    .is_err(),
                "{mode:?}"
            );
            assert!(!stale_ran, "{mode:?}");
        }

        // Returning to Aokie does not revive a pre-claim autonomous action:
        // the dedicated action epoch changed.
        {
            let mut state = handle.inner.state.lock().unwrap();
            state.service_mode = ServiceMode::AokieActive;
        }
        handle.refresh_reserved();
        assert!(handle.aokie_owner_fence().is_some());
        let mut stale_ran = false;
        assert!(handle
            .with_aokie_owner(&original, || stale_ran = true)
            .is_err());
        assert!(!stale_ran);
    }

    #[test]
    fn physical_switch_and_non_monitor_peer_registration_are_mutually_exclusive() {
        let handle = RemoteMediaHandle::spawn().unwrap();
        {
            let mut state = handle.inner.state.lock().unwrap();
            *state = active_state();
        }

        let first = handle
            .aokie_switch_fence()
            .expect("an idle Aokie-owned call can switch");
        assert_eq!(
            handle.with_aokie_switch_owner(&first, || 7),
            Ok(7),
            "the switch command wins atomically"
        );
        assert!(handle.with_aokie_switch_owner(&first, || ()).is_err());
        let after_switch = handle
            .aokie_switch_fence()
            .expect("Aokie still owns the call after the command send");
        assert_eq!(after_switch.switch_epoch, first.switch_epoch + 1);

        // A monitor peer remains observational and does not block CHLD.
        {
            let mut state = handle.inner.state.lock().unwrap();
            let monitor = binding(MediaMode::Monitor, 0, 0, "monitor_switch");
            insert(&mut state, monitor, Duration::from_secs(5));
        }
        assert!(handle.aokie_switch_fence().is_some());

        // The gateway captured this proof while admitting the offer, but CHLD
        // won before the async media manager dequeued Open. The manager must
        // reject the queued non-monitor request rather than capture/bless the
        // already-advanced epoch at dequeue time.
        let admitted_epoch = handle.capture_aokie_switch_epoch().unwrap();
        let queued_binding = binding(MediaMode::PreparedTalk, 0, 12, "manager_dequeue");
        let chld = handle
            .aokie_switch_fence()
            .expect("monitor-only state still permits CHLD");
        handle
            .with_aokie_switch_owner(&chld, || ())
            .expect("CHLD wins after offer admission");
        {
            let state = handle.inner.state.lock().unwrap();
            assert!(state
                .reserve_open_after_switch_epoch(&queued_binding, Some(admitted_epoch))
                .is_err());
        }

        // If asynchronous SDP reserved a non-monitor open before CHLD, the
        // switch epoch must still match when its slot is finally inserted.
        let captured_epoch = {
            let state = handle.inner.state.lock().unwrap();
            state.aokie_switch_epoch
        };
        let prepared_binding = binding(MediaMode::PreparedTalk, 0, 13, "peer_race");
        let prepared_slot = PeerSlot {
            binding: prepared_binding,
            lease_expires_at: Instant::now() + Duration::from_secs(5),
            actor_tx: actor(),
            remote_ice_tx: remote_ice(),
            caller_pcm_tx: caller_pcm(),
        };
        {
            let mut state = handle.inner.state.lock().unwrap();
            state.aokie_switch_epoch = state.aokie_switch_epoch.saturating_add(1);
            assert!(state
                .insert_peer_after_switch_epoch(prepared_slot, Some(captured_epoch))
                .is_err());
        }

        // Conversely, if peer insertion wins first, the switch fence vanishes
        // until that prepared/active route is gone.
        {
            let mut state = handle.inner.state.lock().unwrap();
            state
                .peers
                .retain(|_, slot| slot.binding.mode != MediaMode::Monitor);
            let prepared = binding(MediaMode::PreparedTalk, 0, 14, "peer_first");
            insert(&mut state, prepared, Duration::from_secs(5));
        }
        assert!(handle.aokie_switch_fence().is_none());
    }

    fn prepare_active_talk(
        state: &mut RemoteMediaState,
        suffix: &str,
        fence: u64,
        ttl: Duration,
    ) -> SessionBinding {
        let prepared = binding(MediaMode::PreparedTalk, 0, fence, suffix);
        insert(state, prepared.clone(), ttl);
        state.request_soft_hold(prepared.clone(), ttl).unwrap();
        assert!(matches!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::PrepareHuman { .. })
        ));
        assert_eq!(state.ack_prepare(&prepared, Instant::now()).unwrap(), 1);
        assert_eq!(
            state.service_mode,
            ServiceMode::AokieActive,
            "prepared takeover negotiation never mutes the receptionist"
        );
        assert!(!state.radio_reserved());
        state
            .close_peer(&prepared.rtc_session_id, "active_rebind")
            .unwrap();
        let talk = binding(MediaMode::Talk, 1, fence, suffix);
        insert(state, talk.clone(), ttl);
        talk
    }

    fn prepare_active_consult(
        state: &mut RemoteMediaState,
        suffix: &str,
        ttl: Duration,
    ) -> SessionBinding {
        let prepared = binding(MediaMode::PreparedConsult, 0, 0, suffix);
        insert(state, prepared.clone(), ttl);
        state.request_soft_hold(prepared.clone(), ttl).unwrap();
        assert!(matches!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::PrepareConsult { .. })
        ));
        assert_eq!(state.ack_prepare(&prepared, Instant::now()).unwrap(), 1);
        assert_eq!(state.service_mode, ServiceMode::AokieActive);
        assert!(!state.radio_reserved());
        state
            .close_peer(&prepared.rtc_session_id, "active_rebind")
            .unwrap();
        let consult = binding(MediaMode::Consult, 1, 0, suffix);
        insert(state, consult.clone(), ttl);
        state.note_consult_pcm_received(&consult, Instant::now());
        state.request_consult(consult.clone(), ttl).unwrap();
        assert_eq!(state.service_mode, ServiceMode::ConsultPending);
        assert!(matches!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::EnterConsult { .. })
        ));
        state.ack_enter_consult(&consult, Instant::now()).unwrap();
        consult
    }

    #[test]
    fn physical_call_epoch_is_stable_but_owner_epoch_is_independent() {
        let mut state = active_state();
        assert_eq!(state.current_record().unwrap().call_epoch, 1);
        assert_eq!(state.current_record().unwrap().owner_epoch, 0);
        state.observe_call(Some("call_b"), true);
        assert_eq!(state.current_record().unwrap().call_epoch, 2);
        state.observe_call(Some("call_a"), true);
        assert_eq!(state.current_record().unwrap().call_epoch, 1);
        assert_eq!(state.current_record().unwrap().owner_epoch, 0);
    }

    #[test]
    fn offer_binding_must_match_exact_physical_epochs() {
        let state = active_state();
        let monitor = binding(MediaMode::Monitor, 0, 0, "monitor");
        assert!(state.validate_open(&monitor).is_ok());
        let mut stale = monitor.clone();
        stale.call_epoch = 2;
        assert!(state.validate_open(&stale).is_err());
        stale = monitor;
        stale.owner_epoch = 1;
        assert!(state.validate_open(&stale).is_err());
    }

    #[test]
    fn one_talk_claimant_and_max_six_peers_are_enforced() {
        let mut state = active_state();
        insert(
            &mut state,
            binding(MediaMode::PreparedTalk, 0, 7, "talk"),
            Duration::from_secs(30),
        );
        assert!(state
            .validate_open(&binding(MediaMode::PreparedTalk, 0, 8, "talk2"))
            .is_err());
        for index in 0..5 {
            insert(
                &mut state,
                binding(MediaMode::Monitor, 0, 0, &format!("m{index}")),
                Duration::from_secs(30),
            );
        }
        assert_eq!(state.peers.len(), MAX_REMOTE_PEERS);
        assert!(state
            .validate_open(&binding(MediaMode::Monitor, 0, 0, "overflow"))
            .is_err());
    }

    #[test]
    fn human_active_requires_physical_ack_and_return_advances_owner_epoch() {
        let mut state = active_state();
        let talk = prepare_active_talk(&mut state, "talk", 9, Duration::from_secs(30));
        state
            .request_takeover(talk.clone(), Duration::from_secs(10))
            .unwrap();
        assert_eq!(state.service_mode, ServiceMode::HumanPending);
        assert!(state.radio_reserved());
        assert!(state.active_route.is_none());
        assert_eq!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::EnterHuman {
                binding: talk.clone()
            })
        );
        state.ack_enter(&talk, Instant::now(), true).unwrap();
        assert_eq!(state.service_mode, ServiceMode::HumanActive);
        assert_eq!(state.current_record().unwrap().owner_epoch, 1);
        state.request_revoke(&talk, "operator_return").unwrap();
        assert!(!state.allows_talk(&talk, Instant::now()));
        assert!(matches!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::ReturnToAokie { .. })
        ));
        state.ack_return().unwrap();
        assert_eq!(state.current_record().unwrap().owner_epoch, 2);
        assert_eq!(state.service_mode, ServiceMode::AokieActive);
        assert!(!state.radio_reserved());
    }

    #[test]
    fn accepted_transfer_cannot_cross_its_deadline_at_the_physical_ack() {
        let mut state = active_state();
        let talk =
            prepare_active_talk(&mut state, "transfer_deadline", 19, Duration::from_secs(30));
        let requested_at = Instant::now();
        let deadline = requested_at + Duration::from_millis(5);
        state
            .request_takeover_guarded(
                talk.clone(),
                Duration::from_secs(20),
                Some(TransferActivationGuard {
                    request_id: "assist_transfer_deadline".into(),
                    offered_fence: crate::assistance::AssistanceCallFence {
                        call_id: talk.call_id.clone(),
                        call_epoch: talk.call_epoch,
                        owner_epoch: 0,
                        switchboard_revision: 4,
                        remote_revision: 5,
                    },
                    device_id: talk.device_id.clone(),
                    deadline,
                }),
            )
            .unwrap();
        assert_eq!(state.service_mode, ServiceMode::HumanPending);

        assert!(state.ack_enter(&talk, deadline, true).is_err());
        assert_ne!(state.service_mode, ServiceMode::HumanActive);
        assert!(state.active_route.is_none());
        assert!(matches!(
            state.next_transition(deadline).0,
            Some(RadioTransition::ReturnToAokie { .. })
        ));

        let mut authority_changed = active_state();
        let talk = prepare_active_talk(
            &mut authority_changed,
            "transfer_authority",
            20,
            Duration::from_secs(30),
        );
        authority_changed
            .request_takeover_guarded(
                talk.clone(),
                Duration::from_secs(20),
                Some(TransferActivationGuard {
                    request_id: "assist_transfer_authority".into(),
                    offered_fence: crate::assistance::AssistanceCallFence {
                        call_id: talk.call_id.clone(),
                        call_epoch: talk.call_epoch,
                        owner_epoch: 0,
                        switchboard_revision: 4,
                        remote_revision: 5,
                    },
                    device_id: talk.device_id.clone(),
                    deadline: Instant::now() + Duration::from_secs(5),
                }),
            )
            .unwrap();
        assert!(authority_changed
            .ack_enter(&talk, Instant::now(), false)
            .is_err());
        assert_ne!(authority_changed.service_mode, ServiceMode::HumanActive);
    }

    #[test]
    fn active_talk_preflight_deadline_is_not_extended_by_heartbeats() {
        let mut state = active_state();
        let talk = prepare_active_talk(&mut state, "preflight", 9, Duration::from_secs(30));
        let deadline = state
            .prepared_claim
            .as_ref()
            .and_then(|prepared| prepared.media_ready_deadline)
            .expect("opening the active Talk peer starts a readiness deadline");

        state
            .renew(&talk, Duration::from_secs(30))
            .expect("the lease itself can renew");
        assert_eq!(
            state
                .prepared_claim
                .as_ref()
                .and_then(|prepared| prepared.media_ready_deadline),
            Some(deadline),
            "heartbeats cannot prolong a peer that never proves microphone PCM"
        );
        let (transition, _, notice, cancelled) =
            state.next_transition(deadline + Duration::from_millis(1));
        assert!(transition.is_none());
        assert!(notice.is_none());
        assert!(cancelled.is_some_and(|(slot, reason)| {
            slot.binding == talk && reason == "talk_readiness_timeout"
        }));
        assert_eq!(state.service_mode, ServiceMode::AokieActive);
        assert_eq!(state.current_record().unwrap().owner_epoch, 2);
        assert!(!state.radio_reserved());
    }

    #[test]
    fn every_pre_route_talk_abort_cancels_in_place_and_fences_the_owner_epoch() {
        let mut pending = active_state();
        let prepared = binding(MediaMode::PreparedTalk, 0, 9, "pending-expiry");
        insert(&mut pending, prepared.clone(), Duration::from_secs(30));
        pending
            .request_soft_hold(prepared.clone(), Duration::from_secs(5))
            .unwrap();
        let deadline = pending.pending_prepare.as_ref().unwrap().expires_at;
        let (transition, _, notice, cancelled) =
            pending.next_transition(deadline + Duration::from_millis(1));
        assert!(transition.is_none());
        assert!(notice.is_none());
        assert!(cancelled.is_some_and(|(slot, reason)| {
            slot.binding == prepared && reason == "prepared_lease_expired"
        }));
        assert_eq!(pending.service_mode, ServiceMode::AokieActive);
        assert_eq!(pending.current_record().unwrap().owner_epoch, 1);
        assert!(!pending.radio_reserved());

        let mut revoked = active_state();
        let talk = prepare_active_talk(&mut revoked, "explicit-abort", 10, Duration::from_secs(30));
        let (_, event_binding, needs_return, close_peer) =
            revoked.request_revoke(&talk, "permission_denied").unwrap();
        assert_eq!(event_binding, talk);
        assert!(!needs_return);
        assert!(close_peer);
        assert_eq!(revoked.service_mode, ServiceMode::AokieActive);
        assert_eq!(revoked.current_record().unwrap().owner_epoch, 2);
        assert!(!revoked.radio_reserved());

        let mut disconnected = active_state();
        let talk = prepare_active_talk(
            &mut disconnected,
            "transport-abort",
            11,
            Duration::from_secs(30),
        );
        let (closed, returning) = disconnected.fail_closed_all("relay_disconnected");
        assert!(returning.is_none());
        assert!(closed.iter().any(|slot| slot.binding == talk));
        assert_eq!(disconnected.service_mode, ServiceMode::AokieActive);
        assert_eq!(disconnected.current_record().unwrap().owner_epoch, 2);
        assert!(!disconnected.radio_reserved());
    }

    #[test]
    fn consent_revocation_cancels_talk_preflight_in_place_but_returns_an_active_route() {
        let pending = RemoteMediaHandle::spawn().unwrap();
        let gate = {
            let mut state = pending.inner.state.lock().unwrap();
            *state = active_state();
            let prepared = binding(MediaMode::PreparedTalk, 0, 11, "consent-pending");
            insert(&mut state, prepared.clone(), Duration::from_secs(30));
            state
                .request_soft_hold(prepared, Duration::from_secs(20))
                .unwrap();
            let mut gate = state.consent.clone();
            gate.takeover_enabled = false;
            gate
        };
        pending.refresh_reserved();
        pending.set_remote_consent(gate);
        let snapshot = pending.snapshot();
        assert_eq!(snapshot.service_mode, ServiceMode::AokieActive);
        assert!(!snapshot.radio_reserved);
        assert_eq!(snapshot.owner_epoch, 1, "the pending claimant is fenced");
        assert!(pending.next_radio_transition().is_none());

        let preflight = RemoteMediaHandle::spawn().unwrap();
        let mut gate = {
            let mut state = preflight.inner.state.lock().unwrap();
            *state = active_state();
            prepare_active_talk(&mut state, "consent-preflight", 12, Duration::from_secs(30));
            let mut gate = state.consent.clone();
            gate.takeover_enabled = false;
            gate
        };
        preflight.refresh_reserved();
        assert_eq!(preflight.snapshot().service_mode, ServiceMode::AokieActive);
        assert!(!preflight.snapshot().radio_reserved);

        preflight.set_remote_consent(gate.clone());
        let snapshot = preflight.snapshot();
        assert_eq!(snapshot.service_mode, ServiceMode::AokieActive);
        assert!(!snapshot.radio_reserved);
        assert_eq!(snapshot.owner_epoch, 2, "the cancelled claimant is fenced");
        assert!(preflight.next_radio_transition().is_none());

        let active = RemoteMediaHandle::spawn().unwrap();
        {
            let mut state = active.inner.state.lock().unwrap();
            *state = active_state();
            let talk =
                prepare_active_talk(&mut state, "consent-active", 13, Duration::from_secs(30));
            state
                .request_takeover(talk.clone(), Duration::from_secs(20))
                .unwrap();
            assert!(matches!(
                state.next_transition(Instant::now()).0,
                Some(RadioTransition::EnterHuman { .. })
            ));
            state.ack_enter(&talk, Instant::now(), true).unwrap();
        }
        active.refresh_reserved();
        assert_eq!(active.snapshot().service_mode, ServiceMode::HumanActive);

        // Once the route actually owned the caller, the same policy change
        // must retain the physical ReturnToAokie handshake.
        gate.takeover_enabled = false;
        active.set_remote_consent(gate);
        assert_eq!(
            active.snapshot().service_mode,
            ServiceMode::ReturningToAokie
        );
        assert!(active.snapshot().radio_reserved);
        assert!(matches!(
            active.next_radio_transition(),
            Some(RadioTransition::ReturnToAokie { .. })
        ));
    }

    #[test]
    fn active_talk_route_fails_back_on_missing_or_stalled_forwarded_pcm() {
        let start = Instant::now();
        let mut no_first = active_state();
        let talk = prepare_active_talk(&mut no_first, "no-first", 9, Duration::from_secs(30));
        no_first
            .request_takeover(talk.clone(), Duration::from_secs(20))
            .unwrap();
        no_first.next_transition(start);
        no_first.ack_enter(&talk, start, true).unwrap();
        let (transition, _, notice, _) = no_first.next_transition(start + FIRST_TALK_PCM_TIMEOUT);
        assert!(matches!(
            transition,
            Some(RadioTransition::ReturnToAokie { .. })
        ));
        assert!(notice.is_some_and(|(_, reason)| reason == "talk_audio_never_arrived"));

        let start = Instant::now();
        let mut stalled = active_state();
        let talk = prepare_active_talk(&mut stalled, "stalled", 11, Duration::from_secs(30));
        stalled
            .request_takeover(talk.clone(), Duration::from_secs(20))
            .unwrap();
        stalled.next_transition(start);
        stalled.ack_enter(&talk, start, true).unwrap();
        let last = start + Duration::from_millis(100);
        stalled
            .active_route
            .as_mut()
            .expect("active route")
            .last_forwarded_at = Some(last);
        stalled
            .renew(&talk, Duration::from_secs(30))
            .expect("heartbeat renews authority");
        assert_eq!(
            stalled
                .active_route
                .as_ref()
                .and_then(|route| route.last_forwarded_at),
            Some(last),
            "renewal cannot reset liveness evidence"
        );
        let (transition, _, notice, _) = stalled.next_transition(last + ONGOING_TALK_PCM_TIMEOUT);
        assert!(matches!(
            transition,
            Some(RadioTransition::ReturnToAokie { .. })
        ));
        assert!(notice.is_some_and(|(_, reason)| reason == "talk_audio_stalled"));
    }

    #[test]
    fn human_talk_audio_proof_is_binding_exact_and_survives_renewal() {
        let handle = RemoteMediaHandle::spawn().unwrap();
        let talk = {
            let mut state = handle.inner.state.lock().unwrap();
            *state = active_state();
            let talk = prepare_active_talk(&mut state, "talk-proof", 9, Duration::from_secs(30));
            state
                .request_takeover(talk.clone(), Duration::from_secs(20))
                .unwrap();
            assert!(matches!(
                state.next_transition(Instant::now()).0,
                Some(RadioTransition::EnterHuman { .. })
            ));
            state.ack_enter(&talk, Instant::now(), true).unwrap();
            talk
        };
        handle.refresh_reserved();
        assert_eq!(handle.snapshot().service_mode, ServiceMode::HumanActive);
        assert!(!handle.snapshot().talk_audio_forwarded);

        let mut stale = talk.clone();
        stale.fence += 1;
        assert!(!handle.mark_talk_audio_forwarded(&stale, &[1, 2, 3]));
        assert!(!handle.snapshot().talk_audio_forwarded);

        assert!(handle.mark_talk_audio_forwarded(&talk, &[1, 2, 3]));
        assert!(handle.snapshot().talk_audio_forwarded);
        assert!(!handle.mark_talk_audio_forwarded(&talk, &[4, 5, 6]));

        handle
            .inner
            .state
            .lock()
            .unwrap()
            .renew(&talk, Duration::from_secs(20))
            .expect("renew exact active route");
        assert!(
            handle.snapshot().talk_audio_forwarded,
            "lease expiry extension must not erase an already-proven media path"
        );

        handle.revoke(&talk, "operator_return").unwrap();
        assert!(!handle.snapshot().talk_audio_forwarded);
    }

    #[test]
    fn caller_end_action_is_atomic_with_return_to_aokie() {
        let handle = RemoteMediaHandle::spawn().unwrap();
        let talk = {
            let mut state = handle.inner.state.lock().unwrap();
            *state = active_state();
            let talk = prepare_active_talk(&mut state, "talk", 9, Duration::from_secs(30));
            state
                .request_takeover(talk.clone(), Duration::from_secs(20))
                .unwrap();
            assert!(matches!(
                state.next_transition(Instant::now()).0,
                Some(RadioTransition::EnterHuman { .. })
            ));
            state.ack_enter(&talk, Instant::now(), true).unwrap();
            talk
        };
        handle.refresh_reserved();
        let remote_revision = handle.snapshot().remote_revision;
        let mut actions = 0_u8;
        handle
            .with_active_talk_owner(
                &talk.call_id,
                talk.call_epoch,
                talk.owner_epoch,
                remote_revision,
                &talk.device_id,
                talk.lease_id.as_deref().unwrap(),
                talk.fence,
                || actions += 1,
            )
            .unwrap();
        assert_eq!(actions, 1);

        handle
            .inner
            .state
            .lock()
            .unwrap()
            .request_revoke(&talk, "operator_return")
            .unwrap();
        assert!(handle
            .with_active_talk_owner(
                &talk.call_id,
                talk.call_epoch,
                talk.owner_epoch,
                remote_revision,
                &talk.device_id,
                talk.lease_id.as_deref().unwrap(),
                talk.fence,
                || actions += 1,
            )
            .is_err());
        assert_eq!(
            actions, 1,
            "return won, so caller hangup action did not run"
        );
    }

    #[test]
    fn expired_permit_fails_closed_before_return_ack() {
        let mut state = active_state();
        let talk = prepare_active_talk(&mut state, "talk", 3, Duration::from_secs(2));
        state
            .request_takeover(talk.clone(), Duration::from_millis(2))
            .unwrap();
        state.next_transition(Instant::now());
        state.ack_enter(&talk, Instant::now(), true).unwrap();
        std::thread::sleep(Duration::from_millis(4));
        assert!(!state.allows_talk(&talk, Instant::now()));
        assert_eq!(state.service_mode, ServiceMode::ReturningToAokie);
        assert!(
            state.radio_reserved(),
            "Aokie stays muted until physical flush ACK"
        );
    }

    #[test]
    fn close_or_call_change_revokes_the_route_and_all_peers() {
        let mut state = active_state();
        let talk = prepare_active_talk(&mut state, "talk", 2, Duration::from_secs(30));
        state
            .request_takeover(talk.clone(), Duration::from_secs(20))
            .unwrap();
        let (_slot, needs_return) = state
            .close_peer(&talk.rtc_session_id, "disconnect")
            .unwrap();
        assert!(needs_return);
        assert!(state.active_route.is_none());
        assert!(state.pending_route.is_none());

        let mut state = active_state();
        insert(
            &mut state,
            binding(MediaMode::Monitor, 0, 0, "monitor"),
            Duration::from_secs(30),
        );
        let (actors, _) = state.observe_call(Some("call_b"), true);
        assert_eq!(actors.len(), 1);
        assert!(state.peers.is_empty());
    }

    #[test]
    fn resampler_handles_wideband_and_narrowband_sco() {
        let narrow = vec![0, 1000, 2000, 3000];
        let wide = resample_mono(&narrow, 8_000, 16_000);
        assert_eq!(wide.len(), 8);
        assert_eq!(wide[0], 0);
        assert_eq!(wide[2], 1000);
        let back = resample_mono(&wide, 16_000, 8_000);
        assert_eq!(back.len(), 4);
        assert_eq!(back, narrow);
    }

    #[test]
    fn saturated_audio_and_ice_lanes_do_not_starve_lifecycle_control() {
        let handle = RemoteMediaHandle::spawn().unwrap();
        let talk = binding(MediaMode::Talk, 0, 7, "lane-isolation");
        let (actor_tx, mut actor_rx) = mpsc::channel(ACTOR_QUEUE_CAPACITY);
        let (remote_ice_tx, mut remote_ice_rx) = mpsc::channel(REMOTE_ICE_QUEUE_CAPACITY);
        let (caller_pcm_tx, mut caller_pcm_rx) = mpsc::channel(CALLER_PCM_QUEUE_CAPACITY);
        {
            let mut state = handle.inner.state.lock().unwrap();
            *state = active_state();
            state.peers.insert(
                talk.rtc_session_id.clone(),
                PeerSlot {
                    binding: talk.clone(),
                    lease_expires_at: Instant::now() + Duration::from_secs(30),
                    actor_tx: actor_tx.clone(),
                    remote_ice_tx,
                    caller_pcm_tx,
                },
            );
        }

        for _ in 0..CALLER_PCM_QUEUE_CAPACITY {
            handle.inner.try_push_sco(&[1; 320], MEDIA_SAMPLE_RATE_HZ);
        }
        assert_eq!(caller_pcm_rx.len(), CALLER_PCM_QUEUE_CAPACITY);
        handle.inner.try_push_sco(&[1; 320], MEDIA_SAMPLE_RATE_HZ);
        assert_eq!(handle.inner.dropped_sco_frames.load(Ordering::Relaxed), 1);

        for index in 0..REMOTE_ICE_QUEUE_CAPACITY {
            handle
                .add_remote_ice(
                    &talk.rtc_session_id,
                    IceCandidateSignal {
                        sdp_mid: "audio".into(),
                        sdp_mline_index: 0,
                        candidate: format!(
                            "candidate:{index} 1 udp 2122260223 192.0.2.1 {} typ host",
                            10_000 + index
                        ),
                    },
                )
                .unwrap();
        }
        assert_eq!(remote_ice_rx.len(), REMOTE_ICE_QUEUE_CAPACITY);
        assert!(handle
            .add_remote_ice(
                &talk.rtc_session_id,
                IceCandidateSignal {
                    sdp_mid: "audio".into(),
                    sdp_mline_index: 0,
                    candidate: "candidate:full 1 udp 1 192.0.2.1 9 typ host".into(),
                },
            )
            .unwrap_err()
            .contains("no available capacity"));

        actor_tx
            .try_send(ActorCommand::Authorize(
                RoutePermit::new(talk.clone(), Duration::from_secs(5)).unwrap(),
            ))
            .unwrap();
        actor_tx.try_send(ActorCommand::Revoke).unwrap();
        actor_tx.try_send(ActorCommand::Close).unwrap();

        assert!(matches!(
            actor_rx.try_recv(),
            Ok(ActorCommand::Authorize(permit)) if permit.binding == talk
        ));
        assert!(matches!(actor_rx.try_recv(), Ok(ActorCommand::Revoke)));
        assert!(matches!(actor_rx.try_recv(), Ok(ActorCommand::Close)));
        assert_eq!(caller_pcm_rx.len(), CALLER_PCM_QUEUE_CAPACITY);
        assert_eq!(remote_ice_rx.len(), REMOTE_ICE_QUEUE_CAPACITY);

        // Keep both receivers observably alive until all assertions above;
        // dropping either one would turn isolation into a false closed-channel pass.
        assert!(caller_pcm_rx.try_recv().is_ok());
        assert!(remote_ice_rx.try_recv().is_ok());
    }

    #[test]
    fn monitor_and_talk_get_caller_ingress_but_private_consult_never_does() {
        let state = active_state();
        let now = Instant::now();
        let expires_at = now + Duration::from_secs(30);
        let slot = |mode, suffix| PeerSlot {
            binding: binding(mode, 0, 0, suffix),
            lease_expires_at: expires_at,
            actor_tx: actor(),
            remote_ice_tx: remote_ice(),
            caller_pcm_tx: caller_pcm(),
        };
        let monitor = slot(MediaMode::Monitor, "monitor");
        let prepared = slot(MediaMode::PreparedTalk, "prepared");
        let prepared_consult = slot(MediaMode::PreparedConsult, "prepared-consult");
        let talk = slot(MediaMode::Talk, "talk");
        let consult = slot(MediaMode::Consult, "consult");

        for peer in [&monitor, &prepared, &talk] {
            assert!(routes_caller_ingress(
                &state.consent,
                peer,
                now,
                "call_a",
                1,
                0
            ));
        }
        assert!(!routes_caller_ingress(
            &state.consent,
            &prepared_consult,
            now,
            "call_a",
            1,
            0
        ));
        assert!(routes_caller_output(
            &state.consent,
            &monitor,
            now,
            "call_a",
            1,
            0
        ));
        assert!(!routes_caller_output(
            &state.consent,
            &prepared,
            now,
            "call_a",
            1,
            0
        ));
        assert!(!routes_caller_output(
            &state.consent,
            &talk,
            now,
            "call_a",
            1,
            0
        ));
        assert!(!routes_caller_ingress(
            &state.consent,
            &consult,
            now,
            "call_a",
            1,
            0
        ));
        assert!(!routes_caller_output(
            &state.consent,
            &consult,
            now,
            "call_a",
            1,
            0
        ));
    }

    #[test]
    fn consult_is_never_caller_bound() {
        let mut state = active_state();
        let consult = prepare_active_consult(&mut state, "consult", Duration::from_secs(30));
        let slot = state.peers.get(&consult.rtc_session_id).unwrap();
        let now = Instant::now();

        assert_eq!(state.service_mode, ServiceMode::ConsultActive);
        assert!(state.allows_consult(&consult, now));
        assert!(state.active_route.is_none());
        assert!(state.pending_route.is_none());
        assert!(!routes_caller_ingress(
            &state.consent,
            slot,
            now,
            "call_a",
            1,
            1
        ));
        assert!(!routes_caller_output(
            &state.consent,
            slot,
            now,
            "call_a",
            1,
            1
        ));
    }

    #[test]
    fn consult_requires_pcm_before_isolation_and_returns_when_pcm_stalls() {
        let mut state = active_state();
        let prepared = binding(MediaMode::PreparedConsult, 0, 0, "consult-proof");
        insert(&mut state, prepared.clone(), Duration::from_secs(30));
        state
            .request_soft_hold(prepared.clone(), Duration::from_secs(30))
            .unwrap();
        assert!(matches!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::PrepareConsult { .. })
        ));
        state.ack_prepare(&prepared, Instant::now()).unwrap();
        state
            .close_peer(&prepared.rtc_session_id, "active_rebind")
            .unwrap();

        let consult = binding(MediaMode::Consult, 1, 0, "consult-proof");
        insert(&mut state, consult.clone(), Duration::from_secs(30));
        assert!(state
            .request_consult(consult.clone(), Duration::from_secs(30))
            .is_err());
        assert_eq!(state.service_mode, ServiceMode::AokieActive);
        assert!(!state.radio_reserved());

        let first_pcm = Instant::now();
        state.note_consult_pcm_received(&consult, first_pcm);
        state
            .request_consult(consult.clone(), Duration::from_secs(30))
            .unwrap();
        assert_eq!(state.service_mode, ServiceMode::ConsultPending);
        assert!(state.radio_reserved());
        assert!(matches!(
            state.next_transition(first_pcm).0,
            Some(RadioTransition::EnterConsult { .. })
        ));
        state.ack_enter_consult(&consult, first_pcm).unwrap();

        let still_streaming = first_pcm + Duration::from_millis(1_500);
        state.note_consult_pcm_received(&consult, still_streaming);
        assert!(state.next_transition(still_streaming).0.is_none());
        state
            .renew(&consult, Duration::from_secs(30))
            .expect("lease heartbeat may renew authority");
        assert!(matches!(
            state
                .next_transition(still_streaming + ONGOING_CONSULT_PCM_TIMEOUT)
                .0,
            Some(RadioTransition::ReturnToAokie { .. })
        ));
        assert_eq!(state.service_mode, ServiceMode::ReturningToAokie);
    }

    #[test]
    fn consult_revoke_returns_to_aokie_without_a_talk_promotion() {
        let mut state = active_state();
        let consult = prepare_active_consult(&mut state, "consult", Duration::from_secs(30));

        state.request_revoke(&consult, "consult_complete").unwrap();
        assert!(!state.allows_consult(&consult, Instant::now()));
        assert!(state.active_route.is_none());
        assert!(state.pending_route.is_none());
        assert_eq!(state.service_mode, ServiceMode::ReturningToAokie);
        assert!(matches!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::ReturnToAokie { .. })
        ));
        state.ack_return().unwrap();
        assert_eq!(state.current_record().unwrap().owner_epoch, 2);
        assert_eq!(state.service_mode, ServiceMode::AokieActive);
        assert!(!state.radio_reserved());
    }

    #[test]
    fn consult_expiry_and_consent_revocation_fail_closed() {
        let mut expired = active_state();
        let consult = prepare_active_consult(&mut expired, "expired", Duration::from_secs(30));
        expired
            .active_consult
            .as_mut()
            .expect("active consult")
            .expires_at = Instant::now() - Duration::from_millis(1);
        assert!(!expired.allows_consult(&consult, Instant::now()));
        assert!(matches!(
            expired.next_transition(Instant::now()).0,
            Some(RadioTransition::ReturnToAokie { .. })
        ));
        assert!(expired.active_route.is_none());

        let mut revoked = active_state();
        let consult = prepare_active_consult(&mut revoked, "revoked", Duration::from_secs(30));
        revoked.consent.consult_enabled = false;
        assert!(!revoked.allows_consult(&consult, Instant::now()));
        assert!(!revoked.consent.effective().consult_enabled);
    }

    #[test]
    fn consent_is_fail_closed_and_expires_for_every_remote_lane() {
        let mut state = active_state();
        let monitor = binding(MediaMode::Monitor, 0, 0, "monitor");
        assert!(state.validate_open(&monitor).is_ok());

        state.consent.expires_at = Some("2000-01-01T00:00:00Z".into());
        assert!(state.validate_open(&monitor).is_err());
        assert!(!state.consent.effective().captions_enabled);
        assert!(!state.consent.effective().assistance_enabled);

        state.consent.expires_at = Some("not-an-rfc3339-instant".into());
        assert!(state.validate_open(&monitor).is_err());
        assert!(
            !state.consent.effective().monitor_enabled,
            "a malformed policy timestamp can never authorize audio"
        );

        state.consent.expires_at = Some("2000-01-01T23:59:59+14:00".into());
        assert!(state.validate_open(&monitor).is_err());
        assert!(
            !state.consent.effective().monitor_enabled,
            "offset timestamps are compared as instants rather than text"
        );

        state.consent.expires_at = Some("2999-01-01T00:00:00Z".into());
        state.consent.acknowledged = false;
        assert!(state.validate_open(&monitor).is_err());
    }

    #[test]
    fn wall_clock_consent_expiry_cancels_preflight_and_returns_an_active_talker() {
        let pending = RemoteMediaHandle::spawn().unwrap();
        {
            let mut state = pending.inner.state.lock().unwrap();
            *state = active_state();
            let prepared = binding(MediaMode::PreparedTalk, 0, 15, "expiry-pending");
            insert(&mut state, prepared.clone(), Duration::from_secs(30));
            state
                .request_soft_hold(prepared, Duration::from_secs(20))
                .unwrap();
            state.consent.expires_at = Some("2000-01-01T00:00:00Z".into());
        }
        pending.refresh_reserved();
        assert!(pending.next_radio_transition().is_none());
        let snapshot = pending.snapshot();
        assert_eq!(snapshot.service_mode, ServiceMode::AokieActive);
        assert!(!snapshot.radio_reserved);
        assert_eq!(snapshot.owner_epoch, 1);

        let active = RemoteMediaHandle::spawn().unwrap();
        let talk = {
            let mut state = active.inner.state.lock().unwrap();
            *state = active_state();
            let talk =
                prepare_active_talk(&mut state, "expiry-active", 16, Duration::from_secs(30));
            state
                .request_takeover(talk.clone(), Duration::from_secs(20))
                .unwrap();
            state.next_transition(Instant::now());
            state.ack_enter(&talk, Instant::now(), true).unwrap();
            state.consent.expires_at = Some("2000-01-01T00:00:00Z".into());
            assert!(
                !state.allows_talk(&talk, Instant::now()),
                "expired consent blocks caller TX before the radio tick"
            );
            talk
        };
        active.refresh_reserved();
        assert!(matches!(
            active.next_radio_transition(),
            Some(RadioTransition::ReturnToAokie { .. })
        ));
        let snapshot = active.snapshot();
        assert_eq!(snapshot.service_mode, ServiceMode::ReturningToAokie);
        assert!(snapshot.radio_reserved);
        assert_ne!(snapshot.talk_lease_id.as_deref(), talk.lease_id.as_deref());
    }

    #[test]
    fn provisional_takeover_advances_epoch_without_ever_opening_caller_tx() {
        let mut state = active_state();
        let prepared = binding(MediaMode::PreparedTalk, 0, 11, "prepared");
        insert(&mut state, prepared.clone(), Duration::from_secs(30));
        state
            .request_soft_hold(prepared.clone(), Duration::from_secs(20))
            .unwrap();
        assert!(matches!(
            state.next_transition(Instant::now()).0,
            Some(RadioTransition::PrepareHuman { .. })
        ));
        assert_eq!(state.ack_prepare(&prepared, Instant::now()).unwrap(), 1);
        assert_eq!(state.service_mode, ServiceMode::AokieActive);
        assert!(!state.radio_reserved());
        assert!(state.pending_route.is_none());
        assert!(state.active_route.is_none());
        assert!(!state.allows_talk(&prepared, Instant::now()));
        assert!(RoutePermit::new(prepared, Duration::from_secs(1)).is_err());
    }
}
