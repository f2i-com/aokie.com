//! Volatile, plugin-owned Companion assistance mailbox.
//!
//! This is intentionally not the FormLogic command/event plane. Questions
//! and their single accepted answers travel only over authenticated v2
//! signalling and remain in memory for the live receptionist to consume.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use aokie_protocol::v2::{
    PluginAssistanceAnswerFrame, PluginAssistanceRequestFrame, MAX_ASSISTANCE_ANSWER_BYTES,
    MAX_ASSISTANCE_CONTEXT_BYTES, MAX_ASSISTANCE_QUESTION_BYTES, SCHEMA_VERSION,
};

const MAX_TTL_SECONDS: u64 = 300;

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
    answer: Option<AssistanceAnswer>,
}

impl AssistanceBroker {
    pub fn request(
        &self,
        fence: AssistanceCallFence,
        question: &str,
        context: Option<&str>,
        ttl_seconds: u64,
    ) -> Result<String, String> {
        validate_text(question, MAX_ASSISTANCE_QUESTION_BYTES, "question")?;
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
        if current
            .as_ref()
            .is_some_and(|pending| pending.answer.is_none() && pending.expires_at > now)
        {
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
            answer: None,
        });
        Ok(request_id)
    }

    pub fn pending_frame(&self, app_id: &str) -> Option<PluginAssistanceRequestFrame> {
        let now = unix_now().ok()?;
        let pending = self.inner.lock().ok()?.clone()?;
        if pending.expires_at <= now || pending.answer.is_some() {
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
        if let Some(answer) = &pending.answer {
            return if answer.answer_id == frame.answer_id
                && answer.device_id == frame.device_id
                && answer.answer == frame.answer
            {
                Ok(())
            } else {
                Err("assistance request already consumed its one answer".into())
            };
        }
        validate_text(&frame.answer, MAX_ASSISTANCE_ANSWER_BYTES, "answer")?;
        pending.answer = Some(AssistanceAnswer {
            request_id: frame.request_id,
            answer_id: frame.answer_id,
            device_id: frame.device_id,
            answer: frame.answer,
            voice_consult: false,
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
        if pending.answer.is_some() || pending.expires_at <= now || pending.fence != *previous {
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
        (pending.request_id == request_id && pending.answer.is_none() && pending.expires_at > now)
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
            || pending.expires_at <= now
            || pending.answer.is_some()
        {
            return Err("voice consult answer crossed its assistance fence".into());
        }
        if device_id.is_empty()
            || device_id.len() > 200
            || !device_id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
        {
            return Err("voice consult device identity is invalid".into());
        }
        pending.answer = Some(AssistanceAnswer {
            request_id: request_id.into(),
            answer_id: format!("voice_{}", uuid::Uuid::new_v4().simple()),
            device_id: device_id.into(),
            answer: answer.trim().into(),
            voice_consult: true,
        });
        Ok(())
    }

    /// Take the accepted answer exactly once. A second consumer receives
    /// `None`; a later network replay cannot recreate the removed mailbox.
    pub fn take_answer(&self, request_id: &str) -> Option<AssistanceAnswer> {
        let mut current = self.inner.lock().ok()?;
        if current.as_ref()?.request_id != request_id {
            return None;
        }
        current.take()?.answer
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
                    && pending.answer.is_none()
                    && pending.expires_at > now
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
            answer: "Yes, use the secure drop box.".into(),
        };
        broker.accept(answer.clone()).unwrap();
        broker.accept(answer).unwrap();
        assert!(broker.pending_frame("app_a").is_none());
        assert_eq!(
            broker.take_answer(&request_id).unwrap().answer_id,
            "answer_a"
        );
        assert!(broker.take_answer(&request_id).is_none());
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

        let answer = broker.take_answer(&request_id).unwrap();
        assert!(answer.voice_consult);
        assert_eq!(answer.answer, "Offer Tuesday");
        assert!(broker.take_answer(&request_id).is_none());
    }
}
