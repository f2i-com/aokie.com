//! `GatewaySession` end-caller execution, claim proposals and lease notices.

#[allow(unused_imports)]
use super::*;

impl GatewaySession {
    pub(super) fn handle_end_caller_execute(
        &mut self,
        frame: PluginEndCallerExecuteFrame,
        radio: &RadioHandle,
    ) -> Result<Vec<String>, WorkerError> {
        if frame.app_id != self.app_id {
            return Err(WorkerError::rebootstrap(
                "Companion caller-ending command crossed application identity",
            ));
        }
        if self.pending_end_caller.contains_key(&frame.operation_id) {
            // Gateway idempotency may replay an accepted response to the
            // mobile, but must not normally relay twice. A byte-identical
            // duplicate is harmless and never queues another radio command.
            return Ok(Vec::new());
        }
        if !self.pending_end_caller.is_empty() {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "end_caller_pending",
                "another caller-ending command is still pending",
            )?]);
        }
        let Some(claims) = self.leases.get(&frame.lease_jti) else {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "stale_lease",
                "the exact takeover lease is no longer present",
            )?]);
        };
        if claims.app_id != frame.app_id
            || claims.plugin_id != self.plugin_id
            || claims.device_id != frame.device_id
            || claims.call_id != frame.call_id
            || claims.call_epoch != frame.call_epoch
            || claims.owner_epoch != frame.owner_epoch
            || claims.lease_id != frame.lease_id
            || claims.jti != frame.lease_jti
            || claims.fence != frame.fence
            || claims.mode != LeaseMode::Takeover
            || claims.phase != LeasePhase::Active
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "lease_fence_mismatch",
                "the command does not match the exact active takeover lease",
            )?]);
        }
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
            .snapshot();
        if !radio.is_call_active()
            || radio.current_call_id().as_deref() != Some(frame.call_id.as_str())
            || radio.switchboard_revision() != frame.switchboard_revision
            || remote.call_id.as_deref() != Some(frame.call_id.as_str())
            || remote.call_epoch != frame.call_epoch
            || remote.owner_epoch != frame.owner_epoch
            || remote.remote_revision != frame.remote_revision
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "physical_fence_stale",
                "the physical call epochs or revisions changed",
            )?]);
        }
        if remote.service_mode != LocalServiceMode::HumanActive
            || remote.talk_device_id.as_deref() != Some(frame.device_id.as_str())
            || remote.talk_lease_id.as_deref() != Some(frame.lease_id.as_str())
            || remote.talk_fence != frame.fence
            || !remote.radio_reserved
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "not_active_takeover_owner",
                "the selected Companion is not the physical caller owner",
            )?]);
        }
        if !remote.consent.is_current() || !remote.consent.takeover_enabled {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "consent_required",
                "current remote consent no longer permits takeover control",
            )?]);
        }

        let request = CompanionEndCallerRequest {
            call_id: frame.call_id.clone(),
            call_epoch: frame.call_epoch,
            owner_epoch: frame.owner_epoch,
            switchboard_revision: frame.switchboard_revision,
            remote_revision: frame.remote_revision,
            device_id: frame.device_id.clone(),
            lease_id: frame.lease_id.clone(),
            fence: frame.fence,
        };
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        if radio
            .send(RadioControl::EndCallerFromCompanion {
                request,
                reply: result_tx,
            })
            .is_err()
        {
            return Ok(vec![encode_end_caller_failure(
                &frame,
                "radio_unavailable",
                "the physical radio command queue is unavailable",
            )?]);
        }
        self.pending_end_caller.insert(
            frame.operation_id.clone(),
            PendingEndCaller {
                execute: frame,
                result_rx,
            },
        );
        Ok(Vec::new())
    }

    pub(super) fn drain_end_caller_results(
        &mut self,
        media: &RemoteMediaHandle,
    ) -> Result<Vec<String>, WorkerError> {
        use std::sync::mpsc::TryRecvError;
        let mut finished = Vec::new();
        for (operation_id, pending) in &self.pending_end_caller {
            match pending.result_rx.try_recv() {
                Ok(result) => finished.push((operation_id.clone(), result)),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => finished.push((
                    operation_id.clone(),
                    Err(CompanionEndCallerFailure {
                        code: "radio_result_lost",
                        message: "the physical radio ended without a command result".into(),
                    }),
                )),
            }
        }
        let mut encoded = Vec::with_capacity(finished.len());
        for (operation_id, result) in finished {
            let Some(pending) = self.pending_end_caller.remove(&operation_id) else {
                continue;
            };
            let frame = match result {
                Ok(()) => {
                    // The physical caller is gone. Retire the exact talk lease
                    // immediately so neither a heartbeat nor a stale end-call
                    // challenge can preserve authority after CHUP succeeded.
                    self.revoke_relay_lease_by_id(
                        &pending.execute.lease_id,
                        "caller_ended_by_owner",
                        media,
                    );
                    self.retire_relay_lease(&pending.execute.lease_id);
                    self.relay_end_caller_challenges
                        .retain(|_, challenge| challenge.lease_jti != pending.execute.lease_jti);
                    end_caller_result(&pending.execute, EndCallerOutcome::Completed, None)
                }
                Err(error) => end_caller_result(
                    &pending.execute,
                    EndCallerOutcome::Failed,
                    Some((error.code, error.message.as_str())),
                ),
            };
            frame
                .validate()
                .map_err(|_| WorkerError::reconnect("Companion caller-ending result is invalid"))?;
            encoded.push(serde_json::to_string(&frame).map_err(|_| {
                WorkerError::reconnect("Companion caller-ending result could not be encoded")
            })?);
        }
        Ok(encoded)
    }

    pub(super) fn handle_claim_proposal(
        &mut self,
        notice: LeaseNotice,
        _media: &RemoteMediaHandle,
        radio: &RadioHandle,
        expected_switchboard_revision: Option<u64>,
    ) -> Result<Vec<String>, WorkerError> {
        self.validate_notice(&notice, radio)?;
        if radio.switch_in_flight()
            || expected_switchboard_revision
                .is_some_and(|revision| revision != radio.switchboard_revision())
        {
            return Err(WorkerError::reconnect(
                "Companion claim proposal crossed a physical switchboard transition",
            ));
        }
        let request_id = notice
            .request_id
            .clone()
            .ok_or_else(|| WorkerError::reconnect("Companion claim proposal omitted requestId"))?;
        match notice.lease.mode {
            LeaseMode::Consult if notice.lease.phase == LeasePhase::Prepared => {
                let remote = radio
                    .remote_media()
                    .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
                    .snapshot();
                let assistance = self.assistance.pending_frame(&self.app_id).ok_or_else(|| {
                    WorkerError::reconnect(
                        "Private consultation requires a current Aokie assistance request",
                    )
                })?;
                if !remote.consent.consult_enabled
                    || assistance.call_id != notice.lease.call_id
                    || assistance.call_epoch != notice.lease.call_epoch
                    || assistance.owner_epoch != notice.lease.owner_epoch
                    || assistance.switchboard_revision != radio.switchboard_revision()
                    || assistance.remote_revision != remote.remote_revision
                {
                    return Err(WorkerError::reconnect(
                        "Private consultation does not match the active assistance fence",
                    ));
                }
            }
            LeaseMode::Takeover if notice.lease.phase == LeasePhase::Prepared => {}
            _ => {
                return Err(WorkerError::reconnect(
                    "Companion claim proposal has an invalid lease phase",
                ))
            }
        }
        if self.prepared.is_some() {
            return Err(WorkerError::reconnect(
                "Companion gateway proposed a second consult/takeover claimant",
            ));
        }
        if let Some(transfer_request_id) = notice.accepted_transfer_request_id.as_deref() {
            if notice.lease.mode != LeaseMode::Takeover
                || notice.lease.phase != LeasePhase::Prepared
            {
                return Err(WorkerError::reconnect(
                    "A request-bound transfer must use a prepared takeover lease",
                ));
            }
            let remote = radio
                .remote_media()
                .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
                .snapshot();
            let transfer = self
                .assistance
                .pending_transfer(&notice.lease.call_id, notice.lease.call_epoch)
                .ok_or_else(|| {
                    WorkerError::reconnect("The accepted transfer request is no longer pending")
                })?;
            if transfer.request_id != transfer_request_id
                || transfer.fence.call_id != notice.lease.call_id
                || transfer.fence.call_epoch != notice.lease.call_epoch
                || transfer.fence.owner_epoch != notice.lease.owner_epoch
                || transfer.fence.switchboard_revision != radio.switchboard_revision()
                || transfer.fence.remote_revision != remote.remote_revision
                || transfer
                    .accepted_by
                    .as_deref()
                    .is_some_and(|accepted| accepted != notice.device_id)
            {
                return Err(WorkerError::reconnect(
                    "The accepted transfer crossed its exact assistance fence",
                ));
            }
            self.assistance
                .accept_transfer(&transfer.request_id, &transfer.fence, &notice.device_id)
                .map_err(|_| {
                    WorkerError::reconnect("Another owner endpoint accepted the transfer first")
                })?;
            let accepted = self
                .assistance
                .pending_transfer(&notice.lease.call_id, notice.lease.call_epoch)
                .filter(|accepted| {
                    accepted.request_id == transfer.request_id
                        && accepted.fence == transfer.fence
                        && accepted.accepted_by.as_deref() == Some(notice.device_id.as_str())
                })
                .ok_or_else(|| {
                    WorkerError::reconnect(
                        "The transfer acceptance did not produce a bounded setup window",
                    )
                })?;
            self.accepted_transfers.insert(
                notice.lease.lease_id.clone(),
                AcceptedTransferLease {
                    request_id: transfer.request_id,
                    offered_fence: transfer.fence,
                    device_id: notice.device_id.clone(),
                    setup_expires_at: accepted.expires_at,
                    failback_requested: false,
                },
            );
        }
        self.leases
            .insert(notice.lease.jti.clone(), notice.lease.clone());
        self.prepared = Some(PreparedTakeover {
            request_id,
            provisional: notice.lease,
            expected_switchboard_revision,
            confirmed_owner_epoch: None,
            provisional_sdp_revision: 0,
            provisional_transport_generation: 0,
            decision_sent: false,
            active_rebind_deadline: None,
        });
        if let Some(prepared) = self.prepared.as_ref() {
            eprintln!(
                "[aokie-plugin][takeover] stage=claim_proposed app={} device={} call={} mode={:?} fence={} lease_jti={} rtc={}",
                self.app_id,
                prepared.provisional.device_id,
                prepared.provisional.call_id,
                prepared.provisional.mode,
                prepared.provisional.fence,
                prepared.provisional.jti,
                prepared.provisional.rtc_session_id
            );
        }
        Ok(Vec::new())
    }

    pub(super) fn handle_lease_granted(
        &mut self,
        notice: LeaseNotice,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        self.validate_notice(&notice, radio)?;
        if notice.lease.mode != LeaseMode::Monitor || notice.lease.phase != LeasePhase::Active {
            return Err(WorkerError::reconnect(
                "Only active monitor leases may be granted directly",
            ));
        }
        self.leases.insert(notice.lease.jti.clone(), notice.lease);
        Ok(())
    }

    pub(super) fn handle_lease_renewed(
        &mut self,
        notice: LeaseNotice,
        media: &RemoteMediaHandle,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        self.validate_notice(&notice, radio)?;
        let stable_id = notice.lease.lease_id.clone();
        self.leases.retain(|_, claims| claims.lease_id != stable_id);
        self.leases
            .insert(notice.lease.jti.clone(), notice.lease.clone());
        if let Some(route) = self
            .peers
            .values_mut()
            .find(|route| route.binding.lease_id.as_deref() == Some(stable_id.as_str()))
        {
            route.lease_jti = notice.lease.jti.clone();
            route.lease_ttl_ms = lease_ttl_ms(&notice.lease)?;
            media
                .renew_lease(route.binding.clone(), route.lease_ttl_ms)
                .map_err(|_| WorkerError::reconnect("Companion media lease renewal failed"))?;
        } else if let Some(prepared) = self
            .prepared
            .as_ref()
            .filter(|prepared| prepared.provisional.lease_id == stable_id)
        {
            let binding = binding_for_claims(&notice.lease);
            let _ = prepared;
            media
                .renew_lease(binding, lease_ttl_ms(&notice.lease)?)
                .map_err(|_| WorkerError::reconnect("Prepared media lease renewal failed"))?;
        }
        Ok(())
    }

    pub(super) fn validate_notice(
        &self,
        notice: &LeaseNotice,
        radio: &RadioHandle,
    ) -> Result<(), WorkerError> {
        let _ = (&notice.kind, notice.schema_version);
        if notice.app_id != self.app_id
            || notice.device_id != notice.lease.device_id
            || notice.lease.app_id != self.app_id
            || notice.lease.plugin_id != self.plugin_id
            || notice.lease.plugin_key_thumbprint != self.endpoint_authority.endpoint_key.thumbprint
            || !self
                .endpoint_authority
                .approved_mobile_keys
                .contains_key(&notice.lease.mobile_key_thumbprint)
            || notice.lease_token.is_empty()
            || notice.lease_token.len() > MAX_LEASE_TOKEN_BYTES
        {
            return Err(WorkerError::rebootstrap(
                "Companion lease notice identity is invalid",
            ));
        }
        let now = unix_now()?;
        notice
            .lease
            .validate(now)
            .map_err(|_| WorkerError::reconnect("Companion lease claims are invalid"))?;
        if notice.lease.expires_at > now.saturating_add(300) {
            return Err(WorkerError::reconnect(
                "Companion lease lifetime exceeds the local safety limit",
            ));
        }
        let remote = radio
            .remote_media()
            .ok_or_else(|| WorkerError::reconnect("Companion media is unavailable"))?
            .snapshot();
        if remote.call_id.as_deref() != Some(notice.lease.call_id.as_str())
            || remote.call_epoch != notice.lease.call_epoch
            || remote.owner_epoch != notice.lease.owner_epoch
        {
            return Err(WorkerError::reconnect(
                "Companion lease does not match physical call epochs",
            ));
        }
        Ok(())
    }
}
