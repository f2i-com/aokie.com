use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::MediaError;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaMode {
    Monitor,
    /// Provisional private consultation.  It is receive-only and therefore
    /// cannot request or arm an operating-system microphone before Desktop
    /// has proved the caller is isolated by software hold.
    PreparedConsult,
    /// Provisional takeover: positive winner fence and caller audio receive,
    /// but no microphone track and no caller-bound transmit capability.
    PreparedTalk,
    Consult,
    Talk,
}

impl MediaMode {
    pub fn may_transmit_to_caller(self) -> bool {
        matches!(self, Self::Talk)
    }

    pub fn needs_microphone(self) -> bool {
        matches!(self, Self::Consult | Self::Talk)
    }
}

/// Immutable identity and epoch binding for one native peer connection.
///
/// Every peer has a short-lived authorization lease. Monitor and consult use
/// fence zero because neither can write to the cellular caller. A talk peer
/// must carry a positive gateway-allocated fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBinding {
    pub rtc_session_id: String,
    pub call_id: String,
    pub call_epoch: u64,
    pub owner_epoch: u64,
    pub device_id: String,
    pub mode: MediaMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    pub fence: u64,
}

impl SessionBinding {
    pub fn validate(&self) -> Result<(), MediaError> {
        for value in [&self.rtc_session_id, &self.call_id, &self.device_id] {
            if !safe_id(value) {
                return Err(MediaError::InvalidBinding("identity"));
            }
        }
        if self.call_epoch > MAX_SAFE_INTEGER || self.owner_epoch > MAX_SAFE_INTEGER {
            return Err(MediaError::InvalidBinding("epoch"));
        }
        match self.mode {
            MediaMode::Monitor => {
                if self.lease_id.as_deref().is_none_or(|value| !safe_id(value)) || self.fence != 0 {
                    return Err(MediaError::InvalidBinding(
                        "monitor requires a lease and fence zero",
                    ));
                }
            }
            MediaMode::PreparedConsult => {
                if self.lease_id.as_deref().is_none_or(|value| !safe_id(value)) || self.fence != 0 {
                    return Err(MediaError::InvalidBinding(
                        "prepared consult requires a lease and fence zero",
                    ));
                }
            }
            MediaMode::PreparedTalk => {
                if self.lease_id.as_deref().is_none_or(|value| !safe_id(value))
                    || !(1..=MAX_SAFE_INTEGER).contains(&self.fence)
                {
                    return Err(MediaError::InvalidBinding(
                        "prepared talk requires a lease and positive fence",
                    ));
                }
            }
            MediaMode::Consult => {
                if self.lease_id.as_deref().is_none_or(|value| !safe_id(value)) || self.fence != 0 {
                    return Err(MediaError::InvalidBinding(
                        "consult requires a lease and fence zero",
                    ));
                }
            }
            MediaMode::Talk => {
                if self.lease_id.as_deref().is_none_or(|value| !safe_id(value))
                    || !(1..=MAX_SAFE_INTEGER).contains(&self.fence)
                {
                    return Err(MediaError::InvalidBinding(
                        "talk requires a lease and positive fence",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn lease_id(&self) -> Option<&str> {
        self.lease_id.as_deref()
    }
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

#[derive(Debug, Clone)]
pub struct RoutePermit {
    pub binding: SessionBinding,
    pub expires_at: Instant,
}

impl RoutePermit {
    pub fn new(binding: SessionBinding, lifetime: Duration) -> Result<Self, MediaError> {
        binding.validate()?;
        if !binding.mode.may_transmit_to_caller() {
            return Err(MediaError::UnsafeTransition(
                "only a talk lease can open the caller-bound route",
            ));
        }
        if lifetime.is_zero() {
            return Err(MediaError::UnsafeTransition(
                "route permit is already expired",
            ));
        }
        let expires_at = Instant::now()
            .checked_add(lifetime)
            .ok_or(MediaError::UnsafeTransition("route permit expiry overflow"))?;
        Ok(Self {
            binding,
            expires_at,
        })
    }

    pub fn is_current_for(&self, binding: &SessionBinding, now: Instant) -> bool {
        now < self.expires_at && self.binding == *binding
    }
}

/// Fail-closed caller-transmit gate shared with the decoded-audio task.
///
/// Replacing a permit is allowed only for the same call at a strictly newer
/// owner epoch and fence. Closing the gate is always immediate.
#[derive(Clone, Default)]
pub struct RouteGate {
    inner: Arc<Mutex<Option<RoutePermit>>>,
}

impl RouteGate {
    pub fn authorize(&self, permit: RoutePermit) -> Result<(), MediaError> {
        permit.binding.validate()?;
        let mut current = self
            .inner
            .lock()
            .map_err(|_| MediaError::UnsafeTransition("route gate poisoned"))?;
        if let Some(existing) = current.as_ref() {
            let old = &existing.binding;
            let new = &permit.binding;
            if old.call_id != new.call_id || old.call_epoch != new.call_epoch {
                return Err(MediaError::UnsafeTransition(
                    "a live route must be revoked before changing calls",
                ));
            }
            if new.owner_epoch <= old.owner_epoch || new.fence <= old.fence {
                return Err(MediaError::UnsafeTransition(
                    "replacement route must advance owner epoch and fence",
                ));
            }
        }
        *current = Some(permit);
        Ok(())
    }

    pub fn revoke(&self) {
        if let Ok(mut current) = self.inner.lock() {
            *current = None;
        }
    }

    /// Extends an already-live route without changing its immutable logical
    /// lease, epochs, or fence. Gateway heartbeat tokens rotate their JTI, but
    /// the stable lease ID remains bound to the peer. An expired route cannot
    /// be resurrected by renewal; it must go through a fresh authorization.
    pub fn renew(&self, binding: &SessionBinding, lifetime: Duration) -> Result<(), MediaError> {
        if lifetime.is_zero() {
            return Err(MediaError::UnsafeTransition(
                "route renewal is already expired",
            ));
        }
        let now = Instant::now();
        let expires_at = now
            .checked_add(lifetime)
            .ok_or(MediaError::UnsafeTransition(
                "route renewal expiry overflow",
            ))?;
        let mut current = self
            .inner
            .lock()
            .map_err(|_| MediaError::UnsafeTransition("route gate poisoned"))?;
        let existing = current
            .as_mut()
            .ok_or(MediaError::UnsafeTransition("no live route to renew"))?;
        if !existing.is_current_for(binding, now) {
            *current = None;
            return Err(MediaError::UnsafeTransition(
                "route renewal does not match a current permit",
            ));
        }
        if expires_at > existing.expires_at {
            existing.expires_at = expires_at;
        }
        Ok(())
    }

    pub fn allows(&self, binding: &SessionBinding) -> bool {
        let now = Instant::now();
        let Ok(mut current) = self.inner.lock() else {
            return false;
        };
        if current
            .as_ref()
            .is_some_and(|permit| permit.is_current_for(binding, now))
        {
            true
        } else {
            if current
                .as_ref()
                .is_some_and(|permit| now >= permit.expires_at)
            {
                *current = None;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn talk(owner_epoch: u64, fence: u64) -> SessionBinding {
        SessionBinding {
            rtc_session_id: format!("rtc_{owner_epoch}"),
            call_id: "call_a".into(),
            call_epoch: 7,
            owner_epoch,
            device_id: "device_a".into(),
            mode: MediaMode::Talk,
            lease_id: Some(format!("lease_{fence}")),
            fence,
        }
    }

    #[test]
    fn binding_modes_are_fail_closed() {
        let mut monitor = talk(0, 1);
        monitor.mode = MediaMode::Monitor;
        assert!(monitor.validate().is_err());
        monitor.fence = 0;
        assert!(monitor.validate().is_ok());

        let mut prepared = talk(0, 4);
        prepared.mode = MediaMode::PreparedTalk;
        assert!(prepared.validate().is_ok());
        assert!(!prepared.mode.needs_microphone());
        assert!(!prepared.mode.may_transmit_to_caller());
        assert!(RoutePermit::new(prepared.clone(), Duration::from_secs(1)).is_err());
        prepared.fence = 0;
        assert!(prepared.validate().is_err());

        let mut prepared_consult = monitor.clone();
        prepared_consult.mode = MediaMode::PreparedConsult;
        prepared_consult.lease_id = Some("lease_pc".into());
        assert!(prepared_consult.validate().is_ok());
        assert!(!prepared_consult.mode.needs_microphone());
        assert!(!prepared_consult.mode.may_transmit_to_caller());

        let mut consult = monitor.clone();
        consult.mode = MediaMode::Consult;
        consult.lease_id = Some("lease_c".into());
        assert!(consult.validate().is_ok());
        consult.fence = 1;
        assert!(consult.validate().is_err());
    }

    #[test]
    fn gate_rejects_stale_replacement_and_expires_closed() {
        let gate = RouteGate::default();
        let first = talk(3, 8);
        gate.authorize(RoutePermit::new(first.clone(), Duration::from_millis(20)).unwrap())
            .unwrap();
        assert!(gate.allows(&first));
        assert!(gate
            .authorize(RoutePermit::new(talk(3, 9), Duration::from_secs(1)).unwrap())
            .is_err());
        assert!(gate
            .authorize(RoutePermit::new(talk(4, 8), Duration::from_secs(1)).unwrap())
            .is_err());
        std::thread::sleep(Duration::from_millis(25));
        assert!(!gate.allows(&first));
    }

    #[test]
    fn revoke_is_immediate() {
        let gate = RouteGate::default();
        let binding = talk(1, 1);
        gate.authorize(RoutePermit::new(binding.clone(), Duration::from_secs(1)).unwrap())
            .unwrap();
        gate.revoke();
        assert!(!gate.allows(&binding));
    }

    #[test]
    fn renewal_keeps_the_same_logical_lease_and_cannot_resurrect_expiry() {
        let gate = RouteGate::default();
        let binding = talk(1, 1);
        gate.authorize(RoutePermit::new(binding.clone(), Duration::from_millis(20)).unwrap())
            .unwrap();
        assert!(gate.renew(&binding, Duration::from_secs(1)).is_ok());
        assert!(gate.allows(&binding));

        let expired_gate = RouteGate::default();
        expired_gate
            .authorize(RoutePermit::new(binding.clone(), Duration::from_millis(1)).unwrap())
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(expired_gate
            .renew(&binding, Duration::from_secs(1))
            .is_err());
        assert!(!expired_gate.allows(&binding));
    }
}
