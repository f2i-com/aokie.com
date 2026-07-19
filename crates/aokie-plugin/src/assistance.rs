//! Volatile, plugin-owned Companion assistance mailbox.
//!
//! This is intentionally not the FormLogic command/event plane. Questions
//! and their single accepted answers travel only over authenticated v2
//! signalling and remain in memory for the live receptionist to consume.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use aokie_protocol::v2::{
    AssistanceResponseAction, PluginAssistanceAnswerFrame, PluginAssistanceRequestFrame,
    MAX_ASSISTANCE_ANSWER_BYTES, MAX_ASSISTANCE_CONTEXT_BYTES, MAX_ASSISTANCE_QUESTION_BYTES,
    SCHEMA_VERSION,
};

const MAX_TTL_SECONDS: u64 = 300;
/// A transfer accepted near the end of its ordinary response window gets a
/// separate, bounded media-establishment window. This is longer than the
/// Desktop's normal prepared-peer proof path but still fails back promptly.
pub(crate) const TRANSFER_SETUP_SECONDS: u64 = 45;
/// The radio does not consume an accepted transfer at the exact setup
/// deadline. The gateway gets this short interval to return the exact route to
/// Aokie and publish TransferUnavailable; a dead gateway still converges to an
/// ordinary expiry after the grace expires.
const TRANSFER_RESOLUTION_GRACE_SECONDS: u64 = 10;
const SAFE_TRANSFER_REASON: &str = "Caller requested the owner";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistanceCallFence {
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub switchboard_revision: u64,
    pub remote_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistanceAnswer {
    pub request_id: String,
    pub answer_id: String,
    pub device_id: String,
    pub answer: String,
    /// True only when the answer was transcribed from the exact active,
    /// lease-bound private consult lane.  The radio uses this to allow the
    /// single expected owner-epoch advance when returning from software hold;
    /// ordinary typed answers remain revision-exact.
    pub voice_consult: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistanceIntent {
    Advice,
    Transfer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistanceResolution {
    Answered(AssistanceAnswer),
    Declined {
        device_id: String,
        answer_id: String,
        answer: String,
    },
    TransferTaken {
        device_id: String,
    },
    TransferUnavailable {
        fence: AssistanceCallFence,
    },
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTransfer {
    pub request_id: String,
    pub fence: AssistanceCallFence,
    pub expires_at: u64,
    pub accepted_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceConsultRequest {
    pub request_id: String,
    pub fence: AssistanceCallFence,
    pub question: String,
    pub expires_at: u64,
}

#[derive(Clone, Default)]
pub struct AssistanceBroker {
    inner: Arc<Mutex<Option<PendingAssistance>>>,
}

#[derive(Clone)]
struct PendingAssistance {
    event_id: String,
    request_id: String,
    fence: AssistanceCallFence,
    question: String,
    context: Option<String>,
    expires_at: u64,
    intent: AssistanceIntent,
    accepted_by: Option<String>,
    accepted_setup_expires_at: Option<u64>,
    resolution: Option<AssistanceResolution>,
}

fn transfer_action_deadline(pending: &PendingAssistance) -> u64 {
    pending
        .accepted_setup_expires_at
        .unwrap_or(pending.expires_at)
}

fn mailbox_wait_deadline(pending: &PendingAssistance) -> u64 {
    pending
        .accepted_setup_expires_at
        .map(|deadline| deadline.saturating_add(TRANSFER_RESOLUTION_GRACE_SECONDS))
        .unwrap_or(pending.expires_at)
}

impl AssistanceBroker {
    pub fn request(
        &self,
        fence: AssistanceCallFence,
        question: &str,
        context: Option<&str>,
        ttl_seconds: u64,
    ) -> Result<String, String> {
        self.request_with_intent(
            fence,
            AssistanceIntent::Advice,
            question,
            context,
            ttl_seconds,
        )
    }

    pub fn request_transfer(
        &self,
        fence: AssistanceCallFence,
        _reason: &str,
        context: Option<&str>,
        ttl_seconds: u64,
    ) -> Result<String, String> {
        // Model output is prompt-untrusted and is broadcast to every eligible
        // owner endpoint. The transcript/context already carries useful call
        // detail behind its own grants, so never relay a model-authored name,
        // number, PIN or identifier as control-plane transfer metadata.
        self.request_with_intent(
            fence,
            AssistanceIntent::Transfer,
            SAFE_TRANSFER_REASON,
            context,
            ttl_seconds,
        )
    }

    fn request_with_intent(
        &self,
        fence: AssistanceCallFence,
        intent: AssistanceIntent,
        question: &str,
        context: Option<&str>,
        ttl_seconds: u64,
    ) -> Result<String, String> {
        validate_text(question, MAX_ASSISTANCE_QUESTION_BYTES, "question")?;
        if intent == AssistanceIntent::Transfer
            && (question.len() > crate::speech_plan::MAX_TRANSFER_REASON_BYTES
                || question.contains('[')
                || question.contains(']'))
        {
            return Err("transfer reason is not short control-safe text".into());
        }
        if let Some(context) = context {
            validate_text(context, MAX_ASSISTANCE_CONTEXT_BYTES, "context")?;
        }
        if !(1..=MAX_TTL_SECONDS).contains(&ttl_seconds) {
            return Err("assistance TTL must be 1..300 seconds".into());
        }
        validate_fence(&fence)?;
        let now = unix_now()?;
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "assistance mailbox is unavailable".to_string())?;
        if current.as_ref().is_some_and(|pending| {
            pending.resolution.is_none() && mailbox_wait_deadline(pending) > now
        }) {
            return Err("an assistance request is already pending".into());
        }
        let request_id = format!("assist_{}", uuid::Uuid::new_v4().simple());
        *current = Some(PendingAssistance {
            event_id: format!("assist_event_{}", uuid::Uuid::new_v4().simple()),
            request_id: request_id.clone(),
            fence,
            question: question.trim().to_owned(),
            context: context.map(str::trim).map(str::to_owned),
            expires_at: now + ttl_seconds,
            intent,
            accepted_by: None,
            accepted_setup_expires_at: None,
            resolution: None,
        });
        Ok(request_id)
    }

    pub fn pending_frame(&self, app_id: &str) -> Option<PluginAssistanceRequestFrame> {
        let now = unix_now().ok()?;
        let pending = self.inner.lock().ok()?.clone()?;
        if pending.expires_at <= now || pending.resolution.is_some() {
            return None;
        }
        Some(PluginAssistanceRequestFrame {
            kind: "assistance_request".into(),
            schema_version: SCHEMA_VERSION,
            app_id: app_id.into(),
            event_id: pending.event_id,
            request_id: pending.request_id,
            call_id: pending.fence.call_id,
            call_epoch: pending.fence.call_epoch,
            owner_epoch: pending.fence.owner_epoch,
            switchboard_revision: pending.fence.switchboard_revision,
            remote_revision: pending.fence.remote_revision,
            question: pending.question,
            context: pending.context,
            transfer_offered: pending.intent == AssistanceIntent::Transfer,
            expires_at: pending.expires_at,
        })
    }

    pub fn accept(&self, frame: PluginAssistanceAnswerFrame) -> Result<(), String> {
        frame.validate().map_err(|error| error.to_string())?;
        let now = unix_now()?;
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "assistance mailbox is unavailable".to_string())?;
        let pending = current
            .as_mut()
            .ok_or_else(|| "assistance request is no longer pending".to_string())?;
        if pending.expires_at <= now {
            return Err("assistance request expired".into());
        }
        if pending.request_id != frame.request_id
            || pending.fence.call_id != frame.call_id
            || pending.fence.call_epoch != frame.call_epoch
            || pending.fence.owner_epoch != frame.owner_epoch
            || pending.fence.switchboard_revision != frame.switchboard_revision
            || pending.fence.remote_revision != frame.remote_revision
        {
            return Err("assistance answer crossed call revisions".into());
        }
        if pending.accepted_by.is_some() {
            return Err("transfer acceptance already owns this request".into());
        }
        if pending.intent == AssistanceIntent::Transfer
            && frame.response_action == AssistanceResponseAction::Answer
        {
            return Err("transfer acceptance requires an exact takeover claim".into());
        }
        if let Some(resolution) = &pending.resolution {
            return if resolution_matches_frame(resolution, &frame) {
                Ok(())
            } else {
                Err("assistance request already consumed its one response".into())
            };
        }
        validate_text(&frame.answer, MAX_ASSISTANCE_ANSWER_BYTES, "answer")?;
        pending.resolution = Some(match frame.response_action {
            AssistanceResponseAction::Answer => AssistanceResolution::Answered(AssistanceAnswer {
                request_id: frame.request_id,
                answer_id: frame.answer_id,
                device_id: frame.device_id,
                answer: frame.answer,
                voice_consult: false,
            }),
            AssistanceResponseAction::Decline => AssistanceResolution::Declined {
                device_id: frame.device_id,
                answer_id: frame.answer_id,
                answer: frame.answer,
            },
        });
        Ok(())
    }

    /// Re-fence the one pending request after the radio has physically entered
    /// software hold.  The transition must stay on the same call/switchboard
    /// and strictly advance both local owner and remote revisions.
    pub fn begin_voice_consult(
        &self,
        previous: &AssistanceCallFence,
        current_fence: AssistanceCallFence,
    ) -> Result<VoiceConsultRequest, String> {
        validate_fence(&current_fence)?;
        let now = unix_now()?;
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "assistance mailbox is unavailable".to_string())?;
        let pending = current
            .as_mut()
            .ok_or_else(|| "assistance request is no longer pending".to_string())?;
        if pending.intent != AssistanceIntent::Advice
            || pending.resolution.is_some()
            || pending.accepted_by.is_some()
            || pending.expires_at <= now
            || pending.fence != *previous
        {
            return Err("assistance request changed before private consultation".into());
        }
        if current_fence.call_id != previous.call_id
            || current_fence.call_epoch != previous.call_epoch
            || current_fence.switchboard_revision != previous.switchboard_revision
            || current_fence.owner_epoch <= previous.owner_epoch
            || current_fence.remote_revision <= previous.remote_revision
        {
            return Err("private consultation did not safely advance the call fence".into());
        }
        pending.fence = current_fence.clone();
        Ok(VoiceConsultRequest {
            request_id: pending.request_id.clone(),
            fence: current_fence,
            question: pending.question.clone(),
            expires_at: pending.expires_at,
        })
    }

    pub fn voice_request(&self, request_id: &str) -> Option<VoiceConsultRequest> {
        let now = unix_now().ok()?;
        let pending = self.inner.lock().ok()?.clone()?;
        (pending.request_id == request_id
            && pending.intent == AssistanceIntent::Advice
            && pending.resolution.is_none()
            && pending.accepted_by.is_none()
            && pending.expires_at > now)
            .then(|| VoiceConsultRequest {
                request_id: pending.request_id,
                fence: pending.fence,
                question: pending.question,
                expires_at: pending.expires_at,
            })
    }

    /// Accept one STT transcript from the exact active private lane.  It is
    /// still prompt-untrusted content and cannot carry commands or policy.
    pub fn accept_voice(
        &self,
        request_id: &str,
        fence: &AssistanceCallFence,
        device_id: &str,
        answer: &str,
    ) -> Result<(), String> {
        validate_text(answer, MAX_ASSISTANCE_ANSWER_BYTES, "answer")?;
        let now = unix_now()?;
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "assistance mailbox is unavailable".to_string())?;
        let pending = current
            .as_mut()
            .ok_or_else(|| "assistance request is no longer pending".to_string())?;
        if pending.request_id != request_id
            || pending.fence != *fence
            || pending.intent != AssistanceIntent::Advice
            || pending.expires_at <= now
            || pending.resolution.is_some()
            || pending.accepted_by.is_some()
        {
            return Err("voice consult answer crossed its assistance fence".into());
        }
        validate_device_id(device_id)?;
        pending.resolution = Some(AssistanceResolution::Answered(AssistanceAnswer {
            request_id: request_id.into(),
            answer_id: format!("voice_{}", uuid::Uuid::new_v4().simple()),
            device_id: device_id.into(),
            answer: answer.trim().into(),
            voice_consult: true,
        }));
        Ok(())
    }

    /// Snapshot the exact transfer offered for a call fence. The gateway uses
    /// this to bind an ordinary takeover claim to Aokie's invitation; no media
    /// authority is created by the mailbox itself.
    pub fn pending_transfer(&self, call_id: &str, call_epoch: u64) -> Option<PendingTransfer> {
        let now = unix_now().ok()?;
        let pending = self.inner.lock().ok()?.clone()?;
        let setup_expires_at = transfer_action_deadline(&pending);
        (pending.intent == AssistanceIntent::Transfer
            && pending.fence.call_id == call_id
            && pending.fence.call_epoch == call_epoch
            && pending.resolution.is_none()
            && setup_expires_at > now)
            .then(|| PendingTransfer {
                request_id: pending.request_id,
                fence: pending.fence,
                expires_at: setup_expires_at,
                accepted_by: pending.accepted_by,
            })
    }

    /// Snapshot an already accepted transfer through the short resolution
    /// grace. This is intentionally not actionable media authority: the
    /// returned `expires_at` remains the earlier setup deadline. It only lets a
    /// restarted/failing gateway record TransferUnavailable after exact
    /// AokieActive proof instead of racing the radio's expiry consumer.
    pub fn accepted_transfer(&self, call_id: &str, call_epoch: u64) -> Option<PendingTransfer> {
        let now = unix_now().ok()?;
        let pending = self.inner.lock().ok()?.clone()?;
        (pending.intent == AssistanceIntent::Transfer
            && pending.fence.call_id == call_id
            && pending.fence.call_epoch == call_epoch
            && pending.accepted_by.is_some()
            && pending.resolution.is_none()
            && mailbox_wait_deadline(&pending) > now)
            .then(|| {
                let expires_at = transfer_action_deadline(&pending);
                PendingTransfer {
                    request_id: pending.request_id,
                    fence: pending.fence,
                    expires_at,
                    accepted_by: pending.accepted_by,
                }
            })
    }

    /// Final media-arm predicate for the exact accepted transfer. The remote
    /// media linearization point combines this mailbox proof with its own
    /// monotonic setup deadline before it can enter HumanActive.
    pub fn transfer_activation_is_current(
        &self,
        request_id: &str,
        fence: &AssistanceCallFence,
        device_id: &str,
    ) -> bool {
        let Ok(now) = unix_now() else {
            return false;
        };
        self.inner.lock().ok().is_some_and(|current| {
            current.as_ref().is_some_and(|pending| {
                pending.request_id == request_id
                    && pending.fence == *fence
                    && pending.intent == AssistanceIntent::Transfer
                    && pending.accepted_by.as_deref() == Some(device_id)
                    && pending.resolution.is_none()
                    && transfer_action_deadline(pending) > now
            })
        })
    }

    /// Record the responder that accepted the offered transfer while Aokie
    /// still owns caller audio. Idempotence is deliberately device-exact.
    pub fn accept_transfer(
        &self,
        request_id: &str,
        fence: &AssistanceCallFence,
        device_id: &str,
    ) -> Result<(), String> {
        validate_device_id(device_id)?;
        let setup_expires_at = unix_now()?.saturating_add(TRANSFER_SETUP_SECONDS);
        self.update_transfer(request_id, fence, |pending| {
            if pending.resolution.is_some() {
                return Err("transfer request is already resolved".into());
            }
            if let Some(accepted_by) = &pending.accepted_by {
                return if accepted_by == device_id {
                    Ok(())
                } else {
                    Err("another responder already accepted this transfer".into())
                };
            }
            pending.accepted_by = Some(device_id.to_owned());
            pending.accepted_setup_expires_at = Some(setup_expires_at);
            Ok(())
        })
    }

    /// Release an exact transfer reservation that never reached delivered
    /// provisional media authority. This is deliberately narrower than a
    /// decline: the caller remains with Aokie, the request becomes available
    /// to every eligible owner endpoint again, and a different device can win
    /// only after the failed offer/lease transaction has been retired.
    pub fn release_transfer_acceptance(
        &self,
        request_id: &str,
        fence: &AssistanceCallFence,
        device_id: &str,
    ) -> Result<(), String> {
        validate_device_id(device_id)?;
        self.update_transfer(request_id, fence, |pending| {
            if pending.resolution.is_some() {
                return Err("transfer request is already resolved".into());
            }
            if pending.accepted_by.as_deref() != Some(device_id) {
                return Err("transfer reservation does not match its accepting device".into());
            }
            pending.accepted_by = None;
            pending.accepted_setup_expires_at = None;
            Ok(())
        })
    }

    /// Resolve only the exact accepted offer after the gateway has proved the
    /// same device is HumanActive. The radio consumes this as a silent result:
    /// it must never speak a stale answer into human-owned caller audio.
    pub fn transfer_taken(
        &self,
        request_id: &str,
        fence: &AssistanceCallFence,
        device_id: &str,
    ) -> Result<(), String> {
        validate_device_id(device_id)?;
        self.update_transfer(request_id, fence, |pending| {
            if let Some(resolution) = &pending.resolution {
                return if matches!(
                    resolution,
                    AssistanceResolution::TransferTaken { device_id: completed_by }
                        if completed_by == device_id
                ) {
                    Ok(())
                } else {
                    Err("transfer request is already resolved".into())
                };
            }
            if pending.accepted_by.as_deref() != Some(device_id) {
                return Err("transfer completion does not match its accepting device".into());
            }
            pending.resolution = Some(AssistanceResolution::TransferTaken {
                device_id: device_id.to_owned(),
            });
            Ok(())
        })
    }

    /// Resolve an offered or accepted transfer that can no longer establish
    /// safe media. The caller remains with Aokie and hears the normal
    /// unavailable/message path when the radio consumes this result.
    pub fn transfer_unavailable(
        &self,
        request_id: &str,
        offered_fence: &AssistanceCallFence,
        current_fence: AssistanceCallFence,
        device_id: Option<&str>,
    ) -> Result<(), String> {
        if let Some(device_id) = device_id {
            validate_device_id(device_id)?;
        }
        validate_fence(&current_fence)?;
        if current_fence.call_id != offered_fence.call_id
            || current_fence.call_epoch != offered_fence.call_epoch
            || current_fence.switchboard_revision != offered_fence.switchboard_revision
            || current_fence.owner_epoch < offered_fence.owner_epoch
            || current_fence.remote_revision < offered_fence.remote_revision
        {
            return Err("transfer return did not preserve its call fence".into());
        }
        // Unlike acceptance/completion, failback remains valid after the
        // original offer TTL: a transfer accepted just before expiry may cross
        // that boundary during bounded WebRTC preparation. The gateway must
        // still be able to record the exact returned Aokie fence rather than
        // leave an expired accepted request ambiguously unresolved.
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "assistance mailbox is unavailable".to_string())?;
        let pending = current
            .as_mut()
            .ok_or_else(|| "transfer request is no longer pending".to_string())?;
        if pending.request_id != request_id
            || pending.fence != *offered_fence
            || pending.intent != AssistanceIntent::Transfer
        {
            return Err("transfer request crossed its assistance fence".into());
        }
        if let Some(resolution) = &pending.resolution {
            return if matches!(
                resolution,
                AssistanceResolution::TransferUnavailable { fence }
                    if fence == &current_fence
            ) {
                Ok(())
            } else {
                Err("transfer request is already resolved".into())
            };
        }
        if let Some(accepted_by) = pending.accepted_by.as_deref() {
            if device_id != Some(accepted_by) {
                return Err("transfer failure does not match its accepting device".into());
            }
        }
        pending.resolution = Some(AssistanceResolution::TransferUnavailable {
            fence: current_fence,
        });
        Ok(())
    }

    fn update_transfer(
        &self,
        request_id: &str,
        fence: &AssistanceCallFence,
        update: impl FnOnce(&mut PendingAssistance) -> Result<(), String>,
    ) -> Result<(), String> {
        let now = unix_now()?;
        let mut current = self
            .inner
            .lock()
            .map_err(|_| "assistance mailbox is unavailable".to_string())?;
        let pending = current
            .as_mut()
            .ok_or_else(|| "transfer request is no longer pending".to_string())?;
        if pending.request_id != request_id
            || pending.fence != *fence
            || pending.intent != AssistanceIntent::Transfer
            || transfer_action_deadline(pending) <= now
        {
            return Err("transfer request crossed its assistance fence".into());
        }
        update(pending)
    }

    /// Take one terminal assistance/transfer result exactly once. Expiry is
    /// synthesized under the same lock, so it cannot race a late responder
    /// into both an unavailable announcement and a handoff.
    pub fn take_resolution(&self, request_id: &str) -> Option<AssistanceResolution> {
        let now = unix_now().ok()?;
        let mut current = self.inner.lock().ok()?;
        if current.as_ref()?.request_id != request_id {
            return None;
        }
        let pending = current.as_ref()?;
        if pending.resolution.is_none() && mailbox_wait_deadline(pending) > now {
            return None;
        }
        let pending = current.take()?;
        pending.resolution.or(Some(AssistanceResolution::Expired))
    }

    /// True only while this exact request is still awaiting its one answer.
    /// This lets the radio poll without consuming an unanswered mailbox.
    pub fn is_waiting(&self, request_id: &str) -> bool {
        let Ok(now) = unix_now() else {
            return false;
        };
        self.inner.lock().ok().is_some_and(|current| {
            current.as_ref().is_some_and(|pending| {
                pending.request_id == request_id
                    && pending.resolution.is_none()
                    && mailbox_wait_deadline(pending) > now
            })
        })
    }

    /// Remove this exact mailbox at a call boundary or after timeout. A
    /// mismatched request id is deliberately a no-op.
    pub fn discard(&self, request_id: &str) {
        if let Ok(mut current) = self.inner.lock() {
            if current
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id)
            {
                *current = None;
            }
        }
    }
}

fn resolution_matches_frame(
    resolution: &AssistanceResolution,
    frame: &PluginAssistanceAnswerFrame,
) -> bool {
    match (resolution, frame.response_action) {
        (AssistanceResolution::Answered(answer), AssistanceResponseAction::Answer) => {
            answer.answer_id == frame.answer_id
                && answer.device_id == frame.device_id
                && answer.answer == frame.answer
        }
        (
            AssistanceResolution::Declined {
                device_id,
                answer_id,
                answer,
            },
            AssistanceResponseAction::Decline,
        ) => {
            device_id == &frame.device_id
                && answer_id == &frame.answer_id
                && answer == &frame.answer
        }
        _ => false,
    }
}

fn validate_device_id(device_id: &str) -> Result<(), String> {
    if device_id.is_empty()
        || device_id.len() > 200
        || !device_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err("assistance device identity is invalid".into())
    } else {
        Ok(())
    }
}

fn validate_fence(fence: &AssistanceCallFence) -> Result<(), String> {
    if fence.call_id.is_empty()
        || fence.call_id.len() > 200
        || !fence
            .call_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        || fence.call_epoch == 0
    {
        return Err("assistance call fence is invalid".into());
    }
    Ok(())
}

fn validate_text(value: &str, maximum: usize, name: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(format!("assistance {name} is invalid"))
    } else {
        Ok(())
    }
}

fn unix_now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "system clock is before the Unix epoch".into())
}

pub fn global() -> &'static AssistanceBroker {
    static BROKER: OnceLock<AssistanceBroker> = OnceLock::new();
    BROKER.get_or_init(AssistanceBroker::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence() -> AssistanceCallFence {
        AssistanceCallFence {
            call_id: "call_a".into(),
            call_epoch: 2,
            owner_epoch: 3,
            switchboard_revision: 4,
            remote_revision: 5,
        }
    }

    #[test]
    fn answer_is_revision_fenced_idempotent_and_one_use() {
        let broker = AssistanceBroker::default();
        let request_id = broker
            .request(fence(), "Can we accept it?", None, 30)
            .unwrap();
        let request = broker.pending_frame("app_a").unwrap();
        assert_eq!(request.request_id, request_id);
        let answer = PluginAssistanceAnswerFrame {
            kind: "assistance_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            request_id: request_id.clone(),
            answer_id: "answer_a".into(),
            call_id: "call_a".into(),
            call_epoch: 2,
            owner_epoch: 3,
            switchboard_revision: 4,
            remote_revision: 5,
            response_action: AssistanceResponseAction::Answer,
            answer: "Yes, use the secure drop box.".into(),
        };
        broker.accept(answer.clone()).unwrap();
        broker.accept(answer).unwrap();
        assert!(broker.pending_frame("app_a").is_none());
        assert_eq!(
            broker.take_resolution(&request_id).unwrap(),
            AssistanceResolution::Answered(AssistanceAnswer {
                request_id: request_id.clone(),
                answer_id: "answer_a".into(),
                device_id: "device_a".into(),
                answer: "Yes, use the secure drop box.".into(),
                voice_consult: false,
            })
        );
        assert!(broker.take_resolution(&request_id).is_none());
    }

    #[test]
    fn waiting_and_discard_are_request_scoped() {
        let broker = AssistanceBroker::default();
        let request_id = broker.request(fence(), "Question", None, 30).unwrap();
        assert!(broker.is_waiting(&request_id));
        broker.discard("another_request");
        assert!(broker.is_waiting(&request_id));
        broker.discard(&request_id);
        assert!(!broker.is_waiting(&request_id));
    }

    #[test]
    fn second_or_stale_answer_is_refused() {
        let broker = AssistanceBroker::default();
        let request_id = broker
            .request(fence(), "Question", Some("Context"), 30)
            .unwrap();
        let mut answer = PluginAssistanceAnswerFrame {
            kind: "assistance_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_a".into(),
            request_id,
            answer_id: "answer_a".into(),
            call_id: "call_a".into(),
            call_epoch: 2,
            owner_epoch: 3,
            switchboard_revision: 4,
            remote_revision: 6,
            response_action: AssistanceResponseAction::Answer,
            answer: "No".into(),
        };
        assert!(broker.accept(answer.clone()).is_err());
        answer.remote_revision = 5;
        broker.accept(answer.clone()).unwrap();
        answer.answer_id = "answer_b".into();
        assert!(broker.accept(answer).is_err());
    }

    #[test]
    fn voice_consult_requires_an_exact_advancing_fence_and_is_one_use() {
        let broker = AssistanceBroker::default();
        let previous = fence();
        let request_id = broker
            .request(
                previous.clone(),
                "Which appointment should I offer?",
                None,
                30,
            )
            .unwrap();

        let mut stale = previous.clone();
        stale.remote_revision += 1;
        assert!(
            broker.begin_voice_consult(&previous, stale).is_err(),
            "ownerEpoch must also advance after software hold"
        );

        let mut current = previous.clone();
        current.owner_epoch += 1;
        current.remote_revision += 1;
        let voice = broker
            .begin_voice_consult(&previous, current.clone())
            .unwrap();
        assert_eq!(voice.request_id, request_id);
        assert_eq!(voice.fence, current);

        let mut crossed = voice.fence.clone();
        crossed.remote_revision += 1;
        assert!(broker
            .accept_voice(&request_id, &crossed, "device_a", "Offer Tuesday")
            .is_err());
        broker
            .accept_voice(&request_id, &voice.fence, "device_a", "Offer Tuesday")
            .unwrap();
        assert!(broker
            .accept_voice(&request_id, &voice.fence, "device_a", "Offer Friday")
            .is_err());

        let AssistanceResolution::Answered(answer) = broker.take_resolution(&request_id).unwrap()
        else {
            panic!("voice consult must resolve as an answer");
        };
        assert!(answer.voice_consult);
        assert_eq!(answer.answer, "Offer Tuesday");
        assert!(broker.take_resolution(&request_id).is_none());
    }

    #[test]
    fn transfer_stays_pending_through_acceptance_and_resolves_only_when_taken() {
        let broker = AssistanceBroker::default();
        let offered_fence = fence();
        let request_id = broker
            .request_transfer(
                offered_fence.clone(),
                "PIN 1234 for Gail on 0412345678",
                None,
                30,
            )
            .unwrap();
        let frame = broker.pending_frame("app_a").unwrap();
        assert!(frame.transfer_offered);
        assert_eq!(frame.question, SAFE_TRANSFER_REASON);
        assert!(!frame
            .question
            .chars()
            .any(|character| character.is_ascii_digit()));
        assert!(!frame.question.contains("Gail"));

        broker
            .accept_transfer(&request_id, &offered_fence, "device_owner")
            .unwrap();
        assert!(broker.is_waiting(&request_id));
        assert_eq!(
            broker
                .pending_transfer("call_a", offered_fence.call_epoch)
                .unwrap()
                .accepted_by
                .as_deref(),
            Some("device_owner")
        );
        assert!(broker
            .transfer_taken(&request_id, &offered_fence, "other_device")
            .is_err());
        broker
            .transfer_taken(&request_id, &offered_fence, "device_owner")
            .unwrap();
        broker
            .transfer_taken(&request_id, &offered_fence, "device_owner")
            .unwrap();
        assert!(!broker.is_waiting(&request_id));
        assert_eq!(
            broker.take_resolution(&request_id),
            Some(AssistanceResolution::TransferTaken {
                device_id: "device_owner".into()
            })
        );
    }

    #[test]
    fn explicit_decline_is_typed_revision_fenced_and_terminal() {
        let broker = AssistanceBroker::default();
        let offered_fence = fence();
        let request_id = broker
            .request_transfer(offered_fence, "policy escalation", None, 30)
            .unwrap();
        let decline = PluginAssistanceAnswerFrame {
            kind: "assistance_answer".into(),
            schema_version: SCHEMA_VERSION,
            app_id: "app_a".into(),
            device_id: "device_owner".into(),
            request_id: request_id.clone(),
            answer_id: "answer_decline".into(),
            call_id: "call_a".into(),
            call_epoch: 2,
            owner_epoch: 3,
            switchboard_revision: 4,
            remote_revision: 5,
            response_action: AssistanceResponseAction::Decline,
            // Required for wire compatibility, but never interpreted as a
            // control command or spoken to the caller.
            answer: "declined".into(),
        };
        let mut forbidden_answer = decline.clone();
        forbidden_answer.answer_id = "answer_not_a_transfer".into();
        forbidden_answer.response_action = AssistanceResponseAction::Answer;
        forbidden_answer.answer = "I'll take it".into();
        assert!(broker.accept(forbidden_answer).is_err());
        assert!(broker.is_waiting(&request_id));
        broker.accept(decline.clone()).unwrap();
        broker.accept(decline).unwrap();
        assert_eq!(
            broker.take_resolution(&request_id),
            Some(AssistanceResolution::Declined {
                device_id: "device_owner".into(),
                answer_id: "answer_decline".into(),
                answer: "declined".into(),
            })
        );
    }

    #[test]
    fn transfer_cannot_enter_the_private_consult_lane() {
        let broker = AssistanceBroker::default();
        let offered_fence = fence();
        broker
            .request_transfer(offered_fence.clone(), "caller asked for owner", None, 30)
            .unwrap();
        let mut consult_fence = offered_fence.clone();
        consult_fence.owner_epoch += 1;
        consult_fence.remote_revision += 1;
        assert!(broker
            .begin_voice_consult(&offered_fence, consult_fence)
            .is_err());
    }

    #[test]
    fn failed_accepted_transfer_refences_to_the_returned_aokie_owner() {
        let broker = AssistanceBroker::default();
        let offered_fence = fence();
        let request_id = broker
            .request_transfer(offered_fence.clone(), "caller asked for owner", None, 30)
            .unwrap();
        broker
            .accept_transfer(&request_id, &offered_fence, "device_owner")
            .unwrap();

        let mut returned_fence = offered_fence.clone();
        returned_fence.owner_epoch += 2;
        returned_fence.remote_revision += 3;
        let mut crossed = returned_fence.clone();
        crossed.call_epoch += 1;
        assert!(broker
            .transfer_unavailable(&request_id, &offered_fence, crossed, Some("device_owner"),)
            .is_err());
        broker
            .transfer_unavailable(
                &request_id,
                &offered_fence,
                returned_fence.clone(),
                Some("device_owner"),
            )
            .unwrap();
        broker
            .transfer_unavailable(
                &request_id,
                &offered_fence,
                returned_fence.clone(),
                Some("device_owner"),
            )
            .unwrap();
        assert_eq!(
            broker.take_resolution(&request_id),
            Some(AssistanceResolution::TransferUnavailable {
                fence: returned_fence,
            })
        );
    }

    #[test]
    fn accepted_transfer_uses_a_separate_setup_deadline_and_resolution_grace() {
        let broker = AssistanceBroker::default();
        let offered_fence = fence();
        let request_id = broker
            .request_transfer(offered_fence.clone(), "caller asked for owner", None, 30)
            .unwrap();
        broker
            .accept_transfer(&request_id, &offered_fence, "device_owner")
            .unwrap();
        let now = unix_now().unwrap();

        {
            let mut mailbox = broker.inner.lock().unwrap();
            let pending = mailbox.as_mut().unwrap();
            pending.expires_at = now.saturating_sub(1);
            pending.accepted_setup_expires_at = Some(now + 10);
        }
        assert!(broker.is_waiting(&request_id));
        assert_eq!(
            broker
                .pending_transfer("call_a", offered_fence.call_epoch)
                .unwrap()
                .expires_at,
            now + 10
        );

        {
            let mut mailbox = broker.inner.lock().unwrap();
            mailbox.as_mut().unwrap().accepted_setup_expires_at = Some(now.saturating_sub(1));
        }
        assert!(broker
            .pending_transfer("call_a", offered_fence.call_epoch)
            .is_none());
        assert!(broker
            .accepted_transfer("call_a", offered_fence.call_epoch)
            .is_some());
        assert!(broker.is_waiting(&request_id));
        assert!(broker.take_resolution(&request_id).is_none());

        let mut returned = offered_fence.clone();
        returned.owner_epoch += 2;
        returned.remote_revision += 2;
        broker
            .transfer_unavailable(
                &request_id,
                &offered_fence,
                returned.clone(),
                Some("device_owner"),
            )
            .unwrap();
        assert_eq!(
            broker.take_resolution(&request_id),
            Some(AssistanceResolution::TransferUnavailable { fence: returned })
        );
    }
}
