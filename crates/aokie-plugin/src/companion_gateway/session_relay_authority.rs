//! `GatewaySession` relay authority bookkeeping: grants, revocation, expiry, budgets, replays.

#[allow(unused_imports)]
use super::*;

impl GatewaySession {
    pub(super) fn relay_has_exact_grants(
        &self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        required: &[Grant],
    ) -> bool {
        self.relay_peers.get(device_id).is_some_and(|peer| {
            required
                .iter()
                .all(|grant| peer.grants.contains(grant) && authenticated_grants.contains(grant))
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn relay_validate_active_talk_owner(
        &self,
        device_id: &str,
        lease_token: &str,
        call_id: &str,
        call_epoch: u64,
        owner_epoch: u64,
        switchboard_revision: u64,
        remote_revision: u64,
        fence: u64,
        now: u64,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Option<LeaseClaims> {
        let relay = self.relay_leases.values().find(|lease| {
            lease.device_id == device_id
                && lease.mode == LeaseMode::Takeover
                && lease.phase == LeasePhase::Active
                && relay_status_has_active_authority(lease.status)
                && tokens_match(&lease.token, lease_token)
        })?;
        let claims = self.leases.get(&relay.current_jti)?.clone();
        if claims.expires_at <= now
            || claims.jti != relay.current_jti
            || claims.lease_id != relay.lease_id
            || claims.device_id != device_id
            || claims.mode != LeaseMode::Takeover
            || claims.phase != LeasePhase::Active
            || claims.call_id != call_id
            || claims.call_epoch != call_epoch
            || claims.owner_epoch != owner_epoch
            || claims.fence != fence
            || radio.current_call_id().as_deref() != Some(call_id)
            || !radio.is_call_active()
            || radio.switchboard_revision() != switchboard_revision
            || radio.switch_in_flight()
        {
            return None;
        }
        let remote = media.snapshot();
        (remote.call_id.as_deref() == Some(call_id)
            && remote.call_epoch == call_epoch
            && remote.owner_epoch == owner_epoch
            && remote.remote_revision == remote_revision
            && remote.service_mode == LocalServiceMode::HumanActive
            && remote.talk_device_id.as_deref() == Some(device_id)
            && remote.talk_lease_id.as_deref() == Some(claims.lease_id.as_str())
            && remote.talk_fence == fence
            && remote.radio_reserved
            && remote.consent.is_current()
            && remote.consent.takeover_enabled)
            .then_some(claims)
    }

    pub(super) fn prune_relay_end_caller_challenges(&mut self, now: u64) {
        self.relay_end_caller_challenges
            .retain(|_, challenge| challenge.frame.expires_at > now);
    }

    pub(super) fn remember_used_relay_end_caller_confirmation(&mut self, confirmation_id: String) {
        if self
            .used_relay_end_caller_confirmations
            .insert(confirmation_id.clone())
        {
            self.used_relay_end_caller_order.push_back(confirmation_id);
        }
        while self.used_relay_end_caller_order.len() > MAX_USED_RELAY_END_CALLER_CONFIRMATIONS {
            if let Some(oldest) = self.used_relay_end_caller_order.pop_front() {
                self.used_relay_end_caller_confirmations.remove(&oldest);
            }
        }
    }

    /// The lease this token belongs to, compared in constant time.
    pub(super) fn relay_lease_id_for_token(&self, presented: &str) -> Option<String> {
        self.relay_leases
            .values()
            .find(|lease| tokens_match(&lease.token, presented))
            .map(|lease| lease.lease_id.clone())
    }

    /// Retire one offer whose acceptance transaction could not be delivered or
    /// installed. The identity stays tombstoned through its rewritten replay;
    /// the next snapshot may only publish a newly signed offer id/JTI.
    pub(super) fn retire_failed_relay_offer(&mut self, offer: MintedOffer) {
        let offer_id = offer.claims.offer_id.clone();
        let opportunity_id = offer.claims.opportunity_id.clone();
        let device_id = offer.claims.target_device_id.clone();
        self.relay_offers.remove(&offer_id);
        self.relay_redeeming_transfer_offers.remove(&offer_id);
        if self
            .relay_offer_winners
            .get(&opportunity_id)
            .is_some_and(|winner| winner == &offer_id)
        {
            self.relay_offer_winners.remove(&opportunity_id);
        }
        if let Some(request_id) = offer.claims.accepted_transfer_request_id.as_deref() {
            let fence = crate::assistance::AssistanceCallFence {
                call_id: offer.claims.call_id.clone(),
                call_epoch: offer.claims.call_epoch,
                owner_epoch: offer.claims.owner_epoch,
                switchboard_revision: offer.claims.switchboard_revision,
                remote_revision: offer.claims.remote_revision,
            };
            let _ = self
                .assistance
                .release_transfer_acceptance(request_id, &fence, &device_id);
        }

        // A mobile keeps the exact encoded answer for at-least-once replay.
        // Replaying that answer after rollback must never return the old
        // accepted ACK and advance it toward a spent lease transaction.
        let replay_keys = self
            .relay_replays
            .iter()
            .filter_map(|(key, replay)| match &replay.result {
                RelayReplayResult::OfferAccepted {
                    offer_id: accepted_offer_id,
                    ..
                } if accepted_offer_id == &offer_id => Some(key.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for key in replay_keys {
            let Some((request_id, replay_device)) =
                self.relay_replays.get(&key).and_then(|replay| {
                    serde_json::from_str::<MobileOfferAnswerFrame>(&replay.fingerprint)
                        .ok()
                        .map(|frame| (frame.request_id, replay.device_id.clone()))
                })
            else {
                self.relay_replays.remove(&key);
                continue;
            };
            let Some(encoded) = self
                .relay_reject(
                    &replay_device,
                    &request_id,
                    "offer_retired",
                    "that offer transaction was retired before media authority was delivered",
                )
                .into_iter()
                .next()
            else {
                self.relay_replays.remove(&key);
                continue;
            };
            if let Some(replay) = self.relay_replays.get_mut(&key) {
                replay.result = RelayReplayResult::Rejected { encoded };
            }
        }
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.next_snapshot_poll = Instant::now();
    }

    /// Reconcile a verified re-hello with authority issued to its prior
    /// session/admission. Session rotation revokes everything for the device;
    /// same-session grant narrowing revokes only modes no longer authorized.
    pub(super) fn revoke_relay_device_authority(
        &mut self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        session_changed: bool,
        media: &RemoteMediaHandle,
    ) {
        let retired_offers = self
            .relay_offers
            .values()
            .filter(|offer| {
                offer.claims.target_device_id == device_id
                    && (session_changed
                        || !relay_grants_allow_mode(
                            authenticated_grants,
                            offer.claims.offered_mode,
                        )
                        || !offer
                            .claims
                            .required_grants
                            .iter()
                            .all(|grant| authenticated_grants.contains(grant)))
            })
            .cloned()
            .collect::<Vec<_>>();
        for offer in retired_offers {
            self.retire_failed_relay_offer(offer);
        }
        let retiring = self
            .relay_leases
            .values()
            .filter(|lease| {
                lease.device_id == device_id
                    && (session_changed
                        || !relay_grants_allow_mode(authenticated_grants, lease.mode))
            })
            .map(|lease| lease.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in retiring {
            self.revoke_relay_lease_by_id(
                &lease_id,
                if session_changed {
                    "mobile_session_rotated"
                } else {
                    "admission_grants_narrowed"
                },
                media,
            );
        }
        if session_changed {
            self.relay_replays
                .retain(|_, replay| replay.device_id != device_id);
            self.retired_prepared_rtc
                .retain(|_, retired| retired.claims.device_id != device_id);
            // The replacement mobile session did not hold the retired
            // session's in-memory lease. Its fresh authoritative snapshot is
            // the proof it needs; do not deliver terminal notices owed to the
            // predecessor endpoint session into the replacement.
            self.pending_relay_revocations
                .retain(|_, pending| pending.device_id != device_id);
        }
    }

    /// Revoke one plugin-minted relay lease even when it is between delivery
    /// phases. A committed claim takes the normal media-return path; an
    /// uncommitted monitor/provisional claim is simply retired because it never
    /// held caller authority.
    pub(super) fn revoke_relay_lease_by_id(
        &mut self,
        lease_id: &str,
        reason: &str,
        media: &RemoteMediaHandle,
    ) {
        let entry = self.relay_leases.get(lease_id).cloned();
        let claims = entry
            .as_ref()
            .and_then(|entry| self.leases.get(&entry.current_jti))
            .cloned()
            .or_else(|| {
                self.leases
                    .values()
                    .find(|claims| claims.lease_id == lease_id)
                    .cloned()
            });
        if let Some(claims) = claims {
            let _ = self.handle_lease_revoked(
                LeaseRevokedNotice {
                    kind: "lease_revoked".into(),
                    schema_version: SCHEMA_VERSION,
                    app_id: self.app_id.clone(),
                    device_id: claims.device_id.clone(),
                    lease_id: claims.lease_id,
                    lease_jti: claims.jti,
                    call_id: claims.call_id,
                    call_epoch: claims.call_epoch,
                    fence: claims.fence,
                    reason: reason.into(),
                },
                media,
            );
            return;
        }

        // Registry/book skew must never make an exact stable lease
        // unrevocable. Close every matching route and, if the peer has not
        // opened yet, synthesize the binding from the prepared claim.
        let route_ids = self
            .peers
            .iter()
            .filter_map(|(id, route)| {
                (route.binding.lease_id.as_deref() == Some(lease_id)).then(|| id.clone())
            })
            .collect::<Vec<_>>();
        for route_id in route_ids {
            if let Some(route) = self.peers.remove(&route_id) {
                let _ = media.revoke(&route.binding, reason);
                let _ = media.close_peer(&route_id, reason);
            }
        }
        if let Some(prepared) = self
            .prepared
            .as_ref()
            .filter(|prepared| prepared.provisional.lease_id == lease_id)
        {
            let mut binding = binding_for_claims(&prepared.provisional);
            if let Some(owner_epoch) = prepared.confirmed_owner_epoch {
                binding.owner_epoch = owner_epoch;
                binding.mode = match prepared.provisional.mode {
                    LeaseMode::Consult => MediaMode::Consult,
                    LeaseMode::Takeover => MediaMode::Talk,
                    LeaseMode::Monitor => binding.mode,
                };
            }
            let _ = media.revoke(&binding, reason);
        }
        self.leases.retain(|_, claims| claims.lease_id != lease_id);
        if self
            .prepared
            .as_ref()
            .is_some_and(|prepared| prepared.provisional.lease_id == lease_id)
        {
            self.prepared = None;
        }
        self.retire_relay_lease(lease_id);
    }

    /// Retire an ACTIVE consult/takeover lease whose replacement peer never
    /// opened. The deadline is deliberately separate from the renewable lease
    /// TTL: heartbeats prove that an app process is alive, not that a usable
    /// microphone/media path exists.
    pub(super) fn expire_unbound_active_rebind(&mut self, now: Instant, media: &RemoteMediaHandle) -> bool {
        let Some((lease_id, mode, device_id)) = self.prepared.as_ref().and_then(|prepared| {
            prepared
                .active_rebind_deadline
                .filter(|deadline| *deadline <= now)
                .map(|_| {
                    (
                        prepared.provisional.lease_id.clone(),
                        prepared.provisional.mode,
                        prepared.provisional.device_id.clone(),
                    )
                })
        }) else {
            return false;
        };
        eprintln!(
            "[aokie-plugin][takeover] stage=active_rebind_timeout device={} mode={:?} detail=The active lease never opened its replacement media peer",
            sanitize_gateway_code(&device_id),
            mode
        );
        self.revoke_relay_lease_by_id(&lease_id, "active_peer_not_opened", media);
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.next_snapshot_poll = Instant::now();
        true
    }

    /// Enforce signed lease expiry even when a silent client sends neither a
    /// heartbeat nor a revoke. Without this sweep, a delivered PREPARED lease
    /// that never opens its first peer could occupy the single claimant slot
    /// long after its non-renewable token expired.
    pub(super) fn expire_relay_leases(&mut self, now: u64, media: &RemoteMediaHandle) -> usize {
        let expired = self
            .relay_leases
            .values()
            .filter(|entry| {
                self.leases
                    .get(&entry.current_jti)
                    .is_none_or(|claims| claims.expires_at <= now)
            })
            .map(|entry| entry.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in &expired {
            self.revoke_relay_lease_by_id(lease_id, "lease_expired", media);
            // A missing session-book entry gives revoke_relay_lease_by_id no
            // claims to route through, so make the registry retirement
            // explicit as the final fail-closed step.
            self.retire_relay_lease(lease_id);
        }
        if !expired.is_empty() {
            self.last_snapshot_fingerprint = None;
            self.last_snapshot_sent = None;
            self.next_snapshot_poll = Instant::now();
        }
        expired.len()
    }

    /// Reconcile gateway authority against the media state, rather than
    /// relying exclusively on its bounded signalling/event queue. Terminal
    /// events are intentionally best-effort; if one is dropped behind ICE,
    /// the snapshot still proves that an ACTIVE talk/consult binding has
    /// disappeared and the corresponding lease must stop renewing.
    pub(super) fn reconcile_relay_media_authority(&mut self, media: &RemoteMediaHandle) -> usize {
        let remote = media.snapshot();
        let stale = self
            .relay_leases
            .values()
            .filter(|entry| {
                entry.phase == LeasePhase::Active
                    && matches!(entry.mode, LeaseMode::Consult | LeaseMode::Takeover)
            })
            .filter(|entry| {
                self.leases.get(&entry.current_jti).is_none_or(|claims| {
                    remote.call_id.as_deref() != Some(claims.call_id.as_str())
                        || remote.call_epoch != claims.call_epoch
                        || remote.owner_epoch != claims.owner_epoch
                        || remote.talk_device_id.as_deref() != Some(claims.device_id.as_str())
                        || remote.talk_lease_id.as_deref() != Some(claims.lease_id.as_str())
                        || remote.talk_fence != claims.fence
                })
            })
            .map(|entry| entry.lease_id.clone())
            .collect::<Vec<_>>();
        for lease_id in &stale {
            self.revoke_relay_lease_by_id(lease_id, "media_authority_disappeared", media);
            self.retire_relay_lease(lease_id);
        }
        if !stale.is_empty() {
            self.last_snapshot_fingerprint = None;
            self.last_snapshot_sent = None;
            self.next_snapshot_poll = Instant::now();
        }
        stale.len()
    }

    /// Converge the assistance mailbox with authoritative media truth for
    /// every transfer that won its one owner opportunity.
    ///
    /// This is snapshot-driven rather than event-only: the native event queue
    /// is bounded and may drop a terminal marker behind ICE. HumanActive is
    /// committed only while the separate setup deadline is live; every other
    /// disappearance first revokes/returns media, then records unavailable
    /// only after the same call is explicitly AokieActive again.
    pub(super) fn reconcile_accepted_transfers(
        &mut self,
        now: u64,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> usize {
        let broker = self.assistance.clone();
        let mut resolved = 0;
        let accepted = self
            .accepted_transfers
            .iter()
            .map(|(lease_id, transfer)| (lease_id.clone(), transfer.clone()))
            .collect::<Vec<_>>();

        for (lease_id, transfer) in accepted {
            let remote = media.snapshot();
            let same_call = remote.call_id.as_deref()
                == Some(transfer.offered_fence.call_id.as_str())
                && remote.call_epoch == transfer.offered_fence.call_epoch;
            if !same_call {
                // There is no caller left on the offered epoch to transfer.
                // Radio call-boundary cleanup owns the mailbox; only discard
                // this session-local correlation.
                self.accepted_transfers.remove(&lease_id);
                resolved += 1;
                continue;
            }

            let exact_human_active = remote.service_mode == LocalServiceMode::HumanActive
                && remote.talk_device_id.as_deref() == Some(transfer.device_id.as_str())
                && remote.talk_lease_id.as_deref() == Some(lease_id.as_str());
            if exact_human_active && !transfer.failback_requested && now < transfer.setup_expires_at
            {
                if broker
                    .transfer_taken(
                        &transfer.request_id,
                        &transfer.offered_fence,
                        &transfer.device_id,
                    )
                    .is_ok()
                {
                    self.accepted_transfers.remove(&lease_id);
                    resolved += 1;
                    continue;
                }
                // HumanActive without a matching live transfer request is
                // never allowed to remain an ordinary takeover by accident.
                if let Some(current) = self.accepted_transfers.get_mut(&lease_id) {
                    current.failback_requested = true;
                }
                self.revoke_relay_lease_by_id(&lease_id, "transfer_activation_not_current", media);
            }

            let authority_present = self.relay_leases.contains_key(&lease_id)
                || self
                    .leases
                    .values()
                    .any(|claims| claims.lease_id == lease_id)
                || self
                    .peers
                    .values()
                    .any(|route| route.binding.lease_id.as_deref() == Some(lease_id.as_str()))
                || self
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| prepared.provisional.lease_id == lease_id);
            let must_failback = self
                .accepted_transfers
                .get(&lease_id)
                .is_some_and(|current| current.failback_requested)
                || now >= transfer.setup_expires_at
                || !authority_present;
            if must_failback {
                let already_requested = self
                    .accepted_transfers
                    .get(&lease_id)
                    .is_some_and(|current| current.failback_requested);
                if let Some(current) = self.accepted_transfers.get_mut(&lease_id) {
                    current.failback_requested = true;
                }
                if !already_requested && authority_present {
                    self.revoke_relay_lease_by_id(&lease_id, "transfer_setup_failed", media);
                }
            }

            let returned = media.snapshot();
            let exact_aokie_return = returned.service_mode == LocalServiceMode::AokieActive
                && returned.call_id.as_deref() == Some(transfer.offered_fence.call_id.as_str())
                && returned.call_epoch == transfer.offered_fence.call_epoch
                && returned.owner_epoch >= transfer.offered_fence.owner_epoch
                && returned.remote_revision >= transfer.offered_fence.remote_revision
                && radio.switchboard_revision() == transfer.offered_fence.switchboard_revision;
            if must_failback && exact_aokie_return {
                let current_fence = crate::assistance::AssistanceCallFence {
                    call_id: transfer.offered_fence.call_id.clone(),
                    call_epoch: transfer.offered_fence.call_epoch,
                    owner_epoch: returned.owner_epoch,
                    switchboard_revision: radio.switchboard_revision(),
                    remote_revision: returned.remote_revision,
                };
                // Caller safety is already proved by AokieActive. If the radio
                // consumed the mailbox after its bounded grace, this becomes a
                // harmless bookkeeping miss rather than a reason to retain a
                // dead session correlation forever.
                let _ = broker.transfer_unavailable(
                    &transfer.request_id,
                    &transfer.offered_fence,
                    current_fence,
                    Some(&transfer.device_id),
                );
                self.accepted_transfers.remove(&lease_id);
                resolved += 1;
            }

            // A later call-waiting/switchboard revision may make the strict
            // speech fence intentionally unresolvable. Once exact same-call
            // AokieActive is proved, no media authority remains, and the
            // broker no longer exposes this accepted request (resolved or its
            // bounded grace elapsed), drop only the dead session correlation.
            // This never loosens TransferUnavailable's fence or speaks text.
            let broker_still_owns_request = broker
                .accepted_transfer(
                    &transfer.offered_fence.call_id,
                    transfer.offered_fence.call_epoch,
                )
                .is_some_and(|pending| pending.request_id == transfer.request_id);
            if self.accepted_transfers.contains_key(&lease_id)
                && must_failback
                && returned.service_mode == LocalServiceMode::AokieActive
                && returned.call_id.as_deref() == Some(transfer.offered_fence.call_id.as_str())
                && returned.call_epoch == transfer.offered_fence.call_epoch
                && !authority_present
                && !broker_still_owns_request
            {
                self.accepted_transfers.remove(&lease_id);
                resolved += 1;
            }
        }

        // A transport/session fail-closed can replace GatewaySession after it
        // has already told native media to return. Recover the one accepted
        // mailbox from broker state and close it only behind exact AokieActive.
        let returned = media.snapshot();
        if returned.service_mode == LocalServiceMode::AokieActive {
            if let Some(call_id) = returned.call_id.as_deref() {
                if let Some(orphan) = broker.accepted_transfer(call_id, returned.call_epoch) {
                    let tracked = self
                        .accepted_transfers
                        .values()
                        .any(|current| current.request_id == orphan.request_id);
                    if !tracked
                        && radio.switchboard_revision() == orphan.fence.switchboard_revision
                        && returned.owner_epoch >= orphan.fence.owner_epoch
                        && returned.remote_revision >= orphan.fence.remote_revision
                    {
                        let current_fence = crate::assistance::AssistanceCallFence {
                            call_id: orphan.fence.call_id.clone(),
                            call_epoch: orphan.fence.call_epoch,
                            owner_epoch: returned.owner_epoch,
                            switchboard_revision: radio.switchboard_revision(),
                            remote_revision: returned.remote_revision,
                        };
                        let _ = broker.transfer_unavailable(
                            &orphan.request_id,
                            &orphan.fence,
                            current_fence,
                            orphan.accepted_by.as_deref(),
                        );
                        resolved += 1;
                    }
                }
            }
        }
        resolved
    }

    /// Forget a plugin-minted lease.
    ///
    /// Called wherever the session purges its own lease book, so the relay
    /// registry can never outlive the authority it records — a stale entry
    /// would let a heartbeat renew a lease that failing media already revoked.
    pub(super) fn retire_relay_lease(&mut self, lease_id: &str) {
        self.relay_leases.remove(lease_id);
        self.retired_prepared_rtc.remove(lease_id);
        self.relay_replays.retain(|_, replay| {
            !matches!(
                &replay.result,
                RelayReplayResult::RetiredPreparedRtcDropped {
                    lease_id: retired_lease_id,
                    ..
                } if retired_lease_id == lease_id
            )
        });
        if self
            .deferred_prepare
            .as_ref()
            .is_some_and(|deferred| deferred.lease_id == lease_id)
        {
            if let Some(deferred) = self.deferred_prepare.take() {
                self.relay_replays.remove(&deferred.replay_key);
                self.retire_failed_relay_offer(deferred.offer);
            }
        }
        if self
            .pending_relay_status
            .as_ref()
            .is_some_and(|pending| pending.lease_id() == lease_id)
        {
            self.pending_relay_status = None;
        }
    }

    pub(super) fn prune_retired_prepared_rtc(&mut self, now: Instant) {
        self.retired_prepared_rtc
            .retain(|_, retired| retired.expires_at > now);
    }

    /// Remember one superseded PREPARED generation as a drop-only binding.
    ///
    /// The stable lease cap is also the global tombstone cap, and only one
    /// generation per device is retained.  A claimant therefore cannot grow
    /// memory by repeatedly rotating generations or reconnecting.
    pub(super) fn remember_retired_prepared_rtc(
        &mut self,
        claims: LeaseClaims,
        token: String,
        sdp_revision: u64,
        transport_generation: u64,
    ) {
        let now = Instant::now();
        self.prune_retired_prepared_rtc(now);
        let Ok(now_unix) = unix_now() else {
            return;
        };
        let remaining = claims.expires_at.saturating_sub(now_unix);
        if remaining == 0 {
            return;
        }
        let ttl = RETIRED_PREPARED_RTC_TTL.min(Duration::from_secs(remaining));
        self.retired_prepared_rtc.retain(|_, retired| {
            retired.claims.device_id != claims.device_id
                || retired.claims.lease_id == claims.lease_id
        });
        if self.retired_prepared_rtc.len() >= MAX_RELAY_LEASES
            && !self.retired_prepared_rtc.contains_key(&claims.lease_id)
        {
            if let Some(oldest) = self
                .retired_prepared_rtc
                .iter()
                .min_by_key(|(_, retired)| retired.expires_at)
                .map(|(lease_id, _)| lease_id.clone())
            {
                self.retired_prepared_rtc.remove(&oldest);
            }
        }
        self.retired_prepared_rtc.insert(
            claims.lease_id.clone(),
            RetiredPreparedRtcBinding {
                claims,
                token,
                sdp_revision,
                transport_generation,
                expires_at: now + ttl,
            },
        );
    }

    /// Return the exact retired PREPARED binding named by this frame.
    ///
    /// This is intentionally stricter than finding a stable lease: every
    /// immutable route field and the old bearer token must agree.  Matching
    /// only the lease id would turn the tombstone into an authority alias.
    pub(super) fn retired_prepared_rtc_for_frame(
        &mut self,
        frame: &MobileRtcSignalFrame,
    ) -> Option<RetiredPreparedRtcBinding> {
        self.prune_retired_prepared_rtc(Instant::now());
        self.retired_prepared_rtc
            .values()
            .find(|retired| {
                matches!(
                    &frame.signal,
                    RtcSignal::Ice { .. } | RtcSignal::IceComplete { .. }
                ) && retired.claims.phase == LeasePhase::Prepared
                    && retired.claims.device_id == frame.device_id
                    && retired.claims.jti == frame.lease_jti
                    && retired.claims.rtc_session_id == frame.rtc_session_id
                    && retired.claims.call_id == frame.call_id
                    && retired.claims.call_epoch == frame.call_epoch
                    && retired.claims.owner_epoch == frame.owner_epoch
                    && retired.claims.fence == frame.fence
                    && retired.sdp_revision == frame.sdp_revision
                    && retired.transport_generation == frame.transport_generation
                    && tokens_match(&retired.token, &frame.lease_token)
            })
            .cloned()
    }

    /// Verify that an exact tombstone match was signed by the same approved
    /// mobile endpoint and session as the retired lease.  A bearer alone is
    /// insufficient even though the result will only be dropped.
    pub(super) fn authenticate_retired_prepared_rtc(
        &mut self,
        frame: &MobileRtcSignalFrame,
        retired: &RetiredPreparedRtcBinding,
    ) -> bool {
        let Ok(now) = unix_now() else {
            return false;
        };
        let Ok(Some(authentication)) = frame.signal.verify_endpoint_authentication(now) else {
            return false;
        };
        let supplied_key = match &frame.signal {
            RtcSignal::Offer { binding, .. } | RtcSignal::Answer { binding, .. } => {
                &binding.endpoint_key
            }
            RtcSignal::Ice { envelope, .. } | RtcSignal::IceComplete { envelope } => {
                &envelope.endpoint_key
            }
            RtcSignal::Close { .. } => return false,
        };
        let Some(approved_key) = self
            .endpoint_authority
            .approved_mobile_keys
            .get(authentication.holder_key_thumbprint())
        else {
            return false;
        };
        let expected = &retired.claims;
        if supplied_key != approved_key
            || authentication.endpoint_role() != AdmissionRole::Mobile
            || authentication.endpoint_session_nonce() != expected.session_nonce
            || authentication.holder_key_thumbprint() != expected.mobile_key_thumbprint
            || authentication.peer_key_thumbprint() != expected.plugin_key_thumbprint
        {
            return false;
        }
        self.used_endpoint_jtis
            .retain(|_, expires_at| *expires_at > now);
        if self.used_endpoint_jtis.contains_key(authentication.jti())
            || self.used_endpoint_jtis.len() >= MAX_USED_ENDPOINT_JTIS
        {
            return false;
        }
        self.used_endpoint_jtis
            .insert(authentication.jti().to_owned(), authentication.expires_at());
        true
    }

    /// Encode a minted lease for the device that asked for it.
    pub(super) fn relay_lease_status(
        &self,
        status: PluginLeaseStatus,
        device_id: &str,
        request_id: &str,
        token: &str,
        lease: LeaseClaims,
        now: u64,
    ) -> Vec<String> {
        let frame = PluginLeaseStatusFrame {
            kind: "plugin_lease_status".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.to_owned(),
            request_id: request_id.to_owned(),
            status,
            lease_token: token.to_owned(),
            lease,
        };
        if frame.validate(now).is_err() {
            return self.relay_reject(
                device_id,
                request_id,
                "mode_unavailable",
                "the minted lease failed its own contract validation",
            );
        }
        self.relay_encode(&frame)
    }

    /// Register an exact terminal revoke before asking the relay to carry it.
    ///
    /// This deliberately runs before `send_text`: a delivery result may be
    /// `Dropped`, and the terminal media transition that produced this frame
    /// has already happened.  Retrying the retained bytes is safe; rebuilding
    /// the transition is not.
    pub(super) fn prepare_relay_delivery(&mut self, encoded: &str) {
        if !self.relay_carrier {
            return;
        }
        let Some(frame) = self.outbound_relay_revocation(encoded) else {
            return;
        };
        self.register_pending_relay_revocation(&frame, encoded, Instant::now());
    }

    pub(super) fn outbound_relay_revocation(&self, encoded: &str) -> Option<PluginLeaseRevokeFrame> {
        serde_json::from_str::<PluginLeaseRevokeFrame>(encoded)
            .ok()
            .filter(|frame| frame.app_id == self.app_id && frame.validate().is_ok())
    }

    pub(super) fn prune_pending_relay_revocations(&mut self, now: Instant) {
        self.pending_relay_revocations
            .retain(|_, pending| pending.expires_at > now);
    }

    pub(super) fn register_pending_relay_revocation(
        &mut self,
        frame: &PluginLeaseRevokeFrame,
        encoded: &str,
        now: Instant,
    ) {
        self.prune_pending_relay_revocations(now);
        if self
            .pending_relay_revocations
            .get(&frame.lease_id)
            .is_some_and(|pending| pending.encoded == encoded)
        {
            return;
        }
        if self.pending_relay_revocations.len() >= MAX_PENDING_RELAY_REVOCATIONS
            && !self.pending_relay_revocations.contains_key(&frame.lease_id)
        {
            if let Some(oldest) = self
                .pending_relay_revocations
                .iter()
                .min_by_key(|(_, pending)| pending.registered_at)
                .map(|(lease_id, _)| lease_id.clone())
            {
                self.pending_relay_revocations.remove(&oldest);
            }
        }
        let next_attempt_at = now + SNAPSHOT_POLL;
        self.pending_relay_revocations.insert(
            frame.lease_id.clone(),
            PendingRelayRevocation {
                encoded: encoded.to_owned(),
                device_id: frame.device_id.clone(),
                registered_at: now,
                next_attempt_at,
                expires_at: now + PENDING_RELAY_REVOCATION_TTL,
            },
        );
        if self.next_snapshot_poll > next_attempt_at {
            self.next_snapshot_poll = next_attempt_at;
        }
    }

    /// Return due notices in first-registration order and move their next due
    /// time forward one normal publication tick. Delivery completion either
    /// clears the exact bytes or re-arms them; neither path touches authority.
    pub(super) fn due_pending_relay_revocations(&mut self, now: Instant) -> Vec<String> {
        self.prune_pending_relay_revocations(now);
        let mut due_lease_ids = self
            .pending_relay_revocations
            .iter()
            .filter(|(_, pending)| pending.next_attempt_at <= now)
            .map(|(lease_id, pending)| {
                (
                    pending.next_attempt_at,
                    pending.registered_at,
                    lease_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        due_lease_ids.sort_by(|left, right| left.cmp(right));
        due_lease_ids
            .into_iter()
            .take(MAX_PENDING_RELAY_REVOCATIONS_PER_POLL)
            .filter_map(|(_, _, lease_id)| {
                self.pending_relay_revocations
                    .get_mut(&lease_id)
                    .map(|pending| {
                        pending.next_attempt_at = now + SNAPSHOT_POLL;
                        pending.encoded.clone()
                    })
            })
            .collect()
    }

    /// Commit or roll back the exact relay status frame just sent.
    pub(super) fn finish_relay_delivery(
        &mut self,
        encoded: &str,
        delivery: TransportDelivery,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) {
        if self.relay_carrier && delivery == TransportDelivery::Dropped {
            if let Ok(frame) = serde_json::from_str::<PluginOfferAcceptedFrame>(encoded) {
                if let Some(offer) = self
                    .relay_offers
                    .get(&frame.offer_id)
                    .filter(|offer| {
                        offer.accepted
                            && offer.claims.target_device_id == frame.device_id
                            && offer.claims.jti == frame.offer_jti
                    })
                    .cloned()
                {
                    self.retire_failed_relay_offer(offer);
                    eprintln!(
                        "[aokie-plugin][takeover] stage=relay_offer_acceptance_not_delivered device={} request={} detail=A fresh offer identity will be published; no transfer reservation remains",
                        sanitize_gateway_code(&frame.device_id),
                        sanitize_gateway_code(&frame.request_id)
                    );
                }
            }
        }
        if self.relay_carrier {
            if let Some(frame) = self.outbound_relay_revocation(encoded) {
                match delivery {
                    TransportDelivery::Delivered => {
                        let exact = self
                            .pending_relay_revocations
                            .get(&frame.lease_id)
                            .is_some_and(|pending| pending.encoded == encoded);
                        if exact {
                            self.pending_relay_revocations.remove(&frame.lease_id);
                        }
                    }
                    TransportDelivery::Dropped => {
                        let now = Instant::now();
                        self.register_pending_relay_revocation(&frame, encoded, now);
                        if let Some(pending) = self
                            .pending_relay_revocations
                            .get_mut(&frame.lease_id)
                            .filter(|pending| pending.encoded == encoded)
                        {
                            pending.next_attempt_at = now + SNAPSHOT_POLL;
                            if self.next_snapshot_poll > pending.next_attempt_at {
                                self.next_snapshot_poll = pending.next_attempt_at;
                            }
                        }
                    }
                }
            }
        }
        if self.relay_carrier {
            if let Ok(frame) = serde_json::from_str::<PluginSnapshotFrame>(encoded) {
                if self.relay_snapshot_event_id.as_deref() == Some(frame.event_id.as_str()) {
                    if let Some(device_id) = frame.device_id {
                        match delivery {
                            TransportDelivery::Delivered => {
                                self.relay_snapshot_delivered_devices.insert(device_id);
                            }
                            TransportDelivery::Dropped => {
                                self.relay_snapshot_delivered_devices.remove(&device_id);
                            }
                        }
                    }
                }
            }
            if let Ok(frame) = serde_json::from_str::<PluginAssistanceRequestFrame>(encoded) {
                match delivery {
                    TransportDelivery::Delivered => {
                        self.last_assistance_request_sent = Some(frame.request_id);
                    }
                    TransportDelivery::Dropped => {
                        if self.last_assistance_request_sent.as_deref()
                            == Some(frame.request_id.as_str())
                        {
                            self.last_assistance_request_sent = None;
                        }
                    }
                }
            }
        }
        if self.relay_carrier
            && delivery == TransportDelivery::Dropped
            && serde_json::from_str::<Envelope>(encoded)
                .is_ok_and(|frame| frame.kind == "plugin_snapshot")
        {
            // A snapshot is authoritative state, and immediately after revoke
            // it is also the Companion's completion receipt. A terminal 429 or
            // missing target must make it owed again, but at normal poll cadence
            // so a relay outage cannot turn the radio process into a hot loop.
            self.last_snapshot_fingerprint = None;
            self.last_snapshot_sent = None;
            self.next_snapshot_poll = Instant::now() + SNAPSHOT_POLL;
        }
        if self
            .deferred_prepare
            .as_ref()
            .is_some_and(|pending| pending.encoded == encoded)
        {
            let deferred = self
                .deferred_prepare
                .take()
                .expect("checked deferred prepare");
            if delivery == TransportDelivery::Dropped {
                self.relay_leases.remove(&deferred.lease_id);
                self.relay_replays.remove(&deferred.replay_key);
                self.retire_failed_relay_offer(deferred.offer);
                eprintln!(
                    "[aokie-plugin][takeover] stage=relay_prepare_not_delivered device={} request={} detail=The provisional grant was dropped before delivery; the caller stayed with Aokie",
                    sanitize_gateway_code(&deferred.device_id),
                    sanitize_gateway_code(&deferred.request_id)
                );
                return;
            }
            let lease_id = deferred.lease_id.clone();
            let device_id = deferred.device_id.clone();
            let request_id = deferred.request_id.clone();
            if let Err(error) = self.handle_claim_proposal(
                deferred.notice,
                media,
                radio,
                Some(deferred.expected_switchboard_revision),
            ) {
                self.leases.retain(|_, claims| claims.lease_id != lease_id);
                self.prepared = None;
                self.relay_replays.remove(&deferred.replay_key);
                self.retire_relay_lease(&lease_id);
                self.retire_failed_relay_offer(deferred.offer);
                eprintln!(
                    "[aokie-plugin][takeover] stage=relay_prepare_failed device={} request={} detail={}",
                    sanitize_gateway_code(&device_id),
                    sanitize_gateway_code(&request_id),
                    sanitize_status_message(&error.message)
                );
            }
            return;
        }

        if !self
            .pending_relay_status
            .as_ref()
            .is_some_and(|pending| pending.encoded() == encoded)
        {
            return;
        }
        let pending = self
            .pending_relay_status
            .take()
            .expect("checked relay status transition");
        match pending {
            PendingRelayStatus::MonitorGrant {
                notice,
                lease_id,
                offer_id,
                offer,
                replay_key,
                ..
            } => {
                if delivery == TransportDelivery::Dropped {
                    self.relay_leases.remove(&lease_id);
                    self.relay_replays.remove(&replay_key);
                    self.relay_offers.insert(offer_id, offer);
                    return;
                }
                if let Err(error) = self.handle_lease_granted(notice, radio) {
                    self.relay_replays.remove(&replay_key);
                    self.retire_relay_lease(&lease_id);
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_monitor_commit_failed detail={}",
                        sanitize_status_message(&error.message)
                    );
                }
            }
            PendingRelayStatus::Renewal {
                notice,
                lease_id,
                replay_key,
                ..
            } => {
                if delivery == TransportDelivery::Dropped {
                    self.relay_replays.remove(&replay_key);
                    return;
                }
                let renewed = notice.lease.clone();
                let token = notice.lease_token.clone();
                if let Err(error) = self.handle_lease_renewed(notice, media, radio) {
                    self.relay_replays.remove(&replay_key);
                    self.retire_relay_lease(&lease_id);
                    eprintln!(
                        "[aokie-plugin][companion] stage=relay_renewal_commit_failed detail={}",
                        sanitize_status_message(&error.message)
                    );
                    return;
                }
                if let Some(entry) = self.relay_leases.get_mut(&lease_id) {
                    entry.current_jti = renewed.jti;
                    entry.token = token;
                    entry.phase = renewed.phase;
                    entry.status = PluginLeaseStatus::Renewed;
                }
            }
            PendingRelayStatus::Active {
                lease_id,
                claims,
                token,
                ..
            } => {
                if delivery == TransportDelivery::Dropped {
                    if let Some(entry) = self.relay_leases.get(&lease_id).cloned() {
                        if let Some(current) = self.leases.get(&entry.current_jti).cloned() {
                            let _ = self.handle_lease_revoked(
                                LeaseRevokedNotice {
                                    kind: "lease_revoked".into(),
                                    schema_version: SCHEMA_VERSION,
                                    app_id: self.app_id.clone(),
                                    device_id: entry.device_id,
                                    lease_id: current.lease_id,
                                    lease_jti: current.jti,
                                    call_id: current.call_id,
                                    call_epoch: current.call_epoch,
                                    fence: current.fence,
                                    reason: "active_status_not_delivered".into(),
                                },
                                media,
                            );
                        } else {
                            self.retire_relay_lease(&lease_id);
                        }
                    }
                    return;
                }
                // Preserve only the exact PREPARED receive-only generation
                // that this delivered ACTIVE status supersedes.  This is a
                // short-lived drop-only tombstone, never an alternate live
                // token/JTI for the stable lease.
                let retired = self.relay_leases.get(&lease_id).and_then(|entry| {
                    self.prepared
                        .as_ref()
                        .filter(|prepared| prepared.provisional.lease_id == lease_id)
                        .filter(|prepared| {
                            entry.phase == LeasePhase::Prepared
                                && prepared.provisional.phase == LeasePhase::Prepared
                                && entry.current_jti == prepared.provisional.jti
                                && prepared.provisional_sdp_revision > 0
                                && prepared.provisional_transport_generation > 0
                        })
                        .map(|prepared| {
                            (
                                prepared.provisional.clone(),
                                entry.token.clone(),
                                prepared.provisional_sdp_revision,
                                prepared.provisional_transport_generation,
                            )
                        })
                });
                if let Some((claims, token, sdp_revision, transport_generation)) = retired {
                    self.remember_retired_prepared_rtc(
                        claims,
                        token,
                        sdp_revision,
                        transport_generation,
                    );
                }
                self.leases
                    .retain(|_, existing| existing.lease_id != lease_id);
                self.leases.insert(claims.jti.clone(), claims.clone());
                if let Some(entry) = self.relay_leases.get_mut(&lease_id) {
                    entry.current_jti = claims.jti;
                    entry.token = token;
                    entry.phase = LeasePhase::Active;
                    entry.status = PluginLeaseStatus::Active;
                }
                if let Some(prepared) = self
                    .prepared
                    .as_mut()
                    .filter(|prepared| prepared.provisional.lease_id == lease_id)
                {
                    prepared.active_rebind_deadline = match claims.mode {
                        LeaseMode::Consult => {
                            Some(Instant::now() + RELAY_CONSULT_ACTIVE_REBIND_TIMEOUT)
                        }
                        LeaseMode::Takeover => {
                            Some(Instant::now() + RELAY_TAKEOVER_ACTIVE_REBIND_TIMEOUT)
                        }
                        LeaseMode::Monitor => None,
                    };
                }
            }
        }
    }

    /// Encode one typed, in-band refusal.
    ///
    /// Returns an empty vec if the refusal itself cannot be built, because the
    /// alternative — propagating an error — would let a peer that sent
    /// something unencodable take down a session carrying a live call.
    pub(super) fn relay_reject(
        &self,
        device_id: &str,
        request_id: &str,
        code: &str,
        message: &str,
    ) -> Vec<String> {
        let frame = PluginClaimRejectedFrame {
            kind: "plugin_claim_rejected".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: device_id.to_owned(),
            request_id: request_id.to_owned(),
            code: sanitize_gateway_code(code),
            message: sanitize_status_message(message),
        };
        eprintln!(
            "[aokie-plugin][companion] stage=relay_claim_rejected device={} code={} detail={}",
            sanitize_gateway_code(device_id),
            frame.code,
            frame.message
        );
        if frame.validate().is_err() {
            return Vec::new();
        }
        serde_json::to_string(&frame)
            .map(|encoded| vec![encoded])
            .unwrap_or_default()
    }

    /// Emit and remember a terminal refusal for an offer that was already
    /// consumed. The relay is at-least-once and a successful POST only means
    /// mailbox acceptance, so the peer must be able to repeat the same request
    /// and receive the byte-equivalent decision after a dropped first attempt.
    pub(super) fn relay_recorded_rejection(
        &mut self,
        replay_key: &str,
        fingerprint: &str,
        device_id: &str,
        request_id: &str,
        code: &str,
        message: &str,
    ) -> Vec<String> {
        if let Ok(frame) = serde_json::from_str::<LeaseRequestFrame>(fingerprint) {
            if let Some(offer) = self
                .relay_redeeming_transfer_offers
                .remove(&frame.accepted_offer_id)
            {
                self.retire_failed_relay_offer(offer);
            }
        }
        let frames = self.relay_reject(device_id, request_id, code, message);
        if let Some(encoded) = frames.first() {
            self.relay_record_replay(
                replay_key.to_owned(),
                fingerprint.to_owned(),
                device_id.to_owned(),
                RelayReplayResult::Rejected {
                    encoded: encoded.clone(),
                },
            );
        }
        frames
    }

    /// Fail one authenticated RTC operation without leaving either side with
    /// an ambiguous media lease.
    ///
    /// The generic claim rejection is retained for operation correlation, but
    /// it intentionally carries no lease identity and therefore cannot prove
    /// authority return to the Companion.  Pair the first refusal with an
    /// exact, fully-fenced plugin revocation. Relay egress posts each frame
    /// separately, so the replay result preserves both byte-for-byte with the
    /// authoritative revocation first. Replaying them does not re-run the
    /// terminal state transition; mobile applies duplicate revokes through its
    /// completed-revocation tombstone.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn relay_recorded_terminal_rtc_failure(
        &mut self,
        recognised: &RelayLease,
        rtc_session_id: &str,
        reason: &str,
        replay_key: &str,
        fingerprint: &str,
        device_id: &str,
        request_id: &str,
        message: &str,
        media: &RemoteMediaHandle,
    ) -> Vec<String> {
        let revocation = match self.fail_relay_rtc_authority(
            recognised,
            rtc_session_id,
            reason,
            media,
        ) {
            Ok(encoded) => encoded,
            Err(error) => {
                eprintln!(
                    "[aokie-plugin][takeover] stage=terminal_rtc_revoke_encode_failed rtc={} detail={}",
                    sanitize_gateway_code(rtc_session_id),
                    sanitize_status_message(&error.message)
                );
                None
            }
        };
        let rejection = self
            .relay_reject(device_id, request_id, "lease_unknown", message)
            .into_iter()
            .next();
        match (revocation, rejection) {
            (Some(revocation), Some(rejection)) => {
                self.relay_record_replay(
                    replay_key.to_owned(),
                    fingerprint.to_owned(),
                    device_id.to_owned(),
                    RelayReplayResult::TerminalRtcFailure {
                        revocation: revocation.clone(),
                        rejection: rejection.clone(),
                    },
                );
                vec![revocation, rejection]
            }
            (None, Some(rejection)) => {
                self.relay_record_replay(
                    replay_key.to_owned(),
                    fingerprint.to_owned(),
                    device_id.to_owned(),
                    RelayReplayResult::Rejected {
                        encoded: rejection.clone(),
                    },
                );
                vec![rejection]
            }
            (Some(revocation), None) => vec![revocation],
            (None, None) => Vec::new(),
        }
    }

    /// Retire only the lease authenticated before grant narrowing.  A supplied
    /// rtcSessionId can name another peer, so it is safe to call `fail_peer`
    /// only when that route also matches the recognised device, JTI and stable
    /// lease.  Otherwise revoke the recognised lease without touching the
    /// foreign route and construct its notice from the plugin-minted claims.
    pub(super) fn fail_relay_rtc_authority(
        &mut self,
        recognised: &RelayLease,
        rtc_session_id: &str,
        reason: &str,
        media: &RemoteMediaHandle,
    ) -> Result<Option<String>, WorkerError> {
        let claims = self
            .leases
            .get(&recognised.current_jti)
            .filter(|claims| {
                claims.device_id == recognised.device_id
                    && claims.lease_id == recognised.lease_id
                    && claims.jti == recognised.current_jti
            })
            .cloned();
        let expected_binding = claims.as_ref().map(binding_for_claims);
        let exact_peer = self.peers.get(rtc_session_id).is_some_and(|route| {
            route.device_id == recognised.device_id
                && route.lease_jti == recognised.current_jti
                && route.binding.lease_id.as_deref() == Some(recognised.lease_id.as_str())
                && expected_binding
                    .as_ref()
                    .is_some_and(|binding| route.binding == *binding)
        });
        if exact_peer {
            return self.fail_peer(rtc_session_id, reason, media);
        }

        self.revoke_relay_lease_by_id(&recognised.lease_id, reason, media);
        claims
            .map(|claims| self.encode_failed_lease_revocation(&claims, reason))
            .transpose()
    }

    pub(super) fn encode_failed_lease_revocation(
        &self,
        claims: &LeaseClaims,
        reason: &str,
    ) -> Result<String, WorkerError> {
        let frame = PluginLeaseRevokeFrame {
            kind: "plugin_lease_revoke".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            device_id: claims.device_id.clone(),
            lease_id: claims.lease_id.clone(),
            lease_jti: claims.jti.clone(),
            call_id: claims.call_id.clone(),
            call_epoch: claims.call_epoch,
            fence: claims.fence,
            reason: reason.into(),
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Media revocation frame is invalid"))?;
        serde_json::to_string(&frame)
            .map_err(|_| WorkerError::reconnect("Media revocation could not be encoded"))
    }

    /// Encode one outbound relay frame, dropping it rather than failing.
    pub(super) fn relay_encode<T: Serialize>(&self, frame: &T) -> Vec<String> {
        serde_json::to_string(frame)
            .map(|encoded| vec![encoded])
            .unwrap_or_default()
    }

    /// Whether this device has already spent its control-request budget.
    ///
    /// A roster member that misbehaves — or simply loops — must not be able to
    /// drive minting, signing and radio snapshots at whatever rate it likes on
    /// the process that also runs the radio.
    pub(super) fn relay_over_budget(&mut self, device_id: &str) -> bool {
        Self::relay_budget_over(
            &mut self.relay_request_budget,
            device_id,
            RELAY_REQUEST_BUDGET,
        )
    }

    /// Whether this device has already spent its independent RTC trickle
    /// budget.  Keeping this separate is essential: healthy ICE gathering is
    /// bursty, while offer/lease/heartbeat controls are not.
    pub(super) fn relay_rtc_over_budget(&mut self, device_id: &str) -> bool {
        Self::relay_budget_over(
            &mut self.relay_rtc_signal_budget,
            device_id,
            RELAY_RTC_SIGNAL_BUDGET,
        )
    }

    pub(super) fn relay_budget_over(
        budget: &mut HashMap<String, (Instant, u32)>,
        device_id: &str,
        limit: u32,
    ) -> bool {
        let now = Instant::now();
        budget.retain(|_, (started, _)| now.duration_since(*started) < RELAY_REQUEST_WINDOW);
        if budget.len() >= MAX_RELAY_PEERS && !budget.contains_key(device_id) {
            return true;
        }
        let entry = budget.entry(device_id.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= RELAY_REQUEST_WINDOW {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 > limit
    }

    pub(super) fn relay_prune_replays(&mut self) {
        let now = Instant::now();
        self.relay_replays.retain(|_, replay| {
            now.duration_since(replay.seen_at) < RELAY_REPLAY_TTL
                && !matches!(
                    &replay.result,
                    RelayReplayResult::RetiredPreparedRtcDropped { expires_at, .. }
                        if *expires_at <= now
                )
        });
    }

    pub(super) fn relay_replay(&mut self, key: &str) -> Option<RelayReplay> {
        self.relay_prune_replays();
        self.relay_replays.get(key).cloned()
    }

    pub(super) fn relay_replay_has_room(&mut self, key: &str) -> bool {
        self.relay_prune_replays();
        self.relay_replays.contains_key(key) || self.relay_replays.len() < MAX_RELAY_REPLAYS
    }

    pub(super) fn relay_record_replay(
        &mut self,
        key: String,
        fingerprint: String,
        device_id: String,
        result: RelayReplayResult,
    ) {
        self.relay_prune_replays();
        if self.relay_replays.len() < MAX_RELAY_REPLAYS || self.relay_replays.contains_key(&key) {
            self.relay_replays.insert(
                key,
                RelayReplay {
                    fingerprint,
                    device_id,
                    result,
                    seen_at: Instant::now(),
                },
            );
        }
    }

    /// Re-encode the authority this lease currently holds for an exact retry.
    pub(super) fn relay_current_lease_status(
        &mut self,
        lease_id: &str,
        request_id: &str,
        media: &RemoteMediaHandle,
    ) -> Option<Vec<String>> {
        let entry = self.relay_leases.get(lease_id)?.clone();
        let claims = self.leases.get(&entry.current_jti)?.clone();
        let now = unix_now().ok()?;
        if claims.expires_at <= now {
            self.revoke_relay_lease_by_id(lease_id, "lease_expired", media);
            return None;
        }
        Some(self.relay_lease_status(
            entry.status,
            &entry.device_id,
            request_id,
            &entry.token,
            claims,
            now,
        ))
    }

    /// Whether the party that posted this frame is the device it claims to be.
    ///
    /// Snapshot projection prevents one device receiving another's offer, but
    /// identity is still enforced at the action boundary: copied/stale frames
    /// or a future projection regression cannot occupy the claimant slot and
    /// drive a soft hold on a live caller in someone else's name.
    pub(super) fn relay_sender_owns_device(
        &self,
        from_party: Option<&str>,
        authenticated_subject: Option<&str>,
        device_id: &str,
    ) -> bool {
        let Some(peer) = self.relay_peers.get(device_id) else {
            return false;
        };
        authenticated_subject == Some(device_id)
            && match from_party {
                // Absent only on carriers with no per-party addressing, which are
                // exactly the carriers that never reach this code.
                None => false,
                Some(party) => party == relay_party(&peer.holder_key_thumbprint),
            }
    }

    /// Apply only NARROWING observed in this frame's authenticated admission.
    ///
    /// A re-hello is required to broaden authority, but waiting for a re-hello
    /// to notice revocation would let a custom client keep an active takeover
    /// until its lease TTL. Every proved device frame can safely reduce the
    /// stored set and immediately return unsupported caller routes to Aokie.
    pub(super) fn relay_reconcile_frame_grants(
        &mut self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        media: &RemoteMediaHandle,
    ) {
        let Some(previous) = self.relay_peers.get(device_id) else {
            return;
        };
        let narrowed = previous
            .grants
            .intersection(authenticated_grants)
            .copied()
            .collect::<HashSet<_>>();
        if narrowed == previous.grants {
            return;
        }
        self.revoke_relay_device_authority(device_id, &narrowed, false, media);
        if let Some(peer) = self.relay_peers.get_mut(device_id) {
            peer.grants = narrowed;
        }
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        self.next_snapshot_poll = Instant::now();
    }

    /// Require the intersection of the verified hello's exact admission and
    /// the authenticated metadata on THIS frame. A stale/broadened payload can
    /// therefore neither preserve nor manufacture authority.
    pub(super) fn relay_mode_is_authorized(
        &self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        mode: LeaseMode,
    ) -> bool {
        self.relay_peers
            .get(device_id)
            .is_some_and(|peer| relay_grants_allow_mode(&peer.grants, mode))
            && relay_grants_allow_mode(authenticated_grants, mode)
    }

    /// Check the complete signed grant vector against both the verified hello
    /// and the admission metadata on this exact frame. Mode-only checks are
    /// insufficient for transfer offers, which additionally require the
    /// AssistanceRespond authority that binds acceptance to Aokie's request.
    pub(super) fn relay_required_grants_are_authorized(
        &self,
        device_id: &str,
        authenticated_grants: &HashSet<Grant>,
        required_grants: &[Grant],
    ) -> bool {
        self.relay_peers.get(device_id).is_some_and(|peer| {
            required_grants
                .iter()
                .all(|grant| peer.grants.contains(grant) && authenticated_grants.contains(grant))
        })
    }

}
