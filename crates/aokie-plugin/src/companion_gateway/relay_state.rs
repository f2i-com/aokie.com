//! Relay-session state types: peers, leases, minted offers, takeovers.

#[allow(unused_imports)]
use super::*;

pub(super) type GatewaySocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(super) struct PeerRoute {
    pub(super) binding: SessionBinding,
    pub(super) lease_jti: String,
    pub(super) device_id: String,
    pub(super) sdp_revision: u64,
    pub(super) transport_generation: u64,
    pub(super) lease_ttl_ms: u64,
    pub(super) connected: bool,
    pub(super) remote_audio_ready: bool,
    pub(super) remote_microphone_ready: bool,
    pub(super) transition_requested: bool,
}

pub(super) enum OutboundRtcSignal {
    Answer(String),
    Ice {
        candidate: String,
        sdp_mid: String,
        sdp_m_line_index: u16,
    },
    IceComplete,
}

pub(super) struct PreparedTakeover {
    pub(super) request_id: String,
    pub(super) provisional: LeaseClaims,
    /// Relay offers are minted against the physical switchboard revision.
    /// Socket leases predate that dialect and leave this unset.
    pub(super) expected_switchboard_revision: Option<u64>,
    pub(super) confirmed_owner_epoch: Option<u64>,
    pub(super) provisional_sdp_revision: u64,
    pub(super) provisional_transport_generation: u64,
    pub(super) decision_sent: bool,
    /// Non-renewable bound between delivery of the active lease and opening
    /// its replacement peer. Heartbeats may extend the lease itself, but can
    /// never monopolize the claimant slot (or, for Consult, suppress Aokie)
    /// without a usable media path.
    pub(super) active_rebind_deadline: Option<Instant>,
}

/// A Companion that proved itself over the relay, remembered by device.
///
/// The two recorded facts come from the VERIFIED hello proof, never from a
/// self-asserted field: the thumbprint the signature actually proved, and the
/// mobile's own session nonce. The nonce is not bookkeeping — every later RTC
/// signal is checked against `LeaseClaims::session_nonce`, so a lease minted
/// with the wrong one silently rejects every signal the device sends.
#[derive(Clone)]
pub(super) struct RelayPeer {
    pub(super) holder_key_thumbprint: String,
    pub(super) session_nonce: String,
    /// Exact server-authenticated admission scopes carried by this peer's
    /// verified hello. Payload fields never contribute authority.
    pub(super) grants: HashSet<Grant>,
}

/// An offer this plugin minted, kept so a device cannot redeem one it invented.
#[derive(Clone)]
pub(super) struct MintedOffer {
    pub(super) claims: PendingMobileOfferClaims,
    pub(super) token: String,
    /// Set when the device answered it. An accepted offer is never replaced by
    /// a fresher mint, because the device is mid-redemption against this exact
    /// `offerId`.
    pub(super) accepted: bool,
}

/// A lease this plugin minted, kept so a device cannot present one it invented.
///
/// Only what is needed to recognise the lease later and address its holder; the
/// claims themselves live in the session's own lease book, which stays the
/// authority on whether a lease is still alive.
#[derive(Clone)]
pub(super) struct RelayLease {
    pub(super) lease_id: String,
    pub(super) device_id: String,
    pub(super) request_id: String,
    pub(super) token: String,
    pub(super) current_jti: String,
    pub(super) phase: LeasePhase,
    pub(super) status: PluginLeaseStatus,
    pub(super) mode: LeaseMode,
}

/// The exact receive-only RTC binding that was superseded when a PREPARED
/// consult/takeover became ACTIVE.
///
/// Relay delivery is at-least-once and trickled ICE can overtake the ACTIVE
/// lease status.  Keeping this binding briefly lets the plugin recognise that
/// exact, already-retired generation and discard it without presenting it to
/// the native peer.  It is deliberately not a lease alias: only RTC signalling
/// consults it, every binding field and the bearer token must match, and it can
/// never renew, revoke, route media, or mutate caller ownership.
#[derive(Clone)]
pub(super) struct RetiredPreparedRtcBinding {
    pub(super) claims: LeaseClaims,
    pub(super) token: String,
    pub(super) sdp_revision: u64,
    pub(super) transport_generation: u64,
    pub(super) expires_at: Instant,
}

/// A consult/takeover claim that has been minted but deliberately NOT armed.
///
/// See [`GatewaySession::finish_relay_delivery`] for why the arming waits.
pub(super) struct DeferredPrepare {
    pub(super) encoded: String,
    pub(super) notice: LeaseNotice,
    pub(super) expected_switchboard_revision: u64,
    pub(super) lease_id: String,
    pub(super) device_id: String,
    pub(super) request_id: String,
    pub(super) offer: MintedOffer,
    pub(super) replay_key: String,
}

/// A relay lease-status transition waiting for the carrier's delivery result.
///
/// Authority is either committed on `Delivered` or rolled back on `Dropped`;
/// HTTP backpressure therefore stays non-fatal without becoming permission to
/// extend or arm authority the device never learned about.
pub(super) enum PendingRelayStatus {
    MonitorGrant {
        encoded: String,
        notice: LeaseNotice,
        lease_id: String,
        offer_id: String,
        offer: MintedOffer,
        replay_key: String,
    },
    Renewal {
        encoded: String,
        notice: LeaseNotice,
        lease_id: String,
        replay_key: String,
    },
    Active {
        encoded: String,
        lease_id: String,
        claims: LeaseClaims,
        token: String,
    },
}

impl PendingRelayStatus {
    pub(super) fn encoded(&self) -> &str {
        match self {
            Self::MonitorGrant { encoded, .. }
            | Self::Renewal { encoded, .. }
            | Self::Active { encoded, .. } => encoded,
        }
    }

    pub(super) fn lease_id(&self) -> &str {
        match self {
            Self::MonitorGrant { lease_id, .. }
            | Self::Renewal { lease_id, .. }
            | Self::Active { lease_id, .. } => lease_id,
        }
    }
}

#[derive(Clone)]
pub(super) enum RelayReplayResult {
    OfferAccepted {
        encoded: String,
        offer_id: String,
        mode: LeaseMode,
        required_grants: Vec<Grant>,
    },
    LeaseStatus {
        lease_id: String,
    },
    /// A terminal claim decision after its one-shot offer was consumed.
    /// Remembering the exact encoded refusal is what makes relay delivery
    /// retries converge instead of falling through to a now-missing offer.
    Rejected {
        encoded: String,
    },
    /// A native RTC failure both rejects the provoking signal and revokes the
    /// exact lease. Relay egress posts these frames separately, so retries must
    /// reproduce both byte-for-byte without executing the teardown twice.
    TerminalRtcFailure {
        revocation: String,
        rejection: String,
    },
    RtcAccepted {
        mode: LeaseMode,
    },
    /// A valid signal for the just-retired PREPARED generation was consumed as
    /// a no-op.  Its replay lifetime is the tombstone lifetime, not the normal
    /// two-minute operation ledger lifetime.
    RetiredPreparedRtcDropped {
        lease_id: String,
        mode: LeaseMode,
        expires_at: Instant,
    },
    HeartbeatStatus {
        lease_id: String,
    },
    /// An operation whose successful response is already completely encoded.
    /// The required grants are rechecked on every replay so a narrower fresh
    /// admission cannot use an old acknowledgement to preserve authority.
    DirectResponse {
        encoded: String,
        required_grants: Vec<Grant>,
    },
}

#[derive(Clone)]
pub(super) struct RelayReplay {
    pub(super) fingerprint: String,
    pub(super) device_id: String,
    pub(super) result: RelayReplayResult,
    pub(super) seen_at: Instant,
}

/// One exact terminal lease notice still owed to a relay Companion.
///
/// The lease has already been retired locally; this is delivery bookkeeping
/// only and must never execute that teardown again.  `encoded` is retained
/// byte-for-byte because the mobile's completed-revocation tombstone makes an
/// exact duplicate safe, while synthesising a later notice could change the
/// JTI/fence that proves which authority ended.
pub(super) struct PendingRelayRevocation {
    pub(super) encoded: String,
    pub(super) device_id: String,
    pub(super) registered_at: Instant,
    pub(super) next_attempt_at: Instant,
    pub(super) expires_at: Instant,
}

/// Relay peers remembered per session. One owner rarely approves more.
pub(super) const MAX_RELAY_PEERS: usize = 16;
/// Terminal notices may briefly outlive the leases/peers they retire. Keep
/// enough room for every admitted peer plus overlap, but never let a broken
/// relay grow an unbounded egress ledger.
pub(super) const MAX_PENDING_RELAY_REVOCATIONS: usize = MAX_RELAY_PEERS * 2;
/// Relay delivery has its own bounded retry/backoff. Sending only the oldest
/// debt per gateway tick prevents a full ledger from delaying lease expiry,
/// heartbeats and caller-state reconciliation for many seconds.
pub(super) const MAX_PENDING_RELAY_REVOCATIONS_PER_POLL: usize = 1;
/// Companion lease expiry (or a fresh-session snapshot after reconnect) is
/// the bounded fallback. Exact revokes get a retry window long enough to cross
/// transient relay backpressure, without posting four times a second forever
/// to an unavailable phone.
pub(super) const PENDING_RELAY_REVOCATION_TTL: Duration = Duration::from_secs(30);
/// Live minted offers held at once: at most one published per (device, mode),
/// plus superseded ones kept until expiry so an in-flight answer still resolves.
pub(super) const MAX_RELAY_OFFERS: usize = 32;
/// Concurrent plugin-minted leases. `claimant_busy` keeps the real number at 1;
/// the cap is what makes that structural.
pub(super) const MAX_RELAY_LEASES: usize = 4;
/// Lifetime of a minted offer. Under [`MOBILE_OFFER_MAX_LIFETIME`], and long
/// enough that an offer published at the slowest snapshot cadence is still
/// answerable when it arrives.
pub(super) const RELAY_OFFER_TTL: u64 = 25;
/// Remaining offer life below which a fresh one is minted for that (device,
/// mode). Keeps every published offer comfortably answerable.
pub(super) const RELAY_OFFER_REFRESH_MARGIN: u64 = 10;
/// Lifetime of a prepared consult/takeover lease.
///
/// Deliberately short AND non-renewable: a prepare that cannot complete inside
/// this window must hand the caller back to the AI, and refusing renewal is
/// what makes a stuck prepare impossible to keep alive. This is the largest
/// caller-visible dead-air window the relay path can produce.
pub(super) const RELAY_PREPARED_LEASE_TTL: u64 = 15;
/// Lifetime of an active lease, extended by each heartbeat. Far inside the
/// 300-second local safety cap.
pub(super) const RELAY_ACTIVE_LEASE_TTL: u64 = 20;
/// A prepared takeover leaves Aokie serving the caller, so the first-use
/// microphone permission prompt gets a humane window without risking silence.
pub(super) const RELAY_TAKEOVER_ACTIVE_REBIND_TIMEOUT: Duration = Duration::from_secs(30);
/// Private consult deliberately holds Aokie away from the caller. Bound the
/// active-peer rebind much more tightly so a missing offer cannot become an
/// indefinitely renewable silent hold.
pub(super) const RELAY_CONSULT_ACTIVE_REBIND_TIMEOUT: Duration = Duration::from_secs(8);
/// Rolling window and budget for control requests, per device.
pub(super) const RELAY_REQUEST_WINDOW: Duration = Duration::from_secs(10);
pub(super) const RELAY_REQUEST_BUDGET: u32 = 10;
/// RTC trickle is naturally bursty (offer plus a set of gathered candidates),
/// so it has an independent bounded lane.  Sharing the ten-request control
/// lane made one healthy offer + lease + candidate burst deterministically
/// throttle its own ACTIVE rebind.
pub(super) const RELAY_RTC_SIGNAL_BUDGET: u32 = 32;
/// At-least-once relay ordering can deliver the final PREPARED candidates just
/// after the ACTIVE token/JTI rotation.  Ten seconds covers that reordering
/// without turning the old bearer into long-lived authority.
pub(super) const RETIRED_PREPARED_RTC_TTL: Duration = Duration::from_secs(10);
/// How long a replay result is remembered, and how many at once.
pub(super) const RELAY_REPLAY_TTL: Duration = Duration::from_secs(120);
pub(super) const MAX_RELAY_REPLAYS: usize = 256;
/// Caller ending deliberately requires a second, short-lived, one-use
/// confirmation.  The relay is only a carrier; the plugin that owns the
/// physical radio mints and consumes this nonce itself.
pub(super) const RELAY_END_CALLER_CONFIRM_TTL: u64 = 12;
pub(super) const MAX_RELAY_END_CALLER_CHALLENGES: usize = MAX_RELAY_PEERS;
pub(super) const MAX_USED_RELAY_END_CALLER_CONFIRMATIONS: usize = 64;
