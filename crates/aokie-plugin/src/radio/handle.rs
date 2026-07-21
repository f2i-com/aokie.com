//! `RadioHandle` (the control-plane handle) and the companion end-caller machinery.

#[allow(unused_imports)]
use super::*;

/// Handle held by the [`Plugin`](crate::connector::Plugin): send control
/// requests and read live status. Dropping it (process shutdown) drops the
/// `control_tx`, which ends the radio loop and shuts the runtime down.
#[derive(Clone)]
pub struct RadioHandle {
    pub(super) control_tx: Sender<RadioControl>,
    pub status: Arc<RadioStatus>,
    pub(super) remote_media: Option<crate::remote_media::RemoteMediaHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionEndCallerRequest {
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
    pub device_id: String,
    pub lease_id: String,
    pub fence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionEndCallerFailure {
    pub code: &'static str,
    pub message: String,
}

#[cfg(target_os = "windows")]
pub(super) const COMPANION_END_CALL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(target_os = "windows")]
pub(super) struct PendingCompanionEndCaller {
    pub(super) request: CompanionEndCallerRequest,
    pub(super) reply: Sender<Result<(), CompanionEndCallerFailure>>,
    pub(super) deadline: Instant,
}

#[cfg(target_os = "windows")]
pub(super) fn validate_companion_end_caller(
    request: &CompanionEndCallerRequest,
    tracker: &crate::call_session::SessionTracker,
    status: &RadioStatus,
) -> Result<(), CompanionEndCallerFailure> {
    let physical_call = status.current_call_id.lock().unwrap().clone();
    if !status.call_active.load(Ordering::Acquire)
        || physical_call.as_deref() != Some(request.call_id.as_str())
        || !tracker
            .current()
            .is_some_and(|call| call.is_active() && call.id == request.call_id)
    {
        return Err(CompanionEndCallerFailure {
            code: "physical_call_stale",
            message: "the exact cellular call is no longer active".into(),
        });
    }
    if status.switchboard_revision.load(Ordering::Acquire) != request.switchboard_revision
        || status.switch_in_flight.lock().unwrap().is_some()
    {
        return Err(CompanionEndCallerFailure {
            code: "switchboard_stale",
            message: "the physical switchboard changed or is switching".into(),
        });
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub(super) fn start_companion_end_caller<F>(
    request: CompanionEndCallerRequest,
    reply: Sender<Result<(), CompanionEndCallerFailure>>,
    tracker: &mut crate::call_session::SessionTracker,
    status: &RadioStatus,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    pending: &mut Option<PendingCompanionEndCaller>,
    now: Instant,
    enqueue_hangup: F,
) -> bool
where
    F: FnOnce() -> Result<(), String>,
{
    if pending.is_some() {
        let _ = reply.send(Err(CompanionEndCallerFailure {
            code: "end_caller_pending",
            message: "another caller-ending command is still awaiting physical proof".into(),
        }));
        return false;
    }
    let result = validate_companion_end_caller(&request, tracker, status).and_then(|()| {
        remote_media
            .with_active_talk_owner(
                &request.call_id,
                request.call_epoch,
                request.owner_epoch,
                request.remote_revision,
                &request.device_id,
                &request.lease_id,
                request.fence,
                enqueue_hangup,
            )
            .map_err(|message| CompanionEndCallerFailure {
                code: "remote_owner_stale",
                message,
            })?
            .map_err(|error| CompanionEndCallerFailure {
                code: "radio_hangup_failed",
                message: error,
            })
    });
    match result {
        Ok(()) => {
            tracker.note_intent(crate::call_session::TerminationIntent::OperatorHangup);
            *pending = Some(PendingCompanionEndCaller {
                request,
                reply,
                deadline: now + COMPANION_END_CALL_CONFIRM_TIMEOUT,
            });
            true
        }
        Err(error) => {
            return_companion_end_caller_to_aokie(&request, remote_media, error.code);
            let _ = reply.send(Err(error));
            false
        }
    }
}

#[cfg(target_os = "windows")]
pub(super) fn perform_companion_end_caller(
    request: CompanionEndCallerRequest,
    reply: Sender<Result<(), CompanionEndCallerFailure>>,
    bt: &mut dyn crate::backend::RadioBackend,
    tracker: &mut crate::call_session::SessionTracker,
    status: &RadioStatus,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    pending: &mut Option<PendingCompanionEndCaller>,
) -> bool {
    start_companion_end_caller(
        request,
        reply,
        tracker,
        status,
        remote_media,
        pending,
        Instant::now(),
        || {
            bt.flush_tx_audio();
            bt.hangup()
        },
    )
}

#[cfg(target_os = "windows")]
pub(super) fn complete_companion_end_caller(
    pending: &mut Option<PendingCompanionEndCaller>,
    terminated_call_id: &str,
) {
    if !pending
        .as_ref()
        .is_some_and(|operation| operation.request.call_id == terminated_call_id)
    {
        return;
    }
    if let Some(operation) = pending.take() {
        let _ = operation.reply.send(Ok(()));
    }
}

#[cfg(target_os = "windows")]
pub(super) fn resolve_companion_end_caller_termination(
    pending: &mut Option<PendingCompanionEndCaller>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    terminated_call_id: &str,
    physical_link_connected: bool,
) {
    if !pending
        .as_ref()
        .is_some_and(|operation| operation.request.call_id == terminated_call_id)
    {
        return;
    }
    if !physical_link_connected {
        fail_companion_end_caller(
            pending,
            remote_media,
            "physical_proof_lost",
            "the Desktop/device link was lost, so cellular termination was not proven",
        );
        return;
    }
    complete_companion_end_caller(pending, terminated_call_id);
}

#[cfg(target_os = "windows")]
pub(super) fn fail_companion_end_caller(
    pending: &mut Option<PendingCompanionEndCaller>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    code: &'static str,
    message: impl Into<String>,
) {
    let Some(operation) = pending.take() else {
        return;
    };
    return_companion_end_caller_to_aokie(&operation.request, remote_media, code);
    let _ = operation.reply.send(Err(CompanionEndCallerFailure {
        code,
        message: message.into(),
    }));
}

#[cfg(target_os = "windows")]
pub(super) fn poll_companion_end_caller(
    pending: &mut Option<PendingCompanionEndCaller>,
    tracker: &crate::call_session::SessionTracker,
    status: &RadioStatus,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    now: Instant,
) {
    let Some(operation) = pending.as_ref() else {
        return;
    };
    let target = operation.request.call_id.as_str();
    let tracker_call = tracker.call_id();
    let status_call = status.current_call_id.lock().unwrap().clone();
    let physical_active = status.call_active.load(Ordering::Acquire);

    if !physical_active && tracker_call != Some(target) && status_call.as_deref() != Some(target) {
        fail_companion_end_caller(
            pending,
            remote_media,
            "physical_proof_lost",
            "the Desktop/device link lost call state before cellular termination was proven",
        );
        return;
    }
    if tracker_call != Some(target) || status_call.as_deref() != Some(target) {
        fail_companion_end_caller(
            pending,
            remote_media,
            "physical_call_changed",
            "the exact cellular call changed before hangup was confirmed",
        );
        return;
    }
    let remote = remote_media.snapshot();
    if remote.call_id.as_deref() != Some(target)
        || remote.call_epoch != operation.request.call_epoch
        || remote.owner_epoch != operation.request.owner_epoch
        || remote.service_mode != crate::remote_media::ServiceMode::HumanActive
        || remote.talk_device_id.as_deref() != Some(operation.request.device_id.as_str())
        || remote.talk_lease_id.as_deref() != Some(operation.request.lease_id.as_str())
        || remote.talk_fence != operation.request.fence
    {
        fail_companion_end_caller(
            pending,
            remote_media,
            "remote_owner_stale",
            "the Companion returned or changed before physical hangup was confirmed",
        );
        return;
    }
    if now >= operation.deadline {
        fail_companion_end_caller(
            pending,
            remote_media,
            "physical_hangup_timeout",
            "the cellular call did not terminate before the confirmation deadline",
        );
    }
}

#[cfg(target_os = "windows")]
pub(super) fn return_companion_end_caller_to_aokie(
    request: &CompanionEndCallerRequest,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    reason: &str,
) {
    let Some(binding) = remote_media.active_talk_binding() else {
        return;
    };
    if binding.call_id == request.call_id
        && binding.call_epoch == request.call_epoch
        && binding.owner_epoch == request.owner_epoch
        && binding.device_id == request.device_id
        && binding.lease_id.as_deref() == Some(request.lease_id.as_str())
        && binding.fence == request.fence
    {
        let _ = remote_media.revoke(&binding, reason);
    }
}

impl RadioHandle {
    /// Test-only: a handle wired to a bare channel (no radio thread) so
    /// connector tests can exercise the radio-backed command paths —
    /// acceptance results, operation ids and the agent-owns-replies refusal.
    #[cfg(test)]
    pub fn test_handle() -> (RadioHandle, std::sync::mpsc::Receiver<RadioControl>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (
            RadioHandle {
                control_tx: tx,
                status: Arc::new(RadioStatus::default()),
                remote_media: None,
            },
            rx,
        )
    }

    /// Test-only: [`Self::test_handle`] with a media endpoint attached, plus
    /// the status the caller can drive to stage a live call.
    ///
    /// Anything that mints media authority reads physical truth back through
    /// `remote_media()` and `current_call_id()`, so a handle without both can
    /// only ever exercise the refusal paths.
    #[cfg(test)]
    pub fn test_handle_with_media(
        media: crate::remote_media::RemoteMediaHandle,
    ) -> (
        RadioHandle,
        std::sync::mpsc::Receiver<RadioControl>,
        Arc<RadioStatus>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let status = Arc::new(RadioStatus::default());
        (
            RadioHandle {
                control_tx: tx,
                status: status.clone(),
                remote_media: Some(media),
            },
            rx,
            status,
        )
    }

    pub fn send(&self, c: RadioControl) -> Result<(), String> {
        self.control_tx
            .send(c)
            .map_err(|_| "the radio thread is not running".to_string())
    }

    /// Stop the radio only after its terminal transcript barrier has been
    /// flushed. The bound is longer than the radio's slowest normal control
    /// round-trip, but still prevents a wedged driver from hanging the plugin
    /// RPC thread forever.
    pub fn shutdown_and_wait(&self) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::Shutdown {
            completion: Some(tx),
        })?;
        rx.recv_timeout(std::time::Duration::from_secs(15))
            .map_err(|_| {
                "the radio did not complete graceful shutdown within 15 seconds".to_string()
            })
    }

    pub fn is_initialized(&self) -> bool {
        self.status.initialized.load(Ordering::Relaxed)
    }
    pub fn is_connected(&self) -> bool {
        self.status.connected.load(Ordering::Relaxed)
    }
    pub fn is_call_active(&self) -> bool {
        self.status.call_active.load(Ordering::Relaxed)
    }
    /// Native Companion endpoint owned by this radio. It is absent only on
    /// test handles that do not run the physical/audio thread.
    pub fn remote_media(&self) -> Option<&crate::remote_media::RemoteMediaHandle> {
        self.remote_media.as_ref()
    }
    pub fn remote_media_reserved(&self) -> bool {
        self.remote_media
            .as_ref()
            .is_some_and(crate::remote_media::RemoteMediaHandle::radio_reserved)
    }
    pub fn local_address(&self) -> Option<String> {
        self.status.local_address.lock().unwrap().clone()
    }
    pub fn connected_address(&self) -> Option<String> {
        self.status.connected_address.lock().unwrap().clone()
    }
    pub fn current_caller(&self) -> Option<String> {
        self.status.current_caller.lock().unwrap().clone()
    }
    pub fn current_call_id(&self) -> Option<String> {
        self.status.current_call_id.lock().unwrap().clone()
    }
    pub fn call_started_at(&self) -> Option<String> {
        self.status.call_started_at.lock().unwrap().clone()
    }
    pub fn paired(&self) -> Vec<PairedDevice> {
        self.status.paired.lock().unwrap().clone()
    }
    pub fn last_error(&self) -> Option<String> {
        self.status.last_error.lock().unwrap().clone()
    }
    pub fn stale_stt_results(&self) -> u64 {
        self.status.stale_stt_results.load(Ordering::Relaxed)
    }
    /// AOK-VOICE-001: the last known STT failure (None = no known failure).
    pub fn stt_error(&self) -> Option<String> {
        self.status.stt_error.lock().unwrap().clone()
    }
    /// AOK-VOICE-001: the last known TTS failure (None = no known failure).
    pub fn tts_error(&self) -> Option<String> {
        self.status.tts_error.lock().unwrap().clone()
    }
    /// PROC-001: the last known LLM-reachability failure (None = reachable or
    /// not probed — the probe only runs while the in-plugin agent owns replies).
    pub fn llm_error(&self) -> Option<String> {
        self.status.llm_error.lock().unwrap().clone()
    }
    pub fn realtime_selected(&self) -> bool {
        self.status.realtime_selected.load(Ordering::Relaxed)
    }
    pub fn realtime_ready(&self) -> bool {
        self.status.realtime_ready.load(Ordering::Relaxed)
    }
    pub fn realtime_destination(&self) -> Option<String> {
        self.status.realtime_destination.lock().unwrap().clone()
    }
    pub fn realtime_error(&self) -> Option<String> {
        self.status.realtime_error.lock().unwrap().clone()
    }
    /// VOICE-001: the loopback self-test outcome (None = still running).
    pub fn self_test(&self) -> Option<VoiceSelfTest> {
        self.status.self_test.lock().unwrap().clone()
    }

    /// Round 4: content-free duplex/floor counters — how often each
    /// full-duplex mechanism fired, for dongle.diagnostics tuning.
    pub fn duplex_counters(&self) -> serde_json::Value {
        let s = &self.status;
        json!({
            "earlySttHits": s.early_stt_hits.load(Ordering::Relaxed),
            "probesSent": s.probes_sent.load(Ordering::Relaxed),
            "probeCommands": s.probe_commands.load(Ordering::Relaxed),
            "boundaryYields": s.boundary_yields.load(Ordering::Relaxed),
            "midSpanYields": s.mid_span_yields.load(Ordering::Relaxed),
            "specLlmStarted": s.spec_llm_started.load(Ordering::Relaxed),
            "specLlmKept": s.spec_llm_kept.load(Ordering::Relaxed),
            "specLlmWasted": s.spec_llm_wasted.load(Ordering::Relaxed),
            "gapYields": s.gap_yields.load(Ordering::Relaxed),
            "semanticCuts": s.semantic_cuts.load(Ordering::Relaxed),
            "bargeCuts": s.barge_cuts.load(Ordering::Relaxed),
            "floorShadowCuts": s.floor_shadow_cuts.load(Ordering::Relaxed),
            "floorShadowYields": s.floor_shadow_yields.load(Ordering::Relaxed),
            "floorShadowDucks": s.floor_shadow_ducks.load(Ordering::Relaxed),
            "floorShadowDivergences": s.floor_shadow_divergences.load(Ordering::Relaxed),
        })
    }

    /// Phase 4 observe lane: waiting episodes seen this radio session, the
    /// last callheld indicator state and the most recent AT+CLCC snapshot
    /// — dongle.diagnostics visibility (the log ring wraps in ~1 min under
    /// call load; this survives).
    pub fn call_waiting_diagnostics(&self) -> serde_json::Value {
        let last_clcc =
            self.status
                .clcc_snapshot
                .lock()
                .unwrap()
                .as_ref()
                .map(|(started, lines)| {
                    json!({
                        "ageSecs": started.elapsed().as_secs(),
                        "entries": lines,
                    })
                });
        json!({
            "episodes": self.status.call_waiting_episodes.load(Ordering::Relaxed),
            "callHeldState": self.status.call_held_state.load(Ordering::Relaxed),
            "lastClcc": last_clcc,
        })
    }

    /// Phase 4 switchboard mirrors — the connector's validation view.
    pub fn waiting_call(&self) -> Option<SwitchboardLeg> {
        self.status.waiting_call.lock().unwrap().clone()
    }

    pub fn parked_call_leg(&self) -> Option<SwitchboardLeg> {
        self.status.parked_call.lock().unwrap().clone()
    }

    pub fn switchboard_revision(&self) -> u64 {
        self.status.switchboard_revision.load(Ordering::Relaxed)
    }

    /// A CHLD switch marker held until the radio loop explicitly settles or
    /// clears it. Wall-clock age cannot make an in-progress topology safe.
    pub fn switch_in_flight(&self) -> bool {
        self.status.switch_in_flight.lock().unwrap().is_some()
    }

    /// The authoritative `call.switchboard` snapshot: foreground (the
    /// `call.current` shape), waiting + parked legs, revision, transition
    /// state and the callheld indicator.
    pub fn switchboard_view(&self) -> serde_json::Value {
        let leg_json = |leg: &SwitchboardLeg| {
            json!({
                "callId": leg.call_id,
                "from": leg.from,
                "since": leg.since_iso,
            })
        };
        let foreground = self.current_call_id().map(|call_id| {
            json!({
                "callId": call_id,
                "from": self.current_caller(),
                "state": if self.is_call_active() {
                    crate::contract::call_state::ACTIVE
                } else {
                    crate::contract::call_state::RINGING
                },
                "startedAt": self.call_started_at(),
            })
        });
        json!({
            "foreground": foreground,
            "waiting": self.waiting_call().as_ref().map(leg_json),
            "parked": self.parked_call_leg().as_ref().map(leg_json),
            "revision": self.switchboard_revision(),
            "switchInProgress": self.switch_in_flight(),
            "callHeldState": self.status.call_held_state.load(Ordering::Relaxed),
        })
    }

    /// AOK-BT-001: seconds left in the pairing window, 0 when closed or the radio
    /// isn't up yet. Lock-free read of the shared window (reflects timeout + a
    /// successful-bond auto-close without polling the radio thread).
    pub fn pairing_window_remaining_secs(&self) -> u64 {
        self.status
            .pairing_window
            .lock()
            .ok()
            .and_then(|w| w.as_ref().map(|w| w.remaining_secs()))
            .unwrap_or(0)
    }

    /// AOK-BT-001: open a bounded, discoverable pairing window for `seconds`.
    pub fn start_pairing(&self, seconds: u64) -> Result<(), String> {
        self.send(RadioControl::StartPairing { seconds })
    }

    /// AOK-BT-001: close the pairing window now.
    pub fn stop_pairing(&self) -> Result<(), String> {
        self.send(RadioControl::StopPairing)
    }

    /// AOK-BT-001: forget a bonded device (blocks briefly on the radio thread).
    pub fn remove_paired(&self, address: String) -> Result<bool, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::RemovePaired { address, reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the removePaired request".to_string())?
    }

    /// Bonded (revocable) devices with their captured friendly names
    /// (address, name) — round-trips the radio thread, which owns the store.
    pub fn list_bonded(&self) -> Result<Vec<(String, Option<String>)>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::ListBonded { reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the listPaired request".to_string())
    }

    /// The connected phone's captured friendly name/model, if known yet.
    /// Round-trips the radio thread (the name lives in the radio runtime).
    pub fn connected_name(&self) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        if self
            .send(RadioControl::ConnectedName { reply: tx })
            .is_err()
        {
            return None;
        }
        rx.recv_timeout(std::time::Duration::from_secs(3))
            .ok()
            .flatten()
    }

    /// Disconnect the connected phone but KEEP the bond (remote reconnect/
    /// unstick). Blocks briefly on the radio thread.
    pub fn disconnect(&self, address: String) -> Result<bool, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::Disconnect { address, reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the disconnect request".to_string())?
    }

    /// HARD-001: reconnect a bonded phone from OUR side (page + outbound HFP
    /// setup). Blocks briefly on the radio thread; true = attempt started.
    pub fn connect(&self, address: String) -> Result<bool, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::Connect { address, reply: tx })?;
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| "the radio did not answer the connect request".to_string())?
    }

    /// PAIR-001: the held SSP numeric comparison awaiting the operator, if
    /// any (lock-free slot read; expired prompts read as None).
    pub fn pending_pairing_confirm(
        &self,
    ) -> Option<aokie_dongle::bluetooth::PendingPairingConfirm> {
        self.status
            .pairing_confirm
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|s| s.get()))
    }

    /// PAIR-001: resolve the held SSP numeric comparison (blocks briefly on
    /// the radio thread, which owns the HCI transport).
    pub fn confirm_pairing(&self, address: String, accept: bool) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(RadioControl::ConfirmPairing {
            address,
            accept,
            reply: tx,
        })?;
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| "the radio did not answer the confirmPairing request".to_string())?
    }
}
