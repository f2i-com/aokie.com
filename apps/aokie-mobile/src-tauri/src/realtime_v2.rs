//! Native Companion protocol-v2 transport adapter.
//!
//! Lease tokens never enter the WebView.  The adapter binds every SDP/ICE
//! frame to the current, short-lived lease and drives `aokie-media` directly.

use std::collections::{HashSet, VecDeque};
use std::future::{pending, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aokie_media::{
    IceCandidateSignal, IceServerConfig, MediaMode, SdpSignal, SdpSignalType, SessionBinding,
};
use aokie_protocol::v2::{
    sdp_dtls_fingerprint, sdp_sha256, AdmissionRole, AssistanceResponseAction, AudioLevelSource,
    EndCallerChallengeFrame, EndCallerOutcome, EndpointBindingClaims, EndpointChallengeFrame,
    Grant, HelloProofClaims, LeaseClaims, LeaseHeartbeatFrame, LeaseMode, LeasePhase,
    LeaseRequestFrame, LeaseRevokeFrame, MediaState, MobileAssistanceAnswerFrame,
    MobileEndCallerChallengeRequestFrame, MobileEndCallerConfirmFrame, MobileHello,
    MobileIdleSyncFrame, MobileMicrophoneMuteFrame, MobileOfferAnswerFrame, MobileOfferSurface,
    MobileRtcSignalFrame, MobileSnapshotFrame, PluginAssistanceRequestFrame,
    PluginClaimRejectedFrame, PluginEndCallerResultFrame, PluginHello, PluginIdleFrame,
    PluginLeaseRevokeFrame, PluginLeaseStatus, PluginLeaseStatusFrame,
    PluginMicrophoneMuteStatusFrame, PluginOfferAcceptedFrame, PluginRtcSignalFrame,
    PluginSnapshotFrame, ProjectedCallSnapshot, RemoteConsentPolicy, RtcSignal, ServiceMode,
    SignedPendingMobileOffer, TelephonyState, TrickleCandidateClaims, V2ProtocolError,
    MAX_LEASE_TOKEN_BYTES, MAX_SAFE_INTEGER, SCHEMA_VERSION,
};
use chrono::{DateTime, Utc};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, State};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::{HeaderValue, StatusCode},
        protocol::WebSocketConfig,
        Error as WebSocketError, Message,
    },
    MaybeTlsStream, WebSocketStream,
};
use url::Url;

use crate::managed_auth::{ManagedAdmission, ManagedAdmissionError, ManagedAuthState};
use crate::media::{
    self, AcceptAnswerRequest, AddIceRequest, CreateOfferRequest, LocalSignal, MediaSession,
    MediaSignalEvent, NativeMediaState, RevokeRequest, SessionRequest,
};
use crate::peer_trust::PeerTrustState;
use crate::realtime::{
    activate_session, emit_transport, enqueue_encoded, invalidate_session, session_is_current,
    ConnectionSlot, OutboundCommand, RealtimeConfig, RealtimeState, TransportEvent,
};

const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_SYNC_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const INBOUND_FRESHNESS: Duration = Duration::from_secs(45);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(7);
// A lease currently lasts 20 seconds. Renew only once it has entered this
// window so a session-wide heartbeat tick cannot land in the same clock
// second as a freshly granted/activated lease and produce an expiry that does
// not advance. Two heartbeat intervals leave a full retry opportunity before
// expiry without churning authority immediately after a claim transition.
const LEASE_RENEWAL_WINDOW: Duration = Duration::from_secs(HEARTBEAT_INTERVAL.as_secs() * 2);
const MAX_RECONNECT_DELAY: u64 = 20;
// The live signer issues 90-second admissions. Starting a full worst-case
// OAuth/401 recovery path 83 seconds early would therefore re-admit/re-greet
// every seven seconds forever. Normal overlap budgets one admission request,
// socket connect, challenge, the bounded hello, and retry headroom. Lease
// continuity is independent of the remaining sync time: the predecessor keeps
// renewing until the hello fence, then the replacement applies its exact
// renewal immediately while authoritative state is still arriving. An unusual
// OAuth recovery that exceeds this overlap fails safe and reconnects rather
// than turning every healthy session into continuous rotation traffic.
const MANAGED_ADMISSION_PRIMARY_HTTP_BUDGET: Duration = Duration::from_secs(10);
const MANAGED_HANDOFF_SEND_TIMEOUT: Duration = Duration::from_secs(3);
const MANAGED_HANDOFF_RETRY_HEADROOM: Duration = Duration::from_secs(12);
const ADMISSION_REFRESH_MARGIN: Duration = Duration::from_secs(
    MANAGED_ADMISSION_PRIMARY_HTTP_BUDGET.as_secs()
        + CONNECT_TIMEOUT.as_secs()
        + CONNECT_TIMEOUT.as_secs()
        + MANAGED_HANDOFF_SEND_TIMEOUT.as_secs()
        + MANAGED_HANDOFF_RETRY_HEADROOM.as_secs(),
);
// The old socket becomes unwritable as soon as the gateway accepts the
// replacement hello. Never begin that bounded hello + heartbeat pair with a
// token that cannot remain valid through both writes. A due renewal continues
// on the replacement while it waits for authoritative sync.
const MANAGED_HANDOFF_MIN_LEASE_REMAINING: Duration =
    Duration::from_secs(MANAGED_HANDOFF_SEND_TIMEOUT.as_secs() * 2 + 2);
const NATIVE_ACTION_POLL: Duration = Duration::from_millis(200);
const URGENT_CONTROL_POLL: Duration = Duration::from_millis(100);
const NATIVE_ACTION_TIMEOUT: Duration = Duration::from_secs(5);
// Core-Telecom's explicit user hang-up must outlive the plugin's bounded
// physical radio termination (8-10 s), while still completing before
// Android's 15-second native-action waiter gives up.
const NATIVE_HANGUP_TIMEOUT: Duration = Duration::from_secs(12);
const LEASE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const REVOKE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(4);
const MICROPHONE_MUTE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(8);
// UI confirmation times out quickly, but relay/status delivery can lag across
// the full 20-second plugin lease. Retain only the exact locally-closed lineage
// long enough to contain and return a late renewal/RTC frame without reviving
// native media or failing the authenticated carrier.
const LOCAL_EXPIRY_TOMBSTONE_TTL: Duration = Duration::from_secs(30);
const MAX_LOCAL_EXPIRY_TOMBSTONES: usize = 16;
const MAX_ANSWERED_ASSISTANCE_REQUESTS: usize = 64;
const MAX_COMPLETED_REVOKES: usize = 256;
// A published offer is a one-shot invitation. Once this process has built an
// answer for it, a later claim failure may clear `pending` but must not make
// the cached snapshot answerable again. Keep only the immutable offer/JTI
// identity; the prebuilt outbound frame continues to own exact at-least-once
// delivery of the original attempt.
const MAX_SPENT_MOBILE_OFFERS: usize = 64;
// A locally timed-out offer answer can still receive its independently
// delivered Desktop acknowledgement. Keep only its non-secret exact fence,
// across call and transport resets, so stale authority is terminally
// contained even after the call-scoped spent-offer cache has moved on.
const MAX_TERMINAL_OFFER_ACCEPTANCES: usize = 64;
// Native media activation failures must surrender the authority the Desktop
// already minted, even when the receive loop is about to reconnect. Keep the
// exact-token revoke in a tiny, bounded queue that survives transport resets.
// There can be only one local lease transition in flight; the extra capacity
// is defensive headroom for repeated transport failures, not a work queue.
const MAX_URGENT_CONTROL_FRAMES: usize = 16;
// Relay delivery is at-least-once. Retain only an identifier and digest for
// recently APPLIED authority frames: enough to make exact redelivery a no-op
// without keeping lease tokens, SDP, or ICE payloads alive in memory.
const MAX_APPLIED_RELAY_FRAMES: usize = 256;

type V2WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type V2Writer = SplitSink<V2WebSocket, Message>;
type V2Reader = SplitStream<V2WebSocket>;

/// How long one receive tick waits before handing control back to the session
/// loop. Only a cadence: nothing is lost when it elapses.
const TRANSPORT_READ_TICK: Duration = Duration::from_millis(200);

/// Emergency kill switch for relay writes. Sending is ON after a normal app
/// launch and only an exact `0` disables it.
///
/// Safety no longer depends on a positive deployment flag. `mobile_hello` is
/// harmless capability discovery, while every lease action is reachable only
/// after the Desktop has published a signed, device/mode/surface-bound offer.
/// The exact outbound allowlist below remains the protocol boundary, so an old
/// plugin that publishes no offer can receive a hello but can never be sent a
/// claim. The switch remains solely as an operator rollback lever.
const RELAY_SEND_ENV: &str = "AOKIE_COMPANION_RELAY_SEND_HELLO";

/// Mobile-dialect kinds the plugin's RELAY dispatcher actions directly.
///
/// This is also the outbound safety boundary for [`V2Transport::send_text`]:
/// a newly-added mobile frame does not reach a live Desktop until the plugin
/// has an explicit relay arm for it. The socket gateway remains unaffected and
/// still receives every frame.
const RELAY_PLUGIN_ADMITTED_KINDS: [&str; 10] = [
    "mobile_hello",
    "mobile_offer_answer",
    "lease_request",
    "rtc_signal",
    "lease_heartbeat",
    "lease_revoke",
    "microphone_mute",
    "assistance_answer",
    "end_caller_challenge_request",
    "end_caller_confirm",
];

fn relay_send_enabled() -> bool {
    relay_send_enabled_from(std::env::var(RELAY_SEND_ENV).ok().as_deref())
}

/// Exactly `"0"` disables relay writes; absence is the normal relaunch path.
/// Split out so the rule can be locked without a test mutating process-wide
/// environment under a parallel runner.
fn relay_send_enabled_from(value: Option<&str>) -> bool {
    value != Some("0")
}

fn relay_plugin_admits(kind: &str) -> bool {
    RELAY_PLUGIN_ADMITTED_KINDS.contains(&kind)
}

/// One session, two possible carriers.
///
/// Every protocol decision already happens on text in and text out, so the
/// session is transport-blind: the WebSocket gateway remains the default and
/// the FormLogic-hosted relay is selected only when an admission advertises one.
///
/// Exactly one of these exists per session, so the size gap between the
/// carriers buys nothing worth boxing the socket the live path runs on.
#[allow(clippy::large_enum_variant)]
enum V2Transport {
    WebSocket { writer: V2Writer, reader: V2Reader },
    Relay(RelayCarrier),
}

/// The relay channel plus everything the absent gateway used to supply.
struct RelayCarrier {
    channel: crate::companion_relay::RelayChannel,
    /// Fetched over HTTP while connecting, and consumed once by the handshake.
    /// Over the socket the challenge is the server's first frame; over the
    /// relay it is a document on an authenticated route, so the carrier holds
    /// it until the handshake asks.
    challenge: Option<EndpointChallengeFrame>,
    shim: GatewayShim,
}

/// What one receive tick produced for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum V2Inbound {
    /// Nothing happened this tick.
    Idle,
    /// The carrier proved liveness without producing a protocol frame: a
    /// WebSocket ping, or relay stream bytes that carried no session frame.
    /// Distinct from [`Self::Idle`] because it feeds the inbound freshness
    /// timer.
    Alive,
    Text(String),
    Pong,
    /// The peer ended the session in an expected way; logged, never surfaced as
    /// an error banner.
    Closed(String),
    /// The carrier failed in a way the operator should see.
    Failed(String),
}

impl V2Transport {
    fn websocket(socket: V2WebSocket) -> Self {
        let (writer, reader) = socket.split();
        Self::WebSocket { writer, reader }
    }

    /// The WebSocket keeps itself warm with ping/pong; the relay does not have a
    /// connection to keep warm, because the server sends SSE heartbeat comments
    /// and the reader tracks its own read idleness.
    fn uses_websocket_heartbeat(&self) -> bool {
        matches!(self, Self::WebSocket { .. })
    }

    /// Deliver one frame.
    ///
    /// Over the socket every frame goes out unchanged. Over the relay a frame is
    /// posted when the emergency kill switch is not set and its kind is in
    /// [`RELAY_PLUGIN_ADMITTED_KINDS`]. The plugin actions that lease/RTC,
    /// assistance, and two-step caller-ending family in the mobile dialect.
    ///
    /// A withheld frame reports success so the session survives: the caller's
    /// contract is "false breaks the session", and dropping the working READ
    /// path because a user tapped an action the peer cannot accept would be a
    /// worse outcome than the action quietly not happening. The cost is that a
    /// queued user action resolves as delivered. That is limited to an explicit
    /// operator kill or a future, not-yet-admitted action; neither may
    /// destabilise a Desktop that is handling a real call.
    async fn send_text(&mut self, encoded: String) -> bool {
        match self {
            Self::WebSocket { writer, .. } => send_text(writer, encoded).await,
            Self::Relay(relay) => {
                let kind = parse_kind(&encoded).ok();
                if !relay_send_enabled() {
                    let kind = kind.as_deref().unwrap_or("unparsed");
                    eprintln!(
                        "[AokieCompanion][relay] holding outbound {kind}: relay sending was disabled with {RELAY_SEND_ENV}=0"
                    );
                    return true;
                }
                let Some(kind) = kind else {
                    eprintln!(
                        "[AokieCompanion][relay] holding outbound unparsed frame: only admitted mobile frame kinds may reach the plugin"
                    );
                    return true;
                };
                if !relay_plugin_admits(&kind) {
                    eprintln!(
                        "[AokieCompanion][relay] holding outbound {kind}: the plugin's relay dispatcher does not admit this mobile frame kind"
                    );
                    return true;
                }
                relay.channel.send_text(&encoded).await
            }
        }
    }

    /// The replacement socket has a much smaller safety budget than ordinary
    /// traffic: once its hello is accepted the predecessor is fenced, so a
    /// stalled write must fail closed before the current 20-second lease can
    /// age out silently. Relay HTTP retains its own request timeout and never
    /// fences the live reader during refresh.
    async fn send_managed_handoff_text(&mut self, encoded: String) -> bool {
        if self.is_relay() {
            return self.send_text(encoded).await;
        }
        let Self::WebSocket { writer, .. } = self else {
            unreachable!("relay handoff returned above")
        };
        if encoded.len() > MAX_MESSAGE_BYTES {
            return false;
        }
        send_message_with_timeout(
            writer,
            Message::Text(encoded.into()),
            MANAGED_HANDOFF_SEND_TIMEOUT,
        )
        .await
    }

    async fn send_ping(&mut self) -> bool {
        match self {
            Self::WebSocket { writer, .. } => {
                send_message(writer, Message::Ping(Default::default())).await
            }
            Self::Relay(_) => true,
        }
    }

    /// The endpoint challenge this session must answer.
    ///
    /// Both carriers hand back the SAME document type, so the validation and
    /// signing in [`endpoint_handshake`] are shared verbatim and a relay session
    /// provably proves the same endpoint identity as a socket one.
    async fn next_challenge(
        &mut self,
        deadline: Instant,
    ) -> Result<EndpointChallengeFrame, String> {
        match self {
            Self::Relay(relay) => relay
                .challenge
                .take()
                .ok_or_else(|| "v2 transport closed before endpoint proof".to_string()),
            Self::WebSocket { .. } => loop {
                if Instant::now() >= deadline {
                    return Err("endpoint proof challenge timed out".into());
                }
                match self.recv(TRANSPORT_READ_TICK).await {
                    V2Inbound::Text(text) => {
                        if parse_kind(&text)? != "endpoint_challenge" {
                            return Err(
                                "v2 server did not begin with an endpoint proof challenge".into()
                            );
                        }
                        return strict_parse::<EndpointChallengeFrame>(&text, "endpoint challenge");
                    }
                    V2Inbound::Idle | V2Inbound::Alive | V2Inbound::Pong => {}
                    V2Inbound::Closed(_) => {
                        return Err("v2 transport closed before endpoint proof".into())
                    }
                    V2Inbound::Failed(message) => return Err(message),
                }
            },
        }
    }

    fn is_relay(&self) -> bool {
        matches!(self, Self::Relay(_))
    }

    fn same_carrier_as(&self, other: &Self) -> bool {
        managed_carrier_continuity(self.is_relay(), other.is_relay())
    }

    async fn recv(&mut self, tick: Duration) -> V2Inbound {
        match self {
            Self::WebSocket { writer, reader } => {
                match tokio::time::timeout(tick, reader.next()).await {
                    Err(_) => V2Inbound::Idle,
                    Ok(Some(Ok(Message::Text(text)))) => V2Inbound::Text(text.as_str().to_string()),
                    Ok(Some(Ok(Message::Ping(bytes)))) => {
                        if send_message(writer, Message::Pong(bytes)).await {
                            V2Inbound::Alive
                        } else {
                            V2Inbound::Failed("protocol-v2 heartbeat reply failed".into())
                        }
                    }
                    Ok(Some(Ok(Message::Pong(_)))) => V2Inbound::Pong,
                    Ok(Some(Ok(Message::Close(frame)))) => {
                        V2Inbound::Closed(format!("gateway closed the active socket: {frame:?}"))
                    }
                    Ok(None) => V2Inbound::Closed("active socket reached EOF".into()),
                    Ok(Some(Err(error))) => {
                        V2Inbound::Closed(format!("active socket read failed: {error}"))
                    }
                    Ok(Some(Ok(_))) => {
                        V2Inbound::Failed("gateway sent a non-text protocol-v2 frame".into())
                    }
                }
            }
            Self::Relay(relay) => match relay.channel.recv(tick).await {
                Ok(crate::companion_relay::RelayReceive::Idle) => V2Inbound::Idle,
                Ok(crate::companion_relay::RelayReceive::Alive) => V2Inbound::Alive,
                Ok(crate::companion_relay::RelayReceive::Frame(frame)) => {
                    match relay.shim.translate(&frame) {
                        Ok(Some(translated)) => V2Inbound::Text(translated),
                        // A plugin frame this build has no mobile equivalent
                        // for is carrier traffic, not a session fault: the
                        // shim is deliberately as tolerant inbound as the
                        // plugin is strict, because erroring here would churn
                        // the session on every frame outside the subset.
                        Ok(None) => V2Inbound::Alive,
                        Err(message) => V2Inbound::Failed(message),
                    }
                }
                Err(message) => V2Inbound::Failed(message),
            },
        }
    }

    /// Install a same-carrier managed refresh. Relay keeps its one ordered SSE
    /// reader/cursor and rotates credentials in place; WebSocket moves to the
    /// already-synchronised replacement whose hello fenced the old socket.
    fn install_managed_replacement(&mut self, replacement: Self) -> Result<(), String> {
        match replacement {
            Self::Relay(mut next) => match self {
                Self::Relay(current) => {
                    current.channel.refresh_credentials_from(&next.channel)?;
                    next.shim.adopt_sequence_from(&current.shim);
                    current.shim = next.shim;
                    Ok(())
                }
                Self::WebSocket { .. } => {
                    Err("managed carrier changed during relay refresh".into())
                }
            },
            Self::WebSocket { writer, reader } => match self {
                Self::WebSocket { .. } => {
                    *self = Self::WebSocket { writer, reader };
                    Ok(())
                }
                Self::Relay(_) => Err("managed carrier changed during socket refresh".into()),
            },
        }
    }

    // No `close`: this session has always ended by dropping the carrier, and
    // the relay has nothing to close either — the mailbox holds no
    // per-connection state and already-posted frames stay readable until their
    // TTL expires. Sending a socket close frame here would be new behaviour on
    // a path a live call depends on.
}

fn managed_carrier_continuity(current_is_relay: bool, replacement_is_relay: bool) -> bool {
    current_is_relay == replacement_is_relay
}

fn predecessor_receive_enabled(predecessor_failed: bool, fence_started: bool) -> bool {
    !predecessor_failed && !fence_started
}

fn predecessor_arm_still_current(fence_started: &AtomicBool) -> bool {
    !fence_started.load(Ordering::Acquire)
}

/// The gateway's translation job, done client-side on the relay path.
///
/// Over the WebSocket a gateway process sat between the two endpoints and
/// TRANSLATED: the plugin speaks `plugin_hello` / `plugin_snapshot` /
/// `plugin_idle`, the Companion understands `snapshot` / `idle_sync`, and the
/// two dialects are otherwise disjoint. The relay is a dumb mailbox, so that
/// translation has to happen here.
///
/// ⚠️ The plugin's frames carry an `eventId`, NOT a sequence, and no grants —
/// the gateway minted both. So this shim is not a renaming pass: it supplies the
/// monotonic `sequence` the session tracks authoritative state on, and the
/// `grants` from the admission the server actually issued. A translation that
/// merely copied fields across would fail `validate_snapshot` on every frame.
///
/// Scope includes authoritative state plus the relay lease lifecycle: offer
/// acceptance, lease status/refusal/revoke, and plugin-originated RTC signals.
/// The opposite direction stays in the mobile dialect and is consumed by the
/// plugin's relay-only dispatcher. In particular, that dispatcher authenticates
/// `MobileRtcSignalFrame::leaseToken`, strips it, and hands the remaining
/// plugin-shaped signal to the unchanged RTC handler. Assistance answers
/// and the two-step caller-ending flow use their strict mobile frames too.
struct GatewayShim {
    app_id: String,
    device_id: String,
    grants: Vec<Grant>,
    /// The Desktop endpoint key this admission pinned and the user confirmed.
    /// A `plugin_hello` that does not prove possession of it is not our peer.
    expected_peer_key_thumbprint: String,
    /// Cleared until the peer proves itself; authoritative state is never
    /// projected from an unauthenticated sender.
    peer_verified: bool,
    sequence: u64,
}

/// A shim begins unverified even when another connection previously proved the
/// same long-term Desktop key. Relay mail can outlive a plugin session, so only
/// this carrier's signed hello (or explicit same-key live rotation adoption)
/// may authorize its queued state and lease frames.
impl GatewayShim {
    fn new(
        app_id: String,
        device_id: String,
        grants: Vec<Grant>,
        expected_peer_key_thumbprint: String,
    ) -> Self {
        // A duplicate grant fails `validate_snapshot`/`validate_idle_sync` on
        // EVERY frame, so a server that ever repeats one must not silently
        // brick the relay path.
        let mut unique = Vec::with_capacity(grants.len());
        for grant in grants {
            if !unique.contains(&grant) {
                unique.push(grant);
            }
        }
        Self {
            app_id,
            device_id,
            grants: unique,
            expected_peer_key_thumbprint,
            // Proof is scoped to this relay session. A plugin restart or a new
            // connection with the same long-term key still has to present a
            // fresh signed hello; only explicit live-carrier rotation below
            // may transfer already-verified state.
            peer_verified: false,
            sequence: 0,
        }
    }

    /// Start minting from the session's CURRENT high-water mark rather than
    /// zero.
    ///
    /// ⚠️ The counter this shim mints is compared against a mark that lives in
    /// [`ClientState`] and SURVIVES an admission rotation. A fresh session
    /// begins with both at zero, but a rotation that CHANGES carrier does not:
    /// a WebSocket session rotating onto the relay would start minting at 1
    /// against a gateway-era mark of N, and `is_new_authoritative_sequence`
    /// discards every one of them — silently, because `handle_gateway_frame`
    /// returns `Ok(())` on a stale sequence rather than erroring. The session
    /// would read Connected while its state never moved again. Seeding keeps
    /// the counter continuous across a carrier change in either direction.
    fn seed_sequence(&mut self, authoritative_sequence: u64) {
        self.sequence = self.sequence.max(authoritative_sequence);
    }

    /// The session's authoritative high-water mark must never go backwards
    /// across an admission rotation, or the replacement's first snapshot is
    /// discarded as stale by `is_new_authoritative_sequence`.
    ///
    /// `peer_verified` carries only during this explicit live rotation and only
    /// while the admission still pins the SAME Desktop endpoint key. A newly
    /// constructed shim never inherits this bit; a re-pinned replacement must
    /// prove its own signed hello before projecting state.
    fn adopt_sequence_from(&mut self, previous: &Self) {
        self.sequence = self.sequence.max(previous.sequence);
        if self.expected_peer_key_thumbprint == previous.expected_peer_key_thumbprint {
            self.peer_verified = self.peer_verified || previous.peer_verified;
        }
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }

    fn projected_grants(&self) -> Vec<Grant> {
        self.grants.clone()
    }

    /// `Ok(None)` means "carrier traffic, nothing for the session".
    ///
    /// Deliberately as tolerant inbound as the plugin is strict: an unknown kind
    /// is dropped with a log rather than erroring, because erroring would churn
    /// the whole session over one frame outside this subset. The single
    /// exception is a `plugin_hello` that fails to prove itself — that is an
    /// impostor or a misconfiguration, and translating its state would be worse
    /// than stopping.
    fn translate(&mut self, encoded: &str) -> Result<Option<String>, String> {
        match parse_kind(encoded)?.as_str() {
            "plugin_hello" => {
                // Relay mail can outlive the short proof carried by a hello.
                // A stale re-greeting is carrier traffic: it must neither
                // authorize an unverified replacement nor tear down an
                // already-verified predecessor (and its live media route).
                // Structural, signature, app and pinned-key failures remain
                // fatal in `accept_peer_hello`.
                let _ = self.accept_peer_hello(encoded)?;
                Ok(None)
            }
            "plugin_snapshot" => {
                let frame: PluginSnapshotFrame = strict_parse(encoded, "plugin snapshot")?;
                let Some(()) = self.peer_gate("plugin_snapshot") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay snapshot is bound to another app".into());
                }
                if frame.device_id.as_deref() != Some(self.device_id.as_str()) {
                    return Ok(None);
                }
                frame.validate().map_err(|error| error.to_string())?;
                self.project(frame)
            }
            "plugin_idle" => {
                let frame: PluginIdleFrame = strict_parse(encoded, "plugin idle")?;
                frame.validate().map_err(|error| error.to_string())?;
                let Some(()) = self.peer_gate("plugin_idle") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay idle frame is bound to another app".into());
                }
                let idle = MobileIdleSyncFrame {
                    kind: "idle_sync".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: frame.app_id,
                    sequence: self.next_sequence(),
                    grants: self.projected_grants(),
                };
                idle.validate().map_err(|error| error.to_string())?;
                serde_json::to_string(&idle)
                    .map(Some)
                    .map_err(|_| "could not encode a translated idle sync".to_string())
            }
            "plugin_offer_accepted" => {
                let frame: PluginOfferAcceptedFrame =
                    strict_parse(encoded, "plugin offer acceptance")?;
                let Some(()) = self.peer_gate("plugin_offer_accepted") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay offer acceptance is bound to another app".into());
                }
                if !self.targets_this_device(&frame.device_id, "plugin_offer_accepted") {
                    return Ok(None);
                }
                frame.validate().map_err(|error| error.to_string())?;
                let accepted = MobileOfferAcceptedFrame {
                    kind: "mobile_offer_accepted".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: frame.app_id,
                    request_id: frame.request_id,
                    offer_id: frame.offer_id,
                    offer_jti: frame.offer_jti,
                    offered_mode: frame.offered_mode,
                    accepted: frame.accepted,
                };
                serde_json::to_string(&accepted)
                    .map(Some)
                    .map_err(|_| "could not encode a translated offer acceptance".to_string())
            }
            "plugin_lease_status" => {
                let frame: PluginLeaseStatusFrame = strict_parse(encoded, "plugin lease status")?;
                let Some(()) = self.peer_gate("plugin_lease_status") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay lease status is bound to another app".into());
                }
                if !self.targets_this_device(&frame.device_id, "plugin_lease_status") {
                    return Ok(None);
                }
                match frame.validate(unix_now()?) {
                    Ok(()) => {}
                    // Relay mail is queued. Expiry is an expected stale-mail
                    // outcome, not a session fault that should reconnect a
                    // healthy live state stream. Structural and authority
                    // errors remain fatal below.
                    Err(V2ProtocolError::Expired) => return Ok(None),
                    Err(error) => return Err(error.to_string()),
                }
                if !grants_permit_lease_mode(&self.grants, frame.lease.mode) {
                    eprintln!(
                        "[AokieCompanion][relay] dropped plugin_lease_status: current admission no longer permits its mode"
                    );
                    return Ok(None);
                }
                let (kind, provisional) = mobile_lease_kind(frame.status);
                let status = LeaseStatusFrame {
                    kind: kind.into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: frame.app_id,
                    request_id: frame.request_id,
                    lease_token: frame.lease_token,
                    lease: frame.lease,
                    provisional,
                };
                serde_json::to_string(&status)
                    .map(Some)
                    .map_err(|_| "could not encode a translated lease status".to_string())
            }
            "plugin_claim_rejected" => {
                let frame: PluginClaimRejectedFrame =
                    strict_parse(encoded, "plugin claim rejection")?;
                let Some(()) = self.peer_gate("plugin_claim_rejected") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay claim rejection is bound to another app".into());
                }
                if !self.targets_this_device(&frame.device_id, "plugin_claim_rejected") {
                    return Ok(None);
                }
                frame.validate().map_err(|error| error.to_string())?;
                let rejected = ClaimRejectedFrame {
                    kind: "claim_rejected".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: frame.app_id,
                    request_id: frame.request_id,
                    code: frame.code,
                    message: frame.message,
                };
                serde_json::to_string(&rejected)
                    .map(Some)
                    .map_err(|_| "could not encode a translated claim rejection".to_string())
            }
            // ⚠️ CALLER SAFETY: this frame ALREADY arrives today and is dropped
            // by the catch-all below. The plugin emits it whenever a media route
            // fails, which is the same moment the caller is handed back to the
            // AI receptionist — so a Companion that cannot read it holds a dead
            // lease and keeps telling the operator they are on a call that has
            // already moved on without them.
            "plugin_lease_revoke" => {
                let frame: PluginLeaseRevokeFrame = strict_parse(encoded, "plugin lease revoke")?;
                let Some(()) = self.peer_gate("plugin_lease_revoke") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay lease revoke is bound to another app".into());
                }
                if !self.targets_this_device(&frame.device_id, "plugin_lease_revoke") {
                    return Ok(None);
                }
                frame.validate().map_err(|error| error.to_string())?;
                let revoked = LeaseRevokedFrame {
                    kind: "lease_revoked".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: frame.app_id,
                    // The plugin revokes against a LEASE, not against a request
                    // this session is waiting on, so there is no request to name
                    // and `apply_revocation` matches on the lease identity.
                    request_id: None,
                    lease_id: frame.lease_id,
                    lease_jti: frame.lease_jti,
                    reason: frame.reason,
                };
                serde_json::to_string(&revoked)
                    .map(Some)
                    .map_err(|_| "could not encode a translated lease revoke".to_string())
            }
            // The one frame that needs no rename: `PluginRtcSignalFrame` and the
            // session's own `GatewayRtcFrame` have identical members, so
            // re-encoding could only introduce drift. Validated here as
            // plugin-authored, then handed on byte-for-byte.
            "rtc_signal" => {
                let frame: PluginRtcSignalFrame = strict_parse(encoded, "plugin RTC signal")?;
                let Some(()) = self.peer_gate("rtc_signal") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay RTC signal is bound to another app".into());
                }
                if !self.targets_this_device(&frame.device_id, "rtc_signal") {
                    return Ok(None);
                }
                frame.validate().map_err(|error| error.to_string())?;
                Ok(Some(encoded.to_owned()))
            }
            // This plugin frame already has the same shape the mobile session
            // consumes. The relay outer envelope addressed it to this device;
            // the inner contract binds app/call/revisions, and current
            // admission (not a queued old snapshot) decides disclosure.
            "assistance_request" => {
                let frame: PluginAssistanceRequestFrame =
                    strict_parse(encoded, "plugin assistance request")?;
                let Some(()) = self.peer_gate("assistance_request") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id {
                    return Err("relay assistance request is bound to another app".into());
                }
                if !self.grants.contains(&Grant::StateRead)
                    || !self.grants.contains(&Grant::AssistanceRead)
                {
                    eprintln!(
                        "[AokieCompanion][relay] dropped assistance_request: current admission does not permit it"
                    );
                    return Ok(None);
                }
                match frame.validate(unix_now()?) {
                    Ok(()) => {}
                    Err(V2ProtocolError::Expired) => return Ok(None),
                    Err(error) => return Err(error.to_string()),
                }
                Ok(Some(encoded.to_owned()))
            }
            "microphone_mute_status" => {
                let frame: PluginMicrophoneMuteStatusFrame =
                    strict_parse(encoded, "plugin microphone mute status")?;
                let Some(()) = self.peer_gate("microphone_mute_status") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id
                    || !self.targets_this_device(&frame.device_id, "microphone_mute_status")
                    || !self.grants.contains(&Grant::StateRead)
                    || !self.grants.contains(&Grant::RtcSignal)
                    || (!self.grants.contains(&Grant::Takeover)
                        && !self.grants.contains(&Grant::Consult))
                {
                    return Ok(None);
                }
                frame.validate().map_err(|error| error.to_string())?;
                Ok(Some(encoded.to_owned()))
            }
            "assistance_answer_accepted" => {
                let frame: AssistanceAnswerAcceptedFrame =
                    strict_parse(encoded, "plugin assistance answer acknowledgement")?;
                let Some(()) = self.peer_gate("assistance_answer_accepted") else {
                    return Ok(None);
                };
                validate_common(
                    &frame.kind,
                    frame.schema_version,
                    Some(&frame.app_id),
                    &self.app_id,
                )?;
                if !frame.accepted
                    || !self.grants.contains(&Grant::StateRead)
                    || !self.grants.contains(&Grant::AssistanceRespond)
                {
                    return Ok(None);
                }
                validate_id(&frame.request_id, "assistance requestId")?;
                validate_id(&frame.answer_id, "assistance answerId")?;
                Ok(Some(encoded.to_owned()))
            }
            "end_caller_challenge" => {
                let frame: EndCallerChallengeFrame =
                    strict_parse(encoded, "plugin caller-ending challenge")?;
                let Some(()) = self.peer_gate("end_caller_challenge") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id
                    || !self.targets_this_device(&frame.device_id, "end_caller_challenge")
                    || !self.grants.contains(&Grant::StateRead)
                    || !self.grants.contains(&Grant::Takeover)
                    || !self.grants.contains(&Grant::EndCaller)
                {
                    return Ok(None);
                }
                match frame.validate(unix_now()?) {
                    Ok(()) => Ok(Some(encoded.to_owned())),
                    Err(V2ProtocolError::Expired) => Ok(None),
                    Err(error) => Err(error.to_string()),
                }
            }
            "end_caller_submitted" => {
                let frame: EndCallerSubmittedFrame =
                    strict_parse(encoded, "plugin caller-ending acknowledgement")?;
                let Some(()) = self.peer_gate("end_caller_submitted") else {
                    return Ok(None);
                };
                validate_common(
                    &frame.kind,
                    frame.schema_version,
                    Some(&frame.app_id),
                    &self.app_id,
                )?;
                if !frame.accepted
                    || !self.grants.contains(&Grant::StateRead)
                    || !self.grants.contains(&Grant::Takeover)
                    || !self.grants.contains(&Grant::EndCaller)
                {
                    return Ok(None);
                }
                validate_id(&frame.request_id, "caller-ending requestId")?;
                validate_id(&frame.operation_id, "caller-ending operationId")?;
                validate_id(&frame.confirmation_id, "caller-ending confirmationId")?;
                Ok(Some(encoded.to_owned()))
            }
            "end_caller_result" => {
                let frame: PluginEndCallerResultFrame =
                    strict_parse(encoded, "plugin caller-ending result")?;
                let Some(()) = self.peer_gate("end_caller_result") else {
                    return Ok(None);
                };
                if frame.app_id != self.app_id
                    || !self.targets_this_device(&frame.device_id, "end_caller_result")
                {
                    return Ok(None);
                }
                frame.validate().map_err(|error| error.to_string())?;
                Ok(Some(encoded.to_owned()))
            }
            other => {
                eprintln!("[AokieCompanion][relay] dropped an untranslated plugin frame: {other}");
                Ok(None)
            }
        }
    }

    /// Authoritative state from an unproven peer is dropped, not refused.
    ///
    /// The plugin greets every party on first contact and re-greets after an
    /// admission rotation, so a session that primed its cursor past a hello
    /// recovers on the next one. Erroring instead would turn a recoverable
    /// ordering gap into a reconnect loop.
    fn peer_gate(&self, kind: &str) -> Option<()> {
        if self.peer_verified {
            return Some(());
        }
        eprintln!(
            "[AokieCompanion][relay] dropped {kind}: the Desktop peer has not proved its endpoint key yet"
        );
        None
    }

    fn targets_this_device(&self, device_id: &str, kind: &str) -> bool {
        if device_id == self.device_id {
            return true;
        }
        eprintln!("[AokieCompanion][relay] dropped {kind}: targeted to another Companion device");
        false
    }

    /// Returns `true` only when this frame freshly verifies the pinned peer.
    /// An expired proof is a non-authorizing no-op because relay delivery can
    /// legitimately lag past the proof window. It deliberately leaves the
    /// current verification bit unchanged: false stays fail-closed, while a
    /// live carrier that already proved this exact pinned key is not de-
    /// authenticated by a redundant stale re-greeting.
    fn accept_peer_hello(&mut self, encoded: &str) -> Result<bool, String> {
        let hello: PluginHello = strict_parse(encoded, "plugin hello")?;
        hello.validate().map_err(|error| error.to_string())?;
        if hello.app_id != self.app_id {
            return Err("relay peer hello is bound to another app".into());
        }
        // The binding that matters: over the socket the gateway vouched for the
        // plugin's identity, so on the relay this is the Companion's own proof
        // that the state it is about to project came from the Desktop its
        // admission pinned and its user confirmed.
        if hello.endpoint_proof.claims.holder_key_thumbprint != self.expected_peer_key_thumbprint {
            return Err("relay peer hello presented an unexpected Desktop endpoint key".into());
        }
        let now = unix_now()?;
        match hello.endpoint_proof.verify(now) {
            Ok(()) => {}
            Err(V2ProtocolError::Expired) if hello.endpoint_proof.claims.expires_at <= now => {
                // `verify(now)` checks the clock before the signature. Re-run
                // the complete proof verification at its original issue time
                // so an expired frame with a forged signature is still fatal;
                // only a once-valid, correctly signed proof is stale traffic.
                hello
                    .endpoint_proof
                    .verify(hello.endpoint_proof.claims.issued_at)
                    .map_err(|error| error.to_string())?;
                eprintln!(
                    "[AokieCompanion][relay] dropped stale plugin_hello without changing peer authority"
                );
                return Ok(false);
            }
            Err(error) => return Err(error.to_string()),
        }
        if !self.peer_verified {
            eprintln!("[AokieCompanion][relay] Desktop peer proved its endpoint key");
        }
        self.peer_verified = true;
        Ok(true)
    }

    fn project(&mut self, frame: PluginSnapshotFrame) -> Result<Option<String>, String> {
        if frame.app_id != self.app_id {
            return Err("relay snapshot is bound to another app".into());
        }
        if frame.device_id.as_deref() != Some(self.device_id.as_str()) {
            eprintln!(
                "[AokieCompanion][relay] dropped plugin_snapshot: missing or targeted to another Companion device"
            );
            return Ok(None);
        }
        let source = frame.snapshot;
        // ⚠️ `Vec<Caption>` and `Option<Vec<Caption>>` are NOT the same
        // statement, so this field cannot simply be wrapped. Authoritative
        // captions are "what was transcribed"; the projected `Option` means
        // "captions are EXPOSED to this device", and `validate_snapshot` refuses
        // a `Some` that consent does not currently permit — rejecting the WHOLE
        // snapshot, not just the captions. So the shim has to make the decision
        // the gateway used to make: expose only when the consent policy allows
        // it and this admission actually holds the grant. Withholding costs a
        // caption; getting it wrong costs every snapshot.
        let consent_current = remote_consent_is_current(&source.remote_consent, unix_now()?);
        let captions_permitted = consent_current
            && source.remote_consent.captions_enabled
            && self.grants.contains(&Grant::CaptionsRead);
        let participants = if consent_current && self.grants.contains(&Grant::ParticipantsRead) {
            source
                .participants
                .into_iter()
                .map(|mut participant| {
                    if !self.grants.contains(&Grant::ParticipantIdentityRead) {
                        participant.subject_id = None;
                        participant.display_label = None;
                    }
                    participant
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let participant_ids = participants
            .iter()
            .map(|participant| participant.participant_id.as_str())
            .collect::<HashSet<_>>();
        let audio_levels = (consent_current && self.grants.contains(&Grant::AudioLevelsRead))
            .then_some(source.audio_levels)
            .flatten()
            .map(|levels| {
                levels
                    .into_iter()
                    .filter(|level| {
                        level.source != AudioLevelSource::Companion
                            || level
                                .participant_id
                                .as_deref()
                                .is_some_and(|participant_id| {
                                    participant_ids.contains(participant_id)
                                })
                    })
                    .collect()
            });
        let projected = ProjectedCallSnapshot {
            call_id: source.call_id,
            call_epoch: source.call_epoch,
            owner_epoch: source.owner_epoch,
            switchboard_revision: source.switchboard_revision,
            remote_revision: source.remote_revision,
            telephony_state: source.telephony_state,
            service_mode: source.service_mode,
            media_state: source.media_state,
            remote_capabilities: source.remote_capabilities,
            secondary_call_policy: source.secondary_call_policy,
            secondary_call: source.secondary_call,
            remote_consent: source.remote_consent,
            caller: self
                .grants
                .contains(&Grant::CallerRead)
                .then_some(source.caller)
                .flatten(),
            captions: captions_permitted.then_some(source.captions),
            participants,
            audio_levels,
            companion_microphone_muted: source.companion_microphone_muted,
            // Offers, by contrast, ARE authored — on this carrier the plugin is
            // the lease authority and signs its own, so passing them through is
            // relaying the Desktop's statement, not manufacturing one.
            //
            // ⚠️ Deliberately NOT verified here. `SignedPendingMobileOffer`
            // treats `offerToken` as opaque, and offer integrity belongs at the
            // minting authority: the plugin refuses a lease request naming an
            // offer it did not issue. So a relay that fabricates one buys a
            // single refused request and nothing else — whereas a check here
            // would need a verification key the Companion has no way to pin.
            // What stops a fabricated offer being ACTED on is `peer_gate`
            // (already passed above) plus `select_mobile_offer`, which re-pins
            // every field against the live snapshot and this device's identity.
            pending_mobile_offers: source
                .pending_mobile_offers
                .into_iter()
                .filter(|offer| {
                    offer.offer.target_device_id == self.device_id
                        && grants_permit_lease_mode(&self.grants, offer.offer.offered_mode)
                        && offer
                            .offer
                            .required_grants
                            .iter()
                            .all(|grant| self.grants.contains(grant))
                })
                .collect(),
            occurred_at: source.occurred_at,
        };
        let snapshot = MobileSnapshotFrame {
            kind: "snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: frame.app_id,
            sequence: self.next_sequence(),
            grants: self.projected_grants(),
            snapshot: projected,
        };
        validate_snapshot(&snapshot, &self.app_id)?;
        serde_json::to_string(&snapshot)
            .map(Some)
            .map_err(|_| "could not encode a translated snapshot".to_string())
    }
}

#[derive(Clone, Default)]
pub(crate) struct V2State {
    inner: Arc<AsyncMutex<ClientState>>,
    ids: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedAdmissionStateEvent<'a> {
    value: &'static str,
    code: &'a str,
    message: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AppliedRelayFrame {
    key: String,
    digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpentMobileOffer {
    app_id: String,
    device_id: String,
    call_id: String,
    call_epoch: u64,
    offer_id: String,
    offer_jti: String,
    offered_mode: LeaseMode,
    // Present only when the offer was spent by a lease answer. This exact
    // terminal identity contains an acceptance that arrives after the local
    // attempt timed out, without promoting it into fresh authority.
    offer_request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalOfferAcceptance {
    app_id: String,
    device_id: String,
    request_id: String,
    offer_id: String,
    offer_jti: String,
    offered_mode: LeaseMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletedRevoke {
    app_id: String,
    request_id: Option<String>,
    lease_id: String,
    lease_jti: String,
    // The gateway acknowledgement (Some requestId) and the plugin's
    // de-escalating media-failure notice (no requestId) are two legitimate
    // wire frames for the same returned lease. Fence each dialect separately
    // so either ordering is replay-safe without accepting changed bytes.
    frame_digest: Option<[u8; 32]>,
    unsolicited_frame_digest: Option<[u8; 32]>,
}

#[derive(Default)]
struct ClientState {
    generation: u64,
    app_id: Option<String>,
    device_id: Option<String>,
    session_nonce: Option<String>,
    endpoint_identity: Option<crate::endpoint_identity::EndpointIdentity>,
    peer_key_thumbprint: Option<String>,
    authoritative_sequence: u64,
    snapshot: Option<MobileSnapshotFrame>,
    spent_mobile_offers: VecDeque<SpentMobileOffer>,
    terminal_offer_acceptances: VecDeque<TerminalOfferAcceptance>,
    pending: Option<PendingLease>,
    lease: Option<ClientLease>,
    pending_microphone_mute: Option<PendingMicrophoneMute>,
    pending_revoke: Option<PendingRevoke>,
    local_expiry_tombstones: VecDeque<LocalExpiryTombstone>,
    pending_native_end: Option<PendingNativeEnd>,
    pending_native_hangup: Option<PendingNativeHangup>,
    ice_servers: Vec<IceServerConfig>,
    relay_only: bool,
    // Carrier capability, not ICE policy. Used for at-least-once replay
    // fencing where relay mail can redeliver an already-applied response.
    relay_transport: bool,
    // Managed admission is fresher authority than a previously projected
    // snapshot during an overlapping transport rotation. Custom WebSocket
    // profiles have no local admission object and continue to use the
    // gateway-projected snapshot grants.
    admission_grants: Option<Vec<Grant>>,
    assistance: Option<PluginAssistanceRequestFrame>,
    pending_assistance_answer: Option<String>,
    // IDs only: keep answered help prompts replay-fenced without retaining the
    // private question, context, or answer across reconnects.
    answered_assistance_requests: VecDeque<(String, String)>,
    // At-least-once relay replay fence. Entries survive a transport reconnect
    // and are bounded; exact content is forgotten in favour of a SHA-256
    // digest so SDP, ICE, and tokens are not retained here.
    applied_relay_frames: VecDeque<AppliedRelayFrame>,
    // A lease status is reserved before native media is touched. This prevents
    // an exact at-least-once duplicate from starting a second peer operation
    // while the first one is outside the client lock.
    applying_lease_status: Option<AppliedRelayFrame>,
    // Exact-token lease returns produced by failed native activation/renewal.
    // These survive a carrier reconnect and are removed only after a transport
    // accepted the exact queued bytes.
    urgent_control_frames: VecDeque<UrgentControlFrame>,
    // Completed lease returns/revocations remain replay-fenced across a
    // transport reconnect. Only bounded identifiers and an optional wire hash
    // survive; no lease token or revoke payload is retained.
    completed_revokes: VecDeque<CompletedRevoke>,
    pending_end_caller: Option<PendingEndCaller>,
    seen_remote_endpoint_jtis: HashSet<String>,
}

fn offer_acceptance_replay_key(request_id: &str) -> String {
    format!("offer-accepted:{request_id}")
}

fn lease_status_replay_key(kind: &str, request_id: &str) -> String {
    // A provisional claim and its activation deliberately share requestId, so
    // the authority transition kind is part of the identity.
    format!("lease-status:{kind}:{request_id}")
}

fn rtc_signal_replay_key(signal_id: &str) -> String {
    format!("rtc-signal:{signal_id}")
}

fn claim_rejection_replay_key(request_id: &str) -> String {
    format!("claim-rejected:{request_id}")
}

fn microphone_mute_status_replay_key(request_id: &str) -> String {
    format!("microphone-mute-status:{request_id}")
}

fn assistance_answer_accepted_replay_key(request_id: &str) -> String {
    format!("assistance-answer-accepted:{request_id}")
}

fn end_caller_challenge_replay_key(request_id: &str) -> String {
    format!("end-caller-challenge:{request_id}")
}

fn end_caller_submitted_replay_key(request_id: &str) -> String {
    format!("end-caller-submitted:{request_id}")
}

fn end_caller_result_replay_key(operation_id: &str) -> String {
    format!("end-caller-result:{operation_id}")
}

fn relay_frame_digest(encoded: &str) -> [u8; 32] {
    Sha256::digest(encoded.as_bytes()).into()
}

/// `true` means the exact frame was already applied and is now a no-op.
/// Reusing an authority identifier with any different wire content fails
/// closed, even when the decoded values might look semantically equivalent.
fn applied_relay_frame_is_replay(
    client: &ClientState,
    key: &str,
    encoded: &str,
) -> Result<bool, String> {
    let digest = relay_frame_digest(encoded);
    let Some(applied) = client
        .applied_relay_frames
        .iter()
        .find(|applied| applied.key == key)
    else {
        return Ok(false);
    };
    if applied.digest == digest {
        Ok(true)
    } else {
        Err("relay authority frame identifier was reused with different content".into())
    }
}

fn remember_applied_relay_frame(
    client: &mut ClientState,
    key: String,
    encoded: &str,
) -> Result<(), String> {
    if applied_relay_frame_is_replay(client, &key, encoded)? {
        return Ok(());
    }
    client.applied_relay_frames.push_back(AppliedRelayFrame {
        key,
        digest: relay_frame_digest(encoded),
    });
    while client.applied_relay_frames.len() > MAX_APPLIED_RELAY_FRAMES {
        client.applied_relay_frames.pop_front();
    }
    Ok(())
}

fn relay_frame_was_applied(client: &ClientState, key: &str) -> bool {
    client
        .applied_relay_frames
        .iter()
        .any(|applied| applied.key == key)
}

fn completed_revoke_collides(current: &CompletedRevoke, candidate: &CompletedRevoke) -> bool {
    current.app_id == candidate.app_id
        && (candidate
            .request_id
            .as_ref()
            .is_some_and(|request_id| current.request_id.as_deref() == Some(request_id.as_str()))
            || (current.lease_id == candidate.lease_id && current.lease_jti == candidate.lease_jti))
}

fn merge_completed_revoke(
    current: &mut CompletedRevoke,
    candidate: &CompletedRevoke,
) -> Result<(), String> {
    if current.app_id != candidate.app_id
        || current.lease_id != candidate.lease_id
        || current.lease_jti != candidate.lease_jti
    {
        return Err(
            "completed lease revoke identity was reused across a request or JTI fence".into(),
        );
    }
    match (&current.request_id, &candidate.request_id) {
        (Some(current), Some(candidate)) if current != candidate => {
            return Err("completed lease revoke identity was reused across two request IDs".into());
        }
        (None, Some(request_id)) => current.request_id = Some(request_id.clone()),
        _ => {}
    }
    for (current_digest, candidate_digest) in [
        (&mut current.frame_digest, candidate.frame_digest),
        (
            &mut current.unsolicited_frame_digest,
            candidate.unsolicited_frame_digest,
        ),
    ] {
        match (*current_digest, candidate_digest) {
            (Some(current), Some(candidate)) if current != candidate => {
                return Err(
                    "completed lease revoke was replayed with different wire content".into(),
                );
            }
            (None, Some(digest)) => *current_digest = Some(digest),
            _ => {}
        }
    }
    Ok(())
}

fn remember_completed_revoke(
    client: &mut ClientState,
    candidate: CompletedRevoke,
) -> Result<(), String> {
    if let Some(index) = client
        .completed_revokes
        .iter()
        .position(|current| completed_revoke_collides(current, &candidate))
    {
        return merge_completed_revoke(
            client
                .completed_revokes
                .get_mut(index)
                .expect("completed revoke index came from this queue"),
            &candidate,
        );
    }
    client.completed_revokes.push_back(candidate);
    while client.completed_revokes.len() > MAX_COMPLETED_REVOKES {
        client.completed_revokes.pop_front();
    }
    Ok(())
}

fn completed_revoke_from_pending(pending: &PendingRevoke) -> CompletedRevoke {
    CompletedRevoke {
        app_id: pending.lease.claims.app_id.clone(),
        request_id: Some(pending.request_id.clone()),
        lease_id: pending.lease_id.clone(),
        lease_jti: pending.lease_jti.clone(),
        frame_digest: None,
        unsolicited_frame_digest: None,
    }
}

fn completed_revoke_from_frame(frame: &LeaseRevokedFrame, encoded: &str) -> CompletedRevoke {
    CompletedRevoke {
        app_id: frame.app_id.clone(),
        request_id: frame.request_id.clone(),
        lease_id: frame.lease_id.clone(),
        lease_jti: frame.lease_jti.clone(),
        frame_digest: frame
            .request_id
            .is_some()
            .then(|| relay_frame_digest(encoded)),
        unsolicited_frame_digest: frame
            .request_id
            .is_none()
            .then(|| relay_frame_digest(encoded)),
    }
}

/// Returns true when this revoke belongs to an already-completed fence. A
/// first late acknowledgement fills the tombstone's digest; subsequent
/// delivery must be byte-identical.
fn completed_revoke_frame_is_replay(
    client: &mut ClientState,
    frame: &LeaseRevokedFrame,
    encoded: &str,
) -> Result<bool, String> {
    let candidate = completed_revoke_from_frame(frame, encoded);
    let Some(index) = client
        .completed_revokes
        .iter()
        .position(|current| completed_revoke_collides(current, &candidate))
    else {
        return Ok(false);
    };
    merge_completed_revoke(
        client
            .completed_revokes
            .get_mut(index)
            .expect("completed revoke index came from this queue"),
        &candidate,
    )?;
    Ok(true)
}

fn is_late_duplicate_offer_rejection(
    client: &ClientState,
    frame: &ClaimRejectedFrame,
) -> Result<bool, String> {
    if !relay_frame_was_applied(client, &offer_acceptance_replay_key(&frame.request_id)) {
        return Ok(false);
    }
    if frame.code == "offer_replayed" {
        Ok(true)
    } else {
        Err("an accepted mobile offer was contradicted by a later rejection".into())
    }
}

fn assistance_was_answered(client: &ClientState, app_id: &str, request_id: &str) -> bool {
    client
        .answered_assistance_requests
        .iter()
        .any(|(answered_app_id, answered_request_id)| {
            answered_app_id == app_id && answered_request_id == request_id
        })
}

fn remember_answered_assistance(client: &mut ClientState, app_id: &str, request_id: &str) {
    if assistance_was_answered(client, app_id, request_id) {
        return;
    }
    client
        .answered_assistance_requests
        .push_back((app_id.to_owned(), request_id.to_owned()));
    while client.answered_assistance_requests.len() > MAX_ANSWERED_ASSISTANCE_REQUESTS {
        client.answered_assistance_requests.pop_front();
    }
}

fn discard_expired_assistance(
    client: &mut ClientState,
    now: u64,
    request_id: Option<&str>,
) -> bool {
    let should_discard = client.assistance.as_ref().is_some_and(|assistance| {
        assistance.expires_at <= now
            && request_id.is_none_or(|request_id| assistance.request_id == request_id)
    });
    if should_discard {
        client.assistance = None;
        client.pending_assistance_answer = None;
    }
    should_discard
}

/// Applies a fully validated, current assistance request. `false` means the
/// request was an identical in-flight duplicate or an already-answered replay.
/// In both cases the caller must not republish it to the renderer.
fn apply_assistance_request(
    client: &mut ClientState,
    frame: &PluginAssistanceRequestFrame,
) -> Result<bool, String> {
    if assistance_was_answered(client, &frame.app_id, &frame.request_id) {
        return Ok(false);
    }
    if let Some(current) = &client.assistance {
        if current.request_id == frame.request_id {
            if current != frame {
                return Err("assistance requestId was reused with different content".into());
            }
            // Preserve a pending answer for an at-least-once retransmission.
            return Ok(false);
        }
    }
    client.assistance = Some(frame.clone());
    client.pending_assistance_answer = None;
    Ok(true)
}

#[derive(Default)]
struct IdleStateCleanup {
    failed_native_action_ids: Vec<String>,
    confirmed_revoke_action_id: Option<String>,
    pending_call: Option<(String, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingLease {
    request_id: String,
    offer_request_id: String,
    mode: LeaseMode,
    rtc_session_id: String,
    call_id: String,
    call_epoch: u64,
    owner_epoch: u64,
    accepted_offer_id: String,
    accepted_offer_jti: String,
    lease_frame: LeaseRequestFrame,
    stage: PendingLeaseStage,
    deadline: Instant,
    native_action_id: Option<String>,
    native_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingLeaseStage {
    AwaitingOfferAcceptance,
    ReadyForLeaseDelivery,
    LeaseRequested,
}

fn pending_lease_timeout(pending: &PendingLease, now: Instant) -> Option<bool> {
    let native_timeout = pending
        .native_deadline
        .is_some_and(|deadline| deadline <= now);
    (native_timeout || pending.deadline <= now).then_some(native_timeout)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClientLease {
    request_id: String,
    token: String,
    claims: LeaseClaims,
    session: MediaSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingMicrophoneMuteStage {
    ArmingNative,
    AwaitingStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingMicrophoneMute {
    request_id: String,
    lease_id: String,
    lease_jti: String,
    rtc_session_id: String,
    call_id: String,
    call_epoch: u64,
    owner_epoch: u64,
    switchboard_revision: u64,
    remote_revision: u64,
    fence: u64,
    muted: bool,
    session: MediaSession,
    stage: PendingMicrophoneMuteStage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SnapshotMicrophoneMuteReconciliation {
    None,
    Confirmed(PendingMicrophoneMute),
    Stale(PendingMicrophoneMute),
}

fn reconcile_microphone_mute_from_snapshot(
    client: &mut ClientState,
    frame: &MobileSnapshotFrame,
) -> SnapshotMicrophoneMuteReconciliation {
    let Some(pending) = client.pending_microphone_mute.as_ref() else {
        return SnapshotMicrophoneMuteReconciliation::None;
    };
    let lease_exact = client.lease.as_ref().is_some_and(|lease| {
        lease.claims.lease_id == pending.lease_id
            && lease.claims.jti == pending.lease_jti
            && lease.claims.rtc_session_id == pending.rtc_session_id
            && lease.claims.call_id == pending.call_id
            && lease.claims.call_epoch == pending.call_epoch
            && lease.claims.owner_epoch == pending.owner_epoch
            && lease.claims.fence == pending.fence
            && lease.session == pending.session
    });
    let snapshot_exact = frame.snapshot.call_id == pending.call_id
        && frame.snapshot.call_epoch == pending.call_epoch
        && frame.snapshot.owner_epoch == pending.owner_epoch
        && frame.snapshot.switchboard_revision == pending.switchboard_revision
        && frame.snapshot.telephony_state == TelephonyState::Active
        && matches!(
            frame.snapshot.service_mode,
            ServiceMode::HumanActive | ServiceMode::ConsultActive
        );
    if !lease_exact || !snapshot_exact {
        return SnapshotMicrophoneMuteReconciliation::Stale(
            client
                .pending_microphone_mute
                .take()
                .expect("checked microphone mute transaction"),
        );
    }
    if frame.snapshot.remote_revision > pending.remote_revision
        && frame.snapshot.companion_microphone_muted == pending.muted
    {
        return SnapshotMicrophoneMuteReconciliation::Confirmed(
            client
                .pending_microphone_mute
                .take()
                .expect("checked microphone mute transaction"),
        );
    }
    SnapshotMicrophoneMuteReconciliation::None
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UrgentControlFrame {
    app_id: String,
    request_id: String,
    lease_id: String,
    lease_jti: String,
    rtc_session_id: String,
    fence: u64,
    encoded: String,
}

#[derive(Default)]
struct AdmissionGrantDeescalation {
    pending: Option<PendingLease>,
    lease: Option<ClientLease>,
}

fn apply_rotated_admission_grants(
    client: &mut ClientState,
    grants: &[Grant],
) -> AdmissionGrantDeescalation {
    client.admission_grants = Some(grants.to_vec());
    let pending = client
        .pending
        .as_ref()
        .is_some_and(|pending| !grants_permit_lease_mode(grants, pending.mode))
        .then(|| client.pending.take())
        .flatten();
    let lease = client
        .lease
        .as_ref()
        .is_some_and(|lease| !grants_permit_lease_mode(grants, lease.claims.mode))
        .then(|| client.lease.take())
        .flatten();
    if lease.is_some() {
        client.pending_end_caller = None;
        client.pending_microphone_mute = None;
    }
    AdmissionGrantDeescalation { pending, lease }
}

fn queue_exact_lease_revocation(
    state: &V2State,
    client: &mut ClientState,
    lease: &ClientLease,
    reason: &str,
) -> Result<(), String> {
    if client.pending_revoke.is_some() {
        return Err("another lease return already fences admission de-escalation".into());
    }
    let queued = exact_lease_revocation_frame(state, lease, reason)?;
    let request_id = queued.request_id.clone();
    let queue_error = if client.urgent_control_frames.len() >= MAX_URGENT_CONTROL_FRAMES {
        Some("urgent lease-revoke queue reached its safety bound".to_string())
    } else {
        client.urgent_control_frames.push_back(queued);
        None
    };
    let authoritative_sequence = client.authoritative_sequence;
    let authoritative_remote_revision = client
        .snapshot
        .as_ref()
        .filter(|snapshot| {
            snapshot.app_id == lease.claims.app_id
                && snapshot.snapshot.call_id == lease.claims.call_id
                && snapshot.snapshot.call_epoch == lease.claims.call_epoch
        })
        .map(|snapshot| snapshot.snapshot.remote_revision);
    client.pending_revoke = Some(PendingRevoke {
        request_id,
        lease_id: lease.claims.lease_id.clone(),
        lease_jti: lease.claims.jti.clone(),
        lease: lease.clone(),
        authoritative_sequence,
        authoritative_remote_revision,
        native_action_id: None,
        deadline: Instant::now() + REVOKE_CONFIRM_TIMEOUT,
    });
    match queue_error {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

fn exact_lease_revocation_frame(
    state: &V2State,
    lease: &ClientLease,
    reason: &str,
) -> Result<UrgentControlFrame, String> {
    let request_id = state.next_id("request");
    let frame = LeaseRevokeFrame {
        kind: "lease_revoke".into(),
        schema_version: SCHEMA_VERSION,
        app_id: lease.claims.app_id.clone(),
        request_id: request_id.clone(),
        idempotency_key: format!(
            "mobile:{}:{}",
            lease.claims.device_id,
            state.next_id("admission_revoke")
        ),
        lease_token: lease.token.clone(),
        reason: reason.into(),
    };
    frame.validate().map_err(|error| error.to_string())?;
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode admission-revoked lease return".to_string())?;
    Ok(UrgentControlFrame {
        app_id: lease.claims.app_id.clone(),
        request_id,
        lease_id: lease.claims.lease_id.clone(),
        lease_jti: lease.claims.jti.clone(),
        rtc_session_id: lease.claims.rtc_session_id.clone(),
        fence: lease.claims.fence,
        encoded,
    })
}

#[derive(Clone)]
struct PendingRevoke {
    request_id: String,
    lease_id: String,
    lease_jti: String,
    lease: ClientLease,
    authoritative_sequence: u64,
    authoritative_remote_revision: Option<u64>,
    native_action_id: Option<String>,
    deadline: Instant,
}

#[derive(Clone)]
struct LocalExpiryTombstone {
    lease: ClientLease,
    retain_until: Instant,
    /// Only the newest locally-returnable descendant retains its opaque bearer
    /// token. Ancestors keep claims/session routing solely to authenticate and
    /// discard late RTC/revoke traffic.
    returnable: bool,
}

fn purge_local_expiry_tombstones(client: &mut ClientState, now: Instant) {
    client
        .local_expiry_tombstones
        .retain(|tombstone| tombstone.retain_until > now);
}

fn push_local_expiry_tombstone(
    client: &mut ClientState,
    mut lease: ClientLease,
    retain_until: Instant,
    returnable: bool,
) -> Result<(), String> {
    purge_local_expiry_tombstones(client, Instant::now());
    if let Some(existing) = client
        .local_expiry_tombstones
        .iter_mut()
        .find(|existing| existing.lease.claims.jti == lease.claims.jti)
    {
        if returnable {
            existing.lease = lease;
            existing.returnable = true;
        }
        existing.retain_until = existing.retain_until.min(retain_until);
        return Ok(());
    }
    if client.local_expiry_tombstones.len() >= MAX_LOCAL_EXPIRY_TOMBSTONES {
        return Err("local media safety-fence capacity is exhausted".into());
    }
    if !returnable {
        lease.token.clear();
    }
    client
        .local_expiry_tombstones
        .push_back(LocalExpiryTombstone {
            lease,
            retain_until,
            returnable,
        });
    Ok(())
}

fn demote_local_expiry_tombstone(client: &mut ClientState, lease_jti: &str) {
    if let Some(tombstone) = client
        .local_expiry_tombstones
        .iter_mut()
        .find(|tombstone| tombstone.lease.claims.jti == lease_jti)
    {
        tombstone.lease.token.clear();
        tombstone.returnable = false;
    }
}

fn contain_late_local_expiry_renewal(
    state: &V2State,
    client: &mut ClientState,
    frame: &LeaseStatusFrame,
    replay_key: &str,
    encoded: &str,
    now: u64,
) -> Result<bool, String> {
    if frame.kind != "lease_renewed" {
        return Ok(false);
    }
    purge_local_expiry_tombstones(client, Instant::now());
    let tombstone = client
        .local_expiry_tombstones
        .iter()
        .filter(|tombstone| tombstone.returnable)
        .filter(|tombstone| validate_renewal(&tombstone.lease, &frame.lease).is_ok())
        .max_by_key(|tombstone| tombstone.lease.claims.expires_at)
        .cloned();
    let Some(tombstone) = tombstone else {
        return Ok(false);
    };

    // A relay-delayed renewal may itself have crossed its short validity
    // window. Revalidate it inside its original window so malformed/future
    // claims stay fatal while an exact, once-valid descendant can still be
    // returned. Lease status is carried by the authenticated channel; the
    // immutable authority comparison below is the lineage proof.
    match frame.lease.validate(now) {
        Ok(()) => {}
        Err(V2ProtocolError::Expired) if frame.lease.expires_at <= now => frame
            .lease
            .validate(frame.lease.expires_at.saturating_sub(1))
            .map_err(|error| error.to_string())?,
        Err(error) => return Err(error.to_string()),
    }
    validate_claim_identity(client, &frame.lease)?;
    validate_renewal(&tombstone.lease, &frame.lease)?;
    let mut session = tombstone.lease.session.clone();
    session.expires_at = expiry_datetime(frame.lease.expires_at)?;
    let renewed = ClientLease {
        request_id: tombstone.lease.request_id.clone(),
        token: frame.lease_token.clone(),
        claims: frame.lease.clone(),
        session,
    };

    // Remove only the superseded return for this exact old JTI. A newer
    // authority (or its own return transaction) may coexist while this stale
    // relay lineage is still inside the bounded tombstone window.
    if client.pending_revoke.as_ref().is_some_and(|pending| {
        pending.lease_id == tombstone.lease.claims.lease_id
            && pending.lease_jti == tombstone.lease.claims.jti
    }) {
        client.pending_revoke = None;
    }
    client.urgent_control_frames.retain(|queued| {
        queued.lease_id != tombstone.lease.claims.lease_id
            || queued.lease_jti != tombstone.lease.claims.jti
    });
    let newer_authority_exists = client.lease.is_some()
        || client.pending.is_some()
        || client.pending_revoke.is_some()
        || client.applying_lease_status.is_some();
    let queue_error = if newer_authority_exists {
        match exact_lease_revocation_frame(state, &renewed, "lease_expired") {
            Ok(queued) if client.urgent_control_frames.len() < MAX_URGENT_CONTROL_FRAMES => {
                client.urgent_control_frames.push_back(queued);
                None
            }
            Ok(_) => Some("urgent lease-revoke queue reached its safety bound".to_string()),
            Err(message) => Some(message),
        }
    } else {
        queue_exact_lease_revocation(state, client, &renewed, "lease_expired").err()
    };
    demote_local_expiry_tombstone(client, &tombstone.lease.claims.jti);
    push_local_expiry_tombstone(
        client,
        renewed,
        // Bound retention to the original fail-closed event. Delayed statuses
        // cannot keep token material alive indefinitely.
        tombstone.retain_until,
        true,
    )?;
    remember_applied_relay_frame(client, replay_key.to_owned(), encoded)?;
    if let Some(message) = queue_error {
        eprintln!(
            "[AokieCompanion][realtime] renewed local-expiry return could not enter the urgent queue: {message}"
        );
    }
    Ok(true)
}

#[derive(Clone)]
struct PendingNativeEnd {
    action: NativeCallAction,
    deadline: Instant,
}

#[derive(Clone)]
struct PendingNativeHangup {
    action_id: String,
    deadline: Instant,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NativeCallActionKind {
    Answer,
    Decline,
    End,
    Hangup,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeCallAction {
    schema_version: u16,
    action_id: String,
    kind: NativeCallActionKind,
    offer_id: String,
    app_id: String,
    call_id: String,
    call_epoch: u64,
    owner_epoch: u64,
    #[serde(default)]
    accepted_transfer_request_id: Option<String>,
    #[serde(default)]
    response_text: Option<String>,
    created_at: u64,
}

struct NativeOutbound {
    encoded: String,
    answer_action_id: Option<String>,
    answer_request_id: Option<String>,
    assistance_answer_id: Option<String>,
    revoke_request_id: Option<String>,
}

#[derive(Clone)]
enum PendingEndCaller {
    AwaitingChallenge {
        request_id: String,
    },
    AwaitingConfirmation {
        challenge: EndCallerChallengeFrame,
    },
    AwaitingSubmission {
        request_id: String,
        challenge: EndCallerChallengeFrame,
    },
    AwaitingResult {
        request_id: String,
        challenge: EndCallerChallengeFrame,
        operation_id: String,
    },
}

impl PendingEndCaller {
    fn request_id(&self) -> &str {
        match self {
            Self::AwaitingChallenge { request_id }
            | Self::AwaitingSubmission { request_id, .. }
            | Self::AwaitingResult { request_id, .. } => request_id,
            Self::AwaitingConfirmation { challenge } => &challenge.request_id,
        }
    }

    fn is_awaiting_result(&self) -> bool {
        matches!(self, Self::AwaitingResult { .. })
    }
}

fn apply_end_caller_result(
    client: &mut ClientState,
    frame: &PluginEndCallerResultFrame,
) -> Result<(Option<ClientLease>, Option<String>), String> {
    let (challenge, operation_id) = match client.pending_end_caller.as_ref() {
        Some(PendingEndCaller::AwaitingResult {
            challenge,
            operation_id,
            ..
        }) => (challenge, operation_id),
        _ => return Err("unsolicited caller-ending result".into()),
    };
    if operation_id != &frame.operation_id
        || challenge.confirmation_id != frame.confirmation_id
        || challenge.device_id != frame.device_id
        || challenge.call_id != frame.call_id
        || challenge.call_epoch != frame.call_epoch
        || challenge.owner_epoch != frame.owner_epoch
        || challenge.switchboard_revision != frame.switchboard_revision
        || challenge.remote_revision != frame.remote_revision
        || challenge.lease_id != frame.lease_id
        || challenge.fence != frame.fence
    {
        return Err("caller-ending result crossed an operation fence".into());
    }
    client.pending_end_caller = None;
    let native_hangup_action_id = client
        .pending_native_hangup
        .take()
        .map(|pending| pending.action_id);
    let lease_to_close = if frame.outcome == EndCallerOutcome::Completed
        && client.lease.as_ref().is_some_and(|lease| {
            lease.claims.app_id == frame.app_id
                && lease.claims.device_id == frame.device_id
                && lease.claims.call_id == frame.call_id
                && lease.claims.call_epoch == frame.call_epoch
                && lease.claims.owner_epoch == frame.owner_epoch
                && lease.claims.lease_id == frame.lease_id
                && lease.claims.fence == frame.fence
        }) {
        client.pending_microphone_mute = None;
        client.lease.take()
    } else {
        None
    };
    Ok((lease_to_close, native_hangup_action_id))
}

impl V2State {
    /// The high-water mark `is_new_authoritative_sequence` compares against.
    /// Read by the relay carrier so a client-minted counter starts above
    /// whatever the previous carrier already delivered.
    async fn authoritative_sequence(&self) -> u64 {
        self.inner.lock().await.authoritative_sequence
    }

    pub(crate) async fn reset(&self) {
        let mut state = self.inner.lock().await;
        let answered_assistance_requests = std::mem::take(&mut state.answered_assistance_requests);
        let applied_relay_frames = std::mem::take(&mut state.applied_relay_frames);
        let completed_revokes = std::mem::take(&mut state.completed_revokes);
        let urgent_control_frames = std::mem::take(&mut state.urgent_control_frames);
        let spent_mobile_offers = std::mem::take(&mut state.spent_mobile_offers);
        let terminal_offer_acceptances = std::mem::take(&mut state.terminal_offer_acceptances);
        *state = ClientState {
            answered_assistance_requests,
            applied_relay_frames,
            completed_revokes,
            urgent_control_frames,
            spent_mobile_offers,
            terminal_offer_acceptances,
            ..ClientState::default()
        };
    }

    async fn begin(
        &self,
        app_id: &str,
        device_id: &str,
        session_nonce: &str,
        endpoint_identity: &crate::endpoint_identity::EndpointIdentity,
        ice_servers: &[IceServerConfig],
        relay_only: bool,
        relay_transport: bool,
        admission_grants: Option<&[Grant]>,
    ) {
        let mut state = self.inner.lock().await;
        let answered_assistance_requests = std::mem::take(&mut state.answered_assistance_requests);
        let applied_relay_frames = std::mem::take(&mut state.applied_relay_frames);
        let completed_revokes = std::mem::take(&mut state.completed_revokes);
        let mut urgent_control_frames = std::mem::take(&mut state.urgent_control_frames);
        urgent_control_frames.retain(|frame| frame.app_id == app_id);
        let mut spent_mobile_offers = std::mem::take(&mut state.spent_mobile_offers);
        spent_mobile_offers.retain(|spent| spent.app_id == app_id && spent.device_id == device_id);
        let mut terminal_offer_acceptances = std::mem::take(&mut state.terminal_offer_acceptances);
        terminal_offer_acceptances
            .retain(|terminal| terminal.app_id == app_id && terminal.device_id == device_id);
        *state = ClientState {
            app_id: Some(app_id.to_owned()),
            device_id: Some(device_id.to_owned()),
            session_nonce: Some(session_nonce.to_owned()),
            endpoint_identity: Some(endpoint_identity.clone()),
            ice_servers: ice_servers.to_vec(),
            relay_only,
            relay_transport,
            admission_grants: admission_grants.map(<[Grant]>::to_vec),
            answered_assistance_requests,
            applied_relay_frames,
            completed_revokes,
            urgent_control_frames,
            spent_mobile_offers,
            terminal_offer_acceptances,
            ..ClientState::default()
        };
    }

    async fn set_generation(&self, generation: u64) {
        self.inner.lock().await.generation = generation;
    }

    async fn set_peer_key_thumbprint(&self, thumbprint: String) {
        self.inner.lock().await.peer_key_thumbprint = Some(thumbprint);
    }

    async fn managed_continuity_matches(&self, peer_key: &str, grants: &[Grant]) -> bool {
        let client = self.inner.lock().await;
        client.peer_key_thumbprint.as_deref() == Some(peer_key)
            && client.admission_grants.as_deref() == Some(grants)
    }

    async fn break_managed_continuity(
        &self,
        app: &AppHandle,
        media_state: &NativeMediaState,
        reason: &str,
    ) -> Result<(), String> {
        let (pending, native_hangup_action_id, lease, queue_error) = {
            let mut client = self.inner.lock().await;
            let pending = client.pending.take();
            let native_hangup_action_id = client
                .pending_native_hangup
                .take()
                .map(|pending| pending.action_id);
            let lease = client.lease.take();
            client.pending_end_caller = None;
            client.pending_microphone_mute = None;
            let queue_error = if let Some(lease) = lease.as_ref() {
                let retain_until = Instant::now() + LOCAL_EXPIRY_TOMBSTONE_TTL;
                let tombstone_error =
                    push_local_expiry_tombstone(&mut client, lease.clone(), retain_until, true)
                        .err();
                queue_exact_lease_revocation(self, &mut client, lease, reason)
                    .err()
                    .or(tombstone_error)
            } else {
                None
            };
            (pending, native_hangup_action_id, lease, queue_error)
        };
        if let Some(pending) = pending {
            if let Some(action_id) = pending.native_action_id.as_deref() {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    action_id,
                    false,
                    "managed_admission_continuity_changed",
                )
                .await;
            }
            let _ = crate::android_runtime::reconcile_offer(
                app,
                &pending.call_id,
                pending.call_epoch,
                "cancel",
                "managed_admission_continuity_changed",
            )
            .await;
        }
        if let Some(action_id) = native_hangup_action_id {
            let _ = crate::android_runtime::complete_native_call_action(
                app,
                &action_id,
                false,
                "managed_admission_continuity_changed",
            )
            .await;
        }
        if let Some(lease) = lease {
            let _ = media::revoke(
                app,
                media_state,
                RevokeRequest {
                    session: lease.session,
                    reason: Some("managed admission authority changed".into()),
                },
            )
            .await;
            emit_lease_reset(app);
        }
        match queue_error {
            Some(message) => Err(message),
            None => Ok(()),
        }
    }

    async fn rotate_admission_policy(
        &self,
        app: &AppHandle,
        media_state: &NativeMediaState,
        ice_servers: &[IceServerConfig],
        relay_only: bool,
        relay_transport: bool,
        grants: &[Grant],
    ) -> Result<(), String> {
        let (deescalation, queue_error) = {
            let mut client = self.inner.lock().await;
            client.ice_servers = ice_servers.to_vec();
            client.relay_only = relay_only;
            client.relay_transport = relay_transport;
            let deescalation = apply_rotated_admission_grants(&mut client, grants);
            let queue_error = if let Some(lease) = deescalation.lease.as_ref() {
                queue_exact_lease_revocation(self, &mut client, lease, "admission_grant_revoked")
                    .err()
            } else {
                None
            };
            (deescalation, queue_error)
        };

        if let Some(pending) = deescalation.pending {
            if let Some(action_id) = pending.native_action_id.as_deref() {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    action_id,
                    false,
                    "admission_grant_revoked",
                )
                .await;
            }
            let _ = crate::android_runtime::reconcile_offer(
                app,
                &pending.call_id,
                pending.call_epoch,
                "cancel",
                "admission_grant_revoked",
            )
            .await;
        }
        if let Some(lease) = deescalation.lease {
            let _ = media::revoke(
                app,
                media_state,
                RevokeRequest {
                    session: lease.session,
                    reason: Some("current admission revoked this media mode".into()),
                },
            )
            .await;
            emit_lease_reset(app);
        }
        match queue_error {
            Some(message) => Err(message),
            None => Ok(()),
        }
    }

    async fn reset_generation(&self, generation: u64) {
        let mut state = self.inner.lock().await;
        if state.generation == generation {
            let answered_assistance_requests =
                std::mem::take(&mut state.answered_assistance_requests);
            let applied_relay_frames = std::mem::take(&mut state.applied_relay_frames);
            let completed_revokes = std::mem::take(&mut state.completed_revokes);
            let urgent_control_frames = std::mem::take(&mut state.urgent_control_frames);
            let spent_mobile_offers = std::mem::take(&mut state.spent_mobile_offers);
            let terminal_offer_acceptances = std::mem::take(&mut state.terminal_offer_acceptances);
            *state = ClientState {
                answered_assistance_requests,
                applied_relay_frames,
                completed_revokes,
                urgent_control_frames,
                spent_mobile_offers,
                terminal_offer_acceptances,
                ..ClientState::default()
            };
        }
    }

    fn next_id(&self, prefix: &str) -> String {
        let id = self
            .ids
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
            .max(1);
        format!("{prefix}_{id}")
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseEvent {
    session: MediaSession,
    mode: LeaseMode,
    phase: LeasePhase,
    provisional: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V2RequestReceipt {
    request_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V2AssistanceReceipt {
    request_id: String,
    answer_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndCallerConfirmRequest {
    confirmation_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MicrophoneMuteRequest {
    muted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EndCallerChallengeEvent {
    kind: &'static str,
    schema_version: u16,
    app_id: String,
    request_id: String,
    confirmation_id: String,
    device_id: String,
    call_id: String,
    call_epoch: u64,
    owner_epoch: u64,
    switchboard_revision: u64,
    remote_revision: u64,
    lease_id: String,
    fence: u64,
    expires_at: u64,
}

impl From<&EndCallerChallengeFrame> for EndCallerChallengeEvent {
    fn from(frame: &EndCallerChallengeFrame) -> Self {
        Self {
            kind: "end_caller_challenge",
            schema_version: frame.schema_version,
            app_id: frame.app_id.clone(),
            request_id: frame.request_id.clone(),
            confirmation_id: frame.confirmation_id.clone(),
            device_id: frame.device_id.clone(),
            call_id: frame.call_id.clone(),
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            lease_id: frame.lease_id.clone(),
            fence: frame.fence,
            expires_at: frame.expires_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EndCallerSubmittedFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    request_id: String,
    operation_id: String,
    confirmation_id: String,
    accepted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EndCallerFailureEvent {
    kind: &'static str,
    schema_version: u16,
    request_id: String,
    code: String,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssistanceAnswerRequest {
    request_id: String,
    #[serde(default)]
    response_action: AssistanceResponseAction,
    answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssistanceAnswerAcceptedFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    request_id: String,
    answer_id: String,
    accepted: bool,
}

// ⚠️ The four frames below are `Serialize` as well as `Deserialize` ONLY so the
// relay shim can mint them from their plugin-dialect twins. Nothing in this
// module sends one to a peer — the session consumes its own translation — so
// serialization here is an internal hand-off, not a wire surface, and the
// WebSocket carrier still receives every one of them verbatim from the gateway.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseStatusFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    request_id: String,
    lease_token: String,
    lease: LeaseClaims,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provisional: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GatewayRtcFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    signal_id: String,
    plugin_id: String,
    device_id: String,
    lease_jti: String,
    rtc_session_id: String,
    sdp_revision: u64,
    transport_generation: u64,
    call_id: String,
    call_epoch: u64,
    owner_epoch: u64,
    fence: u64,
    signal: RtcSignal,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseRevokedFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    lease_id: String,
    lease_jti: String,
    reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MobileOfferAcceptedFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    request_id: String,
    offer_id: String,
    offer_jti: String,
    offered_mode: LeaseMode,
    accepted: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaimRejectedFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    request_id: String,
    code: String,
    message: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ClaimRejectionAction {
    AlreadyApplied,
    RejectPending { call_id: String, call_epoch: u64 },
    RejectPrepared { lease: ClientLease },
    ContainedUnmatched,
}

/// The mobile frame KIND a plugin-dialect lease status becomes, and the
/// `provisional` flag [`apply_lease_status`] expects beside it.
///
/// The plugin dialect names the transition in a `status` FIELD; the mobile
/// dialect names it in the frame KIND. This rename is the shim's whole job for
/// that frame, and the flag is not decorative — `apply_lease_status` branches
/// on the kind and then cross-checks `provisional` against `lease.phase`,
/// refusing the frame if they disagree.
///
/// Status-versus-claims agreement is enforced upstream by
/// `PluginLeaseStatusFrame::validate`, so by the time a status reaches here it
/// cannot name a transition its own lease contradicts.
fn mobile_lease_kind(status: PluginLeaseStatus) -> (&'static str, Option<bool>) {
    match status {
        PluginLeaseStatus::Granted => ("lease_granted", None),
        PluginLeaseStatus::Provisional => ("claim_provisional", Some(true)),
        PluginLeaseStatus::Active => ("claim_active", Some(false)),
        PluginLeaseStatus::Renewed => ("lease_renewed", None),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ErrorFrame {
    kind: String,
    schema_version: u16,
    #[serde(default)]
    app_id: Option<String>,
    code: String,
    message: String,
    #[serde(default)]
    request_id: Option<String>,
}

fn tracks_gateway_request(client: &ClientState, request_id: &str) -> bool {
    client.pending.as_ref().is_some_and(|pending| {
        pending.request_id == request_id || pending.offer_request_id == request_id
    }) || client
        .pending_revoke
        .as_ref()
        .is_some_and(|pending| pending.request_id == request_id)
        || (client.pending_assistance_answer.is_some()
            && client
                .assistance
                .as_ref()
                .is_some_and(|assistance| assistance.request_id == request_id))
        || client
            .pending_end_caller
            .as_ref()
            .is_some_and(|pending| pending.request_id() == request_id)
        || client
            .pending_microphone_mute
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id)
}

fn grant_for_requested_mode(mode: LeaseMode) -> Result<Grant, String> {
    match mode {
        LeaseMode::Monitor => Ok(Grant::Monitor),
        LeaseMode::Takeover => Ok(Grant::Takeover),
        LeaseMode::Consult => Ok(Grant::Consult),
    }
}

fn grants_permit_lease_mode(grants: &[Grant], mode: LeaseMode) -> bool {
    grants.contains(&Grant::StateRead)
        && grants.contains(&Grant::RtcSignal)
        && grant_for_requested_mode(mode)
            .is_ok_and(|mode_grant| grants.contains(&mode_grant))
        // Takeover can hand the caller back to Aokie. Without ResumeAokie the
        // mobile must not accept or keep that authority, even if an old queued
        // offer/status was minted under broader admission.
        && (mode != LeaseMode::Takeover || grants.contains(&Grant::ResumeAokie))
}

fn current_client_grants(client: &ClientState) -> &[Grant] {
    client
        .admission_grants
        .as_deref()
        .or_else(|| {
            client
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.grants.as_slice())
        })
        .unwrap_or_default()
}

fn remote_consent_is_current(consent: &RemoteConsentPolicy, now: u64) -> bool {
    consent.enabled
        && consent.acknowledged
        && consent.expires_at.as_ref().is_none_or(|expires_at| {
            DateTime::parse_from_rfc3339(expires_at)
                .ok()
                .and_then(|value| u64::try_from(value.timestamp()).ok())
                .is_some_and(|expires_at| expires_at > now)
        })
}

fn current_consent_permits_mode(consent: &RemoteConsentPolicy, mode: LeaseMode, now: u64) -> bool {
    remote_consent_is_current(consent, now) && consent.allows(mode)
}

#[tauri::command]
pub async fn realtime_v2_request_lease(
    state: State<'_, RealtimeState>,
    request: V2LeaseRequest,
) -> Result<V2RequestReceipt, String> {
    let (frame, request_id) = prepare_offer_answer_for_transfer(
        &state.v2,
        request.mode,
        None,
        request.accepted_transfer_request_id.as_deref(),
    )
    .await?;
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode protocol-v2 mobile offer answer".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        let _ = clear_pending_lease(&state.v2, &request_id).await;
        return Err(error);
    }
    Ok(V2RequestReceipt { request_id })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct V2LeaseRequest {
    mode: LeaseMode,
    #[serde(default)]
    accepted_transfer_request_id: Option<String>,
}

async fn prepare_offer_answer(
    state: &V2State,
    requested_mode: LeaseMode,
    native_action: Option<&NativeCallAction>,
) -> Result<(MobileOfferAnswerFrame, String), String> {
    prepare_offer_answer_for_transfer(state, requested_mode, native_action, None).await
}

async fn prepare_offer_answer_for_transfer(
    state: &V2State,
    requested_mode: LeaseMode,
    native_action: Option<&NativeCallAction>,
    accepted_transfer_request_id: Option<&str>,
) -> Result<(MobileOfferAnswerFrame, String), String> {
    let (frame, request_id) = {
        let mut client = state.inner.lock().await;
        if client.generation == 0 {
            return Err("protocol-v2 realtime is offline or still synchronising".into());
        }
        if client.pending.is_some()
            || client.lease.is_some()
            || client.pending_revoke.is_some()
            || client.pending_native_end.is_some()
            || client.pending_end_caller.is_some()
            || client.pending_native_hangup.is_some()
        {
            return Err("a media lease is already pending or active".into());
        }
        let snapshot = client
            .snapshot
            .clone()
            .ok_or("protocol-v2 authoritative state is unavailable")?;
        let selected = select_mobile_offer(
            &client,
            &snapshot,
            requested_mode,
            native_action,
            accepted_transfer_request_id,
            unix_now()?,
        )?;
        let mode = selected.offer.offered_mode;
        if let Some(action) = native_action {
            validate_native_action(action, unix_now()?)?;
            if action.kind != NativeCallActionKind::Answer {
                return Err("native Answer does not match authoritative call state".into());
            }
        }
        if !grants_permit_lease_mode(current_client_grants(&client), mode) {
            return Err("the admission does not grant this media mode and RTC signalling".into());
        }
        if !matches!(snapshot.snapshot.telephony_state, TelephonyState::Active) {
            return Err("media leases require an active cellular call".into());
        }
        if matches!(
            snapshot.snapshot.media_state,
            MediaState::None | MediaState::Failed
        ) {
            return Err("the Aokie endpoint has no usable media route".into());
        }
        if !current_consent_permits_mode(&snapshot.snapshot.remote_consent, mode, unix_now()?) {
            return Err("current remote disclosure consent does not allow this media mode".into());
        }
        if matches!(mode, LeaseMode::Takeover)
            && !matches!(snapshot.snapshot.service_mode, ServiceMode::AokieActive)
        {
            return Err("takeover is not available in the current service mode".into());
        }
        if let Some(request_id) = accepted_transfer_request_id {
            validate_id(request_id, "accepted transfer requestId")?;
            let assistance = client
                .assistance
                .as_ref()
                .ok_or("the transfer request is no longer active")?;
            if !assistance.transfer_offered
                || assistance.request_id != request_id
                || assistance.expires_at <= unix_now()?
                || assistance.call_id != snapshot.snapshot.call_id
                || assistance.call_epoch != snapshot.snapshot.call_epoch
                || assistance.owner_epoch != snapshot.snapshot.owner_epoch
                || assistance.switchboard_revision != snapshot.snapshot.switchboard_revision
                || assistance.remote_revision != snapshot.snapshot.remote_revision
            {
                return Err("the accepted transfer request is stale against call state".into());
            }
        }
        if matches!(mode, LeaseMode::Consult) {
            if !matches!(snapshot.snapshot.service_mode, ServiceMode::AokieActive) {
                return Err(
                    "private consultation is not available in the current service mode".into(),
                );
            }
            let assistance = client
                .assistance
                .as_ref()
                .ok_or("private consultation requires an active Aokie help request")?;
            if assistance.expires_at <= unix_now()?
                || assistance.call_id != snapshot.snapshot.call_id
                || assistance.call_epoch != snapshot.snapshot.call_epoch
                || assistance.owner_epoch != snapshot.snapshot.owner_epoch
                || assistance.switchboard_revision != snapshot.snapshot.switchboard_revision
                || assistance.remote_revision != snapshot.snapshot.remote_revision
            {
                return Err("private consultation help request is stale against call state".into());
            }
        }

        let offer_request_id = state.next_id("offer_answer");
        let request_id = state.next_id("request");
        let rtc_session_id = state.next_id("rtc");
        let app_id = client
            .app_id
            .clone()
            .ok_or("v2 app identity is unavailable")?;
        let device_id = client
            .device_id
            .clone()
            .ok_or("v2 device identity is unavailable")?;
        let frame = LeaseRequestFrame {
            kind: "lease_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id,
            request_id: request_id.clone(),
            idempotency_key: format!("mobile:{device_id}:{request_id}"),
            call_id: snapshot.snapshot.call_id.clone(),
            expected_call_epoch: snapshot.snapshot.call_epoch,
            expected_owner_epoch: snapshot.snapshot.owner_epoch,
            expected_switchboard_revision: snapshot.snapshot.switchboard_revision,
            expected_remote_revision: snapshot.snapshot.remote_revision,
            mode,
            rtc_session_id: rtc_session_id.clone(),
            accepted_offer_id: selected.offer.offer_id.clone(),
            accepted_offer_jti: selected.offer.jti.clone(),
            accepted_transfer_request_id: selected.offer.accepted_transfer_request_id.clone(),
        };
        frame.validate().map_err(|error| error.to_string())?;
        let answer = MobileOfferAnswerFrame {
            kind: "mobile_offer_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: snapshot.app_id.clone(),
            request_id: offer_request_id.clone(),
            idempotency_key: format!("mobile:{device_id}:{offer_request_id}"),
            offer_id: selected.offer.offer_id.clone(),
            offer_jti: selected.offer.jti.clone(),
            offer_token: selected.offer_token.clone(),
            target_device_id: selected.offer.target_device_id.clone(),
            target_holder_key_thumbprint: selected.offer.target_holder_key_thumbprint.clone(),
            offered_mode: selected.offer.offered_mode,
            call_id: selected.offer.call_id.clone(),
            call_epoch: selected.offer.call_epoch,
            owner_epoch: selected.offer.owner_epoch,
        };
        answer.validate().map_err(|error| error.to_string())?;
        // The authority spends an offer when it accepts this answer. Mirror
        // that one-shot boundary locally before exposing the pending attempt:
        // clearing a failed claim must never reveal the same cached offer to a
        // second button press. The exact `answer` value remains available to
        // the outbound transport for its normal at-least-once retry.
        tombstone_mobile_offer(&mut client, &selected, Some(&offer_request_id));
        client.pending = Some(PendingLease {
            request_id: request_id.clone(),
            offer_request_id,
            mode,
            rtc_session_id,
            call_id: snapshot.snapshot.call_id.clone(),
            call_epoch: snapshot.snapshot.call_epoch,
            owner_epoch: snapshot.snapshot.owner_epoch,
            accepted_offer_id: selected.offer.offer_id.clone(),
            accepted_offer_jti: selected.offer.jti.clone(),
            lease_frame: frame,
            stage: PendingLeaseStage::AwaitingOfferAcceptance,
            deadline: Instant::now() + LEASE_REQUEST_TIMEOUT,
            native_action_id: native_action.map(|action| action.action_id.clone()),
            native_deadline: native_action.map(|_| Instant::now() + NATIVE_ACTION_TIMEOUT),
        });
        (answer, request_id)
    };
    Ok((frame, request_id))
}

fn select_mobile_offer(
    client: &ClientState,
    snapshot: &MobileSnapshotFrame,
    requested_mode: LeaseMode,
    native_action: Option<&NativeCallAction>,
    accepted_transfer_request_id: Option<&str>,
    now: u64,
) -> Result<SignedPendingMobileOffer, String> {
    let device_id = client
        .device_id
        .as_deref()
        .ok_or("v2 device identity is unavailable")?;
    let holder_key_thumbprint = client
        .endpoint_identity
        .as_ref()
        .map(crate::endpoint_identity::EndpointIdentity::thumbprint)
        .ok_or("native endpoint identity is unavailable")?;
    let mut matches = snapshot
        .snapshot
        .pending_mobile_offers
        .iter()
        .filter(|signed| {
            let offer = &signed.offer;
            !mobile_offer_was_spent(client, signed)
                && signed.validate(now).is_ok()
                && offer.target_device_id == device_id
                && offer.target_holder_key_thumbprint == holder_key_thumbprint
                && offer.app_id == snapshot.app_id
                && offer.call_id == snapshot.snapshot.call_id
                && offer.call_epoch == snapshot.snapshot.call_epoch
                && offer.owner_epoch == snapshot.snapshot.owner_epoch
                && offer.switchboard_revision == snapshot.snapshot.switchboard_revision
                && offer.remote_revision == snapshot.snapshot.remote_revision
                && offer.accepted_transfer_request_id.as_deref() == accepted_transfer_request_id
                && offer.required_consent_policy_id == snapshot.snapshot.remote_consent.policy_id
                && offer.required_consent_policy_version
                    == snapshot.snapshot.remote_consent.policy_version
                && offer
                    .required_grants
                    .iter()
                    .all(|grant| snapshot.grants.contains(grant))
                && current_consent_permits_mode(
                    &snapshot.snapshot.remote_consent,
                    offer.offered_mode,
                    now,
                )
                && match native_action {
                    Some(action) => {
                        offer.offer_id == action.offer_id
                            && offer.app_id == action.app_id
                            && offer.call_id == action.call_id
                            && offer.call_epoch == action.call_epoch
                            && offer.owner_epoch == action.owner_epoch
                            && offer.accepted_transfer_request_id
                                == action.accepted_transfer_request_id
                            && offer.surface == MobileOfferSurface::VoiceSystemUi
                    }
                    None => {
                        offer.offered_mode == requested_mode
                            && offer.surface == MobileOfferSurface::InApp
                    }
                }
        });
    let selected = matches
        .next()
        .ok_or("no current signed mobile offer permits this lease")?;
    if matches.next().is_some() {
        return Err("multiple signed mobile offers require an explicit offer selection".into());
    }
    Ok(selected.clone())
}

fn authoritative_android_offer(
    client: &ClientState,
    now: u64,
) -> Result<Option<crate::android_runtime::AndroidAuthoritativeOffer>, String> {
    let Some(snapshot) = client.snapshot.as_ref() else {
        return Ok(None);
    };
    let Some(device_id) = client.device_id.as_deref() else {
        return Ok(None);
    };
    let Some(holder_key_thumbprint) = client
        .endpoint_identity
        .as_ref()
        .map(crate::endpoint_identity::EndpointIdentity::thumbprint)
    else {
        return Ok(None);
    };
    let active_transfer_id = client.assistance.as_ref().and_then(|assistance| {
        (assistance.transfer_offered
            && assistance.expires_at > now
            && assistance.app_id == snapshot.app_id
            && assistance.call_id == snapshot.snapshot.call_id
            && assistance.call_epoch == snapshot.snapshot.call_epoch
            && assistance.owner_epoch == snapshot.snapshot.owner_epoch
            && assistance.switchboard_revision == snapshot.snapshot.switchboard_revision
            && assistance.remote_revision == snapshot.snapshot.remote_revision)
            .then_some(assistance.request_id.as_str())
    });
    let grants = current_client_grants(client);
    let mut matches = snapshot
        .snapshot
        .pending_mobile_offers
        .iter()
        .filter(|signed| {
            let offer = &signed.offer;
            !mobile_offer_was_spent(client, signed)
                && signed.validate(now).is_ok()
                && offer.surface == MobileOfferSurface::VoiceSystemUi
                && offer.offered_mode == LeaseMode::Takeover
                && offer.target_device_id == device_id
                && offer.target_holder_key_thumbprint == holder_key_thumbprint
                && offer.app_id == snapshot.app_id
                && offer.call_id == snapshot.snapshot.call_id
                && offer.call_epoch == snapshot.snapshot.call_epoch
                && offer.owner_epoch == snapshot.snapshot.owner_epoch
                && offer.switchboard_revision == snapshot.snapshot.switchboard_revision
                && offer.remote_revision == snapshot.snapshot.remote_revision
                && offer.accepted_transfer_request_id.as_deref() == active_transfer_id
                && offer.required_consent_policy_id == snapshot.snapshot.remote_consent.policy_id
                && offer.required_consent_policy_version
                    == snapshot.snapshot.remote_consent.policy_version
                && offer
                    .required_grants
                    .iter()
                    .all(|required| grants.contains(required))
                && current_consent_permits_mode(
                    &snapshot.snapshot.remote_consent,
                    LeaseMode::Takeover,
                    now,
                )
        });
    let Some(selected) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err("multiple native call offers matched current authenticated state".into());
    }
    let offer = &selected.offer;
    Ok(Some(crate::android_runtime::AndroidAuthoritativeOffer {
        schema_version: 1,
        event_id: offer.opportunity_id.clone(),
        offer_id: offer.offer_id.clone(),
        opportunity_id: offer.opportunity_id.clone(),
        app_id: offer.app_id.clone(),
        call_id: offer.call_id.clone(),
        call_epoch: offer.call_epoch,
        owner_epoch: offer.owner_epoch,
        expires_at: offer.expires_at,
        accepted_transfer_request_id: offer.accepted_transfer_request_id.clone(),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AndroidOfferReconciliation {
    Present(crate::android_runtime::AndroidAuthoritativeOffer),
    Cancel { call_id: String, call_epoch: u64 },
    PreserveLocalClaim,
    None,
}

fn android_offer_reconciliation(
    client: &ClientState,
    now: u64,
) -> Result<AndroidOfferReconciliation, String> {
    let Some(snapshot) = client.snapshot.as_ref() else {
        return Ok(AndroidOfferReconciliation::None);
    };
    let call_id = snapshot.snapshot.call_id.as_str();
    let call_epoch = snapshot.snapshot.call_epoch;
    let local_claim = client
        .pending
        .as_ref()
        .is_some_and(|pending| pending.call_id == call_id && pending.call_epoch == call_epoch)
        || client.lease.as_ref().is_some_and(|lease| {
            lease.claims.call_id == call_id && lease.claims.call_epoch == call_epoch
        });
    if local_claim {
        return Ok(AndroidOfferReconciliation::PreserveLocalClaim);
    }
    if let Some(offer) = authoritative_android_offer(client, now)? {
        return Ok(AndroidOfferReconciliation::Present(offer));
    }
    Ok(AndroidOfferReconciliation::Cancel {
        call_id: call_id.to_owned(),
        call_epoch,
    })
}

/// Resolve the exact call identity whose Android call surface must be retired
/// before transport state is cleared. A ringing VoiceSystemUi offer has no
/// pending lease yet, so the authenticated snapshot is the final fallback.
fn native_call_to_cancel_on_disconnect(client: &ClientState) -> Option<(String, u64)> {
    client
        .pending
        .as_ref()
        .map(|pending| (pending.call_id.clone(), pending.call_epoch))
        .or_else(|| {
            client
                .lease
                .as_ref()
                .map(|lease| (lease.claims.call_id.clone(), lease.claims.call_epoch))
        })
        .or_else(|| {
            client.snapshot.as_ref().map(|snapshot| {
                (
                    snapshot.snapshot.call_id.clone(),
                    snapshot.snapshot.call_epoch,
                )
            })
        })
}

/// A direct snapshot-to-snapshot call transition must cancel the previous
/// native identity before publishing the replacement. Cancelling the new call
/// cannot match the old encrypted offer-store record and leaves it ringing.
fn replaced_snapshot_call(
    client: &ClientState,
    incoming: &MobileSnapshotFrame,
) -> Option<(String, u64)> {
    client.snapshot.as_ref().and_then(|previous| {
        (previous.snapshot.call_id != incoming.snapshot.call_id
            || previous.snapshot.call_epoch != incoming.snapshot.call_epoch)
            .then(|| {
                (
                    previous.snapshot.call_id.clone(),
                    previous.snapshot.call_epoch,
                )
            })
    })
}

async fn publish_current_android_offer(app: &AppHandle, state: &V2State) -> Result<(), String> {
    let reconciliation = {
        let client = state.inner.lock().await;
        android_offer_reconciliation(&client, unix_now()?)?
    };
    match reconciliation {
        AndroidOfferReconciliation::Present(offer) => {
            let _ = crate::android_runtime::present_authoritative_offer(app, &offer).await?;
        }
        AndroidOfferReconciliation::Cancel {
            call_id,
            call_epoch,
        } => {
            let _ = crate::android_runtime::reconcile_offer(
                app,
                &call_id,
                call_epoch,
                "cancel",
                "authoritative_voice_offer_unavailable",
            )
            .await?;
        }
        AndroidOfferReconciliation::PreserveLocalClaim | AndroidOfferReconciliation::None => {}
    }
    Ok(())
}

fn mobile_offer_was_spent(client: &ClientState, signed: &SignedPendingMobileOffer) -> bool {
    client
        .spent_mobile_offers
        .iter()
        .any(|spent| spent_mobile_offer_matches(spent, signed))
}

fn spent_mobile_offer_matches(spent: &SpentMobileOffer, signed: &SignedPendingMobileOffer) -> bool {
    let offer = &signed.offer;
    spent.app_id == offer.app_id
        && spent.device_id == offer.target_device_id
        && spent.call_id == offer.call_id
        && spent.call_epoch == offer.call_epoch
        // Either identifier being reused inside the same app/device/call is
        // not a fresh invitation. A legitimate replacement rotates both.
        && (spent.offer_id == offer.offer_id || spent.offer_jti == offer.jti)
}

fn remember_terminal_offer_acceptance(client: &mut ClientState, pending: &PendingLease) {
    let Some(device_id) = client.device_id.clone() else {
        return;
    };
    let terminal = TerminalOfferAcceptance {
        app_id: pending.lease_frame.app_id.clone(),
        device_id,
        request_id: pending.offer_request_id.clone(),
        offer_id: pending.accepted_offer_id.clone(),
        offer_jti: pending.accepted_offer_jti.clone(),
        offered_mode: pending.mode,
    };
    if client
        .terminal_offer_acceptances
        .iter()
        .any(|known| known == &terminal)
    {
        return;
    }
    if client.terminal_offer_acceptances.len() >= MAX_TERMINAL_OFFER_ACCEPTANCES {
        client.terminal_offer_acceptances.pop_front();
    }
    client.terminal_offer_acceptances.push_back(terminal);
}

fn terminal_offer_acceptance_matches(
    client: &ClientState,
    frame: &MobileOfferAcceptedFrame,
) -> Result<bool, String> {
    if let Some(terminal) = client
        .terminal_offer_acceptances
        .iter()
        .find(|terminal| terminal.request_id == frame.request_id)
    {
        return if terminal.app_id == frame.app_id
            && terminal.offer_id == frame.offer_id
            && terminal.offer_jti == frame.offer_jti
            && terminal.offered_mode == frame.offered_mode
        {
            Ok(true)
        } else {
            Err("late mobile offer acknowledgement crossed its terminal offer fence".into())
        };
    }
    let Some(spent) = client
        .spent_mobile_offers
        .iter()
        .find(|spent| spent.offer_request_id.as_deref() == Some(frame.request_id.as_str()))
    else {
        return Ok(false);
    };
    if spent.app_id == frame.app_id
        && spent.offer_id == frame.offer_id
        && spent.offer_jti == frame.offer_jti
        && spent.offered_mode == frame.offered_mode
    {
        Ok(true)
    } else {
        Err("late mobile offer acknowledgement crossed its terminal offer fence".into())
    }
}

fn retain_spent_mobile_offers_for_snapshot(client: &mut ClientState, frame: &MobileSnapshotFrame) {
    let device_id = client.device_id.as_deref();
    client.spent_mobile_offers.retain(|spent| {
        spent.app_id == frame.app_id
            && Some(spent.device_id.as_str()) == device_id
            && spent.call_id == frame.snapshot.call_id
            && spent.call_epoch == frame.snapshot.call_epoch
    });
}

fn suppress_spent_mobile_offers(client: &ClientState, frame: &mut MobileSnapshotFrame) {
    frame
        .snapshot
        .pending_mobile_offers
        .retain(|offer| !mobile_offer_was_spent(client, offer));
}

fn tombstone_mobile_offer(
    client: &mut ClientState,
    selected: &SignedPendingMobileOffer,
    offer_request_id: Option<&str>,
) {
    if !mobile_offer_was_spent(client, selected) {
        if client.spent_mobile_offers.len() >= MAX_SPENT_MOBILE_OFFERS {
            client.spent_mobile_offers.pop_front();
        }
        client.spent_mobile_offers.push_back(SpentMobileOffer {
            app_id: selected.offer.app_id.clone(),
            device_id: selected.offer.target_device_id.clone(),
            call_id: selected.offer.call_id.clone(),
            call_epoch: selected.offer.call_epoch,
            offer_id: selected.offer.offer_id.clone(),
            offer_jti: selected.offer.jti.clone(),
            offered_mode: selected.offer.offered_mode,
            offer_request_id: offer_request_id.map(str::to_owned),
        });
    } else if let Some(offer_request_id) = offer_request_id {
        // A decline may have tombstoned this offer without an acceptance
        // request. Never overwrite another request identity, but enrich that
        // exact one-shot record when the lease-answer path owns it.
        if let Some(spent) = client.spent_mobile_offers.iter_mut().find(|spent| {
            spent_mobile_offer_matches(spent, selected) && spent.offer_request_id.is_none()
        }) {
            spent.offer_request_id = Some(offer_request_id.to_owned());
        }
    }
    if let Some(snapshot) = client.snapshot.as_mut() {
        let spent = SpentMobileOffer {
            app_id: selected.offer.app_id.clone(),
            device_id: selected.offer.target_device_id.clone(),
            call_id: selected.offer.call_id.clone(),
            call_epoch: selected.offer.call_epoch,
            offer_id: selected.offer.offer_id.clone(),
            offer_jti: selected.offer.jti.clone(),
            offered_mode: selected.offer.offered_mode,
            offer_request_id: offer_request_id.map(str::to_owned),
        };
        snapshot
            .snapshot
            .pending_mobile_offers
            .retain(|offer| !spent_mobile_offer_matches(&spent, offer));
    }
}

async fn clear_pending_lease(state: &V2State, request_id: &str) -> Option<String> {
    let mut client = state.inner.lock().await;
    if client.pending.as_ref().is_some_and(|pending| {
        pending.request_id == request_id || pending.offer_request_id == request_id
    }) {
        let pending = client.pending.take().expect("checked pending lease");
        remember_terminal_offer_acceptance(&mut client, &pending);
        return pending.native_action_id;
    }
    None
}

fn validate_native_action(action: &NativeCallAction, now: u64) -> Result<(), String> {
    if action.schema_version != 2
        || action.call_epoch == 0
        || action.call_epoch > MAX_SAFE_INTEGER
        || action.owner_epoch > MAX_SAFE_INTEGER
        || action.created_at > now.saturating_add(5)
        || now.saturating_sub(action.created_at) > 15
    {
        return Err("native call action is stale or invalid".into());
    }
    for (value, label) in [
        (&action.action_id, "native actionId"),
        (&action.offer_id, "native offerId"),
        (&action.app_id, "native appId"),
        (&action.call_id, "native callId"),
    ] {
        validate_id(value, label)?;
    }
    if let Some(request_id) = &action.accepted_transfer_request_id {
        validate_id(request_id, "native acceptedTransferRequestId")?;
    }
    if action.response_text.as_ref().is_some_and(|text| {
        text.is_empty() || text.len() > 500 || text.chars().any(char::is_control)
    }) {
        return Err("native transfer response text is invalid".into());
    }
    match action.kind {
        NativeCallActionKind::Answer => {
            if action.response_text.is_some() {
                return Err("native Answer cannot carry response text".into());
            }
        }
        NativeCallActionKind::Decline => {
            if action.accepted_transfer_request_id.is_none() || action.response_text.is_none() {
                return Err("native Decline lacks its exact transfer response binding".into());
            }
        }
        NativeCallActionKind::End | NativeCallActionKind::Hangup => {
            if action.accepted_transfer_request_id.is_some() || action.response_text.is_some() {
                return Err(
                    "native call termination cannot carry transfer response authority".into(),
                );
            }
        }
    }
    Ok(())
}

fn validate_local_end_caller(client: &ClientState, now: u64) -> Result<(), String> {
    let snapshot = client
        .snapshot
        .as_ref()
        .ok_or("protocol-v2 authoritative state is unavailable")?;
    let lease = client
        .lease
        .as_ref()
        .ok_or("an active takeover lease is required to end the caller call")?;
    if !snapshot.grants.contains(&Grant::EndCaller) || !snapshot.grants.contains(&Grant::Takeover) {
        return Err("the admission does not grant caller-ending control".into());
    }
    if lease.claims.mode != LeaseMode::Takeover
        || lease.claims.phase != LeasePhase::Active
        || lease.claims.fence == 0
        || lease.claims.expires_at <= now
    {
        return Err("only the current active takeover owner may end the caller call".into());
    }
    if snapshot.snapshot.call_id != lease.claims.call_id
        || snapshot.snapshot.call_epoch != lease.claims.call_epoch
        || snapshot.snapshot.owner_epoch != lease.claims.owner_epoch
        || snapshot.snapshot.telephony_state != TelephonyState::Active
        || snapshot.snapshot.service_mode != ServiceMode::HumanActive
        || !current_consent_permits_mode(
            &snapshot.snapshot.remote_consent,
            LeaseMode::Takeover,
            now,
        )
    {
        return Err("the active takeover lease no longer matches authoritative call state".into());
    }
    Ok(())
}

#[tauri::command]
pub async fn realtime_v2_prepare_end_caller(
    state: State<'_, RealtimeState>,
) -> Result<V2RequestReceipt, String> {
    let (frame, request_id) = {
        let mut client = state.v2.inner.lock().await;
        if client.generation == 0 {
            return Err("protocol-v2 realtime is offline".into());
        }
        if client.pending_end_caller.is_some() {
            return Err("a caller-ending confirmation is already pending".into());
        }
        validate_local_end_caller(&client, unix_now()?)?;
        let snapshot = client.snapshot.as_ref().expect("validated snapshot");
        let lease = client.lease.as_ref().expect("validated lease");
        let request_id = state.v2.next_id("end_prepare");
        let device_id = client
            .device_id
            .clone()
            .ok_or("v2 device identity is unavailable")?;
        let frame = MobileEndCallerChallengeRequestFrame {
            kind: "end_caller_challenge_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: snapshot.app_id.clone(),
            request_id: request_id.clone(),
            idempotency_key: format!("mobile:{device_id}:{request_id}"),
            lease_token: lease.token.clone(),
            call_id: lease.claims.call_id.clone(),
            call_epoch: lease.claims.call_epoch,
            owner_epoch: lease.claims.owner_epoch,
            switchboard_revision: snapshot.snapshot.switchboard_revision,
            remote_revision: snapshot.snapshot.remote_revision,
            fence: lease.claims.fence,
        };
        frame.validate().map_err(|error| error.to_string())?;
        client.pending_end_caller = Some(PendingEndCaller::AwaitingChallenge {
            request_id: request_id.clone(),
        });
        (frame, request_id)
    };
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode caller-ending challenge request".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        let mut client = state.v2.inner.lock().await;
        if client
            .pending_end_caller
            .as_ref()
            .is_some_and(|pending| pending.request_id() == request_id)
        {
            client.pending_end_caller = None;
        }
        return Err(error);
    }
    Ok(V2RequestReceipt { request_id })
}

#[tauri::command]
pub async fn realtime_v2_confirm_end_caller(
    state: State<'_, RealtimeState>,
    request: EndCallerConfirmRequest,
) -> Result<V2RequestReceipt, String> {
    let (frame, request_id, challenge) = {
        let mut client = state.v2.inner.lock().await;
        if client.generation == 0 {
            return Err("protocol-v2 realtime is offline".into());
        }
        validate_local_end_caller(&client, unix_now()?)?;
        let challenge = match client.pending_end_caller.as_ref() {
            Some(PendingEndCaller::AwaitingConfirmation { challenge })
                if challenge.confirmation_id == request.confirmation_id =>
            {
                challenge.clone()
            }
            _ => return Err("the caller-ending confirmation is not current".into()),
        };
        if challenge.expires_at <= unix_now()? {
            client.pending_end_caller = None;
            return Err("the caller-ending confirmation expired".into());
        }
        let snapshot = client.snapshot.as_ref().expect("validated snapshot");
        let lease = client.lease.as_ref().expect("validated lease");
        if challenge.device_id != client.device_id.as_deref().unwrap_or_default()
            || challenge.call_id != lease.claims.call_id
            || challenge.call_epoch != lease.claims.call_epoch
            || challenge.owner_epoch != lease.claims.owner_epoch
            || challenge.switchboard_revision != snapshot.snapshot.switchboard_revision
            || challenge.remote_revision != snapshot.snapshot.remote_revision
            || challenge.lease_id != lease.claims.lease_id
            || challenge.fence != lease.claims.fence
        {
            client.pending_end_caller = None;
            return Err("the caller-ending confirmation is stale against live authority".into());
        }
        let request_id = state.v2.next_id("end_confirm");
        let device_id = client
            .device_id
            .clone()
            .ok_or("v2 device identity is unavailable")?;
        let frame = MobileEndCallerConfirmFrame {
            kind: "end_caller_confirm".into(),
            schema_version: SCHEMA_VERSION,
            app_id: challenge.app_id.clone(),
            request_id: request_id.clone(),
            idempotency_key: format!("mobile:{device_id}:{request_id}"),
            confirmation_id: challenge.confirmation_id.clone(),
            nonce: challenge.nonce.clone(),
            lease_token: lease.token.clone(),
            call_id: challenge.call_id.clone(),
            call_epoch: challenge.call_epoch,
            owner_epoch: challenge.owner_epoch,
            switchboard_revision: challenge.switchboard_revision,
            remote_revision: challenge.remote_revision,
            fence: challenge.fence,
        };
        frame.validate().map_err(|error| error.to_string())?;
        client.pending_end_caller = Some(PendingEndCaller::AwaitingSubmission {
            request_id: request_id.clone(),
            challenge: challenge.clone(),
        });
        (frame, request_id, challenge)
    };
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode caller-ending confirmation".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        let mut client = state.v2.inner.lock().await;
        if client
            .pending_end_caller
            .as_ref()
            .is_some_and(|pending| pending.request_id() == request_id)
        {
            client.pending_end_caller = Some(PendingEndCaller::AwaitingConfirmation { challenge });
        }
        return Err(error);
    }
    Ok(V2RequestReceipt { request_id })
}

fn current_microphone_mute_authority(
    client: &ClientState,
    now: u64,
) -> Result<(MobileSnapshotFrame, ClientLease), String> {
    if client.generation == 0 {
        return Err("protocol-v2 realtime is offline".into());
    }
    if client.pending_revoke.is_some() {
        return Err("the media lease is already returning to Aokie".into());
    }
    let snapshot = client
        .snapshot
        .as_ref()
        .ok_or("protocol-v2 authoritative state is unavailable")?;
    let lease = client
        .lease
        .as_ref()
        .ok_or("an active Companion media lease is required")?;
    if lease.claims.phase != LeasePhase::Active
        || !matches!(lease.claims.mode, LeaseMode::Takeover | LeaseMode::Consult)
        || lease.claims.expires_at <= now
        || lease.session.expires_at <= Utc::now()
    {
        return Err("the current Companion media lease cannot change microphone authority".into());
    }
    if !grants_permit_lease_mode(current_client_grants(client), lease.claims.mode)
        || snapshot.app_id != lease.claims.app_id
        || snapshot.snapshot.call_id != lease.claims.call_id
        || snapshot.snapshot.call_epoch != lease.claims.call_epoch
        || snapshot.snapshot.owner_epoch != lease.claims.owner_epoch
        || snapshot.snapshot.telephony_state != TelephonyState::Active
        || matches!(
            snapshot.snapshot.media_state,
            MediaState::None | MediaState::Failed
        )
        || !current_consent_permits_mode(&snapshot.snapshot.remote_consent, lease.claims.mode, now)
        || !matches!(
            (lease.claims.mode, snapshot.snapshot.service_mode.clone()),
            (LeaseMode::Takeover, ServiceMode::HumanActive)
                | (LeaseMode::Consult, ServiceMode::ConsultActive)
        )
    {
        return Err("the active media lease no longer matches authoritative call state".into());
    }
    Ok((snapshot.clone(), lease.clone()))
}

fn pending_microphone_mute(
    request_id: String,
    muted: bool,
    stage: PendingMicrophoneMuteStage,
    snapshot: &MobileSnapshotFrame,
    lease: &ClientLease,
) -> PendingMicrophoneMute {
    PendingMicrophoneMute {
        request_id,
        lease_id: lease.claims.lease_id.clone(),
        lease_jti: lease.claims.jti.clone(),
        rtc_session_id: lease.claims.rtc_session_id.clone(),
        call_id: lease.claims.call_id.clone(),
        call_epoch: lease.claims.call_epoch,
        owner_epoch: lease.claims.owner_epoch,
        switchboard_revision: snapshot.snapshot.switchboard_revision,
        remote_revision: snapshot.snapshot.remote_revision,
        fence: lease.claims.fence,
        muted,
        session: lease.session.clone(),
        stage,
    }
}

fn microphone_mute_frame(
    pending: &PendingMicrophoneMute,
    app_id: String,
    device_id: &str,
    lease_token: String,
) -> Result<MobileMicrophoneMuteFrame, String> {
    let frame = MobileMicrophoneMuteFrame {
        kind: "microphone_mute".into(),
        schema_version: SCHEMA_VERSION,
        app_id,
        request_id: pending.request_id.clone(),
        idempotency_key: format!("mobile:{device_id}:{}", pending.request_id),
        lease_token,
        rtc_session_id: pending.rtc_session_id.clone(),
        call_id: pending.call_id.clone(),
        call_epoch: pending.call_epoch,
        owner_epoch: pending.owner_epoch,
        switchboard_revision: pending.switchboard_revision,
        remote_revision: pending.remote_revision,
        fence: pending.fence,
        muted: pending.muted,
    };
    frame.validate().map_err(|error| error.to_string())?;
    Ok(frame)
}

async fn clear_pending_microphone_mute(state: &V2State, request_id: &str) {
    let mut client = state.inner.lock().await;
    if client
        .pending_microphone_mute
        .as_ref()
        .is_some_and(|pending| pending.request_id == request_id)
    {
        client.pending_microphone_mute = None;
    }
}

#[tauri::command]
pub async fn realtime_v2_set_microphone_muted(
    app: AppHandle,
    state: State<'_, RealtimeState>,
    media_state: State<'_, NativeMediaState>,
    request: MicrophoneMuteRequest,
) -> Result<V2RequestReceipt, String> {
    let (request_id, mut frame, initial_session, initial_remote_revision) = {
        let mut client = state.v2.inner.lock().await;
        if client.pending_microphone_mute.is_some() {
            return Err(
                "a microphone authority change is already awaiting Desktop confirmation".into(),
            );
        }
        let (snapshot, lease) = current_microphone_mute_authority(&client, unix_now()?)?;
        if snapshot.snapshot.companion_microphone_muted == request.muted {
            return Err(if request.muted {
                "the Desktop microphone gate is already muted"
            } else {
                "the Desktop microphone gate is already unmuted"
            }
            .into());
        }
        let request_id = state.v2.next_id("microphone_mute");
        let stage = if request.muted {
            PendingMicrophoneMuteStage::AwaitingStatus
        } else {
            PendingMicrophoneMuteStage::ArmingNative
        };
        let pending =
            pending_microphone_mute(request_id.clone(), request.muted, stage, &snapshot, &lease);
        let frame = request
            .muted
            .then(|| {
                microphone_mute_frame(
                    &pending,
                    snapshot.app_id.clone(),
                    &lease.claims.device_id,
                    lease.token.clone(),
                )
            })
            .transpose()?;
        let initial_session = lease.session.clone();
        let initial_remote_revision = snapshot.snapshot.remote_revision;
        client.pending_microphone_mute = Some(pending);
        (request_id, frame, initial_session, initial_remote_revision)
    };

    if !request.muted {
        let arm_result = async {
            media::arm_microphone(
                &app,
                media_state.inner(),
                SessionRequest {
                    session: initial_session.clone(),
                },
            )
            .await?;
            media::await_microphone_proof(
                media_state.inner(),
                &initial_session,
                Duration::from_secs(1),
            )
            .await
        }
        .await;
        if let Err(error) = arm_result {
            clear_pending_microphone_mute(&state.v2, &request_id).await;
            let _ = media::disarm_microphone(
                &app,
                media_state.inner(),
                SessionRequest {
                    session: initial_session,
                },
            )
            .await;
            return Err(error);
        }

        let built_frame = async {
            let mut client = state.v2.inner.lock().await;
            let (snapshot, lease) = current_microphone_mute_authority(&client, unix_now()?)?;
            let pending = client
                .pending_microphone_mute
                .as_mut()
                .filter(|pending| {
                    pending.request_id == request_id
                        && pending.stage == PendingMicrophoneMuteStage::ArmingNative
                        && !pending.muted
                })
                .ok_or("the microphone unmute transaction is no longer current")?;
            if !snapshot.snapshot.companion_microphone_muted
                || lease.session != initial_session
                || pending.call_id != lease.claims.call_id
                || pending.call_epoch != lease.claims.call_epoch
                || pending.owner_epoch != lease.claims.owner_epoch
                || pending.fence != lease.claims.fence
            {
                return Err("microphone authority changed while native capture was proving".into());
            }
            *pending = pending_microphone_mute(
                request_id.clone(),
                false,
                PendingMicrophoneMuteStage::AwaitingStatus,
                &snapshot,
                &lease,
            );
            microphone_mute_frame(
                pending,
                snapshot.app_id,
                &lease.claims.device_id,
                lease.token,
            )
        }
        .await;
        match built_frame {
            Ok(built) => frame = Some(built),
            Err(error) => {
                clear_pending_microphone_mute(&state.v2, &request_id).await;
                let _ = media::disarm_microphone(
                    &app,
                    media_state.inner(),
                    SessionRequest {
                        session: initial_session,
                    },
                )
                .await;
                return Err(error);
            }
        }
    }

    let encoded = serde_json::to_string(frame.as_ref().expect("mute frame was built"))
        .map_err(|_| "could not encode microphone authority request".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        clear_pending_microphone_mute(&state.v2, &request_id).await;
        if !request.muted {
            let _ = media::disarm_microphone(
                &app,
                media_state.inner(),
                SessionRequest {
                    session: initial_session,
                },
            )
            .await;
        }
        return Err(error);
    }

    let deadline = Instant::now() + MICROPHONE_MUTE_CONFIRM_TIMEOUT;
    loop {
        let result = {
            let client = state.v2.inner.lock().await;
            let pending_current = client
                .pending_microphone_mute
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id);
            match current_microphone_mute_authority(&client, unix_now()?) {
                Ok((snapshot, lease))
                    if snapshot.snapshot.companion_microphone_muted == request.muted
                        && snapshot.snapshot.remote_revision > initial_remote_revision
                        && !pending_current =>
                {
                    Some(Ok(lease.session))
                }
                Ok(_) if !pending_current => Some(Err(
                    "Desktop did not confirm the requested microphone authority".into(),
                )),
                Err(error) if !pending_current => Some(Err(error)),
                _ => None,
            }
        };
        if let Some(result) = result {
            match result {
                Ok(session) => {
                    if request.muted {
                        media::disarm_microphone(
                            &app,
                            media_state.inner(),
                            SessionRequest { session },
                        )
                        .await?;
                    }
                    return Ok(V2RequestReceipt { request_id });
                }
                Err(error) => {
                    if !request.muted {
                        let _ = media::disarm_microphone(
                            &app,
                            media_state.inner(),
                            SessionRequest {
                                session: initial_session,
                            },
                        )
                        .await;
                    }
                    return Err(error);
                }
            }
        }
        if Instant::now() >= deadline {
            let safely_muted_session = {
                let mut client = state.v2.inner.lock().await;
                let authority_now = unix_now().ok();
                let authoritative = client
                    .pending_microphone_mute
                    .as_ref()
                    .filter(|pending| pending.request_id == request_id && pending.muted)
                    .and_then(|_| {
                        authority_now
                            .and_then(|now| current_microphone_mute_authority(&client, now).ok())
                    })
                    .filter(|(snapshot, _)| {
                        snapshot.snapshot.companion_microphone_muted
                            && snapshot.snapshot.remote_revision > initial_remote_revision
                    })
                    .map(|(_, lease)| lease.session);
                if client
                    .pending_microphone_mute
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
                {
                    client.pending_microphone_mute = None;
                }
                authoritative
            };
            if let Some(session) = safely_muted_session {
                let _ =
                    media::disarm_microphone(&app, media_state.inner(), SessionRequest { session })
                        .await;
                return Err(
                    "Desktop mute is authoritative and the native microphone was safely disarmed, but its targeted confirmation timed out"
                        .into(),
                );
            }
            // A timeout can never leave native capture running after the user
            // explicitly asked to mute. Desktop authority may still be
            // unresolved, so distinguish that fail-closed local state from an
            // authoritative mute instead of claiming convergence.
            let _ = media::disarm_microphone(
                &app,
                media_state.inner(),
                SessionRequest {
                    session: initial_session,
                },
            )
            .await;
            return Err(if request.muted {
                "Desktop mute confirmation timed out; the handset microphone was locally disarmed for privacy"
            } else {
                "Desktop microphone authority confirmation timed out; the handset microphone was safely disarmed"
            }
            .into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tauri::command]
pub async fn realtime_v2_answer_assistance(
    state: State<'_, RealtimeState>,
    request: AssistanceAnswerRequest,
) -> Result<V2AssistanceReceipt, String> {
    let request_id = request.request_id.clone();
    let (frame, answer_id) = prepare_assistance_answer(&state.v2, &request, None).await?;
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode protocol-v2 assistance answer".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        clear_pending_assistance_answer(&state.v2, &answer_id).await;
        return Err(error);
    }
    Ok(V2AssistanceReceipt {
        request_id,
        answer_id,
    })
}

async fn prepare_assistance_answer(
    state: &V2State,
    request: &AssistanceAnswerRequest,
    native_action: Option<&NativeCallAction>,
) -> Result<(MobileAssistanceAnswerFrame, String), String> {
    let answer = request.answer.trim();
    let (frame, answer_id) = {
        let mut client = state.inner.lock().await;
        if client.generation == 0 {
            return Err("protocol-v2 realtime is offline".into());
        }
        let now = unix_now()?;
        if discard_expired_assistance(&mut client, now, None) {
            return Err("the assistance request is stale or expired".into());
        }
        if client.pending_assistance_answer.is_some() {
            return Err("an assistance answer is already awaiting acknowledgement".into());
        }
        let assistance = client
            .assistance
            .as_ref()
            .ok_or("the assistance request is no longer active")?;
        if assistance.request_id != request.request_id {
            return Err("the assistance request is stale or expired".into());
        }
        if request.response_action == AssistanceResponseAction::Decline
            && !assistance.transfer_offered
        {
            return Err("only an active transfer offer can be declined".into());
        }
        let snapshot = client
            .snapshot
            .as_ref()
            .cloned()
            .ok_or("protocol-v2 authoritative state is unavailable")?;
        if !snapshot.grants.contains(&Grant::AssistanceRespond)
            || !snapshot.snapshot.remote_consent.enabled
            || !snapshot.snapshot.remote_consent.acknowledged
            || !snapshot.snapshot.remote_consent.assistance_enabled
        {
            return Err("current admission or consent does not allow assistance answers".into());
        }
        if snapshot.snapshot.call_id != assistance.call_id
            || snapshot.snapshot.call_epoch != assistance.call_epoch
            || snapshot.snapshot.owner_epoch != assistance.owner_epoch
            || snapshot.snapshot.switchboard_revision != assistance.switchboard_revision
            || snapshot.snapshot.remote_revision != assistance.remote_revision
        {
            return Err("assistance request no longer matches current call revisions".into());
        }
        let device_id = client
            .device_id
            .clone()
            .ok_or("v2 device identity is unavailable")?;
        let selected_native_offer = if let Some(action) = native_action {
            validate_native_action(action, now)?;
            let transfer_request_id = action
                .accepted_transfer_request_id
                .as_deref()
                .ok_or("native transfer decline has no exact request binding")?;
            if action.kind != NativeCallActionKind::Decline
                || request.response_action != AssistanceResponseAction::Decline
                || request.request_id != transfer_request_id
                || action.response_text.as_deref() != Some(answer)
            {
                return Err(
                    "native transfer decline does not match its authenticated action".into(),
                );
            }
            Some(select_mobile_offer(
                &client,
                &snapshot,
                LeaseMode::Takeover,
                Some(action),
                Some(transfer_request_id),
                now,
            )?)
        } else {
            None
        };
        let answer_id = state.next_id("answer");
        let frame = MobileAssistanceAnswerFrame {
            kind: "assistance_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: assistance.app_id.clone(),
            request_id: assistance.request_id.clone(),
            idempotency_key: format!("mobile:{device_id}:assistance:{answer_id}"),
            answer_id: answer_id.clone(),
            call_id: assistance.call_id.clone(),
            call_epoch: assistance.call_epoch,
            owner_epoch: assistance.owner_epoch,
            switchboard_revision: assistance.switchboard_revision,
            remote_revision: assistance.remote_revision,
            response_action: request.response_action,
            answer: answer.to_owned(),
        };
        frame.validate().map_err(|error| error.to_string())?;
        if let Some(selected) = selected_native_offer.as_ref() {
            tombstone_mobile_offer(&mut client, selected, None);
        }
        client.pending_assistance_answer = Some(answer_id.clone());
        (frame, answer_id)
    };
    Ok((frame, answer_id))
}

async fn clear_pending_assistance_answer(state: &V2State, answer_id: &str) {
    let mut client = state.inner.lock().await;
    if client.pending_assistance_answer.as_deref() == Some(answer_id) {
        client.pending_assistance_answer = None;
    }
}

#[tauri::command]
pub async fn realtime_v2_revoke_lease(
    app: AppHandle,
    state: State<'_, RealtimeState>,
    media_state: State<'_, NativeMediaState>,
    reason: String,
) -> Result<V2RequestReceipt, String> {
    let frame = prepare_revoke(&app, &state.v2, media_state.inner(), reason, None).await?;
    let request_id = frame.request_id.clone();
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode protocol-v2 lease revoke".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        abandon_pending_revoke(&state.v2, &request_id).await;
        return Err(error);
    }
    Ok(V2RequestReceipt { request_id })
}

async fn prepare_revoke(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    reason: String,
    native_action_id: Option<String>,
) -> Result<LeaseRevokeFrame, String> {
    let is_native_return = native_action_id.is_some();
    let (frame, lease) = {
        let mut client = state.inner.lock().await;
        if client.pending_revoke.is_some() {
            return Err(
                "a media lease return is already awaiting authoritative confirmation".into(),
            );
        }
        if client.pending_native_hangup.is_some() {
            return Err(
                "caller hang-up confirmation must complete before returning the lease".into(),
            );
        }
        match (
            native_action_id.as_deref(),
            client.pending_native_end.as_ref(),
        ) {
            (Some(action_id), Some(pending)) if pending.action.action_id == action_id => {}
            (Some(_), _) => return Err("native lease return is no longer current".into()),
            (None, Some(_)) => return Err("Core-Telecom lease return must complete first".into()),
            (None, None) => {}
        }
        let lease = client
            .lease
            .take()
            .ok_or("there is no active media lease")?;
        client.pending_end_caller = None;
        client.pending_microphone_mute = None;
        let request_id = state.next_id("request");
        let device_id = client
            .device_id
            .clone()
            .ok_or("v2 device identity is unavailable")?;
        let app_id = client
            .app_id
            .clone()
            .ok_or("v2 app identity is unavailable")?;
        let frame = LeaseRevokeFrame {
            kind: "lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id,
            request_id: request_id.clone(),
            idempotency_key: format!("mobile:{device_id}:{}", state.next_id("revoke")),
            lease_token: lease.token.clone(),
            reason,
        };
        frame.validate().map_err(|error| error.to_string())?;
        let authoritative_sequence = client.authoritative_sequence;
        let authoritative_remote_revision = client
            .snapshot
            .as_ref()
            .filter(|snapshot| {
                snapshot.app_id == lease.claims.app_id
                    && snapshot.snapshot.call_id == lease.claims.call_id
                    && snapshot.snapshot.call_epoch == lease.claims.call_epoch
            })
            .map(|snapshot| snapshot.snapshot.remote_revision);
        client.pending_revoke = Some(PendingRevoke {
            request_id,
            lease_id: lease.claims.lease_id.clone(),
            lease_jti: lease.claims.jti.clone(),
            lease: lease.clone(),
            authoritative_sequence,
            authoritative_remote_revision,
            native_action_id,
            deadline: Instant::now() + REVOKE_CONFIRM_TIMEOUT,
        });
        if is_native_return {
            client.pending_native_end = None;
        }
        (frame, lease)
    };

    // Local audio closes first. Until an exact gateway acknowledgement or a
    // strictly newer authoritative return projection is observed, the lease
    // remains fenced in pending_revoke but can no longer route microphone or
    // caller audio locally.
    let _ = media::revoke(
        app,
        media_state,
        RevokeRequest {
            session: lease.session,
            reason: Some(frame.reason.clone()),
        },
    )
    .await;
    emit_lease_reset(app);
    Ok(frame)
}

async fn abandon_pending_revoke(state: &V2State, request_id: &str) -> Option<String> {
    let mut client = state.inner.lock().await;
    if client
        .pending_revoke
        .as_ref()
        .is_some_and(|pending| pending.request_id == request_id)
    {
        return client
            .pending_revoke
            .take()
            .and_then(|pending| pending.native_action_id);
    }
    None
}

async fn poll_native_call_actions(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
) -> Result<Option<NativeOutbound>, String> {
    expire_native_call_actions(app, state).await;
    if let Some(outbound) = take_ready_lease_request(state).await? {
        return Ok(Some(outbound));
    }
    if let Some(outbound) = progress_native_hangup(app, state).await? {
        return Ok(Some(outbound));
    }
    if let Some(outbound) = progress_native_end(app, state, media_state).await? {
        return Ok(Some(outbound));
    }

    let Some(encoded) = crate::android_runtime::take_native_call_action(app).await? else {
        return Ok(None);
    };
    if encoded.len() > 4 * 1024 {
        return Err("Android native-call action exceeded its size limit".into());
    }
    let action: NativeCallAction = serde_json::from_str(&encoded)
        .map_err(|_| "Android native-call action was malformed".to_string())?;
    if let Err(message) = validate_native_action(&action, unix_now()?) {
        let _ = crate::android_runtime::complete_native_call_action(
            app,
            &action.action_id,
            false,
            "native_action_rejected",
        )
        .await;
        return Err(message);
    }

    let action_id = action.action_id.clone();
    let result: Result<Option<NativeOutbound>, String> = match action.kind {
        NativeCallActionKind::Answer => {
            match prepare_offer_answer_for_transfer(
                state,
                LeaseMode::Takeover,
                Some(&action),
                action.accepted_transfer_request_id.as_deref(),
            )
            .await
            {
                Ok((frame, request_id)) => serde_json::to_string(&frame)
                    .map(|encoded| {
                        Some(NativeOutbound {
                            encoded,
                            // Core-Telecom is completed only after the
                            // accepted offer advances to an actual lease request.
                            answer_action_id: None,
                            answer_request_id: Some(request_id),
                            assistance_answer_id: None,
                            revoke_request_id: None,
                        })
                    })
                    .map_err(|_| "could not encode native Answer lease request".to_string()),
                Err(message) => Err(message),
            }
        }
        NativeCallActionKind::Decline => {
            let request_id = action
                .accepted_transfer_request_id
                .clone()
                .ok_or("native transfer decline has no exact request binding")?;
            let answer = action
                .response_text
                .clone()
                .ok_or("native transfer decline has no private response")?;
            let request = AssistanceAnswerRequest {
                request_id,
                response_action: AssistanceResponseAction::Decline,
                answer,
            };
            match prepare_assistance_answer(state, &request, Some(&action)).await {
                Ok((frame, answer_id)) => serde_json::to_string(&frame)
                    .map(|encoded| {
                        Some(NativeOutbound {
                            encoded,
                            answer_action_id: Some(action.action_id.clone()),
                            answer_request_id: None,
                            assistance_answer_id: Some(answer_id),
                            revoke_request_id: None,
                        })
                    })
                    .map_err(|_| "could not encode native transfer decline response".to_string()),
                Err(message) => Err(message),
            }
        }
        NativeCallActionKind::End => match queue_native_end(state, action).await {
            Ok(()) => progress_native_end(app, state, media_state).await,
            Err(message) => Err(message),
        },
        NativeCallActionKind::Hangup => queue_native_hangup(state, action).await.map(Some),
    };
    if let Err(message) = &result {
        let _ = crate::android_runtime::complete_native_call_action(
            app,
            &action_id,
            false,
            "native_action_rejected",
        )
        .await;
        emit_error(app, message);
    }
    result
}

async fn take_ready_lease_request(state: &V2State) -> Result<Option<NativeOutbound>, String> {
    let mut client = state.inner.lock().await;
    let Some(pending) = client.pending.as_mut() else {
        return Ok(None);
    };
    if pending.stage != PendingLeaseStage::ReadyForLeaseDelivery {
        return Ok(None);
    }
    pending
        .lease_frame
        .validate()
        .map_err(|error| error.to_string())?;
    let encoded = serde_json::to_string(&pending.lease_frame)
        .map_err(|_| "could not encode accepted mobile-offer lease request".to_string())?;
    pending.stage = PendingLeaseStage::LeaseRequested;
    Ok(Some(NativeOutbound {
        encoded,
        answer_action_id: pending.native_action_id.clone(),
        answer_request_id: Some(pending.request_id.clone()),
        assistance_answer_id: None,
        revoke_request_id: None,
    }))
}

async fn mark_native_answer_delivered(state: &V2State, request_id: &str, action_id: &str) {
    let mut client = state.inner.lock().await;
    if let Some(pending) = client.pending.as_mut().filter(|pending| {
        pending.request_id == request_id
            && pending.native_action_id.as_deref() == Some(action_id)
            && pending.stage == PendingLeaseStage::LeaseRequested
    }) {
        pending.native_action_id = None;
        pending.native_deadline = None;
    }
}

async fn queue_native_hangup(
    state: &V2State,
    action: NativeCallAction,
) -> Result<NativeOutbound, String> {
    let mut client = state.inner.lock().await;
    if client.pending_native_hangup.is_some()
        || client.pending_end_caller.is_some()
        || client.pending_native_end.is_some()
        || client.pending_revoke.is_some()
    {
        return Err("a caller-ending or lease-return operation is already pending".into());
    }
    validate_local_end_caller(&client, unix_now()?)?;
    let snapshot = client.snapshot.as_ref().expect("validated snapshot");
    let lease = client.lease.as_ref().expect("validated lease");
    if action.kind != NativeCallActionKind::Hangup
        || client.app_id.as_deref() != Some(action.app_id.as_str())
        || snapshot.app_id != action.app_id
        || snapshot.snapshot.call_id != action.call_id
        || snapshot.snapshot.call_epoch != action.call_epoch
        || snapshot.snapshot.owner_epoch != action.owner_epoch
        || lease.claims.app_id != action.app_id
        || lease.claims.call_id != action.call_id
        || lease.claims.call_epoch != action.call_epoch
        || lease.claims.owner_epoch != action.owner_epoch
    {
        return Err("native Hangup does not match the exact active takeover owner".into());
    }
    let request_id = state.next_id("end_prepare");
    let device_id = client
        .device_id
        .clone()
        .ok_or("v2 device identity is unavailable")?;
    let frame = MobileEndCallerChallengeRequestFrame {
        kind: "end_caller_challenge_request".into(),
        schema_version: SCHEMA_VERSION,
        app_id: snapshot.app_id.clone(),
        request_id: request_id.clone(),
        idempotency_key: format!("mobile:{device_id}:{request_id}"),
        lease_token: lease.token.clone(),
        call_id: lease.claims.call_id.clone(),
        call_epoch: lease.claims.call_epoch,
        owner_epoch: lease.claims.owner_epoch,
        switchboard_revision: snapshot.snapshot.switchboard_revision,
        remote_revision: snapshot.snapshot.remote_revision,
        fence: lease.claims.fence,
    };
    frame.validate().map_err(|error| error.to_string())?;
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode native caller-ending challenge request".to_string())?;
    client.pending_end_caller = Some(PendingEndCaller::AwaitingChallenge { request_id });
    client.pending_native_hangup = Some(PendingNativeHangup {
        action_id: action.action_id,
        deadline: Instant::now() + NATIVE_HANGUP_TIMEOUT,
    });
    Ok(NativeOutbound {
        encoded,
        answer_action_id: None,
        answer_request_id: None,
        assistance_answer_id: None,
        revoke_request_id: None,
    })
}

fn native_hangup_confirmation_frame(
    state: &V2State,
    client: &ClientState,
    challenge: &EndCallerChallengeFrame,
    now: u64,
) -> Result<MobileEndCallerConfirmFrame, String> {
    validate_local_end_caller(client, now)?;
    if challenge.expires_at <= now {
        return Err("the caller-ending confirmation expired".into());
    }
    let snapshot = client.snapshot.as_ref().expect("validated snapshot");
    let lease = client.lease.as_ref().expect("validated lease");
    if challenge.device_id != client.device_id.as_deref().unwrap_or_default()
        || challenge.call_id != lease.claims.call_id
        || challenge.call_epoch != lease.claims.call_epoch
        || challenge.owner_epoch != lease.claims.owner_epoch
        || challenge.switchboard_revision != snapshot.snapshot.switchboard_revision
        || challenge.remote_revision != snapshot.snapshot.remote_revision
        || challenge.lease_id != lease.claims.lease_id
        || challenge.fence != lease.claims.fence
    {
        return Err("the caller-ending confirmation is stale against live authority".into());
    }
    let request_id = state.next_id("end_confirm");
    let device_id = client
        .device_id
        .clone()
        .ok_or("v2 device identity is unavailable")?;
    let frame = MobileEndCallerConfirmFrame {
        kind: "end_caller_confirm".into(),
        schema_version: SCHEMA_VERSION,
        app_id: challenge.app_id.clone(),
        request_id: request_id.clone(),
        idempotency_key: format!("mobile:{device_id}:{request_id}"),
        confirmation_id: challenge.confirmation_id.clone(),
        nonce: challenge.nonce.clone(),
        lease_token: lease.token.clone(),
        call_id: challenge.call_id.clone(),
        call_epoch: challenge.call_epoch,
        owner_epoch: challenge.owner_epoch,
        switchboard_revision: challenge.switchboard_revision,
        remote_revision: challenge.remote_revision,
        fence: challenge.fence,
    };
    frame.validate().map_err(|error| error.to_string())?;
    Ok(frame)
}

async fn progress_native_hangup(
    app: &AppHandle,
    state: &V2State,
) -> Result<Option<NativeOutbound>, String> {
    enum Decision {
        Wait,
        Send(MobileEndCallerConfirmFrame),
        Reject(String, String),
    }

    let decision = {
        let mut client = state.inner.lock().await;
        let Some(native) = client.pending_native_hangup.clone() else {
            return Ok(None);
        };
        match client.pending_end_caller.clone() {
            Some(PendingEndCaller::AwaitingChallenge { .. })
            | Some(PendingEndCaller::AwaitingSubmission { .. })
            | Some(PendingEndCaller::AwaitingResult { .. }) => Decision::Wait,
            Some(PendingEndCaller::AwaitingConfirmation { challenge }) => {
                let result =
                    native_hangup_confirmation_frame(state, &client, &challenge, unix_now()?);
                match result {
                    Ok(frame) => {
                        client.pending_end_caller = Some(PendingEndCaller::AwaitingSubmission {
                            request_id: frame.request_id.clone(),
                            challenge,
                        });
                        Decision::Send(frame)
                    }
                    Err(message) => {
                        client.pending_end_caller = None;
                        client.pending_native_hangup = None;
                        Decision::Reject(native.action_id, message)
                    }
                }
            }
            None => {
                client.pending_native_hangup = None;
                Decision::Reject(
                    native.action_id,
                    "caller-ending challenge state was lost".into(),
                )
            }
        }
    };

    match decision {
        Decision::Wait => Ok(None),
        Decision::Send(frame) => Ok(Some(NativeOutbound {
            encoded: serde_json::to_string(&frame)
                .map_err(|_| "could not encode native caller-ending confirmation".to_string())?,
            answer_action_id: None,
            answer_request_id: None,
            assistance_answer_id: None,
            revoke_request_id: None,
        })),
        Decision::Reject(action_id, message) => reject_native_hangup(app, action_id, message).await,
    }
}

async fn reject_native_hangup(
    app: &AppHandle,
    action_id: String,
    message: String,
) -> Result<Option<NativeOutbound>, String> {
    let _ = crate::android_runtime::complete_native_call_action(
        app,
        &action_id,
        false,
        "caller_end_unconfirmed",
    )
    .await;
    Err(message)
}

async fn queue_native_end(state: &V2State, action: NativeCallAction) -> Result<(), String> {
    let mut client = state.inner.lock().await;
    if client.pending_native_end.is_some() || client.pending_revoke.is_some() {
        return Err("a Core-Telecom lease return is already pending".into());
    }
    let snapshot = client
        .snapshot
        .as_ref()
        .ok_or("protocol-v2 authoritative state is unavailable")?;
    if action.kind != NativeCallActionKind::End
        || client.app_id.as_deref() != Some(action.app_id.as_str())
        || snapshot.app_id != action.app_id
        || snapshot.snapshot.call_id != action.call_id
        || snapshot.snapshot.call_epoch != action.call_epoch
        || action.owner_epoch > snapshot.snapshot.owner_epoch
    {
        return Err("native End does not match authoritative call state".into());
    }
    if let Some(lease) = &client.lease {
        validate_native_end_lease(&action, lease)?;
    }
    if let Some(pending) = &client.pending {
        if pending.call_id != action.call_id
            || pending.call_epoch != action.call_epoch
            || pending.owner_epoch != action.owner_epoch
            || pending.mode != LeaseMode::Takeover
        {
            return Err("native End crossed a pending lease fence".into());
        }
    }
    client.pending_native_end = Some(PendingNativeEnd {
        action,
        deadline: Instant::now() + NATIVE_ACTION_TIMEOUT,
    });
    Ok(())
}

fn validate_native_end_lease(action: &NativeCallAction, lease: &ClientLease) -> Result<(), String> {
    if lease.claims.app_id != action.app_id
        || lease.claims.call_id != action.call_id
        || lease.claims.call_epoch != action.call_epoch
        || lease.claims.mode != LeaseMode::Takeover
    {
        return Err("native End did not match the active takeover lease".into());
    }
    Ok(())
}

async fn progress_native_end(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
) -> Result<Option<NativeOutbound>, String> {
    enum Decision {
        Wait,
        NoLease(String),
        Revoke(String),
        Reject(String),
    }

    let decision = {
        let mut client = state.inner.lock().await;
        let Some(pending_end) = client.pending_native_end.as_ref() else {
            return Ok(None);
        };
        let action_id = pending_end.action.action_id.clone();
        if let Some(lease) = &client.lease {
            if validate_native_end_lease(&pending_end.action, lease).is_ok() {
                Decision::Revoke(action_id)
            } else {
                client.pending_native_end = None;
                Decision::Reject(action_id)
            }
        } else if client.pending.as_ref().is_some_and(|pending| {
            pending.call_id == pending_end.action.call_id
                && pending.call_epoch == pending_end.action.call_epoch
                && pending.mode == LeaseMode::Takeover
        }) {
            Decision::Wait
        } else {
            client.pending_native_end = None;
            Decision::NoLease(action_id)
        }
    };

    match decision {
        Decision::Wait => Ok(None),
        Decision::NoLease(action_id) => {
            crate::android_runtime::complete_native_call_action(
                app,
                &action_id,
                true,
                "no_active_lease",
            )
            .await?;
            Ok(None)
        }
        Decision::Reject(action_id) => {
            let _ = crate::android_runtime::complete_native_call_action(
                app,
                &action_id,
                false,
                "lease_fence_mismatch",
            )
            .await;
            Err("native End crossed the active lease fence".into())
        }
        Decision::Revoke(action_id) => {
            let frame = prepare_revoke(
                app,
                state,
                media_state,
                "core_telecom_end".into(),
                Some(action_id),
            )
            .await?;
            Ok(Some(NativeOutbound {
                encoded: serde_json::to_string(&frame)
                    .map_err(|_| "could not encode Core-Telecom lease return".to_string())?,
                answer_action_id: None,
                answer_request_id: None,
                assistance_answer_id: None,
                revoke_request_id: Some(frame.request_id),
            }))
        }
    }
}

async fn expire_native_call_actions(app: &AppHandle, state: &V2State) {
    let (native_end, native_hangup, pending_revoke, pending_lease) = {
        let mut client = state.inner.lock().await;
        let now = Instant::now();
        purge_local_expiry_tombstones(&mut client, now);
        let native_end = if client
            .pending_native_end
            .as_ref()
            .is_some_and(|pending| pending.deadline <= now)
        {
            client
                .pending_native_end
                .take()
                .map(|pending| pending.action.action_id)
        } else {
            None
        };
        let native_hangup = if client
            .pending_native_hangup
            .as_ref()
            .is_some_and(|pending| pending.deadline <= now)
        {
            let action_id = client
                .pending_native_hangup
                .take()
                .map(|pending| pending.action_id);
            client.pending_end_caller = None;
            action_id
        } else {
            None
        };
        let pending_revoke = if client
            .pending_revoke
            .as_ref()
            .is_some_and(|pending| pending.deadline <= now)
        {
            client.pending_revoke.take()
        } else {
            None
        };
        let pending_timeout = client
            .pending
            .as_ref()
            .and_then(|pending| pending_lease_timeout(pending, now));
        let pending_lease = pending_timeout.and_then(|native_timeout| {
            client
                .pending
                .take()
                .map(|pending| (pending, native_timeout))
        });
        if let Some((pending, _)) = pending_lease.as_ref() {
            remember_terminal_offer_acceptance(&mut client, pending);
        }
        (native_end, native_hangup, pending_revoke, pending_lease)
    };

    if let Some(action_id) = native_hangup {
        let _ = crate::android_runtime::complete_native_call_action(
            app,
            &action_id,
            false,
            "caller_end_unconfirmed",
        )
        .await;
        emit_error(
            app,
            "Caller hang-up was not authoritatively confirmed; Companion remains on the live call",
        );
    }

    let timed_out_action = native_end.or_else(|| {
        pending_revoke
            .as_ref()
            .and_then(|pending| pending.native_action_id.clone())
    });
    if let Some(action_id) = timed_out_action.as_deref() {
        let _ = crate::android_runtime::complete_native_call_action(
            app,
            action_id,
            false,
            "lease_return_unconfirmed",
        )
        .await;
    }
    if pending_revoke.is_some() || timed_out_action.is_some() {
        emit_error(
            app,
            "Companion media was closed, but authoritative lease return was not confirmed",
        );
    }
    if let Some((pending, native_timeout)) = pending_lease {
        if let Some(action_id) = pending.native_action_id.as_deref() {
            let _ = crate::android_runtime::complete_native_call_action(
                app,
                action_id,
                false,
                "authoritative_answer_timed_out",
            )
            .await;
            let _ = crate::android_runtime::reconcile_offer(
                app,
                &pending.call_id,
                pending.call_epoch,
                "cancel",
                "authoritative_answer_timed_out",
            )
            .await;
        }
        emit_error(
            app,
            if native_timeout {
                "Core-Telecom Answer expired before a v2 lease request was admitted"
            } else {
                "The secure media request timed out before the gateway confirmed a lease. No microphone route was opened."
            },
        );
    }
}

async fn fail_native_call_actions_on_disconnect(app: &AppHandle, state: &V2State) {
    let (action_ids, native_call) = {
        let mut client = state.inner.lock().await;
        let mut action_ids = Vec::new();
        if let Some(pending) = client.pending_native_end.take() {
            action_ids.push(pending.action.action_id);
        }
        if let Some(pending) = client.pending_native_hangup.take() {
            action_ids.push(pending.action_id);
            client.pending_end_caller = None;
        }
        if let Some(pending) = client.pending_revoke.take() {
            if let Some(action_id) = pending.native_action_id {
                action_ids.push(action_id);
            }
        }
        if let Some(action_id) = client
            .pending
            .as_ref()
            .and_then(|pending| pending.native_action_id.clone())
        {
            action_ids.push(action_id);
        }
        let native_call = native_call_to_cancel_on_disconnect(&client);
        (action_ids, native_call)
    };
    for action_id in action_ids {
        let _ = crate::android_runtime::complete_native_call_action(
            app,
            &action_id,
            false,
            "realtime_interrupted",
        )
        .await;
    }
    if let Some((call_id, call_epoch)) = native_call {
        let _ = crate::android_runtime::reconcile_offer(
            app,
            &call_id,
            call_epoch,
            "cancel",
            "realtime_interrupted",
        )
        .await;
    }
}

struct ManagedTransportRotation {
    transport: V2Transport,
    admission: ManagedAdmission,
    peer_key_thumbprint: String,
    // WebSocket gateway replacement is authoritative only after its first
    // state frame. Relay refresh keeps the existing sole SSE reader, so fresh
    // plugin state arrives there in normal mailbox order after credential swap.
    initial_frames: Vec<String>,
}

type ManagedTransportRotationFuture = Pin<
    Box<
        dyn Future<Output = Result<ManagedTransportRotation, ManagedTransportRotationError>>
            + Send
            + 'static,
    >,
>;

async fn await_managed_transport_rotation(
    rotation: &mut Option<ManagedTransportRotationFuture>,
) -> Result<ManagedTransportRotation, ManagedTransportRotationError> {
    match rotation.as_mut() {
        Some(rotation) => rotation.await,
        None => pending().await,
    }
}

/// Everything the relay carrier needs, gathered while the admission is still in
/// hand.
///
/// A relay only ever accompanies a MANAGED admission, which is also the only
/// place `expected_peer_key_thumbprint` and the granted scopes exist — so a
/// custom profile structurally cannot take this path.
#[derive(Debug, Clone)]
struct RelayCarrierPlan {
    endpoints: crate::managed_auth::RelayEndpoints,
    access_token: String,
    grants: Vec<Grant>,
    expected_peer_key_thumbprint: String,
}

impl RelayCarrierPlan {
    fn from_admission(admission: &ManagedAdmission) -> Option<Self> {
        Some(Self {
            endpoints: admission.relay.clone()?,
            access_token: admission.access_token.clone(),
            grants: admission.grants.clone(),
            expected_peer_key_thumbprint: admission.expected_peer_key_thumbprint.clone(),
        })
    }
}

/// Why a carrier could not be opened.
///
/// Named rather than stringly-typed because the session treats a finished
/// admission differently from an unreachable endpoint, and that distinction has
/// to survive both carriers.
enum TransportOpenFailure {
    AdmissionRejected,
    Unavailable(String),
    TimedOut,
}

async fn open_relay_transport(
    plan: &RelayCarrierPlan,
    state: &V2State,
    app_id: &str,
    device_id: &str,
    for_refresh: bool,
) -> Result<V2Transport, TransportOpenFailure> {
    let connected = if for_refresh {
        crate::companion_relay::RelayChannel::connect_for_refresh(
            &plan.endpoints,
            &plan.access_token,
            app_id,
            device_id,
        )
        .await
    } else {
        crate::companion_relay::RelayChannel::connect(
            &plan.endpoints,
            &plan.access_token,
            app_id,
            device_id,
        )
        .await
    };
    let (channel, challenge) = connected.map_err(|error| {
        if error.admission_rejected {
            TransportOpenFailure::AdmissionRejected
        } else {
            TransportOpenFailure::Unavailable(error.message)
        }
    })?;
    let mut shim = GatewayShim::new(
        app_id.to_owned(),
        device_id.to_owned(),
        plan.grants.clone(),
        plan.expected_peer_key_thumbprint.clone(),
    );
    // Read from the LIVE session rather than assumed zero: a rotation does not
    // reset `authoritative_sequence`, so a carrier change would otherwise mint
    // sequences the session silently discards as stale. See `seed_sequence`.
    shim.seed_sequence(state.authoritative_sequence().await);
    Ok(V2Transport::Relay(RelayCarrier {
        channel,
        challenge: Some(challenge),
        shim,
    }))
}

enum ManagedTransportRotationError {
    Admission(ManagedAdmissionError),
    Transport(String),
}

#[allow(clippy::too_many_arguments)]
async fn open_overlapping_managed_transport(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    managed_auth: &ManagedAuthState,
    endpoint_identity: &crate::endpoint_identity::EndpointIdentity,
    peer_trust: &PeerTrustState,
    profile_id: &str,
    deployment_id: &str,
    app_id: &str,
    device_id: &str,
    session_nonce: &str,
    app_header: &HeaderValue,
    device_header: &HeaderValue,
    predecessor_fence_started: Arc<AtomicBool>,
) -> Result<ManagedTransportRotation, ManagedTransportRotationError> {
    let admission = managed_auth
        .admission(
            profile_id,
            deployment_id,
            app_id,
            device_id,
            endpoint_identity.thumbprint(),
        )
        .await
        .map_err(ManagedTransportRotationError::Admission)?;
    // A rotation stays on the carrier the refreshed admission calls for: the
    // relay when it advertises one, the signed gateway otherwise.
    if let Some(plan) = RelayCarrierPlan::from_admission(&admission) {
        let mut transport = open_relay_transport(&plan, state, app_id, device_id, true)
            .await
            .map_err(|failure| {
                ManagedTransportRotationError::Transport(match failure {
                    TransportOpenFailure::AdmissionRejected => {
                        "replacement protocol-v2 admission was rejected".into()
                    }
                    TransportOpenFailure::Unavailable(message) => message,
                    TransportOpenFailure::TimedOut => {
                        "replacement protocol-v2 connection attempt timed out".into()
                    }
                })
            })?;
        let peer_key_thumbprint = endpoint_handshake(
            app,
            state,
            endpoint_identity,
            peer_trust,
            profile_id,
            app_id,
            device_id,
            session_nonce,
            Some(&admission.expected_peer_key_thumbprint),
            Some(predecessor_fence_started.as_ref()),
            &mut transport,
        )
        .await
        .map_err(ManagedTransportRotationError::Transport)?;
        return Ok(ManagedTransportRotation {
            transport,
            admission,
            peer_key_thumbprint,
            initial_frames: Vec::new(),
        });
    }
    let gateway_url = managed_gateway_url(&admission.gateway_url)
        .map_err(ManagedTransportRotationError::Transport)?;
    let authorization =
        bearer_header(&admission.access_token).map_err(ManagedTransportRotationError::Transport)?;
    let mut request = gateway_url.as_str().into_client_request().map_err(|_| {
        ManagedTransportRotationError::Transport(
            "could not create replacement protocol-v2 realtime request".into(),
        )
    })?;
    request.headers_mut().insert("authorization", authorization);
    request
        .headers_mut()
        .insert("x-aokie-app-id", app_header.clone());
    request
        .headers_mut()
        .insert("x-aokie-device-id", device_header.clone());
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let socket = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_async_with_config(request, Some(websocket_config), false),
    )
    .await
    {
        Ok(Ok((socket, _))) => socket,
        Ok(Err(error)) => {
            let message = if matches!(
                &error,
                WebSocketError::Http(response)
                    if matches!(
                        response.status(),
                        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                    )
            ) {
                "replacement protocol-v2 admission was rejected"
            } else {
                "replacement protocol-v2 realtime endpoint is unavailable"
            };
            return Err(ManagedTransportRotationError::Transport(message.into()));
        }
        Err(_) => {
            return Err(ManagedTransportRotationError::Transport(
                "replacement protocol-v2 connection attempt timed out".into(),
            ));
        }
    };
    let mut transport = V2Transport::websocket(socket);
    let peer_key_thumbprint = endpoint_handshake(
        app,
        state,
        endpoint_identity,
        peer_trust,
        profile_id,
        app_id,
        device_id,
        session_nonce,
        Some(&admission.expected_peer_key_thumbprint),
        Some(predecessor_fence_started.as_ref()),
        &mut transport,
    )
    .await
    .map_err(ManagedTransportRotationError::Transport)?;
    let initial_frames = receive_replacement_sync(&mut transport, app, state, media_state, app_id)
        .await
        .map_err(ManagedTransportRotationError::Transport)?;
    Ok(ManagedTransportRotation {
        transport,
        admission,
        peer_key_thumbprint,
        initial_frames,
    })
}

pub(crate) fn spawn(
    app: AppHandle,
    connection: Arc<Mutex<ConnectionSlot>>,
    state: V2State,
    media_state: NativeMediaState,
    managed_auth: ManagedAuthState,
    peer_trust: PeerTrustState,
    config: RealtimeConfig,
) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
    let gateway_url = Url::parse(&config.gateway_url)
        .map_err(|_| "validated protocol-v2 gateway URL became invalid".to_string())?;
    let authorization = if config.managed_deployment_id.is_some() {
        HeaderValue::from_static("Bearer managed-native-admission")
    } else {
        bearer_header(&config.access_token)?
    };
    let app_header = HeaderValue::from_str(&config.app_id)
        .map_err(|_| "protocol-v2 app identity contains invalid bytes".to_string())?;
    let device_header = HeaderValue::from_str(&config.device_id)
        .map_err(|_| "protocol-v2 device identity contains invalid bytes".to_string())?;
    let profile_id = realtime_profile_id(&config)?;
    Ok(tauri::async_runtime::spawn(async move {
        let endpoint_identity = match crate::endpoint_identity::load_or_create(&app).await {
            Ok(identity) => identity,
            Err(message) => {
                emit_error(&app, &message);
                emit_transport(&app, TransportEvent::Transport { value: "offline" });
                return;
            }
        };
        let mut reconnect_attempt = 0_u32;
        let mut local_signals = media_state.subscribe_signals();
        loop {
            let status = if reconnect_attempt == 0 {
                "connecting"
            } else {
                "reconnecting"
            };
            emit_transport(&app, TransportEvent::Transport { value: status });
            let (
                attempt_url,
                attempt_authorization,
                attempt_ice_servers,
                attempt_relay_only,
                attempt_admission_grants,
                admission_expected_peer_key_thumbprint,
                admission_refresh_deadline,
                attempt_relay,
            ) = if let Some(deployment_id) = config.managed_deployment_id.as_deref() {
                match managed_auth
                    .admission(
                        &profile_id,
                        deployment_id,
                        &config.app_id,
                        &config.device_id,
                        endpoint_identity.thumbprint(),
                    )
                    .await
                {
                    Ok(admission) => {
                        let url = match managed_gateway_url(&admission.gateway_url) {
                            Ok(url) => url,
                            Err(message) => {
                                emit_error(&app, &message);
                                state.reset().await;
                                emit_transport(
                                    &app,
                                    TransportEvent::Transport { value: "offline" },
                                );
                                reconnect_attempt = reconnect_attempt.saturating_add(1);
                                tokio::time::sleep(reconnect_delay(reconnect_attempt)).await;
                                continue;
                            }
                        };
                        let bearer = match bearer_header(&admission.access_token) {
                            Ok(bearer) => bearer,
                            Err(message) => {
                                emit_error(&app, &message);
                                state.reset().await;
                                emit_transport(
                                    &app,
                                    TransportEvent::Transport { value: "offline" },
                                );
                                reconnect_attempt = reconnect_attempt.saturating_add(1);
                                tokio::time::sleep(reconnect_delay(reconnect_attempt)).await;
                                continue;
                            }
                        };
                        let relay = RelayCarrierPlan::from_admission(&admission);
                        (
                            url,
                            bearer,
                            admission.ice_servers,
                            admission.relay_only,
                            Some(admission.grants),
                            Some(admission.expected_peer_key_thumbprint),
                            Some(managed_admission_refresh_deadline(admission.expires_at)),
                            relay,
                        )
                    }
                    Err(error) => {
                        emit_managed_admission_state(&app, &error);
                        emit_error(&app, &error.to_string());
                        state.reset().await;
                        emit_transport(&app, TransportEvent::Transport { value: "offline" });
                        reconnect_attempt = reconnect_attempt.saturating_add(1);
                        tokio::time::sleep(reconnect_delay(reconnect_attempt)).await;
                        continue;
                    }
                }
            } else {
                (
                    gateway_url.clone(),
                    authorization.clone(),
                    config.ice_servers.clone(),
                    config.relay_only,
                    None,
                    None,
                    None,
                    // A custom profile has no managed admission, so it has no
                    // relay advertisement and no pinned Desktop peer key.
                    None,
                )
            };
            let session_nonce =
                fresh_session_nonce(&state, config.session_nonce.as_deref(), &config.device_id);
            state
                .begin(
                    &config.app_id,
                    &config.device_id,
                    &session_nonce,
                    &endpoint_identity,
                    &attempt_ice_servers,
                    attempt_relay_only,
                    attempt_relay.is_some(),
                    attempt_admission_grants.as_deref(),
                )
                .await;

            let opened = if let Some(plan) = attempt_relay.as_ref() {
                open_relay_transport(plan, &state, &config.app_id, &config.device_id, false).await
            } else {
                let mut request = match attempt_url.as_str().into_client_request() {
                    Ok(request) => request,
                    Err(_) => {
                        emit_error(&app, "could not create protocol-v2 realtime request");
                        return;
                    }
                };
                request
                    .headers_mut()
                    .insert("authorization", attempt_authorization);
                request
                    .headers_mut()
                    .insert("x-aokie-app-id", app_header.clone());
                request
                    .headers_mut()
                    .insert("x-aokie-device-id", device_header.clone());
                let websocket_config = WebSocketConfig::default()
                    .max_message_size(Some(MAX_MESSAGE_BYTES))
                    .max_frame_size(Some(MAX_MESSAGE_BYTES));
                match tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    connect_async_with_config(request, Some(websocket_config), false),
                )
                .await
                {
                    Ok(Ok((socket, _))) => Ok(V2Transport::websocket(socket)),
                    Ok(Err(error)) => Err(
                        if matches!(
                            &error,
                            WebSocketError::Http(response)
                                if matches!(
                                    response.status(),
                                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                                )
                        ) {
                            TransportOpenFailure::AdmissionRejected
                        } else {
                            TransportOpenFailure::Unavailable(
                                "protocol-v2 realtime endpoint is unavailable".into(),
                            )
                        },
                    ),
                    Err(_) => Err(TransportOpenFailure::TimedOut),
                }
            };
            match opened {
                Ok(mut transport) => {
                    let handshake = endpoint_handshake(
                        &app,
                        &state,
                        &endpoint_identity,
                        &peer_trust,
                        &profile_id,
                        &config.app_id,
                        &config.device_id,
                        &session_nonce,
                        admission_expected_peer_key_thumbprint.as_deref(),
                        None,
                        &mut transport,
                    )
                    .await;
                    if let Err(message) = handshake.as_ref() {
                        if config.managed_deployment_id.is_some()
                            && transient_managed_sync_failure(message)
                        {
                            eprintln!(
                                "[AokieCompanion][realtime] managed session rotated during endpoint proof; retrying"
                            );
                        } else {
                            emit_error(&app, message);
                        }
                    } else {
                        let peer_key_thumbprint =
                            handshake.expect("successful endpoint handshake was checked");
                        state.set_peer_key_thumbprint(peer_key_thumbprint).await;
                        let synced = initial_sync(
                            &app,
                            &state,
                            &media_state,
                            &config.app_id,
                            &mut transport,
                        )
                        .await;
                        match synced {
                            Err(message) => {
                                if config.managed_deployment_id.is_some()
                                    && transient_managed_sync_failure(&message)
                                {
                                    eprintln!(
                                        "[AokieCompanion][realtime] managed session rotated before sync; retrying"
                                    );
                                } else {
                                    emit_error(&app, &message);
                                }
                            }
                            Ok(()) => {
                                reconnect_attempt = 0;
                                let (generation, mut outbound) = match activate_session(&connection)
                                {
                                    Ok(active) => active,
                                    Err(message) => {
                                        emit_error(&app, &message);
                                        return;
                                    }
                                };
                                state.set_generation(generation).await;
                                emit_transport(
                                    &app,
                                    TransportEvent::Transport { value: "connected" },
                                );

                                let freshness = tokio::time::sleep(INBOUND_FRESHNESS);
                                tokio::pin!(freshness);
                                let mut ping = tokio::time::interval(PING_INTERVAL);
                                ping.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Delay,
                                );
                                ping.tick().await;
                                let pong_deadline = tokio::time::sleep(PONG_TIMEOUT);
                                tokio::pin!(pong_deadline);
                                let mut awaiting_pong = false;
                                let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
                                heartbeat.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Delay,
                                );
                                heartbeat.tick().await;
                                let mut native_actions = tokio::time::interval(NATIVE_ACTION_POLL);
                                native_actions.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Delay,
                                );
                                native_actions.tick().await;
                                let mut urgent_controls =
                                    tokio::time::interval(URGENT_CONTROL_POLL);
                                urgent_controls.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Delay,
                                );
                                urgent_controls.tick().await;
                                let admission_refresh = tokio::time::sleep_until(
                                    admission_refresh_deadline.unwrap_or_else(|| {
                                        Instant::now() + Duration::from_secs(24 * 60 * 60)
                                    }),
                                );
                                tokio::pin!(admission_refresh);
                                let managed_deployment_id = config.managed_deployment_id.as_deref();
                                // Cached because a select guard cannot borrow the
                                // transport while another arm takes it mutably —
                                // and re-read after a rotation, which is the one
                                // place the carrier can change under the loop.
                                let mut websocket_heartbeat = transport.uses_websocket_heartbeat();
                                let mut managed_rotation: Option<ManagedTransportRotationFuture> =
                                    None;
                                let managed_predecessor_fence_started =
                                    Arc::new(AtomicBool::new(false));
                                let mut predecessor_failure_during_rotation: Option<String> = None;
                                let mut deferred_native: Option<NativeOutbound> = None;
                                let mut deferred_local_frames: VecDeque<String> = VecDeque::new();
                                let mut deferred_command: Option<OutboundCommand> = None;

                                let session_end_reason = 'active_session: loop {
                                    tokio::select! {
                                        _ = &mut admission_refresh, if managed_deployment_id.is_some() && managed_rotation.is_none() => {
                                            eprintln!(
                                                "[AokieCompanion][realtime] opening overlapping managed admission before expiry"
                                            );
                                            let rotation_app = app.clone();
                                            let rotation_state = state.clone();
                                            let rotation_media_state = media_state.clone();
                                            let rotation_managed_auth = managed_auth.clone();
                                            let rotation_endpoint_identity = endpoint_identity.clone();
                                            let rotation_peer_trust = peer_trust.clone();
                                            let rotation_profile_id = profile_id.clone();
                                            let rotation_deployment_id = managed_deployment_id
                                                .expect("managed rotation checked")
                                                .to_owned();
                                            let rotation_app_id = config.app_id.clone();
                                            let rotation_device_id = config.device_id.clone();
                                            let rotation_session_nonce = session_nonce.clone();
                                            let rotation_app_header = app_header.clone();
                                            let rotation_device_header = device_header.clone();
                                            managed_predecessor_fence_started
                                                .store(false, Ordering::Release);
                                            let rotation_predecessor_fence_started =
                                                managed_predecessor_fence_started.clone();
                                            managed_rotation = Some(Box::pin(async move {
                                                open_overlapping_managed_transport(
                                                    &rotation_app,
                                                    &rotation_state,
                                                    &rotation_media_state,
                                                    &rotation_managed_auth,
                                                    &rotation_endpoint_identity,
                                                    &rotation_peer_trust,
                                                    &rotation_profile_id,
                                                    &rotation_deployment_id,
                                                    &rotation_app_id,
                                                    &rotation_device_id,
                                                    &rotation_session_nonce,
                                                    &rotation_app_header,
                                                    &rotation_device_header,
                                                    rotation_predecessor_fence_started,
                                                )
                                                .await
                                            }));
                                        }
                                        rotation_result = await_managed_transport_rotation(&mut managed_rotation), if managed_rotation.is_some() => {
                                            // The completed future is removed before any retry is
                                            // scheduled, so one slow refresh cannot multiply into
                                            // parallel admissions. Dropping the slot also cancels it
                                            // automatically when this active session ends.
                                            managed_rotation = None;
                                            if !session_is_current(&connection, generation) {
                                                break 'active_session "managed rotation completed after realtime generation changed".to_string();
                                            }
                                            match rotation_result {
                                                 Ok(rotation) => {
                                                     let ManagedTransportRotation {
                                                        transport: replacement,
                                                         admission,
                                                         peer_key_thumbprint,
                                                         initial_frames,
                                                     } = rotation;
                                                     let replacement_is_relay = replacement.is_relay();
                                                     if !transport.same_carrier_as(&replacement) {
                                                         let _ = state
                                                             .break_managed_continuity(
                                                                 &app,
                                                                 &media_state,
                                                                 "managed_carrier_changed",
                                                             )
                                                             .await;
                                                         break 'active_session "managed carrier changed; starting a fresh sequence baseline".to_string();
                                                     }
                                                     let continuity = state
                                                         .managed_continuity_matches(
                                                             &peer_key_thumbprint,
                                                             &admission.grants,
                                                         )
                                                         .await;
                                                     if !continuity {
                                                         if let Err(message) = state
                                                             .break_managed_continuity(
                                                                 &app,
                                                                 &media_state,
                                                                 "managed_admission_continuity_changed",
                                                             )
                                                             .await
                                                         {
                                                             emit_error(&app, &message);
                                                         }
                                                     }
                                                     for initial_frame in &initial_frames {
                                                         if let Err(message) = handle_gateway_frame(
                                                             &app,
                                                             &state,
                                                             &media_state,
                                                             &config.app_id,
                                                             initial_frame,
                                                         )
                                                         .await
                                                         {
                                                             if managed_predecessor_fence_started
                                                                 .load(Ordering::Acquire)
                                                             {
                                                                 break 'active_session format!(
                                                                     "fenced replacement authoritative frame was rejected: {message}"
                                                                 );
                                                             }
                                                             admission_refresh.as_mut().reset(
                                                                 Instant::now() + Duration::from_secs(1),
                                                             );
                                                             continue 'active_session;
                                                         }
                                                     }
                                                     if let Err(message) = transport
                                                         .install_managed_replacement(replacement)
                                                     {
                                                         break 'active_session message;
                                                     }
                                                     while let Some(frame) = deferred_local_frames.pop_front() {
                                                         if !transport.send_text(frame).await {
                                                             break 'active_session "deferred native RTC signal failed on managed replacement".to_string();
                                                         }
                                                     }
                                                     if let Some(native) = deferred_native.take() {
                                                         let NativeOutbound {
                                                              encoded,
                                                              answer_action_id,
                                                              answer_request_id,
                                                              assistance_answer_id,
                                                              ..
                                                          } = native;
                                                          if !transport.send_text(encoded).await {
                                                              if let Some(answer_id) = assistance_answer_id.as_deref() {
                                                                  clear_pending_assistance_answer(&state, answer_id).await;
                                                              }
                                                              break 'active_session "deferred native action failed on managed replacement".to_string();
                                                          }
                                                          if let Some(action_id) = answer_action_id {
                                                              let success_code = if assistance_answer_id.is_some() {
                                                                  "assistance_decline_sent"
                                                              } else {
                                                                  "lease_request_sent"
                                                              };
                                                              if let Err(message) = crate::android_runtime::complete_native_call_action(
                                                                  &app,
                                                                  &action_id,
                                                                  true,
                                                                  success_code,
                                                             ).await {
                                                                 emit_error(&app, &message);
                                                             } else if let Some(request_id) = answer_request_id.as_deref() {
                                                                 mark_native_answer_delivered(&state, request_id, &action_id).await;
                                                             }
                                                         }
                                                     }
                                                     if let Some(queued) = deferred_command.take() {
                                                         if transport.send_text(queued.encoded).await {
                                                             let _ = queued.completion.send(Ok(()));
                                                         } else {
                                                             let _ = queued.completion.send(Err("protocol-v2 frame could not be delivered".into()));
                                                             break 'active_session "deferred command failed on managed replacement".to_string();
                                                         }
                                                     }
                                                     managed_predecessor_fence_started
                                                         .store(false, Ordering::Release);
                                                     predecessor_failure_during_rotation = None;
                                                     state
                                                         .set_peer_key_thumbprint(peer_key_thumbprint)
                                                         .await;
                                                     if let Err(message) = state
                                                        .rotate_admission_policy(
                                                            &app,
                                                            &media_state,
                                                            &admission.ice_servers,
                                                            admission.relay_only,
                                                            replacement_is_relay,
                                                            &admission.grants,
                                                        )
                                                        .await
                                                    {
                                                         emit_error(&app, &message);
                                                     }
                                                     websocket_heartbeat =
                                                         transport.uses_websocket_heartbeat();
                                                    admission_refresh.as_mut().reset(
                                                        managed_admission_refresh_deadline(
                                                            admission.expires_at,
                                                        ),
                                                    );
                                                    freshness.as_mut().reset(
                                                        Instant::now() + INBOUND_FRESHNESS,
                                                    );
                                                     awaiting_pong = false;
                                                     ping.reset();
                                                     // Service the media clock immediately after a WS
                                                     // handoff: its predecessor could not be written
                                                     // once the replacement hello fenced it.
                                                     match heartbeat_frame(&state, &config.app_id).await {
                                                         Ok(Some(frame)) => {
                                                             if !transport.send_text(frame).await {
                                                                 break 'active_session "lease heartbeat failed on managed replacement".to_string();
                                                             }
                                                         }
                                                         Ok(None) => {
                                                             if let Some((expired, queue_error)) = take_expired_lease(&state).await {
                                                                 let _ = media::revoke(
                                                                     &app,
                                                                     &media_state,
                                                                     RevokeRequest {
                                                                         session: expired.session,
                                                                         reason: Some("media lease expired during admission refresh".into()),
                                                                     },
                                                                 ).await;
                                                                 emit_lease_reset(&app);
                                                                 if let Some(message) = queue_error {
                                                                     emit_error(&app, &message);
                                                                 }
                                                             }
                                                         }
                                                         Err(message) => {
                                                             break 'active_session format!("lease heartbeat failed after managed rotation: {message}");
                                                         }
                                                     }
                                                     eprintln!(
                                                         "[AokieCompanion][realtime] managed admission rotated; media_continuity={continuity}"
                                                     );
                                                 }
                                                 Err(ManagedTransportRotationError::Admission(error)) => {
                                                     if let Some(message) = predecessor_failure_during_rotation.take() {
                                                         break 'active_session format!("managed predecessor failed before admission replacement: {message}");
                                                     }
                                                    emit_managed_admission_state(&app, &error);
                                                    eprintln!(
                                                        "[AokieCompanion][realtime] managed admission rotation retry: {error}"
                                                    );
                                                    admission_refresh.as_mut().reset(
                                                        Instant::now() + Duration::from_secs(1),
                                                    );
                                                }
                                                 Err(ManagedTransportRotationError::Transport(message)) => {
                                                     if managed_predecessor_fence_started
                                                         .swap(false, Ordering::AcqRel)
                                                     {
                                                         break 'active_session format!(
                                                             "fenced managed replacement failed: {message}"
                                                         );
                                                     }
                                                     if let Some(predecessor) = predecessor_failure_during_rotation.take() {
                                                         break 'active_session format!(
                                                             "managed replacement failed after predecessor became unavailable: {message}; predecessor={predecessor}"
                                                         );
                                                     }
                                                     eprintln!(
                                                        "[AokieCompanion][realtime] managed transport rotation retry: {message}"
                                                    );
                                                    admission_refresh.as_mut().reset(
                                                        Instant::now() + Duration::from_secs(1),
                                                    );
                                                }
                                            }
                                        }
                                        _ = &mut freshness, if predecessor_receive_enabled(
                                            predecessor_failure_during_rotation.is_some(),
                                            managed_predecessor_fence_started.load(Ordering::Acquire),
                                        ) => {
                                            if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                continue 'active_session;
                                            }
                                            emit_error(&app, "protocol-v2 inbound heartbeat timed out");
                                            break 'active_session "inbound heartbeat timed out".to_string();
                                        }
                                        _ = &mut pong_deadline, if predecessor_failure_during_rotation.is_none() && awaiting_pong && !managed_predecessor_fence_started.load(Ordering::Acquire) => {
                                            if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                continue 'active_session;
                                            }
                                            emit_error(&app, "protocol-v2 pong timed out");
                                            break 'active_session "websocket pong timed out".to_string();
                                        }
                                        _ = ping.tick(), if predecessor_failure_during_rotation.is_none() && !awaiting_pong && websocket_heartbeat && !managed_predecessor_fence_started.load(Ordering::Acquire) => {
                                            if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                continue 'active_session;
                                            }
                                            if !transport.send_ping().await {
                                                if managed_rotation.is_some() {
                                                    predecessor_failure_during_rotation = Some("websocket ping delivery failed".into());
                                                    continue 'active_session;
                                                }
                                                break 'active_session "websocket ping delivery failed".to_string();
                                            }
                                            awaiting_pong = true;
                                            pong_deadline.as_mut().reset(Instant::now() + PONG_TIMEOUT);
                                        }
                                        _ = heartbeat.tick(), if predecessor_failure_during_rotation.is_none() && !managed_predecessor_fence_started.load(Ordering::Acquire) => {
                                            if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                continue 'active_session;
                                            }
                                            match heartbeat_frame(&state, &config.app_id).await {
                                                Ok(Some(frame)) => {
                                                    if !transport.send_text(frame).await {
                                                        if managed_rotation.is_some() {
                                                            predecessor_failure_during_rotation = Some("lease heartbeat delivery failed".into());
                                                            continue 'active_session;
                                                        }
                                                        break 'active_session "lease heartbeat delivery failed".to_string();
                                                    }
                                                }
                                                Ok(None) => {
                                                    // A local expiry is an authority boundary, not a
                                                    // signalling failure.  Tear down only the exact
                                                    // expired media route and best-effort return its
                                                    // token, while keeping the authenticated transport
                                                    // alive.  Breaking this loop used to reconnect,
                                                    // send a fresh mobile_hello, and turn one expected
                                                    // lease failback into a second session-wide media
                                                    // failure.
                                                    if let Some((expired, queue_error)) =
                                                        take_expired_lease(&state).await
                                                    {
                                                        let _ = media::revoke(
                                                            &app,
                                                            &media_state,
                                                            RevokeRequest {
                                                                session: expired.session,
                                                                reason: Some("media lease expired locally".into()),
                                                            },
                                                        ).await;
                                                        emit_lease_reset(&app);
                                                        if let Some(message) = queue_error {
                                                            emit_error(&app, &message);
                                                        }
                                                    }
                                                }
                                                Err(message) => {
                                                    emit_error(&app, &message);
                                                    break 'active_session format!("lease heartbeat failed: {message}");
                                                }
                                            }
                                        }
                                        _ = urgent_controls.tick(), if predecessor_failure_during_rotation.is_none() && !managed_predecessor_fence_started.load(Ordering::Acquire) => {
                                                if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                    continue 'active_session;
                                                }
                                                if let Some(urgent) = peek_urgent_control_frame(&state).await {
                                                    if !transport.send_text(urgent.encoded.clone()).await {
                                                        if managed_rotation.is_some() {
                                                            predecessor_failure_during_rotation = Some("urgent control delivery failed".into());
                                                            continue 'active_session;
                                                        }
                                                        break 'active_session "urgent control delivery failed".to_string();
                                                }
                                                if let Err(message) = confirm_urgent_control_frame(
                                                    &state,
                                                    &urgent,
                                                ).await {
                                                    emit_error(&app, &message);
                                                    break 'active_session format!("urgent control confirmation failed: {message}");
                                                }
                                            }
                                        }
                                        _ = native_actions.tick(), if predecessor_failure_during_rotation.is_none() && !managed_predecessor_fence_started.load(Ordering::Acquire) => {
                                            if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                continue 'active_session;
                                            }
                                            match poll_native_call_actions(&app, &state, &media_state).await {
                                                Ok(Some(native)) => {
                                                    let delivered = transport
                                                        .send_text(native.encoded.clone())
                                                        .await;
                                                    if !delivered && managed_rotation.is_some() {
                                                        deferred_native = Some(native);
                                                        predecessor_failure_during_rotation =
                                                            Some("native call action delivery failed".into());
                                                        continue 'active_session;
                                                    }
                                                    let NativeOutbound {
                                                        encoded: _,
                                                         answer_action_id,
                                                         answer_request_id,
                                                         assistance_answer_id,
                                                         revoke_request_id,
                                                    } = native;
                                                    if delivered {
                                                         if let Some(action_id) = answer_action_id {
                                                             let success_code = if assistance_answer_id.is_some() {
                                                                 "assistance_decline_sent"
                                                             } else {
                                                                 "lease_request_sent"
                                                             };
                                                             if let Err(message) = crate::android_runtime::complete_native_call_action(
                                                                 &app,
                                                                 &action_id,
                                                                 true,
                                                                 success_code,
                                                            ).await {
                                                                emit_error(&app, &message);
                                                            } else if let Some(request_id) = answer_request_id.as_deref() {
                                                                mark_native_answer_delivered(&state, request_id, &action_id).await;
                                                            }
                                                        }
                                                     } else {
                                                         if let Some(answer_id) = assistance_answer_id.as_deref() {
                                                             clear_pending_assistance_answer(&state, answer_id).await;
                                                         }
                                                         let cleared_answer_action = if let Some(request_id) = answer_request_id {
                                                            clear_pending_lease(&state, &request_id).await
                                                        } else {
                                                            None
                                                        };
                                                        let native_action_id = if let Some(request_id) = revoke_request_id {
                                                            abandon_pending_revoke(&state, &request_id).await
                                                        } else {
                                                            answer_action_id.or(cleared_answer_action)
                                                        };
                                                        if let Some(action_id) = native_action_id {
                                                            let _ = crate::android_runtime::complete_native_call_action(
                                                                &app,
                                                                &action_id,
                                                                false,
                                                                "native_action_delivery_failed",
                                                            ).await;
                                                        }
                                                        break 'active_session "native call action delivery failed".to_string();
                                                    }
                                                }
                                                Ok(None) => {}
                                                Err(message) => emit_error(&app, &message),
                                            }
                                        }
                                        local = local_signals.recv(), if predecessor_failure_during_rotation.is_none() && !managed_predecessor_fence_started.load(Ordering::Acquire) => {
                                            let predecessor_fenced = !predecessor_arm_still_current(
                                                &managed_predecessor_fence_started,
                                            );
                                            match local {
                                                Ok(signal) => {
                                                    if let LocalSignal::LeaseSafetyFailure { reason } = &signal.signal {
                                                        if let Err(message) = surrender_failed_private_consult(
                                                            &app,
                                                            &state,
                                                            &signal.session,
                                                            reason,
                                                        ).await {
                                                            emit_error(&app, &message);
                                                        }
                                                    } else {
                                                        match local_rtc_frame(&state, signal).await {
                                                            Ok(Some(frame)) => {
                                                                if predecessor_fenced {
                                                                    deferred_local_frames.push_back(frame);
                                                                    continue 'active_session;
                                                                }
                                                                if !transport.send_text(frame.clone()).await {
                                                                    if managed_rotation.is_some() {
                                                                        deferred_local_frames.push_back(frame);
                                                                        predecessor_failure_during_rotation =
                                                                            Some("native RTC signal delivery failed".into());
                                                                        continue 'active_session;
                                                                    }
                                                                    break 'active_session "native RTC signal delivery failed".to_string();
                                                                }
                                                            }
                                                            Ok(None) => {}
                                                            Err(message) => {
                                                                emit_error(&app, &message);
                                                                break 'active_session format!("native RTC signal rejected: {message}");
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                                    emit_error(&app, "native RTC signal capacity was exceeded");
                                                    break 'active_session "native RTC signal capacity exceeded".to_string();
                                                }
                                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                                    break 'active_session "native RTC signal channel closed".to_string();
                                                }
                                            }
                                        }
                                        queued = outbound.recv(), if predecessor_failure_during_rotation.is_none() && !managed_predecessor_fence_started.load(Ordering::Acquire) => {
                                            let Some(queued) = queued else {
                                                break 'active_session "outbound command channel closed".to_string();
                                            };
                                            if queued.generation != generation || !session_is_current(&connection, generation) {
                                                let _ = queued.completion.send(Err("realtime session changed before delivery".into()));
                                                break 'active_session "realtime generation changed before delivery".to_string();
                                            }
                                            if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                if deferred_command.replace(queued).is_some() {
                                                    break 'active_session "multiple user commands crossed the managed handoff fence".to_string();
                                                }
                                                continue 'active_session;
                                            }
                                            if transport.send_text(queued.encoded.clone()).await {
                                                let _ = queued.completion.send(Ok(()));
                                            } else {
                                                if managed_rotation.is_some() {
                                                    deferred_command = Some(queued);
                                                    predecessor_failure_during_rotation =
                                                        Some("queued frame delivery failed".into());
                                                    continue 'active_session;
                                                }
                                                let _ = queued.completion.send(Err("protocol-v2 frame could not be delivered".into()));
                                                break 'active_session "queued frame delivery failed".to_string();
                                            }
                                        }
                                        incoming = transport.recv(TRANSPORT_READ_TICK), if predecessor_receive_enabled(
                                            predecessor_failure_during_rotation.is_some(),
                                            managed_predecessor_fence_started.load(Ordering::Acquire),
                                        ) => {
                                            if !predecessor_arm_still_current(&managed_predecessor_fence_started) {
                                                // This frame completed after the replacement hello
                                                // crossed the gateway fence. It belongs to the old
                                                // socket generation and must not mutate replacement
                                                // authority; its due renewal is sent through the new
                                                // reader by receive_replacement_sync.
                                                continue 'active_session;
                                            }
                                            match incoming {
                                                V2Inbound::Text(text) => {
                                                    freshness.as_mut().reset(Instant::now() + INBOUND_FRESHNESS);
                                                    if let Err(message) = handle_gateway_frame(
                                                        &app,
                                                        &state,
                                                        &media_state,
                                                        &config.app_id,
                                                        &text,
                                                    ).await {
                                                        emit_error(&app, &message);
                                                        break 'active_session format!("gateway frame rejected: {message}");
                                                    }
                                                }
                                                // Carrier liveness without a protocol
                                                // frame: a socket ping, or relay stream
                                                // bytes. It feeds the freshness timer
                                                // exactly as an inbound frame does,
                                                // which is what keeps a healthy but
                                                // quiet relay from timing itself out.
                                                V2Inbound::Alive => {
                                                    freshness.as_mut().reset(Instant::now() + INBOUND_FRESHNESS);
                                                }
                                                V2Inbound::Pong => {
                                                    freshness.as_mut().reset(Instant::now() + INBOUND_FRESHNESS);
                                                    awaiting_pong = false;
                                                    ping.reset();
                                                }
                                                V2Inbound::Idle => {}
                                                V2Inbound::Closed(detail) => {
                                                    eprintln!("[AokieCompanion][realtime] {detail}");
                                                    if managed_rotation.is_some() {
                                                        predecessor_failure_during_rotation = Some(detail);
                                                        continue 'active_session;
                                                    }
                                                    break 'active_session format!("transport closed: {detail}");
                                                }
                                                V2Inbound::Failed(message) => {
                                                    if managed_rotation.is_some() {
                                                        predecessor_failure_during_rotation = Some(message);
                                                        continue 'active_session;
                                                    }
                                                    emit_error(&app, &message);
                                                    break 'active_session format!("transport failed: {message}");
                                                }
                                            }
                                        }
                                    }
                                };
                                eprintln!(
                                    "[AokieCompanion][realtime] stage=session_end generation={} reason={}",
                                    generation, session_end_reason
                                );
                                let _ = invalidate_session(&connection, Some(generation));
                                fail_native_call_actions_on_disconnect(&app, &state).await;
                                state.reset_generation(generation).await;
                                let _ = media::close(
                                    &app,
                                    &media_state,
                                    "protocol-v2 transport was interrupted",
                                )
                                .await;
                                emit_lease_reset(&app);
                            }
                        }
                    }
                }
                Err(TransportOpenFailure::AdmissionRejected) => {
                    emit_error(&app, "protocol-v2 admission expired");
                    emit_transport(&app, TransportEvent::Transport { value: "offline" });
                    state.reset().await;
                    if config.managed_deployment_id.is_none() {
                        return;
                    }
                }
                Err(TransportOpenFailure::Unavailable(message)) => emit_error(&app, &message),
                Err(TransportOpenFailure::TimedOut) => {
                    emit_error(&app, "protocol-v2 connection attempt timed out")
                }
            }

            state.reset().await;
            let _ = media::close(&app, &media_state, "protocol-v2 transport is reconnecting").await;
            emit_lease_reset(&app);
            reconnect_attempt = reconnect_attempt.saturating_add(1);
            emit_transport(&app, TransportEvent::Transport { value: "offline" });
            tokio::time::sleep(reconnect_delay(reconnect_attempt)).await;
        }
    }))
}

fn realtime_profile_id(config: &RealtimeConfig) -> Result<String, String> {
    match (
        config.managed_deployment_id.as_deref(),
        config.managed_profile_id.as_deref(),
    ) {
        (Some(_), Some(profile_id)) => Ok(profile_id.to_owned()),
        (None, None) => {
            let digest = Sha256::digest(
                format!(
                    "{}\0{}\0{}",
                    config.gateway_url, config.app_id, config.device_id
                )
                .as_bytes(),
            );
            Ok(format!("custom_{digest:x}"))
        }
        _ => Err("managed realtime profile binding is incomplete".into()),
    }
}

fn managed_ws_handoff_lease_ready(client: &ClientState, now: u64) -> Result<(), String> {
    let Some(lease) = client.lease.as_ref() else {
        return Ok(());
    };
    if matches!(lease.claims.phase, LeasePhase::Prepared) {
        return Err("managed socket handoff is waiting for provisional media to settle".into());
    }
    if lease.claims.expires_at.saturating_sub(now) <= MANAGED_HANDOFF_MIN_LEASE_REMAINING.as_secs()
    {
        return Err("managed socket handoff is waiting for the active media lease to renew".into());
    }
    Ok(())
}

async fn require_managed_ws_handoff_lease_ready(state: &V2State) -> Result<(), String> {
    let now = unix_now()?;
    let client = state.inner.lock().await;
    managed_ws_handoff_lease_ready(&client, now)
}

#[allow(clippy::too_many_arguments)]
async fn endpoint_handshake(
    app: &AppHandle,
    state: &V2State,
    endpoint_identity: &crate::endpoint_identity::EndpointIdentity,
    peer_trust: &PeerTrustState,
    profile_id: &str,
    app_id: &str,
    device_id: &str,
    session_nonce: &str,
    admission_expected_peer_key_thumbprint: Option<&str>,
    predecessor_fence_started: Option<&AtomicBool>,
    transport: &mut V2Transport,
) -> Result<String, String> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    // Both carriers hand back the same document; only where it comes from
    // differs (the socket's first frame, or an authenticated relay route).
    let challenge = transport.next_challenge(deadline).await?;
    let now = unix_now()?;
    challenge.validate(now).map_err(|error| error.to_string())?;
    if challenge.app_id != app_id
        || challenge.subject_id != device_id
        || challenge.role != AdmissionRole::Mobile
        || challenge.holder_key_thumbprint != endpoint_identity.thumbprint()
    {
        return Err("endpoint challenge is not bound to this mobile installation".into());
    }
    let peer_key_thumbprint = challenge
        .expected_peer_key_thumbprint
        .as_deref()
        .ok_or("mobile endpoint challenge omitted the Desktop peer key")?;
    if admission_expected_peer_key_thumbprint
        .is_some_and(|expected| expected != peer_key_thumbprint)
    {
        return Err("endpoint challenge changed the admission-pinned Desktop peer key".into());
    }
    crate::peer_trust::require_confirmed_peer(
        app,
        peer_trust,
        profile_id,
        app_id,
        device_id,
        peer_key_thumbprint,
    )
    .await?;

    let proof_expiry = challenge.expires_at.min(now.saturating_add(30));
    let proof = endpoint_identity.sign_hello(HelloProofClaims {
        app_id: challenge.app_id.clone(),
        subject_id: challenge.subject_id.clone(),
        role: challenge.role,
        connection_id: challenge.connection_id.clone(),
        challenge_nonce: challenge.challenge_nonce.clone(),
        admission_jti: challenge.admission_jti.clone(),
        session_nonce: session_nonce.to_owned(),
        holder_key_thumbprint: challenge.holder_key_thumbprint.clone(),
        expected_peer_key_thumbprint: challenge.expected_peer_key_thumbprint.clone(),
        approved_peer_key_thumbprints: challenge.approved_peer_key_thumbprints.clone(),
        peer_roster_revision: challenge.peer_roster_revision,
        peer_roster_hash: challenge.peer_roster_hash.clone(),
        nonce: state.next_id("hello_nonce"),
        jti: state.next_id("hello_proof"),
        issued_at: now,
        expires_at: proof_expiry,
    })?;
    let hello = MobileHello {
        kind: "mobile_hello".into(),
        schema_version: SCHEMA_VERSION,
        app_id: app_id.to_owned(),
        device_id: device_id.to_owned(),
        session_nonce: session_nonce.to_owned(),
        endpoint_proof: proof,
    };
    hello.validate().map_err(|error| error.to_string())?;
    let encoded = serde_json::to_string(&hello)
        .map_err(|_| "could not encode endpoint-authenticated mobile hello".to_string())?;
    if !transport.is_relay() {
        // The gateway replaces/fences the prior device peer as soon as this
        // hello may be accepted. Set before the write so predecessor closure
        // cannot race this marker by one scheduler turn.
        if let Some(started) = predecessor_fence_started {
            // Admission/connect/challenge preparation happens while the old
            // socket continues to read and renew. At the exact irreversible
            // handoff boundary, require enough token life for the bounded
            // replacement hello + heartbeat pair. A short token aborts before
            // fencing and the active loop retries after consuming its renewal.
            require_managed_ws_handoff_lease_ready(state).await?;
            started.store(true, Ordering::Release);
        }
    }
    let delivered = if predecessor_fence_started.is_some() && !transport.is_relay() {
        transport.send_managed_handoff_text(encoded).await
    } else {
        transport.send_text(encoded).await
    };
    if !delivered {
        return Err("endpoint-authenticated mobile hello could not be delivered".into());
    }
    Ok(peer_key_thumbprint.to_owned())
}

/// Wait for the replacement's first authenticated authoritative projection
/// without applying it. Managed admission overlap runs concurrently with the
/// active predecessor, so applying state or media changes inside that future
/// would let a not-yet-adopted carrier race the live session.
async fn receive_initial_sync(
    transport: &mut V2Transport,
    tolerate_pre_snapshot_frames: bool,
) -> Result<String, String> {
    let deadline = Instant::now() + INITIAL_SYNC_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            if transport.is_relay() {
                eprintln!(
                    "[AokieCompanion][relay] no authoritative state arrived after the authenticated mobile hello"
                );
            }
            return Err("protocol-v2 authoritative sync timed out".into());
        }
        match transport.recv(TRANSPORT_READ_TICK).await {
            V2Inbound::Text(text) => {
                if initial_sync_frame_ready(&text, tolerate_pre_snapshot_frames)? {
                    return Ok(text);
                }
            }
            V2Inbound::Idle | V2Inbound::Alive | V2Inbound::Pong => {}
            V2Inbound::Closed(_) => {
                return Err("protocol-v2 transport closed before authoritative sync".into());
            }
            V2Inbound::Failed(message) => return Err(message),
        }
    }
}

fn encoded_request_id(encoded: &str, label: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(encoded).map_err(|_| format!("{label} was not valid JSON"))?;
    value
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|request_id| !request_id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{label} omitted requestId"))
}

fn replacement_renewal_confirmed(encoded: &str, request_id: &str) -> Result<bool, String> {
    let kind = parse_kind(encoded)?;
    if !matches!(kind.as_str(), "lease_renewed" | "error") {
        return Ok(false);
    }
    let value: Value = serde_json::from_str(encoded)
        .map_err(|_| "replacement lease response was not valid JSON".to_string())?;
    if value.get("requestId").and_then(Value::as_str) != Some(request_id) {
        return Ok(false);
    }
    if kind == "lease_renewed" {
        return Ok(true);
    }
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error");
    Err(format!(
        "replacement rejected its handoff lease heartbeat: {code}"
    ))
}

async fn receive_replacement_sync(
    transport: &mut V2Transport,
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    app_id: &str,
) -> Result<Vec<String>, String> {
    let renewal_app = app.clone();
    let renewal_state = state.clone();
    let renewal_media_state = media_state.clone();
    let renewal_app_id = app_id.to_owned();
    receive_replacement_sync_with_renewal(transport, state, app_id, move |encoded: String| {
        let app = renewal_app.clone();
        let state = renewal_state.clone();
        let media_state = renewal_media_state.clone();
        let app_id = renewal_app_id.clone();
        async move { handle_gateway_frame(&app, &state, &media_state, &app_id, &encoded).await }
    })
    .await
}

async fn receive_replacement_sync_with_renewal<ApplyRenewal, ApplyRenewalFuture>(
    transport: &mut V2Transport,
    state: &V2State,
    app_id: &str,
    mut apply_renewal: ApplyRenewal,
) -> Result<Vec<String>, String>
where
    ApplyRenewal: FnMut(String) -> ApplyRenewalFuture,
    ApplyRenewalFuture: Future<Output = Result<(), String>>,
{
    let deadline = Instant::now() + INITIAL_SYNC_TIMEOUT;
    let mut buffered = Vec::new();
    let mut authoritative = false;
    let mut renewal_request_id: Option<String> = None;
    let mut renewal_confirmed = false;
    loop {
        if Instant::now() >= deadline {
            return Err("protocol-v2 replacement authoritative sync timed out".into());
        }
        // The old socket is now fenced. If sync is slow enough for the active
        // token to enter its renewal window, renew on the replacement and do
        // not expose the buffered snapshot until the matching reply is also
        // buffered. This prevents the install path from observing an expired
        // old token while its renewal is still queued on the new reader.
        if renewal_request_id.is_none() {
            if let Some(encoded) = heartbeat_frame(state, app_id).await? {
                let request_id = encoded_request_id(&encoded, "handoff lease heartbeat")?;
                if !transport.send_managed_handoff_text(encoded).await {
                    return Err("handoff lease heartbeat could not be delivered".into());
                }
                renewal_request_id = Some(request_id);
            }
        }
        if authoritative && (renewal_request_id.is_none() || renewal_confirmed) {
            return Ok(buffered);
        }
        match transport.recv(TRANSPORT_READ_TICK).await {
            V2Inbound::Text(text) => {
                let frame_is_authoritative = initial_sync_frame_ready(&text, true)?;
                if let Some(request_id) = renewal_request_id.as_deref() {
                    if replacement_renewal_confirmed(&text, request_id)? {
                        // Native expiry is driven by the old MediaSession, not
                        // by this receive loop. Buffering a valid renewal until
                        // a delayed snapshot would therefore still let the
                        // watcher close the live peer. Apply this one exact,
                        // request-bound renewal immediately; it remains in the
                        // ordered buffer and is a replay no-op after install.
                        apply_renewal(text.clone()).await?;
                        renewal_confirmed = true;
                    }
                }
                if buffered.len() >= MAX_APPLIED_RELAY_FRAMES {
                    return Err("replacement pre-sync frame buffer reached its safety bound".into());
                }
                buffered.push(text);
                authoritative |= frame_is_authoritative;
            }
            V2Inbound::Idle | V2Inbound::Alive | V2Inbound::Pong => {}
            V2Inbound::Closed(_) => {
                return Err("protocol-v2 replacement closed before authoritative sync".into())
            }
            V2Inbound::Failed(message) => return Err(message),
        }
    }
}

fn initial_sync_frame_ready(
    encoded: &str,
    tolerate_pre_snapshot_frames: bool,
) -> Result<bool, String> {
    let kind = parse_kind(encoded)?;
    if kind == "snapshot" || kind == "idle_sync" {
        return Ok(true);
    }
    if tolerate_pre_snapshot_frames {
        eprintln!(
            "[AokieCompanion][realtime] buffered authenticated {kind} while replacement awaited authoritative sync"
        );
        return Ok(false);
    }
    Err("protocol-v2 first gateway frame was not authoritative call state".into())
}

async fn initial_sync(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    transport: &mut V2Transport,
) -> Result<(), String> {
    let frame = receive_initial_sync(transport, false).await?;
    handle_gateway_frame(app, state, media_state, expected_app_id, &frame).await
}

async fn handle_gateway_frame(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    encoded: &str,
) -> Result<(), String> {
    if encoded.len() > MAX_MESSAGE_BYTES {
        return Err("protocol-v2 frame exceeded the size limit".into());
    }
    match parse_kind(encoded)?.as_str() {
        "idle_sync" => {
            let frame: MobileIdleSyncFrame = strict_parse(encoded, "idle sync")?;
            validate_idle_sync(&frame, expected_app_id)?;
            let Some(cleanup) = ({
                let mut client = state.inner.lock().await;
                transition_to_idle(&mut client, &frame)?
            }) else {
                // Retries can replay an already-applied authoritative frame.
                // Ignore it without reopening authority or churning transport.
                return Ok(());
            };
            for action_id in cleanup.failed_native_action_ids {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    false,
                    "authoritative_call_idle",
                )
                .await;
            }
            if let Some(action_id) = cleanup.confirmed_revoke_action_id {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    true,
                    "lease_return_confirmed",
                )
                .await;
            }
            if let Some((call_id, call_epoch)) = cleanup.pending_call {
                let _ = crate::android_runtime::reconcile_offer(
                    app,
                    &call_id,
                    call_epoch,
                    "cancel",
                    "authoritative_call_idle",
                )
                .await;
            }
            let _ = media::close(app, media_state, "authoritative call state is idle").await;
            emit_lease_reset(app);
            emit_assistance_reset(app);
            app.emit("aokie-companion://v2-idle-sync", frame)
                .map_err(|_| "could not deliver protocol-v2 idle sync".to_string())?;
        }
        "snapshot" => {
            let mut frame: MobileSnapshotFrame = strict_parse(encoded, "snapshot")?;
            validate_snapshot(&frame, expected_app_id)?;
            let now = unix_now()?;
            let (
                assistance_cleared,
                stale_end_caller_request,
                confirmed_revoke_action_id,
                grant_revoked_lease,
                replaced_native_call,
                microphone_mute_reconciliation,
                failed_native_hangup_action_id,
            ) = {
                let mut client = state.inner.lock().await;
                if !is_new_authoritative_sequence(&client, frame.sequence) {
                    // The idle and call projections share one monotonic high-
                    // water mark. A late snapshot must never resurrect a call.
                    return Ok(());
                }
                if let Some(lease) = &client.lease {
                    if frame.snapshot.call_id != lease.claims.call_id
                        || frame.snapshot.call_epoch != lease.claims.call_epoch
                        || frame.snapshot.owner_epoch < lease.claims.owner_epoch
                    {
                        return Err("protocol-v2 snapshot regressed active media authority".into());
                    }
                }
                let grant_revoked_lease = client
                    .lease
                    .as_ref()
                    .filter(|lease| {
                        let effective_grants = client
                            .admission_grants
                            .as_deref()
                            .unwrap_or(frame.grants.as_slice());
                        !grants_permit_lease_mode(effective_grants, lease.claims.mode)
                            || !current_consent_permits_mode(
                                &frame.snapshot.remote_consent,
                                lease.claims.mode,
                                now,
                            )
                    })
                    .cloned();
                let mut failed_native_hangup_action_id = None;
                if grant_revoked_lease.is_some() {
                    client.lease = None;
                    if client
                        .pending_end_caller
                        .as_ref()
                        .is_none_or(|pending| !pending.is_awaiting_result())
                    {
                        client.pending_end_caller = None;
                        failed_native_hangup_action_id = client
                            .pending_native_hangup
                            .take()
                            .map(|pending| pending.action_id);
                    }
                }
                let microphone_mute_reconciliation =
                    reconcile_microphone_mute_from_snapshot(&mut client, &frame);
                let assistance_cleared = client.assistance.as_ref().is_some_and(|assistance| {
                    frame.snapshot.call_id != assistance.call_id
                        || frame.snapshot.call_epoch != assistance.call_epoch
                        || frame.snapshot.owner_epoch != assistance.owner_epoch
                        || frame.snapshot.switchboard_revision != assistance.switchboard_revision
                        || frame.snapshot.remote_revision != assistance.remote_revision
                        || !remote_consent_is_current(&frame.snapshot.remote_consent, now)
                        || !frame.snapshot.remote_consent.assistance_enabled
                });
                if assistance_cleared {
                    client.assistance = None;
                    client.pending_assistance_answer = None;
                } else if !frame.grants.contains(&Grant::AssistanceRespond) {
                    // A socket-to-relay rotation can leave a text answer
                    // pending from the old carrier. The relay projection
                    // deliberately withholds this grant, so it cannot remain
                    // represented as awaiting acknowledgement.
                    client.pending_assistance_answer = None;
                }
                let stale_end_caller_request =
                    client.pending_end_caller.as_ref().and_then(|pending| {
                        if pending.is_awaiting_result() {
                            return None;
                        }
                        let lease = client.lease.as_ref()?;
                        (frame.snapshot.call_id != lease.claims.call_id
                            || frame.snapshot.call_epoch != lease.claims.call_epoch
                            || frame.snapshot.owner_epoch != lease.claims.owner_epoch
                            || frame.snapshot.switchboard_revision
                                != client
                                    .snapshot
                                    .as_ref()
                                    .map_or(frame.snapshot.switchboard_revision, |current| {
                                        current.snapshot.switchboard_revision
                                    })
                            || frame.snapshot.remote_revision
                                != client
                                    .snapshot
                                    .as_ref()
                                    .map_or(frame.snapshot.remote_revision, |current| {
                                        current.snapshot.remote_revision
                                    })
                            || frame.snapshot.telephony_state != TelephonyState::Active
                            || frame.snapshot.service_mode != ServiceMode::HumanActive
                            || !frame.grants.contains(&Grant::EndCaller)
                            || !current_consent_permits_mode(
                                &frame.snapshot.remote_consent,
                                LeaseMode::Takeover,
                                now,
                            ))
                        .then(|| pending.request_id().to_owned())
                    });
                if stale_end_caller_request.is_some() {
                    client.pending_end_caller = None;
                    failed_native_hangup_action_id = failed_native_hangup_action_id.or_else(|| {
                        client
                            .pending_native_hangup
                            .take()
                            .map(|pending| pending.action_id)
                    });
                }
                let replaced_native_call = replaced_snapshot_call(&client, &frame);
                let confirmed_revoke_action_id =
                    take_snapshot_confirmed_pending_revoke(&mut client, &frame)?
                        .and_then(|pending| pending.native_action_id);
                // Commit the call boundary before filtering its offers. A
                // reconnect can replay the spent call's cached projection,
                // while an authoritative idle/new app/new call must retire
                // that lineage. Genuinely fresh offerId/JTI pairs survive.
                retain_spent_mobile_offers_for_snapshot(&mut client, &frame);
                suppress_spent_mobile_offers(&client, &mut frame);
                client.authoritative_sequence = frame.sequence;
                client.snapshot = Some(frame.clone());
                (
                    assistance_cleared,
                    stale_end_caller_request,
                    confirmed_revoke_action_id,
                    grant_revoked_lease,
                    replaced_native_call,
                    microphone_mute_reconciliation,
                    failed_native_hangup_action_id,
                )
            };
            if let Some(action_id) = confirmed_revoke_action_id {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    true,
                    "lease_return_confirmed",
                )
                .await;
            }
            if let Some(action_id) = failed_native_hangup_action_id {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    false,
                    "caller_end_authority_changed",
                )
                .await;
            }
            if let Some(lease) = grant_revoked_lease {
                let _ = media::revoke(
                    app,
                    media_state,
                    RevokeRequest {
                        session: lease.session,
                        reason: Some(
                            "current admission or consent no longer permits this media mode".into(),
                        ),
                    },
                )
                .await;
                emit_lease_reset(app);
            }
            let microphone_session_to_disarm = match &microphone_mute_reconciliation {
                SnapshotMicrophoneMuteReconciliation::Confirmed(pending) if pending.muted => {
                    Some(pending.session.clone())
                }
                SnapshotMicrophoneMuteReconciliation::Stale(pending) => {
                    Some(pending.session.clone())
                }
                _ => None,
            };
            if let Some(session) = microphone_session_to_disarm {
                let _ =
                    media::disarm_microphone(app, media_state, SessionRequest { session }).await;
            }
            if let Some((call_id, call_epoch)) = replaced_native_call {
                let _ = crate::android_runtime::reconcile_offer(
                    app,
                    &call_id,
                    call_epoch,
                    "cancel",
                    "authoritative_call_replaced",
                )
                .await;
            }
            if let Err(message) = publish_current_android_offer(app, state).await {
                emit_error(app, &message);
            }
            app.emit("aokie-companion://v2-snapshot", frame)
                .map_err(|_| "could not deliver protocol-v2 snapshot".to_string())?;
            if assistance_cleared {
                emit_assistance_reset(app);
            }
            if let Some(request_id) = stale_end_caller_request {
                let _ = app.emit(
                    "aokie-companion://v2-end-caller",
                    EndCallerFailureEvent {
                        kind: "end_caller_failure",
                        schema_version: SCHEMA_VERSION,
                        request_id,
                        code: "stale_end_caller_state".into(),
                        message:
                            "authoritative call ownership changed before confirmation completed"
                                .into(),
                    },
                );
            }
        }
        "assistance_request" => {
            let frame: PluginAssistanceRequestFrame = strict_parse(encoded, "assistance request")?;
            let now = unix_now()?;
            let expired = match frame.validate(now) {
                Ok(()) => false,
                Err(V2ProtocolError::Expired) => true,
                Err(error) => return Err(error.to_string()),
            };
            if frame.app_id != expected_app_id {
                return Err("assistance request belongs to another application".into());
            }
            if expired {
                let cleared = {
                    let mut client = state.inner.lock().await;
                    discard_expired_assistance(&mut client, now, Some(&frame.request_id))
                };
                if cleared {
                    emit_assistance_reset(app);
                }
                return Ok(());
            }
            let publish = {
                let mut client = state.inner.lock().await;
                if assistance_was_answered(&client, &frame.app_id, &frame.request_id) {
                    false
                } else {
                    let snapshot = client
                        .snapshot
                        .as_ref()
                        .ok_or("assistance request arrived before authoritative state")?;
                    if !current_client_grants(&client).contains(&Grant::AssistanceRead)
                        || !snapshot.snapshot.remote_consent.enabled
                        || !snapshot.snapshot.remote_consent.acknowledged
                        || !snapshot.snapshot.remote_consent.assistance_enabled
                        || snapshot.snapshot.call_id != frame.call_id
                        || snapshot.snapshot.call_epoch != frame.call_epoch
                        || snapshot.snapshot.owner_epoch != frame.owner_epoch
                        || snapshot.snapshot.switchboard_revision != frame.switchboard_revision
                        || snapshot.snapshot.remote_revision != frame.remote_revision
                    {
                        return Err(
                            "assistance request failed grant, consent, or revision fencing".into(),
                        );
                    }
                    apply_assistance_request(&mut client, &frame)?
                }
            };
            if !publish {
                return Ok(());
            }
            if let Err(message) = publish_current_android_offer(app, state).await {
                emit_error(app, &message);
            }
            app.emit("aokie-companion://v2-assistance", Some(frame))
                .map_err(|_| "could not deliver protocol-v2 assistance request".to_string())?;
        }
        "assistance_answer_accepted" => {
            let frame: AssistanceAnswerAcceptedFrame =
                strict_parse(encoded, "assistance answer acknowledgement")?;
            validate_common(
                &frame.kind,
                frame.schema_version,
                Some(&frame.app_id),
                expected_app_id,
            )?;
            if !frame.accepted {
                return Err("gateway returned a negative assistance acknowledgement".into());
            }
            validate_id(&frame.request_id, "assistance requestId")?;
            validate_id(&frame.answer_id, "assistance answerId")?;
            let replay_key = assistance_answer_accepted_replay_key(&frame.request_id);
            let (publish, transfer_call) = {
                let mut client = state.inner.lock().await;
                if client.relay_transport
                    && applied_relay_frame_is_replay(&client, &replay_key, encoded)?
                {
                    return Ok(());
                }
                if assistance_was_answered(&client, &frame.app_id, &frame.request_id) {
                    (false, None)
                } else {
                    if client
                        .assistance
                        .as_ref()
                        .is_none_or(|request| request.request_id != frame.request_id)
                        || client.pending_assistance_answer.as_deref()
                            != Some(frame.answer_id.as_str())
                    {
                        return Err("unsolicited assistance acknowledgement".into());
                    }
                    let transfer_call = client.assistance.as_ref().and_then(|request| {
                        request
                            .transfer_offered
                            .then(|| (request.call_id.clone(), request.call_epoch))
                    });
                    remember_answered_assistance(&mut client, &frame.app_id, &frame.request_id);
                    client.assistance = None;
                    client.pending_assistance_answer = None;
                    (true, transfer_call)
                }
            };
            if !publish {
                return Ok(());
            }
            if let Some((call_id, call_epoch)) = transfer_call {
                let _ = crate::android_runtime::reconcile_offer(
                    app,
                    &call_id,
                    call_epoch,
                    "cancel",
                    "transfer_response_accepted",
                )
                .await;
            }
            app.emit("aokie-companion://v2-assistance-answered", frame)
                .map_err(|_| {
                    "could not deliver protocol-v2 assistance acknowledgement".to_string()
                })?;
            {
                let mut client = state.inner.lock().await;
                if client.relay_transport {
                    remember_applied_relay_frame(&mut client, replay_key, encoded)?;
                }
            }
            emit_assistance_reset(app);
        }
        "microphone_mute_status" => {
            let frame: PluginMicrophoneMuteStatusFrame =
                strict_parse(encoded, "microphone mute status")?;
            let updated =
                apply_microphone_mute_status(state, expected_app_id, frame, encoded).await?;
            if let Some(snapshot) = updated {
                app.emit("aokie-companion://v2-snapshot", snapshot)
                    .map_err(|_| {
                        "could not deliver authoritative microphone mute state".to_string()
                    })?;
            }
        }
        "end_caller_challenge" => {
            let frame: EndCallerChallengeFrame = strict_parse(encoded, "caller-ending challenge")?;
            frame
                .validate(unix_now()?)
                .map_err(|error| error.to_string())?;
            if frame.app_id != expected_app_id {
                return Err("caller-ending challenge belongs to another application".into());
            }
            let replay_key = end_caller_challenge_replay_key(&frame.request_id);
            let native_hangup = {
                let mut client = state.inner.lock().await;
                if client.relay_transport
                    && applied_relay_frame_is_replay(&client, &replay_key, encoded)?
                {
                    return Ok(());
                }
                let expected_request = match client.pending_end_caller.as_ref() {
                    Some(PendingEndCaller::AwaitingChallenge { request_id }) => request_id,
                    _ => return Err("unsolicited caller-ending challenge".into()),
                };
                if expected_request != &frame.request_id {
                    return Err("caller-ending challenge does not match its request".into());
                }
                validate_local_end_caller(&client, unix_now()?)?;
                let snapshot = client.snapshot.as_ref().expect("validated snapshot");
                let lease = client.lease.as_ref().expect("validated lease");
                if frame.device_id != client.device_id.as_deref().unwrap_or_default()
                    || frame.call_id != lease.claims.call_id
                    || frame.call_epoch != lease.claims.call_epoch
                    || frame.owner_epoch != lease.claims.owner_epoch
                    || frame.switchboard_revision != snapshot.snapshot.switchboard_revision
                    || frame.remote_revision != snapshot.snapshot.remote_revision
                    || frame.lease_id != lease.claims.lease_id
                    || frame.fence != lease.claims.fence
                {
                    return Err("caller-ending challenge crossed a live authority fence".into());
                }
                client.pending_end_caller = Some(PendingEndCaller::AwaitingConfirmation {
                    challenge: frame.clone(),
                });
                client.pending_native_hangup.is_some()
            };
            // The nonce remains native-only. A stray WebView click cannot
            // manufacture or replay the second protocol operation.
            if !native_hangup {
                app.emit(
                    "aokie-companion://v2-end-caller",
                    EndCallerChallengeEvent::from(&frame),
                )
                .map_err(|_| "could not deliver caller-ending challenge".to_string())?;
            }
            let mut client = state.inner.lock().await;
            if client.relay_transport {
                remember_applied_relay_frame(&mut client, replay_key, encoded)?;
            }
        }
        "end_caller_submitted" => {
            let frame: EndCallerSubmittedFrame =
                strict_parse(encoded, "caller-ending acknowledgement")?;
            validate_common(
                &frame.kind,
                frame.schema_version,
                Some(&frame.app_id),
                expected_app_id,
            )?;
            if !frame.accepted {
                return Err("gateway returned a negative caller-ending acknowledgement".into());
            }
            validate_id(&frame.request_id, "caller-ending requestId")?;
            validate_id(&frame.operation_id, "caller-ending operationId")?;
            validate_id(&frame.confirmation_id, "caller-ending confirmationId")?;
            let replay_key = end_caller_submitted_replay_key(&frame.request_id);
            {
                let mut client = state.inner.lock().await;
                if client.relay_transport
                    && applied_relay_frame_is_replay(&client, &replay_key, encoded)?
                {
                    return Ok(());
                }
                let (request_id, challenge) = match client.pending_end_caller.as_ref() {
                    Some(PendingEndCaller::AwaitingSubmission {
                        request_id,
                        challenge,
                    }) => (request_id.clone(), challenge.clone()),
                    _ => return Err("unsolicited caller-ending acknowledgement".into()),
                };
                if request_id != frame.request_id
                    || challenge.confirmation_id != frame.confirmation_id
                {
                    return Err("caller-ending acknowledgement crossed a confirmation fence".into());
                }
                client.pending_end_caller = Some(PendingEndCaller::AwaitingResult {
                    request_id,
                    challenge,
                    operation_id: frame.operation_id.clone(),
                });
            }
            app.emit("aokie-companion://v2-end-caller", frame)
                .map_err(|_| "could not deliver caller-ending acknowledgement".to_string())?;
            let mut client = state.inner.lock().await;
            if client.relay_transport {
                remember_applied_relay_frame(&mut client, replay_key, encoded)?;
            }
        }
        "end_caller_result" => {
            let frame: PluginEndCallerResultFrame = strict_parse(encoded, "caller-ending result")?;
            frame.validate().map_err(|error| error.to_string())?;
            if frame.app_id != expected_app_id {
                return Err("caller-ending result belongs to another application".into());
            }
            let replay_key = end_caller_result_replay_key(&frame.operation_id);
            let completed = frame.outcome == EndCallerOutcome::Completed;
            let (lease_to_close, native_hangup_action_id) = {
                let mut client = state.inner.lock().await;
                if client.relay_transport
                    && applied_relay_frame_is_replay(&client, &replay_key, encoded)?
                {
                    return Ok(());
                }
                apply_end_caller_result(&mut client, &frame)?
            };
            let emit_result = app
                .emit("aokie-companion://v2-end-caller", frame)
                .map_err(|_| "could not deliver caller-ending result".to_string());
            let replay_result = {
                let mut client = state.inner.lock().await;
                if client.relay_transport {
                    remember_applied_relay_frame(&mut client, replay_key, encoded)
                } else {
                    Ok(())
                }
            };
            if let Some(lease) = lease_to_close {
                let _ = media::revoke(
                    app,
                    media_state,
                    RevokeRequest {
                        session: lease.session,
                        reason: Some("caller end command completed".into()),
                    },
                )
                .await;
                emit_lease_reset(app);
            }
            if let Some(action_id) = native_hangup_action_id {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    completed,
                    if completed {
                        "caller_end_confirmed"
                    } else {
                        "caller_end_failed"
                    },
                )
                .await;
            }
            replay_result?;
            emit_result?;
        }
        "mobile_offer_accepted" => {
            let frame: MobileOfferAcceptedFrame =
                strict_parse(encoded, "mobile offer acknowledgement")?;
            apply_mobile_offer_accepted(state, expected_app_id, frame, encoded).await?;
        }
        "lease_granted" | "claim_provisional" | "claim_active" | "lease_renewed" => {
            let frame: LeaseStatusFrame = strict_parse(encoded, "lease status")?;
            apply_lease_status(app, state, media_state, expected_app_id, frame, encoded).await?;
        }
        "rtc_signal" => {
            let frame: GatewayRtcFrame = strict_parse(encoded, "RTC signal")?;
            apply_remote_rtc(app, state, media_state, expected_app_id, frame, encoded).await?;
        }
        "lease_revoked" => {
            let frame: LeaseRevokedFrame = strict_parse(encoded, "lease revoke")?;
            apply_revocation(app, state, media_state, expected_app_id, frame, encoded).await?;
        }
        "claim_rejected" => {
            let frame: ClaimRejectedFrame = strict_parse(encoded, "claim rejection")?;
            apply_claim_rejection(app, state, media_state, expected_app_id, frame, encoded).await?;
        }
        "error" => {
            let frame: ErrorFrame = strict_parse(encoded, "gateway error")?;
            validate_common(
                &frame.kind,
                frame.schema_version,
                frame.app_id.as_deref(),
                expected_app_id,
            )?;
            validate_text(&frame.code, 200, "gateway error code")?;
            validate_text(&frame.message, 500, "gateway error message")?;
            if matches!(
                frame.code.as_str(),
                "endpoint_reconnecting" | "renewal_not_due"
            ) {
                if let Some(request_id) = frame.request_id.as_deref() {
                    validate_id(request_id, "gateway requestId")?;
                    let client = state.inner.lock().await;
                    if !tracks_gateway_request(&client, request_id) {
                        // Lease heartbeats are deliberately not tracked as UI
                        // operations. The next due heartbeat retries with the
                        // current token, so a short plugin admission refresh
                        // or a same-second "not due" response is expected
                        // transport maintenance rather than a user-visible
                        // media failure.
                        return Ok(());
                    }
                }
            }
            let mut end_caller_failed = false;
            let mut native_hangup_failed = None;
            let mut native_answer_failed = None;
            let mut native_revoke_failed = None;
            let mut rejected_call = None;
            if let Some(request_id) = &frame.request_id {
                validate_id(request_id, "gateway requestId")?;
                let mut client = state.inner.lock().await;
                if client.pending.as_ref().is_some_and(|pending| {
                    pending.request_id == *request_id || pending.offer_request_id == *request_id
                }) {
                    if let Some(pending) = client.pending.take() {
                        rejected_call = Some((pending.call_id, pending.call_epoch));
                        native_answer_failed = pending.native_action_id;
                    }
                }
                if client
                    .pending_revoke
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == *request_id)
                {
                    native_revoke_failed = client
                        .pending_revoke
                        .take()
                        .and_then(|pending| pending.native_action_id);
                }
                if client
                    .assistance
                    .as_ref()
                    .is_some_and(|assistance| assistance.request_id == *request_id)
                {
                    client.pending_assistance_answer = None;
                }
                if client
                    .pending_end_caller
                    .as_ref()
                    .is_some_and(|pending| pending.request_id() == request_id)
                {
                    client.pending_end_caller = None;
                    native_hangup_failed = client
                        .pending_native_hangup
                        .take()
                        .map(|pending| pending.action_id);
                    end_caller_failed = true;
                }
                if client
                    .pending_microphone_mute
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == *request_id)
                {
                    client.pending_microphone_mute = None;
                }
            }
            if let Some((call_id, call_epoch)) = rejected_call {
                let _ = crate::android_runtime::reconcile_offer(
                    app,
                    &call_id,
                    call_epoch,
                    "cancel",
                    "authoritative_claim_rejected",
                )
                .await;
            }
            if let Some(action_id) = native_revoke_failed {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    false,
                    "lease_return_rejected",
                )
                .await;
            }
            if let Some(action_id) = native_answer_failed {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    false,
                    "offer_or_lease_rejected",
                )
                .await;
            }
            if let Some(action_id) = native_hangup_failed {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    false,
                    "caller_end_rejected",
                )
                .await;
            }
            if end_caller_failed {
                let _ = app.emit(
                    "aokie-companion://v2-end-caller",
                    EndCallerFailureEvent {
                        kind: "end_caller_failure",
                        schema_version: SCHEMA_VERSION,
                        request_id: frame.request_id.clone().unwrap_or_default(),
                        code: frame.code.clone(),
                        message: frame.message.clone(),
                    },
                );
            }
            emit_error(app, &format!("{} — {}", frame.code, frame.message));
        }
        _ => return Err("gateway sent an unsupported protocol-v2 message kind".into()),
    }
    Ok(())
}

async fn apply_mobile_offer_accepted(
    state: &V2State,
    expected_app_id: &str,
    frame: MobileOfferAcceptedFrame,
    encoded: &str,
) -> Result<(), String> {
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    if frame.kind != "mobile_offer_accepted" || !frame.accepted {
        return Err("gateway returned an invalid mobile offer acknowledgement".into());
    }
    for (value, label) in [
        (&frame.request_id, "mobile offer requestId"),
        (&frame.offer_id, "mobile offerId"),
        (&frame.offer_jti, "mobile offerJti"),
    ] {
        validate_id(value, label)?;
    }
    let replay_key = offer_acceptance_replay_key(&frame.request_id);
    let mut client = state.inner.lock().await;
    if applied_relay_frame_is_replay(&client, &replay_key, encoded)? {
        return Ok(());
    }
    let acceptance_is_for_current_pending = client
        .pending
        .as_ref()
        .is_some_and(|pending| pending.offer_request_id == frame.request_id);
    if !acceptance_is_for_current_pending && terminal_offer_acceptance_matches(&client, &frame)? {
        // The one-shot offer remains spent, no lease request is released, and
        // exact relay replays become no-ops. This is a terminal receipt only;
        // it never revives the timed-out attempt or opens native media. The
        // Desktop's separate accepted-offer reservation remains bounded by
        // its own 45-second setup deadline; this receipt does not release it.
        remember_applied_relay_frame(&mut client, replay_key, encoded)?;
        return Ok(());
    }
    let pending = client
        .pending
        .as_mut()
        .ok_or("unsolicited mobile offer acknowledgement")?;
    if pending.stage != PendingLeaseStage::AwaitingOfferAcceptance
        || pending.offer_request_id != frame.request_id
        || pending.accepted_offer_id != frame.offer_id
        || pending.accepted_offer_jti != frame.offer_jti
        || pending.mode != frame.offered_mode
    {
        return Err("mobile offer acknowledgement crossed its offer fence".into());
    }
    pending.stage = PendingLeaseStage::ReadyForLeaseDelivery;
    remember_applied_relay_frame(&mut client, replay_key, encoded)?;
    Ok(())
}

async fn apply_microphone_mute_status(
    state: &V2State,
    expected_app_id: &str,
    frame: PluginMicrophoneMuteStatusFrame,
    encoded: &str,
) -> Result<Option<MobileSnapshotFrame>, String> {
    frame.validate().map_err(|error| error.to_string())?;
    if frame.app_id != expected_app_id {
        return Err("microphone mute status belongs to another application".into());
    }
    let replay_key = microphone_mute_status_replay_key(&frame.request_id);
    let mut client = state.inner.lock().await;
    if client.relay_transport && applied_relay_frame_is_replay(&client, &replay_key, encoded)? {
        return Ok(None);
    }
    let pending = client
        .pending_microphone_mute
        .as_ref()
        .filter(|pending| {
            pending.request_id == frame.request_id
                && pending.stage == PendingMicrophoneMuteStage::AwaitingStatus
        })
        .ok_or("unsolicited microphone mute status")?
        .clone();
    let (mut snapshot, lease) = current_microphone_mute_authority(&client, unix_now()?)?;
    if frame.device_id != client.device_id.as_deref().unwrap_or_default()
        || frame.lease_id != pending.lease_id
        || frame.lease_jti != pending.lease_jti
        || frame.rtc_session_id != pending.rtc_session_id
        || frame.call_id != pending.call_id
        || frame.call_epoch != pending.call_epoch
        || frame.owner_epoch != pending.owner_epoch
        || frame.switchboard_revision != pending.switchboard_revision
        || frame.remote_revision <= pending.remote_revision
        || frame.fence != pending.fence
        || frame.muted != pending.muted
        || lease.claims.lease_id != pending.lease_id
        || lease.claims.rtc_session_id != pending.rtc_session_id
        || lease.claims.call_id != pending.call_id
        || lease.claims.call_epoch != pending.call_epoch
        || lease.claims.owner_epoch != pending.owner_epoch
        || lease.claims.fence != pending.fence
        || snapshot.snapshot.switchboard_revision != pending.switchboard_revision
    {
        return Err("microphone mute status crossed its exact lease or call fence".into());
    }
    if snapshot.snapshot.remote_revision > frame.remote_revision {
        if snapshot.snapshot.companion_microphone_muted != frame.muted {
            return Err("newer authoritative state contradicted microphone mute status".into());
        }
    } else {
        snapshot.snapshot.remote_revision = frame.remote_revision;
        snapshot.snapshot.companion_microphone_muted = frame.muted;
        client.snapshot = Some(snapshot.clone());
    }
    client.pending_microphone_mute = None;
    if client.relay_transport {
        remember_applied_relay_frame(&mut client, replay_key, encoded)?;
    }
    Ok(Some(snapshot))
}

async fn apply_lease_status(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    frame: LeaseStatusFrame,
    encoded: &str,
) -> Result<(), String> {
    let native_app = app.clone();
    let native_media = media_state.clone();
    let cleanup_app = app.clone();
    let cleanup_media = media_state.clone();
    let Some(applied) = apply_lease_status_transaction(
        state,
        expected_app_id,
        frame,
        encoded,
        move |operation, session, ice_servers, relay_only| async move {
            match operation {
                LeaseOperation::Create => {
                    media::create_offer(
                        &native_app,
                        &native_media,
                        CreateOfferRequest {
                            session: session.clone(),
                            ice_servers,
                            relay_only,
                        },
                    )
                    .await?;
                    if lease_operation_auto_arms_private_consult(operation, &session) {
                        // Record the exact-session intent before committing
                        // mobile authority. The media watcher waits for the
                        // SDP answer, connectivity, and remote audio; any
                        // scheduling failure therefore rolls back through the
                        // same transaction as peer creation.
                        media::request_private_consult_auto_arm(
                            &native_media,
                            SessionRequest { session },
                        )
                        .await?;
                    }
                    Ok(())
                }
                LeaseOperation::Renew => {
                    media::renew_lease(&native_app, &native_media, SessionRequest { session }).await
                }
            }
        },
        move |sessions| async move {
            // Exact session matching makes this safe even if a command or a
            // newer authority transition won the client-state race while the
            // native operation was awaiting. Never use `media::close` here:
            // it could tear down that newer peer.
            for session in sessions {
                let _ = media::revoke(
                    &cleanup_app,
                    &cleanup_media,
                    RevokeRequest {
                        session,
                        reason: Some("lease status native operation aborted".into()),
                    },
                )
                .await;
            }
            emit_lease_reset(&cleanup_app);
        },
    )
    .await?
    else {
        return Ok(());
    };

    // The state transition and its replay record are already committed as one
    // lock transaction. Android reconciliation and renderer delivery happen
    // afterwards, so neither can leave committed authority replayable as a
    // second native create/renew operation.
    if let Some((call_id, call_epoch)) = applied.reconcile_call {
        if let Err(message) = crate::android_runtime::reconcile_offer(
            app,
            &call_id,
            call_epoch,
            "won",
            "authoritative_talk_lease",
        )
        .await
        {
            emit_error(app, &message);
        }
    }
    app.emit("aokie-companion://v2-lease", applied.lease_event)
        .map_err(|_| "could not deliver protocol-v2 lease state".to_string())?;
    Ok(())
}

#[derive(Clone)]
enum LeaseStatusPredecessor {
    Pending {
        generation: u64,
        pending: PendingLease,
        native_end_action_id: Option<String>,
    },
    Lease {
        generation: u64,
        lease: ClientLease,
        native_end_action_id: Option<String>,
    },
}

impl LeaseStatusPredecessor {
    fn matches_for_commit(&self, client: &ClientState) -> bool {
        match self {
            Self::Pending {
                generation,
                pending,
                native_end_action_id,
            } => {
                client.generation == *generation
                    && client.pending.as_ref() == Some(pending)
                    && client.lease.is_none()
                    && client.pending_revoke.is_none()
                    && pending_native_end_action_id(client) == *native_end_action_id
            }
            Self::Lease {
                generation,
                lease,
                native_end_action_id,
            } => {
                client.generation == *generation
                    && client.pending.is_none()
                    && client.lease.as_ref() == Some(lease)
                    && client.pending_revoke.is_none()
                    && pending_native_end_action_id(client) == *native_end_action_id
            }
        }
    }

    /// Narrower than the commit fence on purpose. If a native End arrived
    /// during media setup, commit must fail, but the exact old authority still
    /// has to be removed so the heartbeat loop cannot renew it.
    fn matches_authority(&self, client: &ClientState) -> bool {
        match self {
            Self::Pending {
                generation,
                pending,
                ..
            } => client.generation == *generation && client.pending.as_ref() == Some(pending),
            Self::Lease {
                generation, lease, ..
            } => client.generation == *generation && client.lease.as_ref() == Some(lease),
        }
    }

    fn previous_session(&self) -> Option<MediaSession> {
        match self {
            Self::Pending { .. } => None,
            Self::Lease { lease, .. } => Some(lease.session.clone()),
        }
    }
}

fn pending_native_end_action_id(client: &ClientState) -> Option<String> {
    client
        .pending_native_end
        .as_ref()
        .map(|pending| pending.action.action_id.clone())
}

struct LeaseStatusPlan {
    replay_key: String,
    predecessor: LeaseStatusPredecessor,
    operation: LeaseOperation,
    proposed: ClientLease,
    lease_event: LeaseEvent,
    reconcile_call: Option<(String, u64)>,
    ice_servers: Vec<IceServerConfig>,
    relay_only: bool,
}

impl LeaseStatusPlan {
    fn cleanup_sessions(&self) -> Vec<MediaSession> {
        let mut sessions = vec![self.proposed.session.clone()];
        if let Some(previous) = self.predecessor.previous_session() {
            if !sessions.contains(&previous) {
                sessions.push(previous);
            }
        }
        sessions
    }
}

#[derive(Debug)]
struct AppliedLeaseStatus {
    lease_event: LeaseEvent,
    reconcile_call: Option<(String, u64)>,
}

async fn apply_lease_status_transaction<Native, NativeFuture, Cleanup, CleanupFuture>(
    state: &V2State,
    expected_app_id: &str,
    frame: LeaseStatusFrame,
    encoded: &str,
    native: Native,
    cleanup: Cleanup,
) -> Result<Option<AppliedLeaseStatus>, String>
where
    Native: FnOnce(LeaseOperation, MediaSession, Vec<IceServerConfig>, bool) -> NativeFuture,
    NativeFuture: std::future::Future<Output = Result<(), String>>,
    Cleanup: FnOnce(Vec<MediaSession>) -> CleanupFuture,
    CleanupFuture: std::future::Future<Output = ()>,
{
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    validate_id(&frame.request_id, "lease requestId")?;
    if frame.lease_token.is_empty() || frame.lease_token.len() > MAX_LEASE_TOKEN_BYTES {
        return Err("lease token is invalid".into());
    }
    let replay_key = lease_status_replay_key(&frame.kind, &frame.request_id);
    {
        let client = state.inner.lock().await;
        if applied_relay_frame_is_replay(&client, &replay_key, encoded)? {
            return Ok(None);
        }
    }
    let now = unix_now()?;
    let expired_before_status = {
        let mut client = state.inner.lock().await;
        take_expired_lease_locked(state, &mut client, now)
    };
    if let Some((expired, queue_error)) = expired_before_status {
        let containment = {
            let mut client = state.inner.lock().await;
            contain_late_local_expiry_renewal(state, &mut client, &frame, &replay_key, encoded, now)
        };
        cleanup(vec![expired.session]).await;
        if let Some(message) = queue_error {
            eprintln!(
                "[AokieCompanion][realtime] local-expiry return could not be queued: {message}"
            );
        }
        return match containment {
            Ok(true) => Ok(None),
            Ok(false) => Err("lease status arrived after the current media lease expired".into()),
            Err(message) => Err(message),
        };
    }
    {
        let mut client = state.inner.lock().await;
        if applied_relay_frame_is_replay(&client, &replay_key, encoded)? {
            return Ok(None);
        }
        if contain_late_local_expiry_renewal(state, &mut client, &frame, &replay_key, encoded, now)?
        {
            // Local media has already failed closed. Authenticate and advance
            // only the tombstoned token lineage before considering current
            // grants/consent, then return it without touching any newer peer.
            return Ok(None);
        }
    }
    frame
        .lease
        .validate(now)
        .map_err(|error| error.to_string())?;

    let plan = {
        let mut client = state.inner.lock().await;
        if applied_relay_frame_is_replay(&client, &replay_key, encoded)? {
            return Ok(None);
        }
        if contain_late_local_expiry_renewal(state, &mut client, &frame, &replay_key, encoded, now)?
        {
            // A local expiry could win while current-time validation was
            // outside the lock; preserve the same exact no-native outcome.
            return Ok(None);
        }
        purge_local_expiry_tombstones(&mut client, Instant::now());
        if matches!(frame.kind.as_str(), "lease_granted" | "claim_provisional")
            && client.local_expiry_tombstones.len() >= MAX_LOCAL_EXPIRY_TOMBSTONES.saturating_sub(1)
        {
            return Err("new media authority is waiting for old safety fences to expire".into());
        }
        validate_claim_identity(&client, &frame.lease)?;
        let snapshot = client
            .snapshot
            .as_ref()
            .ok_or("lease status arrived before current authoritative grants")?;
        if !grants_permit_lease_mode(current_client_grants(&client), frame.lease.mode) {
            return Err("lease status is not permitted by current admission grants".into());
        }
        if !current_consent_permits_mode(&snapshot.snapshot.remote_consent, frame.lease.mode, now) {
            return Err("lease status is not permitted by current remote consent".into());
        }
        let (predecessor, operation, proposed, lease_event, reconcile_call) = match frame
            .kind
            .as_str()
        {
            "lease_granted" | "claim_provisional" => {
                let pending = client
                    .pending
                    .as_ref()
                    .ok_or("unsolicited protocol-v2 lease status")?;
                if pending.stage != PendingLeaseStage::LeaseRequested
                    || pending.request_id != frame.request_id
                    || pending.mode != frame.lease.mode
                    || pending.rtc_session_id != frame.lease.rtc_session_id
                {
                    return Err("protocol-v2 lease status does not match its request".into());
                }
                let provisional = frame.kind == "claim_provisional";
                if provisional != frame.provisional.unwrap_or(provisional)
                    || provisional != matches!(frame.lease.phase, LeasePhase::Prepared)
                    || (!provisional && !matches!(frame.lease.mode, LeaseMode::Monitor))
                {
                    return Err("protocol-v2 lease phase does not match its status".into());
                }
                let session = session_from_claims(&frame.lease, 1, 1)?;
                let lease = ClientLease {
                    request_id: frame.request_id.clone(),
                    token: frame.lease_token.clone(),
                    claims: frame.lease.clone(),
                    session: session.clone(),
                };
                (
                    LeaseStatusPredecessor::Pending {
                        generation: client.generation,
                        pending: pending.clone(),
                        native_end_action_id: pending_native_end_action_id(&client),
                    },
                    LeaseOperation::Create,
                    lease,
                    LeaseEvent {
                        session,
                        mode: frame.lease.mode,
                        phase: frame.lease.phase,
                        provisional,
                    },
                    None,
                )
            }
            "claim_active" => {
                let current = client
                    .lease
                    .as_ref()
                    .ok_or("claim_active has no provisional lease")?;
                if current.request_id != frame.request_id
                    || !matches!(current.claims.phase, LeasePhase::Prepared)
                    || !matches!(frame.lease.phase, LeasePhase::Active)
                    || current.claims.lease_id != frame.lease.lease_id
                    || current.claims.rtc_session_id != frame.lease.rtc_session_id
                    || current.claims.mode != frame.lease.mode
                    || frame.lease.owner_epoch <= current.claims.owner_epoch
                    || frame.provisional == Some(true)
                {
                    return Err("claim_active did not safely advance provisional authority".into());
                }
                let sdp_revision = current
                    .session
                    .sdp_revision
                    .checked_add(1)
                    .filter(|value| *value <= MAX_SAFE_INTEGER)
                    .ok_or("native SDP revision exhausted")?;
                let transport_generation = current
                    .session
                    .transport_generation
                    .checked_add(1)
                    .filter(|value| *value <= MAX_SAFE_INTEGER)
                    .ok_or("native transport generation exhausted")?;
                let session =
                    session_from_claims(&frame.lease, sdp_revision, transport_generation)?;
                let reconcile_call = (frame.lease.mode == LeaseMode::Takeover
                    && client.pending_native_end.is_none())
                .then(|| (frame.lease.call_id.clone(), frame.lease.call_epoch));
                let lease = ClientLease {
                    request_id: frame.request_id.clone(),
                    token: frame.lease_token.clone(),
                    claims: frame.lease.clone(),
                    session: session.clone(),
                };
                (
                    LeaseStatusPredecessor::Lease {
                        generation: client.generation,
                        lease: current.clone(),
                        native_end_action_id: pending_native_end_action_id(&client),
                    },
                    LeaseOperation::Create,
                    lease,
                    LeaseEvent {
                        session,
                        mode: frame.lease.mode,
                        phase: frame.lease.phase,
                        provisional: false,
                    },
                    reconcile_call,
                )
            }
            "lease_renewed" => {
                let current = client
                    .lease
                    .as_ref()
                    .ok_or("lease_renewed has no current lease")?;
                validate_renewal(current, &frame.lease)?;
                let mut session = current.session.clone();
                session.expires_at = expiry_datetime(frame.lease.expires_at)?;
                let lease = ClientLease {
                    request_id: current.request_id.clone(),
                    token: frame.lease_token.clone(),
                    claims: frame.lease.clone(),
                    session: session.clone(),
                };
                (
                    LeaseStatusPredecessor::Lease {
                        generation: client.generation,
                        lease: current.clone(),
                        native_end_action_id: pending_native_end_action_id(&client),
                    },
                    LeaseOperation::Renew,
                    lease,
                    LeaseEvent {
                        session,
                        mode: frame.lease.mode,
                        phase: frame.lease.phase,
                        provisional: matches!(frame.lease.phase, LeasePhase::Prepared),
                    },
                    None,
                )
            }
            _ => return Err("unsupported lease status".into()),
        };

        let reservation = AppliedRelayFrame {
            key: replay_key.clone(),
            digest: relay_frame_digest(encoded),
        };
        if let Some(inflight) = client.applying_lease_status.as_ref() {
            if inflight == &reservation {
                // The first application owns the native transition. An exact
                // duplicate must never start another peer operation.
                return Ok(None);
            }
            return Err("another lease status is already applying native media".into());
        }
        client.applying_lease_status = Some(reservation);
        LeaseStatusPlan {
            replay_key,
            predecessor,
            operation,
            proposed,
            lease_event,
            reconcile_call,
            ice_servers: client.ice_servers.clone(),
            relay_only: client.relay_only,
        }
    };

    let native_result = native(
        plan.operation,
        plan.proposed.session.clone(),
        plan.ice_servers.clone(),
        plan.relay_only,
    )
    .await;
    let failure = match native_result {
        Err(message) => Some(message),
        Ok(()) => match commit_lease_status(state, &plan, encoded).await {
            Ok(applied) => return Ok(Some(applied)),
            Err(message) => Some(message),
        },
    };

    let message = failure.expect("one transaction failure branch was selected");
    let abort_result = abort_lease_status(state, &plan, encoded).await;
    let contained_current_authority = matches!(&abort_result, Ok(true));
    let abort_error = abort_result.err();
    cleanup(plan.cleanup_sessions()).await;
    if plan.operation == LeaseOperation::Renew && contained_current_authority {
        // The native watcher can close exactly at MediaSession::expires_at
        // before this task receives a still-valid authoritative renewal. The
        // renewed token has already been returned and replay-fenced by the
        // abort transaction, so this is a contained fail-closed outcome rather
        // than a signalling failure. Never recreate the expired peer here.
        eprintln!(
            "[AokieCompanion][realtime] contained lease renewal after native media had already closed: {message}"
        );
        return Ok(None);
    }
    match abort_error {
        Some(abort_error) => Err(format!("{message}; {abort_error}")),
        None => Err(message),
    }
}

async fn commit_lease_status(
    state: &V2State,
    plan: &LeaseStatusPlan,
    encoded: &str,
) -> Result<AppliedLeaseStatus, String> {
    let mut client = state.inner.lock().await;
    let reservation = AppliedRelayFrame {
        key: plan.replay_key.clone(),
        digest: relay_frame_digest(encoded),
    };
    if client.applying_lease_status.as_ref() != Some(&reservation) {
        return Err("lease status lost its in-flight replay reservation".into());
    }
    if !plan.predecessor.matches_for_commit(&client) {
        return Err("lease status predecessor changed while native media was opening".into());
    }

    // Record replay and install authority while the same lock is held. No
    // external event can observe one without the other.
    remember_applied_relay_frame(&mut client, plan.replay_key.clone(), encoded)?;
    match &plan.predecessor {
        LeaseStatusPredecessor::Pending { .. } => client.pending = None,
        LeaseStatusPredecessor::Lease { .. } => {}
    }
    client.lease = Some(plan.proposed.clone());
    client.applying_lease_status = None;
    Ok(AppliedLeaseStatus {
        lease_event: plan.lease_event.clone(),
        reconcile_call: plan.reconcile_call.clone(),
    })
}

async fn abort_lease_status(
    state: &V2State,
    plan: &LeaseStatusPlan,
    encoded: &str,
) -> Result<bool, String> {
    let request_id = state.next_id("request");
    let revoke = LeaseRevokeFrame {
        kind: "lease_revoke".into(),
        schema_version: SCHEMA_VERSION,
        app_id: plan.proposed.claims.app_id.clone(),
        request_id: request_id.clone(),
        idempotency_key: format!(
            "mobile:{}:{}",
            plan.proposed.claims.device_id,
            state.next_id("native_failure_revoke")
        ),
        // The Desktop already accepted THIS token. Never synthesize authority
        // from the predecessor or a later snapshot when giving it back.
        lease_token: plan.proposed.token.clone(),
        reason: "native_media_setup_failed".into(),
    };
    revoke.validate().map_err(|error| error.to_string())?;
    let revoke_encoded = serde_json::to_string(&revoke)
        .map_err(|_| "could not encode failed native-media lease revoke".to_string())?;

    let mut client = state.inner.lock().await;
    let reservation = AppliedRelayFrame {
        key: plan.replay_key.clone(),
        digest: relay_frame_digest(encoded),
    };
    if client.applying_lease_status.as_ref() != Some(&reservation) {
        return Err("failed lease status lost its in-flight replay reservation".into());
    }

    let authority_was_current = plan.predecessor.matches_authority(&client);
    if authority_was_current {
        match &plan.predecessor {
            LeaseStatusPredecessor::Pending { .. } => client.pending = None,
            LeaseStatusPredecessor::Lease { .. } => client.lease = None,
        }
        client.pending_end_caller = None;
        client.pending_microphone_mute = None;
    }

    let queue_error = if client.urgent_control_frames.len() >= MAX_URGENT_CONTROL_FRAMES {
        Some("urgent lease-revoke queue reached its safety bound".to_string())
    } else {
        client.urgent_control_frames.push_back(UrgentControlFrame {
            app_id: plan.proposed.claims.app_id.clone(),
            request_id: request_id.clone(),
            lease_id: plan.proposed.claims.lease_id.clone(),
            lease_jti: plan.proposed.claims.jti.clone(),
            rtc_session_id: plan.proposed.claims.rtc_session_id.clone(),
            fence: plan.proposed.claims.fence,
            encoded: revoke_encoded,
        });
        None
    };

    if authority_was_current && client.pending_revoke.is_none() {
        let authoritative_sequence = client.authoritative_sequence;
        let authoritative_remote_revision = client
            .snapshot
            .as_ref()
            .filter(|snapshot| {
                snapshot.app_id == plan.proposed.claims.app_id
                    && snapshot.snapshot.call_id == plan.proposed.claims.call_id
                    && snapshot.snapshot.call_epoch == plan.proposed.claims.call_epoch
            })
            .map(|snapshot| snapshot.snapshot.remote_revision);
        client.pending_revoke = Some(PendingRevoke {
            request_id,
            lease_id: plan.proposed.claims.lease_id.clone(),
            lease_jti: plan.proposed.claims.jti.clone(),
            lease: plan.proposed.clone(),
            authoritative_sequence,
            authoritative_remote_revision,
            native_action_id: None,
            deadline: Instant::now() + REVOKE_CONFIRM_TIMEOUT,
        });
    }
    let tombstone_error = if authority_was_current && plan.operation == LeaseOperation::Renew {
        let retain_until = Instant::now() + LOCAL_EXPIRY_TOMBSTONE_TTL;
        let ancestor_error =
            plan.predecessor
                .previous_session()
                .and_then(|_| match &plan.predecessor {
                    LeaseStatusPredecessor::Lease { lease, .. } => {
                        push_local_expiry_tombstone(&mut client, lease.clone(), retain_until, false)
                            .err()
                    }
                    LeaseStatusPredecessor::Pending { .. } => None,
                });
        ancestor_error.or_else(|| {
            push_local_expiry_tombstone(&mut client, plan.proposed.clone(), retain_until, true)
                .err()
        })
    } else {
        None
    };

    remember_applied_relay_frame(&mut client, plan.replay_key.clone(), encoded)?;
    client.applying_lease_status = None;
    match queue_error.or(tombstone_error) {
        Some(message) => Err(message),
        None => Ok(authority_was_current),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaseOperation {
    Create,
    Renew,
}

fn lease_operation_auto_arms_private_consult(
    operation: LeaseOperation,
    session: &MediaSession,
) -> bool {
    operation == LeaseOperation::Create && session.binding.mode == MediaMode::Consult
}

type RemoteRtcAuthentication = (AdmissionRole, String, String, String);

fn remote_rtc_authentication(
    signal: &RtcSignal,
    now: u64,
) -> Result<Option<RemoteRtcAuthentication>, V2ProtocolError> {
    signal
        .verify_endpoint_authentication(now)
        .map(|authentication| {
            authentication.map(|authentication| {
                (
                    authentication.endpoint_role(),
                    authentication.holder_key_thumbprint().to_owned(),
                    authentication.peer_key_thumbprint().to_owned(),
                    authentication.jti().to_owned(),
                )
            })
        })
}

fn rtc_signal_authentication_window(signal: &RtcSignal) -> Option<(u64, u64)> {
    match signal {
        RtcSignal::Offer { binding, .. } | RtcSignal::Answer { binding, .. } => {
            Some((binding.claims.issued_at, binding.claims.expires_at))
        }
        RtcSignal::Ice { envelope, .. } | RtcSignal::IceComplete { envelope } => {
            Some((envelope.claims.issued_at, envelope.claims.expires_at))
        }
        RtcSignal::Close { .. } => None,
    }
}

fn rtc_outer_matches_lease(frame: &GatewayRtcFrame, lease: &ClientLease) -> bool {
    frame.app_id == lease.claims.app_id
        && frame.plugin_id == lease.claims.plugin_id
        && frame.device_id == lease.claims.device_id
        && frame.lease_jti == lease.claims.jti
        && frame.rtc_session_id == lease.claims.rtc_session_id
        && frame.call_id == lease.claims.call_id
        && frame.call_epoch == lease.claims.call_epoch
        && frame.owner_epoch == lease.claims.owner_epoch
        && frame.fence == lease.claims.fence
        && frame.sdp_revision == lease.session.sdp_revision
        && frame.transport_generation == lease.session.transport_generation
}

/// Digest an exact SDP/ICE/Close frame for a lease whose native peer already
/// crossed a local fail-closed boundary. Signed media proofs normally expire
/// with the lease, so an actually-expired proof is rechecked inside its
/// original validity window before being dropped. This never creates, renews,
/// closes, or otherwise calls native media.
fn contain_locally_closed_rtc(
    client: &mut ClientState,
    frame: &GatewayRtcFrame,
    replay_key: &str,
    encoded: &str,
    now: u64,
) -> Result<bool, String> {
    purge_local_expiry_tombstones(client, Instant::now());
    let lease = client
        .pending_revoke
        .as_ref()
        .filter(|pending| pending.lease_jti == frame.lease_jti)
        .map(|pending| pending.lease.clone())
        .or_else(|| {
            client
                .local_expiry_tombstones
                .iter()
                .find(|tombstone| tombstone.lease.claims.jti == frame.lease_jti)
                .map(|tombstone| tombstone.lease.clone())
        });
    let Some(lease) = lease else {
        return Ok(false);
    };
    if !rtc_outer_matches_lease(frame, &lease) {
        return Err("RTC signal crossed the locally-closed lease fence".into());
    }

    let authentication = match remote_rtc_authentication(&frame.signal, now) {
        Ok(authentication) => authentication,
        Err(V2ProtocolError::Expired) => {
            let Some((_issued_at, expires_at)) = rtc_signal_authentication_window(&frame.signal)
            else {
                return Err("Desktop RTC endpoint signature is invalid or stale".into());
            };
            if expires_at > now {
                // `Expired` with a future expiry is a future-issued or
                // malformed proof, never delayed relay mail.
                return Err("Desktop RTC endpoint signature is invalid or stale".into());
            }
            remote_rtc_authentication(&frame.signal, expires_at.saturating_sub(1))
                .map_err(|_| "Desktop RTC endpoint signature is invalid or stale".to_string())?
        }
        Err(_) => return Err("Desktop RTC endpoint signature is invalid or stale".into()),
    };
    if !validate_remote_signal_route(&frame.signal, &lease, frame) {
        return Err("signed Desktop RTC route crossed the locally-closed lease fence".into());
    }
    if let Some((role, holder, peer, jti)) = authentication {
        if role != AdmissionRole::Plugin
            || holder != lease.claims.plugin_key_thumbprint
            || peer != lease.claims.mobile_key_thumbprint
            || !client.seen_remote_endpoint_jtis.insert(jti)
        {
            return Err("Desktop RTC endpoint identity was substituted or replayed".into());
        }
    } else if !matches!(frame.signal, RtcSignal::Close { .. }) {
        return Err("Desktop RTC media signal omitted endpoint authentication".into());
    }
    remember_applied_relay_frame(client, replay_key.to_owned(), encoded)?;
    Ok(true)
}

async fn apply_remote_rtc(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    frame: GatewayRtcFrame,
    encoded: &str,
) -> Result<(), String> {
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    validate_id(&frame.signal_id, "RTC signalId")?;
    frame.signal.validate().map_err(|error| error.to_string())?;
    let replay_key = rtc_signal_replay_key(&frame.signal_id);
    let now = unix_now()?;
    let expired_before_rtc = {
        let mut client = state.inner.lock().await;
        take_expired_lease_locked(state, &mut client, now)
    };
    if let Some((expired, queue_error)) = expired_before_rtc {
        let _ = media::revoke(
            app,
            media_state,
            RevokeRequest {
                session: expired.session,
                reason: Some("media lease expired before inbound RTC signalling".into()),
            },
        )
        .await;
        emit_lease_reset(app);
        if let Some(message) = queue_error {
            eprintln!(
                "[AokieCompanion][realtime] local-expiry return could not be queued: {message}"
            );
        }
    }
    {
        let mut client = state.inner.lock().await;
        if applied_relay_frame_is_replay(&client, &replay_key, encoded)? {
            return Ok(());
        }
        if client.device_id.as_deref() != Some(frame.device_id.as_str()) {
            return Err("RTC signal targets another Companion device".into());
        }
        if contain_locally_closed_rtc(&mut client, &frame, &replay_key, encoded, now)? {
            return Ok(());
        }
        let current_grants = current_client_grants(&client);
        let active_mode = client
            .lease
            .as_ref()
            .and_then(|lease| (lease.claims.jti == frame.lease_jti).then_some(lease.claims.mode));
        let consent_permits_routed_mode = active_mode.is_none_or(|mode| {
            client.snapshot.as_ref().is_some_and(|snapshot| {
                current_consent_permits_mode(&snapshot.snapshot.remote_consent, mode, now)
            })
        });
        if !current_grants.contains(&Grant::RtcSignal)
            || active_mode.is_some_and(|mode| !grants_permit_lease_mode(current_grants, mode))
            || !consent_permits_routed_mode
        {
            // A queued old signal is not an error from the current session.
            // Digest it so exact redelivery remains a no-op, then discard it
            // before endpoint verification or native media can run.
            remember_applied_relay_frame(&mut client, replay_key, encoded)?;
            return Ok(());
        }
    }
    let authentication = remote_rtc_authentication(&frame.signal, now)
        .map_err(|_| "Desktop RTC endpoint signature is invalid or stale".to_string())?;
    let lease = {
        let mut client = state.inner.lock().await;
        if applied_relay_frame_is_replay(&client, &replay_key, encoded)? {
            return Ok(());
        }
        if contain_locally_closed_rtc(&mut client, &frame, &replay_key, encoded, now)? {
            return Ok(());
        }
        let lease = client
            .lease
            .as_ref()
            .filter(|lease| lease.claims.jti == frame.lease_jti)
            .cloned()
            .ok_or("RTC signal arrived without a matching media lease fence")?;
        let current_grants = current_client_grants(&client);
        if client.device_id.as_deref() != Some(frame.device_id.as_str())
            || !grants_permit_lease_mode(current_grants, lease.claims.mode)
            || client.snapshot.as_ref().is_none_or(|snapshot| {
                !current_consent_permits_mode(
                    &snapshot.snapshot.remote_consent,
                    lease.claims.mode,
                    now,
                )
            })
            || !rtc_outer_matches_lease(&frame, &lease)
            || lease.claims.expires_at <= now
        {
            return Err("RTC signal does not match the current lease fence".into());
        }
        if !validate_remote_signal_route(&frame.signal, &lease, &frame) {
            return Err("signed Desktop RTC route does not match the current lease".into());
        }
        if let Some((role, holder, peer, jti)) = authentication {
            if role != AdmissionRole::Plugin
                || holder != lease.claims.plugin_key_thumbprint
                || peer != lease.claims.mobile_key_thumbprint
                || !client.seen_remote_endpoint_jtis.insert(jti)
            {
                return Err("Desktop RTC endpoint identity was substituted or replayed".into());
            }
        } else if !matches!(frame.signal, RtcSignal::Close { .. }) {
            return Err("Desktop RTC media signal omitted endpoint authentication".into());
        }
        lease
    };

    let result = match frame.signal {
        RtcSignal::Answer { sdp, .. } => {
            media::accept_answer(
                app,
                media_state,
                AcceptAnswerRequest {
                    session: lease.session,
                    answer: SdpSignal {
                        kind: SdpSignalType::Answer,
                        sdp,
                    },
                },
            )
            .await
        }
        RtcSignal::Ice {
            candidate,
            sdp_mid,
            sdp_m_line_index,
            ..
        } => {
            media::add_ice_candidate(
                media_state,
                AddIceRequest {
                    session: lease.session,
                    candidate: IceCandidateSignal {
                        sdp_mid: sdp_mid.unwrap_or_default(),
                        sdp_mline_index: i32::from(sdp_m_line_index.unwrap_or(0)),
                        candidate,
                    },
                },
            )
            .await
        }
        RtcSignal::IceComplete { .. } => Ok(()),
        RtcSignal::Close { reason } => {
            {
                let mut client = state.inner.lock().await;
                if client
                    .lease
                    .as_ref()
                    .is_some_and(|current| current.claims.jti == lease.claims.jti)
                {
                    client.lease = None;
                    client.pending_microphone_mute = None;
                }
                client.pending_end_caller = None;
            }
            let result = media::revoke(
                app,
                media_state,
                RevokeRequest {
                    session: lease.session,
                    reason: Some(reason),
                },
            )
            .await;
            emit_lease_reset(app);
            result
        }
        RtcSignal::Offer { .. } => Err("the Companion is the protocol-v2 offerer".into()),
    };
    result?;
    {
        let mut client = state.inner.lock().await;
        remember_applied_relay_frame(&mut client, replay_key, encoded)?;
    }
    Ok(())
}

fn validate_remote_signal_route(
    signal: &RtcSignal,
    lease: &ClientLease,
    frame: &GatewayRtcFrame,
) -> bool {
    let matches = |app_id: &str,
                   plugin_id: &str,
                   device_id: &str,
                   rtc_session_id: &str,
                   lease_jti: &str,
                   endpoint_role: AdmissionRole,
                   holder: &str,
                   peer: &str,
                   call_id: &str,
                   call_epoch: u64,
                   owner_epoch: u64,
                   fence: u64,
                   sdp_revision: u64,
                   transport_generation: u64| {
        app_id == frame.app_id
            && plugin_id == frame.plugin_id
            && device_id == frame.device_id
            && rtc_session_id == frame.rtc_session_id
            && lease_jti == frame.lease_jti
            && endpoint_role == AdmissionRole::Plugin
            && holder == lease.claims.plugin_key_thumbprint
            && peer == lease.claims.mobile_key_thumbprint
            && call_id == frame.call_id
            && call_epoch == frame.call_epoch
            && owner_epoch == frame.owner_epoch
            && fence == frame.fence
            && sdp_revision == frame.sdp_revision
            && transport_generation == frame.transport_generation
    };
    match signal {
        RtcSignal::Offer { binding, .. } | RtcSignal::Answer { binding, .. } => {
            let claims = &binding.claims;
            matches(
                &claims.app_id,
                &claims.plugin_id,
                &claims.device_id,
                &claims.rtc_session_id,
                &claims.lease_jti,
                claims.endpoint_role,
                &claims.holder_key_thumbprint,
                &claims.peer_key_thumbprint,
                &claims.call_id,
                claims.call_epoch,
                claims.owner_epoch,
                claims.fence,
                claims.sdp_revision,
                claims.transport_generation,
            )
        }
        RtcSignal::Ice { envelope, .. } | RtcSignal::IceComplete { envelope } => {
            let claims = &envelope.claims;
            matches(
                &claims.app_id,
                &claims.plugin_id,
                &claims.device_id,
                &claims.rtc_session_id,
                &claims.lease_jti,
                claims.endpoint_role,
                &claims.holder_key_thumbprint,
                &claims.peer_key_thumbprint,
                &claims.call_id,
                claims.call_epoch,
                claims.owner_epoch,
                claims.fence,
                claims.sdp_revision,
                claims.transport_generation,
            )
        }
        RtcSignal::Close { .. } => true,
    }
}

async fn apply_revocation(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    frame: LeaseRevokedFrame,
    encoded: &str,
) -> Result<(), String> {
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    if let Some(request_id) = &frame.request_id {
        validate_id(request_id, "lease revoke requestId")?;
    }
    validate_id(&frame.lease_id, "leaseId")?;
    validate_id(&frame.lease_jti, "leaseJti")?;
    validate_text(&frame.reason, 500, "lease revoke reason")?;
    let (lease, local_media_already_closed, native_action_id, emit_reset) = {
        let mut client = state.inner.lock().await;
        if completed_revoke_frame_is_replay(&mut client, &frame, encoded)? {
            return Ok(());
        }
        purge_local_expiry_tombstones(&mut client, Instant::now());

        let pending_matches = pending_revoke_identity_matches(&client, &frame)?;
        let tombstone_index = client.local_expiry_tombstones.iter().position(|tombstone| {
            tombstone.lease.claims.lease_id == frame.lease_id
                && tombstone.lease.claims.jti == frame.lease_jti
        });
        let tombstone_matches = tombstone_index.is_some();
        let current_matches = client.lease.as_ref().is_some_and(|lease| {
            lease.claims.lease_id == frame.lease_id && lease.claims.jti == frame.lease_jti
        });

        if pending_matches {
            let pending = client
                .pending_revoke
                .as_ref()
                .expect("matching pending revoke was checked");
            // Preserve the locally-known request fence before merging a
            // requestless plugin media-failure notice. Otherwise the first
            // arbitrary Some(requestId) delivered later could claim this
            // already-completed lease/JTI tombstone.
            let completed_pending = completed_revoke_from_pending(pending);
            remember_completed_revoke(&mut client, completed_pending)?;
            remember_completed_revoke(&mut client, completed_revoke_from_frame(&frame, encoded))?;
            let pending = client
                .pending_revoke
                .take()
                .expect("checked pending revoke");
            if tombstone_matches {
                demote_local_expiry_tombstone(&mut client, &pending.lease_jti);
            }
            client.urgent_control_frames.retain(|queued| {
                queued.lease_id != pending.lease_id || queued.lease_jti != pending.lease_jti
            });
            let newer_authority_exists = client.lease.as_ref().is_some_and(|lease| {
                lease.claims.lease_id != pending.lease_id || lease.claims.jti != pending.lease_jti
            });
            if !newer_authority_exists {
                client.pending_end_caller = None;
                client.pending_microphone_mute = None;
            }
            (
                pending.lease,
                true,
                pending.native_action_id,
                !newer_authority_exists,
            )
        } else if tombstone_matches {
            // A locally-expired acknowledgement belongs to the tombstone
            // before it belongs to any newer active lease. Digest it without
            // clearing current commands or emitting a renderer lease reset.
            remember_completed_revoke(&mut client, completed_revoke_from_frame(&frame, encoded))?;
            let tombstone = client
                .local_expiry_tombstones
                .get(tombstone_index.expect("matching tombstone index was checked"))
                .expect("matching local expiry tombstone was checked")
                .clone();
            demote_local_expiry_tombstone(&mut client, &tombstone.lease.claims.jti);
            client.urgent_control_frames.retain(|queued| {
                queued.lease_id != tombstone.lease.claims.lease_id
                    || queued.lease_jti != tombstone.lease.claims.jti
            });
            let newer_authority_exists = client.lease.is_some() || client.pending.is_some();
            (tombstone.lease, true, None, !newer_authority_exists)
        } else if current_matches {
            remember_completed_revoke(&mut client, completed_revoke_from_frame(&frame, encoded))?;
            client.pending_end_caller = None;
            client.pending_microphone_mute = None;
            (
                client
                    .lease
                    .take()
                    .expect("matching current lease was checked"),
                false,
                None,
                true,
            )
        } else {
            if let Some(pending) = client.pending_revoke.as_ref() {
                validate_pending_revoke_ack(pending, &frame)?;
                unreachable!("a successfully validated pending acknowledgement matched above");
            } else if client.lease.is_some() {
                return Err("stale lease revocation did not match the current JTI".into());
            } else if !client.local_expiry_tombstones.is_empty() {
                return Err("stale lease revocation did not match the local expiry fence".into());
            } else {
                return Err("lease revocation did not match a local lease".into());
            }
        }
    };
    let result = if local_media_already_closed {
        Ok(())
    } else {
        media::revoke(
            app,
            media_state,
            RevokeRequest {
                session: lease.session,
                reason: Some(frame.reason),
            },
        )
        .await
    };
    if emit_reset {
        emit_lease_reset(app);
    }
    if let Some(action_id) = native_action_id {
        crate::android_runtime::complete_native_call_action(
            app,
            &action_id,
            true,
            "lease_return_confirmed",
        )
        .await?;
    }
    result
}

fn validate_pending_revoke_ack(
    pending: &PendingRevoke,
    frame: &LeaseRevokedFrame,
) -> Result<(), String> {
    if frame.lease_id != pending.lease_id
        || frame.lease_jti != pending.lease_jti
        || frame
            .request_id
            .as_deref()
            .is_some_and(|request_id| request_id != pending.request_id)
    {
        return Err("lease return acknowledgement crossed its request or JTI fence".into());
    }
    Ok(())
}

fn pending_revoke_identity_matches(
    client: &ClientState,
    frame: &LeaseRevokedFrame,
) -> Result<bool, String> {
    let Some(pending) = client.pending_revoke.as_ref() else {
        return Ok(false);
    };
    let identity_matches =
        frame.lease_id == pending.lease_id && frame.lease_jti == pending.lease_jti;
    if identity_matches {
        // Do this before tombstone routing: the tombstone names the same
        // locally-closed lease but deliberately does not retain the outbound
        // request ID, so it must never weaken that live request fence.
        validate_pending_revoke_ack(pending, frame)?;
    }
    Ok(identity_matches)
}

/// When the relay provides no exact revoke acknowledgement, a lease return is
/// complete only once a later authoritative call projection proves that the
/// revoked route can no longer own the caller.
///
/// The translated relay sequence alone is not source authority: a same-call
/// projection must also advance Desktop's remote revision beyond the request
/// baseline. Monitor additionally requires Aokie's media route to be ready;
/// consult and takeover require the owner epoch to advance beyond the revoked
/// lease. A different/ended call makes the old lease harmless regardless of
/// mode. Until one of these transitions arrives the timeout remains armed.
fn snapshot_confirms_pending_revoke(pending: &PendingRevoke, frame: &MobileSnapshotFrame) -> bool {
    if frame.sequence <= pending.authoritative_sequence {
        return false;
    }

    let revoked = &pending.lease.claims;
    let current = &frame.snapshot;
    if current.call_id != revoked.call_id
        || current.call_epoch != revoked.call_epoch
        || current.telephony_state == TelephonyState::Ended
        || current.service_mode == ServiceMode::Ended
    {
        return true;
    }

    let remote_revision_advanced = pending
        .authoritative_remote_revision
        .is_some_and(|baseline| current.remote_revision > baseline);
    current.service_mode == ServiceMode::AokieActive
        && remote_revision_advanced
        && match revoked.mode {
            LeaseMode::Monitor => current.media_state == MediaState::Ready,
            LeaseMode::Consult | LeaseMode::Takeover => current.owner_epoch > revoked.owner_epoch,
        }
}

fn take_snapshot_confirmed_pending_revoke(
    client: &mut ClientState,
    frame: &MobileSnapshotFrame,
) -> Result<Option<PendingRevoke>, String> {
    let Some(pending) = client.pending_revoke.as_ref() else {
        return Ok(None);
    };
    if !snapshot_confirms_pending_revoke(pending, frame) {
        return Ok(None);
    }
    let completed = completed_revoke_from_pending(pending);
    remember_completed_revoke(client, completed)?;
    Ok(client.pending_revoke.take())
}

async fn apply_claim_rejection(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    frame: ClaimRejectedFrame,
    encoded: &str,
) -> Result<(), String> {
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    validate_id(&frame.request_id, "claim requestId")?;
    validate_text(&frame.code, 200, "claim rejection code")?;
    validate_text(&frame.message, 500, "claim rejection message")?;
    let action = {
        let mut client = state.inner.lock().await;
        prepare_claim_rejection(&mut client, &frame, encoded)?
    };
    let (lease, rejected_call) = match action {
        ClaimRejectionAction::AlreadyApplied => return Ok(()),
        ClaimRejectionAction::RejectPending {
            call_id,
            call_epoch,
        } => (None, Some((call_id, call_epoch))),
        ClaimRejectionAction::RejectPrepared { lease } => {
            let rejected_call = Some((lease.claims.call_id.clone(), lease.claims.call_epoch));
            (Some(lease), rejected_call)
        }
        ClaimRejectionAction::ContainedUnmatched => {
            // RTC signals and scheduled heartbeats share the plugin's generic
            // rejection dialect, but unlike a pending offer/claim they are not
            // authority transactions. A late or signal-local refusal must not
            // become a one-frame authenticated-session kill switch. Ordinary
            // snapshots, media watchdogs, and explicit revocation still own
            // any subsequent lease/media de-escalation.
            eprintln!(
                "[AokieCompanion][relay] contained unmatched claim rejection request={} code={}; current authority is unchanged",
                frame.request_id, frame.code
            );
            return Ok(());
        }
    };
    if let Some(lease) = lease {
        let _ = media::revoke(
            app,
            media_state,
            RevokeRequest {
                session: lease.session,
                reason: Some(frame.message.clone()),
            },
        )
        .await;
        emit_lease_reset(app);
    }
    if let Some((call_id, call_epoch)) = rejected_call {
        let _ = crate::android_runtime::reconcile_offer(
            app,
            &call_id,
            call_epoch,
            "cancel",
            "authoritative_claim_rejected",
        )
        .await;
    }
    emit_error(app, &format!("{} — {}", frame.code, frame.message));
    {
        let mut client = state.inner.lock().await;
        remember_applied_relay_frame(
            &mut client,
            claim_rejection_replay_key(&frame.request_id),
            encoded,
        )?;
    }
    Ok(())
}

fn prepare_claim_rejection(
    client: &mut ClientState,
    frame: &ClaimRejectedFrame,
    encoded: &str,
) -> Result<ClaimRejectionAction, String> {
    let replay_key = claim_rejection_replay_key(&frame.request_id);
    if applied_relay_frame_is_replay(client, &replay_key, encoded)? {
        return Ok(ClaimRejectionAction::AlreadyApplied);
    }
    if is_late_duplicate_offer_rejection(client, frame)? {
        // POST/SSE delivery is independently at-least-once. If the offer
        // answer itself is retried after its acceptance, the plugin's
        // idempotency fence reports `offer_replayed`; that must not tear down
        // the lease which the first answer legitimately released.
        remember_applied_relay_frame(client, replay_key, encoded)?;
        return Ok(ClaimRejectionAction::AlreadyApplied);
    }
    if client.pending.as_ref().is_some_and(|pending| {
        pending.request_id == frame.request_id || pending.offer_request_id == frame.request_id
    }) {
        let pending = client.pending.take().expect("checked pending claim");
        return Ok(ClaimRejectionAction::RejectPending {
            call_id: pending.call_id,
            call_epoch: pending.call_epoch,
        });
    }
    if client.lease.as_ref().is_some_and(|lease| {
        lease.request_id == frame.request_id && matches!(lease.claims.phase, LeasePhase::Prepared)
    }) {
        let lease = client.lease.take().expect("checked provisional lease");
        client.pending_microphone_mute = None;
        return Ok(ClaimRejectionAction::RejectPrepared { lease });
    }

    // There is deliberately no broad "current lease" match here. The mobile
    // does not retain outbound RTC/heartbeat operation metadata, so a generic
    // rejection cannot prove that it revokes authority. Remembering the exact
    // wire frame makes retries converge while preserving every existing
    // endpoint, app, replay, and lease-validation fence.
    remember_applied_relay_frame(client, replay_key, encoded)?;
    Ok(ClaimRejectionAction::ContainedUnmatched)
}

async fn heartbeat_frame(state: &V2State, app_id: &str) -> Result<Option<String>, String> {
    let (token, device_id) = {
        let client = state.inner.lock().await;
        let Some(lease) = &client.lease else {
            return Ok(None);
        };
        // Prepared is a short, non-renewable proof window. Heartbeating it can
        // race the authority's Prepared -> Active token rotation and turn an
        // otherwise healthy media negotiation into a stale-token refusal.
        if matches!(lease.claims.phase, LeasePhase::Prepared) {
            return Ok(None);
        }
        let current_grants = current_client_grants(&client);
        if !grants_permit_lease_mode(current_grants, lease.claims.mode) {
            return Ok(None);
        }
        let now = unix_now()?;
        if client.snapshot.as_ref().is_none_or(|snapshot| {
            !current_consent_permits_mode(&snapshot.snapshot.remote_consent, lease.claims.mode, now)
        }) {
            return Ok(None);
        }
        if !lease_heartbeat_due(lease.claims.expires_at, now) {
            return Ok(None);
        }
        (
            lease.token.clone(),
            client
                .device_id
                .clone()
                .ok_or("v2 device identity is unavailable")?,
        )
    };
    let request_id = state.next_id("heartbeat");
    let frame = LeaseHeartbeatFrame {
        kind: "lease_heartbeat".into(),
        schema_version: SCHEMA_VERSION,
        app_id: app_id.to_owned(),
        request_id: request_id.clone(),
        idempotency_key: format!("mobile:{device_id}:{request_id}"),
        lease_token: token,
    };
    frame.validate().map_err(|error| error.to_string())?;
    serde_json::to_string(&frame)
        .map(Some)
        .map_err(|_| "could not encode protocol-v2 lease heartbeat".into())
}

async fn peek_urgent_control_frame(state: &V2State) -> Option<UrgentControlFrame> {
    state
        .inner
        .lock()
        .await
        .urgent_control_frames
        .front()
        .cloned()
}

async fn confirm_urgent_control_frame(
    state: &V2State,
    delivered: &UrgentControlFrame,
) -> Result<(), String> {
    let mut client = state.inner.lock().await;
    let current = client
        .urgent_control_frames
        .front()
        .ok_or("urgent lease revoke disappeared before delivery confirmation")?;
    if current != delivered {
        return Err("urgent lease revoke changed before delivery confirmation".into());
    }
    client.urgent_control_frames.pop_front();
    Ok(())
}

fn lease_heartbeat_due(expires_at: u64, now: u64) -> bool {
    expires_at > now && expires_at.saturating_sub(now) <= LEASE_RENEWAL_WINDOW.as_secs()
}

/// Remove only authority whose exact current claims have crossed their local
/// expiry boundary.  The caller closes the matching native peer afterwards;
/// keeping that I/O outside the client lock lets an already-delivered renewal
/// win normally before this check runs.
///
/// The token return is best-effort but is queued before the caller touches
/// native media, so a live transport can promptly converge Desktop's lease
/// book even when its own expiry timer has not fired yet.  Crucially, none of
/// this invalidates the authenticated realtime session.
async fn take_expired_lease(state: &V2State) -> Option<(ClientLease, Option<String>)> {
    let now = unix_now().ok()?;
    let mut client = state.inner.lock().await;
    take_expired_lease_locked(state, &mut client, now)
}

fn take_expired_lease_locked(
    state: &V2State,
    client: &mut ClientState,
    now: u64,
) -> Option<(ClientLease, Option<String>)> {
    let expired = client
        .lease
        .as_ref()
        .is_some_and(|lease| lease.claims.expires_at <= now);
    if !expired {
        return None;
    }
    let lease = client.lease.take().expect("expired lease was present");
    client.pending_end_caller = None;
    client.pending_microphone_mute = None;
    let queue_error = queue_exact_lease_revocation(state, client, &lease, "lease_expired").err();
    let tombstone_error = push_local_expiry_tombstone(
        client,
        lease.clone(),
        Instant::now() + LOCAL_EXPIRY_TOMBSTONE_TTL,
        true,
    )
    .err();
    Some((lease, queue_error.or(tombstone_error)))
}

async fn surrender_failed_private_consult(
    app: &AppHandle,
    state: &V2State,
    failed_session: &MediaSession,
    detail: &str,
) -> Result<(), String> {
    let queue_error = {
        let mut client = state.inner.lock().await;
        let Some(lease) = take_failed_private_consult(&mut client, failed_session) else {
            return Ok(());
        };
        queue_exact_lease_revocation(state, &mut client, &lease, "native_media_setup_failed").err()
    };
    emit_lease_reset(app);
    emit_error(
        app,
        &format!("Private consult returned to Aokie because its microphone route failed: {detail}"),
    );
    match queue_error {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

fn take_failed_private_consult(
    client: &mut ClientState,
    failed_session: &MediaSession,
) -> Option<ClientLease> {
    let exact = client.lease.as_ref().is_some_and(|lease| {
        lease.claims.mode == LeaseMode::Consult && lease.session == *failed_session
    });
    if !exact {
        // A late watcher event from a replaced peer can never revoke the newer
        // lease that now owns the client state.
        return None;
    }
    client.pending_end_caller = None;
    client.pending_microphone_mute = None;
    client.lease.take()
}

async fn local_rtc_frame(
    state: &V2State,
    event: MediaSignalEvent,
) -> Result<Option<String>, String> {
    let now = unix_now()?;
    let (lease, endpoint_identity, endpoint_session_nonce) = {
        let client = state.inner.lock().await;
        let Some(lease) = &client.lease else {
            return Ok(None);
        };
        if !grants_permit_lease_mode(current_client_grants(&client), lease.claims.mode) {
            return Ok(None);
        }
        if client.snapshot.as_ref().is_none_or(|snapshot| {
            !current_consent_permits_mode(&snapshot.snapshot.remote_consent, lease.claims.mode, now)
        }) {
            return Ok(None);
        }
        if event.session != lease.session {
            // Events from a replaced peer are stale and are deliberately
            // dropped rather than being rebound to newer authority.
            return Ok(None);
        }
        if lease.claims.expires_at <= now {
            return Err("native RTC signal was produced after lease expiry".into());
        }
        (
            lease.clone(),
            client
                .endpoint_identity
                .clone()
                .ok_or("native endpoint identity is unavailable")?,
            client
                .session_nonce
                .clone()
                .ok_or("native endpoint session nonce is unavailable")?,
        )
    };
    let signature_expiry = lease.claims.expires_at.min(now.saturating_add(30));
    let signal = match event.signal {
        LocalSignal::Offer { description } => {
            if description.kind != SdpSignalType::Offer {
                return Err("native media emitted a non-offer as an offer".into());
            }
            let binding = endpoint_identity.sign_sdp_binding(EndpointBindingClaims {
                app_id: lease.claims.app_id.clone(),
                plugin_id: lease.claims.plugin_id.clone(),
                device_id: lease.claims.device_id.clone(),
                rtc_session_id: lease.claims.rtc_session_id.clone(),
                endpoint_session_nonce: endpoint_session_nonce.clone(),
                lease_jti: lease.claims.jti.clone(),
                endpoint_role: AdmissionRole::Mobile,
                holder_key_thumbprint: lease.claims.mobile_key_thumbprint.clone(),
                peer_key_thumbprint: lease.claims.plugin_key_thumbprint.clone(),
                call_id: lease.claims.call_id.clone(),
                call_epoch: lease.claims.call_epoch,
                owner_epoch: lease.claims.owner_epoch,
                fence: lease.claims.fence,
                sdp_revision: lease.session.sdp_revision,
                transport_generation: lease.session.transport_generation,
                dtls_fingerprint: sdp_dtls_fingerprint(&description.sdp)
                    .map_err(|error| error.to_string())?,
                sdp_sha256: sdp_sha256(&description.sdp),
                nonce: state.next_id("sdp_nonce"),
                jti: state.next_id("sdp_proof"),
                issued_at: now,
                expires_at: signature_expiry,
            })?;
            RtcSignal::Offer {
                sdp: description.sdp,
                binding,
            }
        }
        LocalSignal::Ice { candidate } => {
            let sdp_mid = Some(candidate.sdp_mid);
            let sdp_m_line_index = u16::try_from(candidate.sdp_mline_index).ok();
            let envelope = endpoint_identity.sign_candidate(TrickleCandidateClaims {
                app_id: lease.claims.app_id.clone(),
                plugin_id: lease.claims.plugin_id.clone(),
                device_id: lease.claims.device_id.clone(),
                rtc_session_id: lease.claims.rtc_session_id.clone(),
                endpoint_session_nonce: endpoint_session_nonce.clone(),
                lease_jti: lease.claims.jti.clone(),
                endpoint_role: AdmissionRole::Mobile,
                holder_key_thumbprint: lease.claims.mobile_key_thumbprint.clone(),
                peer_key_thumbprint: lease.claims.plugin_key_thumbprint.clone(),
                call_id: lease.claims.call_id.clone(),
                call_epoch: lease.claims.call_epoch,
                owner_epoch: lease.claims.owner_epoch,
                fence: lease.claims.fence,
                sdp_revision: lease.session.sdp_revision,
                transport_generation: lease.session.transport_generation,
                candidate: Some(candidate.candidate.clone()),
                sdp_mid: sdp_mid.clone(),
                sdp_m_line_index,
                end_of_candidates: false,
                nonce: state.next_id("ice_nonce"),
                jti: state.next_id("ice_proof"),
                issued_at: now,
                expires_at: signature_expiry,
            })?;
            RtcSignal::Ice {
                candidate: candidate.candidate,
                sdp_mid,
                sdp_m_line_index,
                envelope,
            }
        }
        LocalSignal::IceComplete => {
            let envelope = endpoint_identity.sign_candidate(TrickleCandidateClaims {
                app_id: lease.claims.app_id.clone(),
                plugin_id: lease.claims.plugin_id.clone(),
                device_id: lease.claims.device_id.clone(),
                rtc_session_id: lease.claims.rtc_session_id.clone(),
                endpoint_session_nonce,
                lease_jti: lease.claims.jti.clone(),
                endpoint_role: AdmissionRole::Mobile,
                holder_key_thumbprint: lease.claims.mobile_key_thumbprint.clone(),
                peer_key_thumbprint: lease.claims.plugin_key_thumbprint.clone(),
                call_id: lease.claims.call_id.clone(),
                call_epoch: lease.claims.call_epoch,
                owner_epoch: lease.claims.owner_epoch,
                fence: lease.claims.fence,
                sdp_revision: lease.session.sdp_revision,
                transport_generation: lease.session.transport_generation,
                candidate: None,
                sdp_mid: None,
                sdp_m_line_index: None,
                end_of_candidates: true,
                nonce: state.next_id("ice_nonce"),
                jti: state.next_id("ice_proof"),
                issued_at: now,
                expires_at: signature_expiry,
            })?;
            RtcSignal::IceComplete { envelope }
        }
        LocalSignal::LeaseSafetyFailure { .. } => {
            return Err("native media safety failure entered the RTC encoder".into())
        }
    };
    let frame = MobileRtcSignalFrame {
        kind: "rtc_signal".into(),
        schema_version: SCHEMA_VERSION,
        app_id: lease.claims.app_id.clone(),
        signal_id: state.next_id("signal"),
        plugin_id: lease.claims.plugin_id.clone(),
        device_id: lease.claims.device_id.clone(),
        lease_token: lease.token,
        lease_jti: lease.claims.jti.clone(),
        rtc_session_id: lease.claims.rtc_session_id,
        sdp_revision: lease.session.sdp_revision,
        transport_generation: lease.session.transport_generation,
        call_id: lease.claims.call_id,
        call_epoch: lease.claims.call_epoch,
        owner_epoch: lease.claims.owner_epoch,
        fence: lease.claims.fence,
        signal,
    };
    frame.validate().map_err(|error| error.to_string())?;
    serde_json::to_string(&frame)
        .map(Some)
        .map_err(|_| "could not encode native RTC signal".into())
}

fn session_from_claims(
    claims: &LeaseClaims,
    sdp_revision: u64,
    transport_generation: u64,
) -> Result<MediaSession, String> {
    let mode = match (claims.mode, claims.phase) {
        (LeaseMode::Monitor, LeasePhase::Active) => MediaMode::Monitor,
        (LeaseMode::Consult, LeasePhase::Prepared) => MediaMode::PreparedConsult,
        (LeaseMode::Consult, LeasePhase::Active) => MediaMode::Consult,
        (LeaseMode::Takeover, LeasePhase::Prepared) => MediaMode::PreparedTalk,
        (LeaseMode::Takeover, LeasePhase::Active) => MediaMode::Talk,
        (LeaseMode::Monitor, LeasePhase::Prepared) => {
            return Err("monitor lease cannot be provisional".into())
        }
    };
    Ok(MediaSession {
        app_id: claims.app_id.clone(),
        stream_nonce: claims.session_nonce.clone(),
        binding: SessionBinding {
            rtc_session_id: claims.rtc_session_id.clone(),
            call_id: claims.call_id.clone(),
            call_epoch: claims.call_epoch,
            owner_epoch: claims.owner_epoch,
            device_id: claims.device_id.clone(),
            mode,
            lease_id: Some(claims.lease_id.clone()),
            fence: claims.fence,
        },
        sdp_revision,
        transport_generation,
        expires_at: expiry_datetime(claims.expires_at)?,
    })
}

fn validate_claim_identity(client: &ClientState, claims: &LeaseClaims) -> Result<(), String> {
    let holder_key_thumbprint = client
        .endpoint_identity
        .as_ref()
        .map(crate::endpoint_identity::EndpointIdentity::thumbprint);
    if client.app_id.as_deref() != Some(&claims.app_id)
        || client.device_id.as_deref() != Some(&claims.device_id)
        || client.session_nonce.as_deref() != Some(&claims.session_nonce)
        || holder_key_thumbprint != Some(claims.mobile_key_thumbprint.as_str())
        || client.peer_key_thumbprint.as_deref() != Some(claims.plugin_key_thumbprint.as_str())
    {
        return Err("lease claims are not bound to this authenticated session".into());
    }
    Ok(())
}

fn validate_renewal(current: &ClientLease, renewed: &LeaseClaims) -> Result<(), String> {
    let old = &current.claims;
    if renewed.app_id != old.app_id
        || renewed.plugin_id != old.plugin_id
        || renewed.device_id != old.device_id
        || renewed.plugin_key_thumbprint != old.plugin_key_thumbprint
        || renewed.mobile_key_thumbprint != old.mobile_key_thumbprint
        || renewed.call_id != old.call_id
        || renewed.call_epoch != old.call_epoch
        || renewed.owner_epoch != old.owner_epoch
        || renewed.mode != old.mode
        || renewed.phase != old.phase
        || renewed.tracks != old.tracks
        || renewed.lease_id != old.lease_id
        || renewed.fence != old.fence
        || renewed.session_nonce != old.session_nonce
        || renewed.rtc_session_id != old.rtc_session_id
        || renewed.expires_at <= old.expires_at
        || renewed.jti == old.jti
    {
        return Err("lease renewal changed immutable authority or did not advance".into());
    }
    Ok(())
}

fn validate_idle_sync(frame: &MobileIdleSyncFrame, expected_app_id: &str) -> Result<(), String> {
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    if frame.kind != "idle_sync"
        || frame.sequence == 0
        || frame.sequence > MAX_SAFE_INTEGER
        || frame.grants.len() > 16
        || !frame.grants.contains(&Grant::StateRead)
    {
        return Err("protocol-v2 idle sync is invalid".into());
    }
    let unique: HashSet<_> = frame.grants.iter().collect();
    if unique.len() != frame.grants.len() {
        return Err("protocol-v2 idle sync grants are invalid".into());
    }
    Ok(())
}

fn transition_to_idle(
    client: &mut ClientState,
    frame: &MobileIdleSyncFrame,
) -> Result<Option<IdleStateCleanup>, String> {
    if !is_new_authoritative_sequence(client, frame.sequence) {
        return Ok(None);
    }
    let pending_call = client
        .pending
        .as_ref()
        .map(|pending| (pending.call_id.clone(), pending.call_epoch))
        .or_else(|| {
            client.snapshot.as_ref().map(|snapshot| {
                (
                    snapshot.snapshot.call_id.clone(),
                    snapshot.snapshot.call_epoch,
                )
            })
        });
    let mut failed_native_action_ids = Vec::new();
    if let Some(action_id) = client
        .pending
        .take()
        .and_then(|pending| pending.native_action_id)
    {
        failed_native_action_ids.push(action_id);
    }
    let confirmed_revoke_action_id = if client
        .pending_revoke
        .as_ref()
        .is_some_and(|pending| frame.sequence > pending.authoritative_sequence)
    {
        let completed = completed_revoke_from_pending(
            client
                .pending_revoke
                .as_ref()
                .expect("checked pending revoke"),
        );
        remember_completed_revoke(client, completed)?;
        client
            .pending_revoke
            .take()
            .and_then(|pending| pending.native_action_id)
    } else {
        None
    };
    if let Some(action_id) = client
        .pending_native_end
        .take()
        .map(|pending| pending.action.action_id)
    {
        failed_native_action_ids.push(action_id);
    }
    if let Some(action_id) = client
        .pending_native_hangup
        .take()
        .map(|pending| pending.action_id)
    {
        failed_native_action_ids.push(action_id);
    }
    failed_native_action_ids.sort();
    failed_native_action_ids.dedup();

    client.authoritative_sequence = frame.sequence;
    client.snapshot = None;
    client.spent_mobile_offers.clear();
    client.lease = None;
    client.local_expiry_tombstones.clear();
    client.pending_microphone_mute = None;
    client.assistance = None;
    client.pending_assistance_answer = None;
    client.pending_end_caller = None;
    client.seen_remote_endpoint_jtis.clear();

    Ok(Some(IdleStateCleanup {
        failed_native_action_ids,
        confirmed_revoke_action_id,
        pending_call,
    }))
}

fn is_new_authoritative_sequence(client: &ClientState, sequence: u64) -> bool {
    sequence > client.authoritative_sequence
}

fn validate_snapshot(frame: &MobileSnapshotFrame, expected_app_id: &str) -> Result<(), String> {
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    frame
        .snapshot
        .validate()
        .map_err(|error| error.to_string())?;
    if frame.kind != "snapshot" || frame.sequence == 0 || frame.sequence > MAX_SAFE_INTEGER {
        return Err("protocol-v2 snapshot sequence is invalid".into());
    }
    let unique: HashSet<_> = frame.grants.iter().collect();
    if unique.len() != frame.grants.len() || frame.grants.len() > 16 {
        return Err("protocol-v2 snapshot grants are invalid".into());
    }
    let snapshot = &frame.snapshot;
    snapshot
        .remote_consent
        .validate()
        .map_err(|error| error.to_string())?;
    validate_id(&snapshot.call_id, "snapshot callId")?;
    for (label, value, minimum) in [
        ("callEpoch", snapshot.call_epoch, 1),
        ("ownerEpoch", snapshot.owner_epoch, 0),
        ("switchboardRevision", snapshot.switchboard_revision, 0),
        ("remoteRevision", snapshot.remote_revision, 0),
    ] {
        if value < minimum || value > MAX_SAFE_INTEGER {
            return Err(format!("snapshot {label} is invalid"));
        }
    }
    if matches!(snapshot.telephony_state, TelephonyState::Ended)
        != matches!(snapshot.service_mode, ServiceMode::Ended)
    {
        return Err("snapshot telephony/service ended states disagree".into());
    }
    DateTime::parse_from_rfc3339(&snapshot.occurred_at)
        .map_err(|_| "snapshot occurredAt is invalid".to_string())?;
    if let Some(caller) = &snapshot.caller {
        if let Some(label) = &caller.label {
            validate_text(label, 200, "caller label")?;
        }
        if let Some(number) = &caller.masked_number {
            validate_text(number, 40, "caller maskedNumber")?;
        }
    }
    let consent_current = remote_consent_is_current(&snapshot.remote_consent, unix_now()?);
    if (!snapshot.participants.is_empty() || snapshot.audio_levels.is_some()) && !consent_current {
        return Err("snapshot exposed participants or audio levels without current consent".into());
    }
    if !snapshot.participants.is_empty() && !frame.grants.contains(&Grant::ParticipantsRead) {
        return Err("snapshot exposed participants without the current roster grant".into());
    }
    if snapshot
        .participants
        .iter()
        .any(|participant| participant.subject_id.is_some() || participant.display_label.is_some())
        && !frame.grants.contains(&Grant::ParticipantIdentityRead)
    {
        return Err(
            "snapshot exposed participant identity without the current identity grant".into(),
        );
    }
    if snapshot.audio_levels.is_some() && !frame.grants.contains(&Grant::AudioLevelsRead) {
        return Err("snapshot exposed audio levels without the current levels grant".into());
    }
    if let Some(captions) = &snapshot.captions {
        if !consent_current || !snapshot.remote_consent.captions_enabled {
            return Err("snapshot exposed captions without current consent".into());
        }
        if captions.len() > 200 {
            return Err("snapshot captions exceed the limit".into());
        }
        for caption in captions {
            validate_id(&caption.caption_id, "captionId")?;
            validate_text(&caption.speaker, 40, "caption speaker")?;
            validate_text(&caption.text, 2_000, "caption text")?;
            DateTime::parse_from_rfc3339(&caption.occurred_at)
                .map_err(|_| "caption occurredAt is invalid".to_string())?;
        }
    }
    Ok(())
}

fn validate_common(
    kind: &str,
    schema_version: u16,
    app_id: Option<&str>,
    expected_app_id: &str,
) -> Result<(), String> {
    validate_id(kind, "frame kind")?;
    if schema_version != SCHEMA_VERSION {
        return Err("gateway frame has the wrong schemaVersion".into());
    }
    if app_id.is_some_and(|app_id| app_id != expected_app_id) {
        return Err("gateway frame belongs to a different app".into());
    }
    Ok(())
}

fn parse_kind(encoded: &str) -> Result<String, String> {
    let value: Value = serde_json::from_str(encoded)
        .map_err(|_| "gateway sent malformed protocol-v2 JSON".to_string())?;
    value
        .as_object()
        .and_then(|object| object.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "protocol-v2 frame omitted kind".to_string())
}

fn strict_parse<T: for<'de> Deserialize<'de>>(encoded: &str, label: &str) -> Result<T, String> {
    serde_json::from_str(encoded).map_err(|_| format!("gateway {label} frame is invalid"))
}

fn validate_id(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
}

fn validate_text(value: &str, maximum: usize, label: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
}

fn expiry_datetime(expires_at: u64) -> Result<DateTime<Utc>, String> {
    let seconds = i64::try_from(expires_at).map_err(|_| "lease expiry is invalid")?;
    DateTime::from_timestamp(seconds, 0).ok_or_else(|| "lease expiry is invalid".into())
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before the Unix epoch".into())
}

fn managed_gateway_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "managed admission returned an invalid gateway URL")?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    let managed_beta_loopback = cfg!(feature = "managed-beta-local")
        && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
        && url.scheme() == "ws";
    if url.scheme() != "wss"
        && !(cfg!(debug_assertions) && loopback && url.scheme() == "ws")
        && !managed_beta_loopback
    {
        return Err("managed admission gateway must use wss".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with("/v2/realtime")
    {
        return Err("managed admission returned an unsupported gateway URL".into());
    }
    Ok(url)
}

fn bearer_header(access_token: &str) -> Result<HeaderValue, String> {
    if access_token.len() < 16 || access_token.len() > MAX_LEASE_TOKEN_BYTES {
        return Err("managed admission returned an invalid access token".into());
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {access_token}"))
        .map_err(|_| "managed admission token contains invalid bytes".to_string())?;
    value.set_sensitive(true);
    Ok(value)
}

fn fresh_session_nonce(state: &V2State, configured: Option<&str>, device_id: &str) -> String {
    let suffix = format!(".{}", state.next_id("session"));
    let prefix = configured.unwrap_or(device_id);
    let maximum = 200_usize.saturating_sub(suffix.len());
    let prefix = &prefix[..prefix.len().min(maximum)];
    format!("{prefix}{suffix}")
}

fn managed_admission_refresh_deadline(expires_at: u64) -> Instant {
    let remaining = expires_at.saturating_sub(unix_now().unwrap_or(expires_at));
    Instant::now() + Duration::from_secs(remaining).saturating_sub(ADMISSION_REFRESH_MARGIN)
}

fn transient_managed_sync_failure(message: &str) -> bool {
    matches!(
        message,
        "protocol-v2 transport closed before authoritative sync"
            | "v2 transport closed before endpoint proof"
    )
}

fn reconnect_delay(attempt: u32) -> Duration {
    Duration::from_secs((1_u64 << attempt.min(4)).min(MAX_RECONNECT_DELAY))
}

async fn send_text<S>(writer: &mut S, encoded: String) -> bool
where
    S: SinkExt<Message> + Unpin,
{
    if encoded.len() > MAX_MESSAGE_BYTES {
        return false;
    }
    send_message(writer, Message::Text(encoded.into())).await
}

async fn send_message<S>(writer: &mut S, message: Message) -> bool
where
    S: SinkExt<Message> + Unpin,
{
    send_message_with_timeout(writer, message, SEND_TIMEOUT).await
}

async fn send_message_with_timeout<S>(writer: &mut S, message: Message, timeout: Duration) -> bool
where
    S: SinkExt<Message> + Unpin,
{
    matches!(
        tokio::time::timeout(timeout, writer.send(message)).await,
        Ok(Ok(()))
    )
}

fn emit_error(app: &AppHandle, message: &str) {
    eprintln!("[AokieCompanion][error] {message}");
    emit_transport(app, TransportEvent::Error { message });
}

fn managed_admission_state_event(
    error: &ManagedAdmissionError,
) -> Option<ManagedAdmissionStateEvent<'_>> {
    let (code, message) = error.policy()?;
    let value = match code {
        "mobile_not_paired" => "pairing_required",
        "desktop_identity_unavailable" => "desktop_unavailable",
        _ => "policy_denied",
    };
    Some(ManagedAdmissionStateEvent {
        value,
        code,
        message,
    })
}

fn emit_managed_admission_state(app: &AppHandle, error: &ManagedAdmissionError) {
    if let Some(event) = managed_admission_state_event(error) {
        let _ = app.emit("aokie-companion://managed-admission-state", event);
    }
}

fn emit_lease_reset(app: &AppHandle) {
    let _ = app.emit::<Option<LeaseEvent>>("aokie-companion://v2-lease", None);
}

fn emit_assistance_reset(app: &AppHandle) {
    let _ =
        app.emit::<Option<PluginAssistanceRequestFrame>>("aokie-companion://v2-assistance", None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aokie_protocol::v2::{
        tracks_for, CarrierHoldEvidence, MediaTrack, ProjectedCallSnapshot, RemoteCapabilities,
        SecondaryCallObservation, SecondaryCallPolicy, LEASE_AUDIENCE,
    };

    #[test]
    fn scheduled_heartbeat_refresh_is_not_mistaken_for_a_user_operation() {
        let mut client = ClientState::default();
        assert!(!tracks_gateway_request(&client, "heartbeat_1"));

        client.pending_end_caller = Some(PendingEndCaller::AwaitingChallenge {
            request_id: "end_prepare_1".into(),
        });
        assert!(tracks_gateway_request(&client, "end_prepare_1"));
        assert!(!tracks_gateway_request(&client, "heartbeat_1"));
    }

    #[tokio::test]
    async fn predecessor_arm_enabled_before_fence_rechecks_before_acting() {
        let fence = Arc::new(AtomicBool::new(false));
        assert!(predecessor_receive_enabled(
            false,
            fence.load(Ordering::Acquire)
        ));
        let acted = Arc::new(AtomicU64::new(0));
        let arm_fence = fence.clone();
        let arm_acted = acted.clone();
        let (complete, ready) = tokio::sync::oneshot::channel::<()>();
        let arm = tokio::spawn(async move {
            ready.await.unwrap();
            if predecessor_arm_still_current(&arm_fence) {
                arm_acted.fetch_add(1, Ordering::AcqRel);
            }
        });

        // Models a select guard evaluated before the rotation future writes
        // mobile_hello, followed by this predecessor arm becoming ready only
        // after that irreversible gateway fence.
        fence.store(true, Ordering::Release);
        complete.send(()).unwrap();
        arm.await.unwrap();
        assert_eq!(acted.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn delayed_managed_rotation_does_not_starve_active_lease_heartbeats() {
        let mut rotation: Option<ManagedTransportRotationFuture> = Some(Box::pin(async {
            tokio::time::sleep(Duration::from_millis(45)).await;
            Err(ManagedTransportRotationError::Transport(
                "delayed replacement".into(),
            ))
        }));
        let mut heartbeat = tokio::time::interval(Duration::from_millis(5));
        heartbeat.tick().await;
        let mut serviced = 0_u32;

        let result = loop {
            tokio::select! {
                result = await_managed_transport_rotation(&mut rotation), if rotation.is_some() => {
                    break result;
                }
                _ = heartbeat.tick() => {
                    serviced += 1;
                }
            }
        };

        assert!(matches!(
            result,
            Err(ManagedTransportRotationError::Transport(message))
                if message == "delayed replacement"
        ));
        assert!(
            serviced >= 4,
            "the predecessor must keep renewing while replacement auth/handshake/sync is pending"
        );
    }

    #[tokio::test]
    async fn slow_relay_refresh_applies_inbound_renewal_before_rotation_finishes() {
        let state = V2State::default();
        let (mut client, mut active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        let now = unix_now().unwrap();
        active.expires_at = now + 10;
        client.lease = Some(ClientLease {
            request_id: "claim_request_a".into(),
            token: "signed.old.relay.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active.clone(),
        });
        *state.inner.lock().await = client;
        let mut renewed = active;
        renewed.expires_at = now + 30;
        renewed.jti = "lease_relay_refresh_renewed".into();
        let frame = lease_status_frame("lease_renewed", "signed.new.relay.token", renewed.clone());
        let encoded = serde_json::to_string(&frame).unwrap();

        let mut rotation: Option<ManagedTransportRotationFuture> = Some(Box::pin(async {
            tokio::time::sleep(Duration::from_millis(45)).await;
            Err(ManagedTransportRotationError::Transport(
                "delayed relay refresh".into(),
            ))
        }));
        let inbound = async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            (frame, encoded)
        };
        tokio::pin!(inbound);
        let mut inbound_applied = false;

        let result = loop {
            tokio::select! {
                result = await_managed_transport_rotation(&mut rotation), if rotation.is_some() => {
                    break result;
                }
                inbound = &mut inbound, if !inbound_applied && predecessor_receive_enabled(false, false) => {
                    let (frame, encoded) = inbound;
                    apply_lease_status_transaction(
                        &state,
                        "app_a",
                        frame,
                        &encoded,
                        |_, _, _, _| async { Ok(()) },
                        |_| async {},
                    )
                    .await
                    .expect("the sole live relay reader applies the queued renewal");
                    inbound_applied = true;
                }
            }
        };

        assert!(matches!(
            result,
            Err(ManagedTransportRotationError::Transport(message))
                if message == "delayed relay refresh"
        ));
        assert!(inbound_applied);
        let client = state.inner.lock().await;
        assert_eq!(
            client.lease.as_ref().unwrap().claims.jti,
            renewed.jti,
            "relay admission/challenge work cannot pause lease_renewed consumption"
        );
    }

    #[tokio::test]
    async fn delayed_ws_snapshot_cannot_hold_a_valid_renewal_past_native_expiry() {
        let state = V2State::default();
        let (mut client, mut active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        let now = unix_now().unwrap();
        active.expires_at = now + 10;
        let previous = ClientLease {
            request_id: "claim_request_a".into(),
            token: "signed.old.ws.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active.clone(),
        };
        client.lease = Some(previous);
        managed_ws_handoff_lease_ready(&client, now)
            .expect("ten seconds is enough for the bounded handoff write pair");
        let mut too_late = ClientState {
            lease: client.lease.clone(),
            ..ClientState::default()
        };
        too_late.lease.as_mut().unwrap().claims.expires_at =
            now + MANAGED_HANDOFF_MIN_LEASE_REMAINING.as_secs();
        assert!(managed_ws_handoff_lease_ready(&too_late, now).is_err());
        *state.inner.lock().await = client;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut renewed = active;
        renewed.expires_at = now + 30;
        renewed.jti = "lease_ws_handoff_renewed".into();
        let server_renewed = renewed.clone();
        let renewal_applied = Arc::new(AtomicBool::new(false));
        let expiry_observer = renewal_applied.clone();
        let old_native_expiry = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                expiry_observer.load(Ordering::Acquire),
                "the replacement renewal must update native authority before the old watcher expires"
            );
        });
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let heartbeat = socket
                .next()
                .await
                .expect("client sends handoff heartbeat")
                .expect("heartbeat websocket frame");
            let Message::Text(heartbeat) = heartbeat else {
                panic!("replacement expected a text heartbeat");
            };
            let heartbeat = heartbeat.as_str().to_owned();
            assert_eq!(parse_kind(&heartbeat).unwrap(), "lease_heartbeat");
            let request_id = encoded_request_id(&heartbeat, "test heartbeat").unwrap();

            let mut frame =
                lease_status_frame("lease_renewed", "signed.new.ws.token", server_renewed);
            frame.request_id = request_id;
            let encoded = serde_json::to_string(&frame).unwrap();
            socket
                .send(Message::Text(encoded.clone().into()))
                .await
                .unwrap();
            // Hold authoritative state beyond the synthetic old-native expiry.
            // A buffer-only implementation loses the peer at 50ms; immediate
            // exact renewal application keeps it alive while sync continues.
            tokio::time::sleep(Duration::from_millis(100)).await;
            socket
                .send(Message::Text(
                    serde_json::json!({
                        "kind": "snapshot",
                        "schemaVersion": SCHEMA_VERSION,
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            encoded
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let mut transport = V2Transport::websocket(socket);

        let apply_state = state.clone();
        let applied_signal = renewal_applied.clone();
        let buffered = receive_replacement_sync_with_renewal(
            &mut transport,
            &state,
            "app_a",
            move |encoded| {
                let state = apply_state.clone();
                let applied = applied_signal.clone();
                async move {
                    let frame: LeaseStatusFrame = serde_json::from_str(&encoded)
                        .map_err(|_| "test renewal did not decode".to_string())?;
                    apply_lease_status_transaction(
                        &state,
                        "app_a",
                        frame,
                        &encoded,
                        |_, _, _, _| async { Ok(()) },
                        |_| async {},
                    )
                    .await?
                    .ok_or("test renewal was not applied")?;
                    applied.store(true, Ordering::Release);
                    Ok(())
                }
            },
        )
        .await
        .expect("near-expiry replacement sync includes its renewal");
        old_native_expiry.await.unwrap();
        let renewed_encoded = server.await.unwrap();
        assert_eq!(buffered.len(), 2);
        assert_eq!(buffered[0], renewed_encoded);
        assert!(renewal_applied.load(Ordering::Acquire));
        let client = state.inner.lock().await;
        assert_eq!(client.lease.as_ref().unwrap().claims.jti, renewed.jti);
        assert_eq!(client.lease.as_ref().unwrap().token, "signed.new.ws.token");
    }

    #[test]
    fn managed_refresh_has_one_overlap_path_without_continuous_ninety_second_churn() {
        let overlap_to_fence = MANAGED_ADMISSION_PRIMARY_HTTP_BUDGET
            + CONNECT_TIMEOUT
            + CONNECT_TIMEOUT
            + MANAGED_HANDOFF_SEND_TIMEOUT;
        assert_eq!(ADMISSION_REFRESH_MARGIN, Duration::from_secs(45));
        assert_eq!(
            ADMISSION_REFRESH_MARGIN,
            overlap_to_fence + MANAGED_HANDOFF_RETRY_HEADROOM
        );
        let before = Instant::now();
        let deadline = managed_admission_refresh_deadline(unix_now().unwrap() + 90);
        let remaining = deadline.saturating_duration_since(before);
        assert!(remaining >= Duration::from_secs(44));
        assert!(remaining <= Duration::from_secs(46));
        assert!(
            remaining >= ADMISSION_REFRESH_MARGIN.saturating_sub(Duration::from_secs(1)),
            "a 90-second admission must not trigger another hello every few seconds"
        );
    }

    #[test]
    fn cross_carrier_refresh_requires_a_fresh_sequence_generation() {
        assert!(managed_carrier_continuity(false, false));
        assert!(managed_carrier_continuity(true, true));
        assert!(!managed_carrier_continuity(false, true));
        assert!(!managed_carrier_continuity(true, false));
    }

    #[test]
    fn overlapping_sync_skips_authenticated_control_until_authoritative_state() {
        let control = serde_json::json!({
            "kind": "rtc_signal",
            "schemaVersion": SCHEMA_VERSION,
        })
        .to_string();
        let snapshot = serde_json::json!({
            "kind": "snapshot",
            "schemaVersion": SCHEMA_VERSION,
        })
        .to_string();
        assert_eq!(initial_sync_frame_ready(&control, true), Ok(false));
        assert_eq!(initial_sync_frame_ready(&snapshot, true), Ok(true));
        assert!(initial_sync_frame_ready(&control, false).is_err());
    }

    #[test]
    fn managed_admission_policy_maps_to_strict_ui_states() {
        for (code, expected) in [
            ("mobile_not_paired", "pairing_required"),
            ("desktop_identity_unavailable", "desktop_unavailable"),
            ("insufficient_scope", "policy_denied"),
        ] {
            let error = ManagedAdmissionError::Policy {
                code: code.into(),
                message: "Action is required".into(),
            };
            let event = managed_admission_state_event(&error).unwrap();
            assert_eq!(
                serde_json::to_value(event).unwrap(),
                serde_json::json!({
                    "value": expected,
                    "code": code,
                    "message": "Action is required",
                }),
            );
        }
        assert!(managed_admission_state_event(&ManagedAdmissionError::Other(
            "network unavailable".into(),
        ))
        .is_none());
    }

    #[test]
    fn idle_sync_clears_call_authority_but_retains_sequence_high_water() {
        let mut client = offered_client(LeaseMode::Takeover, MobileOfferSurface::VoiceSystemUi);
        client.authoritative_sequence = 5;
        client.pending_assistance_answer = Some("assistance_answer_a".into());
        client.seen_remote_endpoint_jtis.insert("rtc_jti_a".into());
        let spent = client
            .snapshot
            .as_ref()
            .unwrap()
            .snapshot
            .pending_mobile_offers[0]
            .clone();
        tombstone_mobile_offer(&mut client, &spent, None);
        let frame = MobileIdleSyncFrame {
            kind: "idle_sync".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            sequence: 6,
            grants: vec![Grant::StateRead, Grant::Monitor],
        };
        validate_idle_sync(&frame, "app_a").unwrap();
        let cleanup = transition_to_idle(&mut client, &frame)
            .unwrap()
            .expect("new idle sequence applies");

        assert_eq!(client.authoritative_sequence, 6);
        assert!(client.snapshot.is_none());
        assert!(client.lease.is_none());
        assert!(client.pending.is_none());
        assert!(client.pending_revoke.is_none());
        assert!(client.pending_native_end.is_none());
        assert!(client.assistance.is_none());
        assert!(client.pending_assistance_answer.is_none());
        assert!(client.pending_end_caller.is_none());
        assert!(client.seen_remote_endpoint_jtis.is_empty());
        assert!(client.spent_mobile_offers.is_empty());
        assert_eq!(cleanup.pending_call, Some(("call_a".into(), 7)));

        // A late snapshot at or below the idle sequence can never resurrect
        // call authority; only a strictly newer gateway projection may pass.
        assert!(!is_new_authoritative_sequence(&client, 5));
        assert!(!is_new_authoritative_sequence(&client, 6));
        assert!(is_new_authoritative_sequence(&client, 7));

        let stale_idle = MobileIdleSyncFrame {
            sequence: 6,
            ..frame.clone()
        };
        assert!(transition_to_idle(&mut client, &stale_idle)
            .unwrap()
            .is_none());
        assert_eq!(client.authoritative_sequence, 6);
        assert!(client.snapshot.is_none());

        let mut invalid = frame;
        invalid.sequence = 0;
        assert!(validate_idle_sync(&invalid, "app_a").is_err());
        invalid.sequence = 7;
        invalid.grants = vec![Grant::Monitor];
        assert!(validate_idle_sync(&invalid, "app_a").is_err());
    }

    #[test]
    fn newer_idle_confirms_revoke_separately_from_other_native_failures() {
        let mut client = offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
        client.authoritative_sequence = 8;
        client.pending_revoke = Some(pending_revoke_fixture(LeaseMode::Takeover, 8));
        let mut native_end = native_answer_action();
        native_end.action_id = "native_end_other".into();
        native_end.kind = NativeCallActionKind::End;
        client.pending_native_end = Some(PendingNativeEnd {
            action: native_end,
            deadline: Instant::now() + NATIVE_ACTION_TIMEOUT,
        });
        let stale = MobileIdleSyncFrame {
            kind: "idle_sync".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            sequence: 8,
            grants: vec![Grant::StateRead],
        };

        assert!(transition_to_idle(&mut client, &stale).unwrap().is_none());
        assert!(client.pending_revoke.is_some());

        let current = MobileIdleSyncFrame {
            sequence: 9,
            ..stale
        };
        let cleanup = transition_to_idle(&mut client, &current)
            .unwrap()
            .expect("new idle state applies");
        assert_eq!(
            cleanup.confirmed_revoke_action_id.as_deref(),
            Some("native_return_a")
        );
        assert_eq!(
            cleanup.failed_native_action_ids,
            vec!["native_end_other".to_string()]
        );
        assert!(client.pending_revoke.is_none());
        assert!(client.pending_native_end.is_none());
        assert_eq!(client.completed_revokes.len(), 1);
    }

    #[test]
    fn identical_assistance_retry_preserves_pending_answer() {
        let mut client = offered_client(LeaseMode::Consult, MobileOfferSurface::InApp);
        let frame = client.assistance.clone().expect("consult help request");
        client.pending_assistance_answer = Some("answer_pending_a".into());

        assert!(!apply_assistance_request(&mut client, &frame).unwrap());
        assert_eq!(client.assistance.as_ref(), Some(&frame));
        assert_eq!(
            client.pending_assistance_answer.as_deref(),
            Some("answer_pending_a")
        );
    }

    #[test]
    fn answered_assistance_replay_is_bounded_and_cannot_resurrect() {
        let mut client = offered_client(LeaseMode::Consult, MobileOfferSurface::InApp);
        let frame = client.assistance.take().expect("consult help request");
        remember_answered_assistance(&mut client, &frame.app_id, &frame.request_id);

        assert!(!apply_assistance_request(&mut client, &frame).unwrap());
        assert!(client.assistance.is_none());
        assert!(client.pending_assistance_answer.is_none());

        for index in 0..=MAX_ANSWERED_ASSISTANCE_REQUESTS {
            remember_answered_assistance(
                &mut client,
                "app_a",
                &format!("answered_request_{index}"),
            );
        }
        assert_eq!(
            client.answered_assistance_requests.len(),
            MAX_ANSWERED_ASSISTANCE_REQUESTS
        );
        assert!(!assistance_was_answered(
            &client,
            &frame.app_id,
            &frame.request_id
        ));
        assert!(assistance_was_answered(
            &client,
            "app_a",
            &format!("answered_request_{}", MAX_ANSWERED_ASSISTANCE_REQUESTS)
        ));
    }

    #[test]
    fn expired_assistance_is_discarded_with_pending_answer() {
        let mut client = offered_client(LeaseMode::Consult, MobileOfferSurface::InApp);
        let request_id = client
            .assistance
            .as_ref()
            .expect("consult help request")
            .request_id
            .clone();
        client
            .assistance
            .as_mut()
            .expect("consult help request")
            .expires_at = 10;
        client.pending_assistance_answer = Some("answer_pending_a".into());

        assert!(discard_expired_assistance(
            &mut client,
            11,
            Some(&request_id)
        ));
        assert!(client.assistance.is_none());
        assert!(client.pending_assistance_answer.is_none());
    }

    #[tokio::test]
    async fn answered_assistance_replay_fence_survives_transport_reset() {
        let state = V2State::default();
        {
            let mut client = state.inner.lock().await;
            remember_answered_assistance(&mut client, "app_a", "answered_request_a");
        }

        state.reset().await;

        let client = state.inner.lock().await;
        assert!(assistance_was_answered(
            &client,
            "app_a",
            "answered_request_a"
        ));
    }

    #[test]
    fn at_least_once_authority_frames_accept_only_exact_wire_replays() {
        let mut client = ClientState::default();
        let cases = [
            (
                offer_acceptance_replay_key("offer_answer_1"),
                r#"{"kind":"mobile_offer_accepted","requestId":"offer_answer_1"}"#,
            ),
            (
                lease_status_replay_key("claim_provisional", "claim_1"),
                r#"{"kind":"claim_provisional","requestId":"claim_1"}"#,
            ),
            (
                lease_status_replay_key("lease_granted", "claim_2"),
                r#"{"kind":"lease_granted","requestId":"claim_2"}"#,
            ),
            (
                lease_status_replay_key("claim_active", "claim_1"),
                r#"{"kind":"claim_active","requestId":"claim_1"}"#,
            ),
            (
                lease_status_replay_key("lease_renewed", "heartbeat_1"),
                r#"{"kind":"lease_renewed","requestId":"heartbeat_1"}"#,
            ),
            (
                rtc_signal_replay_key("rtc_signal_1"),
                r#"{"kind":"rtc_signal","signalId":"rtc_signal_1","signal":"answer"}"#,
            ),
        ];

        for (key, encoded) in cases {
            assert!(!applied_relay_frame_is_replay(&client, &key, encoded).unwrap());
            remember_applied_relay_frame(&mut client, key.clone(), encoded).unwrap();
            assert!(applied_relay_frame_is_replay(&client, &key, encoded).unwrap());

            let changed = format!("{encoded} ");
            let error = applied_relay_frame_is_replay(&client, &key, &changed).unwrap_err();
            assert_eq!(
                error,
                "relay authority frame identifier was reused with different content"
            );
        }
    }

    #[test]
    fn applied_relay_replay_fence_is_bounded() {
        let mut client = ClientState::default();
        for index in 0..=MAX_APPLIED_RELAY_FRAMES {
            remember_applied_relay_frame(
                &mut client,
                rtc_signal_replay_key(&format!("signal_{index}")),
                &format!(r#"{{"signalId":"signal_{index}"}}"#),
            )
            .unwrap();
        }
        assert_eq!(client.applied_relay_frames.len(), MAX_APPLIED_RELAY_FRAMES);
        assert!(!relay_frame_was_applied(
            &client,
            &rtc_signal_replay_key("signal_0")
        ));
        assert!(relay_frame_was_applied(
            &client,
            &rtc_signal_replay_key(&format!("signal_{MAX_APPLIED_RELAY_FRAMES}"))
        ));
    }

    #[tokio::test]
    async fn applied_relay_replay_fence_survives_transport_reset() {
        let state = V2State::default();
        let key = lease_status_replay_key("claim_active", "claim_1");
        let encoded = r#"{"kind":"claim_active","requestId":"claim_1"}"#;
        {
            let mut client = state.inner.lock().await;
            remember_applied_relay_frame(&mut client, key.clone(), encoded).unwrap();
        }

        state.reset().await;

        let client = state.inner.lock().await;
        assert!(applied_relay_frame_is_replay(&client, &key, encoded).unwrap());
    }

    #[test]
    fn late_offer_replayed_rejection_cannot_undo_an_accepted_offer() {
        let mut client = ClientState::default();
        let accepted_key = offer_acceptance_replay_key("offer_answer_1");
        remember_applied_relay_frame(
            &mut client,
            accepted_key,
            r#"{"kind":"mobile_offer_accepted","requestId":"offer_answer_1"}"#,
        )
        .unwrap();
        let replayed = ClaimRejectedFrame {
            kind: "claim_rejected".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: "offer_answer_1".into(),
            code: "offer_replayed".into(),
            message: "that offer was already answered".into(),
        };
        assert!(is_late_duplicate_offer_rejection(&client, &replayed).unwrap());

        let mut contradiction = replayed;
        contradiction.code = "offer_invalid".into();
        assert!(is_late_duplicate_offer_rejection(&client, &contradiction).is_err());
    }

    #[test]
    fn unmatched_signal_rejection_is_contained_replay_fenced_and_preserves_authority() {
        for request_id in ["rtc_signal_11", "heartbeat_7"] {
            let (mut client, active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
            let current = ClientLease {
                request_id: "request_a".into(),
                token: "signed.active.token".into(),
                session: session_from_claims(&active, 2, 2).unwrap(),
                claims: active,
            };
            client.lease = Some(current.clone());
            let frame = ClaimRejectedFrame {
                kind: "claim_rejected".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                request_id: request_id.into(),
                code: "rate_limited".into(),
                message: "too many requests".into(),
            };
            let encoded = serde_json::to_string(&frame).unwrap();

            assert_eq!(
                prepare_claim_rejection(&mut client, &frame, &encoded).unwrap(),
                ClaimRejectionAction::ContainedUnmatched
            );
            assert_eq!(client.lease.as_ref(), Some(&current));
            assert!(client.pending.is_none());
            assert_eq!(client.applied_relay_frames.len(), 1);

            assert_eq!(
                prepare_claim_rejection(&mut client, &frame, &encoded).unwrap(),
                ClaimRejectionAction::AlreadyApplied
            );
            assert_eq!(client.lease.as_ref(), Some(&current));
            assert_eq!(client.applied_relay_frames.len(), 1);

            let mut changed = frame;
            changed.message = "changed refusal".into();
            let changed_encoded = serde_json::to_string(&changed).unwrap();
            assert_eq!(
                prepare_claim_rejection(&mut client, &changed, &changed_encoded).unwrap_err(),
                "relay authority frame identifier was reused with different content"
            );
            assert_eq!(client.lease.as_ref(), Some(&current));
        }
    }

    #[test]
    fn peer_trust_profile_is_separate_from_managed_app_and_deployment() {
        let managed_a: RealtimeConfig = serde_json::from_value(serde_json::json!({
            "gatewayUrl": "wss://issuer-a.example/v2/realtime",
            "appId": "app_a",
            "deviceId": "device_a",
            "accessToken": "",
            "protocolVersion": 2,
            "managedDeploymentId": "deployment_shared",
            "managedProfileId": "profile_issuer_a_deployment_shared_app_a"
        }))
        .unwrap();
        let profile_a = realtime_profile_id(&managed_a).unwrap();
        assert_eq!(profile_a, "profile_issuer_a_deployment_shared_app_a");
        assert_ne!(profile_a, managed_a.app_id);
        assert_ne!(profile_a, managed_a.managed_deployment_id.clone().unwrap());

        let managed_b: RealtimeConfig = serde_json::from_value(serde_json::json!({
            "gatewayUrl": "wss://issuer-b.example/v2/realtime",
            "appId": "app_b",
            "deviceId": "device_b",
            "accessToken": "",
            "protocolVersion": 2,
            "managedDeploymentId": "deployment_shared",
            "managedProfileId": "profile_issuer_b_deployment_shared_app_b"
        }))
        .unwrap();
        assert_eq!(
            managed_a.managed_deployment_id,
            managed_b.managed_deployment_id
        );
        assert_ne!(
            realtime_profile_id(&managed_a).unwrap(),
            realtime_profile_id(&managed_b).unwrap()
        );

        let custom: RealtimeConfig = serde_json::from_value(serde_json::json!({
            "gatewayUrl": "wss://self-hosted.example/v2/realtime",
            "appId": "app_self_hosted",
            "deviceId": "device_self_hosted",
            "accessToken": "self-hosted-token",
            "protocolVersion": 2
        }))
        .unwrap();
        let custom_profile = realtime_profile_id(&custom).unwrap();
        assert!(custom_profile.starts_with("custom_"));
        assert_eq!(custom_profile, realtime_profile_id(&custom).unwrap());

        let incomplete: RealtimeConfig = serde_json::from_value(serde_json::json!({
            "gatewayUrl": "wss://self-hosted.example/v2/realtime",
            "appId": "app_self_hosted",
            "deviceId": "device_self_hosted",
            "accessToken": "",
            "protocolVersion": 2,
            "managedDeploymentId": "deployment_orphaned"
        }))
        .unwrap();
        assert!(realtime_profile_id(&incomplete).is_err());
    }

    fn claims(mode: LeaseMode, phase: LeasePhase) -> LeaseClaims {
        LeaseClaims {
            aud: LEASE_AUDIENCE.into(),
            app_id: "app_a".into(),
            plugin_id: "plugin_a".into(),
            device_id: "device_a".into(),
            plugin_key_thumbprint: "plugin_key_a".into(),
            mobile_key_thumbprint: "mobile_key_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            mode,
            phase,
            tracks: tracks_for(mode, phase),
            expires_at: unix_now().unwrap() + 20,
            lease_id: "media_a".into(),
            jti: "lease_a".into(),
            fence: if matches!(mode, LeaseMode::Takeover) {
                9
            } else {
                0
            },
            session_nonce: "session_a".into(),
            rtc_session_id: "rtc_a".into(),
        }
    }

    fn lease_status_client(mode: LeaseMode, phase: LeasePhase) -> (ClientState, LeaseClaims) {
        let mut client = offered_client(mode, MobileOfferSurface::InApp);
        client.peer_key_thumbprint = Some("plugin_key_a".into());
        let mut lease_claims = claims(mode, phase);
        lease_claims.mobile_key_thumbprint = client
            .endpoint_identity
            .as_ref()
            .expect("fixture endpoint identity")
            .thumbprint()
            .into();
        lease_claims.plugin_key_thumbprint = "plugin_key_a".into();
        (client, lease_claims)
    }

    fn plugin_bound_lease_status_client(
        mode: LeaseMode,
        phase: LeasePhase,
    ) -> (
        ClientState,
        LeaseClaims,
        crate::endpoint_identity::EndpointIdentity,
    ) {
        let (mut client, mut lease_claims) = lease_status_client(mode, phase);
        let plugin = crate::endpoint_identity::EndpointIdentity::from_secret([31; 32])
            .expect("test plugin identity");
        lease_claims.plugin_key_thumbprint = plugin.thumbprint().into();
        client.peer_key_thumbprint = Some(plugin.thumbprint().into());
        (client, lease_claims, plugin)
    }

    fn signed_plugin_rtc_for_lease(
        plugin: &crate::endpoint_identity::EndpointIdentity,
        lease: &ClientLease,
        signal_id: &str,
        proof_jti: &str,
        issued_at: u64,
        expires_at: u64,
    ) -> GatewayRtcFrame {
        let envelope = plugin
            .sign_candidate(TrickleCandidateClaims {
                app_id: lease.claims.app_id.clone(),
                plugin_id: lease.claims.plugin_id.clone(),
                device_id: lease.claims.device_id.clone(),
                rtc_session_id: lease.claims.rtc_session_id.clone(),
                endpoint_session_nonce: "plugin_session_a".into(),
                lease_jti: lease.claims.jti.clone(),
                endpoint_role: AdmissionRole::Plugin,
                holder_key_thumbprint: plugin.thumbprint().into(),
                peer_key_thumbprint: lease.claims.mobile_key_thumbprint.clone(),
                call_id: lease.claims.call_id.clone(),
                call_epoch: lease.claims.call_epoch,
                owner_epoch: lease.claims.owner_epoch,
                fence: lease.claims.fence,
                sdp_revision: lease.session.sdp_revision,
                transport_generation: lease.session.transport_generation,
                candidate: None,
                sdp_mid: None,
                sdp_m_line_index: None,
                end_of_candidates: true,
                nonce: format!("nonce_{proof_jti}"),
                jti: proof_jti.into(),
                issued_at,
                expires_at,
            })
            .expect("test RTC proof signs");
        GatewayRtcFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: lease.claims.app_id.clone(),
            signal_id: signal_id.into(),
            plugin_id: lease.claims.plugin_id.clone(),
            device_id: lease.claims.device_id.clone(),
            lease_jti: lease.claims.jti.clone(),
            rtc_session_id: lease.claims.rtc_session_id.clone(),
            sdp_revision: lease.session.sdp_revision,
            transport_generation: lease.session.transport_generation,
            call_id: lease.claims.call_id.clone(),
            call_epoch: lease.claims.call_epoch,
            owner_epoch: lease.claims.owner_epoch,
            fence: lease.claims.fence,
            signal: RtcSignal::IceComplete { envelope },
        }
    }

    fn pending_lease_for(claims: &LeaseClaims) -> PendingLease {
        let request_id = "request_a".to_string();
        PendingLease {
            request_id: request_id.clone(),
            offer_request_id: "offer_answer_a".into(),
            mode: claims.mode,
            rtc_session_id: claims.rtc_session_id.clone(),
            call_id: claims.call_id.clone(),
            call_epoch: claims.call_epoch,
            owner_epoch: claims.owner_epoch,
            accepted_offer_id: "offer_exact_a".into(),
            accepted_offer_jti: "offer_jti_exact_a".into(),
            lease_frame: LeaseRequestFrame {
                kind: "lease_request".into(),
                schema_version: SCHEMA_VERSION,
                app_id: claims.app_id.clone(),
                request_id: request_id.clone(),
                idempotency_key: "mobile:device_a:request_a".into(),
                call_id: claims.call_id.clone(),
                expected_call_epoch: claims.call_epoch,
                expected_owner_epoch: claims.owner_epoch,
                expected_switchboard_revision: 11,
                expected_remote_revision: 13,
                mode: claims.mode,
                rtc_session_id: claims.rtc_session_id.clone(),
                accepted_offer_id: "offer_exact_a".into(),
                accepted_offer_jti: "offer_jti_exact_a".into(),
                accepted_transfer_request_id: None,
            },
            stage: PendingLeaseStage::LeaseRequested,
            deadline: Instant::now() + LEASE_REQUEST_TIMEOUT,
            native_action_id: None,
            native_deadline: None,
        }
    }

    fn lease_status_frame(kind: &str, token: &str, claims: LeaseClaims) -> LeaseStatusFrame {
        LeaseStatusFrame {
            kind: kind.into(),
            schema_version: SCHEMA_VERSION,
            app_id: claims.app_id.clone(),
            request_id: "request_a".into(),
            lease_token: token.into(),
            provisional: match kind {
                "claim_provisional" => Some(true),
                "claim_active" => Some(false),
                _ => None,
            },
            lease: claims,
        }
    }

    async fn failing_lease_status(
        state: &V2State,
        frame: LeaseStatusFrame,
        native_calls: Arc<AtomicU64>,
        cleaned: Arc<Mutex<Vec<MediaSession>>>,
    ) -> Result<Option<AppliedLeaseStatus>, String> {
        let encoded = serde_json::to_string(&frame).unwrap();
        apply_lease_status_transaction(
            state,
            "app_a",
            frame,
            &encoded,
            move |_, _, _, _| {
                native_calls.fetch_add(1, Ordering::AcqRel);
                async { Err("injected native media failure".to_string()) }
            },
            move |sessions| async move {
                *cleaned.lock().unwrap() = sessions;
            },
        )
        .await
    }

    #[tokio::test]
    async fn active_create_failure_surrenders_exact_token_and_replay_is_a_no_op() {
        let state = V2State::default();
        let (mut client, prepared) = lease_status_client(LeaseMode::Takeover, LeasePhase::Prepared);
        client.lease = Some(ClientLease {
            request_id: "request_a".into(),
            token: "signed.prepared.token".into(),
            session: session_from_claims(&prepared, 1, 1).unwrap(),
            claims: prepared.clone(),
        });
        *state.inner.lock().await = client;

        let mut active = prepared;
        active.phase = LeasePhase::Active;
        active.tracks = tracks_for(LeaseMode::Takeover, LeasePhase::Active);
        active.owner_epoch += 1;
        active.expires_at += 1;
        active.jti = "lease_active_a".into();
        let frame = lease_status_frame("claim_active", "signed.active.token", active.clone());
        let encoded = serde_json::to_string(&frame).unwrap();
        let native_calls = Arc::new(AtomicU64::new(0));
        let cleaned = Arc::new(Mutex::new(Vec::new()));

        assert_eq!(
            failing_lease_status(&state, frame.clone(), native_calls.clone(), cleaned.clone(),)
                .await
                .unwrap_err(),
            "injected native media failure"
        );
        assert_eq!(native_calls.load(Ordering::Acquire), 1);
        assert_eq!(cleaned.lock().unwrap().len(), 2);

        {
            let client = state.inner.lock().await;
            assert!(
                client.lease.is_none(),
                "failed active authority must not heartbeat"
            );
            assert!(client.pending.is_none());
            assert!(client.applying_lease_status.is_none());
            let pending_revoke = client.pending_revoke.as_ref().expect("return fence");
            assert_eq!(pending_revoke.lease.token, "signed.active.token");
            assert_eq!(pending_revoke.lease_jti, "lease_active_a");
            let urgent = client.urgent_control_frames.front().expect("urgent revoke");
            assert_eq!(urgent.lease_id, active.lease_id);
            assert_eq!(urgent.lease_jti, active.jti);
            assert_eq!(urgent.rtc_session_id, active.rtc_session_id);
            assert_eq!(urgent.fence, active.fence);
            let revoke: LeaseRevokeFrame = serde_json::from_str(&urgent.encoded).unwrap();
            assert_eq!(revoke.lease_token, "signed.active.token");
        }
        assert!(heartbeat_frame(&state, "app_a").await.unwrap().is_none());
        let stale_offer_session = cleaned.lock().unwrap()[0].clone();
        assert!(local_rtc_frame(
            &state,
            MediaSignalEvent {
                session: stale_offer_session,
                signal: LocalSignal::Offer {
                    description: SdpSignal {
                        kind: SdpSignalType::Offer,
                        sdp: "stale rolled-back offer".into(),
                    },
                },
            },
        )
        .await
        .unwrap()
        .is_none());

        let replay_calls = native_calls.clone();
        let replay = apply_lease_status_transaction(
            &state,
            "app_a",
            frame.clone(),
            &encoded,
            move |_, _, _, _| {
                replay_calls.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            },
            |_| async {},
        )
        .await
        .unwrap();
        assert!(replay.is_none());
        assert_eq!(native_calls.load(Ordering::Acquire), 1);

        let mut changed = frame;
        changed.lease_token = "signed.changed.token".into();
        let changed_encoded = serde_json::to_string(&changed).unwrap();
        assert!(apply_lease_status_transaction(
            &state,
            "app_a",
            changed,
            &changed_encoded,
            |_, _, _, _| async { Ok(()) },
            |_| async {},
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn active_consult_status_runs_create_with_auto_arm_intent_before_commit() {
        let state = V2State::default();
        let (mut client, prepared) = lease_status_client(LeaseMode::Consult, LeasePhase::Prepared);
        client.lease = Some(ClientLease {
            request_id: "request_a".into(),
            token: "signed.consult.prepared".into(),
            session: session_from_claims(&prepared, 1, 1).unwrap(),
            claims: prepared.clone(),
        });
        *state.inner.lock().await = client;

        let mut active = prepared;
        active.phase = LeasePhase::Active;
        active.tracks = tracks_for(LeaseMode::Consult, LeasePhase::Active);
        active.owner_epoch += 1;
        active.expires_at += 1;
        active.jti = "lease_consult_active".into();
        let frame = lease_status_frame("claim_active", "signed.consult.active", active);
        let encoded = serde_json::to_string(&frame).unwrap();
        let native_calls = Arc::new(AtomicU64::new(0));
        let counted = native_calls.clone();
        let applied = apply_lease_status_transaction(
            &state,
            "app_a",
            frame,
            &encoded,
            move |operation, session, _, _| {
                counted.fetch_add(1, Ordering::AcqRel);
                async move {
                    assert_eq!(operation, LeaseOperation::Create);
                    assert_eq!(session.binding.mode, MediaMode::Consult);
                    assert!(lease_operation_auto_arms_private_consult(
                        operation, &session
                    ));
                    Ok(())
                }
            },
            |_| async {},
        )
        .await
        .unwrap();
        assert!(applied.is_some());
        assert_eq!(native_calls.load(Ordering::Acquire), 1);
        let client = state.inner.lock().await;
        assert_eq!(
            client.lease.as_ref().unwrap().session.binding.mode,
            MediaMode::Consult
        );
    }

    #[tokio::test]
    async fn provisional_create_failure_aborts_prepared_authority_and_survives_reconnect() {
        let state = V2State::default();
        let (mut client, prepared) = lease_status_client(LeaseMode::Takeover, LeasePhase::Prepared);
        client.pending = Some(pending_lease_for(&prepared));
        *state.inner.lock().await = client;
        let frame = lease_status_frame(
            "claim_provisional",
            "signed.prepared.received.token",
            prepared,
        );
        let native_calls = Arc::new(AtomicU64::new(0));
        let cleaned = Arc::new(Mutex::new(Vec::new()));

        failing_lease_status(&state, frame, native_calls, cleaned.clone())
            .await
            .unwrap_err();
        {
            let client = state.inner.lock().await;
            assert!(client.pending.is_none());
            assert!(client.lease.is_none());
            assert_eq!(cleaned.lock().unwrap().len(), 1);
            assert_eq!(client.urgent_control_frames.len(), 1);
            assert_eq!(
                client.pending_revoke.as_ref().unwrap().lease.token,
                "signed.prepared.received.token"
            );
        }

        state.reset().await;
        let urgent = peek_urgent_control_frame(&state)
            .await
            .expect("urgent revoke must survive reset/reconnect");
        let revoke: LeaseRevokeFrame = serde_json::from_str(&urgent.encoded).unwrap();
        assert_eq!(revoke.lease_token, "signed.prepared.received.token");
        confirm_urgent_control_frame(&state, &urgent).await.unwrap();
        assert!(peek_urgent_control_frame(&state).await.is_none());
    }

    #[tokio::test]
    async fn native_expiry_before_renewal_is_contained_and_returns_renewed_token() {
        let state = V2State::default();
        let (mut client, active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        client.lease = Some(ClientLease {
            request_id: "request_a".into(),
            token: "signed.old.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active.clone(),
        });
        *state.inner.lock().await = client;

        let mut renewed = active;
        renewed.expires_at += 10;
        renewed.jti = "lease_renewed_a".into();
        let frame = lease_status_frame("lease_renewed", "signed.renewed.token", renewed);
        let native_calls = Arc::new(AtomicU64::new(0));
        let cleaned = Arc::new(Mutex::new(Vec::new()));
        let applied = failing_lease_status(&state, frame, native_calls, cleaned.clone())
            .await
            .expect("native-expired renewal is contained");
        assert!(applied.is_none());

        let client = state.inner.lock().await;
        assert!(client.lease.is_none());
        assert!(client.pending_revoke.is_some());
        assert_eq!(
            client.pending_revoke.as_ref().unwrap().lease.token,
            "signed.renewed.token"
        );
        assert_eq!(cleaned.lock().unwrap().len(), 2);
        assert_eq!(
            client
                .local_expiry_tombstones
                .iter()
                .find(|tombstone| tombstone.returnable)
                .expect("renewed local-expiry lineage")
                .lease
                .token,
            "signed.renewed.token"
        );
        drop(client);
        assert!(heartbeat_frame(&state, "app_a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn renewal_updates_the_exact_native_and_authoritative_session_before_next_tick() {
        let state = V2State::default();
        let (mut client, active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        let previous = ClientLease {
            request_id: "request_a".into(),
            token: "signed.old.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active.clone(),
        };
        client.lease = Some(previous.clone());
        *state.inner.lock().await = client;

        let mut renewed = active;
        renewed.expires_at += 10;
        renewed.jti = "lease_renewed_a".into();
        let expected_session = {
            let mut session = previous.session.clone();
            session.expires_at = expiry_datetime(renewed.expires_at).unwrap();
            session
        };
        let frame = lease_status_frame("lease_renewed", "signed.renewed.token", renewed.clone());
        let encoded = serde_json::to_string(&frame).unwrap();
        let native_session = Arc::new(Mutex::new(None));
        let observed = native_session.clone();

        let applied = apply_lease_status_transaction(
            &state,
            "app_a",
            frame,
            &encoded,
            move |operation, session, _, _| {
                *observed.lock().unwrap() = Some((operation, session));
                async { Ok(()) }
            },
            |_| async {},
        )
        .await
        .unwrap()
        .expect("renewal applies");

        assert_eq!(
            native_session.lock().unwrap().as_ref(),
            Some(&(LeaseOperation::Renew, expected_session.clone()))
        );
        assert_eq!(applied.lease_event.session, expected_session);
        let client = state.inner.lock().await;
        let current = client.lease.as_ref().expect("renewed authority");
        assert_eq!(current.token, "signed.renewed.token");
        assert_eq!(current.claims.jti, renewed.jti);
        assert_eq!(current.claims.expires_at, renewed.expires_at);
        assert_eq!(
            current.session.expires_at,
            expiry_datetime(renewed.expires_at).unwrap()
        );
        assert!(client.pending_revoke.is_none());
    }

    #[tokio::test]
    async fn failed_commit_never_erases_a_newer_concurrent_lease() {
        let state = V2State::default();
        let (mut client, active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        client.lease = Some(ClientLease {
            request_id: "request_a".into(),
            token: "signed.old.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active.clone(),
        });
        *state.inner.lock().await = client;

        let mut renewed = active.clone();
        renewed.expires_at += 10;
        renewed.jti = "lease_renewed_a".into();
        let frame = lease_status_frame("lease_renewed", "signed.renewed.token", renewed);
        let encoded = serde_json::to_string(&frame).unwrap();
        let racing_state = state.clone();
        let mut concurrent_claims = active;
        concurrent_claims.owner_epoch += 2;
        concurrent_claims.expires_at += 12;
        concurrent_claims.jti = "lease_concurrent_a".into();
        let concurrent = ClientLease {
            request_id: "request_concurrent".into(),
            token: "signed.concurrent.token".into(),
            session: session_from_claims(&concurrent_claims, 3, 3).unwrap(),
            claims: concurrent_claims,
        };
        let expected = concurrent.clone();

        assert!(apply_lease_status_transaction(
            &state,
            "app_a",
            frame,
            &encoded,
            move |_, _, _, _| async move {
                racing_state.inner.lock().await.lease = Some(concurrent);
                Ok(())
            },
            |_| async {},
        )
        .await
        .is_err());
        let client = state.inner.lock().await;
        assert_eq!(client.lease.as_ref(), Some(&expected));
        assert!(client.pending_revoke.is_none());
        assert_eq!(client.urgent_control_frames.len(), 1);
        assert!(client.applying_lease_status.is_none());
    }

    #[tokio::test]
    async fn current_narrow_grants_block_queued_status_native_work_and_heartbeats() {
        let pending_state = V2State::default();
        let (mut pending_client, prepared) =
            lease_status_client(LeaseMode::Takeover, LeasePhase::Prepared);
        // The old projected snapshot deliberately remains broad. A rotated
        // managed admission is independent, fresher authority.
        pending_client.admission_grants = Some(vec![Grant::StateRead, Grant::RtcSignal]);
        pending_client.pending = Some(pending_lease_for(&prepared));
        *pending_state.inner.lock().await = pending_client;
        let frame = lease_status_frame("claim_provisional", "signed.old.broad.token", prepared);
        let encoded = serde_json::to_string(&frame).unwrap();
        let native_calls = Arc::new(AtomicU64::new(0));
        let counted = native_calls.clone();
        assert!(apply_lease_status_transaction(
            &pending_state,
            "app_a",
            frame,
            &encoded,
            move |_, _, _, _| {
                counted.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            },
            |_| async {},
        )
        .await
        .is_err());
        assert_eq!(native_calls.load(Ordering::Acquire), 0);
        let pending_client = pending_state.inner.lock().await;
        assert!(pending_client.pending.is_some());
        assert!(pending_client.lease.is_none());
        assert!(pending_client.applying_lease_status.is_none());
        drop(pending_client);

        let active_state = V2State::default();
        let (mut active_client, active) =
            lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        active_client.admission_grants = Some(vec![Grant::StateRead]);
        active_client.lease = Some(ClientLease {
            request_id: "request_a".into(),
            token: "signed.old.active.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active,
        });
        *active_state.inner.lock().await = active_client;
        assert!(heartbeat_frame(&active_state, "app_a")
            .await
            .unwrap()
            .is_none());

        let offer_state = V2State::default();
        let mut offered = offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
        offered.admission_grants = Some(vec![Grant::StateRead, Grant::RtcSignal]);
        *offer_state.inner.lock().await = offered;
        assert!(
            prepare_offer_answer(&offer_state, LeaseMode::Takeover, None)
                .await
                .is_err()
        );
        assert!(offer_state.inner.lock().await.pending.is_none());
    }

    #[tokio::test]
    async fn expired_current_consent_blocks_active_consult_before_native_and_heartbeat() {
        let state = V2State::default();
        let (mut client, mut prepared) =
            lease_status_client(LeaseMode::Consult, LeasePhase::Prepared);
        prepared.expires_at = unix_now().unwrap() + 10;
        client
            .snapshot
            .as_mut()
            .unwrap()
            .snapshot
            .remote_consent
            .expires_at = Some("2000-01-01T00:00:00Z".into());
        client.lease = Some(ClientLease {
            request_id: "request_a".into(),
            token: "signed.consult.prepared".into(),
            session: session_from_claims(&prepared, 1, 1).unwrap(),
            claims: prepared.clone(),
        });
        *state.inner.lock().await = client;

        let mut active = prepared;
        active.phase = LeasePhase::Active;
        active.tracks = tracks_for(LeaseMode::Consult, LeasePhase::Active);
        active.owner_epoch += 1;
        active.expires_at += 1;
        active.jti = "lease_consult_expired_consent".into();
        let frame = lease_status_frame("claim_active", "signed.consult.active", active);
        let encoded = serde_json::to_string(&frame).unwrap();
        let native_calls = Arc::new(AtomicU64::new(0));
        let counted = native_calls.clone();
        assert!(apply_lease_status_transaction(
            &state,
            "app_a",
            frame,
            &encoded,
            move |_, _, _, _| {
                counted.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            },
            |_| async {},
        )
        .await
        .is_err());
        assert_eq!(native_calls.load(Ordering::Acquire), 0);
        assert!(heartbeat_frame(&state, "app_a").await.unwrap().is_none());
        assert_eq!(
            state
                .inner
                .lock()
                .await
                .lease
                .as_ref()
                .unwrap()
                .session
                .binding
                .mode,
            MediaMode::PreparedConsult,
        );
    }

    #[test]
    fn admission_rotation_immediately_extracts_takeover_and_consult_for_exact_return() {
        for mode in [LeaseMode::Takeover, LeaseMode::Consult] {
            let state = V2State::default();
            let (mut client, active) = lease_status_client(mode, LeasePhase::Active);
            let token = format!("signed.{mode:?}.token");
            client.lease = Some(ClientLease {
                request_id: "request_a".into(),
                token: token.clone(),
                session: session_from_claims(&active, 2, 2).unwrap(),
                claims: active.clone(),
            });
            let deescalation = apply_rotated_admission_grants(
                &mut client,
                &[Grant::StateRead, Grant::RtcSignal, Grant::Monitor],
            );
            assert!(client.lease.is_none(), "{mode:?} must stop immediately");
            let lease = deescalation.lease.expect("exact lease to close and return");
            assert_eq!(lease.token, token);
            assert_eq!(lease.claims.jti, active.jti);
            queue_exact_lease_revocation(&state, &mut client, &lease, "admission_grant_revoked")
                .unwrap();
            let urgent = client.urgent_control_frames.front().unwrap();
            assert_eq!(urgent.lease_jti, active.jti);
            assert_eq!(urgent.rtc_session_id, active.rtc_session_id);
            let revoke: LeaseRevokeFrame = serde_json::from_str(&urgent.encoded).unwrap();
            assert_eq!(revoke.lease_token, token);
            assert!(client.pending_revoke.is_some());
        }
    }

    #[test]
    fn prepared_takeover_has_no_armable_microphone() {
        let takeover =
            session_from_claims(&claims(LeaseMode::Takeover, LeasePhase::Prepared), 1, 1).unwrap();
        assert_eq!(takeover.binding.mode, MediaMode::PreparedTalk);
        assert!(!takeover.binding.mode.needs_microphone());
    }

    #[test]
    fn private_consult_rotates_from_receive_only_to_microphone() {
        assert_eq!(
            grant_for_requested_mode(LeaseMode::Consult),
            Ok(Grant::Consult)
        );
        let prepared =
            session_from_claims(&claims(LeaseMode::Consult, LeasePhase::Prepared), 1, 1).unwrap();
        assert_eq!(prepared.binding.mode, MediaMode::PreparedConsult);
        assert!(!prepared.binding.mode.needs_microphone());
        assert!(!lease_operation_auto_arms_private_consult(
            LeaseOperation::Create,
            &prepared,
        ));
        let active =
            session_from_claims(&claims(LeaseMode::Consult, LeasePhase::Active), 2, 2).unwrap();
        assert_eq!(active.binding.mode, MediaMode::Consult);
        assert!(active.binding.mode.needs_microphone());
        assert!(lease_operation_auto_arms_private_consult(
            LeaseOperation::Create,
            &active,
        ));
        assert!(!lease_operation_auto_arms_private_consult(
            LeaseOperation::Renew,
            &active,
        ));
        let takeover =
            session_from_claims(&claims(LeaseMode::Takeover, LeasePhase::Active), 2, 2).unwrap();
        assert!(!lease_operation_auto_arms_private_consult(
            LeaseOperation::Create,
            &takeover,
        ));
    }

    #[test]
    fn failed_consult_surrender_is_exact_session_fenced() {
        let (mut client, active_claims) =
            lease_status_client(LeaseMode::Consult, LeasePhase::Active);
        let exact = ClientLease {
            request_id: "request_a".into(),
            token: "signed.consult.token".into(),
            session: session_from_claims(&active_claims, 2, 2).unwrap(),
            claims: active_claims,
        };
        client.lease = Some(exact.clone());

        let mut stale_session = exact.session.clone();
        stale_session.transport_generation += 1;
        assert!(take_failed_private_consult(&mut client, &stale_session).is_none());
        assert_eq!(client.lease.as_ref(), Some(&exact));

        let returned = take_failed_private_consult(&mut client, &exact.session)
            .expect("exact failed Consult is returned");
        assert_eq!(returned, exact);
        assert!(client.lease.is_none());
    }

    #[test]
    fn active_takeover_is_talk_and_requires_rotated_authority() {
        let prepared = claims(LeaseMode::Takeover, LeasePhase::Prepared);
        let current = ClientLease {
            request_id: "request_a".into(),
            token: "token".into(),
            session: session_from_claims(&prepared, 1, 1).unwrap(),
            claims: prepared,
        };
        let mut active = current.claims.clone();
        active.phase = LeasePhase::Active;
        active.tracks = vec![MediaTrack::PstnIn, MediaTrack::PstnOut];
        active.owner_epoch += 1;
        active.expires_at += 1;
        active.jti = "lease_b".into();
        assert!(validate_renewal(&current, &active).is_err());
        let session = session_from_claims(&active, 2, 2).unwrap();
        assert_eq!(session.binding.mode, MediaMode::Talk);
    }

    #[test]
    fn fresh_lease_waits_for_the_real_renewal_window() {
        let now = 1_700_000_000;
        assert!(!lease_heartbeat_due(now + 20, now));
        assert!(!lease_heartbeat_due(now + 15, now));
        assert!(lease_heartbeat_due(now + 14, now));
        assert!(lease_heartbeat_due(now + 1, now));
        assert!(!lease_heartbeat_due(now, now));
    }

    #[tokio::test]
    async fn expired_lease_is_contained_without_failing_the_realtime_session() {
        let state = V2State::default();
        let (mut client, mut active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        active.expires_at = unix_now().unwrap();
        let expired = ClientLease {
            request_id: "request_a".into(),
            token: "signed.expired.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active.clone(),
        };
        client.lease = Some(expired.clone());
        *state.inner.lock().await = client;

        // Expiry is deliberately no frame, not an Err: the session loop must
        // remain connected and must not emit another mobile_hello.
        assert!(heartbeat_frame(&state, "app_a").await.unwrap().is_none());
        let (returned, queue_error) = take_expired_lease(&state)
            .await
            .expect("exact expired authority is extracted");
        assert_eq!(returned, expired);
        assert!(queue_error.is_none());

        let client = state.inner.lock().await;
        assert!(client.lease.is_none());
        let pending = client.pending_revoke.as_ref().expect("return fence");
        assert_eq!(pending.lease_jti, active.jti);
        assert_eq!(pending.lease.token, "signed.expired.token");
        let queued = client.urgent_control_frames.front().expect("exact return");
        let frame: LeaseRevokeFrame = serde_json::from_str(&queued.encoded).unwrap();
        assert_eq!(frame.lease_token, "signed.expired.token");
        drop(client);
        assert!(take_expired_lease(&state).await.is_none());
    }

    #[tokio::test]
    async fn late_renewal_advances_expired_lineage_and_queues_the_renewed_token() {
        let state = V2State::default();
        let (mut client, mut active) = lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        active.expires_at = unix_now().unwrap();
        let expired = ClientLease {
            request_id: "request_a".into(),
            token: "signed.expired.token".into(),
            session: session_from_claims(&active, 2, 2).unwrap(),
            claims: active.clone(),
        };
        client.lease = Some(expired);
        *state.inner.lock().await = client;
        take_expired_lease(&state)
            .await
            .expect("local expiry extracts authority");

        // Model the short UI confirmation timeout and an already-sent old
        // revoke. The longer security tombstone must still contain a status
        // delayed within the plugin's lease window and enqueue a fresh return.
        {
            let mut client = state.inner.lock().await;
            client.pending_revoke = None;
            client.urgent_control_frames.clear();
        }
        let mut renewed = active;
        renewed.expires_at = unix_now().unwrap() + 20;
        renewed.jti = "lease_renewed_after_expiry".into();
        let frame = lease_status_frame(
            "lease_renewed",
            "signed.renewed.after.expiry",
            renewed.clone(),
        );
        let encoded = serde_json::to_string(&frame).unwrap();
        let native_calls = Arc::new(AtomicU64::new(0));
        let counted = native_calls.clone();

        let applied = apply_lease_status_transaction(
            &state,
            "app_a",
            frame,
            &encoded,
            move |_, _, _, _| {
                counted.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            },
            |_| async {},
        )
        .await
        .expect("exact late renewal is contained");
        assert!(applied.is_none());
        assert_eq!(native_calls.load(Ordering::Acquire), 0);

        let client = state.inner.lock().await;
        assert!(client.lease.is_none());
        let pending = client
            .pending_revoke
            .as_ref()
            .expect("renewed return fence");
        assert_eq!(pending.lease_jti, renewed.jti);
        assert_eq!(pending.lease.token, "signed.renewed.after.expiry");
        let queued = client
            .urgent_control_frames
            .back()
            .expect("renewed return is always queued");
        let revoke: LeaseRevokeFrame = serde_json::from_str(&queued.encoded).unwrap();
        assert_eq!(revoke.lease_token, "signed.renewed.after.expiry");
        assert_eq!(client.urgent_control_frames.len(), 1);
        assert_eq!(
            client
                .local_expiry_tombstones
                .iter()
                .find(|tombstone| tombstone.returnable)
                .expect("advanced expiry lineage")
                .lease
                .claims
                .jti,
            renewed.jti
        );
    }

    #[tokio::test]
    async fn expired_inbound_renewal_is_returned_before_tick_despite_revoked_permissions() {
        let state = V2State::default();
        let (mut client, mut expired_claims, _) =
            plugin_bound_lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        let now = unix_now().unwrap();
        expired_claims.expires_at = now - 20;
        let expired = ClientLease {
            request_id: "request_a".into(),
            token: "signed.expired.before.tick".into(),
            session: session_from_claims(&expired_claims, 2, 2).unwrap(),
            claims: expired_claims.clone(),
        };
        client.lease = Some(expired);
        client.admission_grants = Some(vec![Grant::StateRead]);
        let consent = &mut client.snapshot.as_mut().unwrap().snapshot.remote_consent;
        consent.enabled = false;
        consent.acknowledged = false;
        *state.inner.lock().await = client;

        let mut renewed = expired_claims;
        renewed.expires_at = now - 1;
        renewed.jti = "lease_expired_renewal_before_tick".into();
        let frame = lease_status_frame(
            "lease_renewed",
            "signed.expired.renewal.before.tick",
            renewed.clone(),
        );
        let encoded = serde_json::to_string(&frame).unwrap();
        let native_calls = Arc::new(AtomicU64::new(0));
        let cleanup_calls = Arc::new(AtomicU64::new(0));
        let counted_native = native_calls.clone();
        let counted_cleanup = cleanup_calls.clone();
        let applied = apply_lease_status_transaction(
            &state,
            "app_a",
            frame,
            &encoded,
            move |_, _, _, _| {
                counted_native.fetch_add(1, Ordering::AcqRel);
                async { Ok(()) }
            },
            move |sessions| {
                counted_cleanup.fetch_add(sessions.len() as u64, Ordering::AcqRel);
                async {}
            },
        )
        .await
        .expect("expired exact lineage is contained");

        assert!(applied.is_none());
        assert_eq!(native_calls.load(Ordering::Acquire), 0);
        assert_eq!(cleanup_calls.load(Ordering::Acquire), 1);
        let client = state.inner.lock().await;
        assert!(client.lease.is_none());
        assert_eq!(
            client.pending_revoke.as_ref().unwrap().lease_jti,
            renewed.jti
        );
        assert!(client.local_expiry_tombstones.iter().any(|tombstone| {
            tombstone.returnable && tombstone.lease.claims.jti == renewed.jti
        }));
        assert!(client
            .local_expiry_tombstones
            .iter()
            .any(|tombstone| { !tombstone.returnable && tombstone.lease.token.is_empty() }));
    }

    #[tokio::test]
    async fn expired_signed_rtc_is_authenticated_and_dropped_while_tampering_stays_fatal() {
        let state = V2State::default();
        let (mut client, mut claims, plugin) =
            plugin_bound_lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        let now = unix_now().unwrap();
        claims.expires_at = now - 1;
        let lease = ClientLease {
            request_id: "request_a".into(),
            token: "signed.expired.rtc".into(),
            session: session_from_claims(&claims, 2, 2).unwrap(),
            claims,
        };
        client.lease = Some(lease.clone());
        *state.inner.lock().await = client;
        {
            let mut client = state.inner.lock().await;
            take_expired_lease_locked(&state, &mut client, now)
                .expect("inbound path tombstones the expired peer");
        }

        let exact = signed_plugin_rtc_for_lease(
            &plugin,
            &lease,
            "late_signal_exact",
            "late_proof_exact",
            now - 20,
            now - 1,
        );
        {
            let mut client = state.inner.lock().await;
            assert!(contain_locally_closed_rtc(
                &mut client,
                &exact,
                "rtc:late_signal_exact",
                "exact-late-wire",
                now,
            )
            .expect("once-valid exact RTC is contained"));
        }

        let mut forged = signed_plugin_rtc_for_lease(
            &plugin,
            &lease,
            "late_signal_forged",
            "late_proof_forged",
            now - 20,
            now - 1,
        );
        if let RtcSignal::IceComplete { envelope } = &mut forged.signal {
            envelope.signature = "AAAA".into();
        }
        let mut altered = signed_plugin_rtc_for_lease(
            &plugin,
            &lease,
            "late_signal_altered",
            "late_proof_altered",
            now - 20,
            now - 1,
        );
        altered.fence += 1;
        let future = signed_plugin_rtc_for_lease(
            &plugin,
            &lease,
            "late_signal_future",
            "late_proof_future",
            now + 20,
            now + 40,
        );
        let mut client = state.inner.lock().await;
        for (frame, key) in [
            (forged, "rtc:late_signal_forged"),
            (altered, "rtc:late_signal_altered"),
            (future, "rtc:late_signal_future"),
        ] {
            assert!(contain_locally_closed_rtc(&mut client, &frame, key, key, now).is_err());
        }
    }

    #[tokio::test]
    async fn old_expiry_lineage_coexists_with_new_authority_and_a_second_expiry() {
        let state = V2State::default();
        let (mut client, mut old_claims, plugin) =
            plugin_bound_lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        let now = unix_now().unwrap();
        old_claims.expires_at = now;
        let old = ClientLease {
            request_id: "request_old".into(),
            token: "signed.old".into(),
            session: session_from_claims(&old_claims, 2, 2).unwrap(),
            claims: old_claims.clone(),
        };
        client.lease = Some(old.clone());
        *state.inner.lock().await = client;
        take_expired_lease(&state)
            .await
            .expect("old authority expires");
        {
            let mut client = state.inner.lock().await;
            client.pending_revoke = None;
            client.urgent_control_frames.clear();
            let mut newer_claims = old_claims.clone();
            newer_claims.lease_id = "media_new".into();
            newer_claims.jti = "lease_new".into();
            newer_claims.rtc_session_id = "rtc_new".into();
            newer_claims.expires_at = now + 20;
            newer_claims.fence += 1;
            client.lease = Some(ClientLease {
                request_id: "request_new".into(),
                token: "signed.new".into(),
                session: session_from_claims(&newer_claims, 1, 1).unwrap(),
                claims: newer_claims,
            });
        }

        let mut renewed_old = old_claims;
        renewed_old.expires_at = now + 10;
        renewed_old.jti = "lease_old_renewed".into();
        let frame = lease_status_frame("lease_renewed", "signed.old.renewed", renewed_old.clone());
        let encoded = serde_json::to_string(&frame).unwrap();
        let applied = apply_lease_status_transaction(
            &state,
            "app_a",
            frame,
            &encoded,
            |_, _, _, _| async { panic!("old renewal must never touch the new native peer") },
            |_| async {},
        )
        .await
        .expect("old renewal is contained beside new authority");
        assert!(applied.is_none());

        let old_rtc = signed_plugin_rtc_for_lease(
            &plugin,
            &old,
            "old_route_after_renewal",
            "old_route_proof",
            now - 10,
            now,
        );
        let mut client = state.inner.lock().await;
        assert!(contain_locally_closed_rtc(
            &mut client,
            &old_rtc,
            "rtc:old_route_after_renewal",
            "old-route-wire",
            now,
        )
        .expect("tokenless ancestor still contains queued RTC"));
        assert_eq!(client.lease.as_ref().unwrap().claims.jti, "lease_new");
        assert!(client.pending_revoke.is_none());
        assert_eq!(client.local_expiry_tombstones.len(), 2);

        client.lease.as_mut().unwrap().claims.expires_at = now;
        client.lease.as_mut().unwrap().session.expires_at = expiry_datetime(now).unwrap();
        let second = take_expired_lease_locked(&state, &mut client, now)
            .expect("new authority can expire without overwriting old fences")
            .0;
        assert_eq!(second.claims.jti, "lease_new");
        assert_eq!(client.local_expiry_tombstones.len(), 3);
        assert!(client
            .local_expiry_tombstones
            .iter()
            .any(|tombstone| tombstone.lease.claims.jti == "lease_a"));
        assert!(client
            .local_expiry_tombstones
            .iter()
            .any(|tombstone| tombstone.lease.claims.jti == renewed_old.jti));
        assert!(client
            .local_expiry_tombstones
            .iter()
            .any(|tombstone| tombstone.lease.claims.jti == "lease_new"));
    }

    #[tokio::test]
    async fn prepared_lease_never_emits_a_heartbeat_even_when_renewal_is_due() {
        let state = V2State::default();
        let (mut client, mut prepared) =
            lease_status_client(LeaseMode::Takeover, LeasePhase::Prepared);
        let now = unix_now().unwrap();
        prepared.expires_at = now + 10;
        assert!(lease_heartbeat_due(prepared.expires_at, now));
        let current = ClientLease {
            request_id: "request_a".into(),
            token: "signed.prepared.token".into(),
            session: session_from_claims(&prepared, 1, 1).unwrap(),
            claims: prepared,
        };
        client.lease = Some(current.clone());
        *state.inner.lock().await = client;

        assert!(heartbeat_frame(&state, "app_a").await.unwrap().is_none());
        assert_eq!(state.inner.lock().await.lease.as_ref(), Some(&current));
    }

    fn authoritative_snapshot() -> aokie_protocol::v2::AuthoritativeCallSnapshot {
        aokie_protocol::v2::AuthoritativeCallSnapshot {
            call_id: "call_a".into(),
            call_epoch: 1,
            owner_epoch: 0,
            switchboard_revision: 1,
            remote_revision: 1,
            telephony_state: TelephonyState::Active,
            service_mode: ServiceMode::AokieActive,
            media_state: MediaState::Ready,
            remote_capabilities: RemoteCapabilities {
                software_hold: false,
                carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                secondary_call_observation: SecondaryCallObservation::Unknown,
                voice_consult: false,
                takeover: false,
            },
            secondary_call_policy: SecondaryCallPolicy::Normal,
            secondary_call: None,
            remote_consent: aokie_protocol::v2::RemoteConsentPolicy {
                policy_id: "aokie_remote_access".into(),
                policy_version: 3,
                enabled: false,
                acknowledged: false,
                acknowledged_at: None,
                expires_at: None,
                captions_enabled: false,
                assistance_enabled: false,
                monitor_enabled: false,
                consult_enabled: false,
                takeover_enabled: false,
            },
            caller: None,
            captions: Vec::new(),
            participants: Vec::new(),
            audio_levels: None,
            companion_microphone_muted: false,
            pending_mobile_offers: Vec::new(),
            occurred_at: Utc::now().to_rfc3339(),
        }
    }

    fn verified_shim() -> GatewayShim {
        let mut shim = GatewayShim::new(
            "app_a".into(),
            "device_a".into(),
            vec![Grant::StateRead, Grant::Monitor],
            "desktop_key_thumbprint_1".into(),
        );
        // The hello path is proven separately; these cases exercise the
        // translation, which only runs once the peer is proven.
        shim.peer_verified = true;
        shim
    }

    fn plugin_snapshot_frame() -> String {
        plugin_snapshot_frame_for("app_a")
    }

    fn plugin_snapshot_frame_for(app_id: &str) -> String {
        serde_json::to_string(&PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: app_id.into(),
            event_id: "event_1".into(),
            device_id: Some("device_a".into()),
            snapshot: authoritative_snapshot(),
        })
        .expect("fixture encodes")
    }

    /// The shim stands in for the gateway that used to sit between the two
    /// endpoints. This is the proof it works BEFORE any roster approval: the
    /// fixture is built with `aokie-protocol`'s own constructors and the result
    /// is asserted with its own validators.
    #[test]
    fn a_plugin_snapshot_translates_into_a_valid_mobile_snapshot() {
        let mut shim = verified_shim();

        let translated = shim
            .translate(&plugin_snapshot_frame())
            .expect("a well-formed plugin snapshot translates")
            .expect("a snapshot is a session frame");

        let frame: MobileSnapshotFrame =
            serde_json::from_str(&translated).expect("the translation is a mobile snapshot");
        assert_eq!(frame.kind, "snapshot");
        // The session's own gate is the real assertion: a translation the
        // protocol layer would reject is worthless.
        validate_snapshot(&frame, "app_a").expect("the translation passes the session's own gate");
        frame
            .snapshot
            .validate()
            .expect("the projection passes the protocol's own validator");
        assert_eq!(frame.snapshot.call_id, "call_a");
        // Participant presence is a gateway-side roster the plugin does not
        // publish, and inventing it would assert state no endpoint authored.
        assert!(frame.snapshot.participants.is_empty());
        // Offers ARE published on this carrier — this fixture simply carries
        // none, and an absent offer must never become a present one.
        assert!(frame.snapshot.pending_mobile_offers.is_empty());
        assert_eq!(frame.grants, vec![Grant::StateRead, Grant::Monitor]);
    }

    // ---- relay lease authority: plugin dialect in, mobile dialect out ----

    fn offer_for(
        thumbprint: &str,
        mode: LeaseMode,
        surface: MobileOfferSurface,
        offer_id: &str,
        jti: &str,
    ) -> SignedPendingMobileOffer {
        let now = unix_now().expect("clock");
        SignedPendingMobileOffer {
            offer: aokie_protocol::v2::PendingMobileOfferClaims {
                offer_id: offer_id.into(),
                opportunity_id: "opportunity_relay".into(),
                target_device_id: "device_a".into(),
                target_holder_key_thumbprint: thumbprint.into(),
                offered_mode: mode,
                surface,
                app_id: "app_a".into(),
                call_id: "call_a".into(),
                call_epoch: 1,
                owner_epoch: 0,
                switchboard_revision: 1,
                remote_revision: 1,
                accepted_transfer_request_id: None,
                required_consent_policy_id: "aokie_remote_access".into(),
                required_consent_policy_version: 3,
                required_grants: vec![
                    Grant::StateRead,
                    Grant::RtcSignal,
                    grant_for_requested_mode(mode).expect("mode grant"),
                ],
                issued_at: now,
                // The authority mints inside `MOBILE_OFFER_MAX_LIFETIME` and
                // republishes every snapshot, so an offer is always well clear
                // of expiry by the time a device can answer it.
                expires_at: now + 25,
                jti: jti.into(),
            },
            offer_token: "signed.offer.token".into(),
        }
    }

    /// An authoritative snapshot whose consent actually permits `mode`, so the
    /// offers it carries are answerable rather than merely well-formed.
    fn authoritative_snapshot_offering(
        mode: LeaseMode,
        offers: Vec<SignedPendingMobileOffer>,
    ) -> aokie_protocol::v2::AuthoritativeCallSnapshot {
        let mut snapshot = authoritative_snapshot();
        snapshot.remote_capabilities = RemoteCapabilities {
            software_hold: mode == LeaseMode::Consult,
            carrier_hold_evidence: CarrierHoldEvidence::Unknown,
            secondary_call_observation: SecondaryCallObservation::Unknown,
            voice_consult: mode == LeaseMode::Consult,
            takeover: mode == LeaseMode::Takeover,
        };
        snapshot.remote_consent = aokie_protocol::v2::RemoteConsentPolicy {
            policy_id: "aokie_remote_access".into(),
            policy_version: 3,
            enabled: true,
            acknowledged: true,
            acknowledged_at: Some("2026-07-16T00:00:00Z".into()),
            expires_at: None,
            captions_enabled: false,
            assistance_enabled: false,
            monitor_enabled: mode == LeaseMode::Monitor,
            consult_enabled: mode == LeaseMode::Consult,
            takeover_enabled: mode == LeaseMode::Takeover,
        };
        snapshot.pending_mobile_offers = offers;
        snapshot
    }

    fn shim_granting(grants: Vec<Grant>) -> GatewayShim {
        let mut shim = GatewayShim::new(
            "app_a".into(),
            "device_a".into(),
            grants,
            "desktop_key_thumbprint_1".into(),
        );
        shim.peer_verified = true;
        shim
    }

    fn plugin_frame(value: Value) -> String {
        value.to_string()
    }

    /// A genuinely signed, plugin-role end-of-candidates envelope.
    ///
    /// ⚠️ Hand-rolled JSON would be worthless here: `PluginRtcSignalFrame`
    /// verifies the signature AND pins all twelve route fields against the
    /// frame's own, so a fixture that skipped either would prove the
    /// pass-through accepts things the real path never sees.
    fn plugin_ice_complete_signal(app_id: &str) -> Value {
        let plugin = crate::endpoint_identity::EndpointIdentity::from_secret([31; 32])
            .expect("test plugin identity");
        let now = unix_now().expect("clock");
        let envelope = plugin
            .sign_candidate(TrickleCandidateClaims {
                app_id: app_id.into(),
                plugin_id: "plugin_a".into(),
                device_id: "device_a".into(),
                rtc_session_id: "rtc_a".into(),
                endpoint_session_nonce: "session_p".into(),
                lease_jti: "lease_a".into(),
                endpoint_role: AdmissionRole::Plugin,
                holder_key_thumbprint: plugin.thumbprint().into(),
                peer_key_thumbprint: "mobile_key_a".into(),
                call_id: "call_a".into(),
                call_epoch: 7,
                owner_epoch: 4,
                fence: 9,
                sdp_revision: 1,
                transport_generation: 1,
                candidate: None,
                sdp_mid: None,
                sdp_m_line_index: None,
                end_of_candidates: true,
                nonce: "ice_nonce_p".into(),
                jti: "ice_proof_p".into(),
                issued_at: now,
                expires_at: now + 20,
            })
            .expect("the candidate envelope signs");
        serde_json::json!({ "type": "ice_complete", "envelope": envelope })
    }

    fn plugin_rtc_frame(app_id: &str) -> String {
        plugin_frame(serde_json::json!({
            "kind": "rtc_signal",
            "schemaVersion": SCHEMA_VERSION,
            "appId": app_id,
            "signalId": "signal_a",
            "pluginId": "plugin_a",
            "deviceId": "device_a",
            "leaseJti": "lease_a",
            "rtcSessionId": "rtc_a",
            "sdpRevision": 1,
            "transportGeneration": 1,
            "callId": "call_a",
            "callEpoch": 7,
            "ownerEpoch": 4,
            "fence": 9,
            "signal": plugin_ice_complete_signal(app_id),
        }))
    }

    fn plugin_lease_status_frame(
        status: PluginLeaseStatus,
        mode: LeaseMode,
        phase: LeasePhase,
    ) -> String {
        serde_json::to_string(&PluginLeaseStatusFrame {
            kind: "plugin_lease_status".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            request_id: "request_a".into(),
            status,
            lease_token: "signed.lease.token".into(),
            lease: claims(mode, phase),
        })
        .expect("fixture encodes")
    }

    /// The gap the operator actually hit: the Desktop published offers and the
    /// shim threw them away, so `select_mobile_offer` could never find one and
    /// every takeover failed locally with "no current signed mobile offer
    /// permits this lease".
    #[test]
    fn the_shim_projects_offers_the_desktop_published() {
        let mut shim = shim_granting(vec![
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Takeover,
            Grant::ResumeAokie,
        ]);
        let offers = vec![
            offer_for(
                "mobile_thumb_a",
                LeaseMode::Takeover,
                MobileOfferSurface::InApp,
                "offer_in_app",
                "offer_jti_in_app",
            ),
            offer_for(
                "mobile_thumb_a",
                LeaseMode::Takeover,
                MobileOfferSurface::VoiceSystemUi,
                "offer_voice_ui",
                "offer_jti_voice_ui",
            ),
        ];
        let encoded = serde_json::to_string(&PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            event_id: "event_offers".into(),
            device_id: Some("device_a".into()),
            snapshot: authoritative_snapshot_offering(LeaseMode::Takeover, offers.clone()),
        })
        .expect("fixture encodes");

        let translated = shim
            .translate(&encoded)
            .expect("a snapshot carrying offers translates")
            .expect("a snapshot is a session frame");
        let frame: MobileSnapshotFrame =
            serde_json::from_str(&translated).expect("the translation is a mobile snapshot");

        // Relayed verbatim: the offer is the Desktop's statement, and the shim
        // is not entitled to edit one.
        assert_eq!(frame.snapshot.pending_mobile_offers, offers);
        validate_snapshot(&frame, "app_a").expect("the translation passes the session's own gate");
    }

    /// Relay mail can outlive the admission that caused the Desktop to publish
    /// it. The CURRENT shim grants and exact device target are therefore the
    /// disclosure boundary, never the breadth of the queued snapshot itself.
    #[test]
    fn queued_full_snapshot_is_redacted_by_current_narrow_grants_and_device() {
        let mut full = authoritative_snapshot_offering(
            LeaseMode::Takeover,
            vec![offer_for(
                "mobile_thumb_a",
                LeaseMode::Takeover,
                MobileOfferSurface::InApp,
                "offer_old_full",
                "offer_jti_old_full",
            )],
        );
        full.caller = Some(aokie_protocol::v2::CallerProjection {
            label: Some("Private caller".into()),
            masked_number: Some("04•••123".into()),
        });
        full.audio_levels = Some(vec![aokie_protocol::v2::NormalizedAudioLevel {
            source: aokie_protocol::v2::AudioLevelSource::Caller,
            participant_id: None,
            level_permille: 420,
        }]);
        let frame_for = |device_id: Option<&str>| {
            serde_json::to_string(&PluginSnapshotFrame {
                kind: "plugin_snapshot".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "event_old_full".into(),
                device_id: device_id.map(str::to_owned),
                snapshot: full.clone(),
            })
            .unwrap()
        };

        let mut narrow = shim_granting(vec![Grant::StateRead]);
        let translated = narrow
            .translate(&frame_for(Some("device_a")))
            .unwrap()
            .expect("an exact-device snapshot still projects safe state");
        let projected: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();
        assert!(projected.snapshot.caller.is_none());
        assert!(projected.snapshot.captions.is_none());
        assert!(projected.snapshot.audio_levels.is_none());
        assert!(projected.snapshot.pending_mobile_offers.is_empty());
        assert_eq!(projected.grants, vec![Grant::StateRead]);

        let mut missing_target = shim_granting(vec![Grant::StateRead]);
        assert_eq!(missing_target.translate(&frame_for(None)), Ok(None));
        let mut wrong_target = shim_granting(vec![Grant::StateRead]);
        assert_eq!(
            wrong_target.translate(&frame_for(Some("device_other"))),
            Ok(None)
        );
    }

    #[test]
    fn relay_projection_keeps_supported_assistance_and_owner_end_call_grants() {
        let mut shim = shim_granting(vec![
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::AssistanceRead,
            Grant::AssistanceRespond,
            Grant::Consult,
            Grant::EndCaller,
        ]);
        let translated = shim
            .translate(&plugin_snapshot_frame())
            .unwrap()
            .expect("verified relay snapshot");
        let snapshot: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();
        assert!(snapshot.grants.contains(&Grant::AssistanceRead));
        assert!(snapshot.grants.contains(&Grant::Consult));
        assert!(snapshot.grants.contains(&Grant::AssistanceRespond));
        assert!(snapshot.grants.contains(&Grant::EndCaller));
    }

    #[test]
    fn queued_lease_status_is_dropped_when_current_admission_lacks_its_mode() {
        let mut narrow = shim_granting(vec![Grant::StateRead, Grant::RtcSignal]);
        assert_eq!(
            narrow.translate(&plugin_lease_status_frame(
                PluginLeaseStatus::Active,
                LeaseMode::Takeover,
                LeasePhase::Active,
            )),
            Ok(None)
        );

        let mut wrong_device: PluginLeaseStatusFrame =
            serde_json::from_str(&plugin_lease_status_frame(
                PluginLeaseStatus::Active,
                LeaseMode::Takeover,
                LeasePhase::Active,
            ))
            .unwrap();
        wrong_device.device_id = "device_other".into();
        // Keep the self-consistency validator meaningful by moving the lease
        // with the target. The shim's own exact-device fence must still drop it.
        wrong_device.lease.device_id = "device_other".into();
        let mut broad = shim_granting(vec![
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Takeover,
            Grant::ResumeAokie,
        ]);
        assert_eq!(
            broad.translate(&serde_json::to_string(&wrong_device).unwrap()),
            Ok(None)
        );
    }

    #[test]
    fn expired_queued_lease_status_is_stale_mail_but_malformed_status_still_fails() {
        let mut expired: PluginLeaseStatusFrame = serde_json::from_str(&plugin_lease_status_frame(
            PluginLeaseStatus::Active,
            LeaseMode::Takeover,
            LeasePhase::Active,
        ))
        .unwrap();
        expired.lease.expires_at = unix_now().unwrap();
        let mut shim = shim_granting(vec![
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Takeover,
            Grant::ResumeAokie,
        ]);
        assert_eq!(
            shim.translate(&serde_json::to_string(&expired).unwrap()),
            Ok(None)
        );

        let mut malformed = serde_json::to_value(expired).unwrap();
        malformed.as_object_mut().unwrap().remove("leaseToken");
        assert!(shim.translate(&malformed.to_string()).is_err());
    }

    /// Offers are authority, so they ride the same gate as every other piece of
    /// authoritative state: a Desktop that has not proved its endpoint key
    /// cannot put an answerable offer in front of the operator.
    #[test]
    fn offers_from_an_unproven_peer_are_dropped() {
        let mut shim = GatewayShim::new(
            "app_unproven_offers".into(),
            "device_a".into(),
            vec![Grant::StateRead, Grant::RtcSignal, Grant::Takeover],
            "desktop_key_thumbprint_1".into(),
        );
        assert!(!shim.peer_verified);
        let mut snapshot = authoritative_snapshot_offering(
            LeaseMode::Takeover,
            vec![offer_for(
                "mobile_thumb_a",
                LeaseMode::Takeover,
                MobileOfferSurface::InApp,
                "offer_in_app",
                "offer_jti_in_app",
            )],
        );
        snapshot.call_id = "call_a".into();
        let encoded = serde_json::to_string(&PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_unproven_offers".into(),
            event_id: "event_offers".into(),
            device_id: Some("device_a".into()),
            snapshot,
        })
        .expect("fixture encodes");

        assert_eq!(shim.translate(&encoded), Ok(None));
    }

    /// The plugin names a lease transition in a FIELD; the session branches on
    /// the frame KIND, then cross-checks `provisional` against the phase. Get
    /// the pairing wrong and a prepared claim would present as a live one.
    #[test]
    fn plugin_lease_status_maps_each_status_to_its_mobile_kind() {
        let cases = [
            (
                PluginLeaseStatus::Granted,
                LeaseMode::Monitor,
                LeasePhase::Active,
                "lease_granted",
                None,
            ),
            (
                PluginLeaseStatus::Provisional,
                LeaseMode::Takeover,
                LeasePhase::Prepared,
                "claim_provisional",
                Some(true),
            ),
            (
                PluginLeaseStatus::Active,
                LeaseMode::Takeover,
                LeasePhase::Active,
                "claim_active",
                Some(false),
            ),
            (
                PluginLeaseStatus::Renewed,
                LeaseMode::Takeover,
                LeasePhase::Active,
                "lease_renewed",
                None,
            ),
        ];

        for (status, mode, phase, expected_kind, expected_provisional) in cases {
            let mut grants = vec![
                Grant::StateRead,
                Grant::RtcSignal,
                grant_for_requested_mode(mode).unwrap(),
            ];
            if mode == LeaseMode::Takeover {
                grants.push(Grant::ResumeAokie);
            }
            let mut shim = shim_granting(grants);
            let translated = shim
                .translate(&plugin_lease_status_frame(status, mode, phase))
                .expect("a well-formed lease status translates")
                .expect("a lease status is a session frame");
            let frame: LeaseStatusFrame =
                serde_json::from_str(&translated).expect("the translation is a lease status");
            assert_eq!(frame.kind, expected_kind);
            assert_eq!(frame.provisional, expected_provisional);
            assert_eq!(frame.request_id, "request_a");
            assert_eq!(frame.lease_token, "signed.lease.token");
            assert_eq!(frame.lease, claims(mode, phase));
            // The routing field has done its job and must not survive into the
            // mobile dialect, whose twin has no room for it.
            assert!(!translated.contains("deviceId\":\"device_a\",\"requestId"));
        }
    }

    /// ⚠️ CALLER SAFETY. A prepared lease has already silenced the AI
    /// receptionist, so it is deliberately non-renewable: refusing this
    /// translation is what stops a stuck prepare from heartbeating itself alive
    /// and holding a live caller in soft hold past its short lifetime.
    #[test]
    fn a_prepared_lease_can_never_be_renewed() {
        let mut shim = shim_granting(vec![
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Takeover,
            Grant::ResumeAokie,
        ]);
        assert!(shim
            .translate(&plugin_lease_status_frame(
                PluginLeaseStatus::Renewed,
                LeaseMode::Takeover,
                LeasePhase::Prepared,
            ))
            .is_err());
    }

    /// A status that contradicts the lease it carries would let one transition
    /// deliver another transition's authority.
    #[test]
    fn a_lease_status_that_contradicts_its_own_claims_is_refused() {
        for (status, mode, phase) in [
            // "Granted" is the monitor-only wording: a takeover must never
            // arrive already active without passing through prepare.
            (
                PluginLeaseStatus::Granted,
                LeaseMode::Takeover,
                LeasePhase::Active,
            ),
            // A monitor lease has no prepared phase to be provisional in.
            (
                PluginLeaseStatus::Provisional,
                LeaseMode::Monitor,
                LeasePhase::Active,
            ),
            (
                PluginLeaseStatus::Active,
                LeaseMode::Takeover,
                LeasePhase::Prepared,
            ),
        ] {
            let mut shim = shim_granting(vec![Grant::StateRead, Grant::Takeover]);
            assert!(
                shim.translate(&plugin_lease_status_frame(status, mode, phase))
                    .is_err(),
                "{status:?} must not carry a {mode:?}/{phase:?} lease"
            );
        }
    }

    #[test]
    fn plugin_offer_accepted_translates_and_drops_the_device_id() {
        let mut shim = shim_granting(vec![Grant::StateRead, Grant::Takeover]);
        let encoded = serde_json::to_string(&PluginOfferAcceptedFrame {
            kind: "plugin_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            request_id: "request_offer_a".into(),
            offer_id: "offer_in_app".into(),
            offer_jti: "offer_jti_in_app".into(),
            offered_mode: LeaseMode::Takeover,
            accepted: true,
        })
        .expect("fixture encodes");

        let translated = shim
            .translate(&encoded)
            .expect("a well-formed acceptance translates")
            .expect("an acceptance is a session frame");
        let frame: MobileOfferAcceptedFrame =
            serde_json::from_str(&translated).expect("the translation is a mobile acceptance");
        assert_eq!(frame.kind, "mobile_offer_accepted");
        assert_eq!(frame.request_id, "request_offer_a");
        assert_eq!(frame.offer_id, "offer_in_app");
        assert_eq!(frame.offer_jti, "offer_jti_in_app");
        assert_eq!(frame.offered_mode, LeaseMode::Takeover);
        assert!(frame.accepted);
        // `MobileOfferAcceptedFrame` is `deny_unknown_fields`, so a surviving
        // routing field would fail the parse above — assert it plainly anyway.
        assert!(!translated.contains("deviceId"));
    }

    /// ⚠️ CALLER SAFETY. This frame ALREADY arrives today and is dropped. The
    /// plugin emits it when a media route fails, which is the same moment the
    /// caller is handed back to the AI receptionist — so a Companion that
    /// cannot read it goes on telling the operator they are on a call that has
    /// already moved on without them.
    ///
    /// ⚠️ `requestId` is `None` DELIBERATELY, preserving the plugin's
    /// media-failure dialect. It can race a local return: local WebRTC closes
    /// before `lease_revoke` reaches Desktop, so the plugin may publish this
    /// requestless notice while `pending_revoke` exists. The receiver accepts
    /// `None` only on that sole pending return's exact `leaseId` + `leaseJti`
    /// fence; otherwise it matches the active lease on the same pair. Inventing
    /// a request id would falsely assert acknowledgement of a mobile request
    /// the plugin never observed.
    #[test]
    fn plugin_lease_revoke_translates_to_lease_revoked_without_the_extra_fields() {
        let mut shim = shim_granting(vec![Grant::StateRead, Grant::Takeover]);
        let encoded = serde_json::to_string(&PluginLeaseRevokeFrame {
            kind: "plugin_lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            lease_id: "media_a".into(),
            lease_jti: "lease_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            fence: 9,
            reason: "peer_connection_failed".into(),
        })
        .expect("fixture encodes");

        let translated = shim
            .translate(&encoded)
            .expect("a well-formed revoke translates")
            .expect("a revoke is a session frame");
        let frame: LeaseRevokedFrame =
            serde_json::from_str(&translated).expect("the translation is a mobile revoke");
        assert_eq!(frame.kind, "lease_revoked");
        assert_eq!(frame.lease_id, "media_a");
        assert_eq!(frame.lease_jti, "lease_a");
        assert_eq!(frame.reason, "peer_connection_failed");
        // The plugin revokes against a LEASE, not against a request this
        // session is waiting on, so there is no request to name.
        assert_eq!(frame.request_id, None);
        for dropped in ["deviceId", "callId", "callEpoch", "fence", "requestId"] {
            assert!(
                !translated.contains(dropped),
                "{dropped} has no home in the mobile twin"
            );
        }
    }

    /// Without this the operator watches a spinner until the local request
    /// timeout expires, instead of being told why the claim was refused.
    #[test]
    fn plugin_claim_rejected_translates() {
        let mut shim = shim_granting(vec![Grant::StateRead, Grant::Takeover]);
        let encoded = serde_json::to_string(&PluginClaimRejectedFrame {
            kind: "plugin_claim_rejected".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            request_id: "request_a".into(),
            code: "claimant_busy".into(),
            message: "another device is already preparing a claim".into(),
        })
        .expect("fixture encodes");

        let translated = shim
            .translate(&encoded)
            .expect("a well-formed rejection translates")
            .expect("a rejection is a session frame");
        let frame: ClaimRejectedFrame =
            serde_json::from_str(&translated).expect("the translation is a mobile rejection");
        assert_eq!(frame.kind, "claim_rejected");
        assert_eq!(frame.request_id, "request_a");
        assert_eq!(frame.code, "claimant_busy");
        assert_eq!(frame.message, "another device is already preparing a claim");
        assert!(!translated.contains("deviceId"));
    }

    #[test]
    fn assistance_request_passes_only_under_current_read_grant() {
        let frame = PluginAssistanceRequestFrame {
            kind: "assistance_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            event_id: "assistance_event_a".into(),
            request_id: "assistance_request_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            switchboard_revision: 11,
            remote_revision: 13,
            question: "Can you advise?".into(),
            context: None,
            transfer_offered: false,
            expires_at: unix_now().unwrap() + 20,
        };
        let encoded = serde_json::to_string(&frame).unwrap();
        let mut permitted = shim_granting(vec![
            Grant::StateRead,
            Grant::AssistanceRead,
            Grant::Consult,
        ]);
        assert_eq!(permitted.translate(&encoded), Ok(Some(encoded.clone())));

        let mut narrowed = shim_granting(vec![Grant::StateRead, Grant::Consult]);
        assert_eq!(narrowed.translate(&encoded), Ok(None));
    }

    #[test]
    fn expired_queued_assistance_is_stale_mail_but_malformed_request_still_fails() {
        let mut frame = PluginAssistanceRequestFrame {
            kind: "assistance_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            event_id: "assistance_event_expired".into(),
            request_id: "assistance_request_expired".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            switchboard_revision: 11,
            remote_revision: 13,
            question: "Can you advise?".into(),
            context: None,
            transfer_offered: false,
            expires_at: unix_now().unwrap(),
        };
        let mut shim = shim_granting(vec![Grant::StateRead, Grant::AssistanceRead]);
        assert_eq!(
            shim.translate(&serde_json::to_string(&frame).unwrap()),
            Ok(None)
        );

        frame.expires_at = unix_now().unwrap() + 20;
        let mut malformed = serde_json::to_value(frame).unwrap();
        malformed.as_object_mut().unwrap().remove("question");
        assert!(shim.translate(&malformed.to_string()).is_err());
    }

    /// The one frame that needs no rename. Re-encoding it could only introduce
    /// drift, and an SDP or ICE payload that drifts is a media path that never
    /// connects.
    #[test]
    fn rtc_signal_passes_through_byte_identically() {
        let mut shim = shim_granting(vec![
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Takeover,
            Grant::ResumeAokie,
        ]);
        let encoded = plugin_rtc_frame("app_a");

        let translated = shim
            .translate(&encoded)
            .expect("a well-formed RTC signal translates")
            .expect("an RTC signal is a session frame");
        assert_eq!(translated, encoded, "an RTC signal must not be rewritten");
        // And the session's own parser accepts what came back.
        let frame: GatewayRtcFrame =
            serde_json::from_str(&translated).expect("the pass-through is a session RTC frame");
        assert_eq!(frame.signal_id, "signal_a");
        assert_eq!(frame.lease_jti, "lease_a");
    }

    /// Widening the translated subset moved a boundary, so pin both sides of
    /// it.
    ///
    /// A kind the shim does not translate is carrier traffic and drops quietly
    /// — a future plugin must be able to say things this build has never heard
    /// of without churning the read path. A kind it DOES translate, arriving
    /// malformed, is refused loudly instead, exactly as `plugin_snapshot` has
    /// always been: acting on half a lease frame is not an option, and a
    /// silent drop would hide real drift between two halves that ship
    /// independently.
    #[test]
    fn an_untranslated_plugin_kind_is_still_dropped_rather_than_erroring() {
        let mut shim = shim_granting(vec![Grant::StateRead, Grant::Monitor]);
        for kind in ["something_a_future_plugin_invents"] {
            let encoded = plugin_frame(serde_json::json!({
                "kind": kind,
                "schemaVersion": SCHEMA_VERSION,
                "appId": "app_a",
            }));
            assert_eq!(shim.translate(&encoded), Ok(None), "{kind} must be dropped");
        }

        for translated in [
            "plugin_offer_accepted",
            "plugin_lease_status",
            "plugin_claim_rejected",
            "plugin_lease_revoke",
            "microphone_mute_status",
            "assistance_answer_accepted",
            "end_caller_challenge",
            "end_caller_submitted",
            "end_caller_result",
            "rtc_signal",
        ] {
            let encoded = plugin_frame(serde_json::json!({
                "kind": translated,
                "schemaVersion": SCHEMA_VERSION,
                "appId": "app_a",
            }));
            assert!(
                shim.translate(&encoded).is_err(),
                "a malformed {translated} must be refused, not swallowed"
            );
        }
    }

    /// One Desktop can hold admissions for several apps. Authority minted for
    /// one of them must never be honoured under another.
    #[test]
    fn a_translated_frame_bound_to_another_app_is_refused() {
        let foreign_status = serde_json::to_string(&PluginLeaseStatusFrame {
            kind: "plugin_lease_status".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_other".into(),
            device_id: "device_a".into(),
            request_id: "request_a".into(),
            status: PluginLeaseStatus::Granted,
            lease_token: "signed.lease.token".into(),
            lease: claims(LeaseMode::Monitor, LeasePhase::Active),
        })
        .expect("fixture encodes");
        let foreign_rejection = serde_json::to_string(&PluginClaimRejectedFrame {
            kind: "plugin_claim_rejected".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_other".into(),
            device_id: "device_a".into(),
            request_id: "request_a".into(),
            code: "stale_call".into(),
            message: "the call moved on".into(),
        })
        .expect("fixture encodes");
        let foreign_revoke = serde_json::to_string(&PluginLeaseRevokeFrame {
            kind: "plugin_lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_other".into(),
            device_id: "device_a".into(),
            lease_id: "media_a".into(),
            lease_jti: "lease_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            fence: 9,
            reason: "peer_connection_failed".into(),
        })
        .expect("fixture encodes");
        let foreign_acceptance = serde_json::to_string(&PluginOfferAcceptedFrame {
            kind: "plugin_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_other".into(),
            device_id: "device_a".into(),
            request_id: "request_a".into(),
            offer_id: "offer_in_app".into(),
            offer_jti: "offer_jti_in_app".into(),
            offered_mode: LeaseMode::Monitor,
            accepted: true,
        })
        .expect("fixture encodes");
        let foreign_rtc = plugin_rtc_frame("app_other");

        for encoded in [
            foreign_status,
            foreign_rejection,
            foreign_revoke,
            foreign_acceptance,
            foreign_rtc,
        ] {
            let mut shim = shim_granting(vec![Grant::StateRead, Grant::RtcSignal, Grant::Monitor]);
            assert!(
                shim.translate(&encoded).is_err(),
                "a frame bound to another app must be refused: {encoded}"
            );
        }
    }

    /// Everything the operator's takeover press actually walks, end to end:
    /// the Desktop publishes a signed offer inside an authoritative snapshot,
    /// the shim projects it, the session stores it, and `select_mobile_offer`
    /// finds it.
    ///
    /// ⚠️ THIS IS THE REGRESSION TEST FOR THE REPORTED BUG. Every link existed
    /// already except the projection, which discarded the offers — so the
    /// selector had nothing to match, every press failed locally with "no
    /// current signed mobile offer permits this lease", and the relay never saw
    /// a single frame. Driving the real snapshot through the real shim into the
    /// real selector is the only arrangement that would have caught it; a test
    /// that hand-built the projected snapshot passes with the bug present.
    struct PreparedRelayTakeover {
        shim: GatewayShim,
        state: V2State,
        answer: MobileOfferAnswerFrame,
        lease_request_id: String,
    }

    async fn prepare_relay_takeover_after(
        offers: Vec<SignedPendingMobileOffer>,
    ) -> Result<PreparedRelayTakeover, String> {
        let identity = crate::endpoint_identity::EndpointIdentity::from_secret([9; 32])
            .expect("test endpoint identity");
        let mut shim = shim_granting(vec![
            Grant::StateRead,
            Grant::RtcSignal,
            Grant::Takeover,
            Grant::ResumeAokie,
        ]);
        let encoded = serde_json::to_string(&PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            event_id: "event_offers".into(),
            device_id: Some("device_a".into()),
            snapshot: authoritative_snapshot_offering(LeaseMode::Takeover, offers),
        })
        .expect("fixture encodes");
        let translated = shim
            .translate(&encoded)
            .expect("the snapshot translates")
            .expect("a snapshot is a session frame");
        let snapshot: MobileSnapshotFrame =
            serde_json::from_str(&translated).expect("the translation is a mobile snapshot");

        let state = V2State::default();
        *state.inner.lock().await = ClientState {
            generation: 1,
            app_id: Some("app_a".into()),
            device_id: Some("device_a".into()),
            session_nonce: Some("session_a".into()),
            endpoint_identity: Some(identity),
            snapshot: Some(snapshot),
            ..ClientState::default()
        };

        let (answer, lease_request_id) =
            prepare_offer_answer(&state, LeaseMode::Takeover, None).await?;
        Ok(PreparedRelayTakeover {
            shim,
            state,
            answer,
            lease_request_id,
        })
    }

    async fn takeover_press_after(offers: Vec<SignedPendingMobileOffer>) -> Result<String, String> {
        let prepared = prepare_relay_takeover_after(offers).await?;
        assert!(
            prepared
                .state
                .inner
                .lock()
                .await
                .pending
                .as_ref()
                .is_some_and(|pending| pending.stage == PendingLeaseStage::AwaitingOfferAcceptance),
            "the press must leave a pending lease awaiting the authority's acceptance"
        );
        assert_eq!(prepared.answer.kind, "mobile_offer_answer");
        assert_eq!(prepared.answer.offered_mode, LeaseMode::Takeover);
        Ok(prepared.answer.offer_id)
    }

    #[tokio::test]
    async fn pressing_takeover_enqueues_a_mobile_offer_answer_when_the_snapshot_carries_a_matching_offer(
    ) {
        let identity = crate::endpoint_identity::EndpointIdentity::from_secret([9; 32])
            .expect("test endpoint identity");
        let offer_id = takeover_press_after(vec![
            offer_for(
                identity.thumbprint(),
                LeaseMode::Takeover,
                MobileOfferSurface::InApp,
                "offer_in_app",
                "offer_jti_in_app",
            ),
            // A second offer for a DIFFERENT surface must not make the in-app
            // press ambiguous: Android's system call UI is answered by its own
            // path, with its own offer.
            offer_for(
                identity.thumbprint(),
                LeaseMode::Takeover,
                MobileOfferSurface::VoiceSystemUi,
                "offer_voice_ui",
                "offer_jti_voice_ui",
            ),
        ])
        .await
        .expect("a published, matching in-app offer makes takeover pressable");
        assert_eq!(offer_id, "offer_in_app");
    }

    #[tokio::test]
    async fn old_plugin_snapshot_without_a_signed_offer_cannot_emit_a_claim() {
        let error = takeover_press_after(Vec::new())
            .await
            .expect_err("an old plugin publishes no offer to redeem");
        assert_eq!(error, "no current signed mobile offer permits this lease");
    }

    /// The second half of the reported no-claim regression: once the plugin
    /// accepts the answer, its relay-dialect acknowledgement must release the
    /// exact prebuilt `lease_request` into the session's outbound loop.
    ///
    /// This deliberately starts with a plugin snapshot and routes the
    /// acknowledgement back through the real shim. Hand-building either mobile
    /// frame would miss a dialect drift between the two endpoints.
    #[tokio::test]
    async fn translated_offer_acceptance_releases_the_exact_in_app_lease_request() {
        let identity = crate::endpoint_identity::EndpointIdentity::from_secret([9; 32])
            .expect("test endpoint identity");
        let mut prepared = prepare_relay_takeover_after(vec![offer_for(
            identity.thumbprint(),
            LeaseMode::Takeover,
            MobileOfferSurface::InApp,
            "offer_in_app",
            "offer_jti_in_app",
        )])
        .await
        .expect("a matching relay offer prepares an answer");

        assert!(
            take_ready_lease_request(&prepared.state)
                .await
                .expect("pending state is valid")
                .is_none(),
            "the lease request must wait for the authority's offer acknowledgement"
        );
        assert!(relay_send_enabled_from(None));
        assert!(relay_plugin_admits(&prepared.answer.kind));

        let plugin_acceptance = serde_json::to_string(&PluginOfferAcceptedFrame {
            kind: "plugin_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            request_id: prepared.answer.request_id.clone(),
            offer_id: prepared.answer.offer_id.clone(),
            offer_jti: prepared.answer.offer_jti.clone(),
            offered_mode: prepared.answer.offered_mode,
            accepted: true,
        })
        .expect("plugin acceptance encodes");
        let translated = prepared
            .shim
            .translate(&plugin_acceptance)
            .expect("plugin acceptance translates")
            .expect("offer acceptance is a mobile session frame");
        let accepted: MobileOfferAcceptedFrame =
            serde_json::from_str(&translated).expect("translation has the mobile shape");
        apply_mobile_offer_accepted(&prepared.state, "app_a", accepted.clone(), &translated)
            .await
            .expect("the exact offer fence is accepted");
        apply_mobile_offer_accepted(&prepared.state, "app_a", accepted.clone(), &translated)
            .await
            .expect("an exact at-least-once acceptance is a no-op");
        let mut changed_acceptance = accepted;
        changed_acceptance.offer_jti = "offer_jti_changed".into();
        let changed_encoded = serde_json::to_string(&changed_acceptance).unwrap();
        assert!(apply_mobile_offer_accepted(
            &prepared.state,
            "app_a",
            changed_acceptance,
            &changed_encoded,
        )
        .await
        .is_err());

        let outbound = take_ready_lease_request(&prepared.state)
            .await
            .expect("accepted pending state is valid")
            .expect("acceptance releases the lease request");
        let request: LeaseRequestFrame =
            serde_json::from_str(&outbound.encoded).expect("lease request has its wire shape");
        assert_eq!(request.kind, "lease_request");
        assert_eq!(request.request_id, prepared.lease_request_id);
        assert_eq!(request.mode, LeaseMode::Takeover);
        assert_eq!(request.accepted_offer_id, "offer_in_app");
        assert_eq!(request.accepted_offer_jti, "offer_jti_in_app");
        assert!(
            outbound.answer_action_id.is_none(),
            "an in-app press is not an Android Core-Telecom action"
        );
        assert!(
            take_ready_lease_request(&prepared.state)
                .await
                .expect("post-delivery pending state is valid")
                .is_none(),
            "the same acceptance cannot release the request twice"
        );
    }

    /// The authority publishes exactly one offer per (device, mode, surface).
    /// If two ever matched, the selector could not know which authorisation the
    /// operator was redeeming — so it refuses rather than picking one, and this
    /// locks that contract from the consuming side.
    #[tokio::test]
    async fn two_matching_offers_for_the_same_mode_are_refused() {
        let identity = crate::endpoint_identity::EndpointIdentity::from_secret([9; 32])
            .expect("test endpoint identity");
        let error = takeover_press_after(vec![
            offer_for(
                identity.thumbprint(),
                LeaseMode::Takeover,
                MobileOfferSurface::InApp,
                "offer_in_app",
                "offer_jti_in_app",
            ),
            offer_for(
                identity.thumbprint(),
                LeaseMode::Takeover,
                MobileOfferSurface::InApp,
                "offer_in_app_duplicate",
                "offer_jti_in_app_duplicate",
            ),
        ])
        .await
        .expect_err("two matching offers must not silently resolve to one");
        assert_eq!(
            error,
            "multiple signed mobile offers require an explicit offer selection"
        );
    }

    /// A normal relaunch has no inherited environment override. It must still
    /// introduce itself and redeem a signed offer; only an explicit operator
    /// rollback disables relay writes.
    #[test]
    fn relay_sending_defaults_on_after_relaunch_with_an_exact_zero_kill_switch() {
        for enabled in [None, Some(""), Some("1"), Some("true"), Some("yes")] {
            assert!(
                relay_send_enabled_from(enabled),
                "{enabled:?} must leave relay writes enabled"
            );
        }
        assert!(!relay_send_enabled_from(Some("0")));
        assert!(relay_plugin_admits("mobile_hello"));
    }

    /// Every mobile kind that can cross the relay boundary has a matching arm in
    /// the plugin's relay-only dispatcher.
    ///
    /// This is stronger than documenting the current set: `send_text` consults
    /// the same predicate immediately before the carrier POST, so a future
    /// outbound kind is held by default until both ends explicitly add it.
    #[test]
    fn only_plugin_admitted_mobile_kinds_can_cross_the_relay_boundary() {
        // Verbatim arms in GatewaySession::handle_relay_peer_frame, plus the
        // independently handled authenticated hello.
        const PLUGIN_RELAY_ACCEPTS: [&str; 10] = [
            "mobile_hello",
            "mobile_offer_answer",
            "lease_request",
            "rtc_signal",
            "lease_heartbeat",
            "lease_revoke",
            "microphone_mute",
            "assistance_answer",
            "end_caller_challenge_request",
            "end_caller_confirm",
        ];
        assert_eq!(RELAY_PLUGIN_ADMITTED_KINDS, PLUGIN_RELAY_ACCEPTS);
        for kind in PLUGIN_RELAY_ACCEPTS {
            assert!(
                relay_plugin_admits(kind),
                "{kind} must reach its plugin arm"
            );
        }

        for held in [
            // Gateway-dialect authority notices are never mobile requests.
            "claim_proposal",
            "lease_granted",
            "lease_revoked",
            "something_new",
        ] {
            assert!(
                !relay_plugin_admits(held),
                "{held} has no relay-mobile plugin arm and must be held"
            );
        }
    }

    fn signed_plugin_hello(
        desktop: &crate::endpoint_identity::EndpointIdentity,
        app_id: &str,
    ) -> String {
        let now = unix_now().expect("clock");
        let approved = vec!["mobile_key_thumbprint_1".to_string()];
        let revision = 1;
        let proof = desktop
            .sign_hello(HelloProofClaims {
                app_id: app_id.into(),
                subject_id: "aokie".into(),
                role: AdmissionRole::Plugin,
                connection_id: "connection_p".into(),
                challenge_nonce: "challenge_p".into(),
                admission_jti: "admission_p".into(),
                session_nonce: "session_p".into(),
                holder_key_thumbprint: desktop.thumbprint().into(),
                expected_peer_key_thumbprint: None,
                approved_peer_key_thumbprints: approved.clone(),
                peer_roster_revision: Some(revision),
                peer_roster_hash: Some(aokie_protocol::v2::peer_roster_hash(revision, &approved)),
                nonce: "proof_nonce_p".into(),
                jti: "proof_jti_p".into(),
                issued_at: now,
                expires_at: now + 20,
            })
            .expect("the plugin hello proof signs");
        serde_json::to_string(&PluginHello {
            kind: "plugin_hello".into(),
            schema_version: SCHEMA_VERSION,
            app_id: app_id.into(),
            plugin_id: "aokie".into(),
            session_nonce: "session_p".into(),
            endpoint_proof: proof,
        })
        .expect("fixture encodes")
    }

    /// Over the socket the gateway vouched for the plugin's identity. On the
    /// relay nothing does, so this hello is the Companion's OWN proof that the
    /// state it is about to project came from the Desktop its admission pinned
    /// and its user confirmed — not merely from something holding a plugin-role
    /// admission for this app.
    #[test]
    fn a_plugin_hello_must_prove_the_admission_pinned_desktop_key() {
        let desktop = crate::endpoint_identity::EndpointIdentity::from_secret([9; 32])
            .expect("test identity");
        let impostor = crate::endpoint_identity::EndpointIdentity::from_secret([11; 32])
            .expect("test identity");
        let shim_for = |app_id: &str| {
            GatewayShim::new(
                app_id.into(),
                "device_a".into(),
                vec![Grant::StateRead],
                desktop.thumbprint().into(),
            )
        };

        // The real peer: consumed rather than forwarded, and it unlocks state.
        let mut shim = shim_for("app_proof_ok");
        assert!(!shim.peer_verified);
        assert_eq!(
            shim.translate(&signed_plugin_hello(&desktop, "app_proof_ok")),
            Ok(None)
        );
        assert!(shim.peer_verified);

        // A different endpoint key, however well signed, is not our Desktop.
        let mut shim = shim_for("app_proof_impostor");
        assert!(shim
            .translate(&signed_plugin_hello(&impostor, "app_proof_impostor"))
            .is_err());
        assert!(!shim.peer_verified);

        // Nor is our Desktop speaking for another app.
        let mut shim = shim_for("app_proof_crossed");
        assert!(shim
            .translate(&signed_plugin_hello(&desktop, "app_proof_other"))
            .is_err());
        assert!(!shim.peer_verified);

        // A hello whose signature does not cover its claims is refused before
        // any claim is read.
        let mut tampered: Value =
            serde_json::from_str(&signed_plugin_hello(&desktop, "app_proof_tampered_src")).unwrap();
        tampered["endpointProof"]["claims"]["appId"] = serde_json::json!("app_proof_tampered");
        let mut shim = shim_for("app_proof_tampered");
        assert!(shim.translate(&tampered.to_string()).is_err());
        assert!(!shim.peer_verified);
    }

    /// The relay keeps frames longer than the endpoint proof window. During a
    /// managed-admission overlap the plugin can therefore re-send a cached
    /// hello whose otherwise-correct 30-second proof expired 40 seconds ago.
    /// That stale carrier frame must not fail the predecessor (which would
    /// close a live Talk peer), and it must not authorize the unverified
    /// replacement.
    #[test]
    fn expired_plugin_regreeting_is_non_authorizing_carrier_traffic() {
        let desktop = crate::endpoint_identity::EndpointIdentity::from_secret([23; 32])
            .expect("test identity");
        let shim_for = || {
            GatewayShim::new(
                "app_overlap".into(),
                "device_a".into(),
                vec![Grant::StateRead],
                desktop.thumbprint().into(),
            )
        };
        let now = unix_now().expect("clock");
        let mut stale: PluginHello =
            serde_json::from_str(&signed_plugin_hello(&desktop, "app_overlap"))
                .expect("fixture decodes");
        stale.endpoint_proof.claims.issued_at = now - 70;
        stale.endpoint_proof.claims.expires_at = now - 40;
        // `EndpointIdentity::sign_hello` intentionally refuses to mint an
        // already-expired proof, so reproduce the historically valid cached
        // wire frame with the test's known key after moving its clock window.
        use base64::Engine as _;
        use ed25519_dalek::Signer as _;
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[23; 32]);
        stale.endpoint_proof.signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            signing_key
                .sign(
                    &stale
                        .endpoint_proof
                        .claims
                        .signing_bytes()
                        .expect("claims canonicalize"),
                )
                .to_bytes(),
        );
        let stale_frame = stale.clone();
        let stale = serde_json::to_string(&stale).expect("stale fixture encodes");

        let mut predecessor = shim_for();
        predecessor
            .translate(&signed_plugin_hello(&desktop, "app_overlap"))
            .expect("predecessor proves the pinned Desktop key");
        let sequence_before = predecessor.sequence;
        assert_eq!(predecessor.translate(&stale), Ok(None));
        assert!(
            predecessor.peer_verified,
            "a redundant stale hello cannot de-authenticate live proven transport"
        );
        assert_eq!(predecessor.sequence, sequence_before);

        let mut replacement = shim_for();
        assert_eq!(replacement.translate(&stale), Ok(None));
        assert!(
            !replacement.peer_verified,
            "an expired proof never grants replacement authority"
        );
        assert_eq!(
            replacement.translate(&plugin_snapshot_frame_for("app_overlap")),
            Ok(None),
            "unverified replacement state remains fail-closed"
        );
        assert_eq!(replacement.sequence, 0);

        let mut forged = stale_frame.clone();
        forged.endpoint_proof.signature =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0_u8; 64]);
        let forged = serde_json::to_string(&forged).expect("forged fixture encodes");
        let mut fail_closed = shim_for();
        assert!(
            fail_closed.translate(&forged).is_err(),
            "expiration cannot hide a forged Desktop signature"
        );
        assert!(!fail_closed.peer_verified);

        let mut future = stale_frame;
        future.endpoint_proof.claims.issued_at = now + 70;
        future.endpoint_proof.claims.expires_at = now + 90;
        future.endpoint_proof.signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            signing_key
                .sign(
                    &future
                        .endpoint_proof
                        .claims
                        .signing_bytes()
                        .expect("future claims canonicalize"),
                )
                .to_bytes(),
        );
        let future = serde_json::to_string(&future).expect("future fixture encodes");
        let mut temporal_fail_closed = shim_for();
        assert!(
            temporal_fail_closed.translate(&future).is_err(),
            "a future-issued proof is not stale relay mail"
        );
        assert!(!temporal_fail_closed.peer_verified);
    }

    /// Relay mail can survive a plugin restart. Process-global proof would let
    /// a brand-new session consume that queued authority without its own signed
    /// hello merely because the long-term key string matched.
    #[test]
    fn a_new_shim_never_inherits_another_sessions_peer_proof() {
        let desktop = crate::endpoint_identity::EndpointIdentity::from_secret([21; 32])
            .expect("test identity");
        let shim_for = || {
            GatewayShim::new(
                "app_reconnect".into(),
                "device_a".into(),
                vec![Grant::StateRead],
                desktop.thumbprint().into(),
            )
        };

        let mut first = shim_for();
        assert!(!first.peer_verified, "nothing is trusted before a proof");
        first
            .translate(&signed_plugin_hello(&desktop, "app_reconnect"))
            .expect("the real Desktop proves itself");

        // A brand-new carrier with the same app and long-term key is still a
        // new proof boundary.
        let mut reconnected = shim_for();
        assert!(!reconnected.peer_verified);
        assert_eq!(
            reconnected.translate(&plugin_snapshot_frame_for("app_reconnect")),
            Ok(None),
            "queued state must wait for this session's signed hello",
        );
        reconnected
            .translate(&signed_plugin_hello(&desktop, "app_reconnect"))
            .expect("the new session proves the same pinned key afresh");
        assert!(reconnected.peer_verified);

        // The memory is bound to app AND key: it never vouches for anyone else.
        let other_app = GatewayShim::new(
            "app_reconnect_other".into(),
            "device_a".into(),
            vec![Grant::StateRead],
            desktop.thumbprint().into(),
        );
        assert!(!other_app.peer_verified);
        let other_key = GatewayShim::new(
            "app_reconnect".into(),
            "device_a".into(),
            vec![Grant::StateRead],
            "some_other_desktop_thumbprint".into(),
        );
        assert!(!other_key.peer_verified);
    }

    /// `Vec<Caption>` → `Option<Vec<Caption>>` is a change of MEANING, not just
    /// of shape: the projected `Option` says whether captions are exposed to
    /// this device, and `validate_snapshot` rejects the entire snapshot when a
    /// `Some` outruns consent. Wrapping the field unconditionally would
    /// therefore have discarded every snapshot on a line without caption
    /// consent — which is the default.
    #[test]
    fn captions_are_exposed_only_when_consent_and_grants_allow_it() {
        let mut consented = authoritative_snapshot();
        consented.captions = vec![aokie_protocol::v2::Caption {
            caption_id: "caption_1".into(),
            speaker: "caller".into(),
            text: "hello".into(),
            occurred_at: Utc::now().to_rfc3339(),
            final_text: true,
        }];
        consented.remote_consent.enabled = true;
        consented.remote_consent.acknowledged = true;
        consented.remote_consent.acknowledged_at = Some(Utc::now().to_rfc3339());
        consented.remote_consent.captions_enabled = true;

        let encode = |snapshot: &aokie_protocol::v2::AuthoritativeCallSnapshot| {
            serde_json::to_string(&PluginSnapshotFrame {
                kind: "plugin_snapshot".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                event_id: "event_1".into(),
                device_id: Some("device_a".into()),
                snapshot: snapshot.clone(),
            })
            .expect("fixture encodes")
        };
        let projected = |shim: &mut GatewayShim, snapshot| {
            let translated = shim
                .translate(&encode(snapshot))
                .expect("translates")
                .expect("is a session frame");
            let frame: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();
            validate_snapshot(&frame, "app_a").expect("the session accepts the translation");
            frame.snapshot.captions
        };

        // Consent given AND the grant held: the captions come through.
        let mut with_grant = GatewayShim::new(
            "app_a".into(),
            "device_a".into(),
            vec![Grant::StateRead, Grant::CaptionsRead],
            "desktop_key_thumbprint_1".into(),
        );
        with_grant.peer_verified = true;
        assert_eq!(
            projected(&mut with_grant, &consented).map(|captions| captions.len()),
            Some(1)
        );

        // Consent given but the grant withheld: nothing is exposed, and the
        // rest of the snapshot still arrives.
        let mut without_grant = verified_shim();
        assert_eq!(projected(&mut without_grant, &consented), None);

        // The default line has no caption consent at all.
        let unconsented = authoritative_snapshot();
        assert_eq!(projected(&mut with_grant, &unconsented), None);
    }

    #[test]
    fn participants_and_levels_fail_closed_when_consent_expires_or_is_malformed() {
        let mut source = authoritative_snapshot();
        source.remote_consent.enabled = true;
        source.remote_consent.acknowledged = true;
        source.remote_consent.acknowledged_at = Some(Utc::now().to_rfc3339());
        source.remote_consent.expires_at =
            Some((Utc::now() + chrono::Duration::seconds(60)).to_rfc3339());
        source.participants = vec![aokie_protocol::v2::ParticipantPresence {
            participant_id: "participant_owner".into(),
            mode: aokie_protocol::v2::ParticipantMode::Talker,
            state: aokie_protocol::v2::ParticipantState::Active,
            subject_id: Some("owner_a".into()),
            display_label: Some("Owner".into()),
        }];
        source.audio_levels = Some(vec![aokie_protocol::v2::NormalizedAudioLevel {
            source: AudioLevelSource::Companion,
            participant_id: Some("participant_owner".into()),
            level_permille: 420,
        }]);
        let mut shim = GatewayShim::new(
            "app_a".into(),
            "device_a".into(),
            vec![
                Grant::StateRead,
                Grant::ParticipantsRead,
                Grant::ParticipantIdentityRead,
                Grant::AudioLevelsRead,
            ],
            "desktop_key_thumbprint_1".into(),
        );
        shim.peer_verified = true;
        let project =
            |shim: &mut GatewayShim, snapshot: &aokie_protocol::v2::AuthoritativeCallSnapshot| {
                let encoded = serde_json::to_string(&PluginSnapshotFrame {
                    kind: "plugin_snapshot".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    event_id: "privacy_event".into(),
                    device_id: Some("device_a".into()),
                    snapshot: snapshot.clone(),
                })
                .unwrap();
                let translated = shim.translate(&encoded).unwrap().unwrap();
                serde_json::from_str::<MobileSnapshotFrame>(&translated).unwrap()
            };

        let current = project(&mut shim, &source);
        assert_eq!(current.snapshot.participants.len(), 1);
        assert_eq!(current.snapshot.audio_levels.as_ref().unwrap().len(), 1);
        validate_snapshot(&current, "app_a").unwrap();

        let mut expired = source.clone();
        expired.remote_consent.expires_at =
            Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        let expired_projection = project(&mut shim, &expired);
        assert!(expired_projection.snapshot.participants.is_empty());
        assert!(expired_projection.snapshot.audio_levels.is_none());

        let mut malformed = source;
        malformed.remote_consent.expires_at = Some("not-an-rfc3339-time".into());
        let malformed_projection = project(&mut shim, &malformed);
        assert!(malformed_projection.snapshot.participants.is_empty());
        assert!(malformed_projection.snapshot.audio_levels.is_none());

        let mut leaked_expired = current.clone();
        leaked_expired.snapshot.remote_consent.expires_at =
            Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        assert!(validate_snapshot(&leaked_expired, "app_a").is_err());
        let mut leaked_malformed = current;
        leaked_malformed.snapshot.remote_consent.expires_at = Some("bad-time".into());
        assert!(validate_snapshot(&leaked_malformed, "app_a").is_err());
    }

    /// ⚠️ The plugin's frames carry an `eventId`, never a sequence — the gateway
    /// minted the monotonic counter the session tracks state on. A shim that
    /// merely renamed fields would fail `validate_snapshot` on every frame.
    #[test]
    fn the_shim_mints_the_monotonic_sequence_the_gateway_used_to_supply() {
        let mut shim = verified_shim();

        let mut sequences = Vec::new();
        for _ in 0..3 {
            let translated = shim
                .translate(&plugin_snapshot_frame())
                .expect("translates")
                .expect("is a session frame");
            let frame: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();
            sequences.push(frame.sequence);
        }

        // Starts at 1 (zero fails validation) and never repeats, or
        // `is_new_authoritative_sequence` would discard live state as stale.
        assert_eq!(sequences, vec![1, 2, 3]);

        // A rotation must not restart it, for the same reason.
        let mut replacement = verified_shim();
        replacement.adopt_sequence_from(&shim);
        let translated = replacement
            .translate(&plugin_snapshot_frame())
            .expect("translates")
            .expect("is a session frame");
        let frame: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();
        assert_eq!(frame.sequence, 4);
    }

    /// ⚠️ A rotation that CHANGES carrier is the case a relay-to-relay handover
    /// does not cover.
    ///
    /// `authoritative_sequence` lives in [`ClientState`] and survives an
    /// admission rotation — only `reset`/`begin` clear it, and neither runs on
    /// the rotation path. So a WebSocket session that rotates onto the relay
    /// hands the shim a session already sitting at the gateway's mark, and a
    /// shim that started at zero would mint 1, 2, 3 … which
    /// `is_new_authoritative_sequence` discards. Silently: `handle_gateway_frame`
    /// returns `Ok(())` on a stale sequence rather than erroring, so
    /// `initial_sync` still succeeds and the session reads Connected while its
    /// state never moves again. Deploying the backend's relay advertisement
    /// under a live Companion is exactly how that happens.
    #[test]
    fn a_carrier_change_mints_above_the_sequence_the_previous_carrier_reached() {
        let gateway_high_water = 4_812;
        let mut shim = verified_shim();
        shim.seed_sequence(gateway_high_water);

        let translated = shim
            .translate(&plugin_snapshot_frame())
            .expect("translates")
            .expect("is a session frame");
        let frame: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();

        assert!(
            is_new_authoritative_sequence(
                &ClientState {
                    authoritative_sequence: gateway_high_water,
                    ..ClientState::default()
                },
                frame.sequence,
            ),
            "the session must accept the first frame after a carrier change",
        );
        assert_eq!(frame.sequence, gateway_high_water + 1);

        // Seeding never drags a counter BACKWARDS: a relay session that has
        // already minted past the mark keeps its own position.
        let mut ahead = verified_shim();
        ahead.sequence = gateway_high_water + 10;
        ahead.seed_sequence(gateway_high_water);
        assert_eq!(ahead.sequence, gateway_high_water + 10);
    }

    /// Explicit live rotation may carry proof for the exact same pin, but doing
    /// so across a re-pin would let the old Desktop vouch for the new key.
    #[test]
    fn a_rotation_that_repins_the_desktop_key_must_see_it_prove_itself_again() {
        let shim_with_pin = |pin: &str| {
            GatewayShim::new(
                "app_rotation_pin".into(),
                "device_a".into(),
                vec![Grant::StateRead],
                pin.into(),
            )
        };

        let mut proven = shim_with_pin("desktop_key_before");
        proven.peer_verified = true;

        // Same pin: proof carries, or a quiet line would deadlock its reads
        // waiting for a greeting the plugin has already sent.
        let mut same_pin = shim_with_pin("desktop_key_before");
        same_pin.adopt_sequence_from(&proven);
        assert!(same_pin.peer_verified);

        // Re-pinned: the replacement starts unproven, and drops authoritative
        // state until the new key signs a hello.
        let mut repinned = shim_with_pin("desktop_key_after");
        repinned.adopt_sequence_from(&proven);
        assert!(
            !repinned.peer_verified,
            "proof for one Desktop key must not vouch for another",
        );
        assert_eq!(
            repinned.translate(&plugin_snapshot_frame()),
            Ok(None),
            "state from an unproven re-pinned peer is dropped",
        );
        // The cursor still carries, so the replacement does not regress.
        assert_eq!(repinned.sequence, proven.sequence);
    }

    #[test]
    fn a_plugin_idle_translates_into_a_valid_idle_sync() {
        let mut shim = verified_shim();
        let encoded = serde_json::to_string(&PluginIdleFrame {
            kind: "plugin_idle".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            event_id: "event_2".into(),
        })
        .expect("fixture encodes");

        let translated = shim
            .translate(&encoded)
            .expect("a well-formed plugin idle translates")
            .expect("an idle frame is a session frame");

        let frame: MobileIdleSyncFrame =
            serde_json::from_str(&translated).expect("the translation is an idle sync");
        frame
            .validate()
            .expect("the translation passes the protocol's own validator");
        validate_idle_sync(&frame, "app_a").expect("and the session's own gate");
        assert_eq!(frame.sequence, 1);
    }

    /// Inbound tolerance is the whole point: the plugin's dispatcher errors on
    /// an unknown kind, and a shim that copied that would churn the session on
    /// every frame outside the translated subset.
    ///
    /// ⚠️ `plugin_lease_revoke` used to be one of these examples and is now
    /// TRANSLATED — see `plugin_lease_revoke_translates_to_lease_revoked...`.
    /// Anything listed here must be a kind the shim genuinely has no mobile
    /// equivalent for, or the case proves nothing.
    #[test]
    fn frames_outside_the_translated_subset_are_dropped_rather_than_erroring() {
        let mut shim = verified_shim();

        for untranslated in [
            "{\"kind\":\"claim_decision\"}",
            "{\"kind\":\"something_this_build_has_never_heard_of\"}",
        ] {
            assert_eq!(
                shim.translate(untranslated),
                Ok(None),
                "{untranslated} must drop quietly"
            );
        }
        // Dropping cost nothing: the next real frame still gets sequence 1.
        let translated = shim
            .translate(&plugin_snapshot_frame())
            .expect("translates")
            .expect("is a session frame");
        let frame: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();
        assert_eq!(frame.sequence, 1);
    }

    /// Over the socket the gateway vouched for the plugin. On the relay nothing
    /// does, so authoritative state from an unproven sender is never projected.
    #[test]
    fn authoritative_state_is_not_projected_until_the_desktop_peer_proves_itself() {
        let mut shim = GatewayShim::new(
            "app_a".into(),
            "device_a".into(),
            vec![Grant::StateRead],
            "desktop_key_thumbprint_1".into(),
        );

        // Dropped, not refused: the plugin re-greets after a rotation, so an
        // ordering gap has to be able to heal instead of looping the session.
        assert_eq!(shim.translate(&plugin_snapshot_frame()), Ok(None));

        shim.peer_verified = true;
        assert!(shim
            .translate(&plugin_snapshot_frame())
            .expect("translates once proven")
            .is_some());
    }

    #[test]
    fn a_snapshot_bound_to_another_app_is_refused() {
        let mut shim = verified_shim();
        let encoded = serde_json::to_string(&PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_b".into(),
            event_id: "event_1".into(),
            device_id: Some("device_a".into()),
            snapshot: authoritative_snapshot(),
        })
        .expect("fixture encodes");

        assert!(shim.translate(&encoded).is_err());
    }

    /// A duplicate grant fails `validate_snapshot` on EVERY frame, so it must
    /// not be able to brick the relay path.
    #[test]
    fn duplicate_admission_grants_cannot_brick_every_translated_frame() {
        let mut shim = GatewayShim::new(
            "app_a".into(),
            "device_a".into(),
            vec![Grant::StateRead, Grant::StateRead, Grant::Monitor],
            "desktop_key_thumbprint_1".into(),
        );
        shim.peer_verified = true;

        let translated = shim
            .translate(&plugin_snapshot_frame())
            .expect("translates")
            .expect("is a session frame");
        let frame: MobileSnapshotFrame = serde_json::from_str(&translated).unwrap();

        assert_eq!(frame.grants, vec![Grant::StateRead, Grant::Monitor]);
        validate_snapshot(&frame, "app_a").expect("deduped grants pass the session's gate");
    }

    #[test]
    fn snapshot_validation_is_app_and_sequence_bound() {
        let frame = MobileSnapshotFrame {
            kind: "snapshot".into(),
            schema_version: 2,
            app_id: "app_a".into(),
            sequence: 1,
            grants: vec![Grant::StateRead],
            snapshot: ProjectedCallSnapshot {
                call_id: "call_a".into(),
                call_epoch: 1,
                owner_epoch: 0,
                switchboard_revision: 1,
                remote_revision: 1,
                telephony_state: TelephonyState::Active,
                service_mode: ServiceMode::AokieActive,
                media_state: MediaState::Ready,
                remote_capabilities: RemoteCapabilities {
                    software_hold: false,
                    carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                    secondary_call_observation: SecondaryCallObservation::Unknown,
                    voice_consult: false,
                    takeover: false,
                },
                secondary_call_policy: SecondaryCallPolicy::Normal,
                secondary_call: None,
                remote_consent: aokie_protocol::v2::RemoteConsentPolicy {
                    policy_id: "aokie_remote_access".into(),
                    policy_version: 3,
                    enabled: false,
                    acknowledged: false,
                    acknowledged_at: None,
                    expires_at: None,
                    captions_enabled: false,
                    assistance_enabled: false,
                    monitor_enabled: false,
                    consult_enabled: false,
                    takeover_enabled: false,
                },
                caller: None,
                captions: None,
                participants: Vec::new(),
                audio_levels: None,
                companion_microphone_muted: false,
                pending_mobile_offers: Vec::new(),
                occurred_at: Utc::now().to_rfc3339(),
            },
        };
        assert!(validate_snapshot(&frame, "app_a").is_ok());
        assert!(validate_snapshot(&frame, "app_b").is_err());
    }

    fn end_caller_client() -> ClientState {
        let takeover = claims(LeaseMode::Takeover, LeasePhase::Active);
        ClientState {
            generation: 1,
            app_id: Some("app_a".into()),
            device_id: Some("device_a".into()),
            session_nonce: Some("session_a".into()),
            snapshot: Some(MobileSnapshotFrame {
                kind: "snapshot".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                sequence: 4,
                grants: vec![Grant::StateRead, Grant::Takeover, Grant::EndCaller],
                snapshot: ProjectedCallSnapshot {
                    call_id: takeover.call_id.clone(),
                    call_epoch: takeover.call_epoch,
                    owner_epoch: takeover.owner_epoch,
                    switchboard_revision: 11,
                    remote_revision: 13,
                    telephony_state: TelephonyState::Active,
                    service_mode: ServiceMode::HumanActive,
                    media_state: MediaState::Active,
                    remote_capabilities: RemoteCapabilities {
                        software_hold: true,
                        carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                        secondary_call_observation: SecondaryCallObservation::Unknown,
                        voice_consult: false,
                        takeover: true,
                    },
                    secondary_call_policy: SecondaryCallPolicy::Normal,
                    secondary_call: None,
                    remote_consent: aokie_protocol::v2::RemoteConsentPolicy {
                        policy_id: "aokie_remote_access".into(),
                        policy_version: 3,
                        enabled: true,
                        acknowledged: true,
                        acknowledged_at: Some("2026-07-16T00:00:00Z".into()),
                        expires_at: None,
                        captions_enabled: false,
                        assistance_enabled: false,
                        monitor_enabled: false,
                        consult_enabled: false,
                        takeover_enabled: true,
                    },
                    caller: None,
                    captions: None,
                    participants: Vec::new(),
                    audio_levels: None,
                    companion_microphone_muted: false,
                    pending_mobile_offers: Vec::new(),
                    occurred_at: Utc::now().to_rfc3339(),
                },
            }),
            lease: Some(ClientLease {
                request_id: "takeover_request".into(),
                token: "v2.redacted.signature".into(),
                session: session_from_claims(&takeover, 2, 2).unwrap(),
                claims: takeover,
            }),
            ..ClientState::default()
        }
    }

    fn offered_client(mode: LeaseMode, surface: MobileOfferSurface) -> ClientState {
        let now = unix_now().unwrap();
        let endpoint_identity = crate::endpoint_identity::EndpointIdentity::from_secret([9; 32])
            .expect("test endpoint identity");
        let mode_grant = grant_for_requested_mode(mode).unwrap();
        let mut admission_grants = vec![Grant::StateRead, Grant::RtcSignal, mode_grant];
        if mode == LeaseMode::Takeover {
            admission_grants.push(Grant::ResumeAokie);
        }
        let offer = SignedPendingMobileOffer {
            offer: aokie_protocol::v2::PendingMobileOfferClaims {
                offer_id: "offer_exact_a".into(),
                opportunity_id: "opportunity_a".into(),
                target_device_id: "device_a".into(),
                target_holder_key_thumbprint: endpoint_identity.thumbprint().into(),
                offered_mode: mode,
                surface,
                app_id: "app_a".into(),
                call_id: "call_a".into(),
                call_epoch: 7,
                owner_epoch: 4,
                switchboard_revision: 11,
                remote_revision: 13,
                accepted_transfer_request_id: None,
                required_consent_policy_id: "aokie_remote_access".into(),
                required_consent_policy_version: 3,
                required_grants: vec![Grant::StateRead, Grant::RtcSignal, mode_grant],
                issued_at: now,
                expires_at: now + 20,
                jti: "offer_jti_exact_a".into(),
            },
            offer_token: "signed.offer.token".into(),
        };
        let snapshot = MobileSnapshotFrame {
            kind: "snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            sequence: 8,
            grants: admission_grants,
            snapshot: ProjectedCallSnapshot {
                call_id: "call_a".into(),
                call_epoch: 7,
                owner_epoch: 4,
                switchboard_revision: 11,
                remote_revision: 13,
                telephony_state: TelephonyState::Active,
                service_mode: ServiceMode::AokieActive,
                media_state: MediaState::Ready,
                remote_capabilities: RemoteCapabilities {
                    software_hold: mode == LeaseMode::Consult,
                    carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                    secondary_call_observation: SecondaryCallObservation::Unknown,
                    voice_consult: mode == LeaseMode::Consult,
                    takeover: mode == LeaseMode::Takeover,
                },
                secondary_call_policy: SecondaryCallPolicy::Normal,
                secondary_call: None,
                remote_consent: aokie_protocol::v2::RemoteConsentPolicy {
                    policy_id: "aokie_remote_access".into(),
                    policy_version: 3,
                    enabled: true,
                    acknowledged: true,
                    acknowledged_at: Some("2026-07-16T00:00:00Z".into()),
                    expires_at: None,
                    captions_enabled: false,
                    assistance_enabled: true,
                    monitor_enabled: mode == LeaseMode::Monitor,
                    consult_enabled: mode == LeaseMode::Consult,
                    takeover_enabled: mode == LeaseMode::Takeover,
                },
                caller: None,
                captions: None,
                participants: Vec::new(),
                audio_levels: None,
                companion_microphone_muted: false,
                pending_mobile_offers: vec![offer],
                occurred_at: Utc::now().to_rfc3339(),
            },
        };
        let assistance = (mode == LeaseMode::Consult).then(|| PluginAssistanceRequestFrame {
            kind: "assistance_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            event_id: "assistance_event_a".into(),
            request_id: "assistance_request_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            switchboard_revision: 11,
            remote_revision: 13,
            question: "Can you advise?".into(),
            context: None,
            transfer_offered: false,
            expires_at: now + 20,
        });
        ClientState {
            generation: 1,
            app_id: Some("app_a".into()),
            device_id: Some("device_a".into()),
            session_nonce: Some("session_a".into()),
            endpoint_identity: Some(endpoint_identity),
            snapshot: Some(snapshot),
            assistance,
            ..ClientState::default()
        }
    }

    fn native_answer_action() -> NativeCallAction {
        NativeCallAction {
            schema_version: 2,
            action_id: "native_answer_a".into(),
            kind: NativeCallActionKind::Answer,
            offer_id: "offer_exact_a".into(),
            app_id: "app_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            accepted_transfer_request_id: None,
            response_text: None,
            created_at: unix_now().unwrap(),
        }
    }

    fn transfer_client(surface: MobileOfferSurface) -> ClientState {
        let mut client = offered_client(LeaseMode::Takeover, surface);
        let now = unix_now().unwrap();
        let snapshot = client.snapshot.as_mut().unwrap();
        snapshot.grants.push(Grant::AssistanceRead);
        snapshot.grants.push(Grant::AssistanceRespond);
        let offer = &mut snapshot.snapshot.pending_mobile_offers[0].offer;
        offer.accepted_transfer_request_id = Some("transfer_request_a".into());
        offer.required_grants.push(Grant::AssistanceRespond);
        client.assistance = Some(PluginAssistanceRequestFrame {
            kind: "assistance_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            event_id: "transfer_event_a".into(),
            request_id: "transfer_request_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            switchboard_revision: 11,
            remote_revision: 13,
            question: "Can you take this caller?".into(),
            context: None,
            transfer_offered: true,
            expires_at: now + 20,
        });
        client
    }

    fn native_transfer_action(kind: NativeCallActionKind) -> NativeCallAction {
        NativeCallAction {
            kind,
            accepted_transfer_request_id: Some("transfer_request_a".into()),
            response_text: (kind == NativeCallActionKind::Decline).then(|| "declined".into()),
            ..native_answer_action()
        }
    }

    #[tokio::test]
    async fn native_transfer_answer_requires_the_exact_action_bound_request() {
        let state = V2State::default();
        *state.inner.lock().await = transfer_client(MobileOfferSurface::VoiceSystemUi);
        let action = native_transfer_action(NativeCallActionKind::Answer);
        let (_, request_id) = prepare_offer_answer_for_transfer(
            &state,
            LeaseMode::Takeover,
            Some(&action),
            action.accepted_transfer_request_id.as_deref(),
        )
        .await
        .expect("exact transfer answer selects the voice-system offer");
        let client = state.inner.lock().await;
        assert_eq!(
            client
                .pending
                .as_ref()
                .unwrap()
                .lease_frame
                .accepted_transfer_request_id
                .as_deref(),
            Some("transfer_request_a")
        );
        assert_eq!(client.pending.as_ref().unwrap().request_id, request_id);
        assert_eq!(
            android_offer_reconciliation(&client, unix_now().unwrap()).unwrap(),
            AndroidOfferReconciliation::PreserveLocalClaim
        );

        let generic = V2State::default();
        *generic.inner.lock().await = transfer_client(MobileOfferSurface::VoiceSystemUi);
        assert!(
            prepare_offer_answer(&generic, LeaseMode::Takeover, Some(&native_answer_action()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_transfer_decline_is_private_exact_and_spends_its_offer() {
        let state = V2State::default();
        *state.inner.lock().await = transfer_client(MobileOfferSurface::VoiceSystemUi);
        let action = native_transfer_action(NativeCallActionKind::Decline);
        let request = AssistanceAnswerRequest {
            request_id: "transfer_request_a".into(),
            response_action: AssistanceResponseAction::Decline,
            answer: "declined".into(),
        };
        let (frame, answer_id) = prepare_assistance_answer(&state, &request, Some(&action))
            .await
            .expect("exact native decline becomes an authenticated assistance answer");
        assert_eq!(frame.response_action, AssistanceResponseAction::Decline);
        assert_eq!(frame.answer, "declined");
        let client = state.inner.lock().await;
        assert_eq!(
            client.pending_assistance_answer.as_deref(),
            Some(answer_id.as_str())
        );
        assert_eq!(client.spent_mobile_offers.len(), 1);
    }

    #[test]
    fn android_transfer_surface_requires_current_authenticated_assistance() {
        let now = unix_now().unwrap();
        let client = transfer_client(MobileOfferSurface::VoiceSystemUi);
        let presented = authoritative_android_offer(&client, now)
            .unwrap()
            .expect("current transfer publishes its exact native surface");
        assert_eq!(
            presented.accepted_transfer_request_id.as_deref(),
            Some("transfer_request_a")
        );

        let mut missing_assistance = client;
        missing_assistance.assistance = None;
        assert!(matches!(
            android_offer_reconciliation(&missing_assistance, now).unwrap(),
            AndroidOfferReconciliation::Cancel { .. }
        ));

        let mut other_device_won = transfer_client(MobileOfferSurface::VoiceSystemUi);
        other_device_won
            .snapshot
            .as_mut()
            .unwrap()
            .snapshot
            .pending_mobile_offers
            .clear();
        assert!(matches!(
            android_offer_reconciliation(&other_device_won, now).unwrap(),
            AndroidOfferReconciliation::Cancel { .. }
        ));

        let mut expired = transfer_client(MobileOfferSurface::VoiceSystemUi);
        expired
            .snapshot
            .as_mut()
            .unwrap()
            .snapshot
            .pending_mobile_offers[0]
            .offer
            .expires_at = now;
        assert!(matches!(
            android_offer_reconciliation(&expired, now).unwrap(),
            AndroidOfferReconciliation::Cancel { .. }
        ));

        let mut narrowed = transfer_client(MobileOfferSurface::VoiceSystemUi);
        narrowed
            .snapshot
            .as_mut()
            .unwrap()
            .grants
            .retain(|grant| *grant != Grant::Takeover);
        assert!(matches!(
            android_offer_reconciliation(&narrowed, now).unwrap(),
            AndroidOfferReconciliation::Cancel { .. }
        ));

        let mut consent_withdrawn = transfer_client(MobileOfferSurface::VoiceSystemUi);
        consent_withdrawn
            .snapshot
            .as_mut()
            .unwrap()
            .snapshot
            .remote_consent
            .takeover_enabled = false;
        assert!(matches!(
            android_offer_reconciliation(&consent_withdrawn, now).unwrap(),
            AndroidOfferReconciliation::Cancel { .. }
        ));

        let mut disconnected = transfer_client(MobileOfferSurface::VoiceSystemUi);
        disconnected.snapshot = None;
        assert_eq!(
            android_offer_reconciliation(&disconnected, now).unwrap(),
            AndroidOfferReconciliation::None
        );

        assert_eq!(
            android_offer_reconciliation(&end_caller_client(), now).unwrap(),
            AndroidOfferReconciliation::PreserveLocalClaim
        );
    }

    #[test]
    fn realtime_disconnect_cancels_a_merely_ringing_native_offer_before_reset() {
        let mut client = offered_client(LeaseMode::Takeover, MobileOfferSurface::VoiceSystemUi);

        let cancel = native_call_to_cancel_on_disconnect(&client)
            .expect("the authenticated ringing snapshot owns a native surface");
        assert_eq!(cancel, ("call_a".into(), 7));

        // The disconnect path captures this identity while state is still
        // authoritative, before reset_generation clears the snapshot.
        client.snapshot = None;
        assert_eq!(native_call_to_cancel_on_disconnect(&client), None);
        assert_eq!(cancel, ("call_a".into(), 7));
    }

    #[test]
    fn direct_new_call_snapshot_cancels_the_previous_native_call_identity() {
        let client = offered_client(LeaseMode::Takeover, MobileOfferSurface::VoiceSystemUi);
        let mut replacement = client.snapshot.as_ref().unwrap().clone();
        replacement.sequence += 1;
        replacement.snapshot.call_id = "call_b".into();
        replacement.snapshot.call_epoch = 8;

        assert_eq!(
            replaced_snapshot_call(&client, &replacement),
            Some(("call_a".into(), 7))
        );

        let same_call = client.snapshot.as_ref().unwrap().clone();
        assert_eq!(replaced_snapshot_call(&client, &same_call), None);
    }

    #[tokio::test]
    async fn in_app_takeover_requires_and_accepts_only_an_in_app_signed_offer() {
        let voice_only = V2State::default();
        *voice_only.inner.lock().await =
            offered_client(LeaseMode::Takeover, MobileOfferSurface::VoiceSystemUi);
        let error = prepare_offer_answer(&voice_only, LeaseMode::Takeover, None)
            .await
            .unwrap_err();
        assert_eq!(error, "no current signed mobile offer permits this lease");
        assert!(voice_only.inner.lock().await.pending.is_none());

        let in_app = V2State::default();
        *in_app.inner.lock().await = offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
        let (answer, lease_request_id) = prepare_offer_answer(&in_app, LeaseMode::Takeover, None)
            .await
            .unwrap();
        assert_eq!(answer.offer_id, "offer_exact_a");
        assert_eq!(answer.offer_jti, "offer_jti_exact_a");
        assert_eq!(answer.offered_mode, LeaseMode::Takeover);
        let pending = in_app
            .inner
            .lock()
            .await
            .pending
            .clone()
            .expect("pending lease");
        assert_eq!(pending.request_id, lease_request_id);
        assert!(pending.stage == PendingLeaseStage::AwaitingOfferAcceptance);
        assert_eq!(pending_lease_timeout(&pending, Instant::now()), None);
        let mut expired = pending;
        expired.deadline = Instant::now() - Duration::from_millis(1);
        assert_eq!(pending_lease_timeout(&expired, Instant::now()), Some(false));
    }

    #[tokio::test]
    async fn rejected_offer_or_lease_attempt_cannot_reuse_the_spent_snapshot_offer() {
        for reject_after_offer_acceptance in [false, true] {
            let state = V2State::default();
            *state.inner.lock().await =
                offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
            let (answer, lease_request_id) =
                prepare_offer_answer(&state, LeaseMode::Takeover, None)
                    .await
                    .expect("the first press selects the published offer");

            let rejected_request_id = if reject_after_offer_acceptance {
                let accepted = MobileOfferAcceptedFrame {
                    kind: "mobile_offer_accepted".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    request_id: answer.request_id.clone(),
                    offer_id: answer.offer_id.clone(),
                    offer_jti: answer.offer_jti.clone(),
                    offered_mode: answer.offered_mode,
                    accepted: true,
                };
                let encoded = serde_json::to_string(&accepted).unwrap();
                apply_mobile_offer_accepted(&state, "app_a", accepted, &encoded)
                    .await
                    .expect("the offer acceptance releases its lease request");
                take_ready_lease_request(&state)
                    .await
                    .expect("the pending lease remains valid")
                    .expect("the lease request is delivered exactly once");
                lease_request_id
            } else {
                answer.request_id
            };
            let rejection = ClaimRejectedFrame {
                kind: "claim_rejected".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                request_id: rejected_request_id,
                code: "stale_call".into(),
                message: "the one-shot offer was not redeemed".into(),
            };
            let encoded = serde_json::to_string(&rejection).unwrap();
            let action = {
                let mut client = state.inner.lock().await;
                prepare_claim_rejection(&mut client, &rejection, &encoded)
                    .expect("the matching failure is transactional")
            };
            assert!(matches!(action, ClaimRejectionAction::RejectPending { .. }));

            let client = state.inner.lock().await;
            assert!(client.pending.is_none());
            assert_eq!(client.spent_mobile_offers.len(), 1);
            assert!(client
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.snapshot.pending_mobile_offers.is_empty()));
            drop(client);
            assert_eq!(
                prepare_offer_answer(&state, LeaseMode::Takeover, None)
                    .await
                    .expect_err("an immediate retry cannot answer the spent cached offer"),
                "no current signed mobile offer permits this lease"
            );
        }
    }

    #[tokio::test]
    async fn late_exact_transfer_offer_ack_is_terminal_after_local_timeout() {
        let state = V2State::default();
        *state.inner.lock().await = transfer_client(MobileOfferSurface::VoiceSystemUi);
        let action = native_transfer_action(NativeCallActionKind::Answer);
        let (answer, lease_request_id) = prepare_offer_answer_for_transfer(
            &state,
            LeaseMode::Takeover,
            Some(&action),
            Some("transfer_request_a"),
        )
        .await
        .expect("the transfer offer is answered once");

        // Model the local 10-second timeout: the pending attempt is gone but
        // its exact one-shot offer tombstone remains. Desktop may retain its
        // separate reservation until the 45-second setup deadline.
        assert_eq!(
            clear_pending_lease(&state, &lease_request_id).await,
            Some("native_answer_a".into())
        );
        {
            let mut client = state.inner.lock().await;
            let mut next_call = client.snapshot.as_ref().unwrap().clone();
            next_call.sequence += 1;
            next_call.snapshot.call_id = "call_b".into();
            next_call.snapshot.call_epoch = 8;
            retain_spent_mobile_offers_for_snapshot(&mut client, &next_call);
            assert!(client.spent_mobile_offers.is_empty());
            assert_eq!(client.terminal_offer_acceptances.len(), 1);
            client.snapshot = Some(next_call);
        }
        let accepted = MobileOfferAcceptedFrame {
            kind: "mobile_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: answer.request_id,
            offer_id: answer.offer_id,
            offer_jti: answer.offer_jti,
            offered_mode: answer.offered_mode,
            accepted: true,
        };
        let encoded = serde_json::to_string(&accepted).unwrap();

        apply_mobile_offer_accepted(&state, "app_a", accepted.clone(), &encoded)
            .await
            .expect("the exact late receipt is contained without reviving authority");
        apply_mobile_offer_accepted(&state, "app_a", accepted, &encoded)
            .await
            .expect("an exact relay replay remains a terminal no-op");
        assert!(state.inner.lock().await.pending.is_none());
        assert!(state.inner.lock().await.lease.is_none());
        assert!(take_ready_lease_request(&state).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_new_snapshot_can_replace_a_spent_offer_with_a_fresh_one() {
        let state = V2State::default();
        *state.inner.lock().await = offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
        let original_offer = state
            .inner
            .lock()
            .await
            .snapshot
            .as_ref()
            .unwrap()
            .snapshot
            .pending_mobile_offers[0]
            .clone();
        let (answer, _) = prepare_offer_answer(&state, LeaseMode::Takeover, None)
            .await
            .expect("the first offer is selected");
        let rejection = ClaimRejectedFrame {
            kind: "claim_rejected".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: answer.request_id,
            code: "stale_call".into(),
            message: "the first offer is spent".into(),
        };
        let encoded = serde_json::to_string(&rejection).unwrap();
        {
            let mut client = state.inner.lock().await;
            assert!(matches!(
                prepare_claim_rejection(&mut client, &rejection, &encoded).unwrap(),
                ClaimRejectionAction::RejectPending { .. }
            ));

            let mut fresh_offer = original_offer.clone();
            fresh_offer.offer.offer_id = "offer_exact_b".into();
            fresh_offer.offer.opportunity_id = "opportunity_b".into();
            fresh_offer.offer.jti = "offer_jti_exact_b".into();
            fresh_offer.offer_token = "signed.offer.token.b".into();
            let mut next = client
                .snapshot
                .clone()
                .expect("call snapshot remains current");
            next.sequence += 1;
            next.snapshot.pending_mobile_offers = vec![original_offer, fresh_offer];
            suppress_spent_mobile_offers(&client, &mut next);
            assert_eq!(next.snapshot.pending_mobile_offers.len(), 1);
            client.snapshot = Some(next);
        }

        let (fresh_answer, _) = prepare_offer_answer(&state, LeaseMode::Takeover, None)
            .await
            .expect("a genuinely fresh offer remains answerable");
        assert_eq!(fresh_answer.offer_id, "offer_exact_b");
        assert_eq!(fresh_answer.offer_jti, "offer_jti_exact_b");
    }

    #[tokio::test]
    async fn spent_offer_survives_transport_reset_generation_and_begin() {
        for reset_generation_only in [false, true] {
            let state = V2State::default();
            *state.inner.lock().await =
                offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
            let (identity, cached_snapshot) = {
                let client = state.inner.lock().await;
                (
                    client.endpoint_identity.clone().unwrap(),
                    client.snapshot.clone().unwrap(),
                )
            };
            let (answer, lease_request_id) =
                prepare_offer_answer(&state, LeaseMode::Takeover, None)
                    .await
                    .expect("the first session spends the published offer");
            let original_answer = serde_json::to_string(&answer).unwrap();
            clear_pending_lease(&state, &lease_request_id).await;

            if reset_generation_only {
                state.reset_generation(1).await;
            } else {
                state.reset().await;
            }
            assert_eq!(state.inner.lock().await.spent_mobile_offers.len(), 1);
            state
                .begin(
                    "app_a",
                    "device_a",
                    "session_after_reconnect",
                    &identity,
                    &[],
                    false,
                    true,
                    None,
                )
                .await;
            state.set_generation(2).await;
            {
                let mut client = state.inner.lock().await;
                assert_eq!(client.spent_mobile_offers.len(), 1);
                // Recreate the exact cached snapshot a reconnect can replay.
                // Leave its offer present to prove the selector's tombstone,
                // not merely snapshot filtering, owns this safety boundary.
                client.snapshot = Some(cached_snapshot);
            }

            assert_eq!(
                prepare_offer_answer(&state, LeaseMode::Takeover, None)
                    .await
                    .expect_err("reconnect must not answer the spent cached offer"),
                "no current signed mobile offer permits this lease"
            );
            assert_eq!(
                serde_json::to_string(&answer).unwrap(),
                original_answer,
                "the original prebuilt frame remains byte-identical for exact delivery retry"
            );
        }
    }

    #[tokio::test]
    async fn spent_offer_scope_ends_at_new_call_and_new_app_boundaries() {
        let state = V2State::default();
        *state.inner.lock().await = offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
        let (identity, original_snapshot) = {
            let client = state.inner.lock().await;
            (
                client.endpoint_identity.clone().unwrap(),
                client.snapshot.clone().unwrap(),
            )
        };
        let (_, request_id) = prepare_offer_answer(&state, LeaseMode::Takeover, None)
            .await
            .expect("the old call spends its offer");
        clear_pending_lease(&state, &request_id).await;

        let mut new_call = original_snapshot.clone();
        new_call.sequence += 1;
        new_call.snapshot.call_id = "call_b".into();
        new_call.snapshot.call_epoch += 1;
        for offer in &mut new_call.snapshot.pending_mobile_offers {
            offer.offer.call_id = new_call.snapshot.call_id.clone();
            offer.offer.call_epoch = new_call.snapshot.call_epoch;
        }
        {
            let mut client = state.inner.lock().await;
            retain_spent_mobile_offers_for_snapshot(&mut client, &new_call);
            assert!(client.spent_mobile_offers.is_empty());
            suppress_spent_mobile_offers(&client, &mut new_call);
            assert_eq!(new_call.snapshot.pending_mobile_offers.len(), 1);
            client.snapshot = Some(new_call);
        }
        let (new_call_answer, new_call_request_id) =
            prepare_offer_answer(&state, LeaseMode::Takeover, None)
                .await
                .expect("another call may use coincident offer identifiers");
        assert_eq!(new_call_answer.offer_id, "offer_exact_a");
        clear_pending_lease(&state, &new_call_request_id).await;

        state.reset().await;
        state
            .begin(
                "app_b",
                "device_a",
                "session_app_b",
                &identity,
                &[],
                false,
                true,
                None,
            )
            .await;
        state.set_generation(3).await;
        assert!(state.inner.lock().await.spent_mobile_offers.is_empty());
        let mut new_app = original_snapshot;
        new_app.app_id = "app_b".into();
        for offer in &mut new_app.snapshot.pending_mobile_offers {
            offer.offer.app_id = "app_b".into();
        }
        state.inner.lock().await.snapshot = Some(new_app);
        let (new_app_answer, _) = prepare_offer_answer(&state, LeaseMode::Takeover, None)
            .await
            .expect("another app may use coincident offer identifiers");
        assert_eq!(new_app_answer.offer_id, "offer_exact_a");
    }

    #[tokio::test]
    async fn core_telecom_answer_uses_exact_authoritative_offered_mode_then_waits_for_acceptance() {
        let state = V2State::default();
        *state.inner.lock().await =
            offered_client(LeaseMode::Consult, MobileOfferSurface::VoiceSystemUi);
        let action = native_answer_action();

        // The caller passes Takeover because Android Answer has no local mode
        // selector. The signed offer is authoritative and must win.
        let (answer, lease_request_id) =
            prepare_offer_answer(&state, LeaseMode::Takeover, Some(&action))
                .await
                .unwrap();
        assert_eq!(answer.offer_id, "offer_exact_a");
        assert_eq!(answer.offer_jti, "offer_jti_exact_a");
        assert_eq!(answer.offered_mode, LeaseMode::Consult);
        assert!(take_ready_lease_request(&state).await.unwrap().is_none());

        let accepted = MobileOfferAcceptedFrame {
            kind: "mobile_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: answer.request_id,
            offer_id: answer.offer_id,
            offer_jti: answer.offer_jti,
            offered_mode: LeaseMode::Consult,
            accepted: true,
        };
        let encoded = serde_json::to_string(&accepted).unwrap();
        apply_mobile_offer_accepted(&state, "app_a", accepted, &encoded)
            .await
            .unwrap();
        let outbound = take_ready_lease_request(&state)
            .await
            .unwrap()
            .expect("accepted offer must stage its exact lease request");
        let request: LeaseRequestFrame = serde_json::from_str(&outbound.encoded).unwrap();
        assert_eq!(request.request_id, lease_request_id);
        assert_eq!(request.mode, LeaseMode::Consult);
        assert_eq!(request.accepted_offer_id, "offer_exact_a");
        assert_eq!(request.accepted_offer_jti, "offer_jti_exact_a");
        assert_eq!(
            outbound.answer_action_id.as_deref(),
            Some("native_answer_a")
        );
    }

    #[tokio::test]
    async fn core_telecom_answer_rejects_cross_offer_and_wrong_acceptance_fences() {
        let state = V2State::default();
        *state.inner.lock().await =
            offered_client(LeaseMode::Takeover, MobileOfferSurface::VoiceSystemUi);
        let mut wrong_action = native_answer_action();
        wrong_action.offer_id = "offer_other".into();
        assert!(
            prepare_offer_answer(&state, LeaseMode::Takeover, Some(&wrong_action))
                .await
                .is_err()
        );
        assert!(state.inner.lock().await.pending.is_none());

        let action = native_answer_action();
        let (answer, _) = prepare_offer_answer(&state, LeaseMode::Takeover, Some(&action))
            .await
            .unwrap();
        let wrong = MobileOfferAcceptedFrame {
            kind: "mobile_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: answer.request_id,
            offer_id: answer.offer_id,
            offer_jti: "offer_jti_other".into(),
            offered_mode: LeaseMode::Takeover,
            accepted: true,
        };
        let encoded = serde_json::to_string(&wrong).unwrap();
        assert!(
            apply_mobile_offer_accepted(&state, "app_a", wrong, &encoded)
                .await
                .is_err()
        );
        assert!(take_ready_lease_request(&state).await.unwrap().is_none());
    }

    fn pending_revoke_fixture(mode: LeaseMode, authoritative_sequence: u64) -> PendingRevoke {
        let lease_claims = claims(LeaseMode::Takeover, LeasePhase::Active);
        let lease_claims = LeaseClaims {
            mode,
            tracks: tracks_for(mode, LeasePhase::Active),
            fence: if mode == LeaseMode::Takeover { 9 } else { 0 },
            ..lease_claims
        };
        PendingRevoke {
            request_id: "return_request_a".into(),
            lease_id: lease_claims.lease_id.clone(),
            lease_jti: lease_claims.jti.clone(),
            lease: ClientLease {
                request_id: "claim_request_a".into(),
                token: "v2.redacted.signature".into(),
                session: session_from_claims(&lease_claims, 1, 1).unwrap(),
                claims: lease_claims,
            },
            authoritative_sequence,
            authoritative_remote_revision: Some(13),
            native_action_id: Some("native_return_a".into()),
            deadline: Instant::now() + REVOKE_CONFIRM_TIMEOUT,
        }
    }

    fn return_snapshot(mode: LeaseMode, sequence: u64) -> MobileSnapshotFrame {
        let mut frame = offered_client(mode, MobileOfferSurface::InApp)
            .snapshot
            .expect("fixture has authoritative state");
        frame.sequence = sequence;
        frame.snapshot.pending_mobile_offers.clear();
        frame
    }

    #[test]
    fn lease_return_ack_accepts_requestless_plugin_notice_only_on_exact_lease_fence() {
        let pending = pending_revoke_fixture(LeaseMode::Takeover, 8);
        let exact = LeaseRevokedFrame {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: Some("return_request_a".into()),
            lease_id: "media_a".into(),
            lease_jti: "lease_a".into(),
            reason: "returned".into(),
        };
        validate_pending_revoke_ack(&pending, &exact).unwrap();
        let mut wrong = exact.clone();
        wrong.request_id = None;
        validate_pending_revoke_ack(&pending, &wrong).unwrap();
        wrong = exact.clone();
        wrong.request_id = Some("return_request_other".into());
        assert!(validate_pending_revoke_ack(&pending, &wrong).is_err());
        wrong = exact.clone();
        wrong.lease_jti = "lease_other".into();
        assert!(validate_pending_revoke_ack(&pending, &wrong).is_err());
    }

    #[test]
    fn matching_expiry_tombstone_cannot_bypass_the_pending_revoke_request_fence() {
        let pending = pending_revoke_fixture(LeaseMode::Takeover, 8);
        let mut client = ClientState {
            pending_revoke: Some(pending.clone()),
            ..ClientState::default()
        };
        push_local_expiry_tombstone(
            &mut client,
            pending.lease.clone(),
            Instant::now() + LOCAL_EXPIRY_TOMBSTONE_TTL,
            true,
        )
        .unwrap();
        let wrong_request = LeaseRevokedFrame {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: Some("return_request_attacker".into()),
            lease_id: pending.lease_id,
            lease_jti: pending.lease_jti,
            reason: "returned".into(),
        };

        assert!(pending_revoke_identity_matches(&client, &wrong_request).is_err());
        assert!(client.pending_revoke.is_some());
        assert_eq!(client.local_expiry_tombstones.len(), 1);
    }

    #[test]
    fn acknowledged_expiry_route_still_contains_late_exact_signed_rtc() {
        let (mut client, claims, plugin) =
            plugin_bound_lease_status_client(LeaseMode::Takeover, LeasePhase::Active);
        let lease = ClientLease {
            request_id: "claim_request_a".into(),
            token: "signed.returnable.token".into(),
            session: session_from_claims(&claims, 2, 2).unwrap(),
            claims,
        };
        let pending = PendingRevoke {
            request_id: "return_request_a".into(),
            lease_id: lease.claims.lease_id.clone(),
            lease_jti: lease.claims.jti.clone(),
            lease: lease.clone(),
            authoritative_sequence: 8,
            authoritative_remote_revision: Some(13),
            native_action_id: None,
            deadline: Instant::now() + REVOKE_CONFIRM_TIMEOUT,
        };
        push_local_expiry_tombstone(
            &mut client,
            lease.clone(),
            Instant::now() + LOCAL_EXPIRY_TOMBSTONE_TTL,
            true,
        )
        .unwrap();
        client.pending_revoke = Some(pending);
        let ack = LeaseRevokedFrame {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: Some("return_request_a".into()),
            lease_id: lease.claims.lease_id.clone(),
            lease_jti: lease.claims.jti.clone(),
            reason: "returned".into(),
        };
        assert!(pending_revoke_identity_matches(&client, &ack).unwrap());
        client.pending_revoke = None;
        demote_local_expiry_tombstone(&mut client, &lease.claims.jti);
        let retained = client.local_expiry_tombstones.front().unwrap();
        assert!(!retained.returnable);
        assert!(retained.lease.token.is_empty());

        let now = unix_now().unwrap();
        let late = signed_plugin_rtc_for_lease(
            &plugin,
            &lease,
            "rtc_after_ack",
            "rtc_after_ack_proof",
            now.saturating_sub(1),
            now + 5,
        );
        assert!(contain_locally_closed_rtc(
            &mut client,
            &late,
            "rtc:rtc_after_ack",
            "late-rtc-after-ack-wire",
            now,
        )
        .expect("acknowledgement retains an exact route-only RTC fence"));
    }

    #[test]
    fn plugin_failure_notice_can_complete_a_pending_return_before_gateway_ack() {
        let pending = pending_revoke_fixture(LeaseMode::Takeover, 8);
        let plugin_notice = LeaseRevokedFrame {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: None,
            lease_id: pending.lease_id.clone(),
            lease_jti: pending.lease_jti.clone(),
            reason: "peer_connection_failed".into(),
        };
        validate_pending_revoke_ack(&pending, &plugin_notice).unwrap();
        let plugin_encoded = serde_json::to_string(&plugin_notice).unwrap();
        let pending_tombstone = completed_revoke_from_pending(&pending);
        let mut client = ClientState {
            pending_revoke: Some(pending),
            ..ClientState::default()
        };
        remember_completed_revoke(&mut client, pending_tombstone).unwrap();
        remember_completed_revoke(
            &mut client,
            completed_revoke_from_frame(&plugin_notice, &plugin_encoded),
        )
        .unwrap();
        client.pending_revoke = None;

        let gateway_ack = LeaseRevokedFrame {
            request_id: Some("return_request_a".into()),
            reason: "returned".into(),
            ..plugin_notice
        };
        let gateway_encoded = serde_json::to_string(&gateway_ack).unwrap();
        assert!(
            completed_revoke_frame_is_replay(&mut client, &gateway_ack, &gateway_encoded,).unwrap()
        );
        let completed = client.completed_revokes.front().unwrap();
        assert_eq!(completed.request_id.as_deref(), Some("return_request_a"));
        assert!(completed.frame_digest.is_some());
        assert!(completed.unsolicited_frame_digest.is_some());

        let wrong_ack = LeaseRevokedFrame {
            request_id: Some("return_request_other".into()),
            ..gateway_ack
        };
        let wrong_encoded = serde_json::to_string(&wrong_ack).unwrap();
        assert!(
            completed_revoke_frame_is_replay(&mut client, &wrong_ack, &wrong_encoded,).is_err()
        );
    }

    #[test]
    fn snapshot_completion_tombstones_late_ack_and_changed_replay() {
        let pending = pending_revoke_fixture(LeaseMode::Takeover, 8);
        let mut snapshot = return_snapshot(LeaseMode::Takeover, 9);
        snapshot.snapshot.owner_epoch = pending.lease.claims.owner_epoch + 1;
        snapshot.snapshot.remote_revision = 14;
        let mut client = ClientState {
            pending_revoke: Some(pending),
            ..ClientState::default()
        };
        assert!(
            take_snapshot_confirmed_pending_revoke(&mut client, &snapshot)
                .unwrap()
                .is_some()
        );
        assert_eq!(client.completed_revokes.len(), 1);
        assert!(client.completed_revokes[0].frame_digest.is_none());

        let ack = LeaseRevokedFrame {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: Some("return_request_a".into()),
            lease_id: "media_a".into(),
            lease_jti: "lease_a".into(),
            reason: "returned".into(),
        };
        let encoded = serde_json::to_string(&ack).unwrap();
        assert!(completed_revoke_frame_is_replay(&mut client, &ack, &encoded).unwrap());
        assert!(client.completed_revokes[0].frame_digest.is_some());
        assert!(completed_revoke_frame_is_replay(&mut client, &ack, &encoded).unwrap());

        let mut changed = ack.clone();
        changed.reason = "different_reason".into();
        let changed_encoded = serde_json::to_string(&changed).unwrap();
        assert!(completed_revoke_frame_is_replay(&mut client, &changed, &changed_encoded).is_err());

        // Snapshot may confirm the local return before the plugin's media
        // failure notice arrives. Its requestId=None dialect is compatible on
        // the exact same lease/JTI and gets its own byte-for-byte replay fence.
        let plugin_notice = LeaseRevokedFrame {
            request_id: None,
            reason: "peer_connection_failed".into(),
            ..ack.clone()
        };
        let plugin_encoded = serde_json::to_string(&plugin_notice).unwrap();
        assert!(
            completed_revoke_frame_is_replay(&mut client, &plugin_notice, &plugin_encoded,)
                .unwrap()
        );
        assert!(client.completed_revokes[0]
            .unsolicited_frame_digest
            .is_some());
        assert!(
            completed_revoke_frame_is_replay(&mut client, &plugin_notice, &plugin_encoded,)
                .unwrap()
        );
        let mut changed_plugin = plugin_notice;
        changed_plugin.reason = "rtc_closed".into();
        let changed_plugin_encoded = serde_json::to_string(&changed_plugin).unwrap();
        assert!(completed_revoke_frame_is_replay(
            &mut client,
            &changed_plugin,
            &changed_plugin_encoded,
        )
        .is_err());

        changed = ack;
        changed.lease_jti = "lease_other".into();
        let changed_encoded = serde_json::to_string(&changed).unwrap();
        assert!(completed_revoke_frame_is_replay(&mut client, &changed, &changed_encoded).is_err());
    }

    #[test]
    fn duplicate_plugin_revoke_is_a_noop_but_changed_content_fails_closed() {
        let frame = LeaseRevokedFrame {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: None,
            lease_id: "media_a".into(),
            lease_jti: "lease_a".into(),
            reason: "peer_connection_failed".into(),
        };
        let encoded = serde_json::to_string(&frame).unwrap();
        let mut client = ClientState::default();
        remember_completed_revoke(&mut client, completed_revoke_from_frame(&frame, &encoded))
            .unwrap();

        assert!(completed_revoke_frame_is_replay(&mut client, &frame, &encoded).unwrap());
        let mut changed = frame;
        changed.reason = "rtc_closed".into();
        let changed_encoded = serde_json::to_string(&changed).unwrap();
        assert!(completed_revoke_frame_is_replay(&mut client, &changed, &changed_encoded).is_err());
    }

    #[tokio::test]
    async fn completed_revoke_tombstones_are_bounded_and_survive_transport_reset() {
        let state = V2State::default();
        {
            let mut client = state.inner.lock().await;
            for index in 0..=MAX_COMPLETED_REVOKES {
                remember_completed_revoke(
                    &mut client,
                    CompletedRevoke {
                        app_id: "app_a".into(),
                        request_id: Some(format!("return_request_{index}")),
                        lease_id: format!("media_{index}"),
                        lease_jti: format!("lease_{index}"),
                        frame_digest: Some(relay_frame_digest(&format!("revoke_{index}"))),
                        unsolicited_frame_digest: None,
                    },
                )
                .unwrap();
            }
            assert_eq!(client.completed_revokes.len(), MAX_COMPLETED_REVOKES);
            assert_eq!(
                client
                    .completed_revokes
                    .front()
                    .unwrap()
                    .request_id
                    .as_deref(),
                Some("return_request_1")
            );
        }

        state.reset().await;

        let client = state.inner.lock().await;
        assert_eq!(client.completed_revokes.len(), MAX_COMPLETED_REVOKES);
        assert_eq!(
            client
                .completed_revokes
                .back()
                .unwrap()
                .request_id
                .as_deref(),
            Some("return_request_256")
        );
    }

    #[test]
    fn newer_aokie_snapshot_confirms_each_relay_lease_return_mode() {
        for mode in [LeaseMode::Monitor, LeaseMode::Consult, LeaseMode::Takeover] {
            let pending = pending_revoke_fixture(mode, 8);
            let mut frame = return_snapshot(mode, 9);
            frame.snapshot.remote_revision = 14;
            if mode != LeaseMode::Monitor {
                frame.snapshot.owner_epoch = pending.lease.claims.owner_epoch + 1;
            }

            assert!(snapshot_confirms_pending_revoke(&pending, &frame));
            let mut client = ClientState {
                pending_revoke: Some(pending),
                ..ClientState::default()
            };
            assert!(take_snapshot_confirmed_pending_revoke(&mut client, &frame)
                .unwrap()
                .is_some());
            assert!(client.pending_revoke.is_none());
            assert_eq!(client.completed_revokes.len(), 1);
        }
    }

    #[test]
    fn translated_sequence_cannot_confirm_monitor_without_new_ready_source_state() {
        let pending = pending_revoke_fixture(LeaseMode::Monitor, 8);
        let queued_before_revoke = return_snapshot(LeaseMode::Monitor, 9);
        assert_eq!(queued_before_revoke.snapshot.remote_revision, 13);
        assert!(!snapshot_confirms_pending_revoke(
            &pending,
            &queued_before_revoke
        ));

        let mut advanced_but_not_ready = queued_before_revoke.clone();
        advanced_but_not_ready.snapshot.remote_revision = 14;
        advanced_but_not_ready.snapshot.media_state = MediaState::Active;
        assert!(!snapshot_confirms_pending_revoke(
            &pending,
            &advanced_but_not_ready
        ));

        let mut proven = advanced_but_not_ready;
        proven.snapshot.media_state = MediaState::Ready;
        assert!(snapshot_confirms_pending_revoke(&pending, &proven));

        let mut no_source_baseline = pending;
        no_source_baseline.authoritative_remote_revision = None;
        assert!(!snapshot_confirms_pending_revoke(
            &no_source_baseline,
            &proven
        ));
    }

    #[test]
    fn moved_or_ended_call_confirms_old_relay_lease_return() {
        let pending = pending_revoke_fixture(LeaseMode::Takeover, 8);
        let mut moved = return_snapshot(LeaseMode::Takeover, 9);
        moved.snapshot.call_id = "call_b".into();
        moved.snapshot.service_mode = ServiceMode::HumanActive;
        assert!(snapshot_confirms_pending_revoke(&pending, &moved));

        let mut ended = return_snapshot(LeaseMode::Takeover, 9);
        ended.snapshot.telephony_state = TelephonyState::Ended;
        ended.snapshot.service_mode = ServiceMode::Ended;
        assert!(snapshot_confirms_pending_revoke(&pending, &ended));
    }

    #[test]
    fn stale_human_or_unadvanced_snapshot_cannot_confirm_relay_lease_return() {
        let monitor = pending_revoke_fixture(LeaseMode::Monitor, 8);
        let stale = return_snapshot(LeaseMode::Monitor, 8);
        assert!(!snapshot_confirms_pending_revoke(&monitor, &stale));

        let takeover = pending_revoke_fixture(LeaseMode::Takeover, 8);
        let mut human_active = return_snapshot(LeaseMode::Takeover, 9);
        human_active.snapshot.remote_revision = 14;
        human_active.snapshot.owner_epoch = takeover.lease.claims.owner_epoch + 1;
        human_active.snapshot.service_mode = ServiceMode::HumanActive;
        assert!(!snapshot_confirms_pending_revoke(&takeover, &human_active));

        for mode in [LeaseMode::Consult, LeaseMode::Takeover] {
            let pending = pending_revoke_fixture(mode, 8);
            let mut queued_before_revoke = return_snapshot(mode, 9);
            queued_before_revoke.snapshot.owner_epoch = pending.lease.claims.owner_epoch + 1;
            assert!(!snapshot_confirms_pending_revoke(
                &pending,
                &queued_before_revoke
            ));

            let mut no_owner_advance = return_snapshot(mode, 9);
            no_owner_advance.snapshot.remote_revision = 14;
            assert!(!snapshot_confirms_pending_revoke(
                &pending,
                &no_owner_advance
            ));
        }
    }

    #[test]
    fn caller_ending_requires_exact_active_takeover_grant_and_authority() {
        let now = unix_now().unwrap();
        let mut client = end_caller_client();
        validate_local_end_caller(&client, now).unwrap();

        client
            .snapshot
            .as_mut()
            .unwrap()
            .grants
            .retain(|grant| *grant != Grant::EndCaller);
        assert!(validate_local_end_caller(&client, now).is_err());
        client
            .snapshot
            .as_mut()
            .unwrap()
            .grants
            .push(Grant::EndCaller);
        client.snapshot.as_mut().unwrap().snapshot.service_mode = ServiceMode::ReturningToAokie;
        assert!(validate_local_end_caller(&client, now).is_err());
    }

    #[test]
    fn newer_exact_snapshot_completes_a_lost_targeted_mute_status() {
        let mut client = end_caller_client();
        let snapshot = client.snapshot.as_ref().unwrap().clone();
        let lease = client.lease.as_ref().unwrap().clone();
        client.pending_microphone_mute = Some(pending_microphone_mute(
            "microphone_mute_a".into(),
            true,
            PendingMicrophoneMuteStage::AwaitingStatus,
            &snapshot,
            &lease,
        ));
        let mut newer = snapshot;
        newer.sequence += 1;
        newer.snapshot.remote_revision += 1;
        newer.snapshot.companion_microphone_muted = true;

        assert!(matches!(
            reconcile_microphone_mute_from_snapshot(&mut client, &newer),
            SnapshotMicrophoneMuteReconciliation::Confirmed(pending)
                if pending.muted && pending.session == lease.session
        ));
        assert!(client.pending_microphone_mute.is_none());
    }

    #[tokio::test]
    async fn native_hangup_uses_challenge_confirmation_and_failed_result_keeps_media() {
        let state = V2State::default();
        *state.inner.lock().await = end_caller_client();
        let mut action = native_answer_action();
        action.action_id = "native_hangup_a".into();
        action.kind = NativeCallActionKind::Hangup;
        let outbound = queue_native_hangup(&state, action)
            .await
            .expect("exact active owner may begin caller hangup");
        let request: MobileEndCallerChallengeRequestFrame =
            serde_json::from_str(&outbound.encoded).unwrap();
        assert_eq!(request.kind, "end_caller_challenge_request");

        let challenge = {
            let client = state.inner.lock().await;
            let snapshot = client.snapshot.as_ref().unwrap();
            let lease = client.lease.as_ref().unwrap();
            assert!(client.pending_native_hangup.is_some());
            EndCallerChallengeFrame {
                kind: "end_caller_challenge".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                request_id: request.request_id.clone(),
                confirmation_id: "confirmation_a".into(),
                nonce: "nonce_a".into(),
                device_id: "device_a".into(),
                call_id: lease.claims.call_id.clone(),
                call_epoch: lease.claims.call_epoch,
                owner_epoch: lease.claims.owner_epoch,
                switchboard_revision: snapshot.snapshot.switchboard_revision,
                remote_revision: snapshot.snapshot.remote_revision,
                lease_id: lease.claims.lease_id.clone(),
                fence: lease.claims.fence,
                expires_at: unix_now().unwrap() + 10,
            }
        };
        let confirmation = {
            let client = state.inner.lock().await;
            native_hangup_confirmation_frame(&state, &client, &challenge, unix_now().unwrap())
                .expect("system gesture confirms only the exact returned challenge")
        };
        assert_eq!(confirmation.kind, "end_caller_confirm");
        assert_eq!(confirmation.confirmation_id, challenge.confirmation_id);
        assert_eq!(confirmation.nonce, challenge.nonce);

        let operation_id = "caller_end_operation_a".to_string();
        let failed = PluginEndCallerResultFrame {
            kind: "end_caller_result".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            operation_id: operation_id.clone(),
            confirmation_id: challenge.confirmation_id.clone(),
            device_id: challenge.device_id.clone(),
            call_id: challenge.call_id.clone(),
            call_epoch: challenge.call_epoch,
            owner_epoch: challenge.owner_epoch,
            switchboard_revision: challenge.switchboard_revision,
            remote_revision: challenge.remote_revision,
            lease_id: challenge.lease_id.clone(),
            fence: challenge.fence,
            outcome: EndCallerOutcome::Failed,
            code: Some("radio_end_failed".into()),
            message: Some("the radio did not confirm caller termination".into()),
        };
        failed.validate().unwrap();
        let mut client = state.inner.lock().await;
        client.pending_end_caller = Some(PendingEndCaller::AwaitingResult {
            request_id: confirmation.request_id,
            challenge: challenge.clone(),
            operation_id: operation_id.clone(),
        });
        let original_lease = client.lease.clone();
        let (lease_to_close, native_action) =
            apply_end_caller_result(&mut client, &failed).unwrap();
        assert!(lease_to_close.is_none());
        assert_eq!(native_action.as_deref(), Some("native_hangup_a"));
        assert_eq!(client.lease, original_lease);
        assert!(client.pending_end_caller.is_none());
        assert!(client.pending_native_hangup.is_none());

        let mut turnover = end_caller_client();
        turnover.pending_end_caller = Some(PendingEndCaller::AwaitingResult {
            request_id: "end_confirm_delayed".into(),
            challenge: challenge.clone(),
            operation_id: operation_id.clone(),
        });
        turnover.pending_native_hangup = Some(PendingNativeHangup {
            action_id: "native_hangup_delayed".into(),
            deadline: Instant::now() + NATIVE_HANGUP_TIMEOUT,
        });
        let mut newer_claims = turnover.lease.as_ref().unwrap().claims.clone();
        newer_claims.owner_epoch += 1;
        newer_claims.lease_id = "media_new_owner".into();
        newer_claims.jti = "lease_new_owner".into();
        newer_claims.rtc_session_id = "rtc_new_owner".into();
        newer_claims.fence += 1;
        let newer_lease = ClientLease {
            request_id: "newer_takeover_request".into(),
            token: "v2.newer.signature".into(),
            session: session_from_claims(&newer_claims, 3, 3).unwrap(),
            claims: newer_claims,
        };
        turnover.lease = Some(newer_lease.clone());
        let mut completed = failed;
        completed.outcome = EndCallerOutcome::Completed;
        completed.code = None;
        completed.message = None;
        completed.validate().unwrap();
        let (lease_to_close, native_action) =
            apply_end_caller_result(&mut turnover, &completed).unwrap();
        assert!(lease_to_close.is_none());
        assert_eq!(native_action.as_deref(), Some("native_hangup_delayed"));
        assert_eq!(turnover.lease.as_ref(), Some(&newer_lease));
    }

    #[tokio::test]
    async fn new_claims_are_blocked_while_any_caller_end_operation_is_pending() {
        let state = V2State::default();
        let mut client = offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
        client.pending_end_caller = Some(PendingEndCaller::AwaitingChallenge {
            request_id: "end_prepare_pending".into(),
        });
        *state.inner.lock().await = client;
        assert!(prepare_offer_answer(&state, LeaseMode::Takeover, None)
            .await
            .is_err());

        let mut client = offered_client(LeaseMode::Takeover, MobileOfferSurface::InApp);
        client.pending_native_hangup = Some(PendingNativeHangup {
            action_id: "native_hangup_pending".into(),
            deadline: Instant::now() + NATIVE_HANGUP_TIMEOUT,
        });
        *state.inner.lock().await = client;
        assert!(prepare_offer_answer(&state, LeaseMode::Takeover, None)
            .await
            .is_err());
    }

    #[test]
    fn native_red_end_uses_lease_revoke_and_never_caller_end_confirmation() {
        let frame = LeaseRevokeFrame {
            kind: "lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: "native_red_end".into(),
            idempotency_key: "native-red-end-key".into(),
            lease_token: "v2.redacted.signature".into(),
            reason: "native_call_ui_ended".into(),
        };
        frame.validate().unwrap();
        let encoded = serde_json::to_string(&frame).unwrap();
        assert!(encoded.contains("\"kind\":\"lease_revoke\""));
        assert!(!encoded.contains("end_caller_confirm"));
        assert!(!encoded.contains("end_caller_challenge"));
    }
}
