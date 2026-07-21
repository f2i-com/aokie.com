//! `GatewaySession` connect/run: the transport pump.

#[allow(unused_imports)]
use super::*;

impl GatewaySession {
    /// Authoritative publication ready for the carrier that is actually live.
    ///
    /// The socket sends one full plugin snapshot to trusted gateway
    /// infrastructure, which performs per-peer projection. The dumb relay has
    /// no trusted translator, so the plugin produces one redacted, targeted
    /// snapshot per authenticated device before any bytes leave the Desktop.
    pub(super) fn authoritative_state_frames(
        &mut self,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        let Some(encoded) = self.authoritative_state_frame(radio)? else {
            return Ok(Vec::new());
        };
        if !self.relay_carrier
            || !serde_json::from_str::<Envelope>(&encoded)
                .is_ok_and(|frame| frame.kind == "plugin_snapshot")
        {
            return Ok(vec![encoded]);
        }
        self.relay_project_snapshot(&encoded)
    }

    pub(super) fn authoritative_state_frame(
        &mut self,
        radio: &RadioHandle,
    ) -> Result<Option<String>, WorkerError> {
        if radio.current_call_id().is_none() {
            return self.idle_transition_frame();
        }

        let encoded = self.snapshot_frame(radio)?;
        if encoded.is_some() {
            self.authoritative_idle = false;
        }
        Ok(encoded)
    }

    /// Project one raw authoritative snapshot into device-addressed relay
    /// frames. This is the security boundary the old gateway supplied.
    pub(super) fn relay_project_snapshot(&mut self, encoded: &str) -> Result<Vec<String>, WorkerError> {
        let frame: PluginSnapshotFrame = serde_json::from_str(encoded)
            .map_err(|_| WorkerError::reconnect("Relay snapshot could not be projected"))?;
        if self.relay_snapshot_event_id.as_deref() != Some(frame.event_id.as_str()) {
            self.relay_snapshot_event_id = Some(frame.event_id.clone());
            self.relay_snapshot_delivered_devices.clear();
        }
        let mut devices = self.relay_peers.keys().cloned().collect::<Vec<_>>();
        devices.sort();
        let mut projected = Vec::with_capacity(devices.len());
        for device_id in devices {
            let Some(peer) = self.relay_peers.get(&device_id) else {
                continue;
            };
            if !peer.grants.contains(&Grant::StateRead) {
                continue;
            }
            let mut snapshot = frame.snapshot.clone();
            if !peer.grants.contains(&Grant::CallerRead) {
                snapshot.caller = None;
            }
            let captions_permitted = peer.grants.contains(&Grant::CaptionsRead)
                && snapshot.remote_consent.enabled
                && snapshot.remote_consent.acknowledged
                && snapshot.remote_consent.captions_enabled;
            if !captions_permitted {
                snapshot.captions.clear();
            }
            let telemetry_consent_current = remote_consent_is_current(
                snapshot.remote_consent.enabled,
                snapshot.remote_consent.acknowledged,
                snapshot.remote_consent.expires_at.as_deref(),
            );
            if !telemetry_consent_current {
                snapshot.participants.clear();
                snapshot.audio_levels = None;
            } else {
                if !peer.grants.contains(&Grant::AudioLevelsRead) {
                    snapshot.audio_levels = None;
                }
                if !peer.grants.contains(&Grant::ParticipantsRead) {
                    snapshot.participants.clear();
                    if let Some(levels) = snapshot.audio_levels.as_mut() {
                        levels.retain(|level| level.source != AudioLevelSource::Companion);
                    }
                } else if !peer.grants.contains(&Grant::ParticipantIdentityRead) {
                    for participant in &mut snapshot.participants {
                        participant.subject_id = None;
                        participant.display_label = None;
                    }
                }
            }
            snapshot.pending_mobile_offers.retain(|offer| {
                offer.offer.target_device_id == device_id
                    && relay_grants_allow_mode(&peer.grants, offer.offer.offered_mode)
                    && offer
                        .offer
                        .required_grants
                        .iter()
                        .all(|grant| peer.grants.contains(grant))
            });
            let targeted = PluginSnapshotFrame {
                kind: frame.kind.clone(),
                schema_version: frame.schema_version,
                app_id: frame.app_id.clone(),
                event_id: frame.event_id.clone(),
                device_id: Some(device_id),
                snapshot,
            };
            targeted.validate().map_err(|_| {
                WorkerError::reconnect("Projected relay snapshot failed local validation")
            })?;
            projected.push(serde_json::to_string(&targeted).map_err(|_| {
                WorkerError::reconnect("Projected relay snapshot could not be encoded")
            })?);
        }
        Ok(projected)
    }

    pub(super) fn idle_transition_frame(&mut self) -> Result<Option<String>, WorkerError> {
        if self.authoritative_idle {
            return Ok(None);
        }
        let frame = PluginIdleFrame {
            kind: "plugin_idle".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            event_id: format!("idle_{}", uuid::Uuid::new_v4().simple()),
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Authoritative idle frame is invalid"))?;
        let encoded = serde_json::to_string(&frame)
            .map_err(|_| WorkerError::reconnect("Authoritative idle frame could not be encoded"))?;
        self.authoritative_idle = true;
        self.last_snapshot_fingerprint = None;
        self.last_snapshot_sent = None;
        Ok(Some(encoded))
    }

    pub(super) fn snapshot_frame(&mut self, radio: &RadioHandle) -> Result<Option<String>, WorkerError> {
        let Some(call_id) = radio.current_call_id() else {
            return Ok(None);
        };
        let media = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media endpoint is unavailable"))?;
        let remote = media.snapshot();
        if remote.call_id.as_deref() != Some(call_id.as_str()) || remote.call_epoch == 0 {
            return Ok(None);
        }
        let active = radio.is_call_active();
        let switch_in_flight = radio.switch_in_flight();
        let service_mode = map_service_mode(remote.service_mode);
        let media_state = authoritative_media_state(&remote, active);
        let (participants, audio_levels) = authoritative_remote_telemetry(&remote);
        let caller = radio.current_caller().map(|number| CallerProjection {
            label: None,
            masked_number: mask_number(&number),
        });
        let snapshot = AuthoritativeCallSnapshot {
            call_id,
            call_epoch: remote.call_epoch,
            owner_epoch: remote.owner_epoch,
            switchboard_revision: radio.switchboard_revision(),
            remote_revision: remote.remote_revision,
            telephony_state: if active {
                TelephonyState::Active
            } else {
                TelephonyState::Ringing
            },
            service_mode,
            media_state,
            remote_capabilities: RemoteCapabilities {
                software_hold: true,
                // The local software hold is authoritative; carrier-network
                // hold and secondary-call behaviour remain unknown until the
                // radio reports actual negotiated/observed evidence.
                carrier_hold_evidence: CarrierHoldEvidence::Unknown,
                secondary_call_observation: SecondaryCallObservation::Unknown,
                voice_consult: remote.consent.consult_enabled,
                takeover: remote.consent.takeover_enabled,
            },
            secondary_call_policy: SecondaryCallPolicy::Normal,
            secondary_call: None,
            remote_consent: RemoteConsentPolicy {
                policy_id: remote.consent.policy_id.clone(),
                policy_version: remote.consent.policy_version,
                enabled: remote.consent.enabled,
                acknowledged: remote.consent.acknowledged,
                acknowledged_at: remote.consent.acknowledged_at.clone(),
                expires_at: remote.consent.expires_at.clone(),
                captions_enabled: remote.consent.captions_enabled,
                assistance_enabled: remote.consent.assistance_enabled,
                monitor_enabled: remote.consent.monitor_enabled,
                consult_enabled: remote.consent.consult_enabled,
                takeover_enabled: remote.consent.takeover_enabled,
            },
            caller,
            captions: remote
                .captions
                .iter()
                .map(|caption| Caption {
                    caption_id: caption.caption_id.clone(),
                    speaker: caption.speaker.clone(),
                    text: caption.text.clone(),
                    occurred_at: caption.occurred_at.clone(),
                    final_text: caption.final_text,
                })
                .collect(),
            participants,
            audio_levels,
            companion_microphone_muted: remote.microphone_muted,
            pending_mobile_offers: Vec::new(),
            occurred_at: aokie_core::events::now_iso8601(),
        };
        snapshot
            .validate()
            .map_err(|_| WorkerError::reconnect("Authoritative call snapshot is invalid"))?;
        let fingerprint = serde_json::to_string(&json!({
            "callId": snapshot.call_id,
            "callEpoch": snapshot.call_epoch,
            "ownerEpoch": snapshot.owner_epoch,
            "switchboardRevision": snapshot.switchboard_revision,
            "remoteRevision": snapshot.remote_revision,
            "telephonyState": snapshot.telephony_state,
            "serviceMode": snapshot.service_mode,
            "mediaState": snapshot.media_state,
            "remoteCapabilities": snapshot.remote_capabilities,
            "secondaryCallPolicy": snapshot.secondary_call_policy,
            "secondaryCall": snapshot.secondary_call,
            "caller": snapshot.caller,
            "remoteConsent": snapshot.remote_consent,
            "captions": snapshot.captions,
            "participants": snapshot.participants,
            "audioLevels": snapshot.audio_levels,
            "companionMicrophoneMuted": snapshot.companion_microphone_muted,
            "switchInFlight": switch_in_flight,
        }))
        .map_err(|_| WorkerError::reconnect("Call snapshot fingerprint failed"))?;
        let unchanged = self.last_snapshot_fingerprint.as_deref() == Some(fingerprint.as_str());
        let refresh_due = self
            .last_snapshot_sent
            .is_none_or(|sent| sent.elapsed() >= SNAPSHOT_REFRESH);
        if unchanged && !refresh_due {
            return Ok(None);
        }
        // Attach offers only once this snapshot is definitely going out, and
        // AFTER the fingerprint is taken. The fingerprint deliberately ignores
        // offers: including them would make every offer refresh look like a
        // state change and republish the snapshot on every poll.
        //
        // The Companion filters an offer against the snapshot it arrived in, so
        // these are stamped with the fences of THIS frame rather than a re-read
        // of live state that may already have moved on.
        let snapshot = if switch_in_flight {
            // Publish state so the Companion can lock controls, but never mint
            // or re-publish a caller-seizing offer while CHLD topology is in
            // flight. Including this flag in the fingerprint above forces a
            // fresh offer-bearing snapshot as soon as the switch settles.
            snapshot
        } else {
            self.attach_pending_offers(snapshot, &remote)?
        };
        snapshot
            .validate()
            .map_err(|_| WorkerError::reconnect("Authoritative call snapshot is invalid"))?;
        let frame = PluginSnapshotFrame {
            kind: "plugin_snapshot".into(),
            schema_version: SCHEMA_VERSION,
            app_id: self.app_id.clone(),
            event_id: format!("snapshot_{}", uuid::Uuid::new_v4().simple()),
            device_id: None,
            snapshot,
        };
        frame
            .validate()
            .map_err(|_| WorkerError::reconnect("Plugin snapshot frame is invalid"))?;
        self.last_snapshot_fingerprint = Some(fingerprint);
        self.last_snapshot_sent = Some(Instant::now());
        serde_json::to_string(&frame)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("Plugin snapshot could not be encoded"))
    }

    /// Stamp this snapshot with the offers its recipients may act on.
    ///
    /// Empty on the socket carrier, where the gateway is the authority. Empty
    /// too when the kill switch is off, which is what makes
    /// `AOKIE_RELAY_LEASE_AUTHORITY=0` a complete revert: no offer means the
    /// Companion's own `select_mobile_offer` refuses before it sends anything.
    pub(super) fn attach_pending_offers(
        &mut self,
        mut snapshot: AuthoritativeCallSnapshot,
        remote: &crate::remote_media::RemoteMediaSnapshot,
    ) -> Result<AuthoritativeCallSnapshot, WorkerError> {
        if !self.relay_authority_enabled || !self.relay_carrier || self.relay_peers.is_empty() {
            return Ok(snapshot);
        }
        // An offer is an invitation to seize a LIVE caller's audio, so it is
        // only ever made about a call that is genuinely up. `Ringing` has no
        // audio to take and `Ended` has no caller left.
        if !matches!(snapshot.telephony_state, TelephonyState::Active) {
            return Ok(snapshot);
        }
        let now = unix_now()?;
        let expired = self
            .relay_offers
            .values()
            .filter(|offer| offer.claims.expires_at <= now)
            .filter(|offer| {
                // A delivered transfer decision owns a longer, separately
                // bounded media-setup window than the invitation itself. Keep
                // that exact accepted offer hidden-but-redeemable until the
                // broker window ends.
                !offer.accepted
                    || !offer
                        .claims
                        .accepted_transfer_request_id
                        .as_deref()
                        .and_then(|request_id| {
                            self.assistance
                                .pending_transfer(&offer.claims.call_id, offer.claims.call_epoch)
                                .filter(|pending| {
                                    pending.request_id == request_id
                                        && pending.fence.call_id == offer.claims.call_id
                                        && pending.fence.call_epoch == offer.claims.call_epoch
                                        && pending.fence.owner_epoch == offer.claims.owner_epoch
                                        && pending.fence.switchboard_revision
                                            == offer.claims.switchboard_revision
                                        && pending.fence.remote_revision
                                            == offer.claims.remote_revision
                                        && pending.accepted_by.as_deref()
                                            == Some(offer.claims.target_device_id.as_str())
                                })
                        })
                        .is_some()
            })
            .cloned()
            .collect::<Vec<_>>();
        for offer in expired {
            if offer.accepted {
                self.retire_failed_relay_offer(offer);
            } else {
                self.relay_offers.remove(&offer.claims.offer_id);
            }
        }

        // Consult is the one mode with a precondition beyond consent: it exists
        // to answer a question Aokie asked, so without a current assistance
        // request there is nothing to consult about — and `handle_claim_proposal`
        // would refuse the claim anyway.
        let assistance = self.assistance.pending_frame(&self.app_id);
        let assistance_matches = assistance.as_ref().is_some_and(|assistance| {
            assistance.call_id == snapshot.call_id
                && assistance.call_epoch == snapshot.call_epoch
                && assistance.owner_epoch == snapshot.owner_epoch
                && assistance.switchboard_revision == snapshot.switchboard_revision
                && assistance.remote_revision == snapshot.remote_revision
        });
        let consult_available = assistance_matches
            && assistance
                .as_ref()
                .is_some_and(|assistance| !assistance.transfer_offered);
        let transfer_request_id = assistance
            .as_ref()
            .filter(|assistance| assistance_matches && assistance.transfer_offered)
            .map(|assistance| assistance.request_id.clone());
        let accepted_transfer_in_setup = self
            .assistance
            .accepted_transfer(&snapshot.call_id, snapshot.call_epoch)
            .is_some();
        let current_transfer_opportunity =
            transfer_request_id.as_deref().map(transfer_opportunity_id);
        self.relay_offer_winners.retain(|opportunity_id, winner| {
            current_transfer_opportunity.as_deref() == Some(opportunity_id.as_str())
                || self.relay_offers.values().any(|offer| {
                    offer.claims.opportunity_id == *opportunity_id
                        && offer.claims.offer_id == *winner
                })
        });

        let mut devices = self.relay_peers.keys().cloned().collect::<Vec<_>>();
        devices.sort();
        let mut published: Vec<SignedPendingMobileOffer> = Vec::new();
        for device_id in devices {
            for (mode, surface) in [
                (LeaseMode::Monitor, MobileOfferSurface::InApp),
                (LeaseMode::Consult, MobileOfferSurface::InApp),
                (LeaseMode::Takeover, MobileOfferSurface::InApp),
                (LeaseMode::Takeover, MobileOfferSurface::VoiceSystemUi),
            ] {
                if published.len() >= MAX_PENDING_MOBILE_OFFERS {
                    break;
                }
                if !self
                    .relay_peers
                    .get(&device_id)
                    .is_some_and(|peer| relay_grants_allow_mode(&peer.grants, mode))
                {
                    continue;
                }
                let accepted_transfer_request_id = (mode == LeaseMode::Takeover)
                    .then(|| transfer_request_id.clone())
                    .flatten();
                if mode == LeaseMode::Takeover
                    && accepted_transfer_request_id.is_none()
                    && accepted_transfer_in_setup
                {
                    continue;
                }
                if surface == MobileOfferSurface::VoiceSystemUi
                    && accepted_transfer_request_id.is_none()
                {
                    continue;
                }
                if accepted_transfer_request_id.is_some()
                    && !self
                        .relay_peers
                        .get(&device_id)
                        .is_some_and(|peer| peer.grants.contains(&Grant::AssistanceRespond))
                {
                    continue;
                }
                if accepted_transfer_request_id
                    .as_deref()
                    .is_some_and(|request_id| {
                        self.relay_offer_winners
                            .contains_key(&transfer_opportunity_id(request_id))
                    })
                {
                    continue;
                }
                // Consent is re-read from the effective gate, so an expired or
                // withdrawn disclosure stops producing offers immediately. This
                // is a fast fail only: the authoritative check lives inside the
                // media state lock and runs again on every claim and every
                // caller-bound frame.
                let permitted = remote.consent.enabled
                    && remote.consent.acknowledged
                    && match mode {
                        LeaseMode::Monitor => remote.consent.monitor_enabled,
                        LeaseMode::Consult => remote.consent.consult_enabled && consult_available,
                        LeaseMode::Takeover => remote.consent.takeover_enabled,
                    };
                if !permitted {
                    continue;
                }
                if let Some(offer) = self.reusable_offer(
                    &device_id,
                    mode,
                    surface,
                    accepted_transfer_request_id.as_deref(),
                    &snapshot,
                    now,
                ) {
                    published.push(offer);
                    continue;
                }
                if let Some(offer) = self.mint_offer(
                    &device_id,
                    mode,
                    surface,
                    accepted_transfer_request_id,
                    &snapshot,
                    now,
                )? {
                    published.push(offer);
                }
            }
        }
        snapshot.pending_mobile_offers = published;
        Ok(snapshot)
    }

    /// The live offer for this (device, mode) if it is still worth publishing.
    ///
    /// Reuse keeps the offer identity stable while nothing has changed, which
    /// matters because the fences move on real transitions rather than on a
    /// timer: re-minting per publish would hand the Companion a new `offerId`
    /// every poll and turn an answer already in flight into a stale one.
    pub(super) fn reusable_offer(
        &self,
        device_id: &str,
        mode: LeaseMode,
        surface: MobileOfferSurface,
        accepted_transfer_request_id: Option<&str>,
        snapshot: &AuthoritativeCallSnapshot,
        now: u64,
    ) -> Option<SignedPendingMobileOffer> {
        self.relay_offers
            .values()
            .find(|offer| {
                offer.claims.target_device_id == device_id
                    && offer.claims.offered_mode == mode
                    && offer.claims.surface == surface
                    && offer.claims.accepted_transfer_request_id.as_deref()
                        == accepted_transfer_request_id
                    && !offer.accepted
                    && offer.claims.expires_at > now.saturating_add(RELAY_OFFER_REFRESH_MARGIN)
                    && Self::offer_matches_snapshot(&offer.claims, snapshot)
            })
            .map(|offer| SignedPendingMobileOffer {
                offer: offer.claims.clone(),
                offer_token: offer.token.clone(),
            })
    }

    /// Whether an offer still describes exactly the call state being published.
    ///
    /// The Companion applies the same equality before it will answer, so an
    /// offer that drifts from its snapshot is unusable rather than merely
    /// stale — and re-publishing it would be dead weight in the frame.
    pub(super) fn offer_matches_snapshot(
        claims: &PendingMobileOfferClaims,
        snapshot: &AuthoritativeCallSnapshot,
    ) -> bool {
        claims.call_id == snapshot.call_id
            && claims.call_epoch == snapshot.call_epoch
            && claims.owner_epoch == snapshot.owner_epoch
            && claims.switchboard_revision == snapshot.switchboard_revision
            && claims.remote_revision == snapshot.remote_revision
            && claims.required_consent_policy_id == snapshot.remote_consent.policy_id
            && claims.required_consent_policy_version == snapshot.remote_consent.policy_version
    }

    pub(super) fn mint_offer(
        &mut self,
        device_id: &str,
        mode: LeaseMode,
        surface: MobileOfferSurface,
        accepted_transfer_request_id: Option<String>,
        snapshot: &AuthoritativeCallSnapshot,
        now: u64,
    ) -> Result<Option<SignedPendingMobileOffer>, WorkerError> {
        let Some(peer) = self.relay_peers.get(device_id) else {
            return Ok(None);
        };
        if !relay_grants_allow_mode(&peer.grants, mode) {
            return Ok(None);
        }
        // Drop the superseded, un-answered offer for this (device, mode) so the
        // registry tracks live invitations rather than growing with history.
        // An ACCEPTED one is left alone: the device is mid-redemption against
        // that exact identity and dropping it would refuse a claim already in
        // flight.
        self.relay_offers.retain(|_, offer| {
            offer.accepted
                || offer.claims.target_device_id != device_id
                || offer.claims.offered_mode != mode
                || offer.claims.surface != surface
        });
        if self.relay_offers.len() >= MAX_RELAY_OFFERS {
            // Refuse to mint rather than evict: an entry still in here may be
            // the one a device is redeeming right now. The Companion simply
            // sees no offer this turn and the next publish makes room.
            return Ok(None);
        }
        let claims = PendingMobileOfferClaims {
            offer_id: format!("offer_{}", uuid::Uuid::new_v4().simple()),
            opportunity_id: accepted_transfer_request_id
                .as_deref()
                .map(transfer_opportunity_id)
                .unwrap_or_else(|| format!("opportunity_{}", uuid::Uuid::new_v4().simple())),
            target_device_id: device_id.to_owned(),
            target_holder_key_thumbprint: peer.holder_key_thumbprint.clone(),
            offered_mode: mode,
            surface,
            app_id: self.app_id.clone(),
            call_id: snapshot.call_id.clone(),
            call_epoch: snapshot.call_epoch,
            owner_epoch: snapshot.owner_epoch,
            switchboard_revision: snapshot.switchboard_revision,
            remote_revision: snapshot.remote_revision,
            accepted_transfer_request_id: accepted_transfer_request_id.clone(),
            required_consent_policy_id: snapshot.remote_consent.policy_id.clone(),
            required_consent_policy_version: snapshot.remote_consent.policy_version,
            required_grants: {
                let mut grants = vec![Grant::StateRead, Grant::RtcSignal, relay_mode_grant(mode)];
                if mode == LeaseMode::Takeover {
                    grants.push(Grant::ResumeAokie);
                }
                if accepted_transfer_request_id.is_some() {
                    grants.push(Grant::AssistanceRespond);
                }
                grants
            },
            issued_at: now,
            expires_at: now.saturating_add(RELAY_OFFER_TTL),
            jti: format!("offerjti_{}", uuid::Uuid::new_v4().simple()),
        };
        // Validate what we are about to assert, exactly as the receiver will.
        // A malformed offer would be caught by the snapshot's own validate and
        // take the whole frame down with it — including the state an active
        // call depends on.
        if claims.validate(now).is_err() {
            return Ok(None);
        }
        let token = self.endpoint_authority.sign(
            &claims
                .signing_bytes()
                .map_err(|_| WorkerError::reconnect("Mobile offer could not be signed"))?,
        );
        let signed = SignedPendingMobileOffer {
            offer: claims.clone(),
            offer_token: token.clone(),
        };
        self.relay_offers.insert(
            claims.offer_id.clone(),
            MintedOffer {
                claims,
                token,
                accepted: false,
            },
        );
        Ok(Some(signed))
    }

    pub(super) fn assistance_frame(&mut self, radio: &RadioHandle) -> Result<Option<String>, WorkerError> {
        let Some(frame) = self.assistance.pending_frame(&self.app_id) else {
            self.last_assistance_request_sent = None;
            return Ok(None);
        };
        if self.last_assistance_request_sent.as_deref() == Some(frame.request_id.as_str()) {
            return Ok(None);
        }
        if self.relay_carrier && !self.relay_assistance_snapshot_ready() {
            return Ok(None);
        }
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media endpoint is unavailable"))?
            .snapshot();
        if !remote.consent.enabled
            || !remote.consent.acknowledged
            || !remote.consent.assistance_enabled
        {
            return Ok(None);
        }
        if remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.call_epoch
            || remote.owner_epoch != frame.owner_epoch
            || radio.switchboard_revision() != frame.switchboard_revision
            || remote.remote_revision != frame.remote_revision
        {
            return Ok(None);
        }
        match frame.validate(unix_now()?) {
            Ok(()) => {}
            Err(V2ProtocolError::Expired) => return Ok(None),
            Err(_) => return Err(WorkerError::reconnect("assistance request is invalid")),
        }
        serde_json::to_string(&frame)
            .map(Some)
            .map_err(|_| WorkerError::reconnect("assistance request could not be encoded"))
    }

    pub(super) fn relay_assistance_snapshot_ready(&self) -> bool {
        if self.relay_snapshot_event_id.is_none() {
            return false;
        }
        let mut eligible = self.relay_peers.iter().filter(|(_, peer)| {
            peer.grants.contains(&Grant::StateRead) && peer.grants.contains(&Grant::AssistanceRead)
        });
        let Some((first_device, _)) = eligible.next() else {
            return false;
        };
        self.relay_snapshot_delivered_devices.contains(first_device)
            && eligible
                .all(|(device_id, _)| self.relay_snapshot_delivered_devices.contains(device_id))
    }
}
