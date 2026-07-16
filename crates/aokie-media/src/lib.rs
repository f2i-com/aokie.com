//! Native media primitives for Aokie Companion.
//!
//! This crate deliberately has no FormLogic, gateway, Bluetooth, JSON-RPC, or
//! Tauri dependency. The gateway carries only authenticated control and
//! WebRTC signalling. Desktop pushes caller-side SCO PCM into a
//! [`DesktopPeer`] and may consume remote microphone PCM only while its local
//! [`RoutePermit`] is current. Companion uses [`CompanionPeer`] as the
//! microphone/speaker endpoint and never talks to the Bluetooth dongle.

mod binding;
mod pcm;
mod peer;
mod signal;

pub use binding::{MediaMode, RouteGate, RoutePermit, SessionBinding};
pub use pcm::{OwnedAudioFrame, PcmPacketizer};
pub use peer::{
    enumerate_platform_audio_devices, CompanionPeer, DesktopPeer, PeerEvent, PeerOptions,
    PlatformAudioDevice, PlatformAudioDevices,
};
pub use signal::{IceCandidateSignal, IceServerConfig, SdpSignal, SdpSignalType};

pub const MEDIA_SAMPLE_RATE_HZ: u32 = 16_000;
pub const MEDIA_CHANNELS: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("invalid media binding: {0}")]
    InvalidBinding(&'static str),
    #[error("invalid WebRTC signal: {0}")]
    InvalidSignal(&'static str),
    #[error("unsafe media transition: {0}")]
    UnsafeTransition(&'static str),
    #[error("native audio is unavailable: {0}")]
    AudioUnavailable(String),
    #[error("WebRTC operation failed: {0}")]
    WebRtc(String),
    #[error("media peer is closed")]
    Closed,
}

impl From<libwebrtc::RtcError> for MediaError {
    fn from(value: libwebrtc::RtcError) -> Self {
        Self::WebRtc(value.to_string())
    }
}

impl From<libwebrtc::session_description::SdpParseError> for MediaError {
    fn from(value: libwebrtc::session_description::SdpParseError) -> Self {
        Self::WebRtc(value.to_string())
    }
}
