//! `GatewaySession` inbound message handling: leases, relay frames, RTC signals.

#[allow(unused_imports)]
use super::*;

impl GatewaySession {
    pub(super) fn handle_inbound(
        &mut self,
        encoded: &str,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
        from_relay_peer: bool,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: Option<&HashSet<Grant>>,
    ) -> Result<Vec<String>, WorkerError> {
        // A relay peer is not the gateway.
        //
        // On the socket every frame was minted by trusted gateway
        // infrastructure, so a protocol violation means this session is
        // genuinely broken and tearing it down is the correct fail-closed
        // response. On the relay the frame was posted by an
        // approved-but-untrusted Companion; honouring its content as a
        // lifecycle signal hands any roster member a one-frame kill switch.
        //
        // The relay therefore enters a narrow authenticated allowlist whose
        // handlers re-check party/device identity, current admission grants,
        // replay keys and immutable call/lease fences. Unknown kinds are
        // dropped before the trusted-gateway handlers below can mutate state.
        if from_relay_peer {
            return self.handle_relay_peer_frame(
                encoded,
                from_party,
                authenticated_subject,
                authenticated_grants.unwrap_or(&HashSet::new()),
                media,
                radio,
            );
        }
        let envelope: Envelope = serde_json::from_str(encoded)
            .map_err(|_| WorkerError::reconnect("Companion gateway frame is malformed"))?;
        if envelope.schema_version != SCHEMA_VERSION {
            return Err(WorkerError::rebootstrap(
                "Companion gateway schemaVersion is unsupported",
            ));
        }
        if let Some(app_id) = envelope.app_id.as_deref() {
            if app_id != self.app_id {
                return Err(WorkerError::rebootstrap(
                    "Companion gateway frame crossed application identity",
                ));
            }
        }
        match envelope.kind.as_str() {
            "claim_proposal" => {
                let notice: LeaseNotice = parse_gateway_frame(encoded)?;
                self.handle_claim_proposal(notice, media, radio, None)
            }
            "lease_granted" => {
                let notice: LeaseNotice = parse_gateway_frame(encoded)?;
                self.handle_lease_granted(notice, radio)?;
                Ok(Vec::new())
            }
            "lease_renewed" => {
                let notice: LeaseNotice = parse_gateway_frame(encoded)?;
                self.handle_lease_renewed(notice, media, radio)?;
                Ok(Vec::new())
            }
            "lease_revoked" => {
                let notice: LeaseRevokedNotice = parse_gateway_frame(encoded)?;
                self.handle_lease_revoked(notice, media)?;
                Ok(Vec::new())
            }
            "rtc_signal" => {
                let frame: PluginRtcSignalFrame = parse_gateway_frame(encoded)?;
                frame.validate_routed_mobile().map_err(|_| {
                    WorkerError::reconnect("Companion RTC signal failed contract validation")
                })?;
                self.handle_rtc_signal(frame, media, radio)?;
                Ok(Vec::new())
            }
            "assistance_answer" => {
                let frame: PluginAssistanceAnswerFrame = parse_gateway_frame(encoded)?;
                frame.validate().map_err(|_| {
                    WorkerError::reconnect("Companion assistance answer is invalid")
                })?;
                if frame.app_id != self.app_id {
                    return Err(WorkerError::rebootstrap(
                        "Companion assistance answer crossed application identity",
                    ));
                }
                let remote = radio
                    .remote_media()
                    .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
                    .snapshot();
                if !remote.consent.enabled
                    || !remote.consent.acknowledged
                    || !remote.consent.assistance_enabled
                    || remote.call_id.as_deref() != Some(frame.call_id.as_str())
                    || remote.call_epoch != frame.call_epoch
                    || remote.owner_epoch != frame.owner_epoch
                    || remote.remote_revision != frame.remote_revision
                    || radio.switchboard_revision() != frame.switchboard_revision
                {
                    return Err(WorkerError::reconnect(
                        "Companion assistance answer failed consent or call fencing",
                    ));
                }
                self.assistance.accept(frame).map_err(|_| {
                    WorkerError::reconnect("Companion assistance answer was refused")
                })?;
                Ok(Vec::new())
            }
            "plugin_microphone_mute" => {
                let frame: PluginMicrophoneMuteFrame = parse_gateway_frame(encoded)?;
                frame.validate().map_err(|_| {
                    WorkerError::reconnect("Companion microphone command is invalid")
                })?;
                if frame.app_id != self.app_id {
                    return Err(WorkerError::rebootstrap(
                        "Companion microphone command crossed application identity",
                    ));
                }
                let Some(claims) = self.leases.get(&frame.lease_jti).cloned() else {
                    return Ok(Vec::new());
                };
                let remote = media.snapshot();
                let service_matches = matches!(
                    (claims.mode, remote.service_mode),
                    (LeaseMode::Takeover, LocalServiceMode::HumanActive)
                        | (LeaseMode::Consult, LocalServiceMode::ConsultActive)
                );
                let exact = claims.lease_id == frame.lease_id
                    && claims.jti == frame.lease_jti
                    && claims.device_id == frame.device_id
                    && claims.rtc_session_id == frame.rtc_session_id
                    && claims.call_id == frame.call_id
                    && claims.call_epoch == frame.call_epoch
                    && claims.owner_epoch == frame.owner_epoch
                    && claims.fence == frame.fence
                    && claims.phase == LeasePhase::Active
                    && radio.current_call_id().as_deref() == Some(frame.call_id.as_str())
                    && radio.is_call_active()
                    && !radio.switch_in_flight()
                    && radio.switchboard_revision() == frame.switchboard_revision
                    && remote.call_id.as_deref() == Some(frame.call_id.as_str())
                    && remote.call_epoch == frame.call_epoch
                    && remote.owner_epoch == frame.owner_epoch
                    && remote.remote_revision == frame.remote_revision
                    && remote.talk_device_id.as_deref() == Some(frame.device_id.as_str())
                    && remote.talk_lease_id.as_deref() == Some(frame.lease_id.as_str())
                    && remote.talk_fence == frame.fence
                    && service_matches;
                if !exact {
                    eprintln!(
                        "[aokie-plugin][companion] stage=microphone_mute_refused device={} detail=The exact call, lease, media, or switchboard fence changed",
                        sanitize_gateway_code(&frame.device_id)
                    );
                    return Ok(Vec::new());
                }
                let binding = binding_for_claims(&claims);
                let Ok(remote_revision) =
                    media.set_microphone_muted(&binding, frame.remote_revision, frame.muted)
                else {
                    return Ok(Vec::new());
                };
                let status = PluginMicrophoneMuteStatusFrame {
                    kind: "microphone_mute_status".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: self.app_id.clone(),
                    device_id: frame.device_id,
                    request_id: frame.request_id,
                    lease_id: claims.lease_id,
                    lease_jti: claims.jti,
                    rtc_session_id: claims.rtc_session_id,
                    call_id: claims.call_id,
                    call_epoch: claims.call_epoch,
                    owner_epoch: claims.owner_epoch,
                    switchboard_revision: radio.switchboard_revision(),
                    remote_revision,
                    fence: claims.fence,
                    muted: frame.muted,
                };
                status.validate().map_err(|_| {
                    WorkerError::reconnect("Companion microphone status is invalid")
                })?;
                self.last_snapshot_fingerprint = None;
                self.last_snapshot_sent = None;
                self.next_snapshot_poll = Instant::now();
                serde_json::to_string(&status)
                    .map(|encoded| vec![encoded])
                    .map_err(|_| {
                        WorkerError::reconnect("Companion microphone status could not be encoded")
                    })
            }
            "end_caller_execute" => {
                let frame: PluginEndCallerExecuteFrame = parse_gateway_frame(encoded)?;
                frame.validate().map_err(|_| {
                    WorkerError::reconnect("Companion caller-ending command is invalid")
                })?;
                self.handle_end_caller_execute(frame, radio)
            }
            "error" => {
                let notice: ErrorNotice = parse_gateway_frame(encoded)?;
                let _ = (
                    &notice.kind,
                    notice.schema_version,
                    &notice.message,
                    &notice.request_id,
                );
                Err(WorkerError::reconnect(format!(
                    "Companion gateway reported {}",
                    sanitize_gateway_code(&notice.code)
                )))
            }
            _ => Err(WorkerError::reconnect(
                "Companion gateway sent an unsupported frame",
            )),
        }
    }

    /// Everything a Companion posts directly to this plugin over the relay.
    ///
    /// Returns `Ok` on every path — including malformed JSON, unknown kinds,
    /// forged proofs and replays — so peer traffic can never terminate the
    /// session. `WorkerError` is used inside only as a typed reason for the
    /// log; it never escapes.
    ///
    /// The actionable set beyond the hello exists because this plugin is the
    /// lease authority on this carrier: it mints what it later honours, so an
    /// unrecognised claim is not a protocol violation to fail on, it is simply
    /// something we never issued. Refusals go back in band as
    /// `plugin_claim_rejected` and cost the session nothing.
    pub(super) fn handle_relay_peer_frame(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let kind = serde_json::from_str::<Envelope>(encoded)
            .map(|frame| frame.kind)
            .unwrap_or_else(|_| "malformed".into());
        if self.relay_authority_enabled {
            match kind.as_str() {
                "mobile_offer_answer" => {
                    return Ok(self.relay_offer_answer(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                    ));
                }
                "lease_request" => {
                    return Ok(self.relay_lease_request(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "rtc_signal" => {
                    return Ok(self.relay_rtc_signal(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "lease_heartbeat" => {
                    return Ok(self.relay_lease_heartbeat(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "lease_revoke" => {
                    return Ok(self.relay_lease_revoke(
                        encoded,
                        from_party,
                        authenticated_subject,
                        media,
                    ));
                }
                "microphone_mute" => {
                    return Ok(self.relay_microphone_mute(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "assistance_answer" => {
                    return Ok(self.relay_assistance_answer(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "end_caller_challenge_request" => {
                    return Ok(self.relay_end_caller_challenge_request(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                "end_caller_confirm" => {
                    return Ok(self.relay_end_caller_confirm(
                        encoded,
                        from_party,
                        authenticated_subject,
                        authenticated_grants,
                        media,
                        radio,
                    ));
                }
                _ => {}
            }
        }
        if kind == "mobile_hello" {
            if let Err(error) = self.accept_mobile_hello(
                encoded,
                from_party,
                authenticated_subject,
                authenticated_grants,
                media,
            ) {
                // Rate-limited, never capped. A lifetime cap would go silent
                // after a handful of lines, and a SYSTEMATICALLY refused
                // Companion (clock skew past the signature window, a roster
                // that has not propagated, an assignment naming another
                // Desktop) retries on a timer — so the cap would be spent in
                // the first minute and every later refusal, including a
                // genuinely new one, would vanish. The plugin log would then
                // show nothing but `relay_no_destination`, which is exactly
                // what a Companion that never spoke at all looks like: the
                // undiagnosable deadlock this whole path exists to escape.
                let due = self
                    .relay_hello_rejection_logged_at
                    .is_none_or(|at| at.elapsed() >= RELAY_HELLO_REJECTION_LOG_INTERVAL);
                if due {
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_hello_rejected suppressed={} detail={}",
                        self.relay_hello_rejections_suppressed,
                        sanitize_status_message(&error.message)
                    );
                    self.relay_hello_rejection_logged_at = Some(Instant::now());
                    self.relay_hello_rejections_suppressed = 0;
                } else {
                    self.relay_hello_rejections_suppressed =
                        self.relay_hello_rejections_suppressed.saturating_add(1);
                }
            }
            return Ok(Vec::new());
        }
        // Nothing is lost by dropping the rest.
        //
        // The gateway-dialect lifecycle notices (`claim_proposal`,
        // `lease_granted`, `lease_renewed`, `lease_revoked`, `claim_decision`)
        // stay dropped ON PURPOSE and must never be admitted here: they assert
        // an authority this carrier has no one to exercise, so honouring one
        // would let an approved-but-untrusted Companion declare its own
        // takeover with a fence of its choosing. On this carrier the plugin
        // mints those itself, above, from live radio truth.
        //
        // Assistance answers and the caller-ending pair are translated above
        // only after the relay sender, authenticated grants, exact live call,
        // locally minted lease and one-use operation ledgers all agree. Their
        // plugin-dialect twins remain impossible for a peer to self-assert.
        // Keyed on the SANITIZED code, never the raw kind. A relay frame may be
        // just under the carrier's 1 MiB SSE ceiling and `kind` is peer-supplied
        // string content, so retaining raw kinds would hold up to
        // `MAX_REPORTED_RELAY_KINDS` megabyte-scale strings for the life of the
        // session inside the process that also runs the radio. Keying on what is
        // actually printed bounds retention to the 80-char sanitized form and
        // closes the matching throttle bypass, where distinct raw kinds sharing a
        // sanitized prefix each earned an identical log line.
        let code = sanitize_gateway_code(&kind);
        if self.dropped_relay_kinds.len() < MAX_REPORTED_RELAY_KINDS
            && self.dropped_relay_kinds.insert(code.clone())
        {
            eprintln!(
                "[aokie-plugin][companion] stage=relay_frame_dropped kind={code} detail=The frame is outside the authenticated relay action allowlist"
            );
        }
        Ok(Vec::new())
    }

    /// Admit an owner-approved Companion onto this relay session.
    ///
    /// The carrier already registered the sender as a publish destination when
    /// the frame arrived ([`crate::companion_relay::RelayChannel::learn_route`]),
    /// so this proves the hello and then RE-ARMS authoritative publication.
    /// The re-arm is the load-bearing half: publication is edge-triggered, so a
    /// Companion joining a quiet line would otherwise register successfully and
    /// then receive nothing until the next call.
    pub(super) fn accept_mobile_hello(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        let hello: MobileHello = parse_gateway_frame(encoded)?;
        // Pins kind, schemaVersion, and that role/appId/subjectId/sessionNonce
        // agree with the proof's own claims.
        hello
            .validate()
            .map_err(|_| WorkerError::reconnect("Companion hello failed contract validation"))?;
        if hello.app_id != self.app_id {
            return Err(WorkerError::rebootstrap(
                "Companion hello crossed application identity",
            ));
        }
        let now = unix_now()?;
        // Signature over the domain-separated canonical claims, plus the
        // bounded signature window.
        hello
            .endpoint_proof
            .verify(now)
            .map_err(|_| WorkerError::reconnect("Companion hello endpoint signature is invalid"))?;
        let claims = &hello.endpoint_proof.claims;
        let approved = self
            .endpoint_authority
            .approved_mobile_keys
            .get(&claims.holder_key_thumbprint)
            .ok_or_else(|| {
                WorkerError::reconnect(
                    "Companion hello signer is absent from the owner-approved roster",
                )
            })?;
        if *approved != hello.endpoint_proof.endpoint_key {
            return Err(WorkerError::reconnect(
                "Companion hello key does not match the owner-approved roster entry",
            ));
        }
        // The mobile role always carries this (the protocol's peer policy makes
        // it mandatory), and the identity service sets it to the assigned
        // plugin's endpoint thumbprint — so it proves the hello was addressed to
        // THIS plugin rather than replayed from a session with another Desktop.
        if claims.expected_peer_key_thumbprint.as_deref()
            != Some(self.endpoint_authority.endpoint_key.thumbprint.as_str())
        {
            return Err(WorkerError::reconnect(
                "Companion hello addresses a different plugin endpoint",
            ));
        }
        let verified_party = relay_party(&claims.holder_key_thumbprint);
        if from_party != Some(verified_party.as_str()) {
            return Err(WorkerError::reconnect(
                "Companion hello arrived from a party other than its proved endpoint",
            ));
        }
        if authenticated_subject != Some(hello.device_id.as_str()) {
            return Err(WorkerError::reconnect(
                "Companion hello subject does not match its authenticated admission",
            ));
        }
        if !authenticated_grants.contains(&Grant::StateRead) {
            // The proof and outer sender identity are already established, so
            // this is an authoritative loss of access rather than an anonymous
            // frame. Revoke anything an older admission left behind before
            // refusing to re-admit the peer.
            self.revoke_relay_device_authority(&hello.device_id, &HashSet::new(), false, media);
            if let Some(peer) = self.relay_peers.get_mut(&hello.device_id) {
                peer.grants.clear();
            }
            return Err(WorkerError::reconnect(
                "Companion hello admission does not grant authoritative state access",
            ));
        }
        self.used_endpoint_jtis
            .retain(|_, expires_at| *expires_at > now);
        if self.used_endpoint_jtis.contains_key(&claims.jti) {
            return Err(WorkerError::reconnect("Companion hello was replayed"));
        }
        if self.used_endpoint_jtis.len() >= MAX_USED_ENDPOINT_JTIS {
            return Err(WorkerError::reconnect(
                "Companion hello replay cache is exhausted",
            ));
        }
        self.used_endpoint_jtis
            .insert(claims.jti.clone(), claims.expires_at);

        // Remember the party, now that its signature, roster membership and
        // addressing have all been proved. This is the ONLY place a relay peer
        // is learned, so every later mint is bound to an identity that got
        // through all of the checks above.
        //
        // A device that re-introduces itself REPLACES its entry. Before that
        // replacement, retire authority whose proof or admission no longer
        // supports it. A fresh session nonce fences every old lease; an
        // unchanged session only loses modes actually removed from its grants.
        if !self.relay_peers.contains_key(&hello.device_id)
            && self.relay_peers.len() >= MAX_RELAY_PEERS
        {
            return Err(WorkerError::reconnect(
                "Companion hello exceeds the relay peer limit for this session",
            ));
        }
        if let Some(previous) = self.relay_peers.get(&hello.device_id).cloned() {
            let session_changed = previous.session_nonce != claims.session_nonce
                || previous.holder_key_thumbprint != claims.holder_key_thumbprint;
            let grants_narrowed = previous
                .grants
                .iter()
                .any(|grant| !authenticated_grants.contains(grant));
            if session_changed || grants_narrowed {
                self.revoke_relay_device_authority(
                    &hello.device_id,
                    authenticated_grants,
                    session_changed,
                    media,
                );
            }
        }
        self.relay_peers.insert(
            hello.device_id.clone(),
            RelayPeer {
                holder_key_thumbprint: claims.holder_key_thumbprint.clone(),
                session_nonce: claims.session_nonce.clone(),
                grants: authenticated_grants.clone(),
            },
        );
        // Hand the carrier the route only after every proof above succeeded.
        // This intentionally REBINDS an existing entry: any older route was
        // learned by an earlier verified session, while an arbitrary frame is
        // never allowed to install one in the first place.
        self.relay_verified_route = Some((hello.device_id.clone(), verified_party.clone()));

        // Re-arm authoritative publication for the party that just joined:
        // clear the idle latch, clear the snapshot fingerprint and its refresh
        // clock, and mark the poll due so the next loop turn publishes.
        //
        // Assistance delivery is per current verified audience. A newly
        // admitted eligible device is owed the current projected snapshot and
        // then the still-pending request; both deliveries are idempotent.
        self.rearm_authoritative_publication();

        // Re-arming publication is only half of going live. The Companion
        // DROPS authoritative state from a peer that has not proved its
        // endpoint key, and it learns that proof from our `plugin_hello`, which
        // the carrier prepends exactly ONCE per party per plugin session. A
        // Companion that already greeted, then restarted as a fresh process,
        // has lost its in-memory proof while our greeting book still records it
        // as greeted — so it would receive every re-armed frame and drop every
        // one of them, for the rest of this plugin session.
        //
        // A verified `mobile_hello` IS the signal that a Companion has started
        // a session with us, so it asks the carrier to fetch a fresh challenge,
        // re-sign this logical plugin session and retire that party's greeting
        // mark. The party is derived from the thumbprint the signature just
        // proved, never from the carrier's routing header or the frame's
        // self-asserted `deviceId`.
        self.relay_regreet_party = Some(verified_party);
        eprintln!(
            "[aokie-plugin][companion] stage=relay_peer_hello device={} detail=An approved Companion joined this relay session and authoritative state was re-armed",
            sanitize_gateway_code(&hello.device_id)
        );
        Ok(())
    }

    /// A device redeeming one of the invitations this plugin published.
    ///
    /// Answering does not consume the offer: the lease request that follows
    /// still has to name it, and that is where the single use is spent. Keeping
    /// the binding alive between the two steps is what lets the request resolve
    /// which device is asking at all — `lease_request` carries no device id.
    pub(super) fn relay_offer_answer(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileOfferAnswerFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let device_id = frame.target_device_id.as_str();
        let request_id = frame.request_id.as_str();
        // Identity BEFORE budget, in every arm. The budget is per-device state,
        // so spending it on an unauthenticated claim would let one approved
        // Companion exhaust another's allowance simply by naming it — and the
        // victim's own claims would then be refused as flooding.
        if !self.relay_sender_owns_device(from_party, authenticated_subject, device_id) {
            return self.relay_reject(
                device_id,
                request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        // Capture the complete signed authority requirement before narrowing
        // retires an offer. The peer-controlled `offeredMode` must never
        // choose which grants are checked, and AssistanceRespond is just as
        // material as Takeover for a transfer-bound invitation.
        let authoritative_offer = self.relay_offers.get(&frame.offer_id).cloned();
        let authoritative_mode = authoritative_offer
            .as_ref()
            .map(|offer| offer.claims.offered_mode)
            .unwrap_or(frame.offered_mode);
        self.relay_reconcile_frame_grants(device_id, authenticated_grants, media);
        if authoritative_offer.as_ref().is_some_and(|offer| {
            !self.relay_required_grants_are_authorized(
                device_id,
                authenticated_grants,
                &offer.claims.required_grants,
            )
        }) || !self.relay_mode_is_authorized(device_id, authenticated_grants, authoritative_mode)
        {
            return self.relay_reject(
                device_id,
                request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        let replay_key = format!("offer\u{1f}{device_id}\u{1f}{}", frame.idempotency_key);
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    device_id,
                    request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::OfferAccepted {
                    encoded: response,
                    mode,
                    required_grants,
                    ..
                } if self.relay_mode_is_authorized(device_id, authenticated_grants, mode)
                    && self.relay_required_grants_are_authorized(
                        device_id,
                        authenticated_grants,
                        &required_grants,
                    ) =>
                {
                    vec![response]
                }
                RelayReplayResult::OfferAccepted { .. } => self.relay_reject(
                    device_id,
                    request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                ),
                RelayReplayResult::Rejected { encoded } => vec![encoded],
                _ => self.relay_reject(
                    device_id,
                    request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        let over_budget = self.relay_over_budget(device_id);
        let replay_has_room = self.relay_replay_has_room(&replay_key);
        let Ok(now) = unix_now() else {
            return self.relay_reject(device_id, request_id, "offer_unknown", "clock unavailable");
        };
        let Some(minted) = self.relay_offers.get(&frame.offer_id).cloned() else {
            return self.relay_reject(
                device_id,
                request_id,
                "offer_unknown",
                "this plugin did not issue that offer",
            );
        };
        let minted_device_id = minted.claims.target_device_id.clone();
        let minted_mode = minted.claims.offered_mode;
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &minted_device_id) {
            return self.relay_reject(
                &minted_device_id,
                request_id,
                "device_unknown",
                "this offer belongs to another authenticated device",
            );
        }
        if !self.relay_mode_is_authorized(&minted_device_id, authenticated_grants, minted_mode) {
            return self.relay_reject(
                &minted_device_id,
                request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        if !self.relay_required_grants_are_authorized(
            &minted_device_id,
            authenticated_grants,
            &minted.claims.required_grants,
        ) {
            return self.relay_reject(
                &minted_device_id,
                request_id,
                "grant_required",
                "the authenticated admission no longer grants every operation required by this offer",
            );
        }
        if minted.claims.expires_at <= now {
            self.relay_offers.remove(&frame.offer_id);
            return self.relay_reject(
                device_id,
                request_id,
                "offer_expired",
                "that offer has expired",
            );
        }
        if minted.accepted {
            if over_budget || !replay_has_room {
                return self.relay_reject(
                    device_id,
                    request_id,
                    "rate_limited",
                    "too many Companion requests from this device",
                );
            }
            return self.relay_reject(
                device_id,
                request_id,
                "offer_replayed",
                "that offer was already answered",
            );
        }
        if self
            .relay_offer_winners
            .get(&minted.claims.opportunity_id)
            .is_some_and(|winner| winner != &minted.claims.offer_id)
        {
            return self.relay_reject(
                device_id,
                request_id,
                "offer_already_answered",
                "another endpoint or surface already answered this transfer opportunity",
            );
        }
        // Every field is re-checked against what WE minted rather than trusted
        // from the frame, and the token comparison is constant time: the token
        // is the secret, and a byte-at-a-time timing leak would hand a peer the
        // ability to forge one.
        let matches = minted.claims.jti == frame.offer_jti
            && minted.claims.target_device_id == frame.target_device_id
            && minted.claims.target_holder_key_thumbprint == frame.target_holder_key_thumbprint
            && minted.claims.offered_mode == frame.offered_mode
            && minted.claims.call_id == frame.call_id
            && minted.claims.call_epoch == frame.call_epoch
            && minted.claims.owner_epoch == frame.owner_epoch
            && tokens_match(&minted.token, &frame.offer_token);
        if !matches {
            return self.relay_reject(
                device_id,
                request_id,
                "offer_unknown",
                "that answer does not match the offer this plugin issued",
            );
        }
        if over_budget || !replay_has_room {
            // The mobile spends/tombstones an offer before sending its answer.
            // Once the complete signed answer proved this exact invitation,
            // a terminal local-capacity rejection must retire it too so the
            // next snapshot carries a fresh identity the client can use.
            self.retire_failed_relay_offer(minted.clone());
            return self.relay_reject(
                device_id,
                request_id,
                "rate_limited",
                if over_budget {
                    "too many requests"
                } else {
                    "the replay ledger is full"
                },
            );
        }
        let accepted = PluginOfferAcceptedFrame {
            kind: "plugin_offer_accepted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: frame.target_device_id.clone(),
            request_id: frame.request_id.clone(),
            offer_id: frame.offer_id.clone(),
            offer_jti: frame.offer_jti.clone(),
            offered_mode: frame.offered_mode,
            accepted: true,
        };
        if accepted.validate().is_err() {
            return Vec::new();
        }
        let Ok(response) = serde_json::to_string(&accepted) else {
            return Vec::new();
        };
        // This is the transfer decision linearization point. Offer acceptance
        // and an explicit Assistance Decline now contend on the SAME mailbox
        // mutex/CAS: accept-first makes Decline fail, while decline-first
        // makes this reservation fail before any winner or ACK is recorded.
        if let Some(transfer_request_id) = minted.claims.accepted_transfer_request_id.as_deref() {
            let fence = crate::assistance::AssistanceCallFence {
                call_id: minted.claims.call_id.clone(),
                call_epoch: minted.claims.call_epoch,
                owner_epoch: minted.claims.owner_epoch,
                switchboard_revision: minted.claims.switchboard_revision,
                remote_revision: minted.claims.remote_revision,
            };
            if self
                .assistance
                .accept_transfer(transfer_request_id, &fence, device_id)
                .is_err()
            {
                return self.relay_reject(
                    device_id,
                    request_id,
                    "transfer_unavailable",
                    "that transfer was declined, expired, or accepted by another endpoint",
                );
            }
        }
        if let Some(stored) = self.relay_offers.get_mut(&frame.offer_id) {
            stored.accepted = true;
        }
        self.relay_offer_winners.insert(
            minted.claims.opportunity_id.clone(),
            minted.claims.offer_id.clone(),
        );
        self.relay_record_replay(
            replay_key,
            fingerprint,
            device_id.to_owned(),
            RelayReplayResult::OfferAccepted {
                encoded: response.clone(),
                offer_id: minted.claims.offer_id.clone(),
                mode: minted_mode,
                required_grants: minted.claims.required_grants.clone(),
            },
        );
        vec![response]
    }

    /// Mint a lease, or say plainly why not.
    ///
    /// This is the only place in the plugin that creates media authority, so
    /// every claim in the minted lease comes from live radio truth or from the
    /// proved peer identity — never from the requesting frame. The frame's own
    /// numbers are used for one thing only: to check that the device is asking
    /// about the call state it thinks it is.
    pub(super) fn relay_lease_request(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<LeaseRequestFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let replay_key = format!(
            "lease\u{1f}{}\u{1f}{}",
            frame.accepted_offer_id, frame.idempotency_key
        );
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            let device_id = replay.device_id.clone();
            if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "device_unknown",
                    "this device has not introduced itself on this relay session",
                );
            }
            let replay_mode = match &replay.result {
                RelayReplayResult::LeaseStatus { lease_id } => {
                    self.relay_leases.get(lease_id).map(|entry| entry.mode)
                }
                _ => None,
            };
            self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::LeaseStatus { lease_id } => {
                    let Some(mode) = replay_mode else {
                        return self.relay_reject(
                            &device_id,
                            &frame.request_id,
                            "lease_unknown",
                            "that lease is no longer current",
                        );
                    };
                    if !self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) {
                        return self.relay_reject(
                            &device_id,
                            &frame.request_id,
                            "grant_required",
                            "the authenticated admission no longer grants this media mode",
                        );
                    }
                    self.relay_current_lease_status(&lease_id, &frame.request_id, media)
                        .unwrap_or_else(|| {
                            self.relay_reject(
                                &device_id,
                                &frame.request_id,
                                "lease_unknown",
                                "that lease is no longer current",
                            )
                        })
                }
                RelayReplayResult::Rejected { encoded } => vec![encoded],
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        // The request names no device, so the accepted offer is what identifies
        // the asker. An unknown or unanswered offer means we have no idea who
        // this is, and there is nothing to address a refusal to either.
        let Some(minted) = self.relay_offers.get(&frame.accepted_offer_id).cloned() else {
            let Some(device_id) = authenticated_subject.filter(|device_id| {
                self.relay_sender_owns_device(from_party, authenticated_subject, device_id)
                    && self.relay_peers.contains_key(*device_id)
            }) else {
                return Vec::new();
            };
            return self.relay_reject(
                device_id,
                &frame.request_id,
                "offer_retired",
                "that accepted offer is no longer redeemable; wait for fresh authenticated state",
            );
        };
        if !minted.accepted || minted.claims.jti != frame.accepted_offer_jti {
            return Vec::new();
        }
        let device_id = minted.claims.target_device_id.clone();
        let offered_mode = minted.claims.offered_mode;
        let request_id = frame.request_id.clone();
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        if !self.relay_mode_is_authorized(&device_id, authenticated_grants, offered_mode) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        if minted.claims.accepted_transfer_request_id.is_some()
            && !self.relay_has_exact_grants(
                &device_id,
                authenticated_grants,
                &[
                    Grant::StateRead,
                    Grant::RtcSignal,
                    Grant::Takeover,
                    Grant::AssistanceRespond,
                ],
            )
        {
            return self.relay_reject(
                &device_id,
                &request_id,
                "grant_required",
                "request-bound transfer acceptance requires AssistanceRespond and Takeover",
            );
        }
        if self.relay_over_budget(&device_id) {
            self.retire_failed_relay_offer(minted.clone());
            return self.relay_reject(&device_id, &request_id, "rate_limited", "too many requests");
        }
        if !self.relay_replay_has_room(&replay_key) {
            self.retire_failed_relay_offer(minted.clone());
            return self.relay_reject(
                &device_id,
                &request_id,
                "rate_limited",
                "the replay ledger is full",
            );
        }
        // Spend the offer here, whatever happens next. A refusal below is a
        // decision about this claim, and letting the same invitation be
        // redeemed again would turn one offer into an unbounded retry budget.
        let Some(spent_offer) = self.relay_offers.remove(&frame.accepted_offer_id) else {
            return Vec::new();
        };
        if spent_offer.claims.accepted_transfer_request_id.is_some() {
            self.relay_redeeming_transfer_offers
                .insert(spent_offer.claims.offer_id.clone(), spent_offer.clone());
        }
        if frame.accepted_transfer_request_id != spent_offer.claims.accepted_transfer_request_id {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "offer_substitution",
                "the transfer request does not match the exact signed offer",
            );
        }
        if frame.mode != offered_mode {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "that mode is not the one the accepted offer allowed",
            );
        }
        // One claimant at a time. Two prepared claims would race for the same
        // physical route, and the loser's soft hold would sit on a caller that
        // the winner is already taking.
        if self.prepared.is_some()
            || self.deferred_prepare.is_some()
            || self.pending_relay_status.is_some()
            || self.relay_leases.len() >= MAX_RELAY_LEASES
            || self
                .relay_leases
                .values()
                .any(|lease| lease.device_id == device_id)
        {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "claimant_busy",
                "another media claim is already in flight for this call",
            );
        }
        let Some(peer) = self.relay_peers.get(&device_id) else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        };
        let mobile_key_thumbprint = peer.holder_key_thumbprint.clone();
        let session_nonce = peer.session_nonce.clone();
        if !self
            .endpoint_authority
            .approved_mobile_keys
            .contains_key(&mobile_key_thumbprint)
        {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "device_unknown",
                "this device is no longer on the owner-approved roster",
            );
        }
        let Some(media) = radio.remote_media() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "stale_call",
                "there is no media endpoint to claim",
            );
        };
        let remote = media.snapshot();
        // The SAME predicate `validate_notice` applies to a gateway-minted
        // lease, applied before minting rather than after receiving.
        if remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.expected_call_epoch
            || remote.owner_epoch != frame.expected_owner_epoch
            || remote.remote_revision != frame.expected_remote_revision
            || radio.switchboard_revision() != frame.expected_switchboard_revision
        {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "stale_call",
                "the physical call moved on before this claim arrived",
            );
        }
        // A fast, honest refusal. The authoritative consent check runs inside
        // the media state lock on every transition, and again per caller-bound
        // frame; this one exists so a device is told why instead of watching a
        // claim die silently later.
        let consented = remote.consent.enabled
            && remote.consent.acknowledged
            && match frame.mode {
                LeaseMode::Monitor => remote.consent.monitor_enabled,
                LeaseMode::Consult => remote.consent.consult_enabled,
                LeaseMode::Takeover => remote.consent.takeover_enabled,
            };
        if !consented {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "consent_required",
                "current disclosure consent does not allow this mode",
            );
        }
        if matches!(frame.mode, LeaseMode::Consult) {
            let fenced = self
                .assistance
                .pending_frame(&self.app_id)
                .is_some_and(|assistance| {
                    assistance.call_id == frame.call_id
                        && assistance.call_epoch == remote.call_epoch
                        && assistance.owner_epoch == remote.owner_epoch
                        && assistance.switchboard_revision == radio.switchboard_revision()
                        && assistance.remote_revision == remote.remote_revision
                });
            if !fenced {
                return self.relay_recorded_rejection(
                    &replay_key,
                    &fingerprint,
                    &device_id,
                    &request_id,
                    "assistance_required",
                    "a private consultation needs a current Aokie assistance request",
                );
            }
        }
        let Ok(now) = unix_now() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "stale_call",
                "clock unavailable",
            );
        };
        let accepted_transfer =
            if let Some(transfer_request_id) = frame.accepted_transfer_request_id.as_deref() {
                let Some(transfer) = self
                    .assistance
                    .pending_transfer(&frame.call_id, remote.call_epoch)
                else {
                    return self.relay_recorded_rejection(
                        &replay_key,
                        &fingerprint,
                        &device_id,
                        &request_id,
                        "transfer_unavailable",
                        "that transfer request is no longer available",
                    );
                };
                if transfer.request_id != transfer_request_id
                    || transfer.expires_at <= now
                    || transfer.fence.call_id != frame.call_id
                    || transfer.fence.call_epoch != remote.call_epoch
                    || transfer.fence.owner_epoch != remote.owner_epoch
                    || transfer.fence.switchboard_revision != radio.switchboard_revision()
                    || transfer.fence.remote_revision != remote.remote_revision
                    || transfer
                        .accepted_by
                        .as_deref()
                        .is_some_and(|accepted| accepted != device_id)
                {
                    return self.relay_recorded_rejection(
                        &replay_key,
                        &fingerprint,
                        &device_id,
                        &request_id,
                        "transfer_unavailable",
                        "that transfer request changed or another owner endpoint accepted it",
                    );
                }
                Some(transfer)
            } else {
                None
            };
        let phase = if matches!(frame.mode, LeaseMode::Monitor) {
            LeasePhase::Active
        } else {
            LeasePhase::Prepared
        };
        let fence = if matches!(frame.mode, LeaseMode::Takeover) {
            let fence = self.next_takeover_fence;
            self.next_takeover_fence = self.next_takeover_fence.saturating_add(1);
            fence
        } else {
            0
        };
        let lease = LeaseClaims {
            aud: LEASE_AUDIENCE.into(),
            app_id: self.app_id.clone(),
            plugin_id: self.plugin_id.clone(),
            device_id: device_id.clone(),
            plugin_key_thumbprint: self.endpoint_authority.endpoint_key.thumbprint.clone(),
            mobile_key_thumbprint,
            // Physical truth, not the frame's copy of it. The two were just
            // proved equal, and taking the radio's own values means a lease can
            // never describe a call that does not exist.
            call_id: remote.call_id.clone().unwrap_or_default(),
            call_epoch: remote.call_epoch,
            owner_epoch: remote.owner_epoch,
            mode: frame.mode,
            phase,
            tracks: tracks_for(frame.mode, phase),
            expires_at: now.saturating_add(if matches!(phase, LeasePhase::Prepared) {
                RELAY_PREPARED_LEASE_TTL
            } else {
                RELAY_ACTIVE_LEASE_TTL
            }),
            lease_id: format!("lease_{}", uuid::Uuid::new_v4().simple()),
            jti: format!("leasejti_{}", uuid::Uuid::new_v4().simple()),
            fence,
            // The nonce the device proved in its hello. A lease carrying any
            // other value would reject every RTC signal that device sends.
            session_nonce,
            rtc_session_id: frame.rtc_session_id.clone(),
        };
        // Never emit a lease that would not survive the checks a received one
        // faces. Minting it does not make it valid.
        if lease.validate(now).is_err() {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "the requested lease could not be formed safely",
            );
        }
        let Ok(signing_bytes) = lease.signing_bytes() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "the requested lease could not be signed",
            );
        };
        let token = self.endpoint_authority.sign(&signing_bytes);
        let notice = LeaseNotice {
            kind: "lease_granted".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.clone(),
            request_id: Some(request_id.clone()),
            lease_token: token.clone(),
            lease: lease.clone(),
            accepted_transfer_request_id: frame.accepted_transfer_request_id.clone(),
        };
        let status = if matches!(phase, LeasePhase::Active) {
            PluginLeaseStatus::Granted
        } else {
            PluginLeaseStatus::Provisional
        };
        let frames =
            self.relay_lease_status(status, &device_id, &request_id, &token, lease.clone(), now);
        let Some(encoded_status) = frames.first().cloned() else {
            return self.relay_recorded_rejection(
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "mode_unavailable",
                "the lease status could not be encoded safely",
            );
        };
        let _ = accepted_transfer;
        self.relay_leases.insert(
            lease.lease_id.clone(),
            RelayLease {
                lease_id: lease.lease_id.clone(),
                device_id: device_id.clone(),
                request_id: request_id.clone(),
                token: token.clone(),
                current_jti: lease.jti.clone(),
                phase,
                status,
                mode: offered_mode,
            },
        );
        self.relay_record_replay(
            replay_key.clone(),
            fingerprint,
            device_id.clone(),
            RelayReplayResult::LeaseStatus {
                lease_id: lease.lease_id.clone(),
            },
        );
        self.relay_redeeming_transfer_offers
            .remove(&spent_offer.claims.offer_id);
        if matches!(phase, LeasePhase::Active) {
            // Even monitor authority is committed only after delivery. A
            // dropped status must not leave a hidden lease blocking a retry.
            self.pending_relay_status = Some(PendingRelayStatus::MonitorGrant {
                encoded: encoded_status.clone(),
                notice,
                lease_id: lease.lease_id.clone(),
                offer_id: frame.accepted_offer_id.clone(),
                offer: spent_offer,
                replay_key,
            });
        } else {
            // Consult and takeover are minted here but NOT armed. Arming leads
            // to a soft hold on a live caller and therefore waits for delivery.
            self.deferred_prepare = Some(DeferredPrepare {
                encoded: encoded_status.clone(),
                notice: LeaseNotice {
                    kind: "claim_proposal".into(),
                    ..notice
                },
                expected_switchboard_revision: spent_offer.claims.switchboard_revision,
                lease_id: lease.lease_id.clone(),
                device_id: device_id.clone(),
                request_id: request_id.clone(),
                offer: spent_offer,
                replay_key,
            });
        }
        eprintln!(
            "[aokie-plugin][takeover] stage=relay_lease_minted device={} call={} mode={:?} phase={:?} fence={} lease={} rtc={}",
            sanitize_gateway_code(&device_id),
            lease.call_id,
            lease.mode,
            lease.phase,
            lease.fence,
            lease.lease_id,
            lease.rtc_session_id
        );
        vec![encoded_status]
    }
}
