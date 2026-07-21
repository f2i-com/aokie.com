//! Authenticated v2 Companion signalling connection.
//!
//! The implementation lives in this plugin so media control remains next to
//! the physical radio truth.  Only SDP/ICE and epoch-bound lease transitions
//! cross the WebSocket; PCM remains inside native WebRTC tracks.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aokie_media::{
    IceCandidateSignal, IceServerConfig, MediaMode, SdpSignal, SdpSignalType, SessionBinding,
};
use aokie_protocol::v2::{
    peer_roster_hash, sdp_dtls_fingerprint, sdp_sha256, tracks_for, AdmissionRole,
    AudioLevelSource, AuthoritativeCallSnapshot, CallerProjection, Caption, CarrierHoldEvidence,
    EndCallerChallengeFrame, EndCallerOutcome, EndpointBindingClaims, EndpointChallengeFrame,
    EndpointKeyAlgorithm, EndpointPublicKey, Grant, HelloProofClaims, LeaseClaims,
    LeaseHeartbeatFrame, LeaseMode, LeasePhase, LeaseRequestFrame, LeaseRevokeFrame, MediaState,
    MobileAssistanceAnswerFrame, MobileEndCallerChallengeRequestFrame, MobileEndCallerConfirmFrame,
    MobileHello, MobileMicrophoneMuteFrame, MobileOfferAnswerFrame, MobileOfferSurface,
    MobileRtcSignalFrame, NormalizedAudioLevel, ParticipantMode, ParticipantPresence,
    ParticipantState, PendingMobileOfferClaims, PluginAssistanceAnswerFrame,
    PluginAssistanceRequestFrame, PluginClaimDecisionFrame, PluginClaimRejectedFrame,
    PluginEndCallerExecuteFrame, PluginEndCallerResultFrame, PluginHello, PluginIdleFrame,
    PluginLeaseRevokeFrame, PluginLeaseStatus, PluginLeaseStatusFrame, PluginMicrophoneMuteFrame,
    PluginMicrophoneMuteStatusFrame, PluginOfferAcceptedFrame, PluginRtcSignalFrame,
    PluginSnapshotFrame, RemoteCapabilities, RemoteConsentPolicy, RtcSignal,
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
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, http::HeaderValue, Message};
use url::Url;

use crate::event_bridge::{Sink, StdoutSink};
use crate::host_rpc::HostRpc;
use crate::radio::{
    CompanionEndCallerFailure, CompanionEndCallerRequest, RadioControl, RadioHandle,
};
use crate::remote_media::{
    OpenPeerRequest, RemoteAudioLevelSource, RemoteMediaEvent, RemoteMediaEventKind,
    RemoteMediaHandle, RemoteParticipantState, ServiceMode as LocalServiceMode,
};

mod constants;
mod bootstrap;
mod handle;
mod admission;
mod errors;
mod helpers;
mod relay_state;
mod session;
mod transport;
mod session_run;
mod wire;
mod session_messages;
mod session_notices;
mod session_relay_frames;
mod session_relay_authority;
mod end_caller;
mod session_leases;

// Re-exports: every former `crate::companion_gateway::X` path keeps working.
#[allow(unused_imports)]
pub use self::constants::*;
#[allow(unused_imports)]
pub use self::bootstrap::*;
#[allow(unused_imports)]
pub use self::handle::*;
#[allow(unused_imports)]
pub use self::admission::*;
#[allow(unused_imports)]
pub use self::errors::*;
#[allow(unused_imports)]
pub use self::helpers::*;
#[allow(unused_imports)]
pub use self::relay_state::*;
#[allow(unused_imports)]
pub use self::session::*;
#[allow(unused_imports)]
pub use self::transport::*;
#[allow(unused_imports)]
pub use self::session_run::*;
#[allow(unused_imports)]
pub use self::wire::*;
#[allow(unused_imports)]
pub use self::session_messages::*;
#[allow(unused_imports)]
pub use self::session_notices::*;
#[allow(unused_imports)]
pub use self::session_relay_frames::*;
#[allow(unused_imports)]
pub use self::session_relay_authority::*;
#[allow(unused_imports)]
pub use self::end_caller::*;
#[allow(unused_imports)]
pub use self::session_leases::*;


#[cfg(test)]
mod tests;
