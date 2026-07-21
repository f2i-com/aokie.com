//! `GatewaySession`: the per-connection state bundle.

#[allow(unused_imports)]
use super::*;

pub(super) struct GatewaySession {
    pub(super) app_id: String,
    pub(super) plugin_id: String,
    pub(super) plugin_session_nonce: String,
    pub(super) endpoint_authority: Arc<EndpointAuthority>,
    /// The process mailbox in production, injected as an isolated clone in
    /// tests so the offer/decline decision CAS can be exercised deterministically.
    pub(super) assistance: crate::assistance::AssistanceBroker,
    pub(super) used_endpoint_jtis: HashMap<String, u64>,
    pub(super) ice_servers: Vec<IceServerConfig>,
    pub(super) relay_only: bool,
    pub(super) leases: HashMap<String, LeaseClaims>,
    pub(super) peers: HashMap<String, PeerRoute>,
    pub(super) prepared: Option<PreparedTakeover>,
    pub(super) last_snapshot_fingerprint: Option<String>,
    pub(super) last_snapshot_sent: Option<Instant>,
    pub(super) authoritative_idle: bool,
    pub(super) next_snapshot_poll: Instant,
    pub(super) last_assistance_request_sent: Option<String>,
    pub(super) relay_snapshot_event_id: Option<String>,
    pub(super) relay_snapshot_delivered_devices: HashSet<String>,
    pub(super) pending_end_caller: HashMap<String, PendingEndCaller>,
    pub(super) accepted_transfers: HashMap<String, AcceptedTransferLease>,
    pub(super) relay_end_caller_challenges: HashMap<String, RelayEndCallerChallenge>,
    pub(super) used_relay_end_caller_confirmations: HashSet<String>,
    pub(super) used_relay_end_caller_order: VecDeque<String>,
    /// Unhandled relay frame kinds already reported, so a Companion emitting one
    /// on a timer cannot wrap the bounded log ring during a call. Bounded, and
    /// keyed by kind so a genuinely NEW kind is still surfaced once.
    pub(super) dropped_relay_kinds: HashSet<String>,
    /// When a refused relay hello was last reported, and how many refusals were
    /// held back since. Every refusal is a security event worth recording, and
    /// an approved-yet-misbehaving device retries on a timer, so refusals are
    /// RATE-limited rather than capped: bounded tightly enough that they cannot
    /// wrap the log ring during a call, but never permanently silent.
    pub(super) relay_hello_rejection_logged_at: Option<Instant>,
    pub(super) relay_hello_rejections_suppressed: u32,
    /// A relay party that must be greeted again before the next publish,
    /// carried from [`Self::accept_mobile_hello`] out to the carrier that owns
    /// the greeting book and can fetch a fresh challenge. Set only for a hello
    /// whose proof VERIFIED, so an unauthenticated frame can never provoke an
    /// extra signed hello.
    pub(super) relay_regreet_party: Option<String>,
    /// A device route proved by the hello currently being handled.
    ///
    /// The carrier consumes this immediately after the session returns. Route
    /// ownership must never be learned from a frame's self-asserted `deviceId`:
    /// only the hello proof binds a device to an approved endpoint party.
    pub(super) relay_verified_route: Option<(String, String)>,

    // --- Relay lease authority -------------------------------------------
    //
    // On the socket a trusted gateway minted, signed and fenced every lease,
    // and this plugin only ever CONSUMED the result — which is why
    // `validate_notice` checks identity and epochs but never the lease token.
    // The relay has no such authority, so the plugin takes the role itself:
    // it mints offers and leases from live radio truth and remembers exactly
    // what it minted. That is what replaces the missing signature check. A
    // peer can then only ever ask; anything it did not receive from us here
    // is unrecognised, and refusing is free.
    /// True once per session when the feature is enabled. Read from the
    /// environment at construction so the whole path can be turned off on a
    /// running install without a rebuild.
    pub(super) relay_authority_enabled: bool,
    /// Whether the CURRENT transport is the relay, refreshed each loop turn.
    /// Offers are published and claim decisions are self-consumed only here.
    pub(super) relay_carrier: bool,
    pub(super) relay_peers: HashMap<String, RelayPeer>,
    pub(super) relay_offers: HashMap<String, MintedOffer>,
    pub(super) relay_offer_winners: HashMap<String, String>,
    /// Transfer offer removed for lease validation but not yet installed as a
    /// delivery-gated provisional claim. Any terminal validation exit drains
    /// this exact entry and releases the AssistanceBroker reservation.
    pub(super) relay_redeeming_transfer_offers: HashMap<String, MintedOffer>,
    pub(super) relay_leases: HashMap<String, RelayLease>,
    pub(super) deferred_prepare: Option<DeferredPrepare>,
    pub(super) pending_relay_status: Option<PendingRelayStatus>,
    pub(super) pending_relay_revocations: HashMap<String, PendingRelayRevocation>,
    /// Strictly increasing per session, so a replayed older takeover fence can
    /// never look current.
    pub(super) next_takeover_fence: u64,
    pub(super) relay_request_budget: HashMap<String, (Instant, u32)>,
    pub(super) relay_rtc_signal_budget: HashMap<String, (Instant, u32)>,
    pub(super) retired_prepared_rtc: HashMap<String, RetiredPreparedRtcBinding>,
    pub(super) relay_replays: HashMap<String, RelayReplay>,
}

/// Distinct unhandled relay kinds reported per session.
pub(super) const MAX_REPORTED_RELAY_KINDS: usize = 16;
/// Shortest gap between two reported relay-hello refusals. Long enough that a
/// device retrying on a timer cannot crowd the log ring during a call, short
/// enough that a systematic refusal stays visible for as long as it persists.
pub(super) const RELAY_HELLO_REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(60);

pub(super) struct PendingEndCaller {
    pub(super) execute: PluginEndCallerExecuteFrame,
    pub(super) result_rx: std::sync::mpsc::Receiver<Result<(), CompanionEndCallerFailure>>,
}

#[derive(Clone)]
pub(super) struct AcceptedTransferLease {
    pub(super) request_id: String,
    pub(super) offered_fence: crate::assistance::AssistanceCallFence,
    pub(super) device_id: String,
    pub(super) setup_expires_at: u64,
    pub(super) failback_requested: bool,
}

#[derive(Clone)]
pub(super) struct RelayEndCallerChallenge {
    pub(super) frame: EndCallerChallengeFrame,
    pub(super) lease_jti: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RelayAssistanceAnswerAcceptedFrame {
    pub(super) kind: &'static str,
    pub(super) schema_version: u16,
    pub(super) app_id: String,
    pub(super) request_id: String,
    pub(super) answer_id: String,
    pub(super) accepted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RelayEndCallerSubmittedFrame {
    pub(super) kind: &'static str,
    pub(super) schema_version: u16,
    pub(super) app_id: String,
    pub(super) request_id: String,
    pub(super) operation_id: String,
    pub(super) confirmation_id: String,
    pub(super) accepted: bool,
}

impl GatewaySession {
    pub(super) fn new(credentials: &SessionCredentials, plugin_session_nonce: String) -> Self {
        Self {
            app_id: credentials.app_id.clone(),
            plugin_id: credentials.plugin_id.clone(),
            plugin_session_nonce,
            endpoint_authority: credentials.endpoint_authority.clone(),
            assistance: crate::assistance::global().clone(),
            used_endpoint_jtis: HashMap::new(),
            ice_servers: credentials.ice_servers.clone(),
            relay_only: credentials.relay_only,
            leases: HashMap::new(),
            peers: HashMap::new(),
            prepared: None,
            last_snapshot_fingerprint: None,
            last_snapshot_sent: None,
            authoritative_idle: false,
            next_snapshot_poll: Instant::now(),
            last_assistance_request_sent: None,
            relay_snapshot_event_id: None,
            relay_snapshot_delivered_devices: HashSet::new(),
            pending_end_caller: HashMap::new(),
            accepted_transfers: HashMap::new(),
            relay_end_caller_challenges: HashMap::new(),
            used_relay_end_caller_confirmations: HashSet::new(),
            used_relay_end_caller_order: VecDeque::new(),
            dropped_relay_kinds: HashSet::new(),
            relay_hello_rejection_logged_at: None,
            relay_hello_rejections_suppressed: 0,
            relay_regreet_party: None,
            relay_verified_route: None,
            relay_authority_enabled: relay_lease_authority_enabled(),
            relay_carrier: false,
            relay_peers: HashMap::new(),
            relay_offers: HashMap::new(),
            relay_offer_winners: HashMap::new(),
            relay_redeeming_transfer_offers: HashMap::new(),
            relay_leases: HashMap::new(),
            deferred_prepare: None,
            pending_relay_status: None,
            pending_relay_revocations: HashMap::new(),
            next_takeover_fence: 1,
            relay_request_budget: HashMap::new(),
            relay_rtc_signal_budget: HashMap::new(),
            retired_prepared_rtc: HashMap::new(),
            relay_replays: HashMap::new(),
        }
    }

    /// The relay party owed a fresh greeting, consumed once.
    pub(super) fn take_relay_regreet_party(&mut self) -> Option<String> {
        self.relay_regreet_party.take()
    }

    /// The verified device/party route produced by the last admitted hello.
    pub(super) fn take_relay_verified_route(&mut self) -> Option<(String, String)> {
        self.relay_verified_route.take()
    }

    /// Force the current authoritative state to be published on the next loop
    /// turn. Called once when a mobile hello is admitted and again after its
    /// asynchronous fresh plugin proof is installed: state sent while the
    /// challenge was in flight cannot substitute for state BEHIND that proof.
    pub(super) fn rearm_authoritative_publication(&mut self) {
        self.authoritative_idle = false;
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.relay_snapshot_event_id = None;
        self.relay_snapshot_delivered_devices.clear();
        self.last_assistance_request_sent = None;
        self.next_snapshot_poll = Instant::now();
    }

    pub(super) fn rotate_credentials(
        &mut self,
        credentials: &SessionCredentials,
        plugin_session_nonce: String,
    ) -> Result<(), WorkerError> {
        if self.app_id != credentials.app_id
            || self.plugin_id != credentials.plugin_id
            || self.endpoint_authority.endpoint_key != credentials.endpoint_authority.endpoint_key
            || self.endpoint_authority.roster_revision
                != credentials.endpoint_authority.roster_revision
            || self.endpoint_authority.roster_hash != credentials.endpoint_authority.roster_hash
        {
            return Err(WorkerError::rebootstrap(
                "Rotated Companion admission changed the endpoint authority",
            ));
        }
        self.plugin_session_nonce = plugin_session_nonce;
        self.endpoint_authority = credentials.endpoint_authority.clone();
        self.ice_servers = credentials.ice_servers.clone();
        self.relay_only = credentials.relay_only;
        self.last_snapshot_sent = None;
        self.authoritative_idle = false;
        self.next_snapshot_poll = Instant::now();
        self.last_assistance_request_sent = None;
        self.relay_snapshot_event_id = None;
        self.relay_snapshot_delivered_devices.clear();
        // Endpoint-session rotation invalidates every signature context the
        // retired PREPARED generation carried.  It must not survive merely as
        // a bearer-token match.
        self.retired_prepared_rtc.clear();
        self.relay_replays.retain(|_, replay| {
            !matches!(
                &replay.result,
                RelayReplayResult::RetiredPreparedRtcDropped { .. }
            )
        });
        Ok(())
    }

    pub(super) fn apply_admission_rotation(
        &mut self,
        credentials: &SessionCredentials,
        plugin_session_nonce: String,
        preserve_continuity: bool,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        if preserve_continuity {
            return self.rotate_credentials(credentials, plugin_session_nonce);
        }
        // Sequence numbers, routes and lease heartbeats from another relay
        // mailbox cannot prove authority here. Return the caller first, then
        // replace every logical-session registry in one assignment.
        media.fail_closed_all("gateway_admission_domain_changed");
        *self = Self::new(credentials, plugin_session_nonce);
        Ok(())
    }
}
