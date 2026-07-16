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
use std::time::{Duration, Instant};

use aokie_media::{
    DesktopPeer, IceCandidateSignal, IceServerConfig, MediaMode, OwnedAudioFrame, PeerEvent,
    PeerOptions, RoutePermit, SdpSignal, SessionBinding, MEDIA_SAMPLE_RATE_HZ,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub const MAX_REMOTE_PEERS: usize = 6;
pub const REMOTE_CONSENT_POLICY_ID: &str = "aokie_remote_access";
const ACTOR_QUEUE_CAPACITY: usize = 12;
const MANAGER_QUEUE_CAPACITY: usize = 32;
const ROUTED_AUDIO_FRAMES: usize = 16;
const EVENT_QUEUE_CAPACITY: usize = 128;
const MAX_LEASE_TTL: Duration = Duration::from_secs(5 * 60);
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

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
    pub radio_reserved: bool,
    pub dropped_sco_frames: u64,
    pub quarantined_talk_frames: u64,
    pub dropped_events: u64,
    pub consent: RemoteConsentGate,
    pub captions: Vec<RemoteCaption>,
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
    fn is_current(&self) -> bool {
        self.enabled
            && self.acknowledged
            && self
                .expires_at
                .as_deref()
                .is_none_or(|expiry| expiry > aokie_core::events::now_iso8601().as_str())
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
}

#[derive(Clone)]
struct ActiveConsult {
    binding: SessionBinding,
    expires_at: Instant,
}

struct ActiveRoute {
    permit: RoutePermit,
}

struct RemoteMediaState {
    calls: HashMap<String, CallRecord>,
    call_order: VecDeque<String>,
    next_call_epoch: u64,
    remote_revision: u64,
    current_call_id: Option<String>,
    current_call_active: bool,
    service_mode: ServiceMode,
    peers: HashMap<String, PeerSlot>,
    pending_prepare: Option<PendingPrepare>,
    prepared_claim: Option<PreparedClaim>,
    active_consult: Option<ActiveConsult>,
    pending_route: Option<PendingRoute>,
    active_route: Option<ActiveRoute>,
    talk_audio_binding: Option<SessionBinding>,
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
            remote_revision: 0,
            current_call_id: None,
            current_call_active: false,
            service_mode: ServiceMode::AokieActive,
            peers: HashMap::new(),
            pending_prepare: None,
            prepared_claim: None,
            active_consult: None,
            pending_route: None,
            active_route: None,
            talk_audio_binding: None,
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
    CallerPcm(Arc<Vec<i16>>),
    AddIce(IceCandidateSignal),
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
        let state = self.inner.state.lock().ok();
        let (
            call_id,
            call_epoch,
            owner_epoch,
            remote_revision,
            service_mode,
            peer_count,
            binding,
            talk_audio_forwarded,
            consent,
            captions,
        ) = state
            .as_ref()
            .map(|state| {
                let current = state.current_record();
                let route = state
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
                            .pending_prepare
                            .as_ref()
                            .map(|pending| &pending.binding)
                    })
                    .or_else(|| {
                        state
                            .prepared_claim
                            .as_ref()
                            .map(|prepared| &prepared.provisional_binding)
                    });
                (
                    state.current_call_id.clone(),
                    current.map_or(0, |record| record.call_epoch),
                    current.map_or(0, |record| record.owner_epoch),
                    state.remote_revision,
                    state.service_mode,
                    state.peers.len(),
                    route.cloned(),
                    route.is_some_and(|binding| state.talk_audio_binding.as_ref() == Some(binding)),
                    state.consent.effective(),
                    state.captions.iter().cloned().collect::<Vec<_>>(),
                )
            })
            .unwrap_or((
                None,
                0,
                0,
                0,
                ServiceMode::Recovering,
                0,
                None,
                false,
                RemoteConsentGate::default(),
                Vec::new(),
            ));
        RemoteMediaSnapshot {
            call_id,
            call_epoch,
            owner_epoch,
            remote_revision,
            service_mode,
            peer_count,
            talk_device_id: binding.as_ref().map(|binding| binding.device_id.clone()),
            talk_lease_id: binding
                .as_ref()
                .and_then(|binding| binding.lease_id.clone()),
            talk_fence: binding.as_ref().map_or(0, |binding| binding.fence),
            talk_audio_forwarded,
            radio_reserved: self.radio_reserved(),
            dropped_sco_frames: self.inner.dropped_sco_frames.load(Ordering::Relaxed),
            quarantined_talk_frames: self.inner.quarantined_talk_frames.load(Ordering::Relaxed),
            dropped_events: self.inner.dropped_events.load(Ordering::Relaxed),
            consent,
            captions,
        }
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
            if let Some(binding) = slots
                .iter()
                .find(|slot| {
                    matches!(
                        slot.binding.mode,
                        MediaMode::PreparedConsult
                            | MediaMode::Consult
                            | MediaMode::PreparedTalk
                            | MediaMode::Talk
                    )
                })
                .map(|slot| slot.binding.clone())
            {
                state.active_route = None;
                state.active_consult = None;
                state.pending_route = None;
                state.pending_prepare = None;
                state.prepared_claim = None;
                state.return_binding = Some(binding);
                state.return_reason = Some("remote_consent_revoked".into());
                state.return_dispatched = false;
                state.service_mode = ServiceMode::ReturningToAokie;
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
        let actor = self.actor_for(rtc_session_id)?;
        actor
            .try_send(ActorCommand::AddIce(candidate))
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

    /// Reserve the radio for a provisional, receive-only consultation.  The
    /// Companion microphone does not exist in this phase.  The radio thread
    /// must flush caller TX and ACK the software hold before an active consult
    /// lease can be minted.
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

    /// Activate the isolated consult lane after the rotated lease has created
    /// a fresh bidirectional peer.  This never installs a caller RoutePermit.
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
        self.emit(binding, RemoteMediaEventKind::ConsultActive);
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
            if !state.allows_talk(binding, Instant::now()) {
                return false;
            }
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
            state.request_takeover(binding.clone(), ttl)?;
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
        let (actor, event_binding) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.request_revoke(binding, reason)?
        };
        if let Some(actor) = actor {
            let _ = actor.try_send(ActorCommand::Revoke);
        }
        self.refresh_reserved();
        self.emit(
            event_binding,
            RemoteMediaEventKind::ReturningToAokie {
                reason: sanitize_reason(reason),
            },
        );
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

    /// Revoke every peer and caller route immediately.  Used whenever the
    /// authenticated signalling socket is lost: the radio stays reserved
    /// until its physical return flush ACK, so Aokie and a stale remote can
    /// never overlap on caller TX.
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
        self.inner.try_push_caller_output(samples, sample_rate);
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
            let allowed = self
                .inner
                .state
                .lock()
                .ok()
                .is_some_and(|state| state.allows_consult(&routed.binding, Instant::now()));
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
        let actor = {
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
                .map(|slot| slot.actor_tx.clone())
        };
        actor.is_some_and(|actor| actor.try_send(ActorCommand::CallerPcm(normalized)).is_ok())
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
            let allowed = self
                .inner
                .state
                .lock()
                .ok()
                .is_some_and(|mut state| state.allows_talk(&routed.binding, Instant::now()));
            self.refresh_reserved();
            if allowed {
                return Some(routed.frame);
            }
            self.inner
                .quarantined_talk_frames
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn next_radio_transition(&self) -> Option<RadioTransition> {
        let (transition, revoked, expired) = {
            let mut state = self.inner.state.lock().ok()?;
            let now = Instant::now();
            let expired = state.expire_peers(now);
            let (transition, revoked) = state.next_transition(now);
            (transition, revoked, expired)
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
        self.refresh_reserved();
        transition
    }

    /// Physical ACK from the radio loop after flushing every queued Aokie
    /// sample.  Only this operation opens both transmit gates.
    pub fn ack_enter_human(&self, binding: &SessionBinding) -> Result<(), String> {
        let (actor, permit) = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "remote media state poisoned".to_string())?;
            state.ack_enter(binding, Instant::now())?
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

    fn actor_for(&self, rtc_session_id: &str) -> Result<mpsc::Sender<ActorCommand>, String> {
        self.inner
            .state
            .lock()
            .map_err(|_| "remote media state poisoned".to_string())?
            .peers
            .get(rtc_session_id)
            .map(|slot| slot.actor_tx.clone())
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
    ) && slot.binding.mode != MediaMode::Consult
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
        let Ok(state) = self.state.try_lock() else {
            self.dropped_sco_frames.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let now = Instant::now();
        let Some(current_id) = state.current_call_id.as_deref() else {
            return;
        };
        let Some(record) = state.current_record() else {
            return;
        };
        for slot in state.peers.values() {
            if !routes_caller_ingress(
                &state.consent,
                slot,
                now,
                current_id,
                record.call_epoch,
                record.owner_epoch,
            ) {
                continue;
            }
            if slot
                .actor_tx
                .try_send(ActorCommand::CallerPcm(normalized.clone()))
                .is_err()
            {
                self.dropped_sco_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn try_push_caller_output(&self, samples: &[i16], sample_rate: u32) {
        if samples.is_empty() || sample_rate == 0 {
            return;
        }
        let normalized = Arc::new(resample_mono(samples, sample_rate, MEDIA_SAMPLE_RATE_HZ));
        if normalized.is_empty() {
            return;
        }
        let Ok(state) = self.state.try_lock() else {
            self.dropped_sco_frames.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let now = Instant::now();
        let Some(current_id) = state.current_call_id.as_deref() else {
            return;
        };
        let Some(record) = state.current_record() else {
            return;
        };
        for slot in state.peers.values() {
            if !routes_caller_output(
                &state.consent,
                slot,
                now,
                current_id,
                record.call_epoch,
                record.owner_epoch,
            ) {
                continue;
            }
            if slot
                .actor_tx
                .try_send(ActorCommand::CallerPcm(normalized.clone()))
                .is_err()
            {
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
    inner.try_push_caller_output(samples, sample_rate);
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
    ACTIVE_REMOTE_MEDIA
        .get()
        .and_then(|slot| slot.try_lock().ok())
        .and_then(|active| active.upgrade())
        .is_some_and(|inner| inner.radio_reserved.load(Ordering::Acquire))
}

impl RemoteMediaState {
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

    fn reserve_open(&self, binding: &SessionBinding) -> Result<(), String> {
        self.validate_open(binding)
    }

    fn insert_peer(&mut self, slot: PeerSlot) -> Result<(), String> {
        // Revalidate after the asynchronous SDP operation.  A call/owner
        // transition that raced answer creation makes the answer unusable.
        self.validate_open(&slot.binding)?;
        self.peers.insert(slot.binding.rtc_session_id.clone(), slot);
        self.bump_revision();
        Ok(())
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
            binding,
            expires_at: Instant::now() + requested_ttl.min(remaining),
            transition_dispatched: false,
        });
        self.service_mode = ServiceMode::SoftHold;
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
        self.validate_live_binding(&binding)?;
        let record = self
            .current_record()
            .ok_or_else(|| "physical call epoch is unavailable".to_string())?;
        if binding.owner_epoch != record.owner_epoch {
            return Err("consult ownerEpoch is not the prepared hold epoch".into());
        }
        if self.active_consult.is_some()
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
        let prepared = self.prepared_claim.as_ref().ok_or_else(|| {
            "consultation has not completed software-hold preparation".to_string()
        })?;
        if prepared.expires_at <= Instant::now()
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
        self.active_consult = Some(ActiveConsult {
            binding,
            expires_at: Instant::now() + requested_ttl.min(remaining),
        });
        self.prepared_claim = None;
        self.service_mode = ServiceMode::ConsultActive;
        self.bump_revision();
        Ok(())
    }

    fn request_takeover(
        &mut self,
        binding: SessionBinding,
        requested_ttl: Duration,
    ) -> Result<(), String> {
        if binding.mode != MediaMode::Talk {
            return Err("only a talk peer may request caller ownership".into());
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
        });
        self.prepared_claim = None;
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
            self.active_route = Some(ActiveRoute {
                permit: permit.clone(),
            });
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
        self.bump_revision();
        Ok((actor, permit))
    }

    fn request_revoke(
        &mut self,
        binding: &SessionBinding,
        reason: &str,
    ) -> Result<(Option<mpsc::Sender<ActorCommand>>, SessionBinding), String> {
        self.validate_live_binding(binding)?;
        let actor = self
            .peers
            .get(&binding.rtc_session_id)
            .map(|slot| slot.actor_tx.clone());
        let matches_route = self
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
                .is_some_and(|route| route.permit.binding == *binding)
            || self
                .pending_prepare
                .as_ref()
                .is_some_and(|pending| pending.binding == *binding)
            || self.prepared_claim.as_ref().is_some_and(|prepared| {
                let original = &prepared.provisional_binding;
                original == binding
                    || (original.call_id == binding.call_id
                        && original.call_epoch == binding.call_epoch
                        && original.device_id == binding.device_id
                        && original.rtc_session_id == binding.rtc_session_id
                        && original.lease_id == binding.lease_id
                        && original.fence == binding.fence)
            });
        if !matches_route {
            return Err("binding does not own or await the caller route".into());
        }
        self.pending_prepare = None;
        self.prepared_claim = None;
        self.active_consult = None;
        self.pending_route = None;
        self.active_route = None;
        self.return_binding = Some(binding.clone());
        self.return_reason = Some(sanitize_reason(reason));
        self.return_dispatched = false;
        self.service_mode = ServiceMode::ReturningToAokie;
        self.bump_revision();
        Ok((actor, binding.clone()))
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
        let needs_return = self
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
        if needs_return {
            self.pending_route = None;
            self.active_route = None;
            self.active_consult = None;
            self.return_binding = Some(slot.binding.clone());
            self.return_reason = Some(sanitize_reason(reason));
            self.return_dispatched = false;
            self.service_mode = ServiceMode::ReturningToAokie;
        }
        self.bump_revision();
        Ok((slot, needs_return))
    }

    fn fail_closed_all(&mut self, reason: &str) -> (Vec<PeerSlot>, Option<SessionBinding>) {
        let slots = self.peers.drain().map(|(_, slot)| slot).collect::<Vec<_>>();
        let returning = self
            .active_route
            .take()
            .map(|route| route.permit.binding)
            .or_else(|| self.active_consult.take().map(|route| route.binding))
            .or_else(|| self.pending_route.take().map(|route| route.permit.binding))
            .or_else(|| self.pending_prepare.take().map(|pending| pending.binding))
            .or_else(|| {
                self.prepared_claim
                    .take()
                    .map(|prepared| prepared.provisional_binding)
            });
        if let Some(binding) = returning.clone() {
            self.return_binding = Some(binding);
            self.return_reason = Some(sanitize_reason(reason));
            self.return_dispatched = false;
            self.service_mode = ServiceMode::ReturningToAokie;
        }
        self.bump_revision();
        (slots, returning)
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
    ) -> (Option<RadioTransition>, Option<mpsc::Sender<ActorCommand>>) {
        let mut revoked = None;
        if let Some(active) = self.active_consult.as_ref() {
            let peer_expired = self
                .peers
                .get(&active.binding.rtc_session_id)
                .is_none_or(|slot| slot.lease_expires_at <= now);
            if active.expires_at <= now || peer_expired {
                let binding = active.binding.clone();
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.active_consult = None;
                self.return_binding = Some(binding);
                self.return_reason = Some("consult_lease_expired".into());
                self.return_dispatched = false;
                self.service_mode = ServiceMode::ReturningToAokie;
                self.bump_revision();
            }
        }
        if let Some(active) = self.active_route.as_ref() {
            let peer_expired = self
                .peers
                .get(&active.permit.binding.rtc_session_id)
                .is_none_or(|slot| slot.lease_expires_at <= now);
            if !active.permit.is_current_for(&active.permit.binding, now) || peer_expired {
                let binding = active.permit.binding.clone();
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.active_route = None;
                self.return_binding = Some(binding);
                self.return_reason = Some("lease_expired".into());
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
            if !pending.permit.is_current_for(&pending.permit.binding, now) || peer_expired {
                let binding = pending.permit.binding.clone();
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.pending_route = None;
                self.return_binding = Some(binding);
                self.return_reason = Some("lease_expired_before_physical_ack".into());
                self.return_dispatched = false;
                self.service_mode = ServiceMode::ReturningToAokie;
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
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.pending_prepare = None;
                self.return_binding = Some(binding);
                self.return_reason = Some("prepared_lease_expired".into());
                self.return_dispatched = false;
                self.service_mode = ServiceMode::ReturningToAokie;
                self.bump_revision();
            }
        }
        if let Some(prepared) = self.prepared_claim.as_ref() {
            if prepared.expires_at <= now {
                let binding = prepared.provisional_binding.clone();
                revoked = self
                    .peers
                    .get(&binding.rtc_session_id)
                    .map(|slot| slot.actor_tx.clone());
                self.prepared_claim = None;
                self.return_binding = Some(binding);
                self.return_reason = Some("prepared_lease_expired".into());
                self.return_dispatched = false;
                self.service_mode = ServiceMode::ReturningToAokie;
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
                );
            }
            let _ = binding;
            return (None, revoked);
        }
        if let Some(pending) = self.pending_route.as_mut() {
            if !pending.transition_dispatched {
                pending.transition_dispatched = true;
                return (
                    Some(RadioTransition::EnterHuman {
                        binding: pending.permit.binding.clone(),
                    }),
                    revoked,
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
                return (Some(transition), revoked);
            }
        }
        (None, revoked)
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
        });
        self.service_mode = if binding.mode == MediaMode::PreparedConsult {
            ServiceMode::ConsultPending
        } else {
            ServiceMode::HumanPending
        };
        self.bump_revision();
        Ok(confirmed_owner_epoch)
    }

    fn ack_enter(
        &mut self,
        binding: &SessionBinding,
        now: Instant,
    ) -> Result<(mpsc::Sender<ActorCommand>, RoutePermit), String> {
        self.validate_live_binding(binding)?;
        if !self.current_call_active {
            return Err("physical call ended before takeover ACK".into());
        }
        let pending = self
            .pending_route
            .take()
            .ok_or_else(|| "there is no pending takeover".to_string())?;
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
        self.pending_prepare = None;
        self.prepared_claim = None;
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
        let allowed = self.current_call_active
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
                let reserved = state
                    .lock()
                    .map_err(|_| "remote media state poisoned".to_string())
                    .and_then(|state| state.reserve_open(&binding));
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
                        let slot = PeerSlot {
                            binding: binding.clone(),
                            lease_expires_at: Instant::now()
                                + Duration::from_millis(request.lease_ttl_ms),
                            actor_tx: actor_tx.clone(),
                        };
                        let inserted = state
                            .lock()
                            .map_err(|_| "remote media state poisoned".to_string())
                            .and_then(|mut state| state.insert_peer(slot));
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

async fn peer_actor(
    mut peer: DesktopPeer,
    mut commands: mpsc::Receiver<ActorCommand>,
    talk_tx: std_mpsc::SyncSender<RoutedAudio>,
    consult_tx: std_mpsc::SyncSender<RoutedAudio>,
    events: EventEmitter,
    quarantined: Arc<AtomicU64>,
) {
    let binding = peer.binding().clone();
    let mut microphone_pcm_observed = false;
    loop {
        let mut worked = false;
        while let Ok(command) = commands.try_recv() {
            worked = true;
            match command {
                ActorCommand::CallerPcm(samples) => {
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
                ActorCommand::AddIce(candidate) => {
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
                ActorCommand::Authorize(permit) => {
                    if let Err(error) = peer.authorize_caller_transmit(permit) {
                        events.emit(
                            &binding,
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
                    return;
                }
            }
        }
        if commands.is_closed() {
            peer.close();
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

    fn insert(state: &mut RemoteMediaState, binding: SessionBinding, ttl: Duration) {
        state
            .insert_peer(PeerSlot {
                binding,
                lease_expires_at: Instant::now() + ttl,
                actor_tx: actor(),
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
        state
            .close_peer(&prepared.rtc_session_id, "active_rebind")
            .unwrap();
        let consult = binding(MediaMode::Consult, 1, 0, suffix);
        insert(state, consult.clone(), ttl);
        state.request_consult(consult.clone(), ttl).unwrap();
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
        state.ack_enter(&talk, Instant::now()).unwrap();
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
            state.ack_enter(&talk, Instant::now()).unwrap();
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
            state.ack_enter(&talk, Instant::now()).unwrap();
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
        state.ack_enter(&talk, Instant::now()).unwrap();
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
    fn monitor_gets_both_caller_directions_but_talker_never_gets_its_output() {
        let state = active_state();
        let now = Instant::now();
        let expires_at = now + Duration::from_secs(30);
        let slot = |mode, suffix| PeerSlot {
            binding: binding(mode, 0, 0, suffix),
            lease_expires_at: expires_at,
            actor_tx: actor(),
        };
        let monitor = slot(MediaMode::Monitor, "monitor");
        let prepared = slot(MediaMode::PreparedTalk, "prepared");
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
        let consult = prepare_active_consult(&mut expired, "expired", Duration::from_millis(2));
        std::thread::sleep(Duration::from_millis(4));
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

        state.consent.expires_at = Some("2999-01-01T00:00:00Z".into());
        state.consent.acknowledged = false;
        assert!(state.validate_open(&monitor).is_err());
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
        assert_eq!(state.service_mode, ServiceMode::HumanPending);
        assert!(state.pending_route.is_none());
        assert!(state.active_route.is_none());
        assert!(!state.allows_talk(&prepared, Instant::now()));
        assert!(RoutePermit::new(prepared, Duration::from_secs(1)).is_err());
    }
}
