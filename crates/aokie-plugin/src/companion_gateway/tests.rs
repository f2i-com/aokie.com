use super::*;
use aokie_protocol::v2::{tracks_for, MediaTrack, LEASE_AUDIENCE};

fn test_authority() -> Arc<EndpointAuthority> {
    let signing_key = SigningKey::from_bytes(&[9; 32]);
    let endpoint_key =
        EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
    let mobile_signing_key = SigningKey::from_bytes(&[10; 32]);
    let mobile_key =
        EndpointPublicKey::from_ed25519_bytes(&mobile_signing_key.verifying_key().to_bytes());
    let roster_revision = 1;
    let roster_hash = peer_roster_hash(roster_revision, &[mobile_key.thumbprint.clone()]);
    Arc::new(EndpointAuthority {
        signing_key,
        endpoint_key,
        roster_revision,
        roster_hash,
        approved_mobile_keys: HashMap::from([(mobile_key.thumbprint.clone(), mobile_key)]),
    })
}

fn bootstrap_value() -> Value {
    let authority = test_authority();
    let mobile_keys = authority
        .approved_mobile_keys
        .values()
        .cloned()
        .collect::<Vec<_>>();
    json!({
        "schemaVersion": 2,
        "gatewayUrl": "wss://gateway.example.test/custom?ticket=hidden",
        "accessToken": "top-secret-bearer",
        "appId": "app_a",
        "pluginId": "aokie",
        "iceServers": [{
            "urls": ["stun:stun.example.test:3478"],
            "username": "",
            "credential": ""
        }],
        "relayOnly": false,
        "endpointIdentity": {
            "algorithm": authority.endpoint_key.algorithm,
            "publicKey": authority.endpoint_key.public_key,
            "thumbprint": authority.endpoint_key.thumbprint,
            "privateKeySeed": URL_SAFE_NO_PAD.encode(authority.signing_key.to_bytes())
        },
        "approvedMobileRoster": {
            "revision": authority.roster_revision,
            "rosterHash": authority.roster_hash,
            "keys": mobile_keys
        }
    })
}

fn admission(
    app_id: &str,
    subject_id: &str,
    authority: &EndpointAuthority,
) -> AdmissionResponse {
    serde_json::from_value(admission_value(app_id, subject_id, authority))
        .expect("test admission response is valid")
}

fn admission_value(app_id: &str, subject_id: &str, authority: &EndpointAuthority) -> Value {
    let now = unix_now().unwrap();
    let turn_credential_expires_at = now + 120;
    json!({
        "accessToken": "aokie-adm-v2.secret-value",
        "tokenType": "Bearer",
        "expiresIn": 60,
        "expiresAt": now + 60,
        "gatewayUrl": "wss://gateway.example.test",
        "appId": app_id,
        "subjectId": subject_id,
        "role": "plugin",
        "scopes": ["state_read"],
        "device": {"id": subject_id},
        "iceServers": [{
            "urls": ["turns:turn.example.test:5349"],
            "username": "ephemeral",
            "credential": "credential",
            "expiresAt": turn_credential_expires_at
        }],
        "relayOnly": true,
        "turnCredentialExpiresAt": turn_credential_expires_at,
        "endpointPublicKey": authority.endpoint_key,
        "holderKeyThumbprint": authority.endpoint_key.thumbprint,
        "approvedPeerKeyThumbprints": authority.approved_thumbprints(),
        "peerRosterRevision": authority.roster_revision,
        "peerRosterHash": authority.roster_hash
    })
}

fn test_gateway_session() -> GatewaySession {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();
    let mut session = GatewaySession::new(&credentials, "plugin_session_a".into());
    session.assistance = crate::assistance::AssistanceBroker::default();
    session
}

fn test_plugin_revocation(
    session: &GatewaySession,
    device_id: &str,
    lease_id: &str,
) -> (PluginLeaseRevokeFrame, String) {
    let frame = PluginLeaseRevokeFrame {
        kind: "plugin_lease_revoke".into(),
        schema_version: SCHEMA_VERSION,
        app_id: session.app_id.clone(),
        device_id: device_id.into(),
        lease_id: lease_id.into(),
        lease_jti: format!("{lease_id}_jti"),
        call_id: "call_a".into(),
        call_epoch: 7,
        fence: 9,
        reason: "terminal_rtc_failure".into(),
    };
    frame.validate().unwrap();
    let encoded = serde_json::to_string(&frame).unwrap();
    (frame, encoded)
}

fn remote_snapshot(
    service_mode: LocalServiceMode,
    talk_audio_forwarded: bool,
) -> crate::remote_media::RemoteMediaSnapshot {
    crate::remote_media::RemoteMediaSnapshot {
        call_id: Some("call_a".into()),
        call_epoch: 1,
        owner_epoch: 1,
        remote_revision: 1,
        service_mode,
        peer_count: 1,
        talk_device_id: Some("device_a".into()),
        talk_lease_id: Some("lease_a".into()),
        talk_fence: 1,
        talk_audio_forwarded,
        microphone_muted: false,
        radio_reserved: true,
        dropped_sco_frames: 0,
        quarantined_talk_frames: 0,
        dropped_events: 0,
        consent: crate::remote_media::RemoteConsentGate::default(),
        captions: Vec::new(),
        participants: Vec::new(),
        audio_levels: Vec::new(),
    }
}

#[test]
fn human_media_is_connecting_until_pcm_reaches_the_caller_tx_seam() {
    let waiting = remote_snapshot(LocalServiceMode::HumanActive, false);
    assert_eq!(
        authoritative_media_state(&waiting, true),
        MediaState::Connecting
    );

    let proven = remote_snapshot(LocalServiceMode::HumanActive, true);
    assert_eq!(authoritative_media_state(&proven, true), MediaState::Active);

    let consult = remote_snapshot(LocalServiceMode::ConsultActive, false);
    assert_eq!(
        authoritative_media_state(&consult, true),
        MediaState::Active
    );
}

#[test]
fn socket_authoritative_telemetry_requires_current_remote_consent() {
    let mut remote = remote_snapshot(LocalServiceMode::HumanActive, true);
    remote.participants = vec![crate::remote_media::RemoteParticipant {
        participant_id: "rtc_owner".into(),
        device_id: "device_owner".into(),
        mode: MediaMode::Talk,
        state: RemoteParticipantState::Active,
    }];
    remote.audio_levels = vec![crate::remote_media::RemoteAudioLevel {
        source: RemoteAudioLevelSource::Companion,
        participant_id: Some("rtc_owner".into()),
        level_permille: 700,
    }];

    let (participants, levels) = authoritative_remote_telemetry(&remote);
    assert!(participants.is_empty());
    assert!(levels.is_none(), "disabled consent exposes no telemetry");

    remote.consent.enabled = true;
    remote.consent.acknowledged = true;
    remote.consent.acknowledged_at = Some("2026-07-19T00:00:00Z".into());
    remote.consent.expires_at = Some("2999-01-01T00:00:00Z".into());
    let (participants, levels) = authoritative_remote_telemetry(&remote);
    assert_eq!(participants.len(), 1);
    assert_eq!(levels.as_ref().map(Vec::len), Some(1));

    remote.consent.expires_at = Some("2000-01-01T00:00:00Z".into());
    let (participants, levels) = authoritative_remote_telemetry(&remote);
    assert!(participants.is_empty());
    assert!(levels.is_none(), "expired consent exposes no telemetry");

    remote.consent.expires_at = Some("not-an-rfc3339-instant".into());
    let (participants, levels) = authoritative_remote_telemetry(&remote);
    assert!(participants.is_empty());
    assert!(levels.is_none(), "malformed consent fails closed");

    remote.consent.expires_at = Some("2000-01-01T23:59:59+14:00".into());
    let (participants, levels) = authoritative_remote_telemetry(&remote);
    assert!(participants.is_empty());
    assert!(
        levels.is_none(),
        "an offset timestamp is compared as an instant, not as text"
    );
}

#[test]
fn admission_rotation_updates_transport_identity_without_dropping_native_routes() {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    let mut session = GatewaySession::new(&credentials, "plugin_session_old".into());
    install_takeover_route(&mut session, LeasePhase::Active);
    let lease_count = session.leases.len();
    let peer_count = session.peers.len();

    let mut refreshed = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();
    refreshed.relay_only = false;
    refreshed.ice_servers.clear();
    session
        .rotate_credentials(&refreshed, "plugin_session_new".into())
        .unwrap();

    assert_eq!(session.plugin_session_nonce, "plugin_session_new");
    assert!(!session.relay_only);
    assert!(session.ice_servers.is_empty());
    assert_eq!(session.leases.len(), lease_count);
    assert_eq!(session.peers.len(), peer_count);
}

#[test]
fn pending_revokes_follow_admission_continuity_but_not_a_new_mobile_session_or_domain() {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    let mut session = GatewaySession::new(&credentials, "plugin_session_old".into());
    session.relay_carrier = true;
    let (_, encoded) = test_plugin_revocation(&session, "device_a", "lease_continuity");
    session.prepare_relay_delivery(&encoded);
    assert_eq!(session.pending_relay_revocations.len(), 1);

    let refreshed = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();
    session
        .rotate_credentials(&refreshed, "plugin_session_rotated".into())
        .unwrap();
    assert!(session
        .pending_relay_revocations
        .get("lease_continuity")
        .is_some_and(|pending| pending.encoded == encoded));

    let media = RemoteMediaHandle::spawn().unwrap();
    session.revoke_relay_device_authority("device_a", &full_relay_grants(), true, &media);
    assert!(session.pending_relay_revocations.is_empty());

    session.prepare_relay_delivery(&encoded);
    assert_eq!(session.pending_relay_revocations.len(), 1);
    session
        .apply_admission_rotation(
            &refreshed,
            "plugin_session_new_domain".into(),
            false,
            &media,
        )
        .unwrap();
    assert!(session.pending_relay_revocations.is_empty());
}

#[test]
fn pending_revoke_retry_is_fair_when_the_oldest_delivery_drops_again() {
    let mut harness = RelayHarness::new();
    let (_, first) =
        test_plugin_revocation(&harness.session, &harness.device_id, "lease_retry_a");
    let (_, second) =
        test_plugin_revocation(&harness.session, &harness.device_id, "lease_retry_b");
    harness.session.prepare_relay_delivery(&first);
    harness.session.prepare_relay_delivery(&second);

    let due_at = Instant::now();
    {
        let first_pending = harness
            .session
            .pending_relay_revocations
            .get_mut("lease_retry_a")
            .unwrap();
        first_pending.registered_at = due_at;
        first_pending.next_attempt_at = due_at;
        let second_pending = harness
            .session
            .pending_relay_revocations
            .get_mut("lease_retry_b")
            .unwrap();
        second_pending.registered_at = due_at + Duration::from_nanos(1);
        second_pending.next_attempt_at = due_at;
    }

    let first_attempt = harness.session.due_pending_relay_revocations(due_at);
    assert_eq!(first_attempt, vec![first.clone()]);
    assert_eq!(
        harness
            .session
            .pending_relay_revocations
            .get("lease_retry_b")
            .unwrap()
            .next_attempt_at,
        due_at,
        "only the selected revoke advances"
    );
    harness.session.finish_relay_delivery(
        &first,
        TransportDelivery::Dropped,
        &harness.media,
        &harness.radio,
    );

    let second_attempt = harness
        .session
        .due_pending_relay_revocations(Instant::now());
    assert_eq!(second_attempt, vec![second]);
}

#[test]
fn pending_revoke_ledger_evicts_oldest_at_its_cap_and_prunes_its_ttl() {
    let mut session = test_gateway_session();
    session.relay_carrier = true;
    let base = Instant::now();
    for index in 0..(MAX_PENDING_RELAY_REVOCATIONS + 3) {
        let lease_id = format!("lease_bounded_{index:02}");
        let (frame, encoded) = test_plugin_revocation(&session, "device_a", &lease_id);
        session.register_pending_relay_revocation(
            &frame,
            &encoded,
            base + Duration::from_nanos(index as u64),
        );
    }
    assert_eq!(
        session.pending_relay_revocations.len(),
        MAX_PENDING_RELAY_REVOCATIONS
    );
    assert!(!session
        .pending_relay_revocations
        .contains_key("lease_bounded_00"));
    assert!(session
        .pending_relay_revocations
        .contains_key("lease_bounded_03"));

    assert!(session
        .due_pending_relay_revocations(
            base + PENDING_RELAY_REVOCATION_TTL + Duration::from_secs(1)
        )
        .is_empty());
    assert!(session.pending_relay_revocations.is_empty());
}

fn takeover_claims(session: &GatewaySession, phase: LeasePhase) -> LeaseClaims {
    let owner_epoch = if phase == LeasePhase::Prepared { 3 } else { 4 };
    LeaseClaims {
        aud: LEASE_AUDIENCE.into(),
        app_id: session.app_id.clone(),
        plugin_id: session.plugin_id.clone(),
        device_id: "device_a".into(),
        plugin_key_thumbprint: session.endpoint_authority.endpoint_key.thumbprint.clone(),
        mobile_key_thumbprint: session
            .endpoint_authority
            .approved_mobile_keys
            .keys()
            .next()
            .expect("test authority has a mobile key")
            .clone(),
        call_id: "call_a".into(),
        call_epoch: 7,
        owner_epoch,
        mode: LeaseMode::Takeover,
        phase,
        tracks: tracks_for(LeaseMode::Takeover, phase),
        expires_at: unix_now().unwrap() + 20,
        lease_id: "lease_stable".into(),
        jti: if phase == LeasePhase::Prepared {
            "lease_prepared_jti".into()
        } else {
            "lease_active_jti".into()
        },
        fence: 9,
        session_nonce: "mobile_session".into(),
        rtc_session_id: "rtc_a".into(),
    }
}

fn install_takeover_route(session: &mut GatewaySession, phase: LeasePhase) -> RemoteMediaEvent {
    let provisional = takeover_claims(session, LeasePhase::Prepared);
    let claims = takeover_claims(session, phase);
    let binding = binding_for_claims(&claims);
    session.leases.insert(claims.jti.clone(), claims.clone());
    session.prepared = Some(PreparedTakeover {
        request_id: "request_a".into(),
        provisional,
        expected_switchboard_revision: None,
        confirmed_owner_epoch: (phase == LeasePhase::Active).then_some(claims.owner_epoch),
        provisional_sdp_revision: 1,
        provisional_transport_generation: 1,
        decision_sent: phase == LeasePhase::Active,
        active_rebind_deadline: None,
    });
    session.peers.insert(
        binding.rtc_session_id.clone(),
        PeerRoute {
            binding: binding.clone(),
            lease_jti: claims.jti,
            device_id: claims.device_id,
            sdp_revision: if phase == LeasePhase::Prepared { 1 } else { 2 },
            transport_generation: if phase == LeasePhase::Prepared { 1 } else { 2 },
            lease_ttl_ms: 20_000,
            connected: true,
            remote_audio_ready: phase == LeasePhase::Active,
            remote_microphone_ready: phase == LeasePhase::Active,
            transition_requested: true,
        },
    );
    RemoteMediaEvent {
        sequence: 1,
        rtc_session_id: binding.rtc_session_id,
        call_id: binding.call_id,
        call_epoch: binding.call_epoch,
        owner_epoch: binding.owner_epoch,
        kind: RemoteMediaEventKind::ReturningToAokie {
            reason: "sco_unavailable".into(),
        },
    }
}

#[test]
fn scheduled_admission_refresh_is_immediate_and_resets_failure_backoff() {
    let retry = retry_schedule(WorkerErrorKind::AdmissionRefresh, 32, true);
    assert_eq!(retry.phase, GatewayConnectionPhase::AdmissionRefresh);
    assert_eq!(retry.attempt, 0);
    assert_eq!(retry.delay, Duration::ZERO);
}

#[test]
fn established_session_resets_backoff_but_real_failures_still_escalate() {
    let recovered_then_dropped = retry_schedule(WorkerErrorKind::Reconnect, 32, true);
    assert_eq!(
        recovered_then_dropped.phase,
        GatewayConnectionPhase::Reconnecting
    );
    assert_eq!(recovered_then_dropped.attempt, 1);
    assert_eq!(recovered_then_dropped.delay, Duration::from_secs(1));

    let second_consecutive_failure = retry_schedule(
        WorkerErrorKind::Reconnect,
        recovered_then_dropped.attempt,
        false,
    );
    assert_eq!(second_consecutive_failure.attempt, 2);
    assert_eq!(second_consecutive_failure.delay, Duration::from_secs(2));

    let unsafe_admission = retry_schedule(WorkerErrorKind::Expired, 5, false);
    assert_eq!(unsafe_admission.phase, GatewayConnectionPhase::Expired);
    assert_eq!(unsafe_admission.attempt, 6);
    assert_eq!(unsafe_admission.delay, MAX_BACKOFF);
}

#[test]
fn idle_publication_is_explicit_once_per_no_call_transition() {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();
    let mut session = GatewaySession::new(&credentials, "plugin_session_a".into());

    let encoded = session
        .idle_transition_frame()
        .unwrap()
        .expect("initial idle state must be published");
    let frame: PluginIdleFrame = serde_json::from_str(&encoded).unwrap();
    frame.validate().unwrap();
    assert_eq!(frame.app_id, "app_a");
    assert_eq!(
        serde_json::from_str::<Value>(&encoded)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["appId", "eventId", "kind", "schemaVersion"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert!(session.idle_transition_frame().unwrap().is_none());

    // A subsequently published call re-arms exactly one authoritative
    // no-call assertion when physical state returns to idle.
    session.authoritative_idle = false;
    assert!(session.idle_transition_frame().unwrap().is_some());
    assert!(session.idle_transition_frame().unwrap().is_none());
}

/// A real `mobile_hello`: signed by `signing_key` over the same
/// domain-separated claims the Companion signs, in the mobile peer-policy
/// shape (an expected peer, and no roster members at all).
fn mobile_hello(
    session: &GatewaySession,
    signing_key: &SigningKey,
    device_id: &str,
    jti: &str,
    expected_peer_thumbprint: &str,
) -> String {
    mobile_hello_with_nonce(
        session,
        signing_key,
        device_id,
        jti,
        expected_peer_thumbprint,
        &format!("mobile_session_{device_id}"),
    )
}

fn mobile_hello_with_nonce(
    session: &GatewaySession,
    signing_key: &SigningKey,
    device_id: &str,
    jti: &str,
    expected_peer_thumbprint: &str,
    session_nonce: &str,
) -> String {
    let endpoint_key =
        EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
    let now = unix_now().unwrap();
    let claims = HelloProofClaims {
        app_id: session.app_id.clone(),
        subject_id: device_id.to_owned(),
        role: AdmissionRole::Mobile,
        connection_id: "relay_c0ffee".into(),
        challenge_nonce: "challenge_abc123".into(),
        admission_jti: "jti_abc123".into(),
        session_nonce: session_nonce.to_owned(),
        holder_key_thumbprint: endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: Some(expected_peer_thumbprint.to_owned()),
        approved_peer_key_thumbprints: Vec::new(),
        peer_roster_revision: None,
        peer_roster_hash: None,
        nonce: format!("hello_nonce_{jti}"),
        jti: jti.to_owned(),
        issued_at: now,
        expires_at: now + 30,
    };
    let signature = URL_SAFE_NO_PAD.encode(
        signing_key
            .sign(&claims.signing_bytes().expect("claims canonicalize"))
            .to_bytes(),
    );
    let hello = MobileHello {
        kind: "mobile_hello".into(),
        schema_version: SCHEMA_VERSION,
        app_id: session.app_id.clone(),
        device_id: device_id.to_owned(),
        session_nonce: session_nonce.to_owned(),
        endpoint_proof: SignedHelloProof {
            endpoint_key,
            claims,
            signature,
        },
    };
    // Internal consistency only; whether the SIGNER is approved is exactly
    // what the plugin decides.
    hello.validate().expect("test mobile hello is well formed");
    serde_json::to_string(&hello).expect("test mobile hello encodes")
}

/// The approved Companion's key, as `test_authority` minted it.
fn approved_mobile_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[10; 32])
}

fn approved_mobile_party() -> String {
    relay_party(
        &EndpointPublicKey::from_ed25519_bytes(
            &approved_mobile_signing_key().verifying_key().to_bytes(),
        )
        .thumbprint,
    )
}

fn full_relay_grants() -> HashSet<Grant> {
    HashSet::from([
        Grant::StateRead,
        Grant::RtcSignal,
        Grant::Monitor,
        Grant::Consult,
        Grant::Takeover,
        Grant::ResumeAokie,
    ])
}

fn monitor_relay_grants() -> HashSet<Grant> {
    HashSet::from([Grant::StateRead, Grant::RtcSignal, Grant::Monitor])
}

fn takeover_relay_grants() -> HashSet<Grant> {
    HashSet::from([
        Grant::StateRead,
        Grant::RtcSignal,
        Grant::Takeover,
        Grant::ResumeAokie,
    ])
}

// --- Relay lease authority ------------------------------------------
//
// The plugin is the authority on this carrier, so these drive it exactly
// the way a Companion does: over `handle_relay_peer_frame`, with the
// roster party the carrier reports, and never by reaching into session
// state to install something the mint would not have produced.

/// A live call with an approved Companion greeted on the relay.
struct RelayHarness {
    session: GatewaySession,
    media: RemoteMediaHandle,
    radio: crate::radio::RadioHandle,
    status: Arc<crate::radio::RadioStatus>,
    _control_rx: std::sync::mpsc::Receiver<crate::radio::RadioControl>,
    device_id: String,
    party: String,
    grants: HashSet<Grant>,
}

impl RelayHarness {
    fn new() -> Self {
        Self::with_consent(consenting_gate())
    }

    fn with_consent(gate: crate::remote_media::RemoteConsentGate) -> Self {
        Self::with_consent_and_grants(gate, full_relay_grants())
    }

    fn with_grants(grants: HashSet<Grant>) -> Self {
        Self::with_consent_and_grants(consenting_gate(), grants)
    }

    fn with_consent_and_grants(
        gate: crate::remote_media::RemoteConsentGate,
        grants: HashSet<Grant>,
    ) -> Self {
        let media = RemoteMediaHandle::spawn().unwrap();
        media.set_remote_consent(gate);
        media.observe_physical_call(Some("call_a"), true);
        let (radio, control_rx, status) =
            crate::radio::RadioHandle::test_handle_with_media(media.clone());
        *status.current_call_id.lock().unwrap() = Some("call_a".into());
        status
            .call_active
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let mut session = test_gateway_session();
        session.relay_carrier = true;
        let device_id = "device_a".to_string();
        let peer = plugin_thumbprint(&session);
        let hello = mobile_hello(
            &session,
            &approved_mobile_signing_key(),
            &device_id,
            "hello_jti_authority",
            &peer,
        );
        let party = approved_mobile_party();
        session
            .accept_mobile_hello(&hello, Some(&party), Some(&device_id), &grants, &media)
            .unwrap();
        Self {
            session,
            media,
            radio,
            status,
            _control_rx: control_rx,
            device_id,
            party,
            grants,
        }
    }

    /// Move the physical call, keeping radio and media truth in step the
    /// way the real radio does.
    fn move_call(&mut self, call_id: &str) {
        self.media.observe_physical_call(Some(call_id), true);
        *self.status.current_call_id.lock().unwrap() = Some(call_id.into());
    }

    /// Free the claimant slot, as a revoke or a failed route does.
    fn release_claimant(&mut self) {
        self.session.relay_leases.clear();
        self.session.leases.clear();
        self.session.prepared = None;
        self.session.deferred_prepare = None;
        self.session.pending_relay_status = None;
    }

    fn request_transfer(
        &self,
        reason: &str,
    ) -> (String, crate::assistance::AssistanceCallFence) {
        let remote = self.media.snapshot();
        let fence = crate::assistance::AssistanceCallFence {
            call_id: remote.call_id.expect("test call is active"),
            call_epoch: remote.call_epoch,
            owner_epoch: remote.owner_epoch,
            switchboard_revision: self.radio.switchboard_revision(),
            remote_revision: remote.remote_revision,
        };
        let request_id = self
            .session
            .assistance
            .request_transfer(fence.clone(), reason, None, 60)
            .expect("transfer request is accepted");
        (request_id, fence)
    }

    fn transfer_offer_for(
        &mut self,
        request_id: &str,
        surface: MobileOfferSurface,
    ) -> SignedPendingMobileOffer {
        self.publish_offers()
            .into_iter()
            .find(|offer| {
                offer.offer.offered_mode == LeaseMode::Takeover
                    && offer.offer.surface == surface
                    && offer.offer.accepted_transfer_request_id.as_deref() == Some(request_id)
            })
            .unwrap_or_else(|| panic!("a {surface:?} transfer offer is published"))
    }

    /// Publish a snapshot and return the offers it carried.
    ///
    /// Publication is normally edge-triggered on a state change or the
    /// ten-second refresh; forcing it here is what that refresh does, and it
    /// keeps these tests about the offers rather than about the cadence.
    fn publish_offers(&mut self) -> Vec<SignedPendingMobileOffer> {
        self.session.last_snapshot_fingerprint = None;
        self.session.last_snapshot_sent = None;
        let encoded = self
            .session
            .snapshot_frame(&self.radio)
            .expect("snapshot builds")
            .expect("a live call publishes a snapshot");
        serde_json::from_str::<PluginSnapshotFrame>(&encoded)
            .expect("snapshot decodes")
            .snapshot
            .pending_mobile_offers
    }

    fn offer_for(&mut self, mode: LeaseMode) -> SignedPendingMobileOffer {
        self.publish_offers()
            .into_iter()
            .find(|offer| offer.offer.offered_mode == mode)
            .unwrap_or_else(|| panic!("an offer for {mode:?} is published"))
    }

    /// Drive one peer frame the way the worker loop does.
    fn post(&mut self, encoded: &str) -> Vec<String> {
        self.post_as(encoded, Some(&self.party.clone()))
    }

    fn post_as(&mut self, encoded: &str, party: Option<&str>) -> Vec<String> {
        let grants = self.grants.clone();
        self.post_as_with_grants(encoded, party, &grants)
    }

    fn post_with_grants(
        &mut self,
        encoded: &str,
        authenticated_grants: &HashSet<Grant>,
    ) -> Vec<String> {
        self.post_as_with_grants(encoded, Some(&self.party.clone()), authenticated_grants)
    }

    fn post_as_with_grants(
        &mut self,
        encoded: &str,
        party: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
    ) -> Vec<String> {
        self.session
            .handle_relay_peer_frame(
                encoded,
                party,
                Some(&self.device_id),
                authenticated_grants,
                &self.media,
                &self.radio,
            )
            .expect("relay peer traffic never terminates the session")
    }

    /// Settle the exact frame the transport just attempted, as the worker
    /// loop does after every relay POST.
    fn settle(&mut self, frames: &[String], delivery: TransportDelivery) {
        let encoded = frames.first().expect("a relay frame was returned");
        self.session.prepare_relay_delivery(encoded);
        self.session
            .finish_relay_delivery(encoded, delivery, &self.media, &self.radio);
    }

    fn answer(&mut self, offer: &SignedPendingMobileOffer, request: &str) -> Vec<String> {
        let frame = offer_answer(offer, &self.device_id, request);
        self.post(&frame)
    }

    /// Answer an offer and request the lease it allows, in one step.
    fn claim(&mut self, mode: LeaseMode, request: &str) -> Vec<String> {
        let offer = self.offer_for(mode);
        let accepted = self.answer(&offer, request);
        assert!(!accepted.is_empty(), "the offer answer is accepted");
        let frame = lease_request(&offer, request, "rtc_a");
        self.post(&frame)
    }

    fn granted(&self, frames: &[String]) -> PluginLeaseStatusFrame {
        let encoded = frames.first().expect("a lease status frame was returned");
        serde_json::from_str(encoded)
            .unwrap_or_else(|_| panic!("expected a lease status, got {encoded}"))
    }

    fn rejection(&self, frames: &[String]) -> PluginClaimRejectedFrame {
        frames
            .iter()
            .find_map(|encoded| serde_json::from_str(encoded).ok())
            .unwrap_or_else(|| panic!("expected a refusal, got {frames:?}"))
    }
}

fn activate_takeover_without_replacement_peer(
    harness: &mut RelayHarness,
    request_id: &str,
) -> PluginLeaseStatusFrame {
    let provisional_frames = harness.claim(LeaseMode::Takeover, request_id);
    let provisional = harness.granted(&provisional_frames);
    harness.settle(&provisional_frames, TransportDelivery::Delivered);
    let provisional_binding = binding_for_claims(&provisional.lease);
    harness
        .media
        .install_test_prepared_peer(provisional_binding.clone(), 10_000)
        .unwrap();
    harness
        .media
        .ack_prepare_human(&provisional_binding)
        .unwrap();
    let active_frames = harness
        .session
        .drain_media_events(&harness.media, &harness.radio)
        .unwrap();
    let active_encoded = active_frames
        .iter()
        .find(|encoded| {
            serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
        })
        .expect("preparation emits an active lease")
        .clone();
    let active = serde_json::from_str(&active_encoded).unwrap();
    harness.session.finish_relay_delivery(
        &active_encoded,
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    active
}

/// Install actionable ACTIVE media for a gateway authority test.
///
/// The native test peer intentionally drops its actor receiver, so it
/// cannot exercise `renew_lease` itself. Remove the PREPARED-only gateway
/// guard after installing the exact active media binding; the tests below
/// can then isolate registry renewal and its action gates while the native
/// binding remains live for their bounded duration.
fn install_actionable_takeover_for_renewal_test(
    harness: &mut RelayHarness,
    active: &PluginLeaseStatusFrame,
) {
    let binding = binding_for_claims(&active.lease);
    harness
        .media
        .install_test_active_talk_peer(binding, 20_000)
        .expect("the test route reaches exact active physical ownership");
    harness.session.prepared = None;
}

/// Exercise the same PREPARED -> ACTIVE delivery rotation as production,
/// while marking the prepared peer's negotiated revision/generation.  The
/// native test seam does not negotiate SDP itself, so the two values are
/// supplied explicitly here.
fn activate_takeover_with_retired_prepared_rtc(
    harness: &mut RelayHarness,
    request_id: &str,
) -> (PluginLeaseStatusFrame, PluginLeaseStatusFrame) {
    let provisional_frames = harness.claim(LeaseMode::Takeover, request_id);
    let provisional = harness.granted(&provisional_frames);
    harness.settle(&provisional_frames, TransportDelivery::Delivered);
    let prepared = harness
        .session
        .prepared
        .as_mut()
        .expect("the provisional grant is armed");
    prepared.provisional_sdp_revision = 1;
    prepared.provisional_transport_generation = 1;
    let provisional_binding = binding_for_claims(&provisional.lease);
    harness
        .media
        .install_test_prepared_peer(provisional_binding.clone(), 10_000)
        .unwrap();
    harness
        .media
        .ack_prepare_human(&provisional_binding)
        .unwrap();
    let active_frames = harness
        .session
        .drain_media_events(&harness.media, &harness.radio)
        .unwrap();
    let active_encoded = active_frames
        .iter()
        .find(|encoded| {
            serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
        })
        .expect("preparation emits an active lease")
        .clone();
    let active = serde_json::from_str(&active_encoded).unwrap();
    harness.session.finish_relay_delivery(
        &active_encoded,
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert!(harness
        .session
        .retired_prepared_rtc
        .contains_key(&provisional.lease.lease_id));
    (provisional, active)
}

fn signed_mobile_ice(
    lease: &LeaseClaims,
    lease_token: &str,
    signal_id: &str,
    endpoint_jti: &str,
    sdp_revision: u64,
    transport_generation: u64,
) -> String {
    let signing_key = approved_mobile_signing_key();
    let endpoint_key =
        EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes());
    let now = unix_now().unwrap();
    let candidate = "candidate:1 1 UDP 2122260223 192.0.2.1 54321 typ host".to_string();
    let claims = TrickleCandidateClaims {
        app_id: lease.app_id.clone(),
        plugin_id: lease.plugin_id.clone(),
        device_id: lease.device_id.clone(),
        rtc_session_id: lease.rtc_session_id.clone(),
        endpoint_session_nonce: lease.session_nonce.clone(),
        lease_jti: lease.jti.clone(),
        endpoint_role: AdmissionRole::Mobile,
        holder_key_thumbprint: lease.mobile_key_thumbprint.clone(),
        peer_key_thumbprint: lease.plugin_key_thumbprint.clone(),
        call_id: lease.call_id.clone(),
        call_epoch: lease.call_epoch,
        owner_epoch: lease.owner_epoch,
        fence: lease.fence,
        sdp_revision,
        transport_generation,
        candidate: Some(candidate.clone()),
        sdp_mid: Some("0".into()),
        sdp_m_line_index: Some(0),
        end_of_candidates: false,
        nonce: format!("candidate_nonce_{endpoint_jti}"),
        jti: endpoint_jti.into(),
        issued_at: now,
        expires_at: now + 30,
    };
    let signature = URL_SAFE_NO_PAD.encode(
        signing_key
            .sign(
                &claims
                    .signing_bytes()
                    .expect("candidate claims canonicalize"),
            )
            .to_bytes(),
    );
    serde_json::to_string(&MobileRtcSignalFrame {
        kind: "rtc_signal".into(),
        schema_version: SCHEMA_VERSION,
        app_id: lease.app_id.clone(),
        signal_id: signal_id.into(),
        plugin_id: lease.plugin_id.clone(),
        device_id: lease.device_id.clone(),
        lease_token: lease_token.into(),
        lease_jti: lease.jti.clone(),
        rtc_session_id: lease.rtc_session_id.clone(),
        sdp_revision,
        transport_generation,
        call_id: lease.call_id.clone(),
        call_epoch: lease.call_epoch,
        owner_epoch: lease.owner_epoch,
        fence: lease.fence,
        signal: RtcSignal::Ice {
            candidate,
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
            envelope: SignedTrickleCandidateEnvelope {
                endpoint_key,
                claims,
                signature,
            },
        },
    })
    .expect("signed mobile ICE encodes")
}

fn consenting_gate() -> crate::remote_media::RemoteConsentGate {
    crate::remote_media::RemoteConsentGate {
        enabled: true,
        acknowledged: true,
        acknowledged_at: Some("2026-07-18T00:00:00Z".into()),
        expires_at: None,
        captions_enabled: true,
        assistance_enabled: true,
        monitor_enabled: true,
        consult_enabled: true,
        takeover_enabled: true,
        ..Default::default()
    }
}

fn offer_answer(offer: &SignedPendingMobileOffer, device_id: &str, request_id: &str) -> String {
    serde_json::to_string(&MobileOfferAnswerFrame {
        kind: "mobile_offer_answer".into(),
        schema_version: SCHEMA_VERSION,
        app_id: offer.offer.app_id.clone(),
        request_id: request_id.into(),
        idempotency_key: format!("idem_answer_{request_id}"),
        offer_id: offer.offer.offer_id.clone(),
        offer_jti: offer.offer.jti.clone(),
        offer_token: offer.offer_token.clone(),
        target_device_id: device_id.into(),
        target_holder_key_thumbprint: offer.offer.target_holder_key_thumbprint.clone(),
        offered_mode: offer.offer.offered_mode,
        call_id: offer.offer.call_id.clone(),
        call_epoch: offer.offer.call_epoch,
        owner_epoch: offer.offer.owner_epoch,
    })
    .expect("offer answer encodes")
}

fn lease_request(
    offer: &SignedPendingMobileOffer,
    request_id: &str,
    rtc_session_id: &str,
) -> String {
    serde_json::to_string(&LeaseRequestFrame {
        kind: "lease_request".into(),
        schema_version: SCHEMA_VERSION,
        app_id: offer.offer.app_id.clone(),
        request_id: request_id.into(),
        idempotency_key: format!("idem_lease_{request_id}"),
        call_id: offer.offer.call_id.clone(),
        expected_call_epoch: offer.offer.call_epoch,
        expected_owner_epoch: offer.offer.owner_epoch,
        expected_switchboard_revision: offer.offer.switchboard_revision,
        expected_remote_revision: offer.offer.remote_revision,
        mode: offer.offer.offered_mode,
        rtc_session_id: rtc_session_id.into(),
        accepted_offer_id: offer.offer.offer_id.clone(),
        accepted_offer_jti: offer.offer.jti.clone(),
        accepted_transfer_request_id: offer.offer.accepted_transfer_request_id.clone(),
    })
    .expect("lease request encodes")
}

fn assistance_decline(
    request_id: &str,
    fence: &crate::assistance::AssistanceCallFence,
    answer_id: &str,
) -> String {
    serde_json::to_string(&MobileAssistanceAnswerFrame {
        kind: "assistance_answer".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        request_id: request_id.into(),
        idempotency_key: format!("idem_{answer_id}"),
        answer_id: answer_id.into(),
        call_id: fence.call_id.clone(),
        call_epoch: fence.call_epoch,
        owner_epoch: fence.owner_epoch,
        switchboard_revision: fence.switchboard_revision,
        remote_revision: fence.remote_revision,
        response_action: aokie_protocol::v2::AssistanceResponseAction::Decline,
        answer: "declined".into(),
    })
    .expect("assistance decline encodes")
}

#[test]
fn unadmitted_relay_kinds_still_drop_silently() {
    // The gateway-dialect lifecycle notices are the ones that would let an
    // approved-but-untrusted Companion declare its own takeover with a
    // fence of its choosing. Admitting any of them is the single change
    // that would make this whole path unsafe.
    let mut harness = RelayHarness::new();
    for kind in [
        "claim_proposal",
        "lease_granted",
        "lease_renewed",
        "lease_revoked",
        "claim_decision",
        "assistance_answer",
        "end_caller_challenge_request",
        "end_caller_confirm",
    ] {
        let encoded = json!({"kind": kind, "schemaVersion": SCHEMA_VERSION, "appId": "app_a"})
            .to_string();
        assert!(
            harness.post(&encoded).is_empty(),
            "{kind} must not be actionable over the relay"
        );
    }
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert!(harness.session.deferred_prepare.is_none());
    assert!(harness.session.leases.is_empty());
}

#[test]
fn every_relay_refusal_returns_ok_and_leaves_no_residue() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Takeover);
    let mut forged = offer.clone();
    forged.offer_token = "forged-token".into();

    let hostile = [
        // Malformed, for every admitted kind.
        json!({"kind": "mobile_offer_answer", "schemaVersion": SCHEMA_VERSION}).to_string(),
        json!({"kind": "lease_request", "schemaVersion": SCHEMA_VERSION}).to_string(),
        json!({"kind": "rtc_signal", "schemaVersion": SCHEMA_VERSION}).to_string(),
        json!({"kind": "lease_heartbeat", "schemaVersion": SCHEMA_VERSION}).to_string(),
        json!({"kind": "lease_revoke", "schemaVersion": SCHEMA_VERSION}).to_string(),
        // A wrong schema version, which the socket treats as fatal.
        json!({"kind": "lease_request", "schemaVersion": 9999}).to_string(),
        // A forged token against a real offer.
        offer_answer(&forged, &harness.device_id, "request_forged"),
        // An offer this plugin never issued.
        offer_answer(
            &SignedPendingMobileOffer {
                offer: PendingMobileOfferClaims {
                    offer_id: "offer_invented".into(),
                    ..offer.offer.clone()
                },
                offer_token: offer.offer_token.clone(),
            },
            &harness.device_id,
            "request_invented",
        ),
        // A lease request naming an offer that was never answered.
        lease_request(&offer, "request_unanswered", "rtc_unanswered"),
        // A heartbeat and a revoke for a lease that does not exist.
        json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_ghost",
            "idempotencyKey": "idem_ghost",
            "leaseToken": "not-a-lease"
        })
        .to_string(),
        json!({
            "kind": "lease_revoke",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": "request_ghost2",
            "idempotencyKey": "idem_ghost2",
            "leaseToken": "not-a-lease",
            "reason": "cleanup"
        })
        .to_string(),
        "{ this is not json".to_string(),
    ];

    for encoded in &hostile {
        // Returning Ok is the contract: over this carrier the sender is an
        // approved-but-untrusted peer, so one malformed frame must never
        // cost a session that is carrying a live call.
        assert!(harness
            .session
            .handle_relay_peer_frame(
                encoded,
                Some(&harness.party.clone()),
                Some(&harness.device_id),
                &harness.grants,
                &harness.media,
                &harness.radio
            )
            .is_ok());
        // ...and nothing it sent may leave authority behind.
        assert!(harness.session.relay_leases.is_empty(), "{encoded}");
        assert!(harness.session.leases.is_empty(), "{encoded}");
        assert!(harness.session.prepared.is_none(), "{encoded}");
        assert!(harness.session.deferred_prepare.is_none(), "{encoded}");
        assert!(harness.session.peers.is_empty(), "{encoded}");
    }
}

#[test]
fn a_burned_offer_cannot_be_replayed() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Monitor);

    let exact_answer = offer_answer(&offer, &harness.device_id, "request_first");
    let accepted = harness.post(&exact_answer);
    assert!(!accepted.is_empty());
    assert_eq!(
        harness.post(&exact_answer),
        accepted,
        "an ambiguous offer-acceptance commit replays its acceptance"
    );
    // Answering twice is a replay: the same invitation must not be able to
    // start a second claim.
    let refused = harness.answer(&offer, "request_second");
    assert_eq!(harness.rejection(&refused).code, "offer_replayed");

    // Redeeming spends it outright, so even the first answerer cannot go
    // round again on the same offer.
    let granted = harness.post(&lease_request(&offer, "request_first", "rtc_a"));
    assert!(!granted.is_empty());
    let refused = harness.post(&lease_request(&offer, "request_third", "rtc_b"));
    assert_eq!(
        harness.rejection(&refused).code,
        "offer_retired",
        "a spent offer is explicitly retired so the endpoint cannot retry it"
    );
}

#[test]
fn an_offer_for_another_device_cannot_be_answered() {
    // Even if another approved Companion obtains this frame (for example
    // from a stale pre-projection client or copied logs), only the party
    // and authenticated subject that proved the target device may act on
    // its offer.
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Takeover);
    let frame = offer_answer(&offer, &harness.device_id, "request_impostor");

    let refused = harness.post_as(&frame, Some("mobile:some-other-approved-device"));
    assert_eq!(harness.rejection(&refused).code, "device_unknown");
    assert!(harness.session.relay_leases.is_empty());

    // A carrier that cannot say who sent a frame cannot authorise one
    // either: absent identity fails closed rather than open.
    let refused = harness.post_as(&frame, None);
    assert_eq!(harness.rejection(&refused).code, "device_unknown");

    // The rightful holder is unaffected.
    assert!(!harness.answer(&offer, "request_rightful").is_empty());
}

#[test]
fn monitor_only_admission_never_receives_a_takeover_offer() {
    let mut harness = RelayHarness::with_grants(monitor_relay_grants());
    let offers = harness.publish_offers();

    assert!(
        !offers.is_empty(),
        "state+rtc+monitor still receives its permitted invitation"
    );
    assert!(offers
        .iter()
        .all(|offer| offer.offer.offered_mode == LeaseMode::Monitor));
}

#[test]
fn takeover_requires_resume_aokie_but_exact_holder_can_still_revoke_after_narrowing() {
    let without_resume = HashSet::from([Grant::StateRead, Grant::RtcSignal, Grant::Takeover]);
    let mut narrowed = RelayHarness::with_grants(without_resume.clone());
    assert!(
        narrowed.publish_offers().is_empty(),
        "a device unable to execute mandatory failback receives no takeover invitation"
    );

    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Takeover);
    assert!(offer.offer.required_grants.contains(&Grant::ResumeAokie));
    let refused = harness.post_with_grants(
        &offer_answer(&offer, &harness.device_id, "request_missing_resume"),
        &without_resume,
    );
    assert_eq!(harness.rejection(&refused).code, "grant_required");

    // Revoke is strictly de-escalating and remains available to the exact
    // authenticated holder even when current admission no longer grants
    // the mode (or ResumeAokie). It must never be trapped behind the gate
    // whose purpose is to prevent gaining authority.
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_revoke_narrowed");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);
    let revoke = json!({
        "kind": "lease_revoke",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_revoke_narrowed_now",
        "idempotencyKey": "idem_revoke_narrowed_now",
        "leaseToken": status.lease_token,
        "reason": "permission_removed"
    })
    .to_string();
    assert!(harness
        .session
        .handle_relay_peer_frame(
            &revoke,
            Some(&harness.party),
            Some(&harness.device_id),
            &without_resume,
            &harness.media,
            &harness.radio,
        )
        .unwrap()
        .is_empty());
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
}

#[test]
fn authenticated_grants_not_payload_mode_control_takeover_redemption() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Takeover);
    let monitor_grants = monitor_relay_grants();

    // Even changing the peer-controlled offeredMode to one the admission
    // does grant cannot downgrade the authority check: the plugin uses the
    // mode in the offer it signed.
    let mut spoofed_answer: Value = serde_json::from_str(&offer_answer(
        &offer,
        &harness.device_id,
        "request_grant_spoof",
    ))
    .unwrap();
    spoofed_answer["offeredMode"] = json!("monitor");
    let refused = harness.post_with_grants(&spoofed_answer.to_string(), &monitor_grants);
    assert_eq!(harness.rejection(&refused).code, "grant_required");
    assert!(
        !harness
            .session
            .relay_offers
            .contains_key(&offer.offer.offer_id),
        "current admission narrowing retires the now-unauthorized offer"
    );

    // Exercise the lease request independently with a fresh, fully
    // admitted peer. Broadening is intentionally re-hello-only, so the
    // narrowed session above must not silently regain Takeover here.
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Takeover);
    let accepted = harness.answer(&offer, "request_grant_spoof");
    assert!(!accepted.is_empty());

    // The lease request is checked independently. Its payload can also
    // claim monitor, but the accepted signed offer remains takeover.
    let mut spoofed_request: Value = serde_json::from_str(&lease_request(
        &offer,
        "request_grant_spoof",
        "rtc_grant_spoof",
    ))
    .unwrap();
    spoofed_request["mode"] = json!("monitor");
    let refused = harness.post_with_grants(&spoofed_request.to_string(), &monitor_grants);
    assert_eq!(harness.rejection(&refused).code, "grant_required");
    assert!(harness.session.relay_leases.is_empty());
}

#[test]
fn narrowed_transfer_endpoint_cannot_reserve_the_multi_device_winner() {
    let mut grants = full_relay_grants();
    grants.insert(Grant::AssistanceRespond);
    let mut harness = RelayHarness::with_grants(grants.clone());
    let holder = harness.session.relay_peers[&harness.device_id]
        .holder_key_thumbprint
        .clone();
    harness.session.relay_peers.insert(
        "device_b".into(),
        RelayPeer {
            holder_key_thumbprint: holder.clone(),
            session_nonce: "mobile_session_b".into(),
            grants: grants.clone(),
        },
    );
    let (request_id, _) = harness.request_transfer("The caller asked for the owner");
    let opportunity_id = transfer_opportunity_id(&request_id);
    let offers = harness.publish_offers();
    let offer_a = offers
        .iter()
        .find(|offer| {
            offer.offer.target_device_id == harness.device_id
                && offer.offer.surface == MobileOfferSurface::InApp
                && offer.offer.accepted_transfer_request_id.as_deref()
                    == Some(request_id.as_str())
        })
        .cloned()
        .expect("device A gets the transfer offer");
    let signed_b = offers
        .iter()
        .find(|offer| {
            offer.offer.target_device_id == "device_b"
                && offer.offer.surface == MobileOfferSurface::InApp
                && offer.offer.accepted_transfer_request_id.as_deref()
                    == Some(request_id.as_str())
        })
        .cloned()
        .expect("device B gets the same transfer opportunity");

    let narrowed = grants
        .iter()
        .copied()
        .filter(|grant| *grant != Grant::AssistanceRespond)
        .collect::<HashSet<_>>();
    let refused = harness.post_with_grants(
        &offer_answer(&offer_a, &harness.device_id, "request_narrowed_transfer"),
        &narrowed,
    );
    assert_eq!(harness.rejection(&refused).code, "grant_required");
    assert!(!harness
        .session
        .relay_offer_winners
        .contains_key(&opportunity_id));

    let party_b = relay_party(&holder);
    let accepted = harness
        .session
        .handle_relay_peer_frame(
            &offer_answer(&signed_b, "device_b", "request_valid_transfer"),
            Some(&party_b),
            Some("device_b"),
            &grants,
            &harness.media,
            &harness.radio,
        )
        .unwrap();
    assert!(!accepted.is_empty());
    assert_eq!(
        harness.session.relay_offer_winners.get(&opportunity_id),
        Some(&signed_b.offer.offer_id)
    );
    assert_eq!(
        harness
            .session
            .assistance
            .pending_transfer("call_a", 1)
            .and_then(|pending| pending.accepted_by),
        Some("device_b".into())
    );
}

#[test]
fn rtc_and_heartbeat_recheck_and_apply_current_authenticated_grants() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_grant_rechecks");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);

    let missing_rtc = HashSet::from([Grant::StateRead, Grant::Monitor]);
    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_grant_rechecks_beat",
        "idempotencyKey": "idem_grant_rechecks_beat",
        "leaseToken": status.lease_token
    })
    .to_string();
    let refused = harness.post_with_grants(&heartbeat, &missing_rtc);
    assert_eq!(harness.rejection(&refused).code, "grant_required");
    assert!(
        harness.session.relay_leases.is_empty(),
        "losing a grant on this frame revokes the active lease immediately"
    );
    assert!(harness.session.leases.is_empty());

    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_grant_rechecks_rtc");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);
    let missing_mode = HashSet::from([Grant::StateRead, Grant::RtcSignal]);
    let rtc = serde_json::to_string(&MobileRtcSignalFrame {
        kind: "rtc_signal".into(),
        schema_version: SCHEMA_VERSION,
        app_id: status.lease.app_id.clone(),
        signal_id: "signal_grant_recheck".into(),
        plugin_id: status.lease.plugin_id.clone(),
        device_id: status.lease.device_id.clone(),
        lease_token: status.lease_token.clone(),
        lease_jti: status.lease.jti.clone(),
        rtc_session_id: status.lease.rtc_session_id.clone(),
        sdp_revision: 1,
        transport_generation: 1,
        call_id: status.lease.call_id.clone(),
        call_epoch: status.lease.call_epoch,
        owner_epoch: status.lease.owner_epoch,
        fence: status.lease.fence,
        signal: RtcSignal::Close {
            reason: "done".into(),
        },
    })
    .unwrap();
    let refused = harness.post_with_grants(&rtc, &missing_mode);
    assert_eq!(harness.rejection(&refused).code, "grant_required");
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
}

#[test]
fn grant_narrowing_during_a_prepared_takeover_immediately_returns_the_caller() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_takeover_narrowed");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);
    assert!(harness.session.prepared.is_some());
    assert_eq!(harness.session.leases.len(), 1);

    // This exact frame's authenticated admission has lost Takeover. The
    // plugin must unwind the prepared/soft-hold path before even deciding
    // whether a heartbeat would otherwise be renewable.
    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_takeover_narrowed_beat",
        "idempotencyKey": "idem_takeover_narrowed_beat",
        "leaseToken": status.lease_token
    })
    .to_string();
    let without_takeover = monitor_relay_grants();
    let refused = harness.post_with_grants(&heartbeat, &without_takeover);
    assert_eq!(harness.rejection(&refused).code, "grant_required");
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert!(harness.session.deferred_prepare.is_none());
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive,
        "permission loss must never leave a caller in soft hold"
    );
}

#[test]
fn lease_request_that_misses_physical_epochs_is_refused_stale_call() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Takeover);
    assert!(!harness.answer(&offer, "request_stale").is_empty());

    // The call moves under the claim, exactly as a real one does.
    harness.move_call("call_b");

    let request = lease_request(&offer, "request_stale", "rtc_a");
    let refused = harness.post(&request);
    assert_eq!(harness.rejection(&refused).code, "stale_call");
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.deferred_prepare.is_none());
    assert_eq!(
        harness.post(&request),
        refused,
        "a dropped terminal refusal replays byte-for-byte after the offer was spent"
    );
}

#[test]
fn takeover_without_consent_is_refused_consent_required() {
    let mut gate = consenting_gate();
    gate.takeover_enabled = false;
    let mut harness = RelayHarness::with_consent(gate);

    // Withdrawn consent stops the invitation being made at all...
    assert!(harness
        .publish_offers()
        .iter()
        .all(|offer| offer.offer.offered_mode != LeaseMode::Takeover));

    // ...and a request built from an offer minted while consent still held
    // is refused on its own merits, because consent is re-read at claim
    // time rather than trusted from the offer.
    let monitor = harness.offer_for(LeaseMode::Monitor);
    let mut escalated = monitor.clone();
    escalated.offer.offered_mode = LeaseMode::Takeover;
    assert!(!harness.answer(&monitor, "request_escalate").is_empty());
    let refused = harness.post(&lease_request(&escalated, "request_escalate", "rtc_a"));
    // The accepted offer allowed monitor, so the escalation is refused
    // before consent is even consulted.
    assert_eq!(harness.rejection(&refused).code, "mode_unavailable");
    assert!(harness.session.relay_leases.is_empty());
}

#[test]
fn consult_without_a_matching_assistance_fence_is_refused() {
    // A private consultation exists to answer a question Aokie asked. With
    // no current assistance request there is nothing to consult about.
    let mut harness = RelayHarness::new();
    assert!(harness
        .publish_offers()
        .iter()
        .all(|offer| offer.offer.offered_mode != LeaseMode::Consult));

    let monitor = harness.offer_for(LeaseMode::Monitor);
    let mut consult = monitor.clone();
    consult.offer.offered_mode = LeaseMode::Consult;
    assert!(!harness.answer(&monitor, "request_consult").is_empty());
    let refused = harness.post(&lease_request(&consult, "request_consult", "rtc_a"));
    assert_eq!(harness.rejection(&refused).code, "mode_unavailable");
    assert!(harness.session.relay_leases.is_empty());
}

#[test]
fn a_second_claimant_is_refused_claimant_busy() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_first");
    assert_eq!(
        harness.granted(&granted).status,
        PluginLeaseStatus::Provisional
    );

    // Two prepared claims would race for the same physical route, and the
    // loser's soft hold would sit on a caller the winner is already taking.
    let second = harness.offer_for(LeaseMode::Monitor);
    assert!(!harness.answer(&second, "request_second").is_empty());
    let refused = harness.post(&lease_request(&second, "request_second", "rtc_b"));
    assert_eq!(harness.rejection(&refused).code, "claimant_busy");
    assert_eq!(harness.session.relay_leases.len(), 1);
}

#[test]
fn minted_takeover_leases_carry_a_strictly_increasing_positive_fence() {
    let mut harness = RelayHarness::new();
    let first = harness.claim(LeaseMode::Takeover, "request_fence_1");
    let first = harness.granted(&first).lease;
    assert!(first.fence > 0, "a talk-capable lease needs a real fence");

    // Clear the claimant slot the way a revoke does, then claim again.
    harness.release_claimant();

    let second = harness.claim(LeaseMode::Takeover, "request_fence_2");
    let second = harness.granted(&second).lease;
    assert!(
        second.fence > first.fence,
        "a replayed older fence must never look current: {} then {}",
        first.fence,
        second.fence
    );

    // Non-talk modes carry no fence at all, which is what stops one being
    // mistaken for caller-bound authority.
    harness.release_claimant();
    let monitor = harness.claim(LeaseMode::Monitor, "request_fence_3");
    assert_eq!(harness.granted(&monitor).lease.fence, 0);
}

#[test]
fn minted_leases_pass_their_own_validate_and_never_exceed_the_local_cap() {
    let mut harness = RelayHarness::new();
    let now = unix_now().unwrap();

    let prepared = harness.claim(LeaseMode::Takeover, "request_ttl_1");
    let prepared = harness.granted(&prepared);
    prepared
        .lease
        .validate(now)
        .expect("a minted lease survives the checks a received one faces");
    assert_eq!(prepared.status, PluginLeaseStatus::Provisional);
    assert_eq!(prepared.lease.phase, LeasePhase::Prepared);
    // Short on purpose: this is the bound on how long a caller can sit in
    // soft hold waiting for a handover that is not arriving.
    assert!(prepared.lease.expires_at <= now + RELAY_PREPARED_LEASE_TTL);
    assert!(prepared.lease.expires_at <= now + 300, "local safety cap");
    assert_eq!(prepared.lease.session_nonce, "mobile_session_device_a");
    assert_eq!(
        prepared.lease.plugin_key_thumbprint,
        harness.session.endpoint_authority.endpoint_key.thumbprint
    );

    harness.release_claimant();

    let monitor = harness.claim(LeaseMode::Monitor, "request_ttl_2");
    let monitor = harness.granted(&monitor);
    monitor.lease.validate(now).expect("monitor lease is valid");
    assert_eq!(monitor.status, PluginLeaseStatus::Granted);
    assert_eq!(monitor.lease.phase, LeasePhase::Active);
    assert!(monitor.lease.expires_at <= now + RELAY_ACTIVE_LEASE_TTL);
}

#[test]
fn prepared_leases_are_never_renewable() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_renew");
    let status = harness.granted(&granted);
    let token = status.lease_token.clone();
    harness.settle(&granted, TransportDelivery::Delivered);

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_renew_beat",
        "idempotencyKey": "idem_renew_beat",
        "leaseToken": token
    })
    .to_string();

    // Refusing renewal is what makes a stuck prepare impossible to hold
    // open: it must complete inside its short life or hand the caller back.
    let refused = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&refused).code, "phase_not_renewable");

    let stored = harness
        .session
        .relay_leases
        .values()
        .next()
        .expect("the prepared lease is still recorded");
    assert_eq!(stored.phase, LeasePhase::Prepared);
    assert_eq!(stored.current_jti, status.lease.jti);
}

#[test]
fn silent_prepared_lease_expiry_retires_media_and_frees_the_claimant() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_silent_prepare_expiry");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);
    assert!(harness.session.prepared.is_some());

    assert_eq!(
        harness
            .session
            .expire_relay_leases(status.lease.expires_at, &harness.media),
        1
    );
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive
    );

    let replacement = harness.claim(
        LeaseMode::Takeover,
        "request_silent_prepare_expiry_replacement",
    );
    assert_eq!(
        harness.granted(&replacement).status,
        PluginLeaseStatus::Provisional
    );
}

#[test]
fn a_deferred_prepare_is_not_armed_until_the_grant_frame_is_sent() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_defer");
    assert_eq!(
        harness.granted(&granted).status,
        PluginLeaseStatus::Provisional
    );

    // The grant has been BUILT but not sent. Nothing may be armed yet: an
    // armed claim puts the plugin on the path to soft-holding a live caller
    // on behalf of a device that has not been told it won, so a relay stall
    // here would silence the caller for a handover nobody is completing.
    assert!(harness.session.prepared.is_none());
    assert!(harness.session.deferred_prepare.is_some());
    assert!(harness.session.leases.is_empty());

    // The worker loop commits only after the transport accepted the frame.
    harness.session.finish_relay_delivery(
        &granted[0],
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert!(harness.session.deferred_prepare.is_none());
    assert!(
        harness.session.prepared.is_some(),
        "the claim arms once the device has actually been told"
    );
    assert_eq!(harness.session.leases.len(), 1);
}

#[test]
fn a_deferred_prepare_cannot_arm_after_the_switchboard_revision_moves() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_defer_switch");
    let expected = harness
        .session
        .deferred_prepare
        .as_ref()
        .expect("the provisional grant is delivery-gated")
        .expected_switchboard_revision;
    harness.status.switchboard_revision.store(
        expected.saturating_add(1),
        std::sync::atomic::Ordering::Relaxed,
    );

    harness.session.finish_relay_delivery(
        &granted[0],
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert!(harness.session.deferred_prepare.is_none());
    assert!(harness.session.prepared.is_none());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
}

#[test]
fn a_dropped_provisional_grant_never_arms_or_blocks_the_caller() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_dropped_grant");
    assert_eq!(
        harness.granted(&granted).status,
        PluginLeaseStatus::Provisional
    );
    let lease_id = harness
        .session
        .deferred_prepare
        .as_ref()
        .expect("the provisional grant is waiting on delivery")
        .lease_id
        .clone();

    // A terminal relay 429 and a missing target are non-fatal carrier
    // drops. Neither is permission to soft-hold a live caller.
    harness.session.finish_relay_delivery(
        &granted[0],
        TransportDelivery::Dropped,
        &harness.media,
        &harness.radio,
    );

    assert!(harness.session.deferred_prepare.is_none());
    assert!(harness.session.prepared.is_none());
    assert!(harness.session.leases.is_empty());
    assert!(!harness.session.relay_leases.contains_key(&lease_id));
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive,
        "the caller never left the receptionist"
    );
}

#[test]
fn dropped_transfer_provisional_retires_spent_frames_and_mints_a_fresh_offer() {
    let mut grants = full_relay_grants();
    grants.insert(Grant::AssistanceRespond);
    let mut harness = RelayHarness::with_grants(grants);
    let (transfer_request_id, _) = harness.request_transfer("The caller asked for the owner");
    let opportunity_id = transfer_opportunity_id(&transfer_request_id);
    let offer = harness.transfer_offer_for(&transfer_request_id, MobileOfferSurface::InApp);
    let answer = offer_answer(&offer, &harness.device_id, "request_transfer_drop");
    let accepted = harness.post(&answer);
    assert!(serde_json::from_str::<PluginOfferAcceptedFrame>(&accepted[0]).is_ok());
    harness.settle(&accepted, TransportDelivery::Delivered);

    let request = lease_request(&offer, "request_transfer_drop", "rtc_transfer_drop");
    let granted = harness.post(&request);
    assert_eq!(
        harness.granted(&granted).status,
        PluginLeaseStatus::Provisional
    );
    harness.settle(&granted, TransportDelivery::Dropped);

    assert!(!harness
        .session
        .relay_offer_winners
        .contains_key(&opportunity_id));
    assert!(!harness
        .session
        .relay_offers
        .contains_key(&offer.offer.offer_id));
    assert_eq!(
        harness
            .session
            .assistance
            .pending_transfer("call_a", 1)
            .and_then(|pending| pending.accepted_by),
        None,
        "a grant the endpoint never received cannot reserve the transfer"
    );

    let replayed_answer = harness.post(&answer);
    assert_eq!(
        harness.rejection(&replayed_answer).code,
        "offer_retired",
        "the cached mobile answer must not re-arm its spent invitation"
    );
    let replayed_lease = harness.post(&request);
    assert_eq!(harness.rejection(&replayed_lease).code, "offer_retired");

    let fresh = harness.transfer_offer_for(&transfer_request_id, MobileOfferSurface::InApp);
    assert_ne!(fresh.offer.offer_id, offer.offer.offer_id);
    assert_ne!(fresh.offer.jti, offer.offer.jti);
    assert_eq!(fresh.offer.opportunity_id, opportunity_id);
    assert!(!harness
        .answer(&fresh, "request_transfer_drop_fresh")
        .is_empty());
}

#[test]
fn dropped_transfer_acceptance_ack_releases_the_cas_and_retires_its_answer() {
    let mut grants = full_relay_grants();
    grants.insert(Grant::AssistanceRespond);
    let mut harness = RelayHarness::with_grants(grants);
    let (request_id, _) = harness.request_transfer("The caller asked for the owner");
    let offer = harness.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    let answer = offer_answer(&offer, &harness.device_id, "request_ack_drop");
    let accepted = harness.post(&answer);
    harness.settle(&accepted, TransportDelivery::Dropped);

    assert_eq!(
        harness
            .session
            .assistance
            .pending_transfer("call_a", 1)
            .and_then(|pending| pending.accepted_by),
        None
    );
    let replayed = harness.post(&answer);
    assert_eq!(harness.rejection(&replayed).code, "offer_retired");
    let fresh = harness.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    assert_ne!(fresh.offer.offer_id, offer.offer.offer_id);
}

#[test]
fn transfer_offer_accept_and_decline_share_one_decision_cas() {
    let mut grants = full_relay_grants();
    grants.insert(Grant::AssistanceRespond);

    let mut accept_first = RelayHarness::with_grants(grants.clone());
    let (request_id, fence) = accept_first.request_transfer("The caller asked for the owner");
    let in_app = accept_first.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    let voice = accept_first
        .publish_offers()
        .into_iter()
        .find(|offer| {
            offer.offer.surface == MobileOfferSurface::VoiceSystemUi
                && offer.offer.accepted_transfer_request_id.as_deref()
                    == Some(request_id.as_str())
        })
        .expect("the native surface shares the transfer opportunity");
    assert_eq!(in_app.offer.opportunity_id, voice.offer.opportunity_id);
    let accepted = accept_first.answer(&in_app, "request_accept_first");
    accept_first.settle(&accepted, TransportDelivery::Delivered);
    let declined = accept_first.post(&assistance_decline(
        &request_id,
        &fence,
        "answer_after_accept",
    ));
    assert_eq!(accept_first.rejection(&declined).code, "already_answered");
    let duplicate_surface = accept_first.post(&offer_answer(
        &voice,
        &accept_first.device_id,
        "request_duplicate_surface",
    ));
    assert_eq!(
        accept_first.rejection(&duplicate_surface).code,
        "offer_already_answered"
    );
    assert_eq!(
        accept_first
            .session
            .assistance
            .pending_transfer("call_a", 1)
            .and_then(|pending| pending.accepted_by),
        Some("device_a".into())
    );

    let mut decline_first = RelayHarness::with_grants(grants);
    let (request_id, fence) = decline_first.request_transfer("The caller asked for the owner");
    let offer = decline_first.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    let decline = decline_first.post(&assistance_decline(
        &request_id,
        &fence,
        "answer_decline_first",
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&decline[0]).unwrap()["kind"],
        "assistance_answer_accepted"
    );
    let late_accept = decline_first.answer(&offer, "request_after_decline");
    assert_eq!(
        decline_first.rejection(&late_accept).code,
        "transfer_unavailable"
    );
    assert!(!decline_first
        .session
        .relay_offer_winners
        .contains_key(&offer.offer.opportunity_id));
}

#[test]
fn accepted_transfer_offer_remains_hidden_but_redeemable_through_setup_window() {
    let mut grants = full_relay_grants();
    grants.insert(Grant::AssistanceRespond);
    let mut harness = RelayHarness::with_grants(grants);
    let (request_id, _) = harness.request_transfer("The caller asked for the owner");
    let offer = harness.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    let accepted = harness.answer(&offer, "request_expired_offer_setup");
    harness.settle(&accepted, TransportDelivery::Delivered);
    harness
        .session
        .relay_offers
        .get_mut(&offer.offer.offer_id)
        .unwrap()
        .claims
        .expires_at = 1;

    let published = harness.publish_offers();
    assert!(harness
        .session
        .relay_offers
        .contains_key(&offer.offer.offer_id));
    assert!(published.iter().all(|candidate| {
        candidate.offer.offered_mode != LeaseMode::Takeover
            || candidate.offer.accepted_transfer_request_id.as_deref()
                == Some(request_id.as_str())
    }));

    // Prepared media can legitimately advance mutable owner/media
    // revisions while this accepted reservation is still setting up. The
    // physical call identity remains the suppression key; final activation
    // continues to require the original exact assistance fence.
    harness.session.last_snapshot_fingerprint = None;
    harness.session.last_snapshot_sent = None;
    let encoded = harness
        .session
        .snapshot_frame(&harness.radio)
        .unwrap()
        .unwrap();
    let mut advanced = serde_json::from_str::<PluginSnapshotFrame>(&encoded)
        .unwrap()
        .snapshot;
    advanced.owner_epoch += 1;
    advanced.remote_revision += 1;
    advanced.pending_mobile_offers.clear();
    let advanced = harness
        .session
        .attach_pending_offers(advanced, &harness.media.snapshot())
        .unwrap();
    assert!(advanced
        .pending_mobile_offers
        .iter()
        .all(|candidate| candidate.offer.offered_mode != LeaseMode::Takeover));

    let request = lease_request(&offer, "request_expired_offer_setup", "rtc_expired_offer");
    let granted = harness.post(&request);
    assert_eq!(
        harness.granted(&granted).status,
        PluginLeaseStatus::Provisional
    );
}

#[test]
fn terminal_rate_limits_retire_mobile_spent_transfer_offers() {
    let mut grants = full_relay_grants();
    grants.insert(Grant::AssistanceRespond);

    let mut answer_limited = RelayHarness::with_grants(grants.clone());
    let (request_id, _) = answer_limited.request_transfer("Please transfer me");
    let offer = answer_limited.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    answer_limited.session.relay_request_budget.insert(
        answer_limited.device_id.clone(),
        (Instant::now(), RELAY_REQUEST_BUDGET),
    );
    let refused = answer_limited.answer(&offer, "request_answer_limited");
    assert_eq!(answer_limited.rejection(&refused).code, "rate_limited");
    answer_limited.session.relay_request_budget.clear();
    let fresh = answer_limited.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    assert_ne!(fresh.offer.offer_id, offer.offer.offer_id);

    let mut lease_limited = RelayHarness::with_grants(grants);
    let (request_id, _) = lease_limited.request_transfer("Please transfer me");
    let offer = lease_limited.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    let accepted = lease_limited.answer(&offer, "request_lease_limited");
    lease_limited.settle(&accepted, TransportDelivery::Delivered);
    lease_limited.session.relay_request_budget.insert(
        lease_limited.device_id.clone(),
        (Instant::now(), RELAY_REQUEST_BUDGET),
    );
    let request = lease_request(&offer, "request_lease_limited", "rtc_lease_limited");
    let refused = lease_limited.post(&request);
    assert_eq!(lease_limited.rejection(&refused).code, "rate_limited");
    assert_eq!(
        lease_limited
            .session
            .assistance
            .pending_transfer("call_a", 1)
            .and_then(|pending| pending.accepted_by),
        None
    );
    lease_limited.session.relay_request_budget.clear();
    let fresh = lease_limited.transfer_offer_for(&request_id, MobileOfferSurface::InApp);
    assert_ne!(fresh.offer.offer_id, offer.offer.offer_id);
}

#[test]
fn a_dropped_monitor_grant_leaves_no_hidden_authority_and_can_retry() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Monitor);
    assert!(!harness.answer(&offer, "request_monitor_drop").is_empty());
    let request = lease_request(&offer, "request_monitor_drop", "rtc_monitor_drop");
    let granted = harness.post(&request);
    assert_eq!(harness.granted(&granted).status, PluginLeaseStatus::Granted);
    assert!(harness.session.leases.is_empty());
    assert_eq!(harness.session.relay_leases.len(), 1);

    harness.session.finish_relay_delivery(
        &granted[0],
        TransportDelivery::Dropped,
        &harness.media,
        &harness.radio,
    );

    assert!(harness.session.pending_relay_status.is_none());
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    let retry = harness.post(&request);
    assert!(
        !retry.is_empty(),
        "the accepted offer is restored for the exact retry"
    );
    assert_eq!(harness.session.relay_leases.len(), 1);
}

#[test]
fn a_dropped_relay_snapshot_is_retried_at_normal_poll_cadence() {
    let mut harness = RelayHarness::new();
    harness.session.last_snapshot_fingerprint = None;
    harness.session.last_snapshot_sent = None;
    let encoded = harness
        .session
        .snapshot_frame(&harness.radio)
        .expect("snapshot builds")
        .expect("live call publishes a snapshot");
    assert!(harness.session.last_snapshot_fingerprint.is_some());
    assert!(harness.session.last_snapshot_sent.is_some());

    let settled_at = Instant::now();
    harness.session.finish_relay_delivery(
        &encoded,
        TransportDelivery::Dropped,
        &harness.media,
        &harness.radio,
    );

    assert!(harness.session.last_snapshot_fingerprint.is_none());
    assert!(harness.session.last_snapshot_sent.is_none());
    assert!(
        harness.session.next_snapshot_poll > settled_at,
        "a dropped receipt is owed again without a hot retry loop"
    );
    assert!(
        harness.session.next_snapshot_poll
            <= settled_at + SNAPSHOT_POLL + Duration::from_millis(20)
    );
}

#[test]
fn relay_snapshots_are_targeted_and_projected_from_each_devices_grants() {
    let mut harness = RelayHarness::new();
    harness.session.last_snapshot_fingerprint = None;
    harness.session.last_snapshot_sent = None;
    let encoded = harness
        .session
        .snapshot_frame(&harness.radio)
        .expect("snapshot builds")
        .expect("live call publishes a snapshot");
    let mut raw: PluginSnapshotFrame =
        serde_json::from_str(&encoded).expect("raw snapshot decodes");
    assert!(
        raw.device_id.is_none(),
        "the internal snapshot is untargeted"
    );
    assert!(
        !raw.snapshot.pending_mobile_offers.is_empty(),
        "the raw relay snapshot carries invitations for projection"
    );
    raw.snapshot.caller = Some(CallerProjection {
        label: Some("Alice".into()),
        masked_number: Some("*******5678".into()),
    });
    raw.snapshot.captions = vec![Caption {
        caption_id: "caption_a".into(),
        speaker: "caller".into(),
        text: "my private appointment details".into(),
        occurred_at: "2026-07-19T00:00:00Z".into(),
        final_text: true,
    }];
    raw.snapshot.participants = vec![ParticipantPresence {
        participant_id: "rtc_owner".into(),
        mode: ParticipantMode::Talker,
        state: ParticipantState::Active,
        subject_id: Some("device_owner".into()),
        display_label: Some("Owner Companion".into()),
    }];
    raw.snapshot.audio_levels = Some(vec![NormalizedAudioLevel {
        source: AudioLevelSource::Companion,
        participant_id: Some("rtc_owner".into()),
        level_permille: 700,
    }]);
    let encoded = serde_json::to_string(&raw).unwrap();

    // State alone can render the call shell, but it cannot reveal caller
    // identity, captions, audio telemetry or invitations to claim media.
    harness
        .session
        .relay_peers
        .get_mut(&harness.device_id)
        .unwrap()
        .grants = HashSet::from([Grant::StateRead]);
    let projected = harness.session.relay_project_snapshot(&encoded).unwrap();
    assert_eq!(projected.len(), 1);
    let redacted: PluginSnapshotFrame = serde_json::from_str(&projected[0]).unwrap();
    assert_eq!(
        redacted.device_id.as_deref(),
        Some(harness.device_id.as_str())
    );
    assert!(redacted.snapshot.caller.is_none());
    assert!(redacted.snapshot.captions.is_empty());
    assert!(redacted.snapshot.audio_levels.is_none());
    assert!(redacted.snapshot.pending_mobile_offers.is_empty());

    // Adding only the monitor/caller/caption grants reveals those exact
    // fields and only monitor invitations. Takeover and consult offers for
    // the same device remain outside this projection.
    harness
        .session
        .relay_peers
        .get_mut(&harness.device_id)
        .unwrap()
        .grants = HashSet::from([
        Grant::StateRead,
        Grant::RtcSignal,
        Grant::Monitor,
        Grant::CallerRead,
        Grant::CaptionsRead,
    ]);
    let projected = harness.session.relay_project_snapshot(&encoded).unwrap();
    let permitted: PluginSnapshotFrame = serde_json::from_str(&projected[0]).unwrap();
    assert_eq!(permitted.snapshot.caller, raw.snapshot.caller);
    assert_eq!(permitted.snapshot.captions, raw.snapshot.captions);
    assert!(
        !permitted.snapshot.pending_mobile_offers.is_empty(),
        "the authorized monitor invitation remains"
    );
    assert!(permitted
        .snapshot
        .pending_mobile_offers
        .iter()
        .all(|offer| offer.offer.target_device_id == harness.device_id
            && offer.offer.offered_mode == LeaseMode::Monitor));

    harness
        .session
        .relay_peers
        .get_mut(&harness.device_id)
        .unwrap()
        .grants = HashSet::from([
        Grant::StateRead,
        Grant::ParticipantsRead,
        Grant::ParticipantIdentityRead,
        Grant::AudioLevelsRead,
    ]);
    let projected = harness.session.relay_project_snapshot(&encoded).unwrap();
    let current: PluginSnapshotFrame = serde_json::from_str(&projected[0]).unwrap();
    assert_eq!(current.snapshot.participants, raw.snapshot.participants);
    assert_eq!(current.snapshot.audio_levels, raw.snapshot.audio_levels);

    let mut expired = raw;
    expired.snapshot.remote_consent.expires_at = Some("2000-01-01T00:00:00Z".into());
    let expired = serde_json::to_string(&expired).unwrap();
    let projected = harness.session.relay_project_snapshot(&expired).unwrap();
    let redacted: PluginSnapshotFrame = serde_json::from_str(&projected[0]).unwrap();
    assert!(redacted.snapshot.participants.is_empty());
    assert!(redacted.snapshot.audio_levels.is_none());

    let mut malformed: PluginSnapshotFrame = serde_json::from_str(&encoded).unwrap();
    malformed.snapshot.remote_consent.expires_at = Some("not-rfc3339".into());
    let projected = harness
        .session
        .relay_project_snapshot(&serde_json::to_string(&malformed).unwrap())
        .unwrap();
    let redacted: PluginSnapshotFrame = serde_json::from_str(&projected[0]).unwrap();
    assert!(redacted.snapshot.participants.is_empty());
    assert!(redacted.snapshot.audio_levels.is_none());
}

#[test]
fn relay_assistance_waits_for_every_eligible_devices_current_snapshot_and_retries_drops() {
    let mut harness = RelayHarness::new();
    let grants = HashSet::from([Grant::StateRead, Grant::AssistanceRead]);
    harness
        .session
        .relay_peers
        .get_mut(&harness.device_id)
        .unwrap()
        .grants = grants.clone();
    harness.session.relay_peers.insert(
        "device_b".into(),
        RelayPeer {
            holder_key_thumbprint: "thumbprint_b".into(),
            session_nonce: "mobile_session_b".into(),
            grants,
        },
    );
    harness.session.relay_snapshot_event_id = Some("snapshot_current".into());
    assert!(!harness.session.relay_assistance_snapshot_ready());
    harness
        .session
        .relay_snapshot_delivered_devices
        .insert(harness.device_id.clone());
    assert!(
        !harness.session.relay_assistance_snapshot_ready(),
        "one successful projection cannot expose context before the other party has state"
    );
    harness
        .session
        .relay_snapshot_delivered_devices
        .insert("device_b".into());
    assert!(harness.session.relay_assistance_snapshot_ready());

    let encoded = json!({
        "kind": "assistance_request",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "eventId": "assistance_event_a",
        "requestId": "assistance_request_a",
        "callId": "call_a",
        "callEpoch": 1,
        "ownerEpoch": 0,
        "switchboardRevision": 0,
        "remoteRevision": 1,
        "question": "Can the manager help?",
        "expiresAt": unix_now().unwrap() + 60
    })
    .to_string();
    harness.session.finish_relay_delivery(
        &encoded,
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert_eq!(
        harness.session.last_assistance_request_sent.as_deref(),
        Some("assistance_request_a")
    );
    harness.session.finish_relay_delivery(
        &encoded,
        TransportDelivery::Dropped,
        &harness.media,
        &harness.radio,
    );
    assert!(
        harness.session.last_assistance_request_sent.is_none(),
        "a dropped aggregate delivery remains owed and retryable"
    );
}

#[test]
fn an_rtc_signal_whose_lease_token_was_not_minted_here_is_refused() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_rtc");
    let status = harness.granted(&granted);

    let plugin_id = harness.session.plugin_id.clone();
    let device_id = harness.device_id.clone();
    let signal = |lease_token: &str, lease_jti: &str| {
        json!({
            "kind": "rtc_signal",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "signalId": "signal_a",
            "pluginId": plugin_id,
            "deviceId": device_id,
            "leaseToken": lease_token,
            "leaseJti": lease_jti,
            "rtcSessionId": "rtc_a",
            "sdpRevision": 1,
            "transportGeneration": 1,
            "callId": "call_a",
            "callEpoch": status.lease.call_epoch,
            "ownerEpoch": status.lease.owner_epoch,
            "fence": status.lease.fence,
            "signal": {"type": "close", "reason": "done"}
        })
        .to_string()
    };

    // A token this plugin never minted names no lease it will honour.
    let refused = harness.post(&signal("forged-token", &status.lease.jti));
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");

    // The right token against the wrong lease identity is equally unknown.
    let refused = harness.post(&signal(&status.lease_token, "leasejti_invented"));
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");

    // ...and no peer was opened by any of it.
    assert!(harness.session.peers.is_empty());
}

#[test]
fn an_exact_rtc_redelivery_is_a_noop_but_changed_content_is_rejected() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_rtc_replay");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);

    // A close is enough to exercise the real accepted-signal path without
    // asking the native test peer to negotiate SDP. The first copy removes
    // the exact route; the second can only succeed through replay state.
    let binding = binding_for_claims(&status.lease);
    harness.session.peers.insert(
        status.lease.rtc_session_id.clone(),
        PeerRoute {
            binding,
            lease_jti: status.lease.jti.clone(),
            device_id: status.lease.device_id.clone(),
            sdp_revision: 1,
            transport_generation: 1,
            lease_ttl_ms: 20_000,
            connected: true,
            remote_audio_ready: false,
            remote_microphone_ready: false,
            transition_requested: false,
        },
    );
    let signal = |reason: &str| {
        serde_json::to_string(&MobileRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: status.lease.app_id.clone(),
            signal_id: "signal_replayed_close".into(),
            plugin_id: status.lease.plugin_id.clone(),
            device_id: status.lease.device_id.clone(),
            lease_token: status.lease_token.clone(),
            lease_jti: status.lease.jti.clone(),
            rtc_session_id: status.lease.rtc_session_id.clone(),
            sdp_revision: 1,
            transport_generation: 1,
            call_id: status.lease.call_id.clone(),
            call_epoch: status.lease.call_epoch,
            owner_epoch: status.lease.owner_epoch,
            fence: status.lease.fence,
            signal: RtcSignal::Close {
                reason: reason.into(),
            },
        })
        .expect("RTC close encodes")
    };

    let exact = signal("done");
    assert!(harness.post(&exact).is_empty());
    assert!(harness.session.peers.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert!(
        harness.post(&exact).is_empty(),
        "the exact redelivery is accepted without running Close twice"
    );

    let changed = harness.post(&signal("different reason"));
    assert_eq!(harness.rejection(&changed).code, "duplicate_signal");
    assert!(harness.session.peers.is_empty());

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_after_rtc_close",
        "idempotencyKey": "idem_after_rtc_close",
        "leaseToken": status.lease_token
    })
    .to_string();
    let after_close = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&after_close).code, "lease_unknown");
    let replacement = harness.claim(LeaseMode::Monitor, "request_after_close_replacement");
    assert_eq!(
        harness.granted(&replacement).status,
        PluginLeaseStatus::Granted
    );
}

#[test]
fn terminal_rtc_failure_retires_prepared_takeover_and_replays_the_refusal() {
    let mut harness = RelayHarness::new();
    let provisional_frames = harness.claim(LeaseMode::Takeover, "request_rtc_terminal");
    let provisional = harness.granted(&provisional_frames);
    harness.settle(&provisional_frames, TransportDelivery::Delivered);
    assert!(harness.session.prepared.is_some());

    let provisional_binding = binding_for_claims(&provisional.lease);
    harness
        .media
        .install_test_prepared_peer(provisional_binding.clone(), 10_000)
        .unwrap();
    harness
        .media
        .ack_prepare_human(&provisional_binding)
        .unwrap();
    let active_frames = harness
        .session
        .drain_media_events(&harness.media, &harness.radio)
        .unwrap();
    let active_encoded = active_frames
        .iter()
        .find(|encoded| {
            serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
        })
        .unwrap()
        .clone();
    let status: PluginLeaseStatusFrame = serde_json::from_str(&active_encoded).unwrap();
    harness.session.finish_relay_delivery(
        &active_encoded,
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert_eq!(status.lease.phase, LeasePhase::Active);

    // An active-lease Close before the exact native peer exists reaches
    // the authenticated RTC handler but cannot be applied. That is
    // terminal for this lease:
    // it must not remain heartbeat-renewable after its media transaction
    // failed.
    let signal = serde_json::to_string(&MobileRtcSignalFrame {
        kind: "rtc_signal".into(),
        schema_version: SCHEMA_VERSION,
        app_id: status.lease.app_id.clone(),
        signal_id: "signal_terminal_without_peer".into(),
        plugin_id: status.lease.plugin_id.clone(),
        device_id: status.lease.device_id.clone(),
        lease_token: status.lease_token.clone(),
        lease_jti: status.lease.jti.clone(),
        rtc_session_id: status.lease.rtc_session_id.clone(),
        sdp_revision: 2,
        transport_generation: 2,
        call_id: status.lease.call_id.clone(),
        call_epoch: status.lease.call_epoch,
        owner_epoch: status.lease.owner_epoch,
        fence: status.lease.fence,
        signal: RtcSignal::Close {
            reason: "permission_failed".into(),
        },
    })
    .unwrap();
    let refused = harness.post(&signal);
    assert_eq!(refused.len(), 2);
    assert!(serde_json::from_str::<PluginLeaseRevokeFrame>(&refused[0]).is_ok());
    assert!(serde_json::from_str::<PluginClaimRejectedFrame>(&refused[1]).is_ok());
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");
    let revocations = refused
        .iter()
        .filter_map(|encoded| serde_json::from_str::<PluginLeaseRevokeFrame>(encoded).ok())
        .collect::<Vec<_>>();
    assert_eq!(revocations.len(), 1);
    let revocation = &revocations[0];
    assert_eq!(revocation.device_id, status.lease.device_id);
    assert_eq!(revocation.lease_id, status.lease.lease_id);
    assert_eq!(revocation.lease_jti, status.lease.jti);
    assert_eq!(revocation.call_id, status.lease.call_id);
    assert_eq!(revocation.call_epoch, status.lease.call_epoch);
    assert_eq!(revocation.fence, status.lease.fence);
    assert_eq!(revocation.reason, "terminal_rtc_failure");

    let replay = harness.post(&signal);
    assert_eq!(replay, refused);
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive
    );
}

#[test]
fn terminal_rtc_failure_retries_dropped_exact_revoke_egress_until_delivered() {
    let mut harness = RelayHarness::new();
    let provisional_frames = harness.claim(LeaseMode::Takeover, "request_exact_rtc_failure");
    let provisional = harness.granted(&provisional_frames);
    harness.settle(&provisional_frames, TransportDelivery::Delivered);
    let binding = binding_for_claims(&provisional.lease);
    harness
        .media
        .install_test_prepared_peer(binding.clone(), 10_000)
        .unwrap();
    harness.session.peers.insert(
        provisional.lease.rtc_session_id.clone(),
        PeerRoute {
            binding,
            lease_jti: provisional.lease.jti.clone(),
            device_id: provisional.lease.device_id.clone(),
            sdp_revision: 1,
            transport_generation: 1,
            lease_ttl_ms: 10_000,
            connected: false,
            remote_audio_ready: false,
            remote_microphone_ready: false,
            transition_requested: false,
        },
    );
    harness
        .media
        .close_peer(
            &provisional.lease.rtc_session_id,
            "simulate signalling endpoint loss",
        )
        .unwrap();
    let signal = signed_mobile_ice(
        &provisional.lease,
        &provisional.lease_token,
        "signal_exact_rtc_failure",
        "candidate_jti_exact_rtc_failure",
        1,
        1,
    );

    // The terminal transition executes once and returns the exact revoke
    // first. Model the relay rejecting that outbound POST: no inbound
    // redelivery from the phone is involved in the retry below.
    let failed = harness.post(&signal);
    assert_eq!(failed.len(), 2);
    assert!(serde_json::from_str::<PluginLeaseRevokeFrame>(&failed[0]).is_ok());
    assert!(serde_json::from_str::<PluginClaimRejectedFrame>(&failed[1]).is_ok());
    assert_eq!(harness.rejection(&failed).code, "lease_unknown");
    let revocations = failed
        .iter()
        .filter_map(|encoded| serde_json::from_str::<PluginLeaseRevokeFrame>(encoded).ok())
        .collect::<Vec<_>>();
    assert_eq!(revocations.len(), 1);
    let revocation = &revocations[0];
    assert_eq!(revocation.device_id, provisional.lease.device_id);
    assert_eq!(revocation.lease_id, provisional.lease.lease_id);
    assert_eq!(revocation.lease_jti, provisional.lease.jti);
    assert_eq!(revocation.call_id, provisional.lease.call_id);
    assert_eq!(revocation.call_epoch, provisional.lease.call_epoch);
    assert_eq!(revocation.fence, provisional.lease.fence);
    assert_eq!(revocation.reason, "terminal_rtc_failure");
    assert!(harness.session.peers.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());

    harness.settle(&failed, TransportDelivery::Dropped);
    assert_eq!(harness.session.pending_relay_revocations.len(), 1);
    let due = harness
        .session
        .due_pending_relay_revocations(Instant::now() + SNAPSHOT_POLL);
    assert_eq!(due, vec![failed[0].clone()]);
    harness.session.prepare_relay_delivery(&due[0]);
    harness.session.finish_relay_delivery(
        &due[0],
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert!(harness.session.pending_relay_revocations.is_empty());

    // A later inbound redelivery is still answered byte-for-byte without
    // re-running teardown. If that replayed revoke is itself dropped, the
    // same egress ledger re-arms it and clears only on exact delivery.
    let replay = harness.post(&signal);
    assert_eq!(replay, failed);
    assert!(harness.session.peers.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
    harness.settle(&replay, TransportDelivery::Dropped);
    let replay_due = harness
        .session
        .due_pending_relay_revocations(Instant::now() + SNAPSHOT_POLL);
    assert_eq!(replay_due, vec![failed[0].clone()]);
    harness.session.prepare_relay_delivery(&replay_due[0]);
    harness.session.finish_relay_delivery(
        &replay_due[0],
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert!(harness.session.pending_relay_revocations.is_empty());
}

#[test]
fn terminal_rtc_failure_never_revokes_a_foreign_peer_named_by_the_frame() {
    let mut harness = RelayHarness::new();
    let provisional_frames = harness.claim(LeaseMode::Takeover, "request_foreign_rtc_failure");
    let provisional = harness.granted(&provisional_frames);
    harness.settle(&provisional_frames, TransportDelivery::Delivered);

    let foreign_rtc = "rtc_foreign".to_string();
    let mut foreign_binding = binding_for_claims(&provisional.lease);
    foreign_binding.rtc_session_id = foreign_rtc.clone();
    foreign_binding.device_id = "device_foreign".into();
    foreign_binding.lease_id = Some("lease_foreign".into());
    harness.session.peers.insert(
        foreign_rtc.clone(),
        PeerRoute {
            binding: foreign_binding,
            lease_jti: "lease_jti_foreign".into(),
            device_id: "device_foreign".into(),
            sdp_revision: 1,
            transport_generation: 1,
            lease_ttl_ms: 10_000,
            connected: true,
            remote_audio_ready: true,
            remote_microphone_ready: false,
            transition_requested: false,
        },
    );
    let signal = serde_json::to_string(&MobileRtcSignalFrame {
        kind: "rtc_signal".into(),
        schema_version: SCHEMA_VERSION,
        app_id: provisional.lease.app_id.clone(),
        signal_id: "signal_foreign_rtc_failure".into(),
        plugin_id: provisional.lease.plugin_id.clone(),
        device_id: provisional.lease.device_id.clone(),
        lease_token: provisional.lease_token.clone(),
        lease_jti: provisional.lease.jti.clone(),
        rtc_session_id: foreign_rtc.clone(),
        sdp_revision: 1,
        transport_generation: 1,
        call_id: provisional.lease.call_id.clone(),
        call_epoch: provisional.lease.call_epoch,
        owner_epoch: provisional.lease.owner_epoch,
        fence: provisional.lease.fence,
        signal: RtcSignal::Close {
            reason: "foreign route probe".into(),
        },
    })
    .unwrap();

    let failed = harness.post(&signal);
    assert_eq!(failed.len(), 2);
    assert!(serde_json::from_str::<PluginLeaseRevokeFrame>(&failed[0]).is_ok());
    assert!(serde_json::from_str::<PluginClaimRejectedFrame>(&failed[1]).is_ok());
    assert_eq!(harness.rejection(&failed).code, "lease_unknown");
    let revocation = failed
        .iter()
        .find_map(|encoded| serde_json::from_str::<PluginLeaseRevokeFrame>(encoded).ok())
        .expect("the authenticated lease gets an exact terminal notice");
    assert_eq!(revocation.device_id, provisional.lease.device_id);
    assert_eq!(revocation.lease_id, provisional.lease.lease_id);
    assert_eq!(revocation.lease_jti, provisional.lease.jti);
    assert_ne!(revocation.device_id, "device_foreign");
    assert_ne!(revocation.lease_id, "lease_foreign");
    assert!(
        harness.session.peers.contains_key(&foreign_rtc),
        "an authenticated device can retire only its recognised lease, never a peer named by untrusted rtcSessionId"
    );
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
}

#[test]
fn exact_late_prepared_ice_is_replay_fenced_and_cannot_touch_the_active_lease() {
    let mut harness = RelayHarness::new();
    let (prepared, active) =
        activate_takeover_with_retired_prepared_rtc(&mut harness, "request_late_prepared_ice");
    let current_before = harness
        .session
        .relay_leases
        .get(&active.lease.lease_id)
        .expect("the active stable lease is current")
        .clone();
    let active_claims_before = harness
        .session
        .leases
        .get(&active.lease.jti)
        .expect("active authority is committed")
        .clone();
    let peer_count_before = harness.session.peers.len();
    let signal = signed_mobile_ice(
        &prepared.lease,
        &prepared.lease_token,
        "signal_late_prepared_ice",
        "candidate_jti_late_prepared_ice",
        1,
        1,
    );

    assert!(
        harness.post(&signal).is_empty(),
        "an exact valid late candidate is consumed as a no-op"
    );
    assert_eq!(
        harness
            .session
            .relay_leases
            .get(&active.lease.lease_id)
            .expect("the current lease survives")
            .current_jti,
        current_before.current_jti
    );
    assert_eq!(
        harness
            .session
            .relay_leases
            .get(&active.lease.lease_id)
            .expect("the current lease survives")
            .token,
        current_before.token
    );
    assert_eq!(
        harness.session.leases.get(&active.lease.jti),
        Some(&active_claims_before)
    );
    assert_eq!(harness.session.leases.len(), 1);
    assert_eq!(harness.session.peers.len(), peer_count_before);
    let charged = harness
        .session
        .relay_rtc_signal_budget
        .get(&harness.device_id)
        .expect("the first late signal used the RTC lane")
        .1;
    assert!(
        harness.post(&signal).is_empty(),
        "an at-least-once redelivery is also a no-op"
    );
    assert_eq!(
        harness
            .session
            .relay_rtc_signal_budget
            .get(&harness.device_id)
            .unwrap()
            .1,
        charged,
        "replay lookup happens before the RTC lane is charged"
    );

    let revoke = json!({
        "kind": "lease_revoke",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_late_prepared_ice_revoke",
        "idempotencyKey": "idem_late_prepared_ice_revoke",
        "leaseToken": active.lease_token,
        "reason": "operator_return"
    })
    .to_string();
    assert!(harness.post(&revoke).is_empty());
    assert!(
        harness.session.retired_prepared_rtc.is_empty(),
        "stable-lease revoke also clears its old-generation tombstone"
    );
    assert!(harness.session.relay_replays.values().all(|replay| {
        !matches!(
            &replay.result,
            RelayReplayResult::RetiredPreparedRtcDropped { .. }
        )
    }));
}

#[test]
fn altered_or_expired_prepared_rtc_bindings_are_never_tombstone_authorized() {
    let mut harness = RelayHarness::new();
    let (prepared, active) = activate_takeover_with_retired_prepared_rtc(
        &mut harness,
        "request_retired_binding_limits",
    );
    let current_jti = active.lease.jti.clone();
    let current_token = active.lease_token.clone();

    // Even the approved mobile signer cannot widen the remembered route:
    // a changed owner epoch is a different binding and remains unknown.
    let mut altered = prepared.lease.clone();
    altered.owner_epoch = altered.owner_epoch.saturating_add(1);
    let refused = harness.post(&signed_mobile_ice(
        &altered,
        &prepared.lease_token,
        "signal_altered_prepared_ice",
        "candidate_jti_altered_prepared_ice",
        1,
        1,
    ));
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");
    assert_eq!(
        harness
            .session
            .relay_leases
            .get(&active.lease.lease_id)
            .unwrap()
            .current_jti,
        current_jti
    );

    harness
        .session
        .retired_prepared_rtc
        .get_mut(&prepared.lease.lease_id)
        .expect("the exact old generation is remembered")
        .expires_at = Instant::now();
    let refused = harness.post(&signed_mobile_ice(
        &prepared.lease,
        &prepared.lease_token,
        "signal_expired_prepared_ice",
        "candidate_jti_expired_prepared_ice",
        1,
        1,
    ));
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");
    assert!(harness.session.retired_prepared_rtc.is_empty());
    let current = harness
        .session
        .relay_leases
        .get(&active.lease.lease_id)
        .expect("rejecting the old binding leaves ACTIVE current");
    assert_eq!(current.current_jti, current_jti);
    assert_eq!(current.token, current_token);
    assert!(harness.session.leases.contains_key(&active.lease.jti));
}

#[test]
fn active_rebind_deadline_is_not_renewable_and_frees_the_claimant() {
    let mut harness = RelayHarness::new();
    let active = activate_takeover_without_replacement_peer(
        &mut harness,
        "request_active_rebind_timeout",
    );
    let deadline = harness
        .session
        .prepared
        .as_ref()
        .and_then(|prepared| prepared.active_rebind_deadline)
        .expect("active delivery starts a bounded replacement-peer window");

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_active_rebind_timeout_beat",
        "idempotencyKey": "idem_active_rebind_timeout_beat",
        "leaseToken": active.lease_token
    })
    .to_string();
    let renewed = harness.post(&heartbeat);
    assert_eq!(harness.granted(&renewed).status, PluginLeaseStatus::Renewed);
    harness.settle(&renewed, TransportDelivery::Delivered);
    assert_eq!(
        harness
            .session
            .prepared
            .as_ref()
            .and_then(|prepared| prepared.active_rebind_deadline),
        Some(deadline),
        "heartbeats cannot extend the media handoff deadline"
    );

    assert!(harness
        .session
        .expire_unbound_active_rebind(deadline + Duration::from_millis(1), &harness.media,));
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive
    );
    let refused = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");

    let replacement = harness.claim(
        LeaseMode::Takeover,
        "request_active_rebind_timeout_replacement",
    );
    assert_eq!(
        harness.granted(&replacement).status,
        PluginLeaseStatus::Provisional
    );
}

#[test]
fn media_reconciliation_retires_authority_when_the_terminal_event_is_not_drained() {
    let mut harness = RelayHarness::new();
    let active =
        activate_takeover_without_replacement_peer(&mut harness, "request_terminal_event_drop");
    let binding = binding_for_claims(&active.lease);
    harness
        .media
        .revoke(&binding, "simulated_terminal_event_drop")
        .expect("native media returns safely even if its event is not drained");
    assert_eq!(harness.session.relay_leases.len(), 1);

    assert_eq!(
        harness
            .session
            .reconcile_relay_media_authority(&harness.media),
        1
    );
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.prepared.is_none());

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_terminal_event_drop_beat",
        "idempotencyKey": "idem_terminal_event_drop_beat",
        "leaseToken": active.lease_token
    })
    .to_string();
    let refused = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");
}

#[test]
fn talk_readiness_timeout_matches_active_route_and_makes_heartbeat_terminal() {
    let mut harness = RelayHarness::new();
    let provisional_frames = harness.claim(LeaseMode::Takeover, "request_ready_timeout");
    let provisional = harness.granted(&provisional_frames);
    harness.settle(&provisional_frames, TransportDelivery::Delivered);

    let provisional_binding = binding_for_claims(&provisional.lease);
    harness
        .media
        .install_test_prepared_peer(provisional_binding.clone(), 10_000)
        .unwrap();
    harness
        .media
        .ack_prepare_human(&provisional_binding)
        .expect("test radio ACKs the non-mutating prepared Talk transition");
    let active_frames = harness
        .session
        .drain_media_events(&harness.media, &harness.radio)
        .unwrap();
    let active_encoded = active_frames
        .iter()
        .find(|encoded| {
            serde_json::from_str::<PluginLeaseStatusFrame>(encoded)
                .is_ok_and(|status| status.status == PluginLeaseStatus::Active)
        })
        .expect("preparation emits an active lease status")
        .clone();
    let active: PluginLeaseStatusFrame = serde_json::from_str(&active_encoded).unwrap();
    harness.session.finish_relay_delivery(
        &active_encoded,
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );

    let active_binding = binding_for_claims(&active.lease);
    harness.session.peers.insert(
        active.lease.rtc_session_id.clone(),
        PeerRoute {
            binding: active_binding.clone(),
            lease_jti: active.lease.jti.clone(),
            device_id: active.lease.device_id.clone(),
            sdp_revision: 2,
            transport_generation: 2,
            lease_ttl_ms: 20_000,
            connected: true,
            remote_audio_ready: true,
            remote_microphone_ready: false,
            transition_requested: false,
        },
    );
    let timeout = RemoteMediaEvent {
        sequence: 1,
        rtc_session_id: active_binding.rtc_session_id.clone(),
        call_id: active_binding.call_id.clone(),
        call_epoch: active_binding.call_epoch,
        owner_epoch: active_binding.owner_epoch,
        kind: RemoteMediaEventKind::Closed {
            reason: "talk_readiness_timeout".into(),
        },
    };
    assert!(
        harness
            .session
            .fail_route_if_current(&timeout, "talk_readiness_timeout", &harness.media)
            .unwrap()
            .is_some(),
        "the exact post-rotation binding retires the live gateway route"
    );
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive
    );

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_ready_timeout_beat",
        "idempotencyKey": "idem_ready_timeout_beat",
        "leaseToken": active.lease_token
    })
    .to_string();
    let refused = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");
}

#[test]
fn a_heartbeat_retry_with_the_previous_token_replays_current_renewal() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_heartbeat_replay");
    let original = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_heartbeat_replay_beat",
        "idempotencyKey": "idem_heartbeat_replay_beat",
        "leaseToken": original.lease_token
    })
    .to_string();
    let renewed = harness.post(&heartbeat);
    let renewed_status = harness.granted(&renewed);
    assert_eq!(renewed_status.status, PluginLeaseStatus::Renewed);
    assert_ne!(renewed_status.lease_token, original.lease_token);
    assert_ne!(renewed_status.lease.jti, original.lease.jti);
    harness.settle(&renewed, TransportDelivery::Delivered);

    // The retry still carries the token that was current when the request
    // was first made. Its idempotency record names the stable lease, so it
    // receives the current renewal rather than minting another rotation.
    assert_eq!(harness.post(&heartbeat), renewed);
    assert_eq!(harness.session.leases.len(), 1);
    assert!(harness
        .session
        .leases
        .contains_key(&renewed_status.lease.jti));

    let mut changed: Value = serde_json::from_str(&heartbeat).unwrap();
    changed["requestId"] = Value::String("request_changed".into());
    let refused = harness.post(&changed.to_string());
    assert_eq!(harness.rejection(&refused).code, "duplicate_request");
}

#[test]
fn a_renewed_takeover_keeps_exact_microphone_authority() {
    let mut harness = RelayHarness::new();
    let active =
        activate_takeover_without_replacement_peer(&mut harness, "request_renewed_mute");
    install_actionable_takeover_for_renewal_test(&mut harness, &active);

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_renewed_mute_beat",
        "idempotencyKey": "idem_renewed_mute_beat",
        "leaseToken": active.lease_token
    })
    .to_string();
    let renewed_frames = harness.post(&heartbeat);
    let renewed = harness.granted(&renewed_frames);
    assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
    harness.settle(&renewed_frames, TransportDelivery::Delivered);
    assert_eq!(
        harness
            .session
            .relay_leases
            .get(&renewed.lease.lease_id)
            .expect("the renewed relay lease stays current")
            .status,
        PluginLeaseStatus::Renewed
    );

    let remote = harness.media.snapshot();
    let switchboard_revision = harness.radio.switchboard_revision();
    let microphone_frame =
        |request_id: &str,
         idempotency_key: &str,
         lease_token: String,
         remote_revision: u64,
         muted: bool| MobileMicrophoneMuteFrame {
            kind: "microphone_mute".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            request_id: request_id.into(),
            idempotency_key: idempotency_key.into(),
            lease_token,
            rtc_session_id: renewed.lease.rtc_session_id.clone(),
            call_id: renewed.lease.call_id.clone(),
            call_epoch: renewed.lease.call_epoch,
            owner_epoch: renewed.lease.owner_epoch,
            switchboard_revision,
            remote_revision,
            fence: renewed.lease.fence,
            muted,
        };

    let stale = microphone_frame(
        "request_renewed_mute_stale",
        "idem_renewed_mute_stale",
        active.lease_token,
        remote.remote_revision,
        true,
    );
    let refused = harness.post(&serde_json::to_string(&stale).unwrap());
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");
    assert!(!harness.media.snapshot().microphone_muted);

    let mute = microphone_frame(
        "request_renewed_mute_on",
        "idem_renewed_mute_on",
        renewed.lease_token.clone(),
        remote.remote_revision,
        true,
    );
    let muted_frames = harness.post(&serde_json::to_string(&mute).unwrap());
    let muted: PluginMicrophoneMuteStatusFrame =
        serde_json::from_str(muted_frames.first().expect("mute is confirmed")).unwrap();
    assert!(muted.muted);
    assert_eq!(muted.lease_jti, renewed.lease.jti);
    assert!(harness.media.snapshot().microphone_muted);

    let after_mute = harness.media.snapshot();
    let unmute = microphone_frame(
        "request_renewed_mute_off",
        "idem_renewed_mute_off",
        renewed.lease_token,
        after_mute.remote_revision,
        false,
    );
    let unmuted_frames = harness.post(&serde_json::to_string(&unmute).unwrap());
    let unmuted: PluginMicrophoneMuteStatusFrame =
        serde_json::from_str(unmuted_frames.first().expect("unmute is confirmed")).unwrap();
    assert!(!unmuted.muted);
    assert_eq!(unmuted.lease_jti, renewed.lease.jti);
    assert!(!harness.media.snapshot().microphone_muted);
}

#[test]
fn a_dropped_renewal_does_not_extend_authority_and_can_be_retried() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_renewal_drop");
    let original = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);
    let original_entry = harness
        .session
        .relay_leases
        .values()
        .next()
        .expect("monitor relay lease exists")
        .clone();
    let original_claims = harness
        .session
        .leases
        .get(&original_entry.current_jti)
        .expect("monitor authority is committed")
        .clone();

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_renewal_drop_beat",
        "idempotencyKey": "idem_renewal_drop_beat",
        "leaseToken": original.lease_token
    })
    .to_string();
    let renewed = harness.post(&heartbeat);
    assert_eq!(harness.granted(&renewed).status, PluginLeaseStatus::Renewed);
    harness.settle(&renewed, TransportDelivery::Dropped);

    let after = harness
        .session
        .relay_leases
        .values()
        .next()
        .expect("the previous monitor lease remains current");
    assert_eq!(after.token, original_entry.token);
    assert_eq!(after.current_jti, original_entry.current_jti);
    assert_eq!(after.status, original_entry.status);
    assert_eq!(
        harness
            .session
            .leases
            .get(&after.current_jti)
            .expect("the old authority remains")
            .expires_at,
        original_claims.expires_at,
        "a status the device never received cannot extend its authority"
    );
    assert!(harness.session.pending_relay_status.is_none());

    let retry = harness.post(&heartbeat);
    assert_eq!(harness.granted(&retry).status, PluginLeaseStatus::Renewed);
    harness.settle(&retry, TransportDelivery::Dropped);
}

#[test]
fn an_expired_current_lease_cannot_be_renewed_or_resurrected() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_expired_renewal");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);
    let current_jti = harness
        .session
        .relay_leases
        .values()
        .next()
        .expect("monitor lease exists")
        .current_jti
        .clone();
    harness
        .session
        .leases
        .get_mut(&current_jti)
        .expect("monitor authority exists")
        .expires_at = unix_now().unwrap();

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_expired_renewal_beat",
        "idempotencyKey": "idem_expired_renewal_beat",
        "leaseToken": status.lease_token
    })
    .to_string();
    let refused = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&refused).code, "lease_expired");
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.pending_relay_status.is_none());
    let again = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&again).code, "lease_unknown");
}

#[test]
fn relay_replay_state_is_strictly_bounded() {
    let mut harness = RelayHarness::new();
    for index in 0..(MAX_RELAY_REPLAYS + 32) {
        harness.session.relay_record_replay(
            format!("test_replay_{index}"),
            format!("fingerprint_{index}"),
            harness.device_id.clone(),
            RelayReplayResult::RtcAccepted {
                mode: LeaseMode::Monitor,
            },
        );
    }
    assert_eq!(harness.session.relay_replays.len(), MAX_RELAY_REPLAYS);
    assert!(!harness
        .session
        .relay_replay_has_room("one_more_distinct_operation"));
    assert!(harness.session.relay_replay_has_room("test_replay_0"));
}

#[test]
fn a_dropped_active_status_revokes_the_provisional_authority() {
    let mut harness = RelayHarness::new();
    let provisional = harness.claim(LeaseMode::Takeover, "request_active_drop");
    let provisional_status = harness.granted(&provisional);
    harness.settle(&provisional, TransportDelivery::Delivered);

    let lease_id = provisional_status.lease.lease_id.clone();
    let mut active = provisional_status.lease.clone();
    active.phase = LeasePhase::Active;
    active.tracks = tracks_for(active.mode, LeasePhase::Active);
    active.owner_epoch = active.owner_epoch.saturating_add(1);
    active.jti = "leasejti_active_delivery_drop".into();
    active.expires_at = unix_now().unwrap() + RELAY_ACTIVE_LEASE_TTL;
    let token = harness
        .session
        .endpoint_authority
        .sign(&active.signing_bytes().expect("active claims canonicalize"));
    let encoded = harness
        .session
        .relay_lease_status(
            PluginLeaseStatus::Active,
            &harness.device_id,
            "request_active_drop",
            &token,
            active.clone(),
            unix_now().unwrap(),
        )
        .into_iter()
        .next()
        .expect("active status encodes");
    let prepared = harness
        .session
        .prepared
        .as_mut()
        .expect("the delivered provisional grant is armed");
    prepared.confirmed_owner_epoch = Some(active.owner_epoch);
    prepared.decision_sent = true;
    harness.session.pending_relay_status = Some(PendingRelayStatus::Active {
        encoded: encoded.clone(),
        lease_id,
        claims: active,
        token,
    });

    harness.session.finish_relay_delivery(
        &encoded,
        TransportDelivery::Dropped,
        &harness.media,
        &harness.radio,
    );
    assert!(harness.session.pending_relay_status.is_none());
    assert!(harness.session.prepared.is_none());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_leases.is_empty());
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive,
        "an undelivered active grant cannot strand caller ownership"
    );
}

#[test]
fn the_kill_switch_disables_offers_and_every_admitted_arm() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Takeover);

    // Flipping the switch is a complete revert to the behaviour that
    // shipped before this path existed.
    harness.session.relay_authority_enabled = false;

    assert!(
        harness.publish_offers().is_empty(),
        "no offer means the Companion's own selection refuses before it sends"
    );
    for encoded in [
        offer_answer(&offer, &harness.device_id, "request_off"),
        lease_request(&offer, "request_off", "rtc_a"),
    ] {
        assert!(harness.post(&encoded).is_empty());
    }
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.deferred_prepare.is_none());

    // The authenticated hello still works, so a Companion can still join
    // and watch: only the authority is withdrawn.
    let peer = plugin_thumbprint(&harness.session);
    let hello = mobile_hello(
        &harness.session,
        &approved_mobile_signing_key(),
        "device_a",
        "hello_jti_switch",
        &peer,
    );
    assert!(harness.post(&hello).is_empty());
    assert!(harness.session.relay_peers.contains_key("device_a"));
    // The re-greet signal is set only by a hello whose proof VERIFIED, so
    // it proves the hello was actually processed rather than merely
    // tolerated.
    assert!(harness.session.take_relay_regreet_party().is_some());
}

#[test]
fn offers_are_published_only_for_the_relay_carrier_and_a_live_call() {
    let mut harness = RelayHarness::new();
    assert!(!harness.publish_offers().is_empty());

    // The socket carrier has a gateway to mint these, and a snapshot
    // carrying ours there would be a second authority.
    harness.session.relay_carrier = false;
    assert!(harness.publish_offers().is_empty());
}

#[test]
fn switchboard_transition_publishes_state_without_media_offers_then_reopens() {
    let mut harness = RelayHarness::new();
    *harness.status.switch_in_flight.lock().unwrap() =
        Some(("test_switch".into(), Instant::now()));
    assert!(
        harness.publish_offers().is_empty(),
        "no caller-seizing invitation exists while CHLD topology is unsettled"
    );

    *harness.status.switch_in_flight.lock().unwrap() = None;
    assert!(
        !harness.publish_offers().is_empty(),
        "settling the switch triggers a fresh offer-bearing projection"
    );
}

#[test]
fn a_republished_offer_keeps_its_identity_until_the_call_state_moves() {
    let mut harness = RelayHarness::new();
    let first = harness.offer_for(LeaseMode::Takeover);

    // Re-minting per publish would hand the Companion a new offerId every
    // poll and turn an answer already in flight into a stale one.
    let again = harness.offer_for(LeaseMode::Takeover);
    assert_eq!(again.offer.offer_id, first.offer.offer_id);
    assert_eq!(again.offer_token, first.offer_token);

    // A real transition invalidates it, because the Companion filters an
    // offer against the exact snapshot it arrived in.
    harness.move_call("call_b");
    let moved = harness.offer_for(LeaseMode::Takeover);
    assert_ne!(moved.offer.offer_id, first.offer.offer_id);
    assert_eq!(moved.offer.call_id, "call_b");
}

#[test]
fn a_revoked_lease_stops_being_recognised_at_once() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_revoke");
    let status = harness.granted(&granted);
    harness.settle(&granted, TransportDelivery::Delivered);
    assert_eq!(harness.session.relay_leases.len(), 1);
    quiesce_publication(&mut harness.session);

    let revoke = json!({
        "kind": "lease_revoke",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_revoke_now",
        "idempotencyKey": "idem_revoke_now",
        "leaseToken": status.lease_token,
        "reason": "handing back"
    })
    .to_string();
    assert!(harness.post(&revoke).is_empty());

    // Both books must forget it together: a token still recognised by the
    // relay registry could be heartbeated back to life after the session
    // already withdrew the authority.
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.last_snapshot_fingerprint.is_none());
    assert!(harness.session.last_snapshot_sent.is_none());
    assert!(harness.session.next_snapshot_poll <= Instant::now());

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_revoked_beat",
        "idempotencyKey": "idem_revoked_beat",
        "leaseToken": status.lease_token
    })
    .to_string();
    let refused = harness.post(&heartbeat);
    assert_eq!(harness.rejection(&refused).code, "lease_unknown");
}

#[test]
fn a_redelivered_claim_cannot_mint_a_second_lease() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Monitor);
    assert!(!harness.answer(&offer, "request_dupe").is_empty());
    let request = lease_request(&offer, "request_dupe", "rtc_a");

    let first = harness.post(&request);
    assert!(!first.is_empty());
    harness.session.finish_relay_delivery(
        &first[0],
        TransportDelivery::Delivered,
        &harness.media,
        &harness.radio,
    );
    assert_eq!(harness.session.relay_leases.len(), 1);

    // The carrier can redeliver after an ambiguous commit. Replay the
    // current grant byte-for-byte; never mint a second lease.
    assert_eq!(harness.post(&request), first);
    assert_eq!(harness.session.relay_leases.len(), 1);
}

#[test]
fn a_flooding_device_is_refused_before_it_can_drive_minting() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Monitor);

    let mut refusals = 0;
    for index in 0..(RELAY_REQUEST_BUDGET + 4) {
        let frame = offer_answer(&offer, &harness.device_id, &format!("request_{index}"));
        let response = harness.post(&frame);
        if response
            .first()
            .and_then(|encoded| serde_json::from_str::<PluginClaimRejectedFrame>(encoded).ok())
            .is_some_and(|refusal| refusal.code == "rate_limited")
        {
            refusals += 1;
        }
    }
    assert!(
        refusals > 0,
        "a roster member that loops must not drive signing at whatever rate it likes"
    );
    assert!(harness.session.relay_leases.is_empty());
}

#[test]
fn a_normal_claim_and_candidate_burst_use_independent_device_lanes() {
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Monitor);
    let answer = offer_answer(&offer, &harness.device_id, "request_lane_split");
    assert!(!harness.post(&answer).is_empty());
    let request = lease_request(&offer, "request_lane_split", "rtc_lane_split");
    assert!(!harness.post(&request).is_empty());
    assert_eq!(
        harness
            .session
            .relay_request_budget
            .get(&harness.device_id)
            .expect("offer + lease spend the control lane")
            .1,
        2
    );
    assert!(harness.session.relay_rtc_signal_budget.is_empty());

    // The live incident contained sixteen inbound SDP/ICE signals across
    // the prepared and active negotiations.  That healthy burst must not
    // consume (or be consumed by) the offer/lease control allowance.
    for _ in 0..16 {
        assert!(!harness.session.relay_rtc_over_budget(&harness.device_id));
    }
    assert_eq!(
        harness
            .session
            .relay_request_budget
            .get(&harness.device_id)
            .unwrap()
            .1,
        2
    );
    assert!(
        !harness.session.relay_over_budget(&harness.device_id),
        "a later control remains inside its own allowance"
    );

    let control_before_replay = harness
        .session
        .relay_request_budget
        .get(&harness.device_id)
        .unwrap()
        .1;
    assert!(!harness.post(&answer).is_empty());
    assert_eq!(
        harness
            .session
            .relay_request_budget
            .get(&harness.device_id)
            .unwrap()
            .1,
        control_before_replay,
        "control replays are resolved before charging too"
    );
}

#[test]
fn rtc_trickle_is_bounded_per_device_without_spending_control_budget() {
    let mut harness = RelayHarness::new();
    for _ in 0..RELAY_RTC_SIGNAL_BUDGET {
        assert!(!harness.session.relay_rtc_over_budget(&harness.device_id));
    }
    assert!(harness.session.relay_rtc_over_budget(&harness.device_id));
    assert!(
        harness.session.relay_request_budget.is_empty(),
        "RTC floods cannot exhaust the control lane"
    );
    assert!(
        !harness.session.relay_rtc_over_budget("device_b"),
        "one device cannot spend another device's RTC allowance"
    );
}

#[test]
fn one_device_cannot_spend_another_devices_request_budget() {
    // The budget is per-device state, so it may only be spent by a claim
    // whose sender has been identified. Otherwise an approved Companion
    // could name a rival in `targetDeviceId`, exhaust its allowance, and
    // have the rival's own claims refused as flooding — locking a device
    // out of taking over a live call it is entitled to take.
    let mut harness = RelayHarness::new();
    let offer = harness.offer_for(LeaseMode::Monitor);
    let frame = offer_answer(&offer, &harness.device_id, "request_flood");

    for _ in 0..(RELAY_REQUEST_BUDGET * 3) {
        let refused = harness.post_as(&frame, Some("mobile:some-other-approved-device"));
        assert_eq!(harness.rejection(&refused).code, "device_unknown");
    }

    // The rightful holder still has its full allowance.
    assert!(
        !harness.answer(&offer, "request_rightful").is_empty(),
        "an impostor's traffic must not consume the real device's budget"
    );
}

fn plugin_thumbprint(session: &GatewaySession) -> String {
    session.endpoint_authority.endpoint_key.thumbprint.clone()
}

fn accept_test_mobile_hello(
    session: &mut GatewaySession,
    encoded: &str,
    party: Option<&str>,
) -> Result<(), WorkerError> {
    let media = RemoteMediaHandle::spawn().unwrap();
    let subject = serde_json::from_str::<MobileHello>(encoded)
        .ok()
        .map(|hello| hello.device_id);
    session.accept_mobile_hello(
        encoded,
        party,
        subject.as_deref(),
        &full_relay_grants(),
        &media,
    )
}

/// Leave publication in the state a quiet, already-running session reaches:
/// idle asserted once, nothing due for a minute.
fn quiesce_publication(session: &mut GatewaySession) {
    session.authoritative_idle = true;
    session.last_snapshot_fingerprint = Some("stale-fingerprint".into());
    session.last_snapshot_sent = Some(Instant::now());
    session.next_snapshot_poll = Instant::now() + Duration::from_secs(60);
}

fn publication_is_quiesced(session: &GatewaySession) -> bool {
    session.authoritative_idle
        && session.last_snapshot_fingerprint.is_some()
        && session.last_snapshot_sent.is_some()
        && session.next_snapshot_poll > Instant::now()
}

#[test]
fn approved_relay_hello_rearms_authoritative_publication_for_the_party_that_joined() {
    let mut session = test_gateway_session();
    let peer = plugin_thumbprint(&session);
    let encoded = mobile_hello(
        &session,
        &approved_mobile_signing_key(),
        "device_a",
        "hello_jti_1",
        &peer,
    );
    quiesce_publication(&mut session);
    session.last_assistance_request_sent = Some("assistance_pending".into());
    session.relay_snapshot_event_id = Some("snapshot_old".into());
    session
        .relay_snapshot_delivered_devices
        .insert("device_a".into());
    let party = approved_mobile_party();

    accept_test_mobile_hello(&mut session, &encoded, Some(&party))
        .expect("an approved Companion hello is admitted");

    // The carrier already registered the sender as a destination; this is
    // the half that makes the plugin actually speak to it.
    assert!(!session.authoritative_idle);
    assert!(session.last_snapshot_fingerprint.is_none());
    assert!(session.last_snapshot_sent.is_none());
    assert!(session.next_snapshot_poll <= Instant::now());

    // The behaviour that matters on a quiet line: authoritative state is
    // published again rather than waiting for the next call.
    assert!(session.idle_transition_frame().unwrap().is_some());

    // A fresh verified party is owed a new projected snapshot before the
    // still-pending assistance context can be delivered to it.
    assert!(session.last_assistance_request_sent.is_none());
    assert!(session.relay_snapshot_event_id.is_none());
    assert!(session.relay_snapshot_delivered_devices.is_empty());
}

#[test]
fn a_verified_hello_without_state_read_is_not_admitted() {
    let mut session = test_gateway_session();
    let peer = plugin_thumbprint(&session);
    let encoded = mobile_hello(
        &session,
        &approved_mobile_signing_key(),
        "device_a",
        "hello_jti_no_state",
        &peer,
    );
    let media = RemoteMediaHandle::spawn().unwrap();
    let refusal = session
        .accept_mobile_hello(
            &encoded,
            Some(&approved_mobile_party()),
            Some("device_a"),
            &HashSet::from([Grant::RtcSignal, Grant::Monitor]),
            &media,
        )
        .unwrap_err();

    assert!(refusal.message.contains("state access"), "{refusal:?}");
    assert!(session.relay_peers.is_empty());
    assert!(session.take_relay_verified_route().is_none());
}

#[test]
fn a_verified_rehello_without_state_read_revokes_existing_takeover_authority() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Takeover, "request_state_removed");
    harness.settle(&granted, TransportDelivery::Delivered);
    assert!(harness.session.prepared.is_some());
    assert_eq!(harness.session.leases.len(), 1);

    let peer = plugin_thumbprint(&harness.session);
    let encoded = mobile_hello(
        &harness.session,
        &approved_mobile_signing_key(),
        &harness.device_id,
        "hello_jti_state_removed",
        &peer,
    );
    let grants_without_state = HashSet::from([Grant::RtcSignal, Grant::Takeover]);
    let refusal = harness
        .session
        .accept_mobile_hello(
            &encoded,
            Some(&harness.party),
            Some(&harness.device_id),
            &grants_without_state,
            &harness.media,
        )
        .unwrap_err();

    assert!(refusal.message.contains("state access"), "{refusal:?}");
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.prepared.is_none());
    assert!(harness.session.deferred_prepare.is_none());
    assert!(harness
        .session
        .relay_peers
        .get(&harness.device_id)
        .is_some_and(|peer| peer.grants.is_empty()));
    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive
    );
}

#[test]
fn a_signed_hello_cannot_invent_a_device_id_outside_its_authenticated_subject() {
    let mut session = test_gateway_session();
    let peer = plugin_thumbprint(&session);
    let encoded = mobile_hello(
        &session,
        &approved_mobile_signing_key(),
        "device_squatted",
        "hello_jti_subject_mismatch",
        &peer,
    );
    quiesce_publication(&mut session);
    let media = RemoteMediaHandle::spawn().unwrap();

    let refusal = session
        .accept_mobile_hello(
            &encoded,
            Some(&approved_mobile_party()),
            Some("device_authenticated"),
            &full_relay_grants(),
            &media,
        )
        .unwrap_err();

    assert!(
        refusal.message.contains("authenticated admission"),
        "{refusal:?}"
    );
    assert!(session.relay_peers.is_empty());
    assert!(session.take_relay_verified_route().is_none());
    assert!(publication_is_quiesced(&session));
}

#[test]
fn same_session_grant_narrowing_revokes_only_unauthorized_modes() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_narrow_grants");
    harness.settle(&granted, TransportDelivery::Delivered);
    let peer = plugin_thumbprint(&harness.session);
    let signing_key = approved_mobile_signing_key();

    // A byte-equivalent admission on the same mobile session is a
    // re-introduction only; it must not touch existing authority.
    let unchanged = mobile_hello(
        &harness.session,
        &signing_key,
        &harness.device_id,
        "hello_jti_grants_unchanged",
        &peer,
    );
    harness
        .session
        .accept_mobile_hello(
            &unchanged,
            Some(&harness.party),
            Some(&harness.device_id),
            &full_relay_grants(),
            &harness.media,
        )
        .unwrap();
    assert_eq!(harness.session.relay_leases.len(), 1);
    assert_eq!(harness.session.leases.len(), 1);

    // Removing unrelated modes still leaves the authorized monitor alone.
    let still_monitor = mobile_hello(
        &harness.session,
        &signing_key,
        &harness.device_id,
        "hello_jti_grants_monitor_only",
        &peer,
    );
    harness
        .session
        .accept_mobile_hello(
            &still_monitor,
            Some(&harness.party),
            Some(&harness.device_id),
            &monitor_relay_grants(),
            &harness.media,
        )
        .unwrap();
    assert_eq!(harness.session.relay_leases.len(), 1);
    assert_eq!(harness.session.leases.len(), 1);

    // Once Monitor itself disappears the exact monitor lease is revoked;
    // a newly granted Takeover scope cannot preserve another mode.
    let no_monitor = mobile_hello(
        &harness.session,
        &signing_key,
        &harness.device_id,
        "hello_jti_grants_no_monitor",
        &peer,
    );
    harness
        .session
        .accept_mobile_hello(
            &no_monitor,
            Some(&harness.party),
            Some(&harness.device_id),
            &takeover_relay_grants(),
            &harness.media,
        )
        .unwrap();
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
}

#[test]
fn a_new_mobile_session_nonce_retires_all_prior_device_authority() {
    let mut harness = RelayHarness::new();
    let granted = harness.claim(LeaseMode::Monitor, "request_nonce_rotation");
    harness.settle(&granted, TransportDelivery::Delivered);
    let peer = plugin_thumbprint(&harness.session);
    let rotated = mobile_hello_with_nonce(
        &harness.session,
        &approved_mobile_signing_key(),
        &harness.device_id,
        "hello_jti_nonce_rotated",
        &peer,
        "mobile_session_device_a_rotated",
    );

    harness
        .session
        .accept_mobile_hello(
            &rotated,
            Some(&harness.party),
            Some(&harness.device_id),
            &full_relay_grants(),
            &harness.media,
        )
        .unwrap();
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    assert_eq!(
        harness
            .session
            .relay_peers
            .get(&harness.device_id)
            .unwrap()
            .session_nonce,
        "mobile_session_device_a_rotated"
    );
}

#[test]
fn an_admitted_relay_hello_makes_the_carrier_greet_that_party_again() {
    let mut session = test_gateway_session();
    let peer = plugin_thumbprint(&session);
    let signing_key = approved_mobile_signing_key();
    let thumbprint =
        EndpointPublicKey::from_ed25519_bytes(&signing_key.verifying_key().to_bytes())
            .thumbprint;
    let party = relay_party(&thumbprint);

    let hello = mobile_hello(&session, &signing_key, "device_a", "hello_jti_greet", &peer);
    accept_test_mobile_hello(&mut session, &hello, Some(&party))
        .expect("an approved Companion hello is admitted");

    assert_eq!(
        session.take_relay_verified_route(),
        Some(("device_a".to_string(), party.clone())),
        "only the verified hello creates the carrier's targeted route"
    );

    // Re-arming publication alone is not enough to go live. The Companion
    // DROPS authoritative state from a peer whose endpoint key it has not
    // seen proved, and it learns that proof only from our `plugin_hello` —
    // which the carrier prepends once per party per plugin session. A
    // Companion that restarts as a fresh process is the SAME party (its
    // endpoint key is on disk) but has lost the proof, so without retiring
    // its greeting it would receive every re-armed frame and drop all of
    // them until this plugin session ends.
    assert_eq!(
        session.take_relay_regreet_party().as_deref(),
        Some(relay_party(&thumbprint).as_str())
    );
    // Consumed once: the carrier re-greets on the next send, and a stale
    // signal would make every later publish carry a redundant hello.
    assert!(session.take_relay_regreet_party().is_none());
}

#[test]
fn a_refused_relay_hello_never_makes_the_plugin_reissue_its_hello() {
    let mut session = test_gateway_session();
    let peer = plugin_thumbprint(&session);

    // Correctly signed, just not by a key the owner approved.
    let media = RemoteMediaHandle::spawn().unwrap();
    let (radio, _control_rx) = crate::radio::RadioHandle::test_handle();
    session
        .handle_relay_peer_frame(
            &mobile_hello(
                &session,
                &SigningKey::from_bytes(&[11; 32]),
                "device_intruder",
                "hello_jti_greet_intruder",
                &peer,
            ),
            None,
            Some("device_intruder"),
            &full_relay_grants(),
            &media,
            &radio,
        )
        .expect("a forged hello never terminates the session");

    // The re-greet is driven by a proof that VERIFIED, so an unapproved
    // party cannot make the plugin reissue anything on demand.
    assert!(session.take_relay_regreet_party().is_none());
}

#[test]
fn a_replayed_relay_hello_is_refused_and_republishes_nothing() {
    let mut session = test_gateway_session();
    let peer = plugin_thumbprint(&session);
    let encoded = mobile_hello(
        &session,
        &approved_mobile_signing_key(),
        "device_a",
        "hello_jti_replay",
        &peer,
    );
    let party = approved_mobile_party();
    accept_test_mobile_hello(&mut session, &encoded, Some(&party)).unwrap();

    quiesce_publication(&mut session);
    let refusal = accept_test_mobile_hello(&mut session, &encoded, Some(&party)).unwrap_err();
    // Pin WHICH gate refused it: a replay must be caught by the jti cache,
    // not incidentally by an expired signature window.
    assert!(refusal.message.contains("replayed"), "{refusal:?}");
    assert!(publication_is_quiesced(&session));
}

#[test]
fn a_relay_hello_signed_outside_the_owner_approved_roster_is_refused() {
    let mut session = test_gateway_session();
    let peer = plugin_thumbprint(&session);
    // Internally consistent and correctly signed — just not by a key the
    // owner approved.
    let encoded = mobile_hello(
        &session,
        &SigningKey::from_bytes(&[11; 32]),
        "device_intruder",
        "hello_jti_2",
        &peer,
    );
    quiesce_publication(&mut session);

    let refusal =
        accept_test_mobile_hello(&mut session, &encoded, Some("mobile:intruder")).unwrap_err();
    // The proof itself is valid; it is the ROSTER that refuses it. Pinning
    // the reason keeps this from passing on a malformed-signature accident.
    assert!(
        refusal.message.contains("owner-approved roster"),
        "{refusal:?}"
    );
    assert!(publication_is_quiesced(&session));
}

#[test]
fn a_relay_hello_addressed_to_another_plugin_endpoint_is_refused() {
    let mut session = test_gateway_session();
    let elsewhere = EndpointPublicKey::from_ed25519_bytes(
        &SigningKey::from_bytes(&[12; 32]).verifying_key().to_bytes(),
    );
    let encoded = mobile_hello(
        &session,
        &approved_mobile_signing_key(),
        "device_a",
        "hello_jti_3",
        &elsewhere.thumbprint,
    );
    quiesce_publication(&mut session);

    let party = approved_mobile_party();
    let refusal = accept_test_mobile_hello(&mut session, &encoded, Some(&party)).unwrap_err();
    // This signer IS on the roster, so only the peer-binding check can
    // refuse it — which is the point of the check.
    assert!(
        refusal.message.contains("different plugin endpoint"),
        "{refusal:?}"
    );
    assert!(publication_is_quiesced(&session));
}

#[test]
fn unhandled_relay_frames_are_dropped_without_touching_the_session() {
    let mut session = test_gateway_session();
    let media = RemoteMediaHandle::spawn().unwrap();
    let (radio, _control_rx) = crate::radio::RadioHandle::test_handle();
    let grants = full_relay_grants();
    quiesce_publication(&mut session);

    // Gateway authority frames remain permanently non-actionable from an
    // untrusted peer, even though mobile assistance/end-caller requests
    // now have their own authenticated relay translations.
    for encoded in [
        json!({"kind": "claim_proposal", "schemaVersion": SCHEMA_VERSION}).to_string(),
        json!({"kind": "claim_decision", "schemaVersion": SCHEMA_VERSION}).to_string(),
        "{ this is not json".to_string(),
    ] {
        assert!(session
            .handle_relay_peer_frame(&encoded, None, None, &grants, &media, &radio)
            .expect("relay peer traffic never terminates the session")
            .is_empty());
    }
    assert!(publication_is_quiesced(&session));

    // Reported once per distinct kind, so a Companion emitting one on a
    // timer cannot wrap the log ring during a call.
    assert!(session
        .handle_relay_peer_frame(
            &"{ this is not json".to_string(),
            None,
            None,
            &grants,
            &media,
            &radio,
        )
        .is_ok());
    assert_eq!(session.dropped_relay_kinds.len(), 3);
}

#[test]
fn the_relay_carrier_never_lets_peer_traffic_terminate_a_session_the_socket_still_refuses() {
    let media = RemoteMediaHandle::spawn().unwrap();
    let (radio, _control_rx) = crate::radio::RadioHandle::test_handle();
    let peer = plugin_thumbprint(&test_gateway_session());
    let grants = full_relay_grants();
    let hostile = [
        json!({"kind": "lease_request", "schemaVersion": SCHEMA_VERSION}).to_string(),
        json!({"kind": "error", "schemaVersion": SCHEMA_VERSION, "code": "boom", "message": "x"})
            .to_string(),
        json!({"kind": "claim_proposal", "schemaVersion": 9999}).to_string(),
        "{ this is not json".to_string(),
    ];

    for encoded in &hostile {
        // The carrier that carries untrusted peers: refuse the frame, keep
        // the session.
        let mut relay_session = test_gateway_session();
        assert!(relay_session
            .handle_inbound(
                encoded,
                &media,
                &radio,
                true,
                Some("mobile:unknown"),
                Some("device_unknown"),
                Some(&grants),
            )
            .is_ok());

        // The carrier that carries trusted gateway infrastructure: a
        // violation still means the session is broken. Unchanged.
        let mut socket_session = test_gateway_session();
        assert!(socket_session
            .handle_inbound(encoded, &media, &radio, false, None, None, None)
            .is_err());
    }

    // The one actionable kind, on each carrier: admitted over the relay,
    // and still an unsupported frame on the socket, where a real gateway
    // never sends it.
    let mut relay_session = test_gateway_session();
    let hello = mobile_hello(
        &relay_session,
        &approved_mobile_signing_key(),
        "device_a",
        "hello_jti_4",
        &peer,
    );
    quiesce_publication(&mut relay_session);
    let party = approved_mobile_party();
    assert!(relay_session
        .handle_inbound(
            &hello,
            &media,
            &radio,
            true,
            Some(&party),
            Some("device_a"),
            Some(&grants),
        )
        .is_ok());
    assert!(!publication_is_quiesced(&relay_session));

    let mut socket_session = test_gateway_session();
    assert!(socket_session
        .handle_inbound(&hello, &media, &radio, false, None, None, None)
        .is_err());
}

#[test]
fn bootstrap_is_strict_and_debug_output_never_contains_bearer() {
    let value = bootstrap_value();
    let bootstrap = CompanionBootstrap::parse(&value).unwrap();
    let rendered = format!("{bootstrap:?}");
    assert!(!rendered.contains("top-secret-bearer"));
    assert!(!rendered.contains("ticket=hidden"));
    let endpoint = normalize_gateway_url(bootstrap.gateway_url.as_deref().unwrap()).unwrap();
    assert_eq!(endpoint.path(), "/custom/v2/realtime");

    let mut extra = value;
    extra["unexpected"] = json!(true);
    assert!(CompanionBootstrap::parse(&extra).is_err());
}

#[test]
fn identity_only_bootstrap_starts_with_brokered_admission_refresh() {
    let mut value = bootstrap_value();
    let object = value.as_object_mut().unwrap();
    object.remove("gatewayUrl");
    object.remove("accessToken");
    object.remove("appId");
    object.remove("iceServers");
    object.remove("relayOnly");

    let bootstrap = CompanionBootstrap::parse(&value).unwrap();
    assert!(bootstrap.gateway_url.is_none());
    assert!(bootstrap.access_token.is_none());
    assert!(bootstrap.app_id.is_none());
    let startup = GatewayStartup::from_bootstrap(bootstrap).unwrap();
    assert!(matches!(
        startup,
        GatewayStartup::Managed {
            app_id: None,
            plugin_id,
            initial: None,
            ..
        } if plugin_id == "aokie"
    ));

    let mut torn = value;
    torn["gatewayUrl"] = json!("wss://gateway.example.test/v2/realtime");
    assert!(CompanionBootstrap::parse(&torn)
        .unwrap_err()
        .contains("all present or all absent"));
}

#[test]
fn managed_admission_accepts_current_formlogic_ice_shape_and_pins_identity() {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    assert_eq!(credentials.app_id, "app_a");
    assert_eq!(credentials.plugin_id, "aokie");
    assert!(credentials.relay_only);
    assert!(credentials.turn_credential_expires_at.is_some());
    assert_eq!(credentials.ice_servers.len(), 1);
    assert_eq!(
        credentials.ice_servers[0].urls,
        vec!["turns:turn.example.test:5349"]
    );
    assert_eq!(credentials.ice_servers[0].username, "ephemeral");
    assert_eq!(credentials.ice_servers[0].credential, "credential");
    let rendered = format!("{credentials:?}");
    assert!(!rendered.contains("secret-value"));

    let wrong = admission("app_b", "other", &authority)
        .into_credentials(Some("app_a"), "aokie", authority.clone())
        .unwrap_err();
    assert!(matches!(wrong.kind, WorkerErrorKind::Rebootstrap));

    let different_key = EndpointPublicKey::from_ed25519_bytes(
        &SigningKey::from_bytes(&[99; 32]).verifying_key().to_bytes(),
    );
    let mut wrong_endpoint = admission("app_a", "aokie", &authority);
    wrong_endpoint.endpoint_public_key = different_key;
    let error = wrong_endpoint
        .into_credentials(Some("app_a"), "aokie", authority.clone())
        .unwrap_err();
    assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

    let mut missing_endpoint = admission_value("app_a", "aokie", &authority);
    missing_endpoint
        .as_object_mut()
        .unwrap()
        .remove("endpointPublicKey");
    assert!(serde_json::from_value::<AdmissionResponse>(missing_endpoint).is_err());
}

#[test]
fn managed_admission_requires_every_formlogic_ice_policy_member() {
    let authority = test_authority();
    for member in ["iceServers", "relayOnly", "turnCredentialExpiresAt"] {
        let mut value = admission_value("app_a", "aokie", &authority);
        value.as_object_mut().unwrap().remove(member);
        assert!(
            serde_json::from_value::<AdmissionResponse>(value).is_err(),
            "missing {member} must fail closed"
        );
    }
    for member in ["urls", "username", "credential"] {
        let mut value = admission_value("app_a", "aokie", &authority);
        value["iceServers"][0]
            .as_object_mut()
            .unwrap()
            .remove(member);
        assert!(
            serde_json::from_value::<AdmissionResponse>(value).is_err(),
            "missing ICE server {member} must fail closed"
        );
    }

    let mut null_server_expiry = admission_value("app_a", "aokie", &authority);
    null_server_expiry["iceServers"][0]["expiresAt"] = Value::Null;
    assert!(serde_json::from_value::<AdmissionResponse>(null_server_expiry).is_err());

    let mut direct = admission_value("app_a", "aokie", &authority);
    direct["iceServers"] = json!([]);
    direct["relayOnly"] = json!(false);
    direct["turnCredentialExpiresAt"] = Value::Null;
    let credentials = serde_json::from_value::<AdmissionResponse>(direct)
        .unwrap()
        .into_credentials(Some("app_a"), "aokie", authority)
        .unwrap();
    assert!(credentials.ice_servers.is_empty());
    assert!(!credentials.relay_only);
    assert_eq!(credentials.turn_credential_expires_at, None);
}

#[test]
fn managed_admission_rejects_inconsistent_or_unsafe_turn_policy() {
    let authority = test_authority();

    let mut mismatch = admission_value("app_a", "aokie", &authority);
    mismatch["turnCredentialExpiresAt"] =
        json!(mismatch["turnCredentialExpiresAt"].as_u64().unwrap() + 1);
    let error = serde_json::from_value::<AdmissionResponse>(mismatch)
        .unwrap()
        .into_credentials(Some("app_a"), "aokie", authority.clone())
        .unwrap_err();
    assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

    let mut relay_without_turn = admission_value("app_a", "aokie", &authority);
    relay_without_turn["iceServers"] = json!([{
        "urls": ["stun:stun.example.test:3478"],
        "username": "",
        "credential": ""
    }]);
    relay_without_turn["turnCredentialExpiresAt"] = Value::Null;
    let error = serde_json::from_value::<AdmissionResponse>(relay_without_turn)
        .unwrap()
        .into_credentials(Some("app_a"), "aokie", authority.clone())
        .unwrap_err();
    assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

    let mut missing_expiry = admission_value("app_a", "aokie", &authority);
    missing_expiry["iceServers"][0]
        .as_object_mut()
        .unwrap()
        .remove("expiresAt");
    let error = serde_json::from_value::<AdmissionResponse>(missing_expiry)
        .unwrap()
        .into_credentials(Some("app_a"), "aokie", authority.clone())
        .unwrap_err();
    assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));

    let now = unix_now().unwrap();
    for unsafe_expiry in [
        now + MIN_TURN_CREDENTIAL_TTL_SECONDS,
        now + MAX_TURN_CREDENTIAL_TTL_SECONDS + 60,
    ] {
        let mut value = admission_value("app_a", "aokie", &authority);
        value["iceServers"][0]["expiresAt"] = json!(unsafe_expiry);
        value["turnCredentialExpiresAt"] = json!(unsafe_expiry);
        let error = serde_json::from_value::<AdmissionResponse>(value)
            .unwrap()
            .into_credentials(Some("app_a"), "aokie", authority.clone())
            .unwrap_err();
        assert!(matches!(error.kind, WorkerErrorKind::Rebootstrap));
    }
}

#[test]
fn managed_admission_bounds_socket_lifetime_by_turn_expiry() {
    let authority = test_authority();
    let now = unix_now().unwrap();
    let turn_expiry = now + 40;
    let mut value = admission_value("app_a", "aokie", &authority);
    value["iceServers"][0]["expiresAt"] = json!(turn_expiry);
    value["turnCredentialExpiresAt"] = json!(turn_expiry);
    let credentials = serde_json::from_value::<AdmissionResponse>(value)
        .unwrap()
        .into_credentials(Some("app_a"), "aokie", authority)
        .unwrap();
    assert!(credentials.lifetime <= Duration::from_secs(30));
    assert!(credentials.lifetime >= Duration::from_secs(20));
}

#[test]
fn lease_modes_map_to_immutable_native_bindings() {
    let now = unix_now().unwrap();
    let claims = LeaseClaims {
        aud: LEASE_AUDIENCE.into(),
        app_id: "app_a".into(),
        plugin_id: "aokie".into(),
        device_id: "device_a".into(),
        plugin_key_thumbprint: "plugin_key_a".into(),
        mobile_key_thumbprint: "mobile_key_a".into(),
        call_id: "call_a".into(),
        call_epoch: 7,
        owner_epoch: 3,
        mode: LeaseMode::Takeover,
        phase: LeasePhase::Prepared,
        tracks: tracks_for(LeaseMode::Takeover, LeasePhase::Prepared),
        expires_at: now + 20,
        lease_id: "lease_stable".into(),
        jti: "lease_jti".into(),
        fence: 9,
        session_nonce: "mobile_session".into(),
        rtc_session_id: "rtc_a".into(),
    };
    assert_eq!(claims.tracks, vec![MediaTrack::PstnIn]);
    let prepared = binding_for_claims(&claims);
    assert_eq!(prepared.mode, MediaMode::PreparedTalk);
    assert!(!prepared.mode.may_transmit_to_caller());

    let mut active = claims;
    active.phase = LeasePhase::Active;
    active.owner_epoch += 1;
    active.tracks = tracks_for(LeaseMode::Takeover, LeasePhase::Active);
    let talk = binding_for_claims(&active);
    assert_eq!(talk.mode, MediaMode::Talk);
    assert!(talk.mode.may_transmit_to_caller());
}

#[test]
fn automatic_takeover_terminal_events_revoke_prepared_and_active_authority() {
    for (phase, event_kind) in [
        (
            LeasePhase::Prepared,
            RemoteMediaEventKind::ReturningToAokie {
                reason: "sco_unavailable".into(),
            },
        ),
        (
            LeasePhase::Active,
            RemoteMediaEventKind::Closed {
                reason: "physical_call_changed".into(),
            },
        ),
    ] {
        let mut session = test_gateway_session();
        let mut event = install_takeover_route(&mut session, phase);
        event.kind = event_kind;
        let media = RemoteMediaHandle::spawn().unwrap();

        let reason = match &event.kind {
            RemoteMediaEventKind::ReturningToAokie { reason }
            | RemoteMediaEventKind::Closed { reason } => reason.clone(),
            _ => unreachable!(),
        };
        let encoded = session
            .fail_route_if_current(&event, &reason, &media)
            .unwrap()
            .expect("a current failed route must revoke its lease");
        let frame: PluginLeaseRevokeFrame = serde_json::from_str(&encoded).unwrap();
        frame.validate().unwrap();
        assert_eq!(frame.lease_id, "lease_stable");
        assert_eq!(
            frame.lease_jti,
            if phase == LeasePhase::Prepared {
                "lease_prepared_jti"
            } else {
                "lease_active_jti"
            }
        );
        assert_eq!(frame.reason, reason);
        assert!(session.peers.is_empty());
        assert!(session.leases.is_empty());
        assert!(session.prepared.is_none());
    }
}

#[test]
fn authoritative_operator_return_does_not_emit_a_second_plugin_revoke() {
    let mut session = test_gateway_session();
    let mut event = install_takeover_route(&mut session, LeasePhase::Active);
    event.kind = RemoteMediaEventKind::ReturningToAokie {
        reason: "operator_return".into(),
    };
    let media = RemoteMediaHandle::spawn().unwrap();
    session
        .handle_lease_revoked(
            LeaseRevokedNotice {
                kind: "lease_revoked".into(),
                schema_version: SCHEMA_VERSION,
                app_id: "app_a".into(),
                device_id: "device_a".into(),
                lease_id: "lease_stable".into(),
                lease_jti: "lease_active_jti".into(),
                call_id: "call_a".into(),
                call_epoch: 7,
                fence: 9,
                reason: "operator_return".into(),
            },
            &media,
        )
        .unwrap();

    assert!(session
        .fail_route_if_current(&event, "operator_return", &media)
        .unwrap()
        .is_none());
    assert!(session.peers.is_empty());
    assert!(session.leases.is_empty());
    assert!(session.prepared.is_none());
}

#[test]
fn stale_terminal_event_cannot_revoke_a_newer_takeover_route() {
    let mut session = test_gateway_session();
    let mut event = install_takeover_route(&mut session, LeasePhase::Active);
    event.owner_epoch -= 1;
    let media = RemoteMediaHandle::spawn().unwrap();

    assert!(session
        .fail_route_if_current(&event, "physical_call_changed", &media)
        .unwrap()
        .is_none());
    assert!(session.peers.contains_key("rtc_a"));
    assert!(session.leases.contains_key("lease_active_jti"));
    assert!(session.prepared.is_some());
}

#[test]
fn caller_mask_never_exposes_more_than_four_digits() {
    assert_eq!(mask_number("+61 412 345 678"), Some("***5678".into()));
    assert_eq!(mask_number("private"), None);
}

#[test]
fn caller_end_result_preserves_every_physical_fence_and_typed_failure() {
    let execute = PluginEndCallerExecuteFrame {
        kind: "end_caller_execute".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        operation_id: "operation_a".into(),
        confirmation_id: "confirmation_a".into(),
        device_id: "device_a".into(),
        call_id: "call_a".into(),
        call_epoch: 7,
        owner_epoch: 4,
        switchboard_revision: 11,
        remote_revision: 13,
        lease_id: "media_a".into(),
        lease_jti: "lease_a".into(),
        fence: 9,
    };
    let failure = encode_end_caller_failure(
        &execute,
        "physical_fence_stale",
        "the physical call changed",
    )
    .unwrap();
    let frame: PluginEndCallerResultFrame = serde_json::from_str(&failure).unwrap();
    frame.validate().unwrap();
    assert_eq!(frame.outcome, EndCallerOutcome::Failed);
    assert_eq!(frame.operation_id, execute.operation_id);
    assert_eq!(frame.call_epoch, execute.call_epoch);
    assert_eq!(frame.owner_epoch, execute.owner_epoch);
    assert_eq!(frame.switchboard_revision, execute.switchboard_revision);
    assert_eq!(frame.remote_revision, execute.remote_revision);
    assert_eq!(frame.fence, execute.fence);
}

#[test]
fn relay_caller_end_after_renewal_queues_one_physical_hangup() {
    let mut grants = full_relay_grants();
    grants.insert(Grant::EndCaller);
    let mut harness = RelayHarness::with_grants(grants);
    let active =
        activate_takeover_without_replacement_peer(&mut harness, "request_end_caller_owner");
    install_actionable_takeover_for_renewal_test(&mut harness, &active);

    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "request_end_before_challenge_beat",
        "idempotencyKey": "idem_end_before_challenge_beat",
        "leaseToken": active.lease_token.clone()
    })
    .to_string();
    let renewed_frames = harness.post(&heartbeat);
    let renewed = harness.granted(&renewed_frames);
    assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
    harness.settle(&renewed_frames, TransportDelivery::Delivered);
    assert_eq!(renewed.lease.lease_id, active.lease.lease_id);
    assert_ne!(renewed.lease.jti, active.lease.jti);

    let remote = harness.media.snapshot();
    let switchboard_revision = harness.radio.switchboard_revision();

    let prepare_request_id = "request_end_prepare";
    let prepare = MobileEndCallerChallengeRequestFrame {
        kind: "end_caller_challenge_request".into(),
        schema_version: SCHEMA_VERSION,
        app_id: harness.session.app_id.clone(),
        request_id: prepare_request_id.into(),
        idempotency_key: "idem_end_prepare".into(),
        lease_token: renewed.lease_token.clone(),
        call_id: renewed.lease.call_id.clone(),
        call_epoch: renewed.lease.call_epoch,
        owner_epoch: renewed.lease.owner_epoch,
        switchboard_revision,
        remote_revision: remote.remote_revision,
        fence: renewed.lease.fence,
    };
    let challenge_frames =
        harness.post(&serde_json::to_string(&prepare).expect("prepare encodes"));
    let challenge: EndCallerChallengeFrame = serde_json::from_str(
        challenge_frames
            .first()
            .expect("the relay returns a confirmation challenge"),
    )
    .expect("challenge decodes");
    assert_eq!(challenge.request_id, prepare_request_id);
    assert_eq!(renewed.lease.lease_id, challenge.lease_id);

    let stale_confirm = MobileEndCallerConfirmFrame {
        kind: "end_caller_confirm".into(),
        schema_version: SCHEMA_VERSION,
        app_id: harness.session.app_id.clone(),
        request_id: "request_end_confirm_stale".into(),
        idempotency_key: "idem_end_confirm_stale".into(),
        confirmation_id: challenge.confirmation_id.clone(),
        nonce: challenge.nonce.clone(),
        lease_token: active.lease_token.clone(),
        call_id: challenge.call_id.clone(),
        call_epoch: challenge.call_epoch,
        owner_epoch: challenge.owner_epoch,
        switchboard_revision: challenge.switchboard_revision,
        remote_revision: challenge.remote_revision,
        fence: challenge.fence,
    };
    let stale_refusal =
        harness.post(&serde_json::to_string(&stale_confirm).expect("stale confirm encodes"));
    assert_eq!(
        harness.rejection(&stale_refusal).code,
        "not_active_takeover_owner"
    );
    assert!(matches!(
        harness._control_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert!(harness
        .session
        .relay_end_caller_challenges
        .contains_key(&challenge.confirmation_id));

    // The native client deliberately mints a fresh operation/request ID
    // for the destructive confirmation. Confirmation ID + nonce + the
    // exact active lease and physical fences bind it to this preparation;
    // the request ID is an idempotency/routing identity, not authority.
    let confirm_request_id = "request_end_confirm";
    assert_ne!(confirm_request_id, prepare_request_id);
    let confirm = MobileEndCallerConfirmFrame {
        kind: "end_caller_confirm".into(),
        schema_version: SCHEMA_VERSION,
        app_id: harness.session.app_id.clone(),
        request_id: confirm_request_id.into(),
        idempotency_key: "idem_end_confirm".into(),
        confirmation_id: challenge.confirmation_id.clone(),
        nonce: challenge.nonce,
        lease_token: renewed.lease_token.clone(),
        call_id: challenge.call_id,
        call_epoch: challenge.call_epoch,
        owner_epoch: challenge.owner_epoch,
        switchboard_revision: challenge.switchboard_revision,
        remote_revision: challenge.remote_revision,
        fence: challenge.fence,
    };
    let submitted = harness.post(&serde_json::to_string(&confirm).expect("confirm encodes"));
    let submitted: Value = serde_json::from_str(
        submitted
            .first()
            .expect("the relay acknowledges the queued physical hangup"),
    )
    .expect("submission acknowledgement decodes");
    assert_eq!(submitted["kind"], "end_caller_submitted");
    assert_eq!(submitted["requestId"], confirm_request_id);
    assert_eq!(submitted["confirmationId"], challenge.confirmation_id);
    assert_eq!(submitted["accepted"], true);
    let submitted_operation_id = submitted["operationId"]
        .as_str()
        .expect("the submission names its physical operation")
        .to_owned();

    let physical_reply = match harness
        ._control_rx
        .try_recv()
        .expect("one physical hangup command is queued")
    {
        crate::radio::RadioControl::EndCallerFromCompanion { request, reply } => {
            assert_eq!(request.call_id, active.lease.call_id);
            assert_eq!(request.device_id, harness.device_id);
            assert_eq!(request.lease_id, active.lease.lease_id);
            assert_eq!(request.fence, active.lease.fence);
            reply
        }
        _ => panic!("the queued command must be the caller hangup"),
    };
    assert!(matches!(
        harness._control_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    assert!(
        harness
            .session
            .drain_end_caller_results(&harness.media)
            .unwrap()
            .is_empty(),
        "queue admission is not physical completion"
    );
    physical_reply.send(Ok(())).unwrap();
    let completed = harness
        .session
        .drain_end_caller_results(&harness.media)
        .unwrap();
    assert_eq!(completed.len(), 1);
    let completed: PluginEndCallerResultFrame =
        serde_json::from_str(&completed[0]).expect("completion result decodes");
    assert_eq!(completed.outcome, EndCallerOutcome::Completed);
    assert_eq!(completed.operation_id, submitted_operation_id);
}

#[test]
fn socket_end_caller_waits_for_the_same_physical_result_channel() {
    let mut harness = RelayHarness::new();
    let active = activate_takeover_without_replacement_peer(
        &mut harness,
        "request_socket_end_caller_owner",
    );
    let active_binding = binding_for_claims(&active.lease);
    harness
        .media
        .install_test_active_talk_peer(active_binding, 20_000)
        .expect("the test route reaches exact active physical ownership");
    let remote = harness.media.snapshot();
    let execute = PluginEndCallerExecuteFrame {
        kind: "end_caller_execute".into(),
        schema_version: SCHEMA_VERSION,
        app_id: harness.session.app_id.clone(),
        operation_id: "operation_socket_end".into(),
        confirmation_id: "confirmation_socket_end".into(),
        device_id: harness.device_id.clone(),
        call_id: active.lease.call_id.clone(),
        call_epoch: active.lease.call_epoch,
        owner_epoch: active.lease.owner_epoch,
        switchboard_revision: harness.radio.switchboard_revision(),
        remote_revision: remote.remote_revision,
        lease_id: active.lease.lease_id.clone(),
        lease_jti: active.lease.jti.clone(),
        fence: active.lease.fence,
    };
    execute.validate().unwrap();
    let immediate = harness
        .session
        .handle_inbound(
            &serde_json::to_string(&execute).unwrap(),
            &harness.media,
            &harness.radio,
            false,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(immediate.is_empty());
    let physical_reply = match harness._control_rx.try_recv().unwrap() {
        crate::radio::RadioControl::EndCallerFromCompanion { request, reply } => {
            assert_eq!(request.call_id, execute.call_id);
            reply
        }
        _ => panic!("the socket command must use the physical caller-ending lane"),
    };
    assert!(harness
        .session
        .drain_end_caller_results(&harness.media)
        .unwrap()
        .is_empty());
    physical_reply.send(Ok(())).unwrap();
    let completed = harness
        .session
        .drain_end_caller_results(&harness.media)
        .unwrap();
    let completed: PluginEndCallerResultFrame =
        serde_json::from_str(completed.first().unwrap()).unwrap();
    assert_eq!(completed.outcome, EndCallerOutcome::Completed);
    assert_eq!(completed.operation_id, execute.operation_id);
}

#[test]
fn managed_beta_allows_only_numeric_loopback_plain_ws() {
    let numeric = normalize_gateway_url("ws://127.0.0.1:18787/v2/realtime");
    if cfg!(feature = "managed-beta-driver") || cfg!(debug_assertions) {
        assert!(numeric.is_ok());
    } else {
        assert!(numeric.is_err());
    }
    assert!(normalize_gateway_url("ws://192.168.1.40:18787/v2/realtime").is_err());
    assert!(normalize_gateway_url("ws://gateway.example.test/v2/realtime").is_err());
    if cfg!(feature = "managed-beta-driver") && !cfg!(debug_assertions) {
        assert!(normalize_gateway_url("ws://localhost:18787/v2/realtime").is_err());
    }
}

#[test]
fn relay_urls_keep_their_path_and_refuse_unsafe_advertisements() {
    let frames = normalize_relay_url(
        "https://api.example.test/api/aokie-companion/relay/frames",
        "framesUrl",
    )
    .unwrap();
    // The mailbox path is authoritative: normalize_gateway_url's
    // /v2/realtime rewrite would destroy it.
    assert_eq!(frames.path(), "/api/aokie-companion/relay/frames");
    assert_ne!(frames.path(), "/v2/realtime");

    assert!(
        normalize_relay_url("https://api.example.test/relay?since=4", "streamUrl").is_ok(),
        "an existing query is not a reason to refuse the endpoint"
    );
    assert!(
        normalize_relay_url("https://user:pass@api.example.test/relay", "framesUrl").is_err()
    );
    assert!(normalize_relay_url("https://api.example.test/relay#part", "framesUrl").is_err());
    assert!(normalize_relay_url("/api/aokie-companion/relay/frames", "framesUrl").is_err());
    assert!(normalize_relay_url("wss://api.example.test/relay", "framesUrl").is_err());
    assert!(normalize_relay_url("http://public.example.test/relay", "framesUrl").is_err());

    // The live install serves plain http on .local names, so managed-beta
    // builds accept exactly those and nothing wider.
    let local = normalize_relay_url("http://api.formlogic.local/api/relay/frames", "framesUrl");
    let loopback = normalize_relay_url("http://127.0.0.1:17872/api/relay/frames", "framesUrl");
    if cfg!(feature = "managed-beta-driver") {
        assert!(local.is_ok());
        assert!(loopback.is_ok());
        assert_eq!(local.unwrap().path(), "/api/relay/frames");
    } else {
        assert!(local.is_err());
        assert!(loopback.is_err());
    }
}

#[test]
fn admission_tolerates_the_relay_member_and_defaults_to_the_socket() {
    let authority = test_authority();

    let socket_only = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    assert!(
        socket_only.relay.is_none(),
        "an admission without the member keeps the untouched WebSocket path"
    );
    assert_eq!(socket_only.transport_label(), "websocket");

    let mut advertised = admission_value("app_a", "aokie", &authority);
    advertised["relay"] = json!({
        "challengeUrl": "https://api.example.test/api/aokie-companion/relay/challenge",
        "framesUrl": "https://api.example.test/api/aokie-companion/relay/frames",
        "streamUrl": "https://api.example.test/api/aokie-companion/relay/stream"
    });
    let response: AdmissionResponse =
        serde_json::from_value(advertised.clone()).expect("the relay member is tolerated");
    let credentials = response
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    let relay = credentials.relay.as_ref().expect("relay endpoints survive");
    assert_eq!(
        relay.stream_url,
        "https://api.example.test/api/aokie-companion/relay/stream"
    );
    assert_eq!(credentials.transport_label(), "relay");
    assert!(!format!("{credentials:?}").contains("aokie-adm-v2.secret-value"));

    // A split-origin or unsafe advertisement degrades to the socket
    // instead of failing the admission the live line depends on.
    let mut split = advertised.clone();
    split["relay"]["streamUrl"] = json!("https://elsewhere.example.test/relay/stream");
    let degraded: AdmissionResponse = serde_json::from_value(split).unwrap();
    assert!(degraded
        .into_credentials(None, "aokie", authority.clone())
        .unwrap()
        .relay
        .is_none());

    // A server that grows the advertisement keeps this build on the relay:
    // the transport hint is additive, so an unknown member is ignored
    // rather than failing the admission the live line depends on.
    let mut grown = advertised.clone();
    grown["relay"]["pollUrl"] =
        json!("https://api.example.test/api/aokie-companion/relay/frames");
    let tolerated: AdmissionResponse =
        serde_json::from_value(grown).expect("an added relay member is not fatal");
    assert!(tolerated
        .into_credentials(None, "aokie", authority.clone())
        .unwrap()
        .relay
        .is_some());

    // A reshaped advertisement this build cannot use degrades to the
    // socket — it must never cost the whole admission.
    let mut reshaped = advertised.clone();
    reshaped["relay"] = json!({"framesUrl": "https://api.example.test/relay/frames"});
    let degraded: AdmissionResponse =
        serde_json::from_value(reshaped).expect("a reshaped relay member is not fatal");
    assert!(degraded
        .into_credentials(None, "aokie", authority.clone())
        .unwrap()
        .relay
        .is_none());

    // The admission document itself stays strict: tolerance is scoped to
    // the additive transport hint, not to the security envelope.
    let mut unknown = advertised;
    unknown["mailboxUrl"] = json!("https://api.example.test/relay/mailbox");
    assert!(serde_json::from_value::<AdmissionResponse>(unknown).is_err());
}

#[test]
fn the_admission_request_asks_for_the_relay_carrier_this_build_can_actually_open() {
    let authority = test_authority();
    let params = admission_request_params(Some("app_a"), "aokie", &authority).unwrap();

    // Desktop forwards the optional `relay` member ONLY to a build that
    // declares it, and strips it otherwise. Without this the carrier is
    // unreachable: every admission arrives relay-less and the plugin sits
    // on the WebSocket gateway forever, looking exactly like a backend that
    // never advertised.
    let declared = params.get("supportedTransports");
    if cfg!(feature = "voice") {
        assert_eq!(
            declared,
            Some(&json!([RELAY_TRANSPORT])),
            "the relay carrier only activates when the plugin asks for it"
        );
    } else {
        assert!(
            declared.is_none(),
            "a build with no relay carrier must keep the pre-relay request shape"
        );
    }

    // The rest of the request is the pre-relay contract, unchanged.
    assert_eq!(params.get("pluginId"), Some(&json!("aokie")));
    assert_eq!(params.get("appId"), Some(&json!("app_a")));
    assert_eq!(
        params.get("peerRosterHash"),
        Some(&json!(authority.roster_hash))
    );
    assert!(admission_request_params(None, "aokie", &authority)
        .unwrap()
        .get("appId")
        .is_none());
}

#[test]
fn endpoint_hello_is_identical_whichever_transport_fetched_the_challenge() {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    let now = unix_now().unwrap();
    let challenge = EndpointChallengeFrame {
        kind: "endpoint_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        subject_id: "aokie".into(),
        role: AdmissionRole::Plugin,
        connection_id: "relay_c0ffee".into(),
        challenge_nonce: "challenge_abc123".into(),
        admission_jti: "jti_abc123".into(),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        expires_at: now + 30,
    };

    let (socket_hello, socket_nonce) = endpoint_hello(&challenge, &credentials, now).unwrap();
    let (relay_hello, relay_nonce) = endpoint_hello(&challenge, &credentials, now).unwrap();

    // Only the per-connection nonces differ; everything the gateway binds
    // the connection to comes from the challenge itself.
    assert_ne!(socket_nonce, relay_nonce);
    assert_eq!(socket_hello.app_id, relay_hello.app_id);
    assert_eq!(socket_hello.plugin_id, relay_hello.plugin_id);
    for hello in [&socket_hello, &relay_hello] {
        hello.validate().unwrap();
        let claims = &hello.endpoint_proof.claims;
        assert_eq!(claims.connection_id, challenge.connection_id);
        assert_eq!(claims.challenge_nonce, challenge.challenge_nonce);
        assert_eq!(claims.admission_jti, challenge.admission_jti);
        assert_eq!(claims.expires_at, challenge.expires_at);
        assert_eq!(
            claims.approved_peer_key_thumbprints,
            authority.approved_thumbprints()
        );
        assert!(claims.expected_peer_key_thumbprint.is_none());
    }

    // A challenge minted for another identity is refused on either carrier.
    let mut foreign = challenge.clone();
    foreign.app_id = "app_b".into();
    assert!(endpoint_hello(&foreign, &credentials, now).is_err());

    // The plugin role must never be handed a mobile's peer expectation.
    let mut peered = challenge;
    peered.expected_peer_key_thumbprint = Some("mobile-thumbprint".into());
    assert!(endpoint_hello(&peered, &credentials, now).is_err());
}

#[test]
fn a_relay_regreeting_refreshes_an_expired_proof_without_rotating_the_session() {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    let refresh_now = unix_now().unwrap();
    let original_now = refresh_now.saturating_sub(40);
    let logical_session = "plugin_session_live_takeover";
    let original_challenge = EndpointChallengeFrame {
        kind: "endpoint_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        subject_id: "aokie".into(),
        role: AdmissionRole::Plugin,
        connection_id: "relay_original".into(),
        challenge_nonce: "challenge_original".into(),
        admission_jti: "admission_original".into(),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        expires_at: original_now + 30,
    };
    let original = endpoint_hello_for_session(
        &original_challenge,
        &credentials,
        original_now,
        logical_session,
    )
    .unwrap();
    assert_eq!(
        original.endpoint_proof.verify(refresh_now),
        Err(V2ProtocolError::Expired),
        "the cached proof reproduces the live failure after its 30-second window"
    );

    let fresh_challenge = EndpointChallengeFrame {
        connection_id: "relay_refreshed".into(),
        challenge_nonce: "challenge_refreshed".into(),
        admission_jti: "admission_refreshed".into(),
        expires_at: refresh_now + 30,
        ..original_challenge
    };
    let refreshed = endpoint_hello_for_session(
        &fresh_challenge,
        &credentials,
        refresh_now,
        logical_session,
    )
    .unwrap();

    refreshed.validate().unwrap();
    refreshed.endpoint_proof.verify(refresh_now).unwrap();
    assert_eq!(refreshed.session_nonce, logical_session);
    assert_eq!(
        refreshed.endpoint_proof.claims.session_nonce,
        logical_session
    );
    assert_eq!(
        refreshed.endpoint_proof.claims.connection_id,
        "relay_refreshed"
    );
    assert_ne!(
        refreshed.endpoint_proof.claims.jti,
        original.endpoint_proof.claims.jti
    );
}

#[cfg(feature = "voice")]
#[tokio::test]
async fn refreshed_relay_greeting_is_sent_before_the_rearmed_state() {
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    let now = unix_now().unwrap();
    let logical_session = "plugin_session_live_takeover";
    let original_now = now.saturating_sub(40);
    let original_challenge = EndpointChallengeFrame {
        kind: "endpoint_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        subject_id: "aokie".into(),
        role: AdmissionRole::Plugin,
        connection_id: "relay_original".into(),
        challenge_nonce: "challenge_original".into(),
        admission_jti: "admission_original".into(),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        expires_at: original_now + 30,
    };
    let original = endpoint_hello_for_session(
        &original_challenge,
        &credentials,
        original_now,
        logical_session,
    )
    .unwrap();
    let original_jti = original.endpoint_proof.claims.jti.clone();

    let fresh_challenge = EndpointChallengeFrame {
        connection_id: "relay_refreshed".into(),
        challenge_nonce: "challenge_refreshed".into(),
        admission_jti: "admission_refreshed".into(),
        expires_at: now + 30,
        ..original_challenge
    };
    let challenge_response = fresh_challenge.clone();
    let posts = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured_posts = posts.clone();
    let router = axum::Router::new()
        .route(
            "/challenge",
            axum::routing::get(move || {
                let challenge = challenge_response.clone();
                async move { axum::Json(challenge) }
            }),
        )
        .route(
            "/frames",
            axum::routing::get(|| async { axum::Json(json!({"frames": [], "lastSeq": 0})) })
                .post(move |axum::Json(body): axum::Json<Value>| {
                    let captured_posts = captured_posts.clone();
                    async move {
                        captured_posts.lock().unwrap().push(body);
                        axum::http::StatusCode::OK
                    }
                }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let endpoints = RelayEndpoints {
        challenge_url: format!("http://{address}/challenge"),
        frames_url: format!("http://{address}/frames"),
        stream_url: format!("http://{address}/stream"),
    };
    let (mut channel, _) = crate::companion_relay::RelayChannel::connect(
        &endpoints,
        &credentials.token,
        &credentials.app_id,
        &credentials.plugin_id,
        authority.approved_thumbprints(),
    )
    .await
    .unwrap();
    channel.arm(serde_json::to_string(&original).unwrap());
    let party = relay_party(&authority.approved_thumbprints()[0]);
    channel.authorize_route("device_a", &party, &HashSet::from([Grant::StateRead]));
    let mut transport = GatewayTransport::Relay(channel);
    let state = "{\"kind\":\"plugin_snapshot\",\"deviceId\":\"device_a\"}";

    // Establish the bug's starting point: this party was greeted while the
    // original proof was current, but the cached document is now expired.
    transport.send_text(state).await.unwrap();
    posts.lock().unwrap().clear();

    let mut tasks = RelayGreetingTasks::default();
    tasks.schedule(
        party.clone(),
        transport.regreeting_request().unwrap(),
        credentials.clone(),
        logical_session.into(),
    );
    let greeting = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(result) = tasks.take_finished().await.into_iter().next() {
                break result.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("fresh challenge completes");
    assert!(transport.install_regreeting(greeting));
    transport.send_text(state).await.unwrap();

    let posted = posts.lock().unwrap();
    assert_eq!(posted.len(), 1);
    let frames = posted[0]["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2, "fresh hello must precede re-armed state");
    let hello: PluginHello = serde_json::from_value(frames[0].clone()).unwrap();
    hello.endpoint_proof.verify(unix_now().unwrap()).unwrap();
    assert_eq!(hello.session_nonce, logical_session);
    assert_eq!(hello.endpoint_proof.claims.session_nonce, logical_session);
    assert_eq!(
        hello.endpoint_proof.claims.connection_id,
        fresh_challenge.connection_id
    );
    assert_ne!(hello.endpoint_proof.claims.jti, original_jti);
    assert_eq!(frames[1]["kind"], "plugin_snapshot");
    server.abort();
}

#[test]
fn admission_domain_change_returns_caller_and_drops_all_lease_continuity() {
    let mut harness = RelayHarness::new();
    let active =
        activate_takeover_without_replacement_peer(&mut harness, "request_domain_change_reset");
    assert_eq!(active.lease.phase, LeasePhase::Active);
    assert_eq!(harness.session.relay_leases.len(), 1);
    let authority = harness.session.endpoint_authority.clone();
    let refreshed = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();

    harness
        .session
        .apply_admission_rotation(
            &refreshed,
            "plugin_session_new_domain".into(),
            false,
            &harness.media,
        )
        .unwrap();

    assert_eq!(
        harness.media.snapshot().service_mode,
        LocalServiceMode::AokieActive
    );
    assert_eq!(
        harness.session.plugin_session_nonce,
        "plugin_session_new_domain"
    );
    assert!(harness.session.relay_leases.is_empty());
    assert!(harness.session.leases.is_empty());
    assert!(harness.session.relay_peers.is_empty());
    assert!(harness.session.peers.is_empty());
}

#[tokio::test]
async fn delayed_admission_rotation_keeps_heartbeats_ahead_of_lease_expiry() {
    let mut harness = RelayHarness::new();
    let active = activate_takeover_without_replacement_peer(
        &mut harness,
        "request_admission_rotation_nonblocking",
    );
    assert_eq!(active.lease.phase, LeasePhase::Active);
    let logical_session = harness.session.plugin_session_nonce.clone();

    // Put the predecessor lease one second from expiry. The delayed
    // replacement represents the broker + endpoint challenge/open that
    // used to run synchronously on this same authority path.
    let now = unix_now().unwrap();
    harness
        .session
        .leases
        .get_mut(&active.lease.jti)
        .expect("the active lease is current")
        .expires_at = now + 1;
    let host_rpc = HostRpc::new();
    let (request_id, _line, response) = host_rpc.begin("companion.admission", json!({}));
    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();
    let mut rotation = AdmissionRotationTask {
        host_rpc: host_rpc.clone(),
        generation: 0,
        phase: Some(AdmissionRotationPhase::Broker(PendingAdmissionBroker {
            request_id,
            response,
            started_at: Instant::now(),
            expected_app_id: credentials.app_id.clone(),
            plugin_id: credentials.plugin_id.clone(),
            endpoint_authority: credentials.endpoint_authority.clone(),
            status: Arc::new(Mutex::new(GatewayStatusSnapshot::starting())),
            attempt: 0,
            predecessor_domain: AdmissionCarrierDomain::WebSocket,
        })),
    };
    assert!(rotation.is_pending());

    let polled_at = Instant::now();
    assert!(rotation.take_finished().await.is_none());
    assert!(
        polled_at.elapsed() < Duration::from_millis(50),
        "polling an unfinished rotation must not inherit its delay"
    );

    // The heartbeat is handled while the replacement remains in flight,
    // rotating the lease beyond its old deadline. Sweeping at a synthetic
    // time after that old deadline therefore keeps authority alive. The
    // former inline refresh could not read this frame before the sweep.
    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "heartbeat_during_admission_rotation",
        "idempotencyKey": "idem_heartbeat_during_admission_rotation",
        "leaseToken": active.lease_token
    })
    .to_string();
    let renewed_frames = harness.post(&heartbeat);
    let renewed = harness.granted(&renewed_frames);
    assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
    assert_eq!(renewed.lease.phase, LeasePhase::Active);
    harness.settle(&renewed_frames, TransportDelivery::Delivered);
    assert!(rotation.is_pending());
    assert_eq!(
        harness.session.expire_relay_leases(now + 2, &harness.media),
        0,
        "the queued heartbeat must renew before the old lease deadline is swept"
    );
    assert_eq!(harness.session.relay_leases.len(), 1);
    assert!(harness.session.leases.contains_key(&renewed.lease.jti));
    assert_eq!(harness.session.plugin_session_nonce, logical_session);

    assert!(host_rpc.try_route_response(&json!({
        "id": request_id,
        "error": {"code": -32000, "message": "synthetic broker refusal"}
    })));
    let failure = match rotation
        .take_finished()
        .await
        .expect("the broker response completes the rotation")
    {
        Ok(_) => panic!("the synthetic broker refusal unexpectedly opened a transport"),
        Err(error) => error,
    };
    assert_eq!(failure.kind, WorkerErrorKind::Rebootstrap);
    assert_eq!(harness.session.relay_leases.len(), 1);
    assert!(harness.session.leases.contains_key(&renewed.lease.jti));
}

#[cfg(feature = "voice")]
#[tokio::test]
async fn delayed_real_relay_rotation_opens_off_the_authority_path() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let authority = test_authority();
    let now = unix_now().unwrap();
    let challenge = EndpointChallengeFrame {
        kind: "endpoint_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        subject_id: "aokie".into(),
        role: AdmissionRole::Plugin,
        connection_id: "relay_admission_rotation".into(),
        challenge_nonce: "challenge_admission_rotation".into(),
        admission_jti: "admission_rotation".into(),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        expires_at: now + 30,
    };
    let tail_requests = Arc::new(AtomicUsize::new(0));
    let seen_tail_requests = tail_requests.clone();
    let router = axum::Router::new()
        .route(
            "/challenge",
            axum::routing::get(move || {
                let challenge = challenge.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    axum::Json(challenge)
                }
            }),
        )
        .route(
            "/frames",
            axum::routing::get(move || {
                seen_tail_requests.fetch_add(1, Ordering::SeqCst);
                async { axum::Json(json!({"frames": [], "lastSeq": 0})) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let endpoints = RelayEndpoints {
        challenge_url: format!("http://{address}/challenge"),
        frames_url: format!("http://{address}/frames"),
        stream_url: format!("http://{address}/stream"),
    };
    let mut credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();
    // Local test servers deliberately bypass production URL admission;
    // the production decoder's HTTPS/managed-beta policy has its own
    // contract tests. Everything from replacement open onward is real.
    credentials.relay = Some(endpoints);

    let host_rpc = HostRpc::new();
    let status = Arc::new(Mutex::new(GatewayStatusSnapshot {
        configured: true,
        connected: true,
        phase: GatewayConnectionPhase::Connected,
        reconnect_attempt: 0,
        last_error: None,
        changed_at: aokie_core::events::now_iso8601(),
    }));
    let rotation_credentials = credentials.clone();
    let delayed_open = tokio::spawn(async move {
        let (transport, plugin_session_nonce) =
            GatewayTransport::open_replacement(&rotation_credentials, &status, 0, true).await?;
        Ok(OpenedAdmissionRotation {
            generation: 7,
            credentials: rotation_credentials,
            transport,
            plugin_session_nonce,
            preserve_continuity: true,
        })
    });
    let mut rotation = AdmissionRotationTask::from_opening_for_test(host_rpc, 7, delayed_open);
    tokio::task::yield_now().await;
    assert!(rotation.take_finished().await.is_none());
    assert!(rotation.is_pending());

    let mut harness = RelayHarness::new();
    let active = activate_takeover_without_replacement_peer(
        &mut harness,
        "request_real_admission_rotation_nonblocking",
    );
    harness
        .session
        .leases
        .get_mut(&active.lease.jti)
        .unwrap()
        .expires_at = now + 1;
    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "heartbeat_during_real_admission_open",
        "idempotencyKey": "idem_heartbeat_during_real_admission_open",
        "leaseToken": active.lease_token
    })
    .to_string();
    let renewed_frames = harness.post(&heartbeat);
    let renewed = harness.granted(&renewed_frames);
    harness.settle(&renewed_frames, TransportDelivery::Delivered);
    assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
    assert_eq!(
        harness.session.expire_relay_leases(now + 2, &harness.media),
        0
    );
    assert!(rotation.is_pending());

    let opened = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(result) = rotation.take_finished().await {
                break match result {
                    Ok(opened) => opened,
                    Err(error) => panic!("replacement failed: {}", error.message),
                };
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the delayed replacement opens");
    assert_eq!(opened.generation, 7);
    assert!(opened.transport.is_relay());
    assert_eq!(
        tail_requests.load(Ordering::SeqCst),
        0,
        "a same-relay replacement inherits the predecessor cursor instead of walking the tail"
    );
    assert_eq!(harness.session.relay_leases.len(), 1);
    assert!(harness.session.leases.contains_key(&renewed.lease.jti));
    opened.transport.close().await;
    server.abort();
}

#[tokio::test]
async fn websocket_rotation_defers_the_fencing_hello_until_atomic_handoff() {
    enum OldSocketCommand {
        Frame(String),
        Close,
    }

    let authority = test_authority();
    let now = unix_now().unwrap();
    let challenge = |suffix: &str| EndpointChallengeFrame {
        kind: "endpoint_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        subject_id: "aokie".into(),
        role: AdmissionRole::Plugin,
        connection_id: format!("ws_rotation_{suffix}"),
        challenge_nonce: format!("challenge_ws_rotation_{suffix}"),
        admission_jti: format!("admission_ws_rotation_{suffix}"),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        expires_at: now + 30,
    };
    let old_challenge = challenge("old");
    let replacement_challenge = challenge("replacement");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let (old_commands, mut old_command_rx) =
        tokio::sync::mpsc::unbounded_channel::<OldSocketCommand>();
    let fence_old = old_commands.clone();
    let (old_ready_tx, old_ready_rx) = tokio::sync::oneshot::channel();
    let (new_hello_tx, mut new_hello_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (old_stream, _) = listener.accept().await.unwrap();
        let mut old_socket = tokio_tungstenite::accept_async(old_stream).await.unwrap();
        old_socket
            .send(Message::Text(
                serde_json::to_string(&old_challenge).unwrap().into(),
            ))
            .await
            .unwrap();
        let old_hello = old_socket.next().await.unwrap().unwrap();
        assert!(matches!(old_hello, Message::Text(_)));
        let _ = old_ready_tx.send(());
        let old_writer = tokio::spawn(async move {
            while let Some(command) = old_command_rx.recv().await {
                match command {
                    OldSocketCommand::Frame(encoded) => {
                        old_socket
                            .send(Message::Text(encoded.into()))
                            .await
                            .unwrap();
                    }
                    OldSocketCommand::Close => {
                        let _ = old_socket.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        });

        let (replacement_stream, _) = listener.accept().await.unwrap();
        let mut replacement_socket = tokio_tungstenite::accept_async(replacement_stream)
            .await
            .unwrap();
        replacement_socket
            .send(Message::Text(
                serde_json::to_string(&replacement_challenge)
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
        let replacement_hello = replacement_socket.next().await.unwrap().unwrap();
        let Message::Text(encoded) = replacement_hello else {
            panic!("replacement endpoint proof is a text frame");
        };
        let hello: PluginHello = serde_json::from_str(encoded.as_str()).unwrap();
        assert_eq!(hello.kind, "plugin_hello");
        // This is the v2 gateway's same-plugin behaviour: accepting the
        // replacement hello immediately fences the predecessor.
        fence_old.send(OldSocketCommand::Close).unwrap();
        let _ = new_hello_tx.send(());
        while let Some(message) = replacement_socket.next().await {
            if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
        let _ = old_writer.await;
    });

    let mut credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority)
        .unwrap();
    credentials.endpoint =
        Url::parse(&format!("ws://{address}/v2/realtime")).expect("local ws URL");
    credentials.relay = None;
    credentials.relay_only = false;
    let status = Arc::new(Mutex::new(GatewayStatusSnapshot::starting()));
    let (mut current, current_nonce) = GatewayTransport::open(&credentials, &status, 0)
        .await
        .unwrap();
    old_ready_rx.await.unwrap();

    let (mut replacement, replacement_nonce) =
        GatewayTransport::open_replacement(&credentials, &status, 0, false)
            .await
            .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(75), &mut new_hello_rx)
            .await
            .is_err(),
        "background open must not send the hello that fences the live predecessor"
    );

    let mut harness = RelayHarness::new();
    harness.session.plugin_session_nonce = current_nonce;
    let active =
        activate_takeover_without_replacement_peer(&mut harness, "request_ws_rotation_handoff");
    let heartbeat = json!({
        "kind": "lease_heartbeat",
        "schemaVersion": SCHEMA_VERSION,
        "appId": "app_a",
        "requestId": "heartbeat_before_ws_handoff",
        "idempotencyKey": "idem_heartbeat_before_ws_handoff",
        "leaseToken": active.lease_token
    })
    .to_string();
    old_commands
        .send(OldSocketCommand::Frame(heartbeat))
        .unwrap();
    let inbound = current
        .recv_text(Duration::from_secs(1))
        .await
        .unwrap()
        .expect("the predecessor still carries its heartbeat");
    let renewed_frames = harness.post(&inbound);
    let renewed = harness.granted(&renewed_frames);
    harness.settle(&renewed_frames, TransportDelivery::Delivered);
    assert_eq!(renewed.status, PluginLeaseStatus::Renewed);

    replacement.activate_replacement().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), &mut new_hello_rx)
        .await
        .expect("the replacement hello reaches the gateway")
        .unwrap();
    harness
        .session
        .rotate_credentials(&credentials, replacement_nonce)
        .unwrap();
    let mut fenced_predecessor = std::mem::replace(&mut current, replacement);
    assert!(current.adopt_routing_from(&mut fenced_predecessor));
    assert!(
        fenced_predecessor
            .recv_text(Duration::from_secs(1))
            .await
            .is_err(),
        "the gateway really fenced the old socket after the committed hello"
    );
    assert_eq!(harness.session.relay_leases.len(), 1);
    assert!(harness.session.leases.contains_key(&renewed.lease.jti));

    current.close().await;
    drop(old_commands);
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .expect("test gateway shuts down")
        .unwrap();
}

#[cfg(feature = "voice")]
#[tokio::test]
async fn delayed_or_failed_regreeting_never_stalls_active_takeover_heartbeats() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let authority = test_authority();
    let credentials = admission("app_a", "aokie", &authority)
        .into_credentials(None, "aokie", authority.clone())
        .unwrap();
    let now = unix_now().unwrap();
    let challenge = EndpointChallengeFrame {
        kind: "endpoint_challenge".into(),
        schema_version: SCHEMA_VERSION,
        app_id: "app_a".into(),
        subject_id: "aokie".into(),
        role: AdmissionRole::Plugin,
        connection_id: "relay_delayed".into(),
        challenge_nonce: "challenge_delayed".into(),
        admission_jti: "admission_delayed".into(),
        holder_key_thumbprint: authority.endpoint_key.thumbprint.clone(),
        expected_peer_key_thumbprint: None,
        approved_peer_key_thumbprints: authority.approved_thumbprints(),
        peer_roster_revision: Some(authority.roster_revision),
        peer_roster_hash: Some(authority.roster_hash.clone()),
        expires_at: now + 30,
    };
    let challenge_requests = Arc::new(AtomicUsize::new(0));
    let seen_challenges = challenge_requests.clone();
    let challenge_response = challenge.clone();
    let router = axum::Router::new()
        .route(
            "/challenge",
            axum::routing::get(move || {
                let request = seen_challenges.fetch_add(1, Ordering::SeqCst);
                let challenge = challenge_response.clone();
                async move {
                    use axum::response::IntoResponse;
                    if request == 0 {
                        // Initial channel open is unrelated to per-party
                        // re-greeting and completes immediately.
                        return axum::Json(challenge).into_response();
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    if request == 1 {
                        axum::Json(challenge).into_response()
                    } else {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    }
                }
            }),
        )
        .route(
            "/frames",
            axum::routing::get(|| async { axum::Json(json!({"frames": [], "lastSeq": 0})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let endpoints = RelayEndpoints {
        challenge_url: format!("http://{address}/challenge"),
        frames_url: format!("http://{address}/frames"),
        stream_url: format!("http://{address}/stream"),
    };
    let (channel, _) = crate::companion_relay::RelayChannel::connect(
        &endpoints,
        &credentials.token,
        &credentials.app_id,
        &credentials.plugin_id,
        authority.approved_thumbprints(),
    )
    .await
    .unwrap();
    let transport = GatewayTransport::Relay(channel);

    let mut harness = RelayHarness::new();
    let active = activate_takeover_without_replacement_peer(
        &mut harness,
        "request_regreeting_nonblocking",
    );
    assert_eq!(active.lease.phase, LeasePhase::Active);
    let logical_session = harness.session.plugin_session_nonce.clone();
    let party = harness.party.clone();
    let heartbeat = |request_id: &str, token: &str| {
        json!({
            "kind": "lease_heartbeat",
            "schemaVersion": SCHEMA_VERSION,
            "appId": "app_a",
            "requestId": request_id,
            "idempotencyKey": format!("idem_{request_id}"),
            "leaseToken": token
        })
        .to_string()
    };

    let mut tasks = RelayGreetingTasks::default();
    tasks.schedule(
        party.clone(),
        transport.regreeting_request().unwrap(),
        credentials.clone(),
        logical_session.clone(),
    );
    // A repeat hello coalesces rather than spawning an unbounded second
    // challenge request for the same roster party.
    tasks.schedule(
        party.clone(),
        transport.regreeting_request().unwrap(),
        credentials.clone(),
        logical_session.clone(),
    );
    tokio::task::yield_now().await;
    assert!(tasks.is_pending(&party));
    assert!(tasks.take_finished().await.is_empty());

    // The delayed HTTP request is still in flight, but the exact active
    // takeover heartbeat rotates normally on the authority path.
    let renewed_frames =
        harness.post(&heartbeat("heartbeat_during_delay", &active.lease_token));
    let renewed = harness.granted(&renewed_frames);
    assert_eq!(renewed.status, PluginLeaseStatus::Renewed);
    assert_eq!(renewed.lease.phase, LeasePhase::Active);
    harness.settle(&renewed_frames, TransportDelivery::Delivered);
    assert_eq!(harness.session.plugin_session_nonce, logical_session);
    assert_eq!(harness.session.relay_leases.len(), 1);

    let fresh = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(result) = tasks.take_finished().await.into_iter().next() {
                break result.expect("the delayed refresh succeeds");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("delayed refresh completes");
    assert_eq!(fresh.plugin_session_nonce, logical_session);

    // The next attempt is delayed and then fails. It is equally isolated:
    // a second heartbeat advances while it is pending, and the error owns
    // no session/media state it could revoke.
    tasks.schedule(
        party.clone(),
        transport.regreeting_request().unwrap(),
        credentials,
        logical_session.clone(),
    );
    tokio::task::yield_now().await;
    assert!(tasks.is_pending(&party));
    let renewed_again_frames = harness.post(&heartbeat(
        "heartbeat_during_failed_refresh",
        &renewed.lease_token,
    ));
    let renewed_again = harness.granted(&renewed_again_frames);
    assert_eq!(renewed_again.status, PluginLeaseStatus::Renewed);
    harness.settle(&renewed_again_frames, TransportDelivery::Delivered);

    let failure = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(result) = tasks.take_finished().await.into_iter().next() {
                break result.expect_err("the second challenge is refused");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("failed refresh completes");
    assert_eq!(failure.kind, WorkerErrorKind::Reconnect);
    assert_eq!(harness.session.plugin_session_nonce, logical_session);
    assert_eq!(harness.session.relay_leases.len(), 1);
    assert!(harness
        .session
        .leases
        .contains_key(&renewed_again.lease.jti));
    assert_eq!(
        harness
            .session
            .expire_relay_leases(unix_now().unwrap(), &harness.media),
        0,
        "challenge failure cannot expire or revoke active authority"
    );
    server.abort();
}

#[test]
fn gateway_error_envelope_accepts_typed_fields_without_app_identity() {
    let encoded = json!({
        "kind": "error",
        "schemaVersion": SCHEMA_VERSION,
        "code": "stale_snapshot",
        "message": "plugin snapshot regressed",
        "requestId": null
    })
    .to_string();

    let envelope: Envelope = serde_json::from_str(&encoded).unwrap();
    assert_eq!(envelope.kind, "error");
    assert_eq!(envelope.schema_version, SCHEMA_VERSION);
    assert!(envelope.app_id.is_none());

    let notice: ErrorNotice = parse_gateway_frame(&encoded).unwrap();
    assert_eq!(notice.code, "stale_snapshot");
}
