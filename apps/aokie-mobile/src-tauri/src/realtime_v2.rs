//! Native Companion protocol-v2 transport adapter.
//!
//! Lease tokens never enter the WebView.  The adapter binds every SDP/ICE
//! frame to the current, short-lived lease and drives `aokie-media` directly.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aokie_media::{
    IceCandidateSignal, IceServerConfig, MediaMode, SdpSignal, SdpSignalType, SessionBinding,
};
use aokie_protocol::v2::{
    sdp_dtls_fingerprint, sdp_sha256, AdmissionRole, EndCallerChallengeFrame, EndCallerOutcome,
    EndpointBindingClaims, EndpointChallengeFrame, Grant, HelloProofClaims, LeaseClaims,
    LeaseHeartbeatFrame, LeaseMode, LeasePhase, LeaseRequestFrame, LeaseRevokeFrame, MediaState,
    MobileAssistanceAnswerFrame, MobileEndCallerChallengeRequestFrame, MobileEndCallerConfirmFrame,
    MobileHello, MobileIdleSyncFrame, MobileOfferAnswerFrame, MobileOfferSurface,
    MobileRtcSignalFrame, MobileSnapshotFrame, PluginAssistanceRequestFrame,
    PluginEndCallerResultFrame, PluginHello, PluginIdleFrame, PluginSnapshotFrame,
    ProjectedCallSnapshot, RtcSignal, ServiceMode, SignedPendingMobileOffer, TelephonyState,
    TrickleCandidateClaims, V2ProtocolError, MAX_LEASE_TOKEN_BYTES, MAX_SAFE_INTEGER,
    SCHEMA_VERSION,
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
    ConnectionSlot, RealtimeConfig, RealtimeState, TransportEvent,
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
const ADMISSION_REFRESH_MARGIN: Duration = Duration::from_secs(20);
const NATIVE_ACTION_POLL: Duration = Duration::from_millis(200);
const NATIVE_ACTION_TIMEOUT: Duration = Duration::from_secs(5);
const LEASE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const REVOKE_ACK_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_ANSWERED_ASSISTANCE_REQUESTS: usize = 64;

type V2WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type V2Writer = SplitSink<V2WebSocket, Message>;
type V2Reader = SplitStream<V2WebSocket>;

/// How long one receive tick waits before handing control back to the session
/// loop. Only a cadence: nothing is lost when it elapses.
const TRANSPORT_READ_TICK: Duration = Duration::from_millis(200);

/// Environment opt-in for POSTING ANYTHING AT ALL over the relay.
///
/// ⚠️ DEFAULT OFF, DELIBERATELY, AND IT COVERS EVERY OUTBOUND FRAME — not just
/// the hello. The live plugin's frame dispatcher accepts exactly eight
/// GATEWAY-dialect kinds (`claim_proposal`, `lease_granted`, `lease_renewed`,
/// `lease_revoked`, `rtc_signal`, `assistance_answer`, `end_caller_execute`,
/// `error`) and ends in `_ => Err(reconnect("unsupported frame"))`, which tears
/// its session down. Over the socket a gateway process TRANSLATED the mobile
/// dialect into that one; on the relay nothing does, so every frame this
/// session emits is fatal to the peer:
///
/// * `mobile_hello`, `lease_request`, `end_caller_challenge_request` and
///   `end_caller_confirm` have no arm at all — straight to the catch-all.
/// * the two kind names that DO overlap still fail to parse, because the twins
///   are `deny_unknown_fields` and differ: `MobileRtcSignalFrame` carries a
///   `leaseToken` `PluginRtcSignalFrame` does not, and
///   `MobileAssistanceAnswerFrame` carries an `idempotencyKey` while
///   `PluginAssistanceAnswerFrame` wants a `deviceId`. A parse failure is a
///   teardown too.
///
/// So once this Companion is on the plugin's approved roster, ONE tap in the
/// Companion UI — answer, end-caller, assistance — would drop the session of a
/// desktop answering a real phone line, and would do it again on every retry.
/// Gating only the hello would have left every one of those paths open.
///
/// Turning this on is safe only after the plugin ships additive arms for the
/// mobile dialect. Until then the carrier runs genuinely read-only: it
/// authenticates, primes its cursor and reads the stream, and posts nothing.
///
/// ⚠️ Read-only also means SILENT, and roster approval does not change that.
/// The plugin's relay routing is speak-first — it learns a party from an
/// inbound frame and broadcasts only to parties it has learned — so a build
/// that posts nothing is never a destination, approved or not. Expect
/// `initial_sync` to time out on a loop until the plugin can accept a hello.
const RELAY_SEND_ENV: &str = "AOKIE_COMPANION_RELAY_SEND_HELLO";

fn relay_send_opt_in() -> bool {
    relay_send_opt_in_from(std::env::var(RELAY_SEND_ENV).ok().as_deref())
}

/// Exactly `"1"`, nothing else. Split out so the rule can be locked without a
/// test mutating process-wide environment under a parallel runner.
fn relay_send_opt_in_from(value: Option<&str>) -> bool {
    value == Some("1")
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
    /// Over the socket every frame goes out unchanged. Over the relay ALL of
    /// them are withheld unless [`RELAY_SEND_ENV`] opts in — see that constant
    /// for why posting any frame in today's mobile dialect tears down the
    /// session of a desktop on a live call.
    ///
    /// A withheld frame reports success so the session survives: the caller's
    /// contract is "false breaks the session", and dropping the working READ
    /// path because a user tapped an action the peer cannot accept would be a
    /// worse outcome than the action quietly not happening. The cost is that a
    /// queued user action resolves as delivered; that is bounded to the
    /// opt-in-off relay path, which is exactly the path where no user action
    /// can be honoured at all.
    async fn send_text(&mut self, encoded: String) -> bool {
        match self {
            Self::WebSocket { writer, .. } => send_text(writer, encoded).await,
            Self::Relay(relay) => {
                if !relay_send_opt_in() {
                    let kind = parse_kind(&encoded).unwrap_or_else(|_| "unparsed".into());
                    eprintln!(
                        "[AokieCompanion][relay] holding outbound {kind}: the plugin has no handler for the mobile dialect yet (set {RELAY_SEND_ENV}=1 once it does)"
                    );
                    return true;
                }
                relay.channel.send_text(&encoded).await
            }
        }
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
                    Ok(Some(Ok(Message::Close(frame)))) => V2Inbound::Closed(format!(
                        "gateway closed the active socket: {frame:?}"
                    )),
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

    /// Preserve carrier-level continuity across an admission rotation.
    ///
    /// The socket needs nothing here — the gateway holds the routing and the
    /// predecessor stays live during the overlap. The relay has no such
    /// middleman: its replacement must inherit the read cursor and any frame
    /// already read but not yet handled, or it re-reads what the session has
    /// already processed. The shim's sequence must carry too, because the
    /// session tracks authoritative state on a monotonic high-water mark.
    fn adopt_routing_from(&mut self, previous: &mut Self) {
        if let (Self::Relay(next), Self::Relay(previous)) = (self, previous) {
            next.channel.adopt_routing_from(&mut previous.channel);
            next.shim.adopt_sequence_from(&previous.shim);
        }
    }

    // No `close`: this session has always ended by dropping the carrier, and
    // the relay has nothing to close either — the mailbox holds no
    // per-connection state and already-posted frames stay readable until their
    // TTL expires. Sending a socket close frame here would be new behaviour on
    // a path a live call depends on.
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
/// Scope is the READ-ONLY subset. The lease path (`claim_decision`,
/// `plugin_lease_revoke`) and RTC signalling are dropped rather than
/// half-translated; when they land, note that `MobileRtcSignalFrame` carries a
/// `leaseToken` its plugin twin does not, and both are `deny_unknown_fields`, so
/// the shim must strip it exactly as `aokie-protocol`'s own comment (v2.rs, on
/// `validate_routed_mobile`) records the gateway doing.
struct GatewayShim {
    app_id: String,
    grants: Vec<Grant>,
    /// The Desktop endpoint key this admission pinned and the user confirmed.
    /// A `plugin_hello` that does not prove possession of it is not our peer.
    expected_peer_key_thumbprint: String,
    /// Cleared until the peer proves itself; authoritative state is never
    /// projected from an unauthenticated sender.
    peer_verified: bool,
    sequence: u64,
}

/// Desktop endpoints that have proved possession of their admission-pinned key
/// during this process's lifetime, keyed `<app_id>/<thumbprint>`.
///
/// ⚠️ Peer proof MUST outlive one carrier. The plugin greets a party exactly
/// once per PLUGIN session: its `greeted` set is cleared when the plugin arms a
/// rotated hello, never when this Companion reconnects. So a Companion that
/// verifies, loses its carrier and comes back is never greeted again — and with
/// verification scoped to the carrier, `peer_gate` would then drop every
/// snapshot for the rest of the plugin's session. That is not a slow recovery,
/// it is a permanent read deadlock that only a plugin restart clears, and the
/// same gap swallows an admission rotation whenever the line is quiet enough
/// that no state change re-greets us inside the replacement's 15s sync window.
///
/// Caching cannot promote an impostor. An entry is written only after a real
/// signature over the EXACT thumbprint the admission pinned and the user
/// confirmed, and it is read back only for that same app and thumbprint — so
/// the set can never say more than "this Desktop already proved itself to us".
fn proven_peers() -> &'static Mutex<HashSet<String>> {
    static PROVEN: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    PROVEN.get_or_init(|| Mutex::new(HashSet::new()))
}

fn proven_peer_key(app_id: &str, thumbprint: &str) -> String {
    format!("{app_id}/{thumbprint}")
}

impl GatewayShim {
    fn new(app_id: String, grants: Vec<Grant>, expected_peer_key_thumbprint: String) -> Self {
        // A duplicate grant fails `validate_snapshot`/`validate_idle_sync` on
        // EVERY frame, so a server that ever repeats one must not silently
        // brick the relay path.
        let mut unique = Vec::with_capacity(grants.len());
        for grant in grants {
            if !unique.contains(&grant) {
                unique.push(grant);
            }
        }
        // A Desktop that already proved this exact key to this process stays
        // proven: the plugin will not greet us a second time, so re-demanding a
        // hello here deadlocks the read path rather than securing anything.
        let peer_verified = proven_peers()
            .lock()
            .map(|proven| proven.contains(&proven_peer_key(&app_id, &expected_peer_key_thumbprint)))
            .unwrap_or(false);
        Self {
            app_id,
            grants: unique,
            expected_peer_key_thumbprint,
            peer_verified,
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
    /// ⚠️ `peer_verified` carries only while the admission still pins the SAME
    /// Desktop endpoint key. [`proven_peers`] is deliberately keyed by
    /// `<app_id>/<thumbprint>` so proof can never transfer to a different key;
    /// ORing the predecessor's flag in unconditionally would launder it around
    /// exactly that key. A rotation CAN re-pin — that is what happens when the
    /// Desktop's endpoint identity changes — and the replacement must then see
    /// the new key prove possession before any state is projected.
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
                self.accept_peer_hello(encoded)?;
                Ok(None)
            }
            "plugin_snapshot" => {
                let frame: PluginSnapshotFrame = strict_parse(encoded, "plugin snapshot")?;
                frame.validate().map_err(|error| error.to_string())?;
                let Some(()) = self.peer_gate("plugin_snapshot") else {
                    return Ok(None);
                };
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
                    grants: self.grants.clone(),
                };
                idle.validate().map_err(|error| error.to_string())?;
                serde_json::to_string(&idle)
                    .map(Some)
                    .map_err(|_| "could not encode a translated idle sync".to_string())
            }
            other => {
                eprintln!(
                    "[AokieCompanion][relay] dropped an untranslated plugin frame: {other}"
                );
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

    fn accept_peer_hello(&mut self, encoded: &str) -> Result<(), String> {
        let hello: PluginHello = strict_parse(encoded, "plugin hello")?;
        hello.validate().map_err(|error| error.to_string())?;
        hello
            .endpoint_proof
            .verify(unix_now()?)
            .map_err(|error| error.to_string())?;
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
        if !self.peer_verified {
            eprintln!("[AokieCompanion][relay] Desktop peer proved its endpoint key");
        }
        self.peer_verified = true;
        // Remembered for the life of the process, because the plugin will not
        // greet this party again until its own session rotates.
        if let Ok(mut proven) = proven_peers().lock() {
            proven.insert(proven_peer_key(
                &self.app_id,
                &self.expected_peer_key_thumbprint,
            ));
        }
        Ok(())
    }

    fn project(&mut self, frame: PluginSnapshotFrame) -> Result<Option<String>, String> {
        if frame.app_id != self.app_id {
            return Err("relay snapshot is bound to another app".into());
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
        let captions_permitted = source.remote_consent.enabled
            && source.remote_consent.acknowledged
            && source.remote_consent.captions_enabled
            && self.grants.contains(&Grant::CaptionsRead);
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
            caller: source.caller,
            captions: captions_permitted.then_some(source.captions),
            // The plugin publishes neither, and inventing either would be the
            // shim asserting state no endpoint authored: participant presence
            // is a gateway-side roster, and a pending offer must carry the
            // Desktop's own signature to be answerable at all.
            participants: Vec::new(),
            audio_levels: source.audio_levels,
            pending_mobile_offers: Vec::new(),
            occurred_at: source.occurred_at,
        };
        let snapshot = MobileSnapshotFrame {
            kind: "snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: frame.app_id,
            sequence: self.next_sequence(),
            grants: self.grants.clone(),
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
    pending: Option<PendingLease>,
    lease: Option<ClientLease>,
    pending_revoke: Option<PendingRevoke>,
    pending_native_end: Option<PendingNativeEnd>,
    ice_servers: Vec<IceServerConfig>,
    relay_only: bool,
    assistance: Option<PluginAssistanceRequestFrame>,
    pending_assistance_answer: Option<String>,
    // IDs only: keep answered help prompts replay-fenced without retaining the
    // private question, context, or answer across reconnects.
    answered_assistance_requests: VecDeque<(String, String)>,
    pending_end_caller: Option<PendingEndCaller>,
    seen_remote_endpoint_jtis: HashSet<String>,
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
    native_action_ids: Vec<String>,
    pending_call: Option<(String, u64)>,
}

#[derive(Clone)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone)]
struct ClientLease {
    request_id: String,
    token: String,
    claims: LeaseClaims,
    session: MediaSession,
}

#[derive(Clone)]
struct PendingRevoke {
    request_id: String,
    lease_id: String,
    lease_jti: String,
    lease: ClientLease,
    native_action_id: Option<String>,
    deadline: Instant,
}

#[derive(Clone)]
struct PendingNativeEnd {
    action: NativeCallAction,
    deadline: Instant,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum NativeCallActionKind {
    Answer,
    End,
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
    created_at: u64,
}

struct NativeOutbound {
    encoded: String,
    answer_action_id: Option<String>,
    answer_request_id: Option<String>,
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
        *state = ClientState {
            answered_assistance_requests,
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
    ) {
        let mut state = self.inner.lock().await;
        let answered_assistance_requests = std::mem::take(&mut state.answered_assistance_requests);
        *state = ClientState {
            app_id: Some(app_id.to_owned()),
            device_id: Some(device_id.to_owned()),
            session_nonce: Some(session_nonce.to_owned()),
            endpoint_identity: Some(endpoint_identity.clone()),
            ice_servers: ice_servers.to_vec(),
            relay_only,
            answered_assistance_requests,
            ..ClientState::default()
        };
    }

    async fn set_generation(&self, generation: u64) {
        self.inner.lock().await.generation = generation;
    }

    async fn set_peer_key_thumbprint(&self, thumbprint: String) {
        self.inner.lock().await.peer_key_thumbprint = Some(thumbprint);
    }

    async fn rotate_admission_policy(&self, ice_servers: &[IceServerConfig], relay_only: bool) {
        let mut state = self.inner.lock().await;
        state.ice_servers = ice_servers.to_vec();
        state.relay_only = relay_only;
    }

    async fn reset_generation(&self, generation: u64) {
        let mut state = self.inner.lock().await;
        if state.generation == generation {
            let answered_assistance_requests =
                std::mem::take(&mut state.answered_assistance_requests);
            *state = ClientState {
                answered_assistance_requests,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseStatusFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    request_id: String,
    lease_token: String,
    lease: LeaseClaims,
    #[serde(default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseRevokedFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    #[serde(default)]
    request_id: Option<String>,
    lease_id: String,
    lease_jti: String,
    reason: String,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaimRejectedFrame {
    kind: String,
    schema_version: u16,
    app_id: String,
    request_id: String,
    code: String,
    message: String,
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
}

fn grant_for_requested_mode(mode: LeaseMode) -> Result<Grant, String> {
    match mode {
        LeaseMode::Monitor => Ok(Grant::Monitor),
        LeaseMode::Takeover => Ok(Grant::Takeover),
        LeaseMode::Consult => Ok(Grant::Consult),
    }
}

#[tauri::command]
pub async fn realtime_v2_request_lease(
    state: State<'_, RealtimeState>,
    mode: LeaseMode,
) -> Result<V2RequestReceipt, String> {
    let (frame, request_id) = prepare_offer_answer(&state.v2, mode, None).await?;
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode protocol-v2 mobile offer answer".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        let _ = clear_pending_lease(&state.v2, &request_id).await;
        return Err(error);
    }
    Ok(V2RequestReceipt { request_id })
}

async fn prepare_offer_answer(
    state: &V2State,
    requested_mode: LeaseMode,
    native_action: Option<&NativeCallAction>,
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
            unix_now()?,
        )?;
        let mode = selected.offer.offered_mode;
        let required = grant_for_requested_mode(mode)?;
        if let Some(action) = native_action {
            validate_native_action(action, unix_now()?)?;
            if action.kind != NativeCallActionKind::Answer {
                return Err("native Answer does not match authoritative call state".into());
            }
        }
        if !snapshot.grants.contains(&required) || !snapshot.grants.contains(&Grant::RtcSignal) {
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
        if !snapshot.snapshot.remote_consent.allows(mode) {
            return Err("current remote disclosure consent does not allow this media mode".into());
        }
        if matches!(mode, LeaseMode::Takeover)
            && !matches!(snapshot.snapshot.service_mode, ServiceMode::AokieActive)
        {
            return Err("takeover is not available in the current service mode".into());
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
            signed.validate(now).is_ok()
                && offer.target_device_id == device_id
                && offer.target_holder_key_thumbprint == holder_key_thumbprint
                && offer.app_id == snapshot.app_id
                && offer.call_id == snapshot.snapshot.call_id
                && offer.call_epoch == snapshot.snapshot.call_epoch
                && offer.owner_epoch == snapshot.snapshot.owner_epoch
                && offer.switchboard_revision == snapshot.snapshot.switchboard_revision
                && offer.remote_revision == snapshot.snapshot.remote_revision
                && offer.required_consent_policy_id == snapshot.snapshot.remote_consent.policy_id
                && offer.required_consent_policy_version
                    == snapshot.snapshot.remote_consent.policy_version
                && offer
                    .required_grants
                    .iter()
                    .all(|grant| snapshot.grants.contains(grant))
                && snapshot.snapshot.remote_consent.allows(offer.offered_mode)
                && match native_action {
                    Some(action) => {
                        offer.offer_id == action.offer_id
                            && offer.app_id == action.app_id
                            && offer.call_id == action.call_id
                            && offer.call_epoch == action.call_epoch
                            && offer.owner_epoch == action.owner_epoch
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

async fn clear_pending_lease(state: &V2State, request_id: &str) -> Option<String> {
    let mut client = state.inner.lock().await;
    if client.pending.as_ref().is_some_and(|pending| {
        pending.request_id == request_id || pending.offer_request_id == request_id
    }) {
        return client
            .pending
            .take()
            .and_then(|pending| pending.native_action_id);
    }
    None
}

fn validate_native_action(action: &NativeCallAction, now: u64) -> Result<(), String> {
    if action.schema_version != 1
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
        || !snapshot.snapshot.remote_consent.allows(LeaseMode::Takeover)
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

#[tauri::command]
pub async fn realtime_v2_answer_assistance(
    state: State<'_, RealtimeState>,
    request: AssistanceAnswerRequest,
) -> Result<V2AssistanceReceipt, String> {
    let answer = request.answer.trim();
    let (frame, answer_id) = {
        let mut client = state.v2.inner.lock().await;
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
        let snapshot = client
            .snapshot
            .as_ref()
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
        let answer_id = state.v2.next_id("answer");
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
            answer: answer.to_owned(),
        };
        frame.validate().map_err(|error| error.to_string())?;
        client.pending_assistance_answer = Some(answer_id.clone());
        (frame, answer_id)
    };
    let encoded = serde_json::to_string(&frame)
        .map_err(|_| "could not encode protocol-v2 assistance answer".to_string())?;
    if let Err(error) = enqueue_encoded(&state, encoded).await {
        let mut client = state.v2.inner.lock().await;
        if client.pending_assistance_answer.as_deref() == Some(answer_id.as_str()) {
            client.pending_assistance_answer = None;
        }
        return Err(error);
    }
    Ok(V2AssistanceReceipt {
        request_id: request.request_id,
        answer_id,
    })
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
            return Err("a media lease return is already awaiting acknowledgement".into());
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
        client.pending_revoke = Some(PendingRevoke {
            request_id,
            lease_id: lease.claims.lease_id.clone(),
            lease_jti: lease.claims.jti.clone(),
            lease: lease.clone(),
            native_action_id,
            deadline: Instant::now() + REVOKE_ACK_TIMEOUT,
        });
        if is_native_return {
            client.pending_native_end = None;
        }
        (frame, lease)
    };

    // Local audio closes first. Until an exact gateway acknowledgement is
    // observed the lease remains fenced in pending_revoke, but can no longer
    // route microphone or caller audio locally.
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
            match prepare_offer_answer(state, LeaseMode::Takeover, Some(&action)).await {
                Ok((frame, request_id)) => serde_json::to_string(&frame)
                    .map(|encoded| {
                        Some(NativeOutbound {
                            encoded,
                            // Core-Telecom is completed only after the
                            // accepted offer advances to an actual lease request.
                            answer_action_id: None,
                            answer_request_id: Some(request_id),
                            revoke_request_id: None,
                        })
                    })
                    .map_err(|_| "could not encode native Answer lease request".to_string()),
                Err(message) => Err(message),
            }
        }
        NativeCallActionKind::End => match queue_native_end(state, action).await {
            Ok(()) => progress_native_end(app, state, media_state).await,
            Err(message) => Err(message),
        },
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
                revoke_request_id: Some(frame.request_id),
            }))
        }
    }
}

async fn expire_native_call_actions(app: &AppHandle, state: &V2State) {
    let (native_end, pending_revoke, pending_lease) = {
        let mut client = state.inner.lock().await;
        let now = Instant::now();
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
        (native_end, pending_revoke, pending_lease)
    };

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
    let (action_ids, pending_call) = {
        let mut client = state.inner.lock().await;
        let mut action_ids = Vec::new();
        if let Some(pending) = client.pending_native_end.take() {
            action_ids.push(pending.action.action_id);
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
        let pending_call = client
            .pending
            .as_ref()
            .map(|pending| (pending.call_id.clone(), pending.call_epoch));
        (action_ids, pending_call)
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
    if let Some((call_id, call_epoch)) = pending_call {
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
) -> Result<V2Transport, TransportOpenFailure> {
    let (channel, challenge) = crate::companion_relay::RelayChannel::connect(
        &plan.endpoints,
        &plan.access_token,
        app_id,
        device_id,
    )
    .await
    .map_err(|error| {
        if error.admission_rejected {
            TransportOpenFailure::AdmissionRejected
        } else {
            TransportOpenFailure::Unavailable(error.message)
        }
    })?;
    let mut shim = GatewayShim::new(
        app_id.to_owned(),
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
        let mut transport = open_relay_transport(&plan, state, app_id, device_id)
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
        endpoint_handshake(
            app,
            state,
            endpoint_identity,
            peer_trust,
            profile_id,
            app_id,
            device_id,
            session_nonce,
            Some(&admission.expected_peer_key_thumbprint),
            &mut transport,
        )
        .await
        .map_err(ManagedTransportRotationError::Transport)?;
        initial_sync(app, state, media_state, app_id, &mut transport)
            .await
            .map_err(ManagedTransportRotationError::Transport)?;
        return Ok(ManagedTransportRotation {
            transport,
            admission,
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
    endpoint_handshake(
        app,
        state,
        endpoint_identity,
        peer_trust,
        profile_id,
        app_id,
        device_id,
        session_nonce,
        Some(&admission.expected_peer_key_thumbprint),
        &mut transport,
    )
    .await
    .map_err(ManagedTransportRotationError::Transport)?;
    initial_sync(app, state, media_state, app_id, &mut transport)
        .await
        .map_err(ManagedTransportRotationError::Transport)?;
    Ok(ManagedTransportRotation {
        transport,
        admission,
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
                )
                .await;

            let opened = if let Some(plan) = attempt_relay.as_ref() {
                open_relay_transport(plan, &state, &config.app_id, &config.device_id).await
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
                        &mut transport,
                    )
                    .await;
                    if let Err(message) = handshake {
                        if config.managed_deployment_id.is_some()
                            && transient_managed_sync_failure(&message)
                        {
                            eprintln!(
                                "[AokieCompanion][realtime] managed session rotated during endpoint proof; retrying"
                            );
                        } else {
                            emit_error(&app, &message);
                        }
                    } else {
                        let synced =
                            initial_sync(&app, &state, &media_state, &config.app_id, &mut transport)
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
                                let mut websocket_heartbeat =
                                    transport.uses_websocket_heartbeat();

                                loop {
                                    tokio::select! {
                                        _ = &mut admission_refresh, if managed_deployment_id.is_some() => {
                                            eprintln!(
                                                "[AokieCompanion][realtime] opening overlapping managed admission before expiry"
                                            );
                                            match open_overlapping_managed_transport(
                                                &app,
                                                &state,
                                                &media_state,
                                                &managed_auth,
                                                &endpoint_identity,
                                                &peer_trust,
                                                &profile_id,
                                                managed_deployment_id.expect("managed rotation checked"),
                                                &config.app_id,
                                                &config.device_id,
                                                &session_nonce,
                                                &app_header,
                                                &device_header,
                                            ).await {
                                                Ok(rotation) => {
                                                    state
                                                        .rotate_admission_policy(
                                                            &rotation.admission.ice_servers,
                                                            rotation.admission.relay_only,
                                                        )
                                                        .await;
                                                    // The replacement inherits the
                                                    // predecessor's relay cursor and
                                                    // sequence before it reads anything,
                                                    // or it replays state the session
                                                    // has already handled.
                                                    let mut predecessor = std::mem::replace(
                                                        &mut transport,
                                                        rotation.transport,
                                                    );
                                                    transport.adopt_routing_from(&mut predecessor);
                                                    // A refreshed admission can
                                                    // change carrier. Left stale,
                                                    // a relay replacement would
                                                    // be pinged, never ponged,
                                                    // and time itself out.
                                                    websocket_heartbeat =
                                                        transport.uses_websocket_heartbeat();
                                                    // Dropped rather than closed, exactly
                                                    // as before: an explicit close frame
                                                    // here would be new behaviour on the
                                                    // rotation path a live call depends on.
                                                    drop(predecessor);
                                                    admission_refresh.as_mut().reset(
                                                        managed_admission_refresh_deadline(
                                                            rotation.admission.expires_at,
                                                        ),
                                                    );
                                                    freshness.as_mut().reset(
                                                        Instant::now() + INBOUND_FRESHNESS,
                                                    );
                                                    awaiting_pong = false;
                                                    ping.reset();
                                                    heartbeat.reset();
                                                    eprintln!(
                                                        "[AokieCompanion][realtime] managed admission rotated with media continuity preserved"
                                                    );
                                                }
                                                Err(ManagedTransportRotationError::Admission(error)) => {
                                                    emit_managed_admission_state(&app, &error);
                                                    eprintln!(
                                                        "[AokieCompanion][realtime] managed admission rotation retry: {error}"
                                                    );
                                                    admission_refresh.as_mut().reset(
                                                        Instant::now() + Duration::from_secs(1),
                                                    );
                                                }
                                                Err(ManagedTransportRotationError::Transport(message)) => {
                                                    eprintln!(
                                                        "[AokieCompanion][realtime] managed transport rotation retry: {message}"
                                                    );
                                                    admission_refresh.as_mut().reset(
                                                        Instant::now() + Duration::from_secs(1),
                                                    );
                                                }
                                            }
                                        }
                                        _ = &mut freshness => {
                                            emit_error(&app, "protocol-v2 inbound heartbeat timed out");
                                            break;
                                        }
                                        _ = &mut pong_deadline, if awaiting_pong => {
                                            emit_error(&app, "protocol-v2 pong timed out");
                                            break;
                                        }
                                        _ = ping.tick(), if !awaiting_pong && websocket_heartbeat => {
                                            if !transport.send_ping().await {
                                                break;
                                            }
                                            awaiting_pong = true;
                                            pong_deadline.as_mut().reset(Instant::now() + PONG_TIMEOUT);
                                        }
                                        _ = heartbeat.tick() => {
                                            match heartbeat_frame(&state, &config.app_id).await {
                                                Ok(Some(frame)) => {
                                                    if !transport.send_text(frame).await { break; }
                                                }
                                                Ok(None) => {}
                                                Err(message) => {
                                                    emit_error(&app, &message);
                                                    break;
                                                }
                                            }
                                        }
                                        _ = native_actions.tick() => {
                                            match poll_native_call_actions(&app, &state, &media_state).await {
                                                Ok(Some(native)) => {
                                                    let NativeOutbound {
                                                        encoded,
                                                        answer_action_id,
                                                        answer_request_id,
                                                        revoke_request_id,
                                                    } = native;
                                                    if transport.send_text(encoded).await {
                                                        if let Some(action_id) = answer_action_id {
                                                            if let Err(message) = crate::android_runtime::complete_native_call_action(
                                                                &app,
                                                                &action_id,
                                                                true,
                                                                "lease_request_sent",
                                                            ).await {
                                                                emit_error(&app, &message);
                                                            } else if let Some(request_id) = answer_request_id.as_deref() {
                                                                mark_native_answer_delivered(&state, request_id, &action_id).await;
                                                            }
                                                        }
                                                    } else {
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
                                                        break;
                                                    }
                                                }
                                                Ok(None) => {}
                                                Err(message) => emit_error(&app, &message),
                                            }
                                        }
                                        local = local_signals.recv() => {
                                            match local {
                                                Ok(signal) => match local_rtc_frame(&state, signal).await {
                                                    Ok(Some(frame)) => {
                                                        if !transport.send_text(frame).await { break; }
                                                    }
                                                    Ok(None) => {}
                                                    Err(message) => {
                                                        emit_error(&app, &message);
                                                        break;
                                                    }
                                                },
                                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                                    emit_error(&app, "native RTC signal capacity was exceeded");
                                                    break;
                                                }
                                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                            }
                                        }
                                        queued = outbound.recv() => {
                                            let Some(queued) = queued else { break; };
                                            if queued.generation != generation || !session_is_current(&connection, generation) {
                                                let _ = queued.completion.send(Err("realtime session changed before delivery".into()));
                                                break;
                                            }
                                            if transport.send_text(queued.encoded).await {
                                                let _ = queued.completion.send(Ok(()));
                                            } else {
                                                let _ = queued.completion.send(Err("protocol-v2 frame could not be delivered".into()));
                                                break;
                                            }
                                        }
                                        incoming = transport.recv(TRANSPORT_READ_TICK) => {
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
                                                        break;
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
                                                    break;
                                                }
                                                V2Inbound::Failed(message) => {
                                                    emit_error(&app, &message);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
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
    transport: &mut V2Transport,
) -> Result<(), String> {
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
    if !transport.send_text(encoded).await {
        return Err("endpoint-authenticated mobile hello could not be delivered".into());
    }
    state
        .set_peer_key_thumbprint(peer_key_thumbprint.to_owned())
        .await;
    Ok(())
}

async fn initial_sync(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    transport: &mut V2Transport,
) -> Result<(), String> {
    let deadline = Instant::now() + INITIAL_SYNC_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            if transport.is_relay() {
                // The expected outcome, and roster approval alone does NOT
                // change it. The plugin's relay routing is speak-first: it
                // learns a party only from an inbound frame (`learn_route`) and
                // broadcasts only to parties it has learned, so a Companion
                // that has posted nothing is not a destination at all — before
                // OR after approval. With outbound frames withheld (see
                // `RELAY_SEND_ENV`) silence is therefore the correct steady
                // state, not a fault. Said plainly here so it does not get
                // misread as a broken transport or a missing approval.
                eprintln!(
                    "[AokieCompanion][relay] no authoritative state arrived: the carrier is up, but the plugin only publishes to a party that has spoken to it, and this build posts nothing yet"
                );
            }
            return Err("protocol-v2 authoritative sync timed out".into());
        }
        match transport.recv(TRANSPORT_READ_TICK).await {
            V2Inbound::Text(text) => {
                let kind = parse_kind(&text)?;
                if kind != "snapshot" && kind != "idle_sync" {
                    return Err(
                        "protocol-v2 first gateway frame was not authoritative call state".into(),
                    );
                }
                handle_gateway_frame(app, state, media_state, expected_app_id, &text).await?;
                return Ok(());
            }
            V2Inbound::Idle | V2Inbound::Alive | V2Inbound::Pong => {}
            V2Inbound::Closed(_) => {
                return Err("protocol-v2 transport closed before authoritative sync".into());
            }
            V2Inbound::Failed(message) => return Err(message),
        }
    }
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
            for action_id in cleanup.native_action_ids {
                let _ = crate::android_runtime::complete_native_call_action(
                    app,
                    &action_id,
                    false,
                    "authoritative_call_idle",
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
            let frame: MobileSnapshotFrame = strict_parse(encoded, "snapshot")?;
            validate_snapshot(&frame, expected_app_id)?;
            let (assistance_cleared, stale_end_caller_request) = {
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
                let assistance_cleared = client.assistance.as_ref().is_some_and(|assistance| {
                    frame.snapshot.call_id != assistance.call_id
                        || frame.snapshot.call_epoch != assistance.call_epoch
                        || frame.snapshot.owner_epoch != assistance.owner_epoch
                        || frame.snapshot.switchboard_revision != assistance.switchboard_revision
                        || frame.snapshot.remote_revision != assistance.remote_revision
                        || !frame.snapshot.remote_consent.enabled
                        || !frame.snapshot.remote_consent.acknowledged
                        || !frame.snapshot.remote_consent.assistance_enabled
                });
                if assistance_cleared {
                    client.assistance = None;
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
                            || !frame.snapshot.remote_consent.allows(LeaseMode::Takeover))
                        .then(|| pending.request_id().to_owned())
                    });
                if stale_end_caller_request.is_some() {
                    client.pending_end_caller = None;
                }
                client.authoritative_sequence = frame.sequence;
                client.snapshot = Some(frame.clone());
                (assistance_cleared, stale_end_caller_request)
            };
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
                    if !snapshot.grants.contains(&Grant::AssistanceRead)
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
            let publish = {
                let mut client = state.inner.lock().await;
                if assistance_was_answered(&client, &frame.app_id, &frame.request_id) {
                    false
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
                    remember_answered_assistance(&mut client, &frame.app_id, &frame.request_id);
                    client.assistance = None;
                    client.pending_assistance_answer = None;
                    true
                }
            };
            if !publish {
                return Ok(());
            }
            app.emit("aokie-companion://v2-assistance-answered", frame)
                .map_err(|_| {
                    "could not deliver protocol-v2 assistance acknowledgement".to_string()
                })?;
            emit_assistance_reset(app);
        }
        "end_caller_challenge" => {
            let frame: EndCallerChallengeFrame = strict_parse(encoded, "caller-ending challenge")?;
            frame
                .validate(unix_now()?)
                .map_err(|error| error.to_string())?;
            if frame.app_id != expected_app_id {
                return Err("caller-ending challenge belongs to another application".into());
            }
            {
                let mut client = state.inner.lock().await;
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
            }
            // The nonce remains native-only. A stray WebView click cannot
            // manufacture or replay the second protocol operation.
            app.emit(
                "aokie-companion://v2-end-caller",
                EndCallerChallengeEvent::from(&frame),
            )
            .map_err(|_| "could not deliver caller-ending challenge".to_string())?;
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
            {
                let mut client = state.inner.lock().await;
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
        }
        "end_caller_result" => {
            let frame: PluginEndCallerResultFrame = strict_parse(encoded, "caller-ending result")?;
            frame.validate().map_err(|error| error.to_string())?;
            if frame.app_id != expected_app_id {
                return Err("caller-ending result belongs to another application".into());
            }
            let lease_to_close = {
                let mut client = state.inner.lock().await;
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
                if frame.outcome == EndCallerOutcome::Completed {
                    client.lease.take()
                } else {
                    None
                }
            };
            app.emit("aokie-companion://v2-end-caller", frame)
                .map_err(|_| "could not deliver caller-ending result".to_string())?;
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
        }
        "mobile_offer_accepted" => {
            let frame: MobileOfferAcceptedFrame =
                strict_parse(encoded, "mobile offer acknowledgement")?;
            apply_mobile_offer_accepted(state, expected_app_id, frame).await?;
        }
        "lease_granted" | "claim_provisional" | "claim_active" | "lease_renewed" => {
            let frame: LeaseStatusFrame = strict_parse(encoded, "lease status")?;
            apply_lease_status(app, state, media_state, expected_app_id, frame).await?;
        }
        "rtc_signal" => {
            let frame: GatewayRtcFrame = strict_parse(encoded, "RTC signal")?;
            apply_remote_rtc(app, state, media_state, expected_app_id, frame).await?;
        }
        "lease_revoked" => {
            let frame: LeaseRevokedFrame = strict_parse(encoded, "lease revoke")?;
            apply_revocation(app, state, media_state, expected_app_id, frame).await?;
        }
        "claim_rejected" => {
            let frame: ClaimRejectedFrame = strict_parse(encoded, "claim rejection")?;
            apply_claim_rejection(app, state, media_state, expected_app_id, frame).await?;
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
                    end_caller_failed = true;
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
    let mut client = state.inner.lock().await;
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
    Ok(())
}

async fn apply_lease_status(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    frame: LeaseStatusFrame,
) -> Result<(), String> {
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
    let now = unix_now()?;
    frame
        .lease
        .validate(now)
        .map_err(|error| error.to_string())?;

    let (operation, session, lease_event, reconcile_call) = {
        let mut client = state.inner.lock().await;
        validate_claim_identity(&client, &frame.lease)?;
        match frame.kind.as_str() {
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
                    token: frame.lease_token,
                    claims: frame.lease.clone(),
                    session: session.clone(),
                };
                client.pending = None;
                client.lease = Some(lease);
                (
                    LeaseOperation::Create,
                    session.clone(),
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
                client.lease = Some(ClientLease {
                    request_id: frame.request_id.clone(),
                    token: frame.lease_token,
                    claims: frame.lease.clone(),
                    session: session.clone(),
                });
                (
                    LeaseOperation::Create,
                    session.clone(),
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
                client.lease = Some(ClientLease {
                    request_id: current.request_id.clone(),
                    token: frame.lease_token,
                    claims: frame.lease.clone(),
                    session: session.clone(),
                });
                (
                    LeaseOperation::Renew,
                    session.clone(),
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
        }
    };

    match operation {
        LeaseOperation::Create => {
            media::create_offer(
                app,
                media_state,
                CreateOfferRequest {
                    session: session.clone(),
                    ice_servers: current_ice_servers(state).await,
                    relay_only: current_relay_only(state).await,
                },
            )
            .await?;
        }
        LeaseOperation::Renew => {
            media::renew_lease(
                app,
                media_state,
                SessionRequest {
                    session: session.clone(),
                },
            )
            .await?;
        }
    }
    if let Some((call_id, call_epoch)) = reconcile_call {
        crate::android_runtime::reconcile_offer(
            app,
            &call_id,
            call_epoch,
            "won",
            "authoritative_talk_lease",
        )
        .await?;
    }
    app.emit("aokie-companion://v2-lease", lease_event)
        .map_err(|_| "could not deliver protocol-v2 lease state".to_string())?;
    Ok(())
}

#[derive(Clone, Copy)]
enum LeaseOperation {
    Create,
    Renew,
}

async fn current_ice_servers(state: &V2State) -> Vec<IceServerConfig> {
    state.inner.lock().await.ice_servers.clone()
}

async fn current_relay_only(state: &V2State) -> bool {
    state.inner.lock().await.relay_only
}

async fn apply_remote_rtc(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    frame: GatewayRtcFrame,
) -> Result<(), String> {
    validate_common(
        &frame.kind,
        frame.schema_version,
        Some(&frame.app_id),
        expected_app_id,
    )?;
    validate_id(&frame.signal_id, "RTC signalId")?;
    let now = unix_now()?;
    let authentication = frame
        .signal
        .verify_endpoint_authentication(now)
        .map_err(|_| "Desktop RTC endpoint signature is invalid or stale".to_string())?
        .map(|authentication| {
            (
                authentication.endpoint_role(),
                authentication.holder_key_thumbprint().to_owned(),
                authentication.peer_key_thumbprint().to_owned(),
                authentication.jti().to_owned(),
            )
        });
    let lease = {
        let mut client = state.inner.lock().await;
        let lease = client
            .lease
            .as_ref()
            .ok_or("RTC signal arrived without a media lease")?
            .clone();
        if frame.plugin_id != lease.claims.plugin_id
            || frame.device_id != lease.claims.device_id
            || frame.lease_jti != lease.claims.jti
            || frame.rtc_session_id != lease.claims.rtc_session_id
            || frame.call_id != lease.claims.call_id
            || frame.call_epoch != lease.claims.call_epoch
            || frame.owner_epoch != lease.claims.owner_epoch
            || frame.fence != lease.claims.fence
            || frame.sdp_revision != lease.session.sdp_revision
            || frame.transport_generation != lease.session.transport_generation
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

    match frame.signal {
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
    }
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
    let (lease, local_media_already_closed, native_action_id) = {
        let mut client = state.inner.lock().await;
        if let Some(pending) = client.pending_revoke.as_ref() {
            validate_pending_revoke_ack(pending, &frame)?;
            let pending = client
                .pending_revoke
                .take()
                .expect("checked pending revoke");
            client.pending_end_caller = None;
            (pending.lease, true, pending.native_action_id)
        } else {
            let lease = client
                .lease
                .as_ref()
                .ok_or("lease revocation did not match a local lease")?;
            if lease.claims.lease_id != frame.lease_id || lease.claims.jti != frame.lease_jti {
                return Err("stale lease revocation did not match the current JTI".into());
            }
            client.pending_end_caller = None;
            (client.lease.take().expect("checked lease"), false, None)
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
    emit_lease_reset(app);
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
    if frame.request_id.as_deref() != Some(pending.request_id.as_str())
        || frame.lease_id != pending.lease_id
        || frame.lease_jti != pending.lease_jti
    {
        return Err("lease return acknowledgement crossed its request or JTI fence".into());
    }
    Ok(())
}

async fn apply_claim_rejection(
    app: &AppHandle,
    state: &V2State,
    media_state: &NativeMediaState,
    expected_app_id: &str,
    frame: ClaimRejectedFrame,
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
    let (lease, rejected_call) = {
        let mut client = state.inner.lock().await;
        if client
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request_id == frame.request_id)
        {
            let pending = client.pending.take().expect("checked pending claim");
            (None, Some((pending.call_id, pending.call_epoch)))
        } else if client.lease.as_ref().is_some_and(|lease| {
            lease.request_id == frame.request_id
                && matches!(lease.claims.phase, LeasePhase::Prepared)
        }) {
            let lease = client.lease.take().expect("checked provisional lease");
            let rejected_call = Some((lease.claims.call_id.clone(), lease.claims.call_epoch));
            (Some(lease), rejected_call)
        } else {
            return Err("claim rejection did not match pending authority".into());
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
    Ok(())
}

async fn heartbeat_frame(state: &V2State, app_id: &str) -> Result<Option<String>, String> {
    let (token, device_id) = {
        let client = state.inner.lock().await;
        let Some(lease) = &client.lease else {
            return Ok(None);
        };
        let now = unix_now()?;
        if !lease_heartbeat_due(lease.claims.expires_at, now)? {
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

fn lease_heartbeat_due(expires_at: u64, now: u64) -> Result<bool, String> {
    if expires_at <= now {
        return Err("protocol-v2 media lease expired before renewal".into());
    }
    Ok(expires_at.saturating_sub(now) <= LEASE_RENEWAL_WINDOW.as_secs())
}

async fn local_rtc_frame(
    state: &V2State,
    event: MediaSignalEvent,
) -> Result<Option<String>, String> {
    let (lease, endpoint_identity, endpoint_session_nonce) = {
        let client = state.inner.lock().await;
        let Some(lease) = &client.lease else {
            return Ok(None);
        };
        if event.session != lease.session {
            // Events from a replaced peer are stale and are deliberately
            // dropped rather than being rebound to newer authority.
            return Ok(None);
        }
        if lease.claims.expires_at <= unix_now()? {
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
    let now = unix_now()?;
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
    let mut native_action_ids = Vec::new();
    if let Some(action_id) = client
        .pending
        .take()
        .and_then(|pending| pending.native_action_id)
    {
        native_action_ids.push(action_id);
    }
    if let Some(action_id) = client
        .pending_revoke
        .take()
        .and_then(|pending| pending.native_action_id)
    {
        native_action_ids.push(action_id);
    }
    if let Some(action_id) = client
        .pending_native_end
        .take()
        .map(|pending| pending.action.action_id)
    {
        native_action_ids.push(action_id);
    }
    native_action_ids.sort();
    native_action_ids.dedup();

    client.authoritative_sequence = frame.sequence;
    client.snapshot = None;
    client.lease = None;
    client.assistance = None;
    client.pending_assistance_answer = None;
    client.pending_end_caller = None;
    client.seen_remote_endpoint_jtis.clear();

    Ok(Some(IdleStateCleanup {
        native_action_ids,
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
    if let Some(captions) = &snapshot.captions {
        if !snapshot.remote_consent.enabled
            || !snapshot.remote_consent.acknowledged
            || !snapshot.remote_consent.captions_enabled
        {
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
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, writer.send(message)).await,
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
        let active =
            session_from_claims(&claims(LeaseMode::Consult, LeasePhase::Active), 2, 2).unwrap();
        assert_eq!(active.binding.mode, MediaMode::Consult);
        assert!(active.binding.mode.needs_microphone());
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
        assert!(!lease_heartbeat_due(now + 20, now).unwrap());
        assert!(!lease_heartbeat_due(now + 15, now).unwrap());
        assert!(lease_heartbeat_due(now + 14, now).unwrap());
        assert!(lease_heartbeat_due(now + 1, now).unwrap());
        assert!(lease_heartbeat_due(now, now).is_err());
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
            audio_levels: None,
            occurred_at: Utc::now().to_rfc3339(),
        }
    }

    fn verified_shim() -> GatewayShim {
        let mut shim = GatewayShim::new(
            "app_a".into(),
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
        // Neither is something the plugin publishes, and inventing either would
        // assert state no endpoint authored.
        assert!(frame.snapshot.participants.is_empty());
        assert!(frame.snapshot.pending_mobile_offers.is_empty());
        assert_eq!(frame.grants, vec![Grant::StateRead, Grant::Monitor]);
    }

    /// ⚠️ THE safety constraint of the relay carrier.
    ///
    /// The live plugin's dispatcher accepts eight gateway-dialect kinds and
    /// ends in `_ => Err(reconnect("unsupported frame"))`; its relay carrier
    /// admits any frame from an approved roster member. Over the socket a
    /// gateway translated the mobile dialect into that one, and on the relay
    /// nothing does — so once this Companion is approved, ANY frame it posts
    /// tears down the session of a desktop answering a real phone line, and
    /// does it again on every retry. Nothing goes out until the plugin ships
    /// handlers.
    #[test]
    fn the_relay_withholds_every_outbound_frame_unless_explicitly_opted_in() {
        assert!(
            !relay_send_opt_in(),
            "posting to the plugin must never be the default"
        );

        assert!(relay_send_opt_in_from(Some("1")));
        for refused in [None, Some(""), Some("0"), Some("true"), Some("yes"), Some(" 1")] {
            assert!(
                !relay_send_opt_in_from(refused),
                "{refused:?} must not read as consent"
            );
        }
    }

    /// Every kind this session can emit is fatal to today's plugin — by the
    /// catch-all arm, or by a `deny_unknown_fields` twin that does not match.
    /// Gating only `mobile_hello` would have left the rest of them live, which
    /// is worse than the hello: a hello fires once per session, whereas
    /// `end_caller_confirm` fires when a user taps a button mid-call.
    #[test]
    fn no_frame_this_session_emits_is_one_the_plugin_can_accept() {
        // The plugin's dispatcher arms, verbatim.
        const PLUGIN_ACCEPTS: [&str; 8] = [
            "claim_proposal",
            "lease_granted",
            "lease_renewed",
            "lease_revoked",
            "rtc_signal",
            "assistance_answer",
            "end_caller_execute",
            "error",
        ];
        // Kinds this build posts, from the encode sites in this module.
        const MOBILE_EMITS: [&str; 6] = [
            "mobile_hello",
            "lease_request",
            "lease_heartbeat",
            "end_caller_challenge_request",
            "end_caller_confirm",
            "assistance_answer",
        ];

        for kind in MOBILE_EMITS {
            if PLUGIN_ACCEPTS.contains(&kind) {
                // The one overlapping name still fails: the twins are
                // `deny_unknown_fields` and disagree on their members, so the
                // plugin's parse — not its dispatch — is what tears down.
                assert_eq!(kind, "assistance_answer");
                let mobile = serde_json::to_string(&MobileAssistanceAnswerFrame {
                    kind: kind.into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: "app_a".into(),
                    request_id: "request_a".into(),
                    idempotency_key: "mobile:device_a:request_a".into(),
                    answer_id: "answer_a".into(),
                    call_id: "call_a".into(),
                    call_epoch: 1,
                    owner_epoch: 0,
                    switchboard_revision: 1,
                    remote_revision: 1,
                    answer: "yes".into(),
                })
                .expect("fixture encodes");
                assert!(
                    serde_json::from_str::<aokie_protocol::v2::PluginAssistanceAnswerFrame>(
                        &mobile
                    )
                    .is_err(),
                    "the mobile assistance answer must not silently parse as the plugin's twin"
                );
            }
        }

        // And the rtc signal the local media path emits, for the same reason.
        assert!(
            serde_json::from_str::<aokie_protocol::v2::PluginRtcSignalFrame>(
                "{\"kind\":\"rtc_signal\",\"schemaVersion\":1,\"appId\":\"app_a\",\"signalId\":\"s\",\"pluginId\":\"aokie\",\"deviceId\":\"d\",\"leaseToken\":\"t\",\"leaseJti\":\"j\",\"rtcSessionId\":\"r\",\"sdpRevision\":1}"
            )
            .is_err(),
            "leaseToken has no home in the plugin's twin"
        );
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
                vec![Grant::StateRead],
                desktop.thumbprint().into(),
            )
        };

        // ⚠️ Each case gets its OWN app id. Proof is remembered per
        // `<app_id>/<thumbprint>` for the life of the process (see
        // `proven_peers`), so sharing one id here would let the successful case
        // pre-verify the refusal cases and assert nothing.

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

    /// ⚠️ The plugin greets a party exactly ONCE per plugin session — its
    /// `greeted` set is cleared when it arms a rotated hello, never when this
    /// Companion reconnects. So if peer proof were scoped to one carrier, a
    /// Companion that verified and then lost its stream would never be greeted
    /// again, and `peer_gate` would drop every snapshot for the rest of the
    /// plugin's session: a permanent read deadlock, not a slow recovery. Proof
    /// therefore survives the carrier.
    #[test]
    fn a_proven_desktop_stays_proven_across_a_companion_reconnect() {
        let desktop = crate::endpoint_identity::EndpointIdentity::from_secret([21; 32])
            .expect("test identity");
        let shim_for = || {
            GatewayShim::new(
                "app_reconnect".into(),
                vec![Grant::StateRead],
                desktop.thumbprint().into(),
            )
        };

        let mut first = shim_for();
        assert!(!first.peer_verified, "nothing is trusted before a proof");
        first
            .translate(&signed_plugin_hello(&desktop, "app_reconnect"))
            .expect("the real Desktop proves itself");

        // A brand new carrier — the reconnect the plugin will not greet again.
        let mut reconnected = shim_for();
        assert!(
            reconnected.peer_verified,
            "a Desktop that already proved this key must not have to prove it again"
        );
        assert!(
            reconnected
                .translate(&plugin_snapshot_frame_for("app_reconnect"))
                .expect("translates")
                .is_some(),
            "authoritative state must flow after a reconnect, or the read path is dead"
        );

        // The memory is bound to app AND key: it never vouches for anyone else.
        let other_app = GatewayShim::new(
            "app_reconnect_other".into(),
            vec![Grant::StateRead],
            desktop.thumbprint().into(),
        );
        assert!(!other_app.peer_verified);
        let other_key = GatewayShim::new(
            "app_reconnect".into(),
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

    /// [`proven_peers`] is keyed `<app_id>/<thumbprint>` precisely so proof can
    /// never transfer to a different Desktop key. Carrying `peer_verified`
    /// across a rotation unconditionally would launder it around that key: a
    /// re-pinned admission would project authoritative state without the NEW
    /// key ever proving possession.
    #[test]
    fn a_rotation_that_repins_the_desktop_key_must_see_it_prove_itself_again() {
        let shim_with_pin = |pin: &str| {
            GatewayShim::new(
                "app_rotation_pin".into(),
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
    #[test]
    fn frames_outside_the_translated_subset_are_dropped_rather_than_erroring() {
        let mut shim = verified_shim();

        for untranslated in [
            "{\"kind\":\"claim_decision\"}",
            "{\"kind\":\"plugin_lease_revoke\"}",
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
            grants: vec![Grant::StateRead, Grant::RtcSignal, mode_grant],
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
            schema_version: 1,
            action_id: "native_answer_a".into(),
            kind: NativeCallActionKind::Answer,
            offer_id: "offer_exact_a".into(),
            app_id: "app_a".into(),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch: 4,
            created_at: unix_now().unwrap(),
        }
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

        apply_mobile_offer_accepted(
            &state,
            "app_a",
            MobileOfferAcceptedFrame {
                kind: "mobile_offer_accepted".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                request_id: answer.request_id,
                offer_id: answer.offer_id,
                offer_jti: answer.offer_jti,
                offered_mode: LeaseMode::Consult,
                accepted: true,
            },
        )
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
        assert!(apply_mobile_offer_accepted(&state, "app_a", wrong)
            .await
            .is_err());
        assert!(take_ready_lease_request(&state).await.unwrap().is_none());
    }

    #[test]
    fn lease_return_ack_requires_exact_request_lease_and_jti() {
        let lease_claims = claims(LeaseMode::Takeover, LeasePhase::Active);
        let pending = PendingRevoke {
            request_id: "return_request_a".into(),
            lease_id: lease_claims.lease_id.clone(),
            lease_jti: lease_claims.jti.clone(),
            lease: ClientLease {
                request_id: "claim_request_a".into(),
                token: "v2.redacted.signature".into(),
                session: session_from_claims(&lease_claims, 1, 1).unwrap(),
                claims: lease_claims,
            },
            native_action_id: Some("native_end_a".into()),
            deadline: Instant::now() + REVOKE_ACK_TIMEOUT,
        };
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
        assert!(validate_pending_revoke_ack(&pending, &wrong).is_err());
        wrong = exact.clone();
        wrong.lease_jti = "lease_other".into();
        assert!(validate_pending_revoke_ack(&pending, &wrong).is_err());
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
