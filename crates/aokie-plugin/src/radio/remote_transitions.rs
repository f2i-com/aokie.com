//! Extracted from `run_loop` (mechanical carve-up 2026-07-21): `service_remote_media_transitions`.

#[allow(unused_imports)]
use super::*;

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "voice"), allow(unused_variables))]
pub(super) fn service_remote_media_transitions(
    bt: &mut dyn crate::backend::RadioBackend,
    status: &Arc<RadioStatus>,
    remote_media: &crate::remote_media::RemoteMediaHandle,
    tracker: &mut crate::call_session::SessionTracker,
    ctx: &mut CallVoiceContext,
    #[cfg(feature = "voice")] synth: &crate::synth::SynthHandle,
    #[cfg(feature = "voice")] stt_buf: &mut Vec<f32>,
    #[cfg(feature = "voice")] stt_had_speech: &mut bool,
    #[cfg(feature = "voice")] stt_silence: &mut std::time::Duration,
    #[cfg(feature = "voice")] aec: &mut Option<crate::aec::EchoCanceller>,
) {
    // Companion media follows PHYSICAL truth, not a host/browser
    // snapshot. A takeover becomes human_active only after this thread
    // verifies the same active call and flushes the SCO TX tail.
    let physical_active = tracker.current().is_some_and(|call| call.is_active());
    remote_media.observe_physical_call(tracker.call_id(), physical_active);
    if let Some(transition) = remote_media.next_radio_transition() {
        match transition {
            crate::remote_media::RadioTransition::PrepareHuman { binding } => {
                let same_call = tracker
                    .current()
                    .is_some_and(|call| call.is_active() && call.id == binding.call_id);
                let sample_rate = bt.get_sample_rate();
                eprintln!(
                    "[aokie-plugin][takeover] stage=radio_prepare call={} same_call={} sample_rate={} owner_epoch={} fence={} rtc={}",
                    binding.call_id,
                    same_call,
                    sample_rate,
                    binding.owner_epoch,
                    binding.fence,
                    binding.rtc_session_id
                );
                if same_call && sample_rate > 0 {
                    // Receive-only preparation advances the ownership
                    // epoch but deliberately leaves Aokie speaking. A
                    // fresh active peer must deliver binding-exact
                    // microphone PCM before the final EnterHuman arm is
                    // even requested; only that final arm flushes TX.
                    if let Err(error) = remote_media.ack_prepare_human(&binding) {
                        eprintln!(
                            "[aokie-plugin] Companion soft-hold physical ACK refused: {error}"
                        );
                        let _ = remote_media.revoke(&binding, "physical_prepare_failed");
                    } else {
                        eprintln!(
                            "[aokie-plugin][takeover] stage=radio_prepare_acked call={} fence={} rtc={}",
                            binding.call_id, binding.fence, binding.rtc_session_id
                        );
                    }
                } else {
                    let _ = remote_media.revoke(
                        &binding,
                        if same_call {
                            "sco_unavailable"
                        } else {
                            "physical_call_changed"
                        },
                    );
                }
            }
            crate::remote_media::RadioTransition::PrepareConsult { binding } => {
                let same_call = tracker
                    .current()
                    .is_some_and(|call| call.is_active() && call.id == binding.call_id);
                #[cfg(feature = "voice")]
                let assistance_fence = ctx
                    .pending_assistance
                    .as_ref()
                    .map(|pending| pending.fence.clone());
                #[cfg(not(feature = "voice"))]
                let assistance_fence: Option<
                    crate::assistance::AssistanceCallFence,
                > = None;
                if same_call && bt.get_sample_rate() > 0 && assistance_fence.is_some() {
                    // This ACK only rotates the owner fence so an active
                    // bidirectional lease can be minted. Aokie remains on
                    // the caller until that exact peer proves microphone
                    // PCM; no speech is cancelled or flushed here.
                    if let Err(error) = remote_media.ack_prepare_consult(&binding) {
                        eprintln!(
                            "[aokie-plugin] Companion consult prepare ACK refused: {error}"
                        );
                        let _ = remote_media.revoke(&binding, "consult_prepare_failed");
                    }
                } else {
                    let _ = remote_media.revoke(
                        &binding,
                        if !same_call {
                            "physical_call_changed"
                        } else if bt.get_sample_rate() == 0 {
                            "sco_unavailable"
                        } else {
                            "consult_has_no_assistance_request"
                        },
                    );
                }
            }
            crate::remote_media::RadioTransition::EnterConsult { binding } => {
                let same_call = tracker
                    .current()
                    .is_some_and(|call| call.is_active() && call.id == binding.call_id);
                #[cfg(feature = "voice")]
                let assistance_fence = ctx
                    .pending_assistance
                    .as_ref()
                    .map(|pending| pending.fence.clone());
                #[cfg(not(feature = "voice"))]
                let assistance_fence: Option<
                    crate::assistance::AssistanceCallFence,
                > = None;
                if same_call && bt.get_sample_rate() > 0 && assistance_fence.is_some() {
                    // Final isolation is downstream of exact decoded app
                    // microphone PCM. Flush the Aokie tail atomically on
                    // the radio thread, then open only this consult fence.
                    bt.flush_tx_audio();
                    #[cfg(feature = "voice")]
                    {
                        synth.cancel();
                        *aec = None;
                        stt_buf.clear();
                        *stt_had_speech = false;
                        *stt_silence = Duration::ZERO;
                    }
                    if let Err(error) = remote_media.ack_enter_consult(&binding) {
                        eprintln!(
                            "[aokie-plugin] Companion consult enter ACK refused: {error}"
                        );
                        let _ = remote_media.revoke(&binding, "consult_enter_failed");
                    } else {
                        #[cfg(feature = "voice")]
                        {
                            let remote = remote_media.snapshot();
                            let previous = assistance_fence.expect("checked above");
                            let current = crate::assistance::AssistanceCallFence {
                                call_id: binding.call_id.clone(),
                                call_epoch: remote.call_epoch,
                                owner_epoch: remote.owner_epoch,
                                switchboard_revision: status
                                    .switchboard_revision
                                    .load(Ordering::Relaxed),
                                remote_revision: remote.remote_revision,
                            };
                            match crate::assistance::global()
                                .begin_voice_consult(&previous, current.clone())
                            {
                                Ok(_) => {
                                    if let Some(pending) = ctx.pending_assistance.as_mut() {
                                        pending.fence = current;
                                    }
                                }
                                Err(error) => {
                                    eprintln!(
                                        "[aokie-plugin] private consult assistance fence refused: {error}"
                                    );
                                    let _ = remote_media
                                        .revoke(&binding, "consult_assistance_stale");
                                }
                            }
                        }
                    }
                } else {
                    let _ = remote_media.revoke(
                        &binding,
                        if !same_call {
                            "physical_call_changed"
                        } else if bt.get_sample_rate() == 0 {
                            "sco_unavailable"
                        } else {
                            "consult_has_no_assistance_request"
                        },
                    );
                }
            }
            crate::remote_media::RadioTransition::EnterHuman { binding } => {
                let same_call = tracker
                    .current()
                    .is_some_and(|call| call.is_active() && call.id == binding.call_id);
                let sample_rate = bt.get_sample_rate();
                eprintln!(
                    "[aokie-plugin][takeover] stage=radio_enter call={} same_call={} sample_rate={} owner_epoch={} fence={} rtc={}",
                    binding.call_id,
                    same_call,
                    sample_rate,
                    binding.owner_epoch,
                    binding.fence,
                    binding.rtc_session_id
                );
                if same_call && sample_rate > 0 {
                    // Pending state already blocks every Aokie/TTS TX
                    // chokepoint. Remove its previously queued tail, then
                    // open the exact permit/fence.
                    bt.flush_tx_audio();
                    if let Err(error) = remote_media.ack_enter_human(&binding) {
                        eprintln!(
                            "[aokie-plugin] Companion takeover physical ACK refused: {error}"
                        );
                        let _ = remote_media.revoke(&binding, "physical_ack_failed");
                    } else {
                        eprintln!(
                            "[aokie-plugin][takeover] stage=radio_enter_acked call={} fence={} rtc={}",
                            binding.call_id, binding.fence, binding.rtc_session_id
                        );
                    }
                } else {
                    let _ = remote_media.revoke(
                        &binding,
                        if same_call {
                            "sco_unavailable"
                        } else {
                            "physical_call_changed"
                        },
                    );
                }
            }
            crate::remote_media::RadioTransition::ReturnToAokie { reason } => {
                #[cfg(feature = "voice")]
                {
                    synth.cancel();
                    *aec = None;
                    stt_buf.clear();
                    *stt_had_speech = false;
                    *stt_silence = Duration::ZERO;
                }
                bt.flush_tx_audio();
                if let Err(error) = remote_media.ack_return_to_aokie() {
                    eprintln!("[aokie-plugin] Companion return ACK failed ({reason}): {error}");
                }
            }
        }
    }
}
