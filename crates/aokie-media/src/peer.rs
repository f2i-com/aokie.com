use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::{native::NativeAudioSource, AudioSourceOptions};
use libwebrtc::audio_stream::native::{NativeAudioStream, NativeAudioStreamOptions};
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
use libwebrtc::MediaType;
use tokio::sync::{mpsc, Mutex};

use crate::{
    IceCandidateSignal, IceServerConfig, MediaError, MediaMode, OwnedAudioFrame, PcmPacketizer,
    RouteGate, RoutePermit, SdpSignal, SdpSignalType, SessionBinding, MEDIA_CHANNELS,
    MEDIA_SAMPLE_RATE_HZ,
};

const EVENT_QUEUE_CAPACITY: usize = 64;
const REMOTE_AUDIO_QUEUE_FRAMES: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    LocalIce(IceCandidateSignal),
    IceComplete,
    ConnectionState(&'static str),
    RemoteAudioReady,
    ProtocolViolation(&'static str),
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

pub struct DesktopPeer {
    binding: SessionBinding,
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
        let (remote_audio_tx, remote_audio_rx) = mpsc::channel(REMOTE_AUDIO_QUEUE_FRAMES);
        let route_gate = RouteGate::default();
        let quarantined_frames = Arc::new(AtomicU64::new(0));
        let remote_track_seen = Arc::new(AtomicBool::new(false));
        let runtime = tokio::runtime::Handle::current();
        let callback_binding = binding.clone();
        let callback_gate = route_gate.clone();
        let callback_quarantined = quarantined_frames.clone();
        peer.on_track(Some(Box::new(move |event| {
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
            let gate = callback_gate.clone();
            let binding = callback_binding.clone();
            let quarantined = callback_quarantined.clone();
            runtime.spawn(async move {
                let mut stream = NativeAudioStream::with_options(
                    track,
                    options.sample_rate as i32,
                    options.channels as i32,
                    NativeAudioStreamOptions {
                        queue_size_frames: Some(REMOTE_AUDIO_QUEUE_FRAMES),
                    },
                );
                while let Some(frame) = stream.next().await {
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
        let audio_transceivers = peer
            .transceivers()
            .into_iter()
            .filter(|transceiver| {
                transceiver
                    .receiver()
                    .track()
                    .is_some_and(|track| matches!(track, MediaStreamTrack::Audio(_)))
            })
            .collect::<Vec<_>>();
        if audio_transceivers.len() != 1 {
            peer.close();
            return Err(MediaError::InvalidSignal(
                "offer must negotiate exactly one audio transceiver",
            ));
        }
        audio_transceivers[0]
            .sender()
            .set_track(Some(caller_track.into()))?;
        let answer = peer.create_answer(AnswerOptions::default()).await?;
        peer.set_local_description(answer.clone()).await?;
        let answer = SdpSignal {
            kind: SdpSignalType::Answer,
            sdp: answer.to_string(),
        };
        answer.validate()?;

        let packetizer = PcmPacketizer::new(
            options.sample_rate,
            options.channels,
            options.max_pcm_buffer_ms,
        )
        .ok_or(MediaError::InvalidBinding("audio packetizer"))?;
        Ok((
            Self {
                binding,
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
    peer: PeerConnection,
    factory: PeerConnectionFactory,
    microphone_track: Option<libwebrtc::audio_track::RtcAudioTrack>,
    microphone_active: Arc<AtomicBool>,
    microphone_generation: Arc<AtomicU64>,
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
            track.set_enabled(false);
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
                peer,
                factory,
                microphone_track,
                microphone_active: Arc::new(AtomicBool::new(false)),
                microphone_generation: Arc::new(AtomicU64::new(0)),
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
        self.microphone_active.store(true, Ordering::Release);
        let generation = self
            .microphone_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let active = self.microphone_active.clone();
        let current_generation = self.microphone_generation.clone();
        let factory = self.factory.clone();
        let track = track.clone();
        tokio::runtime::Handle::current().spawn(async move {
            tokio::time::sleep(lease_lifetime).await;
            if current_generation.load(Ordering::Acquire) == generation {
                track.set_enabled(false);
                let _ = factory.stop_recording();
                factory.set_adm_recording_enabled(false);
                active.store(false, Ordering::Release);
            }
        });
        Ok(())
    }

    pub fn disarm_microphone(&self) {
        self.microphone_generation.fetch_add(1, Ordering::AcqRel);
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

    pub async fn next_event(&mut self) -> Option<PeerEvent> {
        self.event_rx.recv().await
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.disarm_microphone();
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
    let normalized = sdp.replace("\r\n", "\n");
    let mut audio_sections = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in normalized.lines() {
        if line.starts_with("m=") {
            if let Some(section) = current.take() {
                audio_sections.push(section);
            }
            if line.starts_with("m=audio ") {
                current = Some(vec![line]);
            } else if !line.contains(" 0 ") && !line.ends_with(" 0") {
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
    let expected = if matches!(
        mode,
        MediaMode::Monitor | MediaMode::PreparedConsult | MediaMode::PreparedTalk
    ) {
        "recvonly"
    } else {
        "sendrecv"
    };
    if direction != expected {
        return Err(MediaError::InvalidSignal(
            "offer media direction does not match the lease mode",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_only_modes_require_recvonly_and_talk_requires_sendrecv() {
        let recvonly = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\n";
        let sendrecv = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendrecv\r\n";
        assert!(validate_offer_media_policy(recvonly, MediaMode::Monitor).is_ok());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::Monitor).is_err());
        assert!(validate_offer_media_policy(recvonly, MediaMode::PreparedTalk).is_ok());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::PreparedTalk).is_err());
        assert!(validate_offer_media_policy(recvonly, MediaMode::PreparedConsult).is_ok());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::PreparedConsult).is_err());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::Consult).is_ok());
        assert!(validate_offer_media_policy(recvonly, MediaMode::Consult).is_err());
        assert!(validate_offer_media_policy(sendrecv, MediaMode::Talk).is_ok());
        assert!(validate_offer_media_policy(recvonly, MediaMode::Talk).is_err());
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
