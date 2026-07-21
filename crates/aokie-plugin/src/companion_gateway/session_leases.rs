//! `GatewaySession` lease/roster bookkeeping.

#[allow(unused_imports)]
use super::*;

impl GatewaySession {
    pub(super) fn handle_rtc_signal(
        &mut self,
        frame: PluginRtcSignalFrame,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        if frame.app_id != self.app_id || frame.plugin_id != self.plugin_id {
            return Err(WorkerError::rebootstrap(
                "Companion RTC signal crossed application identity",
            ));
        }
        let now = unix_now()?;
        let authentication = frame
            .signal
            .verify_endpoint_authentication(now)
            .map_err(|_| WorkerError::reconnect("Companion RTC endpoint signature is invalid"))?;
        if let Some(authentication) = authentication {
            let expected = self
                .leases
                .get(&frame.lease_jti)
                .or_else(|| self.prepared.as_ref().map(|prepared| &prepared.provisional))
                .ok_or_else(|| {
                    WorkerError::reconnect("Companion RTC signature has no exact lease context")
                })?;
            let supplied_key = match &frame.signal {
                RtcSignal::Offer { binding, .. } | RtcSignal::Answer { binding, .. } => {
                    &binding.endpoint_key
                }
                RtcSignal::Ice { envelope, .. } | RtcSignal::IceComplete { envelope } => {
                    &envelope.endpoint_key
                }
                RtcSignal::Close { .. } => unreachable!("authenticated signal is not close"),
            };
            let approved_key = self
                .endpoint_authority
                .approved_mobile_keys
                .get(authentication.holder_key_thumbprint())
                .ok_or_else(|| {
                    WorkerError::rebootstrap(
                        "Companion RTC signer is absent from the local owner-approved roster",
                    )
                })?;
            if supplied_key != approved_key
                || authentication.endpoint_role() != AdmissionRole::Mobile
                || authentication.endpoint_session_nonce() != expected.session_nonce
                || authentication.holder_key_thumbprint() != expected.mobile_key_thumbprint
                || authentication.peer_key_thumbprint() != expected.plugin_key_thumbprint
            {
                return Err(WorkerError::rebootstrap(
                    "Companion RTC signer does not match the explicit local roster and lease",
                ));
            }
            self.used_endpoint_jtis
                .retain(|_, expires_at| *expires_at > now);
            if self.used_endpoint_jtis.contains_key(authentication.jti()) {
                return Err(WorkerError::reconnect(
                    "Companion RTC endpoint signature was replayed",
                ));
            }
            if self.used_endpoint_jtis.len() >= MAX_USED_ENDPOINT_JTIS {
                return Err(WorkerError::reconnect(
                    "Companion RTC endpoint replay cache is exhausted",
                ));
            }
            self.used_endpoint_jtis
                .insert(authentication.jti().to_owned(), authentication.expires_at());
        }
        match frame.signal.clone() {
            RtcSignal::Offer { sdp, .. } => self.open_offer(frame, sdp, media, radio),
            RtcSignal::Ice {
                candidate,
                sdp_mid,
                sdp_m_line_index,
                ..
            } => {
                let route = self.route_for_frame(&frame)?;
                if route.sdp_revision != frame.sdp_revision
                    || route.transport_generation != frame.transport_generation
                {
                    return Err(WorkerError::reconnect(
                        "Companion ICE signal has a stale SDP generation",
                    ));
                }
                let sdp_mid = sdp_mid
                    .ok_or_else(|| WorkerError::reconnect("Companion ICE signal omitted sdpMid"))?;
                let index = sdp_m_line_index.ok_or_else(|| {
                    WorkerError::reconnect("Companion ICE signal omitted sdpMLineIndex")
                })?;
                media
                    .add_remote_ice(
                        &frame.rtc_session_id,
                        IceCandidateSignal {
                            sdp_mid,
                            sdp_mline_index: i32::from(index),
                            candidate,
                        },
                    )
                    .map_err(|error| {
                        eprintln!(
                            "[aokie-plugin][takeover] stage=remote_ice_rejected call={} owner_epoch={} fence={} rtc={} detail={}",
                            frame.call_id,
                            frame.owner_epoch,
                            frame.fence,
                            frame.rtc_session_id,
                            sanitize_status_message(&error)
                        );
                        WorkerError::reconnect(format!(
                            "Remote ICE candidate was rejected: {}",
                            sanitize_status_message(&error)
                        ))
                    })
            }
            RtcSignal::IceComplete { .. } => {
                let _ = self.route_for_frame(&frame)?;
                Ok(())
            }
            RtcSignal::Close { reason } => {
                let route = self.route_for_frame(&frame)?.binding.clone();
                let lease_id = route
                    .lease_id
                    .clone()
                    .ok_or_else(|| WorkerError::reconnect("Companion RTC route omitted leaseId"))?;
                if matches!(
                    route.mode,
                    MediaMode::PreparedConsult
                        | MediaMode::Consult
                        | MediaMode::PreparedTalk
                        | MediaMode::Talk
                ) {
                    let _ = media.revoke(&route, &reason);
                }
                let _ = media.close_peer(&frame.rtc_session_id, &reason);
                self.peers.remove(&frame.rtc_session_id);
                // Close is a terminal media statement, not merely a peer
                // transport hint. Purge every JTI for the stable lease and
                // its claimant/relay registry now; the ensuing native terminal
                // event may be coalesced or dropped and must not be required to
                // make future heartbeats fail.
                self.leases.retain(|_, claims| claims.lease_id != lease_id);
                if self
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.provisional.lease_id == lease_id)
                {
                    self.prepared = None;
                }
                self.retire_relay_lease(&lease_id);
                self.last_snapshot_fingerprint = None;
                self.last_snapshot_sent = None;
                self.next_snapshot_poll = Instant::now();
                Ok(())
            }
            RtcSignal::Answer { .. } => Err(WorkerError::reconnect(
                "Companion endpoint sent an answer in the offer direction",
            )),
        }
    }

    pub(super) fn open_offer(
        &mut self,
        frame: PluginRtcSignalFrame,
        sdp: String,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        // Capture this BEFORE reading the public switch marker/revision. If a
        // CHLD wins after either read but before the media manager dequeues the
        // open, its dedicated epoch changes and the queued peer fails closed.
        // Capturing after admission would bless the already-started switch.
        let admitted_switch_epoch = media.capture_aokie_switch_epoch().map_err(|_| {
            WorkerError::reconnect("Companion media switchboard proof is unavailable")
        })?;
        if radio.switch_in_flight() {
            return Err(WorkerError::reconnect(
                "Companion media offer arrived while the physical switchboard was moving",
            ));
        }
        if let Some((lease_id, expected_revision)) = self.prepared.as_ref().and_then(|prepared| {
            prepared
                .expected_switchboard_revision
                .map(|revision| (prepared.provisional.lease_id.clone(), revision))
        }) {
            if radio.switchboard_revision() != expected_revision {
                self.revoke_relay_lease_by_id(&lease_id, "switchboard_changed_before_media", media);
                return Err(WorkerError::reconnect(
                    "Companion media offer crossed a physical switchboard transition",
                ));
            }
        }
        if let Some(lease_id) = self.prepared.as_ref().and_then(|prepared| {
            prepared
                .active_rebind_deadline
                .filter(|deadline| *deadline <= Instant::now())
                .map(|_| prepared.provisional.lease_id.clone())
        }) {
            self.revoke_relay_lease_by_id(&lease_id, "active_peer_not_opened", media);
            return Err(WorkerError::reconnect(
                "Active Companion media offer arrived after its bounded handoff window",
            ));
        }
        let now = unix_now()?;
        let claims = if let Some(claims) = self.leases.get(&frame.lease_jti).cloned() {
            claims
        } else {
            let prepared = self.prepared.as_ref().ok_or_else(|| {
                WorkerError::reconnect("Active consult/takeover offer has no prepared claim")
            })?;
            let confirmed = prepared.confirmed_owner_epoch.ok_or_else(|| {
                WorkerError::reconnect("Active offer arrived before physical preparation")
            })?;
            let old = &prepared.provisional;
            if frame.device_id != old.device_id
                || frame.rtc_session_id != old.rtc_session_id
                || frame.call_id != old.call_id
                || frame.call_epoch != old.call_epoch
                || frame.owner_epoch != confirmed
                || frame.fence != old.fence
                || frame.sdp_revision <= prepared.provisional_sdp_revision
                || frame.transport_generation <= prepared.provisional_transport_generation
            {
                return Err(WorkerError::reconnect(
                    "Active offer does not advance the prepared binding",
                ));
            }
            let mut active = old.clone();
            active.owner_epoch = confirmed;
            active.phase = LeasePhase::Active;
            active.tracks = aokie_protocol::v2::tracks_for(active.mode, LeasePhase::Active);
            active.jti = frame.lease_jti.clone();
            active.expires_at = now.saturating_add(ACTIVE_LEASE_FALLBACK_TTL);
            active
        };
        if matches!(claims.mode, LeaseMode::Consult | LeaseMode::Takeover)
            && claims.phase == LeasePhase::Active
        {
            let prepared = self.prepared.as_ref().ok_or_else(|| {
                WorkerError::reconnect("Active consult/takeover offer has no prepared claim")
            })?;
            if prepared.confirmed_owner_epoch != Some(claims.owner_epoch)
                || frame.sdp_revision <= prepared.provisional_sdp_revision
                || frame.transport_generation <= prepared.provisional_transport_generation
            {
                return Err(WorkerError::reconnect(
                    "Active offer did not advance SDP and transport generations",
                ));
            }
        }
        if claims.jti != frame.lease_jti
            || claims.device_id != frame.device_id
            || claims.rtc_session_id != frame.rtc_session_id
            || claims.call_id != frame.call_id
            || claims.call_epoch != frame.call_epoch
            || claims.owner_epoch != frame.owner_epoch
            || claims.fence != frame.fence
        {
            return Err(WorkerError::reconnect(
                "Companion RTC offer does not match its lease binding",
            ));
        }
        claims
            .validate(now)
            .map_err(|_| WorkerError::reconnect("Companion RTC lease is invalid"))?;
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
            .snapshot();
        if remote.call_id.as_deref() != Some(claims.call_id.as_str())
            || remote.call_epoch != claims.call_epoch
            || remote.owner_epoch != claims.owner_epoch
        {
            return Err(WorkerError::reconnect(
                "Companion RTC offer is stale against physical call truth",
            ));
        }
        if let Some(existing) = self.peers.get(&frame.rtc_session_id) {
            let replacing_prepared = matches!(
                (existing.binding.mode, claims.mode, claims.phase),
                (
                    MediaMode::PreparedTalk,
                    LeaseMode::Takeover,
                    LeasePhase::Active
                ) | (
                    MediaMode::PreparedConsult,
                    LeaseMode::Consult,
                    LeasePhase::Active
                )
            );
            if !replacing_prepared {
                return Err(WorkerError::reconnect(
                    "Companion RTC session already has a native peer",
                ));
            }
            let _ = media.close_peer(&frame.rtc_session_id, "active_rebind");
            self.peers.remove(&frame.rtc_session_id);
        }
        let binding = binding_for_claims(&claims);
        let ttl = lease_ttl_ms(&claims)?;
        let offer = SdpSignal {
            kind: SdpSignalType::Offer,
            sdp,
        };
        offer
            .validate()
            .map_err(|_| WorkerError::reconnect("Companion SDP offer is invalid"))?;
        media
            .open_peer(OpenPeerRequest {
                binding: binding.clone(),
                offer,
                lease_ttl_ms: ttl,
                ice_servers: self.ice_servers.clone(),
                relay_only: self.relay_only,
                expected_switch_epoch: (binding.mode != MediaMode::Monitor)
                    .then_some(admitted_switch_epoch),
            })
            .map_err(|error| {
                eprintln!(
                    "[aokie-plugin][takeover] stage=peer_open_failed call={} mode={:?} owner_epoch={} fence={} rtc={} detail={}",
                    binding.call_id,
                    binding.mode,
                    binding.owner_epoch,
                    binding.fence,
                    binding.rtc_session_id,
                    sanitize_status_message(&error)
                );
                WorkerError::reconnect(format!(
                    "Native Companion peer could not open: {}",
                    sanitize_status_message(&error)
                ))
            })?;
        if claims.phase == LeasePhase::Active {
            if let Some(prepared) = self
                .prepared
                .as_mut()
                .filter(|prepared| prepared.provisional.lease_id == claims.lease_id)
            {
                // The native media state now owns its own non-renewable PCM
                // readiness deadline. Retire the earlier "active status but
                // no replacement peer" guard so the two stages cannot race.
                prepared.active_rebind_deadline = None;
            }
        }
        eprintln!(
            "[aokie-plugin][takeover] stage=peer_opened call={} mode={:?} owner_epoch={} fence={} sdp={} generation={} rtc={}",
            binding.call_id,
            binding.mode,
            binding.owner_epoch,
            binding.fence,
            frame.sdp_revision,
            frame.transport_generation,
            binding.rtc_session_id
        );
        if matches!(
            binding.mode,
            MediaMode::PreparedTalk | MediaMode::PreparedConsult
        ) {
            let prepared = self
                .prepared
                .as_mut()
                .ok_or_else(|| WorkerError::reconnect("Prepared takeover context disappeared"))?;
            prepared.provisional_sdp_revision = frame.sdp_revision;
            prepared.provisional_transport_generation = frame.transport_generation;
        }
        self.leases.insert(claims.jti.clone(), claims);
        self.peers.insert(
            frame.rtc_session_id,
            PeerRoute {
                binding,
                lease_jti: frame.lease_jti,
                device_id: frame.device_id,
                sdp_revision: frame.sdp_revision,
                transport_generation: frame.transport_generation,
                lease_ttl_ms: ttl,
                connected: false,
                remote_audio_ready: false,
                remote_microphone_ready: false,
                transition_requested: false,
            },
        );
        Ok(())
    }

    pub(super) fn route_for_frame(&self, frame: &PluginRtcSignalFrame) -> Result<&PeerRoute, WorkerError> {
        let route = self.peers.get(&frame.rtc_session_id).ok_or_else(|| {
            WorkerError::reconnect("Companion RTC signal names an unknown native peer")
        })?;
        if route.lease_jti != frame.lease_jti
            || route.device_id != frame.device_id
            || route.binding.call_id != frame.call_id
            || route.binding.call_epoch != frame.call_epoch
            || route.binding.owner_epoch != frame.owner_epoch
            || route.binding.fence != frame.fence
        {
            return Err(WorkerError::reconnect(
                "Companion RTC signal does not match the immutable peer binding",
            ));
        }
        Ok(route)
    }

    pub(super) fn handle_lease_revoked(
        &mut self,
        notice: LeaseRevokedNotice,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        let _ = (&notice.kind, notice.schema_version);
        if notice.app_id != self.app_id {
            return Err(WorkerError::rebootstrap(
                "Companion revocation crossed application identity",
            ));
        }
        let route_id = self.peers.iter().find_map(|(id, route)| {
            (route.lease_jti == notice.lease_jti
                && route.device_id == notice.device_id
                && route.binding.lease_id.as_deref() == Some(notice.lease_id.as_str())
                && route.binding.call_id == notice.call_id
                && route.binding.call_epoch == notice.call_epoch
                && route.binding.fence == notice.fence)
                .then(|| id.clone())
        });
        if let Some(route_id) = route_id {
            if let Some(route) = self.peers.remove(&route_id) {
                if matches!(
                    route.binding.mode,
                    MediaMode::PreparedConsult
                        | MediaMode::Consult
                        | MediaMode::PreparedTalk
                        | MediaMode::Talk
                ) {
                    let _ = media.revoke(&route.binding, &notice.reason);
                }
                let _ = media.close_peer(&route_id, &notice.reason);
            }
        } else if let Some(prepared) = self.prepared.as_ref().filter(|prepared| {
            prepared.provisional.lease_id == notice.lease_id
                && prepared.provisional.device_id == notice.device_id
                && prepared.provisional.call_id == notice.call_id
                && prepared.provisional.call_epoch == notice.call_epoch
                && prepared.provisional.fence == notice.fence
        }) {
            let mut binding = binding_for_claims(&prepared.provisional);
            if let Some(owner_epoch) = prepared.confirmed_owner_epoch {
                binding.owner_epoch = owner_epoch;
                binding.mode = match prepared.provisional.mode {
                    LeaseMode::Consult => MediaMode::Consult,
                    LeaseMode::Takeover => MediaMode::Talk,
                    LeaseMode::Monitor => binding.mode,
                };
            }
            let _ = media.revoke(&binding, &notice.reason);
        }
        self.relay_end_caller_challenges
            .retain(|_, challenge| challenge.lease_jti != notice.lease_jti);
        self.leases.remove(&notice.lease_jti);
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.provisional.lease_id == notice.lease_id)
        {
            self.prepared = None;
        }
        // A revoked lease must leave the relay registry too, or its token would
        // still be recognised and a heartbeat could renew authority that was
        // just withdrawn.
        self.retire_relay_lease(&notice.lease_id);
        Ok(())
    }

    pub(super) fn drain_media_events(
        &mut self,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let mut outbound = Vec::new();
        for event in media.drain_events(64) {
            let event_detail = match &event.kind {
                RemoteMediaEventKind::ProtocolViolation { message } => {
                    sanitize_status_message(message)
                }
                RemoteMediaEventKind::Error { operation, message } => format!(
                    "operation={} message={}",
                    sanitize_gateway_code(operation),
                    sanitize_status_message(message)
                ),
                RemoteMediaEventKind::ConnectionState { state } => {
                    format!("state={}", sanitize_gateway_code(state))
                }
                RemoteMediaEventKind::Closed { reason }
                | RemoteMediaEventKind::ReturningToAokie { reason } => {
                    format!("reason={}", sanitize_status_message(reason))
                }
                _ => "none".to_string(),
            };
            eprintln!(
                "[aokie-plugin][takeover] stage=media_event call={} owner_epoch={} rtc={} event={} detail={}",
                event.call_id,
                event.owner_epoch,
                event.rtc_session_id,
                remote_media_event_kind(&event.kind),
                event_detail,
            );
            match event.kind.clone() {
                RemoteMediaEventKind::TakeoverPrepared {
                    confirmed_owner_epoch,
                } => {
                    if let Some(encoded) =
                        self.complete_preparation(&event, confirmed_owner_epoch, media, radio)?
                    {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::ConsultPrepared {
                    confirmed_owner_epoch,
                } => {
                    if let Some(encoded) =
                        self.complete_preparation(&event, confirmed_owner_epoch, media, radio)?
                    {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::SdpAnswer { answer } => {
                    if answer.kind != SdpSignalType::Answer {
                        return Err(WorkerError::reconnect(
                            "Native media produced a non-answer SDP response",
                        ));
                    }
                    if let Some(route) = self.route_for_event(&event) {
                        outbound
                            .push(self.encode_rtc(route, OutboundRtcSignal::Answer(answer.sdp))?);
                    }
                }
                RemoteMediaEventKind::LocalIce { candidate } => {
                    if let Some(route) = self.route_for_event(&event) {
                        let index = u16::try_from(candidate.sdp_mline_index).map_err(|_| {
                            WorkerError::reconnect("Native ICE candidate index is invalid")
                        })?;
                        outbound.push(self.encode_rtc(
                            route,
                            OutboundRtcSignal::Ice {
                                candidate: candidate.candidate,
                                sdp_mid: candidate.sdp_mid,
                                sdp_m_line_index: index,
                            },
                        )?);
                    }
                }
                RemoteMediaEventKind::IceComplete => {
                    if let Some(route) = self.route_for_event(&event) {
                        outbound.push(self.encode_rtc(route, OutboundRtcSignal::IceComplete)?);
                    }
                }
                RemoteMediaEventKind::ConnectionState { state } => {
                    let failed = matches!(state.as_str(), "failed" | "disconnected" | "closed");
                    if let Some(route) = self.route_for_event_mut(&event) {
                        route.connected = state == "connected";
                    }
                    if failed {
                        if let Some(encoded) =
                            self.fail_peer(&event.rtc_session_id, "peer_connection_failed", media)?
                        {
                            outbound.push(encoded);
                        }
                    } else {
                        self.maybe_request_transition(&event.rtc_session_id, media)?;
                    }
                }
                RemoteMediaEventKind::RemoteAudioReady => {
                    if let Some(route) = self.route_for_event_mut(&event) {
                        route.remote_audio_ready = true;
                    }
                    self.maybe_request_transition(&event.rtc_session_id, media)?;
                }
                RemoteMediaEventKind::RemoteMicrophoneReady => {
                    if let Some(route) = self.route_for_event_mut(&event) {
                        // This event came from first decoded PCM on the exact
                        // direct WebRTC Talk peer. Its frame remains
                        // quarantined until the radio opens the route.
                        route.remote_microphone_ready = true;
                    }
                    self.maybe_request_transition(&event.rtc_session_id, media)?;
                }
                RemoteMediaEventKind::ProtocolViolation { .. }
                | RemoteMediaEventKind::Error { .. } => {
                    if let Some(encoded) =
                        self.fail_peer(&event.rtc_session_id, "media_protocol_failure", media)?
                    {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::ReturningToAokie { reason }
                | RemoteMediaEventKind::Closed { reason } => {
                    // Gateway-authoritative operator returns and remote Close
                    // signals remove `PeerRoute` before native media emits its
                    // terminal event.  A still-registered exact route therefore
                    // means Desktop/radio failed it locally (for example SCO
                    // vanished during PrepareHuman/EnterHuman).  Revoke that
                    // exact lease so a prepared/active takeover cannot remain
                    // renewable after Desktop has already returned to Aokie.
                    if let Some(encoded) = self.fail_route_if_current(&event, &reason, media)? {
                        outbound.push(encoded);
                    }
                }
                RemoteMediaEventKind::TakeoverPending
                | RemoteMediaEventKind::ConsultActive
                | RemoteMediaEventKind::HumanActive
                | RemoteMediaEventKind::AokieActive => {}
            }
        }
        Ok(outbound)
    }

    pub(super) fn fail_route_if_current(
        &mut self,
        event: &RemoteMediaEvent,
        reason: &str,
        media: &RemoteMediaHandle,
    ) -> Result<Option<String>, WorkerError> {
        if self.route_for_event(event).is_none() {
            return Ok(None);
        }
        self.fail_peer(&event.rtc_session_id, reason, media)
    }

    pub(super) fn route_for_event(&self, event: &RemoteMediaEvent) -> Option<&PeerRoute> {
        self.peers.get(&event.rtc_session_id).filter(|route| {
            route.binding.call_id == event.call_id
                && route.binding.call_epoch == event.call_epoch
                && route.binding.owner_epoch == event.owner_epoch
        })
    }

    pub(super) fn route_for_event_mut(&mut self, event: &RemoteMediaEvent) -> Option<&mut PeerRoute> {
        self.peers.get_mut(&event.rtc_session_id).filter(|route| {
            route.binding.call_id == event.call_id
                && route.binding.call_epoch == event.call_epoch
                && route.binding.owner_epoch == event.owner_epoch
        })
    }

    pub(super) fn maybe_request_transition(
        &mut self,
        rtc_session_id: &str,
        media: &RemoteMediaHandle,
    ) -> Result<(), WorkerError> {
        let action = self.peers.get(rtc_session_id).and_then(|route| {
            if route.transition_requested || !route.connected {
                return None;
            }
            match route.binding.mode {
                MediaMode::PreparedConsult => {
                    Some((0_u8, route.binding.clone(), route.lease_ttl_ms))
                }
                MediaMode::PreparedTalk => Some((1_u8, route.binding.clone(), route.lease_ttl_ms)),
                MediaMode::Consult if route.remote_audio_ready && route.remote_microphone_ready => {
                    Some((2_u8, route.binding.clone(), route.lease_ttl_ms))
                }
                MediaMode::Talk if route.remote_audio_ready && route.remote_microphone_ready => {
                    Some((3_u8, route.binding.clone(), route.lease_ttl_ms))
                }
                _ => None,
            }
        });
        let Some((action, binding, ttl)) = action else {
            return Ok(());
        };
        let accepted_transfer = if action == 3 {
            binding
                .lease_id
                .as_deref()
                .and_then(|lease_id| self.accepted_transfers.get(lease_id).cloned())
        } else {
            None
        };
        if let Some(accepted) = accepted_transfer.as_ref() {
            if let Some(lease_id) = binding.lease_id.as_deref() {
                let still_current = self
                    .assistance
                    .pending_transfer(
                        &accepted.offered_fence.call_id,
                        accepted.offered_fence.call_epoch,
                    )
                    .is_some_and(|pending| {
                        pending.request_id == accepted.request_id
                            && pending.fence == accepted.offered_fence
                            && pending.accepted_by.as_deref() == Some(accepted.device_id.as_str())
                            && pending.expires_at > unix_now().unwrap_or(u64::MAX)
                    });
                if !still_current {
                    if let Some(current) = self.accepted_transfers.get_mut(lease_id) {
                        current.failback_requested = true;
                    }
                    self.revoke_relay_lease_by_id(
                        lease_id,
                        "transfer_setup_expired_before_activation",
                        media,
                    );
                    return Ok(());
                }
            }
        }
        let action_name = match action {
            0 => "prepare_consult",
            1 => "prepare_takeover",
            2 => "enter_consult",
            _ => "enter_takeover",
        };
        eprintln!(
            "[aokie-plugin][takeover] stage=transition_requested action={} call={} owner_epoch={} fence={} rtc={}",
            action_name,
            binding.call_id,
            binding.owner_epoch,
            binding.fence,
            binding.rtc_session_id
        );
        let result = match action {
            0 => media.request_consult_hold(binding, ttl),
            1 => media.request_soft_hold(binding, ttl),
            2 => media.request_consult(binding, ttl),
            _ => match accepted_transfer {
                Some(accepted) => media.request_transfer_takeover(
                    binding,
                    ttl,
                    accepted.request_id,
                    accepted.offered_fence,
                    accepted.device_id,
                    accepted.setup_expires_at,
                ),
                None => media.request_takeover(binding, ttl),
            },
        };
        result.map_err(|_| {
            WorkerError::reconnect(match action {
                0 => "Prepared private consultation could not enter software hold",
                1 => "Prepared Companion takeover could not enter soft hold",
                2 => "Active private consultation could not enter its isolated route",
                _ => "Active Companion takeover could not enter the radio route",
            })
        })?;
        if let Some(route) = self.peers.get_mut(rtc_session_id) {
            route.transition_requested = true;
        }
        Ok(())
    }

    pub(super) fn complete_preparation(
        &mut self,
        event: &RemoteMediaEvent,
        confirmed_owner_epoch: u64,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<Option<String>, WorkerError> {
        let prepared = match self.prepared.as_mut() {
            Some(prepared)
                if prepared.provisional.rtc_session_id == event.rtc_session_id
                    && prepared.provisional.call_id == event.call_id
                    && prepared.provisional.call_epoch == event.call_epoch
                    && prepared.provisional.owner_epoch == event.owner_epoch
                    && !prepared.decision_sent =>
            {
                prepared
            }
            _ => return Ok(None),
        };
        let remote = media.snapshot();
        if remote.owner_epoch != confirmed_owner_epoch
            || confirmed_owner_epoch <= prepared.provisional.owner_epoch
        {
            return Err(WorkerError::reconnect(
                "Physical owner epoch did not advance during soft hold",
            ));
        }
        let decision = PluginClaimDecisionFrame {
            kind: "claim_decision".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            request_id: prepared.request_id.clone(),
            device_id: prepared.provisional.device_id.clone(),
            call_id: prepared.provisional.call_id.clone(),
            call_epoch: prepared.provisional.call_epoch,
            fence: prepared.provisional.fence,
            accepted: true,
            media_ready: true,
            confirmed_owner_epoch,
            switchboard_revision: radio.switchboard_revision(),
            remote_revision: remote.remote_revision,
            reason: None,
        };
        decision.validate().map_err(|_| {
            WorkerError::reconnect("Prepared claim decision failed local validation")
        })?;
        // On the relay there is no gateway to address a decision to, and no one
        // else to promote the prepared claim. The plugin consumes its own
        // decision instead: it mints the ACTIVE lease from the epoch the radio
        // just confirmed and hands it to the device, which is what lets the
        // Companion send the second, microphone-bearing offer.
        //
        // Everything the socket path does either side of this stays: the
        // ownerEpoch must still have strictly advanced above, and the
        // receive-only peer is still closed below so no active binding can be
        // reached without a fresh higher-generation offer.
        let relay_status = if self.relay_carrier && self.relay_authority_enabled {
            let lease_id = prepared.provisional.lease_id.clone();
            let mut active = prepared.provisional.clone();
            active.owner_epoch = confirmed_owner_epoch;
            active.phase = LeasePhase::Active;
            active.tracks = tracks_for(active.mode, LeasePhase::Active);
            active.jti = format!("leasejti_{}", uuid::Uuid::new_v4().simple());
            active.expires_at = unix_now()?.saturating_add(RELAY_ACTIVE_LEASE_TTL);
            Some((lease_id, active))
        } else {
            None
        };
        prepared.confirmed_owner_epoch = Some(confirmed_owner_epoch);
        prepared.decision_sent = true;
        eprintln!(
            "[aokie-plugin][takeover] stage=preparation_complete app={} device={} call={} mode={:?} owner_epoch={} fence={} rtc={}",
            self.app_id,
            prepared.provisional.device_id,
            prepared.provisional.call_id,
            prepared.provisional.mode,
            confirmed_owner_epoch,
            prepared.provisional.fence,
            prepared.provisional.rtc_session_id
        );

        // DesktopPeer bindings are immutable. Close the receive-only peer;
        // the gateway rotates JTI/ownerEpoch and the Companion must send a
        // fresh higher-generation offer before any microphone track exists.
        let _ = media.close_peer(&event.rtc_session_id, "awaiting_active_rebind");
        self.peers.remove(&event.rtc_session_id);
        if let Some((lease_id, active)) = relay_status {
            if self.pending_relay_status.is_some() {
                return Err(WorkerError::reconnect(
                    "Another relay lease status is still awaiting delivery",
                ));
            }
            let now = unix_now()?;
            let signing_bytes = active
                .signing_bytes()
                .map_err(|_| WorkerError::reconnect("Active lease could not be signed"))?;
            let token = self.endpoint_authority.sign(&signing_bytes);
            let (device_id, request_id) = match self.relay_leases.get(&lease_id) {
                Some(entry) => (entry.device_id.clone(), entry.request_id.clone()),
                // The registry lost this lease while the radio was preparing,
                // which means something already revoked it. Say nothing rather
                // than hand out authority the session no longer holds.
                None => return Ok(None),
            };
            let encoded = self
                .relay_lease_status(
                    PluginLeaseStatus::Active,
                    &device_id,
                    &request_id,
                    &token,
                    active.clone(),
                    now,
                )
                .into_iter()
                .next();
            if let Some(encoded) = encoded {
                self.pending_relay_status = Some(PendingRelayStatus::Active {
                    encoded: encoded.clone(),
                    lease_id,
                    claims: active,
                    token,
                });
                return Ok(Some(encoded));
            }
            return Ok(None);
        }
        serde_json::to_string(&decision)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("Claim decision could not be encoded"))
    }

    pub(super) fn encode_rtc(
        &self,
        route: &PeerRoute,
        outbound: OutboundRtcSignal,
    ) -> Result<String, WorkerError> {
        let now = unix_now()?;
        let lease = self.leases.get(&route.lease_jti).ok_or_else(|| {
            WorkerError::reconnect("Outbound RTC signal has no exact signed lease")
        })?;
        if lease.plugin_key_thumbprint != self.endpoint_authority.endpoint_key.thumbprint
            || !self
                .endpoint_authority
                .approved_mobile_keys
                .contains_key(&lease.mobile_key_thumbprint)
        {
            return Err(WorkerError::rebootstrap(
                "Outbound RTC lease keys do not match local endpoint policy",
            ));
        }
        let expires_at = lease.expires_at.min(now.saturating_add(30));
        if expires_at <= now {
            return Err(WorkerError::expired("Outbound RTC lease is expired"));
        }
        let signal = match outbound {
            OutboundRtcSignal::Answer(sdp) => {
                let claims = EndpointBindingClaims {
                    app_id: self.app_id.clone(),
                    plugin_id: self.plugin_id.clone(),
                    device_id: route.device_id.clone(),
                    rtc_session_id: route.binding.rtc_session_id.clone(),
                    endpoint_session_nonce: self.plugin_session_nonce.clone(),
                    lease_jti: route.lease_jti.clone(),
                    endpoint_role: AdmissionRole::Plugin,
                    holder_key_thumbprint: lease.plugin_key_thumbprint.clone(),
                    peer_key_thumbprint: lease.mobile_key_thumbprint.clone(),
                    call_id: route.binding.call_id.clone(),
                    call_epoch: route.binding.call_epoch,
                    owner_epoch: route.binding.owner_epoch,
                    fence: route.binding.fence,
                    sdp_revision: route.sdp_revision,
                    transport_generation: route.transport_generation,
                    dtls_fingerprint: sdp_dtls_fingerprint(&sdp).map_err(|_| {
                        WorkerError::reconnect("Native SDP answer omitted a valid DTLS fingerprint")
                    })?,
                    sdp_sha256: sdp_sha256(&sdp),
                    nonce: format!("rtc_nonce_{}", uuid::Uuid::new_v4().simple()),
                    jti: format!("rtc_jti_{}", uuid::Uuid::new_v4().simple()),
                    issued_at: now,
                    expires_at,
                };
                let signature = self
                    .endpoint_authority
                    .sign(&claims.signing_bytes().map_err(|_| {
                        WorkerError::reconnect("Outbound SDP binding could not be canonicalized")
                    })?);
                RtcSignal::Answer {
                    sdp,
                    binding: SignedEndpointBinding {
                        endpoint_key: self.endpoint_authority.endpoint_key.clone(),
                        claims,
                        signature,
                    },
                }
            }
            OutboundRtcSignal::Ice {
                candidate,
                sdp_mid,
                sdp_m_line_index,
            } => {
                let claims = TrickleCandidateClaims {
                    app_id: self.app_id.clone(),
                    plugin_id: self.plugin_id.clone(),
                    device_id: route.device_id.clone(),
                    rtc_session_id: route.binding.rtc_session_id.clone(),
                    endpoint_session_nonce: self.plugin_session_nonce.clone(),
                    lease_jti: route.lease_jti.clone(),
                    endpoint_role: AdmissionRole::Plugin,
                    holder_key_thumbprint: lease.plugin_key_thumbprint.clone(),
                    peer_key_thumbprint: lease.mobile_key_thumbprint.clone(),
                    call_id: route.binding.call_id.clone(),
                    call_epoch: route.binding.call_epoch,
                    owner_epoch: route.binding.owner_epoch,
                    fence: route.binding.fence,
                    sdp_revision: route.sdp_revision,
                    transport_generation: route.transport_generation,
                    candidate: Some(candidate.clone()),
                    sdp_mid: Some(sdp_mid.clone()),
                    sdp_m_line_index: Some(sdp_m_line_index),
                    end_of_candidates: false,
                    nonce: format!("rtc_nonce_{}", uuid::Uuid::new_v4().simple()),
                    jti: format!("rtc_jti_{}", uuid::Uuid::new_v4().simple()),
                    issued_at: now,
                    expires_at,
                };
                let signature = self
                    .endpoint_authority
                    .sign(&claims.signing_bytes().map_err(|_| {
                        WorkerError::reconnect("Outbound ICE envelope could not be canonicalized")
                    })?);
                RtcSignal::Ice {
                    candidate,
                    sdp_mid: Some(sdp_mid),
                    sdp_m_line_index: Some(sdp_m_line_index),
                    envelope: SignedTrickleCandidateEnvelope {
                        endpoint_key: self.endpoint_authority.endpoint_key.clone(),
                        claims,
                        signature,
                    },
                }
            }
            OutboundRtcSignal::IceComplete => {
                let claims = TrickleCandidateClaims {
                    app_id: self.app_id.clone(),
                    plugin_id: self.plugin_id.clone(),
                    device_id: route.device_id.clone(),
                    rtc_session_id: route.binding.rtc_session_id.clone(),
                    endpoint_session_nonce: self.plugin_session_nonce.clone(),
                    lease_jti: route.lease_jti.clone(),
                    endpoint_role: AdmissionRole::Plugin,
                    holder_key_thumbprint: lease.plugin_key_thumbprint.clone(),
                    peer_key_thumbprint: lease.mobile_key_thumbprint.clone(),
                    call_id: route.binding.call_id.clone(),
                    call_epoch: route.binding.call_epoch,
                    owner_epoch: route.binding.owner_epoch,
                    fence: route.binding.fence,
                    sdp_revision: route.sdp_revision,
                    transport_generation: route.transport_generation,
                    candidate: None,
                    sdp_mid: None,
                    sdp_m_line_index: None,
                    end_of_candidates: true,
                    nonce: format!("rtc_nonce_{}", uuid::Uuid::new_v4().simple()),
                    jti: format!("rtc_jti_{}", uuid::Uuid::new_v4().simple()),
                    issued_at: now,
                    expires_at,
                };
                let signature = self
                    .endpoint_authority
                    .sign(&claims.signing_bytes().map_err(|_| {
                        WorkerError::reconnect("Outbound ICE completion could not be canonicalized")
                    })?);
                RtcSignal::IceComplete {
                    envelope: SignedTrickleCandidateEnvelope {
                        endpoint_key: self.endpoint_authority.endpoint_key.clone(),
                        claims,
                        signature,
                    },
                }
            }
        };
        let frame = PluginRtcSignalFrame {
            kind: "rtc_signal".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            signal_id: format!("signal_{}", uuid::Uuid::new_v4().simple()),
            plugin_id: self.plugin_id.clone(),
            device_id: route.device_id.clone(),
            lease_jti: route.lease_jti.clone(),
            rtc_session_id: route.binding.rtc_session_id.clone(),
            sdp_revision: route.sdp_revision,
            transport_generation: route.transport_generation,
            call_id: route.binding.call_id.clone(),
            call_epoch: route.binding.call_epoch,
            owner_epoch: route.binding.owner_epoch,
            fence: route.binding.fence,
            signal,
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Outbound RTC signal is invalid"))?;
        serde_json::to_string(&frame)
            .map_err(|_| WorkerError::reconnect("Outbound RTC signal could not be encoded"))
    }

    pub(super) fn fail_peer(
        &mut self,
        rtc_session_id: &str,
        reason: &str,
        media: &RemoteMediaHandle,
    ) -> Result<Option<String>, WorkerError> {
        let Some(route) = self.peers.remove(rtc_session_id) else {
            return Ok(None);
        };
        eprintln!(
            "[aokie-plugin][takeover] stage=peer_failed call={} mode={:?} owner_epoch={} fence={} rtc={} reason={}",
            route.binding.call_id,
            route.binding.mode,
            route.binding.owner_epoch,
            route.binding.fence,
            rtc_session_id,
            reason
        );
        let lease_id = route.binding.lease_id.clone().ok_or_else(|| {
            WorkerError::reconnect("Failed media route omitted stable lease identity")
        })?;
        if matches!(
            route.binding.mode,
            MediaMode::PreparedConsult
                | MediaMode::Consult
                | MediaMode::PreparedTalk
                | MediaMode::Talk
        ) {
            let _ = media.revoke(&route.binding, reason);
        }
        let _ = media.close_peer(rtc_session_id, reason);

        // Refuse any late active rebind immediately; waiting for the gateway's
        // revocation echo would leave a window in which a failed prepared Talk
        // route could be reopened with otherwise valid, still-live claims.
        self.leases.retain(|_, claims| claims.lease_id != lease_id);
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.provisional.lease_id == lease_id)
        {
            self.prepared = None;
        }
        // Same reasoning as the refusal above, for the relay's own registry: a
        // failed route's token must stop being recognised at once, so a
        // heartbeat cannot re-extend a lease whose media has already gone.
        self.retire_relay_lease(&lease_id);
        let frame = PluginLeaseRevokeFrame {
            kind: "plugin_lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: route.device_id,
            lease_id,
            lease_jti: route.lease_jti,
            call_id: route.binding.call_id,
            call_epoch: route.binding.call_epoch,
            fence: route.binding.fence,
            reason: reason.into(),
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Media revocation frame is invalid"))?;
        serde_json::to_string(&frame)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("Media revocation could not be encoded"))
    }
}
