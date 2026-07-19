use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::{native::NativeAudioSource, AudioSourceOptions};
use libwebrtc::audio_stream::native::{NativeAudioStream, NativeAudioStreamOptions};
use libwebrtc::data_channel::{DataChannel, DataChannelInit, DataChannelState};
use libwebrtc::ice_candidate::IceCandidate;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::{
    AnswerOptions, IceGatheringState, OfferOptions, PeerConnection, PeerConnectionState,
};
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::peer_connection_factory::{
    ContinualGatheringPolicy, IceServer, IceTransportsType, PeerConnectionFactory, RtcConfiguration,
};
use libwebrtc::rtp_transceiver::{RtpTransceiverDirection, RtpTransceiverInit};
use libwebrtc::session_description::{SdpType, SessionDescription};
use libwebrtc::stats::RtcStats;
use libwebrtc::MediaType;
use tokio::sync::{mpsc, Mutex};

use crate::{
    IceCandidateSignal, IceServerConfig, MediaError, MediaMode, OwnedAudioFrame, PcmPacketizer,
    RouteGate, RoutePermit, SdpSignal, SdpSignalType, SessionBinding, MEDIA_CHANNELS,
    MEDIA_SAMPLE_RATE_HZ,
};

const EVENT_QUEUE_CAPACITY: usize = 64;
const REMOTE_AUDIO_QUEUE_FRAMES: usize = 12;
const MICROPHONE_AUTHORITY_CHANNEL: &str = "aokie-microphone-authority-v1";
const MAX_MICROPHONE_AUTHORITY_BYTES: usize = 2_048;
const MICROPHONE_STATS_POLL: Duration = Duration::from_millis(50);
const MICROPHONE_PCM_PROGRESS_WINDOW: Duration = Duration::from_millis(125);
const MICROPHONE_READY_PROGRESS_FLOOR: Duration = Duration::from_millis(100);
// A fresh baseline normally re-proves in three 50 ms polls. Keep enough room
// for scheduling/stats jitter without borrowing any extra PCM authority.
const MICROPHONE_REPROOF_TIMEOUT: Duration = Duration::from_millis(500);
// Two isolated native-stats discontinuities may recover; a third within this
// peer-local window is no longer a transient and fails the exact route.
const MICROPHONE_DISCONTINUITY_WINDOW: Duration = Duration::from_secs(10);
const MICROPHONE_DISCONTINUITY_BUDGET: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    LocalIce(IceCandidateSignal),
    IceComplete,
    ConnectionState(&'static str),
    RemoteAudioReady,
    /// Companion's exact-peer DTLS authority channel is open. Native capture
    /// must not arm before this edge because its proof could not be delivered.
    MicrophoneAuthorityReady,
    /// The exact Desktop-side talk peer decoded its first microphone PCM
    /// frame. The frame itself remains quarantined until the independent
    /// caller-route permit opens; this event is readiness evidence only.
    RemoteMicrophoneReady,
    ProtocolViolation(&'static str),
}

/// Current, native WebRTC-reported audio levels for a Companion endpoint.
///
/// A missing value means libwebrtc has not reported real samples for that
/// direction yet. Callers must not turn that absence into an invented zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompanionAudioLevels {
    pub microphone_level_permille: Option<u16>,
    pub remote_level_permille: Option<u16>,
}

/// Cumulative counters from the exact Companion microphone source and its
/// outbound audio RTP stream. Unmute proof snapshots a post-arm baseline and
/// requires every counter to advance; old samples from before mute cannot
/// satisfy that transaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MicrophoneSampleProgress {
    pub samples_captured: u64,
    pub packets_sent: u64,
    pub bytes_sent: u64,
}

impl MicrophoneSampleProgress {
    pub fn strictly_advanced_from(self, baseline: Self) -> bool {
        self.samples_captured > baseline.samples_captured
            && self.packets_sent > baseline.packets_sent
            && self.bytes_sent > baseline.bytes_sent
    }
}

#[derive(Debug, Clone)]
pub struct PeerOptions {
    pub ice_servers: Vec<IceServerConfig>,
    pub relay_only: bool,
    pub sample_rate: u32,
    pub channels: u32,
    pub max_pcm_buffer_ms: u32,
    /// Stable platform endpoint GUIDs. `None` follows the operating-system
    /// default when the ADM is created.
    pub recording_device_guid: Option<String>,
    pub playout_device_guid: Option<String>,
}

impl Default for PeerOptions {
    fn default() -> Self {
        Self {
            ice_servers: Vec::new(),
            relay_only: false,
            sample_rate: MEDIA_SAMPLE_RATE_HZ,
            channels: MEDIA_CHANNELS,
            max_pcm_buffer_ms: 100,
            recording_device_guid: None,
            playout_device_guid: None,
        }
    }
}

impl PeerOptions {
    fn validate(&self) -> Result<(), MediaError> {
        IceServerConfig::validate_all(&self.ice_servers)?;
        if self.sample_rate == 0
            || !self.sample_rate.is_multiple_of(100)
            || self.channels != 1
            || !(20..=500).contains(&self.max_pcm_buffer_ms)
        {
            return Err(MediaError::InvalidBinding("audio format"));
        }
        if self
            .recording_device_guid
            .as_deref()
            .is_some_and(|guid| !valid_device_guid(guid))
            || self
                .playout_device_guid
                .as_deref()
                .is_some_and(|guid| !valid_device_guid(guid))
        {
            return Err(MediaError::InvalidBinding("audio endpoint GUID"));
        }
        Ok(())
    }

    fn rtc_configuration(&self) -> RtcConfiguration {
        RtcConfiguration {
            ice_servers: self
                .ice_servers
                .iter()
                .map(|server| IceServer {
                    urls: server.urls.clone(),
                    username: server.username.clone(),
                    password: server.credential.clone(),
                })
                .collect(),
            continual_gathering_policy: ContinualGatheringPolicy::GatherContinually,
            ice_transport_type: if self.relay_only {
                IceTransportsType::Relay
            } else {
                IceTransportsType::All
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformAudioDevice {
    pub guid: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformAudioDevices {
    pub recording: Vec<PlatformAudioDevice>,
    pub playout: Vec<PlatformAudioDevice>,
}

fn valid_device_guid(guid: &str) -> bool {
    !guid.is_empty() && guid.len() <= 1_024 && !guid.chars().any(char::is_control)
}

fn devices_from_factory(
    factory: &PeerConnectionFactory,
) -> Result<PlatformAudioDevices, MediaError> {
    let recording_count = factory.recording_devices();
    let playout_count = factory.playout_devices();
    if recording_count < 0 || playout_count < 0 {
        return Err(MediaError::AudioUnavailable(
            "operating-system audio endpoint enumeration failed".into(),
        ));
    }
    let recording = (0..recording_count as u16)
        .filter_map(|index| {
            let guid = factory.recording_device_guid(index);
            valid_device_guid(&guid).then(|| PlatformAudioDevice {
                guid,
                label: factory.recording_device_name(index),
            })
        })
        .collect();
    let playout = (0..playout_count as u16)
        .filter_map(|index| {
            let guid = factory.playout_device_guid(index);
            valid_device_guid(&guid).then(|| PlatformAudioDevice {
                guid,
                label: factory.playout_device_name(index),
            })
        })
        .collect();
    Ok(PlatformAudioDevices { recording, playout })
}

/// Enumerates desktop ADM endpoints while no Companion peer owns the module.
/// Callers must serialize this with peer creation; the temporary guard stops
/// and releases the ADM when enumeration completes.
pub fn enumerate_platform_audio_devices() -> Result<PlatformAudioDevices, MediaError> {
    let factory = PeerConnectionFactory::default();
    if !factory.acquire_platform_adm() {
        return Err(MediaError::AudioUnavailable(
            "no operating-system audio device module".into(),
        ));
    }
    let _guard = PlatformAdmGuard {
        factory: factory.clone(),
    };
    devices_from_factory(&factory)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicrophoneAuthorityAction {
    Arm,
    Disarm,
}

impl MicrophoneAuthorityAction {
    fn label(self) -> &'static str {
        match self {
            Self::Arm => "arm",
            Self::Disarm => "disarm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MicrophoneAuthorityProof {
    action: MicrophoneAuthorityAction,
    generation: u64,
}

#[derive(Debug, Default)]
struct RemoteMicrophoneAuthority {
    generation: u64,
    armed: bool,
    protocol_failed: bool,
}

impl RemoteMicrophoneAuthority {
    fn apply(&mut self, proof: MicrophoneAuthorityProof) -> Result<(), &'static str> {
        if self.protocol_failed {
            self.armed = false;
            return Err("microphone authority protocol is permanently failed");
        }
        if proof.generation < self.generation {
            return Ok(());
        }
        let armed = proof.action == MicrophoneAuthorityAction::Arm;
        if proof.generation == self.generation {
            return if self.armed == armed {
                Ok(())
            } else {
                self.fail();
                Err("conflicting microphone authority replay")
            };
        }
        self.generation = proof.generation;
        self.armed = armed;
        Ok(())
    }

    fn fail(&mut self) {
        self.armed = false;
        self.protocol_failed = true;
    }

    fn snapshot(&self) -> (u64, bool) {
        (self.generation, self.armed)
    }
}

fn media_mode_label(mode: MediaMode) -> &'static str {
    match mode {
        MediaMode::Monitor => "monitor",
        MediaMode::PreparedConsult => "prepared_consult",
        MediaMode::PreparedTalk => "prepared_talk",
        MediaMode::Consult => "consult",
        MediaMode::Talk => "talk",
    }
}

fn microphone_authority_payload(
    binding: &SessionBinding,
    action: MicrophoneAuthorityAction,
    generation: u64,
) -> Vec<u8> {
    format!(
        "{MICROPHONE_AUTHORITY_CHANNEL}|{}|{generation}|{}|{}|{}|{}|{}|{}|{}|{}",
        action.label(),
        binding.rtc_session_id,
        binding.call_id,
        binding.call_epoch,
        binding.owner_epoch,
        binding.device_id,
        media_mode_label(binding.mode),
        binding.lease_id.as_deref().unwrap_or_default(),
        binding.fence,
    )
    .into_bytes()
}

fn parse_microphone_authority_payload(
    data: &[u8],
    binding: &SessionBinding,
) -> Result<MicrophoneAuthorityProof, &'static str> {
    if data.len() > MAX_MICROPHONE_AUTHORITY_BYTES {
        return Err("microphone authority payload is too large");
    }
    let encoded = std::str::from_utf8(data).map_err(|_| "microphone authority is not UTF-8")?;
    let mut fields = encoded.splitn(12, '|');
    let protocol = fields
        .next()
        .ok_or("missing microphone authority protocol")?;
    let action = fields.next().ok_or("missing microphone authority action")?;
    let generation = fields
        .next()
        .ok_or("missing microphone authority generation")?;
    let rtc_session_id = fields.next().ok_or("missing microphone RTC session")?;
    let call_id = fields.next().ok_or("missing microphone call")?;
    let call_epoch = fields
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("invalid microphone call epoch")?;
    let owner_epoch = fields
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("invalid microphone owner epoch")?;
    let device_id = fields.next().ok_or("missing microphone device")?;
    let mode = fields.next().ok_or("missing microphone mode")?;
    let lease_id = fields.next().ok_or("missing microphone lease")?;
    let fence = fields
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or("invalid microphone fence")?;
    if fields.next().is_some()
        || protocol != MICROPHONE_AUTHORITY_CHANNEL
        || rtc_session_id != binding.rtc_session_id
        || call_id != binding.call_id
        || call_epoch != binding.call_epoch
        || owner_epoch != binding.owner_epoch
        || device_id != binding.device_id
        || mode != media_mode_label(binding.mode)
        || lease_id != binding.lease_id.as_deref().unwrap_or_default()
        || fence != binding.fence
    {
        return Err("microphone authority does not match this peer binding");
    }
    let action = match action {
        "arm" => MicrophoneAuthorityAction::Arm,
        "disarm" => MicrophoneAuthorityAction::Disarm,
        _ => return Err("unknown microphone authority action"),
    };
    let generation = generation
        .parse::<u64>()
        .ok()
        .filter(|generation| *generation > 0)
        .ok_or("invalid microphone authority generation")?;
    Ok(MicrophoneAuthorityProof { action, generation })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InboundAudioProgress {
    report_id: String,
    ssrc: u32,
    mid: String,
    packets: u64,
    bytes: u64,
    nonconcealed_samples: u64,
}

fn inbound_audio_progress(stats: &[RtcStats]) -> Option<InboundAudioProgress> {
    let mut reports = stats.iter().filter_map(|stat| {
        let RtcStats::InboundRtp(inbound) = stat else {
            return None;
        };
        (inbound.stream.kind == "audio"
            && !inbound.rtc.id.is_empty()
            && inbound.stream.ssrc != 0
            && !inbound.inbound.mid.is_empty())
        .then(|| InboundAudioProgress {
            report_id: inbound.rtc.id.clone(),
            ssrc: inbound.stream.ssrc,
            mid: inbound.inbound.mid.clone(),
            packets: inbound.received.packets_received,
            bytes: inbound.inbound.bytes_received,
            nonconcealed_samples: inbound
                .inbound
                .total_samples_received
                .saturating_sub(inbound.inbound.concealed_samples),
        })
    });
    let report = reports.next()?;
    reports.next().is_none().then_some(report)
}

#[derive(Debug, Default)]
struct RtpProgressGate {
    authority_generation: u64,
    identity: Option<(String, u32, String)>,
    last: Option<InboundAudioProgress>,
    strict_advances: u8,
    first_strict_at: Option<Instant>,
    ready: bool,
    reproof_pending: bool,
    reproof_deadline: Option<Instant>,
    discontinuities: VecDeque<Instant>,
    live_until: Option<Instant>,
}

impl RtpProgressGate {
    fn begin_authority(&mut self, generation: u64) {
        // Proof state is authority-generation local. Discontinuity history is
        // peer local and deliberately survives disarm/re-arm so cycling the
        // DTLS marker cannot reset the sliding churn budget.
        self.authority_generation = generation;
        self.identity = None;
        self.last = None;
        self.strict_advances = 0;
        self.first_strict_at = None;
        self.ready = false;
        self.reproof_pending = false;
        self.reproof_deadline = None;
        self.live_until = None;
    }

    fn disarm(&mut self, generation: u64) {
        self.begin_authority(generation);
    }

    fn authority_generation(&self) -> u64 {
        self.authority_generation
    }

    fn observe(
        &mut self,
        progress: InboundAudioProgress,
        now: Instant,
    ) -> Result<(), &'static str> {
        let identity = (
            progress.report_id.clone(),
            progress.ssrc,
            progress.mid.clone(),
        );
        let Some(last) = self.last.as_ref() else {
            self.identity = Some(identity);
            self.last = Some(progress);
            return Ok(());
        };
        if self.identity.as_ref() != Some(&identity) {
            return self.rebaseline(identity, progress, now);
        }
        if progress.packets < last.packets
            || progress.bytes < last.bytes
            || progress.nonconcealed_samples < last.nonconcealed_samples
        {
            return self.rebaseline(identity, progress, now);
        }
        if progress.packets > last.packets
            && progress.bytes > last.bytes
            && progress.nonconcealed_samples > last.nonconcealed_samples
        {
            self.last = Some(progress);
            self.strict_advances = self.strict_advances.saturating_add(1);
            let first_strict_at = *self.first_strict_at.get_or_insert(now);
            if self.strict_advances >= 2
                && now.duration_since(first_strict_at) >= MICROPHONE_READY_PROGRESS_FLOOR
            {
                self.ready = true;
                self.reproof_pending = false;
                self.reproof_deadline = None;
            }
            if self.ready {
                self.live_until = now.checked_add(MICROPHONE_PCM_PROGRESS_WINDOW);
            }
        }
        Ok(())
    }

    fn rebaseline(
        &mut self,
        identity: (String, u32, String),
        progress: InboundAudioProgress,
        now: Instant,
    ) -> Result<(), &'static str> {
        if self.reproof_pending {
            return self.fail("microphone RTP proof remained discontinuous");
        }
        while self.discontinuities.front().is_some_and(|seen| {
            now.checked_duration_since(*seen)
                .is_some_and(|age| age >= MICROPHONE_DISCONTINUITY_WINDOW)
        }) {
            self.discontinuities.pop_front();
        }
        if self.discontinuities.len() >= MICROPHONE_DISCONTINUITY_BUDGET {
            return self.fail("microphone RTP discontinuity budget exceeded");
        }
        self.discontinuities.push_back(now);

        // A native stats report can be replaced or restart its cumulative
        // counters while the underlying receiver and decoded PCM remain
        // healthy. Treat one such edge as a new baseline, never as fresh
        // proof. The previously earned PCM window is deliberately left
        // untouched: it may run to its original deadline, but this baseline
        // cannot extend it. A second edge before full re-proof is churn and
        // fails the exact peer above.
        self.identity = Some(identity);
        self.last = Some(progress);
        self.strict_advances = 0;
        self.first_strict_at = None;
        self.ready = false;
        self.reproof_pending = true;
        let Some(deadline) = now.checked_add(MICROPHONE_REPROOF_TIMEOUT) else {
            return self.fail("microphone RTP reproof deadline unavailable");
        };
        self.reproof_deadline = Some(deadline);
        Ok(())
    }

    fn reproof_deadline(&self) -> Option<Instant> {
        if self.reproof_pending {
            self.reproof_deadline
        } else {
            None
        }
    }

    fn fail_if_reproof_expired(&mut self, now: Instant) -> Result<(), &'static str> {
        if self.reproof_pending {
            return match self.reproof_deadline {
                Some(deadline) if now < deadline => Ok(()),
                Some(_) => self.fail("microphone RTP reproof timed out"),
                None => self.fail("microphone RTP reproof deadline unavailable"),
            };
        }
        Ok(())
    }

    fn fail(&mut self, reason: &'static str) -> Result<(), &'static str> {
        self.ready = false;
        self.reproof_pending = false;
        self.reproof_deadline = None;
        self.live_until = None;
        Err(reason)
    }

    fn miss(&mut self) {
        self.live_until = None;
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn allows_pcm(&self, now: Instant) -> bool {
        self.live_until.is_some_and(|deadline| now < deadline)
    }
}

fn reconcile_reproof_wait(
    microphone_rtp: &mut RtpProgressGate,
    authority: &StdMutex<RemoteMicrophoneAuthority>,
    failure_reason: &'static str,
) -> Result<(), &'static str> {
    let (authority_generation, armed) = authority
        .lock()
        .map_err(|_| "microphone authority state poisoned")?
        .snapshot();
    if !armed {
        microphone_rtp.disarm(authority_generation);
        return Ok(());
    }
    if microphone_rtp.authority_generation() != authority_generation {
        microphone_rtp.begin_authority(authority_generation);
        return Ok(());
    }
    microphone_rtp.fail(failure_reason)
}

pub struct DesktopPeer {
    binding: SessionBinding,
    microphone_authority_channel: Arc<StdMutex<Option<DataChannel>>>,
    peer: PeerConnection,
    _factory: PeerConnectionFactory,
    caller_source: NativeAudioSource,
    caller_packetizer: Mutex<PcmPacketizer>,
    route_gate: RouteGate,
    remote_audio_rx: mpsc::Receiver<OwnedAudioFrame>,
    event_rx: mpsc::Receiver<PeerEvent>,
    quarantined_frames: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
}

impl DesktopPeer {
    /// Accept a Companion-created offer. Desktop always supplies the caller
    /// receive track. A monitor offer is enforced as receive-only; consult and
    /// talk offers are bidirectional but decoded microphone frames remain
    /// quarantined until the appropriate local route consumes them.
    pub async fn answer(
        binding: SessionBinding,
        offer: SdpSignal,
        options: PeerOptions,
    ) -> Result<(Self, SdpSignal), MediaError> {
        binding.validate()?;
        options.validate()?;
        offer.validate()?;
        if offer.kind != SdpSignalType::Offer {
            return Err(MediaError::InvalidSignal("Desktop requires an SDP offer"));
        }
        validate_offer_media_policy(&offer.sdp, binding.mode)?;

        let factory = PeerConnectionFactory::default();
        // Desktop uses explicit SCO PCM, never an operating-system microphone
        // or speaker device.
        factory.set_adm_recording_enabled(false);
        factory.set_adm_playout_enabled(false);
        let peer = factory.create_peer_connection(options.rtc_configuration())?;
        let caller_source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            options.sample_rate,
            options.channels,
            0,
        );
        let caller_track = factory.create_audio_track("pstn_in", caller_source.clone());

        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        install_common_callbacks(&peer, event_tx.clone());
        let microphone_authority = Arc::new(StdMutex::new(RemoteMicrophoneAuthority::default()));
        let microphone_authority_channel = Arc::new(StdMutex::new(None));
        let authority_channel_seen = Arc::new(AtomicBool::new(false));
        let authority_binding = binding.clone();
        let authority_state = microphone_authority.clone();
        let authority_slot = microphone_authority_channel.clone();
        let authority_event_tx = event_tx.clone();
        peer.on_data_channel(Some(Box::new(move |channel| {
            if !authority_binding.mode.needs_microphone()
                || channel.label() != MICROPHONE_AUTHORITY_CHANNEL
                || authority_channel_seen.swap(true, Ordering::AcqRel)
            {
                if let Ok(mut authority) = authority_state.lock() {
                    authority.fail();
                }
                channel.close();
                let _ = authority_event_tx.try_send(PeerEvent::ProtocolViolation(
                    "unexpected microphone authority channel",
                ));
                return;
            }
            let message_binding = authority_binding.clone();
            let message_state = authority_state.clone();
            let message_tx = authority_event_tx.clone();
            channel.on_message(Some(Box::new(move |buffer| {
                let proof = buffer
                    .binary
                    .then(|| parse_microphone_authority_payload(buffer.data, &message_binding))
                    .unwrap_or(Err("microphone authority must be binary"));
                let result = proof.and_then(|proof| {
                    message_state
                        .lock()
                        .map_err(|_| "microphone authority state poisoned")?
                        .apply(proof)
                });
                if let Err(reason) = result {
                    if let Ok(mut state) = message_state.lock() {
                        state.fail();
                    }
                    let _ = message_tx.try_send(PeerEvent::ProtocolViolation(reason));
                }
            })));
            let close_state = authority_state.clone();
            let close_tx = authority_event_tx.clone();
            channel.on_state_change(Some(Box::new(move |state| {
                if matches!(state, DataChannelState::Closing | DataChannelState::Closed) {
                    let was_armed = close_state
                        .lock()
                        .map(|mut authority| {
                            let was_armed = authority.armed;
                            authority.fail();
                            was_armed
                        })
                        .unwrap_or(true);
                    let _ = close_tx.try_send(PeerEvent::ProtocolViolation(if was_armed {
                        "microphone authority channel closed while armed"
                    } else {
                        "microphone authority channel closed before arm"
                    }));
                }
            })));
            match authority_slot.lock() {
                Ok(mut slot) => *slot = Some(channel),
                Err(_) => {
                    if let Ok(mut authority) = authority_state.lock() {
                        authority.fail();
                    }
                    channel.close();
                    let _ = authority_event_tx.try_send(PeerEvent::ProtocolViolation(
                        "microphone authority channel state poisoned",
                    ));
                }
            }
        })));
        let (remote_audio_tx, remote_audio_rx) = mpsc::channel(REMOTE_AUDIO_QUEUE_FRAMES);
        let route_gate = RouteGate::default();
        let quarantined_frames = Arc::new(AtomicU64::new(0));
        let remote_track_seen = Arc::new(AtomicBool::new(false));
        let runtime = tokio::runtime::Handle::current();
        let callback_binding = binding.clone();
        let callback_gate = route_gate.clone();
        let callback_authority = microphone_authority.clone();
        let callback_quarantined = quarantined_frames.clone();
        peer.on_track(Some(Box::new(move |event| {
            let receiver = event.receiver.clone();
            let MediaStreamTrack::Audio(track) = event.track else {
                let _ = event_tx.try_send(PeerEvent::ProtocolViolation(
                    "non-audio media track was rejected",
                ));
                return;
            };
            if matches!(
                callback_binding.mode,
                MediaMode::Monitor | MediaMode::PreparedConsult | MediaMode::PreparedTalk
            ) {
                let _ = event_tx.try_send(PeerEvent::ProtocolViolation(
                    "receive-only peer attempted to publish microphone audio",
                ));
                return;
            }
            if remote_track_seen.swap(true, Ordering::AcqRel) {
                let _ = event_tx.try_send(PeerEvent::ProtocolViolation(
                    "more than one remote audio track was rejected",
                ));
                return;
            }
            let _ = event_tx.try_send(PeerEvent::RemoteAudioReady);
            let tx = remote_audio_tx.clone();
            let readiness_tx = event_tx.clone();
            let gate = callback_gate.clone();
            let authority = callback_authority.clone();
            let binding = callback_binding.clone();
            let quarantined = callback_quarantined.clone();
            runtime.spawn(async move {
                let mut microphone_rtp = RtpProgressGate::default();
                let mut microphone_ready_emitted = false;
                let mut next_stats_poll = Instant::now();
                let mut stream = NativeAudioStream::with_options(
                    track,
                    options.sample_rate as i32,
                    options.channels as i32,
                    NativeAudioStreamOptions {
                        queue_size_frames: Some(REMOTE_AUDIO_QUEUE_FRAMES),
                    },
                );
                'media: loop {
                    let next_frame = if let Some(deadline) = microphone_rtp.reproof_deadline() {
                        match tokio::time::timeout_at(
                            tokio::time::Instant::from_std(deadline),
                            stream.next(),
                        )
                        .await
                        {
                            Ok(frame) => frame,
                            Err(_) => {
                                match reconcile_reproof_wait(
                                    &mut microphone_rtp,
                                    &authority,
                                    "microphone RTP reproof timed out",
                                ) {
                                    Ok(()) => {
                                        microphone_ready_emitted = false;
                                        next_stats_poll = Instant::now();
                                        continue 'media;
                                    }
                                    Err(reason) => {
                                        let _ = readiness_tx
                                            .try_send(PeerEvent::ProtocolViolation(reason));
                                        break 'media;
                                    }
                                }
                            }
                        }
                    } else {
                        stream.next().await
                    };
                    let Some(frame) = next_frame else {
                        if microphone_rtp.reproof_deadline().is_some() {
                            if let Err(reason) = reconcile_reproof_wait(
                                &mut microphone_rtp,
                                &authority,
                                "microphone RTP stream ended during reproof",
                            ) {
                                let _ = readiness_tx.try_send(PeerEvent::ProtocolViolation(reason));
                            }
                        }
                        break;
                    };
                    if matches!(binding.mode, MediaMode::Consult | MediaMode::Talk) {
                        let (authority_generation, armed) = match authority.lock() {
                            Ok(authority) => authority.snapshot(),
                            Err(_) => {
                                let _ = readiness_tx.try_send(PeerEvent::ProtocolViolation(
                                    "microphone authority state poisoned",
                                ));
                                break;
                            }
                        };
                        if !armed {
                            if microphone_rtp.authority_generation() != authority_generation {
                                microphone_rtp.disarm(authority_generation);
                                microphone_ready_emitted = false;
                            }
                            quarantined.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if microphone_rtp.authority_generation() != authority_generation {
                            // The first pinned report after the exact post-arm
                            // DTLS marker is only a baseline. Readiness needs
                            // two later strict advances, so queued pre-marker
                            // PCM and cross-stream SCTP/RTP ordering cannot arm.
                            microphone_rtp.begin_authority(authority_generation);
                            microphone_ready_emitted = false;
                            next_stats_poll = Instant::now();
                        }
                        let now = Instant::now();
                        if let Err(reason) = microphone_rtp.fail_if_reproof_expired(now) {
                            let _ = readiness_tx.try_send(PeerEvent::ProtocolViolation(reason));
                            break;
                        }
                        if now >= next_stats_poll {
                            next_stats_poll = now.checked_add(MICROPHONE_STATS_POLL).unwrap_or(now);
                            let stats = if let Some(deadline) = microphone_rtp.reproof_deadline() {
                                match tokio::time::timeout_at(
                                    tokio::time::Instant::from_std(deadline),
                                    receiver.get_stats(),
                                )
                                .await
                                {
                                    Ok(stats) => stats.ok(),
                                    Err(_) => {
                                        match reconcile_reproof_wait(
                                            &mut microphone_rtp,
                                            &authority,
                                            "microphone RTP reproof timed out",
                                        ) {
                                            Ok(()) => {
                                                microphone_ready_emitted = false;
                                                next_stats_poll = Instant::now();
                                                continue 'media;
                                            }
                                            Err(reason) => {
                                                let _ = readiness_tx
                                                    .try_send(PeerEvent::ProtocolViolation(reason));
                                                break 'media;
                                            }
                                        }
                                    }
                                }
                            } else {
                                receiver.get_stats().await.ok()
                            };
                            match stats.and_then(|stats| inbound_audio_progress(&stats)) {
                                Some(progress) => {
                                    if let Err(reason) =
                                        microphone_rtp.observe(progress, Instant::now())
                                    {
                                        let _ = readiness_tx
                                            .try_send(PeerEvent::ProtocolViolation(reason));
                                        break;
                                    }
                                }
                                None => microphone_rtp.miss(),
                            }
                        }
                        let allows_pcm = microphone_rtp.allows_pcm(Instant::now());
                        if microphone_rtp.ready() && allows_pcm && !microphone_ready_emitted {
                            // Retry on later decoded frames if the bounded
                            // event queue is momentarily full; exactly one
                            // successfully delivered readiness event is enough.
                            // The current decoded frame follows two strict
                            // advances of this receiver's pinned RTP identity;
                            // concealed pre-arm playout cannot satisfy it.
                            microphone_ready_emitted = readiness_tx
                                .try_send(PeerEvent::RemoteMicrophoneReady)
                                .is_ok();
                        }
                        if !allows_pcm {
                            quarantined.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                    // Consult audio is delivered only through the consult
                    // consumer. Talk audio is dropped until the Desktop's
                    // independently verified lease/fence gate is open.
                    if binding.mode == MediaMode::Talk && !gate.allows(&binding) {
                        quarantined.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let owned = OwnedAudioFrame {
                        samples: frame.data.into_owned(),
                        sample_rate: frame.sample_rate,
                        channels: frame.num_channels,
                    };
                    if tx.try_send(owned).is_err() {
                        quarantined.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        })));

        let remote = SessionDescription::parse(&offer.sdp, SdpType::Offer)?;
        peer.set_remote_description(remote).await?;
        // `RtpSender::set_track` alone does not update an answerer's
        // transceiver direction. `add_track` follows the WebRTC AddTrack
        // algorithm: it reuses the offered audio transceiver and changes
        // inactive/recvonly into sendonly/sendrecv before answer creation.
        // Without this, ICE/DTLS can connect while the answer remains
        // `a=inactive`, so Companion never receives caller audio.
        peer.add_track(caller_track.into(), &[binding.rtc_session_id.as_str()])?;
        let answer = peer.create_answer(AnswerOptions::default()).await?;
        peer.set_local_description(answer.clone()).await?;
        let answer = SdpSignal {
            kind: SdpSignalType::Answer,
            sdp: answer.to_string(),
        };
        answer.validate()?;
        validate_answer_media_policy(&answer.sdp, binding.mode)?;

        let packetizer = PcmPacketizer::new(
            options.sample_rate,
            options.channels,
            options.max_pcm_buffer_ms,
        )
        .ok_or(MediaError::InvalidBinding("audio packetizer"))?;
        Ok((
            Self {
                binding,
                microphone_authority_channel,
                peer,
                _factory: factory,
                caller_source,
                caller_packetizer: Mutex::new(packetizer),
                route_gate,
                remote_audio_rx,
                event_rx,
                quarantined_frames,
                closed: Arc::new(AtomicBool::new(false)),
            },
            answer,
        ))
    }

    pub fn binding(&self) -> &SessionBinding {
        &self.binding
    }

    pub async fn add_remote_candidate(
        &self,
        candidate: IceCandidateSignal,
    ) -> Result<(), MediaError> {
        candidate.validate()?;
        self.ensure_open()?;
        let candidate = IceCandidate::parse(
            &candidate.sdp_mid,
            candidate.sdp_mline_index,
            &candidate.candidate,
        )?;
        self.peer.add_ice_candidate(candidate).await?;
        Ok(())
    }

    pub async fn push_caller_pcm(&self, samples: &[i16]) -> Result<usize, MediaError> {
        self.ensure_open()?;
        let frames = {
            let mut packetizer = self.caller_packetizer.lock().await;
            packetizer.push(samples);
            let mut frames = Vec::new();
            while let Some(frame) = packetizer.next_frame() {
                frames.push(frame);
            }
            frames
        };
        let count = frames.len();
        for frame in frames {
            self.caller_source
                .capture_frame(&AudioFrame {
                    data: Cow::Borrowed(&frame.samples),
                    sample_rate: frame.sample_rate,
                    num_channels: frame.channels,
                    samples_per_channel: (frame.samples.len() as u32) / frame.channels,
                })
                .await?;
        }
        Ok(count)
    }

    /// Opens the only caller-bound transmit gate. The permit must exactly
    /// match this peer's immutable session/epoch/lease/fence binding.
    pub fn authorize_caller_transmit(&self, permit: RoutePermit) -> Result<(), MediaError> {
        if self.binding.mode != MediaMode::Talk || permit.binding != self.binding {
            return Err(MediaError::UnsafeTransition(
                "route permit does not match the talk peer",
            ));
        }
        self.route_gate.authorize(permit)
    }

    pub fn revoke_caller_transmit(&self) {
        self.route_gate.revoke();
    }

    pub fn renew_caller_transmit(&self, lease_lifetime: Duration) -> Result<(), MediaError> {
        if self.binding.mode != MediaMode::Talk {
            return Err(MediaError::UnsafeTransition(
                "only a talk peer has a caller-bound route",
            ));
        }
        self.route_gate.renew(&self.binding, lease_lifetime)
    }

    pub async fn recv_caller_microphone(&mut self) -> Result<OwnedAudioFrame, MediaError> {
        if self.binding.mode != MediaMode::Talk {
            return Err(MediaError::UnsafeTransition(
                "caller microphone is available only to a talk peer",
            ));
        }
        self.remote_audio_rx.recv().await.ok_or(MediaError::Closed)
    }

    pub async fn recv_consult_microphone(&mut self) -> Result<OwnedAudioFrame, MediaError> {
        if self.binding.mode != MediaMode::Consult {
            return Err(MediaError::UnsafeTransition(
                "consult microphone is available only to a consult peer",
            ));
        }
        self.remote_audio_rx.recv().await.ok_or(MediaError::Closed)
    }

    pub async fn next_event(&mut self) -> Option<PeerEvent> {
        self.event_rx.recv().await
    }

    pub fn quarantined_frames(&self) -> u64 {
        self.quarantined_frames.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.route_gate.revoke();
            self.caller_source.clear_buffer();
            if let Ok(mut channel) = self.microphone_authority_channel.lock() {
                if let Some(channel) = channel.take() {
                    channel.close();
                }
            }
            self.peer.close();
        }
    }

    fn ensure_open(&self) -> Result<(), MediaError> {
        if self.closed.load(Ordering::Acquire) {
            Err(MediaError::Closed)
        } else {
            Ok(())
        }
    }
}

impl Drop for DesktopPeer {
    fn drop(&mut self) {
        self.close();
    }
}

struct PlatformAdmGuard {
    factory: PeerConnectionFactory,
}

impl Drop for PlatformAdmGuard {
    fn drop(&mut self) {
        self.factory.set_adm_recording_enabled(false);
        let _ = self.factory.stop_recording();
        let _ = self.factory.stop_playout();
        self.factory.release_platform_adm();
    }
}

pub struct CompanionPeer {
    binding: SessionBinding,
    microphone_authority_channel: Option<DataChannel>,
    peer: PeerConnection,
    factory: PeerConnectionFactory,
    microphone_track: Option<libwebrtc::audio_track::RtcAudioTrack>,
    microphone_active: Arc<AtomicBool>,
    microphone_generation: Arc<AtomicU64>,
    microphone_authority_generation: Arc<AtomicU64>,
    event_rx: mpsc::Receiver<PeerEvent>,
    closed: Arc<AtomicBool>,
    _platform_audio: PlatformAdmGuard,
}

impl CompanionPeer {
    /// Creates the offer from the native Companion endpoint. A monitor peer
    /// contains no microphone track and keeps ADM recording disabled. Consult
    /// and talk contain a disabled device track that is armed only after a
    /// fresh lease/epoch decision.
    pub async fn offer(
        binding: SessionBinding,
        options: PeerOptions,
    ) -> Result<(Self, SdpSignal), MediaError> {
        binding.validate()?;
        options.validate()?;
        let factory = PeerConnectionFactory::default();
        if !factory.acquire_platform_adm() {
            return Err(MediaError::AudioUnavailable(
                "no operating-system audio device module".into(),
            ));
        }
        factory.set_adm_recording_enabled(false);
        factory.set_adm_playout_enabled(true);
        let platform_audio = PlatformAdmGuard {
            factory: factory.clone(),
        };
        if options
            .recording_device_guid
            .as_deref()
            .is_some_and(|guid| !factory.set_recording_device_by_guid(guid))
        {
            return Err(MediaError::AudioUnavailable(
                "selected microphone is unavailable".into(),
            ));
        }
        if options
            .playout_device_guid
            .as_deref()
            .is_some_and(|guid| !factory.set_playout_device_by_guid(guid))
        {
            return Err(MediaError::AudioUnavailable(
                "selected speaker is unavailable".into(),
            ));
        }
        let peer = match factory.create_peer_connection(options.rtc_configuration()) {
            Ok(peer) => peer,
            Err(error) => return Err(error.into()),
        };
        let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        install_common_callbacks(&peer, event_tx.clone());
        let microphone_authority_channel = if binding.mode.needs_microphone() {
            let channel = peer.create_data_channel(
                MICROPHONE_AUTHORITY_CHANNEL,
                DataChannelInit {
                    protocol: MICROPHONE_AUTHORITY_CHANNEL.into(),
                    ..DataChannelInit::default()
                },
            )?;
            let authority_tx = event_tx.clone();
            channel.on_state_change(Some(Box::new(move |state| {
                if state == DataChannelState::Open {
                    let _ = authority_tx.try_send(PeerEvent::MicrophoneAuthorityReady);
                }
            })));
            Some(channel)
        } else {
            None
        };
        let remote_track_seen = Arc::new(AtomicBool::new(false));
        peer.on_track(Some(Box::new(move |event| {
            if !matches!(event.track, MediaStreamTrack::Audio(_)) {
                let _ = event_tx.try_send(PeerEvent::ProtocolViolation(
                    "non-audio media track was rejected",
                ));
                return;
            }
            if remote_track_seen.swap(true, Ordering::AcqRel) {
                let _ = event_tx.try_send(PeerEvent::ProtocolViolation(
                    "more than one remote audio track was rejected",
                ));
                return;
            }
            // With platform playout enabled libwebrtc sends this track to the
            // selected speaker. The callback is readiness evidence only.
            let _ = event_tx.try_send(PeerEvent::RemoteAudioReady);
        })));

        let microphone_track = if binding.mode.needs_microphone() {
            let track = factory.create_device_audio_track(match binding.mode {
                MediaMode::Consult => "consult_tx",
                MediaMode::Talk => "pstn_out",
                MediaMode::Monitor | MediaMode::PreparedConsult | MediaMode::PreparedTalk => {
                    unreachable!()
                }
            });
            if !track.set_enabled(false) {
                return Err(MediaError::AudioUnavailable(
                    "microphone track could not be fenced before negotiation".into(),
                ));
            }
            peer.add_transceiver(
                track.clone().into(),
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::SendRecv,
                    stream_ids: vec![binding.rtc_session_id.clone()],
                    send_encodings: Vec::new(),
                },
            )?;
            Some(track)
        } else {
            peer.add_transceiver_for_media(
                MediaType::Audio,
                RtpTransceiverInit {
                    direction: RtpTransceiverDirection::RecvOnly,
                    stream_ids: vec![binding.rtc_session_id.clone()],
                    send_encodings: Vec::new(),
                },
            )?;
            None
        };

        let offer = peer
            .create_offer(OfferOptions {
                offer_to_receive_audio: true,
                ..OfferOptions::default()
            })
            .await?;
        peer.set_local_description(offer.clone()).await?;
        let offer = SdpSignal {
            kind: SdpSignalType::Offer,
            sdp: offer.to_string(),
        };
        offer.validate()?;
        validate_offer_media_policy(&offer.sdp, binding.mode)?;

        Ok((
            Self {
                binding,
                microphone_authority_channel,
                peer,
                factory,
                microphone_track,
                microphone_active: Arc::new(AtomicBool::new(false)),
                microphone_generation: Arc::new(AtomicU64::new(0)),
                microphone_authority_generation: Arc::new(AtomicU64::new(0)),
                event_rx,
                closed: Arc::new(AtomicBool::new(false)),
                _platform_audio: platform_audio,
            },
            offer,
        ))
    }

    pub fn binding(&self) -> &SessionBinding {
        &self.binding
    }

    pub fn audio_devices(&self) -> Result<PlatformAudioDevices, MediaError> {
        self.ensure_open()?;
        devices_from_factory(&self.factory)
    }

    /// Reads the exact peer's current microphone and decoded remote-audio
    /// levels from native WebRTC statistics. Values are present only after the
    /// corresponding real sample counters have advanced.
    pub async fn audio_levels(&self) -> Result<CompanionAudioLevels, MediaError> {
        self.ensure_open()?;
        let mut levels = CompanionAudioLevels::default();
        for stat in self.peer.get_stats().await? {
            match stat {
                RtcStats::InboundRtp(stat)
                    if stat.stream.kind == "audio"
                        && stat.received.packets_received > 0
                        && stat.inbound.total_samples_received > 0 =>
                {
                    retain_loudest_level(
                        &mut levels.remote_level_permille,
                        stat.inbound.audio_level,
                    );
                }
                RtcStats::MediaSource(stat)
                    if stat.source.kind == "audio" && stat.audio.total_samples_captured > 0 =>
                {
                    retain_loudest_level(
                        &mut levels.microphone_level_permille,
                        stat.audio.audio_level,
                    );
                }
                _ => {}
            }
        }
        if !self.microphone_active() {
            levels.microphone_level_permille = None;
        }
        Ok(levels)
    }

    /// Returns real cumulative microphone and outbound RTP counters for this
    /// exact peer. Both stats families must exist and the microphone must be
    /// armed; callers use a strict post-arm delta, never mere non-zero history.
    pub async fn microphone_sample_progress(
        &self,
    ) -> Result<Option<MicrophoneSampleProgress>, MediaError> {
        self.ensure_open()?;
        if !self.microphone_active() {
            return Ok(None);
        }
        let mut samples_captured: Option<u64> = None;
        let mut outbound = None;
        for stat in self.peer.get_stats().await? {
            match stat {
                RtcStats::MediaSource(stat) if stat.source.kind == "audio" => {
                    samples_captured = Some(
                        samples_captured
                            .unwrap_or_default()
                            .max(stat.audio.total_samples_captured),
                    );
                }
                RtcStats::OutboundRtp(stat) if stat.stream.kind == "audio" => {
                    let current = (stat.sent.packets_sent, stat.sent.bytes_sent);
                    outbound = Some(outbound.map_or(current, |previous: (u64, u64)| {
                        (previous.0.max(current.0), previous.1.max(current.1))
                    }));
                }
                _ => {}
            }
        }
        Ok(match (samples_captured, outbound) {
            (Some(samples_captured), Some((packets_sent, bytes_sent))) => {
                Some(MicrophoneSampleProgress {
                    samples_captured,
                    packets_sent,
                    bytes_sent,
                })
            }
            _ => None,
        })
    }

    pub async fn accept_answer(&self, answer: SdpSignal) -> Result<(), MediaError> {
        self.ensure_open()?;
        answer.validate()?;
        if answer.kind != SdpSignalType::Answer {
            return Err(MediaError::InvalidSignal(
                "Companion requires an SDP answer",
            ));
        }
        let answer = SessionDescription::parse(&answer.sdp, SdpType::Answer)?;
        self.peer.set_remote_description(answer).await?;
        Ok(())
    }

    pub async fn add_remote_candidate(
        &self,
        candidate: IceCandidateSignal,
    ) -> Result<(), MediaError> {
        candidate.validate()?;
        self.ensure_open()?;
        let candidate = IceCandidate::parse(
            &candidate.sdp_mid,
            candidate.sdp_mline_index,
            &candidate.candidate,
        )?;
        self.peer.add_ice_candidate(candidate).await?;
        Ok(())
    }

    /// Enables the selected microphone only for this exact session binding and
    /// for a bounded lease lifetime. Monitor peers can never call this method.
    /// A watchdog disables recording at expiry even if signalling disappears.
    pub fn arm_microphone(
        &self,
        binding: &SessionBinding,
        lease_lifetime: Duration,
    ) -> Result<(), MediaError> {
        self.ensure_open()?;
        if binding != &self.binding || !binding.mode.needs_microphone() || lease_lifetime.is_zero()
        {
            return Err(MediaError::UnsafeTransition(
                "microphone lease does not match this native peer",
            ));
        }
        if self.peer.connection_state() != PeerConnectionState::Connected {
            return Err(MediaError::UnsafeTransition(
                "microphone cannot open before WebRTC is connected",
            ));
        }
        if !self.microphone_authority_ready() {
            return Err(MediaError::UnsafeTransition(
                "microphone authority channel is not open",
            ));
        }
        let track = self
            .microphone_track
            .as_ref()
            .ok_or(MediaError::UnsafeTransition("peer has no microphone track"))?;
        self.factory.set_adm_recording_enabled(true);
        if !self.factory.recording_is_initialized() && !self.factory.init_recording() {
            self.factory.set_adm_recording_enabled(false);
            return Err(MediaError::AudioUnavailable(
                "microphone initialization failed".into(),
            ));
        }
        if !self.factory.start_recording() {
            self.factory.set_adm_recording_enabled(false);
            return Err(MediaError::AudioUnavailable(
                "microphone recording failed".into(),
            ));
        }
        if !track.set_enabled(true) {
            let _ = self.factory.stop_recording();
            self.factory.set_adm_recording_enabled(false);
            return Err(MediaError::AudioUnavailable(
                "microphone track could not be enabled".into(),
            ));
        }
        if let Err(error) = self.send_microphone_authority(MicrophoneAuthorityAction::Arm) {
            track.set_enabled(false);
            let _ = self.factory.stop_recording();
            self.factory.set_adm_recording_enabled(false);
            self.microphone_active.store(false, Ordering::Release);
            return Err(error);
        }
        self.microphone_active.store(true, Ordering::Release);
        self.schedule_microphone_expiry(lease_lifetime);
        Ok(())
    }

    /// Extends the watchdog for an already-open microphone without stopping
    /// the operating-system capture session or toggling the negotiated track.
    /// A lease renewal changes only expiry authority; restarting the ADM here
    /// creates audible transmit gaps and can repeatedly reset WebRTC capture.
    pub fn renew_microphone_lease(
        &self,
        binding: &SessionBinding,
        lease_lifetime: Duration,
    ) -> Result<(), MediaError> {
        self.ensure_open()?;
        if binding != &self.binding || !binding.mode.needs_microphone() || lease_lifetime.is_zero()
        {
            return Err(MediaError::UnsafeTransition(
                "microphone lease does not match this native peer",
            ));
        }
        if self.peer.connection_state() != PeerConnectionState::Connected {
            return Err(MediaError::UnsafeTransition(
                "microphone lease cannot renew while WebRTC is disconnected",
            ));
        }
        if !self.microphone_active.load(Ordering::Acquire) {
            return Err(MediaError::UnsafeTransition(
                "microphone lease cannot renew before the microphone is armed",
            ));
        }
        self.schedule_microphone_expiry(lease_lifetime);
        Ok(())
    }

    fn schedule_microphone_expiry(&self, lease_lifetime: Duration) {
        let track = self
            .microphone_track
            .as_ref()
            .expect("microphone watchdog requires a microphone track")
            .clone();
        let generation = self
            .microphone_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let active = self.microphone_active.clone();
        let current_generation = self.microphone_generation.clone();
        let authority_generation = self.microphone_authority_generation.clone();
        let authority_channel = self
            .microphone_authority_channel
            .as_ref()
            .expect("microphone watchdog requires an authority channel")
            .clone();
        let binding = self.binding.clone();
        let factory = self.factory.clone();
        tokio::runtime::Handle::current().spawn(async move {
            tokio::time::sleep(lease_lifetime).await;
            if current_generation.load(Ordering::Acquire) == generation {
                let authority_generation = authority_generation
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                let _ = authority_channel.send(
                    &microphone_authority_payload(
                        &binding,
                        MicrophoneAuthorityAction::Disarm,
                        authority_generation,
                    ),
                    true,
                );
                track.set_enabled(false);
                let _ = factory.stop_recording();
                factory.set_adm_recording_enabled(false);
                active.store(false, Ordering::Release);
            }
        });
    }

    pub fn disarm_microphone(&self) {
        self.microphone_generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.send_microphone_authority(MicrophoneAuthorityAction::Disarm);
        if let Some(track) = &self.microphone_track {
            track.set_enabled(false);
        }
        let _ = self.factory.stop_recording();
        self.factory.set_adm_recording_enabled(false);
        self.microphone_active.store(false, Ordering::Release);
    }

    pub fn microphone_active(&self) -> bool {
        self.microphone_active.load(Ordering::Acquire)
    }

    pub fn microphone_authority_ready(&self) -> bool {
        self.microphone_authority_channel
            .as_ref()
            .is_some_and(|channel| channel.state() == DataChannelState::Open)
    }

    pub fn is_connected(&self) -> bool {
        self.peer.connection_state() == PeerConnectionState::Connected
    }

    pub async fn next_event(&mut self) -> Option<PeerEvent> {
        self.event_rx.recv().await
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.disarm_microphone();
            if let Some(channel) = &self.microphone_authority_channel {
                channel.close();
            }
            self.peer.close();
        }
    }

    fn send_microphone_authority(
        &self,
        action: MicrophoneAuthorityAction,
    ) -> Result<(), MediaError> {
        let channel =
            self.microphone_authority_channel
                .as_ref()
                .ok_or(MediaError::UnsafeTransition(
                    "peer has no microphone authority channel",
                ))?;
        if channel.state() != DataChannelState::Open {
            return Err(MediaError::UnsafeTransition(
                "microphone authority channel is not open",
            ));
        }
        let generation = self
            .microphone_authority_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        channel
            .send(
                &microphone_authority_payload(&self.binding, action, generation),
                true,
            )
            .map_err(|error| MediaError::WebRtc(error.to_string()))
    }

    fn ensure_open(&self) -> Result<(), MediaError> {
        if self.closed.load(Ordering::Acquire) {
            Err(MediaError::Closed)
        } else {
            Ok(())
        }
    }
}

fn retain_loudest_level(current: &mut Option<u16>, level: f64) {
    let Some(level) = normalized_audio_level_permille(level) else {
        return;
    };
    *current = Some(current.map_or(level, |existing| existing.max(level)));
}

fn normalized_audio_level_permille(level: f64) -> Option<u16> {
    level
        .is_finite()
        .then(|| (level.clamp(0.0, 1.0) * 1_000.0).round() as u16)
}

impl Drop for CompanionPeer {
    fn drop(&mut self) {
        self.close();
    }
}

fn install_common_callbacks(peer: &PeerConnection, event_tx: mpsc::Sender<PeerEvent>) {
    let connection_tx = event_tx.clone();
    peer.on_connection_state_change(Some(Box::new(move |state| {
        let _ = connection_tx.try_send(PeerEvent::ConnectionState(connection_state_label(state)));
    })));
    let ice_tx = event_tx.clone();
    peer.on_ice_candidate(Some(Box::new(move |candidate| {
        let signal = IceCandidateSignal {
            sdp_mid: candidate.sdp_mid(),
            sdp_mline_index: candidate.sdp_mline_index(),
            candidate: candidate.candidate(),
        };
        if signal.validate().is_ok() {
            let _ = ice_tx.try_send(PeerEvent::LocalIce(signal));
        } else {
            let _ = ice_tx.try_send(PeerEvent::ProtocolViolation(
                "native WebRTC produced an invalid ICE candidate",
            ));
        }
    })));
    peer.on_ice_gathering_state_change(Some(Box::new(move |state| {
        if state == IceGatheringState::Complete {
            let _ = event_tx.try_send(PeerEvent::IceComplete);
        }
    })));
}

fn connection_state_label(state: PeerConnectionState) -> &'static str {
    match state {
        PeerConnectionState::New => "new",
        PeerConnectionState::Connecting => "connecting",
        PeerConnectionState::Connected => "connected",
        PeerConnectionState::Disconnected => "disconnected",
        PeerConnectionState::Failed => "failed",
        PeerConnectionState::Closed => "closed",
    }
}

fn validate_offer_media_policy(sdp: &str, mode: MediaMode) -> Result<(), MediaError> {
    validate_media_policy(sdp, offer_direction_for_mode(mode), mode.needs_microphone())
}

fn validate_answer_media_policy(sdp: &str, mode: MediaMode) -> Result<(), MediaError> {
    let expected = if mode.needs_microphone() {
        "sendrecv"
    } else {
        "sendonly"
    };
    validate_media_policy(sdp, expected, mode.needs_microphone())
}

fn offer_direction_for_mode(mode: MediaMode) -> &'static str {
    if matches!(
        mode,
        MediaMode::Monitor | MediaMode::PreparedConsult | MediaMode::PreparedTalk
    ) {
        "recvonly"
    } else {
        "sendrecv"
    }
}

fn validate_media_policy(
    sdp: &str,
    expected_direction: &str,
    expect_microphone_authority: bool,
) -> Result<(), MediaError> {
    let normalized = sdp.replace("\r\n", "\n");
    let mut audio_sections = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    let mut active_authority_sections = 0_usize;
    for line in normalized.lines() {
        if line.starts_with("m=") {
            if let Some(section) = current.take() {
                audio_sections.push(section);
            }
            if line.starts_with("m=audio ") {
                current = Some(vec![line]);
            } else if line.starts_with("m=application ") {
                let active = line.split_ascii_whitespace().nth(1) != Some("0");
                if !active || !line.contains("DTLS/SCTP") || !line.ends_with(" webrtc-datachannel")
                {
                    return Err(MediaError::InvalidSignal(
                        "invalid microphone authority media section",
                    ));
                }
                active_authority_sections += 1;
            } else if line.split_ascii_whitespace().nth(1) != Some("0") {
                return Err(MediaError::InvalidSignal(
                    "only one active audio media section is permitted",
                ));
            }
        } else if let Some(section) = current.as_mut() {
            section.push(line);
        }
    }
    if let Some(section) = current {
        audio_sections.push(section);
    }
    if audio_sections.len() != 1 {
        return Err(MediaError::InvalidSignal(
            "exactly one audio media section is required",
        ));
    }
    if active_authority_sections != usize::from(expect_microphone_authority) {
        return Err(MediaError::InvalidSignal(
            "microphone authority media section does not match the lease mode",
        ));
    }
    let section = &audio_sections[0];
    let direction = section
        .iter()
        .find_map(|line| match *line {
            "a=sendrecv" => Some("sendrecv"),
            "a=sendonly" => Some("sendonly"),
            "a=recvonly" => Some("recvonly"),
            "a=inactive" => Some("inactive"),
            _ => None,
        })
        .unwrap_or("sendrecv");
    if direction != expected_direction {
        eprintln!(
            "[aokie-media] rejected negotiated audio direction actual={} expected={}",
            direction, expected_direction
        );
        return Err(MediaError::InvalidSignal(
            "media direction does not match the lease mode",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_audio_levels_are_bounded_and_never_invent_non_finite_values() {
        assert_eq!(normalized_audio_level_permille(0.0), Some(0));
        assert_eq!(normalized_audio_level_permille(0.421), Some(421));
        assert_eq!(normalized_audio_level_permille(2.0), Some(1_000));
        assert_eq!(normalized_audio_level_permille(-1.0), Some(0));
        assert_eq!(normalized_audio_level_permille(f64::NAN), None);
        assert_eq!(normalized_audio_level_permille(f64::INFINITY), None);

        let mut loudest = None;
        retain_loudest_level(&mut loudest, 0.2);
        retain_loudest_level(&mut loudest, 0.1);
        retain_loudest_level(&mut loudest, 0.7);
        assert_eq!(loudest, Some(700));
    }

    #[test]
    fn microphone_unmute_proof_requires_fresh_capture_and_rtp_progress() {
        let baseline = MicrophoneSampleProgress {
            samples_captured: 1_000,
            packets_sent: 10,
            bytes_sent: 2_000,
        };
        assert!(MicrophoneSampleProgress {
            samples_captured: 1_160,
            packets_sent: 11,
            bytes_sent: 2_320,
        }
        .strictly_advanced_from(baseline));
        assert!(!MicrophoneSampleProgress {
            samples_captured: 1_160,
            ..baseline
        }
        .strictly_advanced_from(baseline));
        assert!(!MicrophoneSampleProgress {
            packets_sent: 11,
            bytes_sent: 2_320,
            ..baseline
        }
        .strictly_advanced_from(baseline));
    }

    #[test]
    fn receive_only_modes_require_recvonly_and_talk_requires_sendrecv() {
        let recvonly = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n";
        let sendrecv = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n";
        let sendrecv_authority = format!(
            "{sendrecv}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\n"
        );
        assert!(validate_offer_media_policy(recvonly, MediaMode::Monitor).is_ok());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::Monitor).is_err());
        assert!(validate_offer_media_policy(recvonly, MediaMode::PreparedTalk).is_ok());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::PreparedTalk).is_err());
        assert!(validate_offer_media_policy(recvonly, MediaMode::PreparedConsult).is_ok());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::PreparedConsult).is_err());
        assert!(validate_offer_media_policy(&sendrecv_authority, MediaMode::Consult).is_ok());
        assert!(validate_offer_media_policy(recvonly, MediaMode::Consult).is_err());
        assert!(validate_offer_media_policy(&sendrecv_authority, MediaMode::Talk).is_ok());
        assert!(validate_offer_media_policy(recvonly, MediaMode::Talk).is_err());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::Talk).is_err());
        assert!(validate_offer_media_policy(&sendrecv_authority, MediaMode::Monitor).is_err());
    }

    #[test]
    fn desktop_answers_send_caller_audio_for_every_mode() {
        let sendonly = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\n";
        let sendrecv = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n";
        let sendrecv_authority = format!(
            "{sendrecv}m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\n"
        );
        assert!(validate_answer_media_policy(sendonly, MediaMode::Monitor).is_ok());
        assert!(validate_answer_media_policy(sendonly, MediaMode::PreparedTalk).is_ok());
        assert!(validate_answer_media_policy(sendonly, MediaMode::PreparedConsult).is_ok());
        assert!(validate_answer_media_policy(&sendrecv_authority, MediaMode::Consult).is_ok());
        assert!(validate_answer_media_policy(&sendrecv_authority, MediaMode::Talk).is_ok());
        assert!(validate_answer_media_policy(sendrecv, MediaMode::Monitor).is_err());
        assert!(validate_answer_media_policy(sendonly, MediaMode::Talk).is_err());
    }

    #[test]
    fn microphone_authority_media_section_is_exactly_one_active_sctp_channel() {
        let audio = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n";
        let authority = "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\n";
        let valid = format!("{audio}{authority}");
        let duplicate = format!("{audio}{authority}{authority}");
        let inactive = format!(
            "{audio}m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\na=sctp-port:5000\r\n"
        );
        let wrong_protocol =
            format!("{audio}m=application 9 TCP/BAD webrtc-datachannel\r\na=sctp-port:5000\r\n");
        assert!(validate_offer_media_policy(&valid, MediaMode::Talk).is_ok());
        assert!(validate_offer_media_policy(&duplicate, MediaMode::Talk).is_err());
        assert!(validate_offer_media_policy(&inactive, MediaMode::Talk).is_err());
        assert!(validate_offer_media_policy(&wrong_protocol, MediaMode::Talk).is_err());
        assert!(validate_offer_media_policy(&valid, MediaMode::PreparedTalk).is_err());
    }

    fn test_talk_binding() -> SessionBinding {
        SessionBinding {
            rtc_session_id: "rtc_authority".into(),
            call_id: "call_authority".into(),
            call_epoch: 3,
            owner_epoch: 4,
            device_id: "device_authority".into(),
            mode: MediaMode::Talk,
            lease_id: Some("lease_authority".into()),
            fence: 5,
        }
    }

    #[test]
    fn microphone_authority_is_binding_exact_bounded_and_replay_safe() {
        let binding = test_talk_binding();
        let arm = microphone_authority_payload(&binding, MicrophoneAuthorityAction::Arm, 1);
        assert!(arm.len() < MAX_MICROPHONE_AUTHORITY_BYTES);
        assert_eq!(
            parse_microphone_authority_payload(&arm, &binding).unwrap(),
            MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Arm,
                generation: 1,
            }
        );
        let mut wrong = binding.clone();
        wrong.owner_epoch += 1;
        assert!(parse_microphone_authority_payload(&arm, &wrong).is_err());
        assert!(parse_microphone_authority_payload(
            &vec![b'x'; MAX_MICROPHONE_AUTHORITY_BYTES + 1],
            &binding,
        )
        .is_err());

        let mut authority = RemoteMicrophoneAuthority::default();
        authority
            .apply(MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Arm,
                generation: 1,
            })
            .unwrap();
        authority
            .apply(MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Disarm,
                generation: 2,
            })
            .unwrap();
        authority
            .apply(MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Arm,
                generation: 1,
            })
            .unwrap();
        assert_eq!(authority.snapshot(), (2, false));
        assert!(authority
            .apply(MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Arm,
                generation: 2,
            })
            .is_err());
        assert_eq!(authority.snapshot(), (2, false));
        assert!(authority
            .apply(MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Arm,
                generation: 3,
            })
            .is_err());

        // Safety is independent of delivery on the bounded PeerEvent lane:
        // even if ProtocolViolation is dropped because that queue is full, a
        // later syntactically valid higher generation can never re-arm.
        let mut event_lost = RemoteMicrophoneAuthority::default();
        event_lost
            .apply(MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Arm,
                generation: 1,
            })
            .unwrap();
        event_lost.fail();
        assert!(event_lost
            .apply(MicrophoneAuthorityProof {
                action: MicrophoneAuthorityAction::Arm,
                generation: 99,
            })
            .is_err());
        assert_eq!(event_lost.snapshot(), (1, false));
    }

    fn progress(packets: u64, bytes: u64, samples: u64) -> InboundAudioProgress {
        InboundAudioProgress {
            report_id: "inbound_audio".into(),
            ssrc: 42,
            mid: "0".into(),
            packets,
            bytes,
            nonconcealed_samples: samples,
        }
    }

    #[test]
    fn rtp_progress_requires_sustained_post_authority_advances_and_stalls_closed() {
        let start = Instant::now();
        let mut gate = RtpProgressGate::default();
        gate.begin_authority(7);
        gate.observe(progress(10, 100, 160), start).unwrap();
        gate.observe(progress(11, 120, 320), start + Duration::from_millis(50))
            .unwrap();
        gate.observe(progress(12, 140, 480), start + Duration::from_millis(100))
            .unwrap();
        assert!(!gate.ready());
        gate.observe(progress(13, 160, 640), start + Duration::from_millis(150))
            .unwrap();
        assert!(gate.ready());
        assert!(gate.allows_pcm(start + Duration::from_millis(200)));
        assert!(!gate.allows_pcm(start + Duration::from_millis(276)));
        gate.observe(progress(14, 180, 800), start + Duration::from_millis(300))
            .unwrap();
        assert!(gate.allows_pcm(start + Duration::from_millis(350)));
        gate.miss();
        assert!(!gate.allows_pcm(start + Duration::from_millis(351)));
    }

    #[test]
    fn rtp_progress_identity_change_rebaselines_and_reproves() {
        let start = Instant::now();
        let mut gate = RtpProgressGate::default();
        gate.begin_authority(1);
        gate.observe(progress(10, 100, 160), start).unwrap();
        gate.observe(progress(11, 120, 320), start + Duration::from_millis(50))
            .unwrap();
        gate.observe(progress(12, 140, 480), start + Duration::from_millis(100))
            .unwrap();
        gate.observe(progress(13, 160, 640), start + Duration::from_millis(150))
            .unwrap();
        assert!(gate.ready());

        let mut changed = progress(20, 200, 800);
        changed.ssrc = 99;
        gate.observe(changed, start + Duration::from_millis(200))
            .unwrap();
        assert!(!gate.ready());
        assert!(gate.allows_pcm(start + Duration::from_millis(274)));
        assert!(!gate.allows_pcm(start + Duration::from_millis(275)));

        for (elapsed_ms, packets, bytes, samples) in [
            (250, 21, 220, 960),
            (300, 22, 240, 1_120),
            (350, 23, 260, 1_280),
        ] {
            let mut reproved = progress(packets, bytes, samples);
            reproved.ssrc = 99;
            gate.observe(reproved, start + Duration::from_millis(elapsed_ms))
                .unwrap();
        }
        assert!(gate.ready());
        assert!(gate.allows_pcm(start + Duration::from_millis(400)));
    }

    #[test]
    fn rtp_progress_counter_reset_rebaselines_and_reproves() {
        let start = Instant::now();
        let mut gate = RtpProgressGate::default();
        gate.begin_authority(1);
        gate.observe(progress(10, 100, 160), start).unwrap();
        gate.observe(progress(11, 120, 320), start + Duration::from_millis(50))
            .unwrap();
        gate.observe(progress(12, 140, 480), start + Duration::from_millis(100))
            .unwrap();
        gate.observe(progress(13, 160, 640), start + Duration::from_millis(150))
            .unwrap();
        assert!(gate.ready());

        gate.observe(progress(1, 20, 160), start + Duration::from_millis(200))
            .unwrap();
        assert!(!gate.ready());
        assert!(gate.allows_pcm(start + Duration::from_millis(274)));
        assert!(!gate.allows_pcm(start + Duration::from_millis(275)));

        gate.observe(progress(2, 40, 320), start + Duration::from_millis(250))
            .unwrap();
        gate.observe(progress(3, 60, 480), start + Duration::from_millis(300))
            .unwrap();
        gate.observe(progress(4, 80, 640), start + Duration::from_millis(350))
            .unwrap();
        assert!(gate.ready());
        assert!(gate.allows_pcm(start + Duration::from_millis(400)));
    }

    #[test]
    fn rtp_progress_persistent_identity_churn_fails_closed() {
        let start = Instant::now();
        let mut gate = RtpProgressGate::default();
        gate.begin_authority(1);
        gate.observe(progress(10, 100, 160), start).unwrap();
        gate.observe(progress(11, 120, 320), start + Duration::from_millis(50))
            .unwrap();
        gate.observe(progress(12, 140, 480), start + Duration::from_millis(100))
            .unwrap();
        gate.observe(progress(13, 160, 640), start + Duration::from_millis(150))
            .unwrap();

        let mut first_replacement = progress(20, 200, 800);
        first_replacement.ssrc = 99;
        gate.observe(first_replacement, start + Duration::from_millis(200))
            .unwrap();
        assert!(gate.allows_pcm(start + Duration::from_millis(225)));

        let mut second_replacement = progress(30, 300, 960);
        second_replacement.ssrc = 100;
        assert_eq!(
            gate.observe(second_replacement, start + Duration::from_millis(250)),
            Err("microphone RTP proof remained discontinuous")
        );
        assert!(!gate.allows_pcm(start + Duration::from_millis(250)));
    }

    #[test]
    fn rtp_progress_stalled_reproof_times_out_after_old_pcm_proof_expires() {
        let start = Instant::now();
        let mut gate = RtpProgressGate::default();
        gate.begin_authority(1);
        gate.observe(progress(10, 100, 160), start).unwrap();
        gate.observe(progress(11, 120, 320), start + Duration::from_millis(50))
            .unwrap();
        gate.observe(progress(12, 140, 480), start + Duration::from_millis(100))
            .unwrap();
        gate.observe(progress(13, 160, 640), start + Duration::from_millis(150))
            .unwrap();

        let mut replacement = progress(20, 200, 800);
        replacement.ssrc = 99;
        gate.observe(replacement, start + Duration::from_millis(200))
            .unwrap();
        assert!(gate.allows_pcm(start + Duration::from_millis(274)));
        assert!(!gate.allows_pcm(start + Duration::from_millis(275)));

        // Missing stats close PCM immediately but leave the bounded re-proof
        // timer armed so the exact peer cannot remain silently quarantined.
        gate.miss();
        assert!(!gate.allows_pcm(start + Duration::from_millis(300)));
        assert_eq!(
            gate.fail_if_reproof_expired(start + Duration::from_millis(699)),
            Ok(())
        );
        assert_eq!(
            gate.fail_if_reproof_expired(start + Duration::from_millis(700)),
            Err("microphone RTP reproof timed out")
        );
    }

    #[test]
    fn rtp_progress_repeated_successful_reproof_cycles_exhaust_budget() {
        let start = Instant::now();
        let mut gate = RtpProgressGate::default();
        gate.begin_authority(1);
        gate.observe(progress(10, 100, 160), start).unwrap();
        gate.observe(progress(11, 120, 320), start + Duration::from_millis(50))
            .unwrap();
        gate.observe(progress(12, 140, 480), start + Duration::from_millis(100))
            .unwrap();
        gate.observe(progress(13, 160, 640), start + Duration::from_millis(150))
            .unwrap();

        for (cycle, ssrc) in [(0_u64, 99_u32), (1, 100)] {
            let baseline_ms = 200 + cycle * 200;
            let mut replacement = progress(20 + cycle * 10, 200 + cycle * 100, 800);
            replacement.ssrc = ssrc;
            gate.observe(replacement, start + Duration::from_millis(baseline_ms))
                .unwrap();
            for (offset_ms, increment) in [(50, 1_u64), (100, 2), (150, 3)] {
                let mut reproved = progress(
                    20 + cycle * 10 + increment,
                    200 + cycle * 100 + increment * 20,
                    800 + increment * 160,
                );
                reproved.ssrc = ssrc;
                gate.observe(
                    reproved,
                    start + Duration::from_millis(baseline_ms + offset_ms),
                )
                .unwrap();
            }
            assert!(gate.ready());
        }

        let mut third_replacement = progress(50, 500, 1_600);
        third_replacement.ssrc = 101;
        assert_eq!(
            gate.observe(third_replacement, start + Duration::from_millis(600)),
            Err("microphone RTP discontinuity budget exceeded")
        );
        assert!(!gate.allows_pcm(start + Duration::from_millis(600)));
    }

    #[test]
    fn inactive_extra_media_is_tolerated_but_active_video_is_rejected() {
        let inactive_video = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\nm=video 0 UDP/TLS/RTP/SAVPF 96\r\na=inactive\r\n";
        let active_video = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n";
        assert!(validate_offer_media_policy(inactive_video, MediaMode::Monitor).is_ok());
        assert!(validate_offer_media_policy(active_video, MediaMode::Monitor).is_err());
    }

    #[test]
    fn endpoint_guids_are_bounded_printable_values() {
        let mut options = PeerOptions {
            recording_device_guid: Some("{capture-endpoint-guid}".into()),
            playout_device_guid: Some("speaker endpoint".into()),
            ..PeerOptions::default()
        };
        assert!(options.validate().is_ok());
        options.recording_device_guid = Some("bad\nendpoint".into());
        assert!(options.validate().is_err());
        options.recording_device_guid = Some("x".repeat(1_025));
        assert!(options.validate().is_err());
    }
}
