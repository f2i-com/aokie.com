//! Software recovery for a dongle whose USB byte stream went bad.
//!
//! The runtime's ACL accumulator counts "garbage prefix" resyncs, and a dense
//! burst of them (see `note_acl_corruption` in `runtime.rs`) used to raise a
//! hardware error that told the operator to unplug the dongle. This module
//! decides what the runtime does instead, least invasive first:
//!
//! 1. Flush: drop the accumulator and the ACL pipe's buffered bytes. The
//!    phone stays connected.
//! 2. Reopen: close the WinUSB handle, wait, reopen it, HCI_Reset and
//!    re-initialise the controller. The phone link drops and the runtime's
//!    auto-reconnect pages it back. Up to `REOPEN_BACKOFF.len()` times per
//!    `RECOVERY_WINDOW`, with growing waits; never while a call is up
//!    (it would cut the call's audio), so a burst during a call only flushes
//!    and the reopen waits for the call to end.
//! 3. Give up: only then ask for a physical replug, saying what was tried.
//!
//! A USB port reset or device restart (CM_Query_And_Remove_SubTree,
//! pnputil /restart-device) would be the next rung, but it needs
//! administrator rights, so it is not attempted here.
//!
//! Pure bookkeeping — no I/O — so the ladder is unit-tested without hardware.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Attempts older than this are forgotten: a dongle that stayed clean this
/// long after a recovery starts the next incident from the first rung.
pub(crate) const RECOVERY_WINDOW: Duration = Duration::from_secs(30 * 60);

/// Wait before each reopen within one window; its length is the reopen cap.
pub(crate) const REOPEN_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(10),
    Duration::from_secs(60),
];

/// What the dense-corruption detector saw, for the give-up message.
pub(crate) const CAUSE_CORRUPTED_STREAM: &str =
    "Bluetooth dongle USB stream corrupted (continuous garbage in reads)";

/// After recovery gave up because the dongle could not be reopened, keep
/// trying this often so a replug (or a dongle that comes back by itself)
/// is picked up without restarting the plugin.
pub(crate) const REOPEN_RETRY_AFTER_GIVE_UP: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempt {
    FlushAclPipe,
    Reopen,
}

/// What the runtime should do about a corrupted stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    /// Drop the ACL accumulator and the pipe's buffered bytes; keep the link.
    FlushAclPipe,
    /// The flush already ran this window but a call is up: flush again now
    /// and reopen once the call ends (`after_call_ended`).
    FlushAndReopenAfterCall,
    /// Close the WinUSB handle, wait `delay`, reopen and re-initialise.
    ReopenTransport { delay: Duration },
    /// Software recovery is used up for this window: ask for a replug.
    GiveUp,
}

#[derive(Debug, Default)]
pub(crate) struct TransportRecovery {
    attempts: VecDeque<(Instant, Attempt)>,
    /// A reopen was chosen and the controller has not initialised since.
    reopen_in_progress: bool,
    /// The give-up report went out for the current window.
    give_up_reported: bool,
    /// The controller initialised since the supervisor last looked.
    initialized_since_check: bool,
}

impl TransportRecovery {
    /// The dense-corruption detector tripped.
    pub(crate) fn on_dense_corruption(
        &mut self,
        now: Instant,
        call_active: bool,
    ) -> RecoveryAction {
        self.prune(now);
        if self.count(Attempt::FlushAclPipe) == 0 {
            self.attempts.push_back((now, Attempt::FlushAclPipe));
            return RecoveryAction::FlushAclPipe;
        }
        if call_active {
            return RecoveryAction::FlushAndReopenAfterCall;
        }
        self.reopen_or_give_up(now)
    }

    /// A reopen was deferred for a call and the call has ended.
    pub(crate) fn after_call_ended(&mut self, now: Instant) -> RecoveryAction {
        self.prune(now);
        self.reopen_or_give_up(now)
    }

    /// Opening or initialising the dongle failed during a reopen. Returns
    /// the wait before the next try, or `None` once the reopens are used up.
    pub(crate) fn next_reopen_after_failed_open(&mut self, now: Instant) -> Option<Duration> {
        self.prune(now);
        match self.reopen_or_give_up(now) {
            RecoveryAction::ReopenTransport { delay } => Some(delay),
            _ => None,
        }
    }

    /// An explicit reset request (`AokieRuntime::reset_transport`): reopen
    /// now, counted like any other reopen but never refused.
    pub(crate) fn force_reopen(&mut self, now: Instant) -> Duration {
        self.prune(now);
        self.attempts.push_back((now, Attempt::Reopen));
        self.reopen_in_progress = true;
        REOPEN_BACKOFF[0]
    }

    /// The controller came up after `run_runtime` (re)opened it.
    pub(crate) fn mark_initialized(&mut self) {
        self.reopen_in_progress = false;
        self.initialized_since_check = true;
    }

    /// Whether the run that just ended got as far as initialising the
    /// controller (an error after that is the transport failing mid-session,
    /// which a reopen can fix; an error before it on a first start is not
    /// ours to retry).
    pub(crate) fn take_initialized(&mut self) -> bool {
        std::mem::take(&mut self.initialized_since_check)
    }

    pub(crate) fn reopen_in_progress(&self) -> bool {
        self.reopen_in_progress
    }

    /// True exactly once per window after recovery gave up, so the
    /// replug request is raised once rather than on every burst.
    pub(crate) fn take_give_up_report(&mut self) -> bool {
        if self.give_up_reported {
            return false;
        }
        self.give_up_reported = true;
        true
    }

    /// The hardware error raised once software recovery is used up.
    /// `cause` names what went wrong (`CAUSE_CORRUPTED_STREAM`, or the
    /// transport error that ended a session).
    pub(crate) fn give_up_message(&self, cause: &str, detail: Option<&str>) -> String {
        let flushes = self.count(Attempt::FlushAclPipe);
        let reopens = self.count(Attempt::Reopen);
        let mut msg = format!(
            "{} and software recovery did not clear it ({} in the last {} minutes) - \
             connections cannot succeed until the dongle is power-cycled: unplug it, wait 5 \
             seconds, plug it back in",
            cause,
            describe_attempts(flushes, reopens),
            RECOVERY_WINDOW.as_secs() / 60,
        );
        if let Some(detail) = detail {
            msg.push_str(" (last reopen failed: ");
            msg.push_str(detail);
            msg.push(')');
        }
        msg
    }

    pub(crate) fn attempt_counts(&self) -> (usize, usize) {
        (
            self.count(Attempt::FlushAclPipe),
            self.count(Attempt::Reopen),
        )
    }

    fn reopen_or_give_up(&mut self, now: Instant) -> RecoveryAction {
        let reopens = self.count(Attempt::Reopen);
        if reopens < REOPEN_BACKOFF.len() {
            self.attempts.push_back((now, Attempt::Reopen));
            self.reopen_in_progress = true;
            return RecoveryAction::ReopenTransport {
                delay: REOPEN_BACKOFF[reopens],
            };
        }
        RecoveryAction::GiveUp
    }

    fn prune(&mut self, now: Instant) {
        while self
            .attempts
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RECOVERY_WINDOW)
        {
            self.attempts.pop_front();
        }
        if self.attempts.is_empty() {
            self.give_up_reported = false;
        }
    }

    fn count(&self, kind: Attempt) -> usize {
        self.attempts.iter().filter(|(_, a)| *a == kind).count()
    }
}

fn describe_attempts(flushes: usize, reopens: usize) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{} {}", n, if n == 1 { one } else { many });
    format!(
        "tried {} and {}",
        plural(flushes, "USB pipe flush", "USB pipe flushes"),
        plural(reopens, "dongle reset", "dongle resets"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_burst_flushes_and_keeps_the_link() {
        let mut r = TransportRecovery::default();
        let t0 = Instant::now();
        assert_eq!(
            r.on_dense_corruption(t0, false),
            RecoveryAction::FlushAclPipe
        );
        assert!(!r.reopen_in_progress());
    }

    #[test]
    fn repeated_bursts_climb_to_reopens_with_growing_waits_then_give_up() {
        let mut r = TransportRecovery::default();
        let t0 = Instant::now();
        assert_eq!(
            r.on_dense_corruption(t0, false),
            RecoveryAction::FlushAclPipe
        );
        for (i, wait) in REOPEN_BACKOFF.iter().enumerate() {
            let at = t0 + Duration::from_secs(60 * (i as u64 + 1));
            assert_eq!(
                r.on_dense_corruption(at, false),
                RecoveryAction::ReopenTransport { delay: *wait }
            );
            assert!(r.reopen_in_progress());
            r.mark_initialized();
            assert!(!r.reopen_in_progress());
        }
        assert!(REOPEN_BACKOFF.windows(2).all(|w| w[0] < w[1]));
        let at = t0 + Duration::from_secs(300);
        assert_eq!(r.on_dense_corruption(at, false), RecoveryAction::GiveUp);
        assert_eq!(r.attempt_counts(), (1, REOPEN_BACKOFF.len()));
    }

    #[test]
    fn a_call_defers_the_reopen_until_it_ends() {
        let mut r = TransportRecovery::default();
        let t0 = Instant::now();
        assert_eq!(
            r.on_dense_corruption(t0, true),
            RecoveryAction::FlushAclPipe
        );
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(
            r.on_dense_corruption(t1, true),
            RecoveryAction::FlushAndReopenAfterCall
        );
        // Deferring records no reopen: the whole reopen budget is still there.
        assert_eq!(r.attempt_counts(), (1, 0));
        assert_eq!(
            r.after_call_ended(t1 + Duration::from_secs(90)),
            RecoveryAction::ReopenTransport {
                delay: REOPEN_BACKOFF[0]
            }
        );
    }

    #[test]
    fn a_clean_window_starts_the_next_incident_from_the_flush() {
        let mut r = TransportRecovery::default();
        let t0 = Instant::now();
        r.on_dense_corruption(t0, false);
        r.on_dense_corruption(t0 + Duration::from_secs(10), false);
        let later = t0 + RECOVERY_WINDOW + Duration::from_secs(11);
        assert_eq!(
            r.on_dense_corruption(later, false),
            RecoveryAction::FlushAclPipe
        );
        assert_eq!(r.attempt_counts(), (1, 0));
    }

    #[test]
    fn the_supervisor_learns_whether_a_run_initialised_once() {
        let mut r = TransportRecovery::default();
        assert!(!r.take_initialized());
        r.force_reopen(Instant::now());
        r.mark_initialized();
        assert!(!r.reopen_in_progress());
        assert!(r.take_initialized());
        assert!(!r.take_initialized(), "consumed by the first look");
    }

    #[test]
    fn failed_reopens_back_off_then_stop() {
        let mut r = TransportRecovery::default();
        let t0 = Instant::now();
        let first = r.force_reopen(t0);
        assert_eq!(first, REOPEN_BACKOFF[0]);
        assert!(r.reopen_in_progress());
        assert_eq!(
            r.next_reopen_after_failed_open(t0 + Duration::from_secs(2)),
            Some(REOPEN_BACKOFF[1])
        );
        assert_eq!(
            r.next_reopen_after_failed_open(t0 + Duration::from_secs(15)),
            Some(REOPEN_BACKOFF[2])
        );
        assert_eq!(
            r.next_reopen_after_failed_open(t0 + Duration::from_secs(80)),
            None
        );
    }

    #[test]
    fn give_up_is_reported_once_per_window_and_says_what_was_tried() {
        let mut r = TransportRecovery::default();
        let t0 = Instant::now();
        r.on_dense_corruption(t0, false);
        for i in 0..REOPEN_BACKOFF.len() {
            r.on_dense_corruption(t0 + Duration::from_secs(i as u64 + 1), false);
        }
        assert_eq!(
            r.on_dense_corruption(t0 + Duration::from_secs(10), false),
            RecoveryAction::GiveUp
        );
        assert!(r.take_give_up_report());
        assert!(!r.take_give_up_report());
        let msg = r.give_up_message(CAUSE_CORRUPTED_STREAM, None);
        assert!(msg.starts_with(CAUSE_CORRUPTED_STREAM), "{msg}");
        assert!(msg.contains("software recovery did not clear it"), "{msg}");
        assert!(
            msg.contains("1 USB pipe flush and 3 dongle resets"),
            "{msg}"
        );
        assert!(msg.contains("unplug it, wait 5 seconds"), "{msg}");
        let with_detail = r.give_up_message(
            CAUSE_CORRUPTED_STREAM,
            Some("no HCI-capable WinUSB radio interface found"),
        );
        assert!(with_detail
            .ends_with("(last reopen failed: no HCI-capable WinUSB radio interface found)"));
        // Once the window has passed, the ladder (and the report) re-arm.
        let later = t0 + RECOVERY_WINDOW + Duration::from_secs(20);
        assert_eq!(
            r.on_dense_corruption(later, false),
            RecoveryAction::FlushAclPipe
        );
        assert!(r.take_give_up_report());
    }
}
