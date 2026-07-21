//! `GatewaySession` relay control frames: RTC signals, heartbeats, revokes, mute,
//! assistance answers and end-caller challenges.

#[allow(unused_imports)]
use super::*;

impl GatewaySession {
    /// A device signalling SDP or ICE for a lease this plugin minted.
    ///
    /// The mobile dialect differs from the plugin's by exactly one field, the
    /// bearer `leaseToken`. Checking that token here and then handing the
    /// remaining fields to the unchanged handler is precisely the translation
    /// the gateway used to perform — every endpoint-signature, roster,
    /// session-nonce and replay check downstream stays untouched.
    pub(super) fn relay_rtc_signal(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileRtcSignalFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err()
            || frame.app_id != self.app_id
            || frame.plugin_id != self.plugin_id
        {
            return Vec::new();
        }
        let device_id = frame.device_id.clone();
        // RTC signals carry no requestId; the signal id is what correlates a
        // refusal with what provoked it.
        let request_id = frame.signal_id.clone();
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        let recognised_before_narrowing = self
            .relay_leases
            .values()
            .find(|lease| {
                lease.device_id == device_id
                    && lease.current_jti == frame.lease_jti
                    && tokens_match(&lease.token, &frame.lease_token)
            })
            .cloned();
        let retired_before_narrowing = self.retired_prepared_rtc_for_frame(&frame);
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        let replay_key = format!("rtc\u{1f}{device_id}\u{1f}{}", frame.signal_id);
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &request_id,
                    "duplicate_signal",
                    "that signal id was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::RtcAccepted { mode }
                    if self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) =>
                {
                    Vec::new()
                }
                RelayReplayResult::RtcAccepted { .. } => self.relay_reject(
                    &device_id,
                    &request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                ),
                RelayReplayResult::RetiredPreparedRtcDropped {
                    mode, expires_at, ..
                } if expires_at > Instant::now()
                    && self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) =>
                {
                    Vec::new()
                }
                RelayReplayResult::RetiredPreparedRtcDropped { expires_at, .. }
                    if expires_at <= Instant::now() =>
                {
                    self.relay_replays.remove(&replay_key);
                    self.relay_reject(
                        &device_id,
                        &request_id,
                        "lease_unknown",
                        "that retired RTC generation has expired",
                    )
                }
                RelayReplayResult::RetiredPreparedRtcDropped { .. } => self.relay_reject(
                    &device_id,
                    &request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                ),
                RelayReplayResult::TerminalRtcFailure {
                    revocation,
                    rejection,
                } => vec![revocation, rejection],
                RelayReplayResult::Rejected { encoded } => vec![encoded],
                _ => self.relay_reject(
                    &device_id,
                    &request_id,
                    "duplicate_signal",
                    "that signal id belongs to another operation",
                ),
            };
        }
        if self.relay_rtc_over_budget(&device_id) {
            return self.relay_reject(&device_id, &request_id, "rate_limited", "too many requests");
        }
        if !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "rate_limited",
                "the replay ledger is full",
            );
        }
        if let Some(retired) = retired_before_narrowing {
            if !self.relay_mode_is_authorized(&device_id, authenticated_grants, retired.claims.mode)
            {
                return self.relay_reject(
                    &device_id,
                    &request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this media mode",
                );
            }
            if !self.authenticate_retired_prepared_rtc(&frame, &retired) {
                return self.relay_recorded_rejection(
                    &replay_key,
                    &fingerprint,
                    &device_id,
                    &request_id,
                    "lease_unknown",
                    "that retired RTC signal did not match its endpoint proof",
                );
            }
            self.relay_record_replay(
                replay_key,
                fingerprint,
                device_id.clone(),
                RelayReplayResult::RetiredPreparedRtcDropped {
                    lease_id: retired.claims.lease_id.clone(),
                    mode: retired.claims.mode,
                    expires_at: retired.expires_at,
                },
            );
            eprintln!(
                "[aokie-plugin][takeover] stage=retired_prepared_rtc_dropped device={} call={} lease={} rtc={} sdp={} generation={} detail=A late signal for the superseded receive-only generation was replay-fenced and ignored",
                sanitize_gateway_code(&device_id),
                retired.claims.call_id,
                retired.claims.lease_id,
                retired.claims.rtc_session_id,
                retired.sdp_revision,
                retired.transport_generation
            );
            return Vec::new();
        }
        let Some(recognised) = recognised_before_narrowing else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "that lease was not issued by this plugin",
            );
        };
        if !self.relay_mode_is_authorized(&device_id, authenticated_grants, recognised.mode) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        let routed = PluginRtcSignalFrame {
            kind: frame.kind,
            schema_version: frame.schema_version,
            app_id: frame.app_id,
            signal_id: frame.signal_id,
            plugin_id: frame.plugin_id,
            device_id: frame.device_id,
            lease_jti: frame.lease_jti,
            rtc_session_id: frame.rtc_session_id,
            sdp_revision: frame.sdp_revision,
            transport_generation: frame.transport_generation,
            call_id: frame.call_id,
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            fence: frame.fence,
            signal: frame.signal,
        };
        if routed.validate_routed_mobile().is_err() {
            return self.relay_recorded_terminal_rtc_failure(
                &recognised,
                &routed.rtc_session_id,
                "terminal_rtc_contract_failure",
                &replay_key,
                &fingerprint,
                &device_id,
                &request_id,
                "that RTC signal failed contract validation",
                media,
            );
        }
        let rtc_session_id = routed.rtc_session_id.clone();
        match self.handle_rtc_signal(routed, media, radio) {
            Ok(()) => {
                self.relay_record_replay(
                    replay_key,
                    fingerprint,
                    device_id,
                    RelayReplayResult::RtcAccepted {
                        mode: recognised.mode,
                    },
                );
                Vec::new()
            }
            Err(error) => {
                // Once an authenticated signal reaches the native RTC path,
                // any failure is terminal for this exact peer/lease. Keeping
                // it heartbeat-renewable would leave a prepared or active
                // authority with no usable media path. ICE-order retries are
                // not currently classified as safe; candidates are accepted
                // only after the exact peer exists.
                self.relay_recorded_terminal_rtc_failure(
                    &recognised,
                    &rtc_session_id,
                    "terminal_rtc_failure",
                    &replay_key,
                    &fingerprint,
                    &device_id,
                    &request_id,
                    &error.message,
                    media,
                )
            }
        }
    }

    /// Extend an active lease.
    ///
    /// A PREPARED lease is deliberately not renewable. Its short life is the
    /// bound on how long a caller can sit in soft hold waiting for a handover
    /// that is not arriving, and a renewable prepare would let a stuck one be
    /// held open indefinitely.
    pub(super) fn relay_lease_heartbeat(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<LeaseHeartbeatFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let expired_exact = unix_now().ok().and_then(|now| {
            self.relay_leases
                .values()
                .find(|entry| {
                    tokens_match(&entry.token, &frame.lease_token)
                        && self
                            .leases
                            .get(&entry.current_jti)
                            .is_none_or(|claims| claims.expires_at <= now)
                })
                .cloned()
        });
        if let Ok(now) = unix_now() {
            self.expire_relay_leases(now, media);
        }
        self.expire_unbound_active_rebind(Instant::now(), media);
        self.reconcile_relay_media_authority(media);
        if let Some(expired) = expired_exact {
            if !self.relay_sender_owns_device(from_party, authenticated_subject, &expired.device_id)
            {
                return self.relay_reject(
                    &expired.device_id,
                    &frame.request_id,
                    "device_unknown",
                    "this device has not introduced itself on this relay session",
                );
            }
            return self.relay_reject(
                &expired.device_id,
                &frame.request_id,
                "lease_expired",
                "that lease has expired and cannot be renewed",
            );
        }
        let replay_key = format!("heartbeat\u{1f}{}", frame.idempotency_key);
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
                RelayReplayResult::HeartbeatStatus { lease_id } => {
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
                RelayReplayResult::HeartbeatStatus { lease_id } => {
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
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        let Some(lease_id) = self.relay_lease_id_for_token(&frame.lease_token) else {
            // The outer relay subject still tells us which verified device to
            // address after a failed route's token has been retired. Return a
            // typed terminal answer so the client cannot mistake silence for
            // a renewable lease and keep heartbeating forever.
            let Some(device_id) = authenticated_subject.filter(|device_id| {
                self.relay_sender_owns_device(from_party, authenticated_subject, device_id)
            }) else {
                return Vec::new();
            };
            return self.relay_reject(
                device_id,
                &frame.request_id,
                "lease_unknown",
                "that lease is no longer current",
            );
        };
        let Some(entry) = self.relay_leases.get(&lease_id).cloned() else {
            return Vec::new();
        };
        let device_id = entry.device_id.clone();
        let request_id = frame.request_id.clone();
        let current_jti = entry.current_jti.clone();
        let phase = entry.phase;
        let mode = entry.mode;
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "device_unknown",
                "this device has not introduced itself on this relay session",
            );
        }
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        if !self.relay_mode_is_authorized(&device_id, authenticated_grants, mode) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "grant_required",
                "the authenticated admission does not grant this media mode",
            );
        }
        if self.relay_over_budget(&device_id) {
            return self.relay_reject(&device_id, &request_id, "rate_limited", "too many requests");
        }
        if !self.relay_replay_has_room(&replay_key) || self.pending_relay_status.is_some() {
            return self.relay_reject(
                &device_id,
                &request_id,
                "rate_limited",
                "another relay transition is still being delivered",
            );
        }
        if matches!(phase, LeasePhase::Prepared) {
            return self.relay_reject(
                &device_id,
                &request_id,
                "phase_not_renewable",
                "a prepared claim must complete rather than be extended",
            );
        }
        // The session's own lease book is the authority on whether this lease
        // is still alive: a peer that failed, or was revoked, has already been
        // purged from it, and renewing on the strength of the relay registry
        // alone would resurrect authority that was deliberately withdrawn.
        let Some(live) = self.leases.get(&current_jti).cloned() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "that lease is no longer live on this session",
            );
        };
        let Ok(now) = unix_now() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "clock unavailable",
            );
        };
        if live.expires_at <= now {
            let expired = LeaseRevokedNotice {
                kind: "lease_revoked".into(),
                schema_version: SCHEMA_VERSION,
                app_id: self.app_id.clone(),
                device_id: device_id.clone(),
                lease_id: live.lease_id.clone(),
                lease_jti: live.jti.clone(),
                call_id: live.call_id.clone(),
                call_epoch: live.call_epoch,
                fence: live.fence,
                reason: "lease_expired".into(),
            };
            let _ = self.handle_lease_revoked(expired, media);
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_expired",
                "that lease expired before the heartbeat arrived",
            );
        }
        let mut renewed = live;
        renewed.jti = format!("leasejti_{}", uuid::Uuid::new_v4().simple());
        renewed.expires_at = now.saturating_add(RELAY_ACTIVE_LEASE_TTL);
        if renewed.validate(now).is_err() {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "the renewed lease could not be formed safely",
            );
        }
        let Ok(signing_bytes) = renewed.signing_bytes() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "the renewed lease could not be signed",
            );
        };
        let token = self.endpoint_authority.sign(&signing_bytes);
        let notice = LeaseNotice {
            kind: "lease_renewed".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.clone(),
            request_id: Some(request_id.clone()),
            lease_token: token.clone(),
            lease: renewed.clone(),
            accepted_transfer_request_id: None,
        };
        let frames = self.relay_lease_status(
            PluginLeaseStatus::Renewed,
            &device_id,
            &request_id,
            &token,
            renewed.clone(),
            now,
        );
        let Some(encoded_status) = frames.first().cloned() else {
            return self.relay_reject(
                &device_id,
                &request_id,
                "lease_unknown",
                "the renewed lease status could not be encoded safely",
            );
        };
        self.relay_record_replay(
            replay_key.clone(),
            fingerprint,
            device_id,
            RelayReplayResult::HeartbeatStatus {
                lease_id: lease_id.clone(),
            },
        );
        self.pending_relay_status = Some(PendingRelayStatus::Renewal {
            encoded: encoded_status.clone(),
            notice,
            lease_id,
            replay_key,
        });
        let _ = (media, radio);
        vec![encoded_status]
    }

    /// A device handing its lease back.
    ///
    /// Strictly de-escalating, and the shortest path from a live takeover back
    /// to the AI answering. Nothing here can extend authority.
    pub(super) fn relay_lease_revoke(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        media: &RemoteMediaHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<LeaseRevokeFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let Some(lease_id) = self.relay_lease_id_for_token(&frame.lease_token) else {
            return Vec::new();
        };
        let Some(entry) = self.relay_leases.get(&lease_id) else {
            return Vec::new();
        };
        let device_id = entry.device_id.clone();
        let current_jti = entry.current_jti.clone();
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return Vec::new();
        }
        let Some(lease) = self.leases.get(&current_jti).cloned() else {
            // Already gone. Forget our copy and say nothing: the device is
            // asking for a state it is already in.
            self.retire_relay_lease(&lease_id);
            return Vec::new();
        };
        let notice = LeaseRevokedNotice {
            kind: "lease_revoked".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id,
            lease_id: lease.lease_id.clone(),
            lease_jti: current_jti,
            call_id: lease.call_id.clone(),
            call_epoch: lease.call_epoch,
            fence: lease.fence,
            reason: frame.reason.clone(),
        };
        let result = self.handle_lease_revoked(notice, media);
        self.retire_relay_lease(&lease_id);
        match result {
            Ok(()) => {
                // The next authoritative Aokie-active snapshot is the mobile's
                // strong completion receipt. Force it on the next loop turn;
                // no separate optimistic revoke-ack frame is needed.
                self.last_snapshot_fingerprint = None;
                self.last_snapshot_sent = None;
                self.next_snapshot_poll = Instant::now();
            }
            Err(error) => {
                eprintln!(
                    "[aokie-plugin][companion] stage=relay_revoke_failed detail={}",
                    sanitize_status_message(&error.message)
                );
            }
        }
        Vec::new()
    }

    /// Apply one exact active-lease microphone authority change. The payload
    /// carries no deviceId, so relay-authenticated subject metadata supplies
    /// identity; every call/media/switchboard fence is then rechecked against
    /// local truth while the Desktop PCM gate is changed.
    pub(super) fn relay_microphone_mute(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileMicrophoneMuteFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let Some(device_id) = authenticated_subject.map(str::to_owned) else {
            return Vec::new();
        };
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return Vec::new();
        }
        let Some(relay) = self
            .relay_leases
            .values()
            .find(|lease| {
                lease.device_id == device_id
                    && lease.phase == LeasePhase::Active
                    && relay_status_has_active_authority(lease.status)
                    && tokens_match(&lease.token, &frame.lease_token)
            })
            .cloned()
        else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "lease_unknown",
                "that active media lease is no longer current",
            );
        };
        let required = match relay.mode {
            LeaseMode::Takeover => vec![Grant::StateRead, Grant::RtcSignal, Grant::Takeover],
            LeaseMode::Consult => vec![Grant::StateRead, Grant::RtcSignal, Grant::Consult],
            LeaseMode::Monitor => {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "mute_not_available",
                    "listen-only monitor authority has no microphone route",
                )
            }
        };
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        if !self.relay_has_exact_grants(&device_id, authenticated_grants, &required) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "grant_required",
                "the authenticated admission no longer grants this microphone route",
            );
        }
        let replay_key = format!(
            "microphone_mute\u{1f}{device_id}\u{1f}{}",
            frame.idempotency_key
        );
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::DirectResponse {
                    encoded,
                    required_grants,
                } if self.relay_has_exact_grants(
                    &device_id,
                    authenticated_grants,
                    &required_grants,
                ) =>
                {
                    vec![encoded]
                }
                RelayReplayResult::DirectResponse { .. } => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "grant_required",
                    "the authenticated admission no longer grants this microphone route",
                ),
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        if self.relay_over_budget(&device_id) || !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "rate_limited",
                "the microphone-control operation budget is full",
            );
        }
        let Ok(now) = unix_now() else {
            return Vec::new();
        };
        let Some(claims) = self.leases.get(&relay.current_jti).cloned() else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "lease_unknown",
                "that active media lease is no longer live",
            );
        };
        let remote = media.snapshot();
        let service_matches = matches!(
            (claims.mode, remote.service_mode),
            (LeaseMode::Takeover, LocalServiceMode::HumanActive)
                | (LeaseMode::Consult, LocalServiceMode::ConsultActive)
        );
        if claims.expires_at <= now
            || claims.jti != relay.current_jti
            || claims.lease_id != relay.lease_id
            || claims.device_id != device_id
            || claims.mode != relay.mode
            || claims.phase != LeasePhase::Active
            || claims.rtc_session_id != frame.rtc_session_id
            || claims.call_id != frame.call_id
            || claims.call_epoch != frame.call_epoch
            || claims.owner_epoch != frame.owner_epoch
            || claims.fence != frame.fence
            || radio.current_call_id().as_deref() != Some(frame.call_id.as_str())
            || !radio.is_call_active()
            || radio.switch_in_flight()
            || radio.switchboard_revision() != frame.switchboard_revision
            || remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.call_epoch
            || remote.owner_epoch != frame.owner_epoch
            || remote.remote_revision != frame.remote_revision
            || remote.talk_device_id.as_deref() != Some(device_id.as_str())
            || remote.talk_lease_id.as_deref() != Some(claims.lease_id.as_str())
            || remote.talk_fence != frame.fence
            || !service_matches
        {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "stale_state",
                "the microphone request crossed its active call or switchboard fence",
            );
        }
        let binding = binding_for_claims(&claims);
        let Ok(remote_revision) =
            media.set_microphone_muted(&binding, frame.remote_revision, frame.muted)
        else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "stale_state",
                "the active microphone route changed before the request was applied",
            );
        };
        let response = PluginMicrophoneMuteStatusFrame {
            kind: "microphone_mute_status".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.clone(),
            request_id: frame.request_id.clone(),
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
        if response.validate().is_err() {
            return Vec::new();
        }
        let Ok(response) = serde_json::to_string(&response) else {
            return Vec::new();
        };
        self.relay_record_replay(
            replay_key,
            fingerprint,
            device_id,
            RelayReplayResult::DirectResponse {
                encoded: response.clone(),
                required_grants: required,
            },
        );
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.next_snapshot_poll = Instant::now();
        vec![response]
    }

    /// Translate one mobile-dialect assistance answer on the dumb relay.
    /// Identity comes from authenticated relay metadata, never from the
    /// payload (the mobile frame deliberately carries no deviceId).
    pub(super) fn relay_assistance_answer(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileAssistanceAnswerFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let Some(device_id) = authenticated_subject.map(str::to_owned) else {
            return Vec::new();
        };
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return Vec::new();
        }
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        let required = [Grant::StateRead, Grant::AssistanceRespond];
        if !self.relay_has_exact_grants(&device_id, authenticated_grants, &required) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "grant_required",
                "the authenticated admission cannot answer assistance requests",
            );
        }
        let replay_key = format!("assistance\u{1f}{device_id}\u{1f}{}", frame.idempotency_key);
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::DirectResponse {
                    encoded,
                    required_grants,
                } if self.relay_has_exact_grants(
                    &device_id,
                    authenticated_grants,
                    &required_grants,
                ) =>
                {
                    vec![encoded]
                }
                RelayReplayResult::DirectResponse { .. } => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "grant_required",
                    "the authenticated admission no longer permits this assistance answer",
                ),
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        if self.relay_over_budget(&device_id) || !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "rate_limited",
                "the assistance operation budget is full",
            );
        }
        let Some(request) = self.assistance.pending_frame(&self.app_id) else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "stale_assistance",
                "the assistance request is no longer pending",
            );
        };
        let remote = media.snapshot();
        if request.request_id != frame.request_id
            || request.call_id != frame.call_id
            || request.call_epoch != frame.call_epoch
            || request.owner_epoch != frame.owner_epoch
            || request.switchboard_revision != frame.switchboard_revision
            || request.remote_revision != frame.remote_revision
            || remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.call_epoch
            || remote.owner_epoch != frame.owner_epoch
            || remote.remote_revision != frame.remote_revision
            || radio.switchboard_revision() != frame.switchboard_revision
            || !remote.consent.enabled
            || !remote.consent.acknowledged
            || !remote.consent.assistance_enabled
        {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "stale_state",
                "the assistance answer does not match current call authority",
            );
        }
        let routed = PluginAssistanceAnswerFrame {
            kind: "assistance_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.clone(),
            request_id: frame.request_id.clone(),
            answer_id: frame.answer_id.clone(),
            call_id: frame.call_id,
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            response_action: frame.response_action,
            answer: frame.answer,
        };
        if routed.validate().is_err() || self.assistance.accept(routed).is_err() {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "already_answered",
                "the assistance request already consumed its one answer",
            );
        }
        let response = RelayAssistanceAnswerAcceptedFrame {
            kind: "assistance_answer_accepted",
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            request_id: frame.request_id.clone(),
            answer_id: frame.answer_id,
            accepted: true,
        };
        let Ok(response) = serde_json::to_string(&response) else {
            return Vec::new();
        };
        self.relay_record_replay(
            replay_key,
            fingerprint,
            device_id,
            RelayReplayResult::DirectResponse {
                encoded: response.clone(),
                required_grants: required.to_vec(),
            },
        );
        vec![response]
    }

    pub(super) fn relay_end_caller_challenge_request(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileEndCallerChallengeRequestFrame>(encoded)
        else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let Some(device_id) = authenticated_subject.map(str::to_owned) else {
            return Vec::new();
        };
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return Vec::new();
        }
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        let required = [Grant::StateRead, Grant::Takeover, Grant::EndCaller];
        if !self.relay_has_exact_grants(&device_id, authenticated_grants, &required) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "end_caller_denied",
                "the authenticated admission cannot end the caller call",
            );
        }
        let replay_key = format!(
            "end_challenge\u{1f}{device_id}\u{1f}{}",
            frame.idempotency_key
        );
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::DirectResponse {
                    encoded,
                    required_grants,
                } if self.relay_has_exact_grants(
                    &device_id,
                    authenticated_grants,
                    &required_grants,
                ) =>
                {
                    vec![encoded]
                }
                RelayReplayResult::DirectResponse { .. } => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "end_caller_denied",
                    "the authenticated admission no longer permits caller ending",
                ),
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        if self.relay_over_budget(&device_id) || !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "rate_limited",
                "the caller-ending operation budget is full",
            );
        }
        let Ok(now) = unix_now() else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "stale_lease",
                "the local clock is unavailable",
            );
        };
        let Some(claims) = self.relay_validate_active_talk_owner(
            &device_id,
            &frame.lease_token,
            &frame.call_id,
            frame.call_epoch,
            frame.owner_epoch,
            frame.switchboard_revision,
            frame.remote_revision,
            frame.fence,
            now,
            media,
            radio,
        ) else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "not_active_takeover_owner",
                "only the exact active takeover owner may end the caller call",
            );
        };
        if !self.pending_end_caller.is_empty() {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "end_caller_pending",
                "another caller-ending operation is already pending",
            );
        }
        self.prune_relay_end_caller_challenges(now);
        if self.relay_end_caller_challenges.len() >= MAX_RELAY_END_CALLER_CHALLENGES {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "rate_limited",
                "the caller-ending challenge ledger is full",
            );
        }
        self.relay_end_caller_challenges
            .retain(|_, challenge| challenge.frame.device_id != device_id);
        let confirmation_id = format!("end_confirm_{}", uuid::Uuid::new_v4().simple());
        let challenge = EndCallerChallengeFrame {
            kind: "end_caller_challenge".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            request_id: frame.request_id.clone(),
            confirmation_id: confirmation_id.clone(),
            nonce: format!("nonce_{}", uuid::Uuid::new_v4().simple()),
            device_id: device_id.clone(),
            call_id: claims.call_id.clone(),
            call_epoch: claims.call_epoch,
            owner_epoch: claims.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            lease_id: claims.lease_id.clone(),
            fence: claims.fence,
            expires_at: now.saturating_add(RELAY_END_CALLER_CONFIRM_TTL),
        };
        if challenge.validate(now).is_err() {
            return Vec::new();
        }
        let Ok(response) = serde_json::to_string(&challenge) else {
            return Vec::new();
        };
        self.relay_end_caller_challenges.insert(
            confirmation_id,
            RelayEndCallerChallenge {
                frame: challenge,
                lease_jti: claims.jti,
            },
        );
        self.relay_record_replay(
            replay_key,
            fingerprint,
            device_id,
            RelayReplayResult::DirectResponse {
                encoded: response.clone(),
                required_grants: required.to_vec(),
            },
        );
        vec![response]
    }

    pub(super) fn relay_end_caller_confirm(
        &mut self,
        encoded: &str,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<MobileEndCallerConfirmFrame>(encoded) else {
            return Vec::new();
        };
        if frame.validate().is_err() || frame.app_id != self.app_id {
            return Vec::new();
        }
        let Some(device_id) = authenticated_subject.map(str::to_owned) else {
            return Vec::new();
        };
        if !self.relay_sender_owns_device(from_party, authenticated_subject, &device_id) {
            return Vec::new();
        }
        self.relay_reconcile_frame_grants(&device_id, authenticated_grants, media);
        let required = [Grant::StateRead, Grant::Takeover, Grant::EndCaller];
        if !self.relay_has_exact_grants(&device_id, authenticated_grants, &required) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "end_caller_denied",
                "the authenticated admission cannot end the caller call",
            );
        }
        let replay_key = format!(
            "end_confirm\u{1f}{device_id}\u{1f}{}",
            frame.idempotency_key
        );
        let Ok(fingerprint) = serde_json::to_string(&frame) else {
            return Vec::new();
        };
        if let Some(replay) = self.relay_replay(&replay_key) {
            if replay.fingerprint != fingerprint {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key was already used with different content",
                );
            }
            return match replay.result {
                RelayReplayResult::DirectResponse {
                    encoded,
                    required_grants,
                } if self.relay_has_exact_grants(
                    &device_id,
                    authenticated_grants,
                    &required_grants,
                ) =>
                {
                    vec![encoded]
                }
                RelayReplayResult::DirectResponse { .. } => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "end_caller_denied",
                    "the authenticated admission no longer permits caller ending",
                ),
                _ => self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "duplicate_request",
                    "that idempotency key belongs to another operation",
                ),
            };
        }
        if self.relay_over_budget(&device_id) || !self.relay_replay_has_room(&replay_key) {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "rate_limited",
                "the caller-ending operation budget is full",
            );
        }
        let Ok(now) = unix_now() else {
            return Vec::new();
        };
        self.prune_relay_end_caller_challenges(now);
        if self
            .used_relay_end_caller_confirmations
            .contains(&frame.confirmation_id)
        {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "confirmation_used",
                "that caller-ending confirmation was already consumed",
            );
        }
        let Some(challenge) = self
            .relay_end_caller_challenges
            .get(&frame.confirmation_id)
            .cloned()
        else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "confirmation_stale",
                "that caller-ending confirmation is no longer current",
            );
        };
        let exact = challenge.frame.nonce == frame.nonce
            && challenge.frame.device_id == device_id
            && challenge.frame.call_id == frame.call_id
            && challenge.frame.call_epoch == frame.call_epoch
            && challenge.frame.owner_epoch == frame.owner_epoch
            && challenge.frame.switchboard_revision == frame.switchboard_revision
            && challenge.frame.remote_revision == frame.remote_revision
            && challenge.frame.fence == frame.fence;
        if !exact {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "confirmation_denied",
                "the confirmation nonce or exact caller fence does not match",
            );
        }
        let Some(claims) = self.relay_validate_active_talk_owner(
            &device_id,
            &frame.lease_token,
            &frame.call_id,
            frame.call_epoch,
            frame.owner_epoch,
            frame.switchboard_revision,
            frame.remote_revision,
            frame.fence,
            now,
            media,
            radio,
        ) else {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "not_active_takeover_owner",
                "the exact takeover lease changed before confirmation",
            );
        };
        if claims.jti != challenge.lease_jti || claims.lease_id != challenge.frame.lease_id {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "stale_lease",
                "the exact takeover lease changed before confirmation",
            );
        }
        if !self.pending_end_caller.is_empty() {
            return self.relay_reject(
                &device_id,
                &frame.request_id,
                "end_caller_pending",
                "another caller-ending operation is already pending",
            );
        }
        let execute = PluginEndCallerExecuteFrame {
            kind: "end_caller_execute".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            operation_id: format!("end_op_{}", uuid::Uuid::new_v4().simple()),
            confirmation_id: frame.confirmation_id.clone(),
            device_id: device_id.clone(),
            call_id: frame.call_id,
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            lease_id: claims.lease_id,
            lease_jti: claims.jti,
            fence: frame.fence,
        };
        if execute.validate().is_err() {
            return Vec::new();
        }
        self.relay_end_caller_challenges
            .remove(&frame.confirmation_id);
        self.remember_used_relay_end_caller_confirmation(frame.confirmation_id.clone());
        match self.handle_end_caller_execute(execute.clone(), radio) {
            Ok(frames) if frames.is_empty() => {}
            Ok(_) | Err(_) => {
                return self.relay_reject(
                    &device_id,
                    &frame.request_id,
                    "radio_unavailable",
                    "the physical caller-ending command could not be queued",
                );
            }
        }
        let response = RelayEndCallerSubmittedFrame {
            kind: "end_caller_submitted",
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            request_id: frame.request_id.clone(),
            operation_id: execute.operation_id,
            confirmation_id: execute.confirmation_id,
            accepted: true,
        };
        let Ok(response) = serde_json::to_string(&response) else {
            return Vec::new();
        };
        self.relay_record_replay(
            replay_key,
            fingerprint,
            device_id,
            RelayReplayResult::DirectResponse {
                encoded: response.clone(),
                required_grants: required.to_vec(),
            },
        );
        vec![response]
    }

}
