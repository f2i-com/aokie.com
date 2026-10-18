//! One endpoint's view of the server clock.
//!
//! The phone and the Desktop check each other's proofs, offers and leases, and
//! the gateway's, against their own clocks with only
//! [`ENDPOINT_PROOF_CLOCK_SKEW`](crate::v2::ENDPOINT_PROOF_CLOCK_SKEW) of
//! slack. A phone or Desktop more than about five seconds out therefore could
//! not take over, and its leases lapsed early or late.
//!
//! Each endpoint now reads the server's clock off the response that carries
//! its endpoint challenge (the HTTP `Date` header of the gateway's WebSocket
//! upgrade, or of the relay's challenge document) and moves its own clock onto
//! it. Every endpoint then agrees with the gateway and with every other
//! endpoint. Nothing on the wire changes: a clock already within a couple of
//! seconds is left exactly as it is, so an endpoint that predates this, or a
//! server that sends no `Date`, behaves as before.
//!
//! The server's time is read only as a session connects, and a connection can
//! last for hours, so the offset is held against a steady clock (review 4 #9):
//! one nobody sets, that runs on while the computer sleeps. Should this
//! computer's clock be set while the connection lasts (by hand, or by network
//! time putting it right), the step shows against the steady clock, and the
//! offset takes it up, so protocol time goes on as the server's. Between
//! steps protocol time is this computer's clock plus the offset, as before:
//! that clock is the one network time keeps right.

use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// The largest correction taken from a server's `Date`. A clock further out
/// than an hour is left alone: it is wrong enough that one header is not
/// trusted to fix it, and it was already broken before this existed.
pub const MAX_CLOCK_OFFSET_SECONDS: i64 = 60 * 60;

/// A `Date` is whole seconds, written a moment before it arrives, so a clock
/// in step reads up to about two seconds off. Within this the clock is left
/// exactly as it is.
pub const CLOCK_OFFSET_DEAD_BAND_SECONDS: i64 = 2;

/// How far the server's clock is ahead of this one (negative: behind), read
/// from an HTTP `Date` header that arrived at `local_now`.
///
/// `Some(0)` when the clock is already in step. `None` when the header is not
/// an HTTP date or names a time more than [`MAX_CLOCK_OFFSET_SECONDS`] away;
/// the caller then keeps the offset it had.
pub fn offset_from_http_date(date: &str, local_now: u64) -> Option<i64> {
    let server_now = chrono::DateTime::parse_from_rfc2822(date.trim())
        .ok()?
        .timestamp();
    let local_now = i64::try_from(local_now).ok()?;
    let offset = server_now.checked_sub(local_now)?;
    if offset.abs() <= CLOCK_OFFSET_DEAD_BAND_SECONDS {
        Some(0)
    } else if offset.abs() <= MAX_CLOCK_OFFSET_SECONDS {
        Some(offset)
    } else {
        None
    }
}

/// `local_now` moved onto the server's clock.
pub fn corrected_now(local_now: u64, offset: i64) -> u64 {
    if offset >= 0 {
        local_now.saturating_add(offset.unsigned_abs())
    } else {
        local_now.saturating_sub(offset.unsigned_abs()).max(1)
    }
}

/// The one clock offset an endpoint process keeps.
///
/// It is shared by everything in the process that dates or checks a
/// protocol time, so a proof this endpoint signs and a proof it checks are
/// always measured on the same clock.
#[derive(Debug, Default)]
pub struct ServerClock {
    state: Mutex<ClockState>,
}

#[derive(Debug, Default, Clone, Copy)]
struct ClockState {
    /// Seconds the server's clock is ahead of this computer's.
    offset: i64,
    /// This computer's clock (Unix seconds) and the steady clock (ms) when
    /// protocol time was last read or observed: what a step of this
    /// computer's clock is measured against. None before the first `Date`,
    /// and where there is no steady clock.
    anchor: Option<Anchor>,
}

#[derive(Debug, Clone, Copy)]
struct Anchor {
    local: u64,
    steady_ms: u64,
}

impl ClockState {
    /// The offset now: when this computer's clock moved against the steady
    /// one since the last read by more than whole-second rounding and drift
    /// allow, it was set, and the offset takes the step up. A step that would
    /// take the offset past [`MAX_CLOCK_OFFSET_SECONDS`] is not followed.
    fn settle(&mut self, local_now: u64, steady_ms: Option<u64>) -> i64 {
        let (Some(anchor), Some(steady_ms)) = (self.anchor, steady_ms) else {
            return self.offset;
        };
        if let (Some(steady_elapsed_ms), Ok(now), Ok(then)) = (
            steady_ms.checked_sub(anchor.steady_ms),
            i64::try_from(local_now),
            i64::try_from(anchor.local),
        ) {
            let steady_elapsed = i64::try_from(steady_elapsed_ms / 1_000).unwrap_or(i64::MAX);
            let step = now.saturating_sub(then).saturating_sub(steady_elapsed);
            // Whole seconds read a moment apart differ by up to two; any
            // clock drifts, and network time slews it, by well under 1,000
            // parts per million.
            let slack = CLOCK_OFFSET_DEAD_BAND_SECONDS
                .saturating_add(i64::try_from(steady_elapsed_ms / 1_000_000).unwrap_or(i64::MAX));
            if step.abs() > slack {
                let offset = self.offset.saturating_sub(step);
                if offset.abs() <= MAX_CLOCK_OFFSET_SECONDS {
                    self.offset = offset;
                }
            }
        }
        self.anchor = Some(Anchor {
            local: local_now,
            steady_ms,
        });
        self.offset
    }
}

impl ServerClock {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(ClockState {
                offset: 0,
                anchor: None,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, ClockState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Seconds the server's clock is ahead of this one, now.
    pub fn offset(&self) -> i64 {
        let local_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs());
        let mut state = self.state();
        match local_now {
            Some(local_now) => state.settle(local_now, steady_millis()),
            None => state.offset,
        }
    }

    /// Take the server's clock from the `Date` of a response that arrived at
    /// `local_now`. Returns the offset now in use.
    ///
    /// A reading within the dead band of the current offset is the same clock
    /// seen through different network latency, and is ignored so "now" does
    /// not jitter by a second each time a challenge arrives.
    pub fn observe_http_date(&self, date: Option<&str>, local_now: u64) -> i64 {
        self.observe_at(date, local_now, steady_millis())
    }

    fn observe_at(&self, date: Option<&str>, local_now: u64, steady_ms: Option<u64>) -> i64 {
        let mut state = self.state();
        let current = state.settle(local_now, steady_ms);
        let Some(observed) = date.and_then(|date| offset_from_http_date(date, local_now)) else {
            return current;
        };
        if (observed - current).abs() > CLOCK_OFFSET_DEAD_BAND_SECONDS {
            state.offset = observed;
        }
        // From here a step of this computer's clock is measured against the
        // server's time now.
        state.anchor = steady_ms.map(|steady_ms| Anchor {
            local: local_now,
            steady_ms,
        });
        state.offset
    }

    /// `local_now` on the server's clock.
    pub fn now(&self, local_now: u64) -> u64 {
        self.now_at(local_now, steady_millis())
    }

    fn now_at(&self, local_now: u64, steady_ms: Option<u64>) -> u64 {
        corrected_now(local_now, self.state().settle(local_now, steady_ms))
    }
}

/// Milliseconds on a steady clock: one nobody sets, that runs on while the
/// computer sleeps (Rust's `Instant` stops then on macOS, Linux and Android,
/// and a sleep would read as this computer's clock being set). None where
/// there is none: protocol time is then this computer's clock and the
/// offset, as it was.
fn steady_millis() -> Option<u64> {
    steady::millis()
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
mod steady {
    pub(super) fn millis() -> Option<u64> {
        // Linux and Android: the boot clock counts suspend. Apple: its
        // CLOCK_MONOTONIC goes on while asleep (CLOCK_UPTIME_RAW does not).
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
        #[cfg(target_vendor = "apple")]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
        // SAFETY: a zeroed timespec is a valid value, and clock_gettime only
        // writes the one it is given.
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(CLOCK, &mut now) } != 0 {
            return None;
        }
        let seconds = u64::try_from(now.tv_sec).ok()?;
        let nanos = u64::try_from(now.tv_nsec).ok()?;
        seconds.checked_mul(1_000)?.checked_add(nanos / 1_000_000)
    }
}

#[cfg(windows)]
mod steady {
    #[link(name = "kernel32")]
    extern "system" {
        /// Milliseconds since Windows started, sleep and hibernation included.
        fn GetTickCount64() -> u64;
    }

    pub(super) fn millis() -> Option<u64> {
        // SAFETY: no arguments, no state; it only reads the tick count.
        Some(unsafe { GetTickCount64() })
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
mod steady {
    pub(super) fn millis() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: u64 = 1_791_000_000;

    fn http_date(unix: i64) -> String {
        chrono::DateTime::from_timestamp(unix, 0)
            .unwrap()
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string()
    }

    #[test]
    fn a_clock_in_step_is_left_alone() {
        for skew in -CLOCK_OFFSET_DEAD_BAND_SECONDS..=CLOCK_OFFSET_DEAD_BAND_SECONDS {
            let date = http_date(LOCAL as i64 + skew);
            assert_eq!(offset_from_http_date(&date, LOCAL), Some(0), "skew {skew}");
        }
    }

    #[test]
    fn a_clock_out_of_step_is_moved_onto_the_servers() {
        for skew in [-3_600, -61, -6, -3, 3, 6, 61, 3_600] {
            let date = http_date(LOCAL as i64 + skew);
            let offset = offset_from_http_date(&date, LOCAL).unwrap();
            assert_eq!(offset, skew);
            assert_eq!(corrected_now(LOCAL, offset), (LOCAL as i64 + skew) as u64);
        }
    }

    #[test]
    fn a_wild_or_unreadable_date_changes_nothing() {
        assert_eq!(offset_from_http_date(&http_date(LOCAL as i64 + 3_601), LOCAL), None);
        assert_eq!(offset_from_http_date(&http_date(LOCAL as i64 - 86_400), LOCAL), None);
        assert_eq!(offset_from_http_date("yesterday", LOCAL), None);
        assert_eq!(offset_from_http_date("", LOCAL), None);
        let clock = ServerClock::new();
        assert_eq!(clock.observe_http_date(Some(&http_date(LOCAL as i64 + 30)), LOCAL), 30);
        assert_eq!(clock.observe_http_date(Some("not a date"), LOCAL), 30);
        assert_eq!(clock.observe_http_date(None, LOCAL), 30);
        assert_eq!(
            clock.observe_http_date(Some(&http_date(LOCAL as i64 + 7_200)), LOCAL),
            30
        );
    }

    /// A steady clock reading, in ms, `seconds` after the observation.
    fn steady(seconds: u64) -> Option<u64> {
        Some(5_000_000 + seconds * 1_000)
    }

    #[test]
    fn latency_jitter_does_not_move_the_clock_but_a_real_change_does() {
        let clock = ServerClock::new();
        let date = |unix: u64| http_date(unix as i64);
        assert_eq!(clock.observe_at(Some(&date(LOCAL - 20)), LOCAL, steady(0)), -20);
        assert_eq!(clock.now_at(LOCAL, steady(0)), LOCAL - 20);
        // The same clock a second or two later through a slower network.
        assert_eq!(clock.observe_at(Some(&date(LOCAL - 21)), LOCAL, steady(0)), -20);
        assert_eq!(clock.observe_at(Some(&date(LOCAL - 18)), LOCAL, steady(0)), -20);
        // The server's clock itself moved (or a new server): taken.
        assert_eq!(clock.observe_at(Some(&date(LOCAL + 145)), LOCAL + 100, steady(100)), 45);
        assert_eq!(clock.now_at(LOCAL + 110, steady(110)), LOCAL + 155);
        // And a server that sends no Date leaves it as it is.
        assert_eq!(clock.observe_at(None, LOCAL + 120, steady(120)), 45);
    }

    /// Review 4 #9: the server's time is read only as a session connects. A
    /// phone 20 s fast whose clock network time puts right a minute later
    /// had protocol time 20 s slow until it connected again: the step shows
    /// against the steady clock, and protocol time goes on as the server's.
    #[test]
    fn a_clock_set_while_connected_leaves_protocol_time_as_the_servers() {
        let date = |unix: u64| http_date(unix as i64);
        // Set back (a fast clock put right, or set by hand).
        let clock = ServerClock::new();
        assert_eq!(clock.observe_at(Some(&date(LOCAL - 20)), LOCAL, steady(0)), -20);
        assert_eq!(clock.now_at(LOCAL + 60, steady(60)), LOCAL + 40);
        let set_right = LOCAL + 60 - 20;
        assert_eq!(clock.now_at(set_right, steady(60)), LOCAL + 40);
        assert_eq!(clock.now_at(set_right + 30, steady(90)), LOCAL + 70);
        assert_eq!(clock.observe_at(Some(&date(LOCAL + 70)), set_right + 30, steady(90)), 0, "in step now");
        // Set forward (a slow clock put right).
        let clock = ServerClock::new();
        assert_eq!(clock.observe_at(Some(&date(LOCAL + 30)), LOCAL, steady(0)), 30);
        assert_eq!(clock.now_at(LOCAL + 10 + 30, steady(10)), LOCAL + 40);
        assert_eq!(clock.now_at(LOCAL + 50 + 30, steady(50)), LOCAL + 80);
        // A clock in step at the observation, then set ten minutes wrong: the
        // offset follows. Then set two hours the other way: past the bound,
        // the offset stays as it was.
        let clock = ServerClock::new();
        assert_eq!(clock.observe_at(Some(&date(LOCAL)), LOCAL, steady(0)), 0);
        assert_eq!(clock.now_at(LOCAL - 600, steady(5)), LOCAL + 5);
        assert_eq!(clock.now_at(LOCAL + 7_200, steady(6)), LOCAL + 7_200 + 605, "past the bound: not followed");
    }

    /// A sleep, rounding and drift are not a step, and with no steady clock
    /// or no observation nothing changes from before.
    #[test]
    fn only_a_step_moves_the_offset() {
        let date = |unix: u64| http_date(unix as i64);
        let clock = ServerClock::new();
        assert_eq!(clock.observe_at(Some(&date(LOCAL - 20)), LOCAL, steady(0)), -20);
        // An hour asleep: the steady clock ran on.
        assert_eq!(clock.now_at(LOCAL + 3_600, steady(3_600)), LOCAL + 3_580);
        // Whole seconds read a moment apart.
        assert_eq!(clock.now_at(LOCAL + 3_602, Some(5_000_000 + 3_600_400)), LOCAL + 3_582);
        // A day without a read, and the clock drifted (or was slewed) 30 s.
        assert_eq!(clock.now_at(LOCAL + 3_602 + 86_430, steady(3_600 + 86_400)), LOCAL + 3_582 + 86_430);

        // Nowhere to measure a step against: this computer's clock and the offset.
        let clock = ServerClock::new();
        assert_eq!(clock.observe_at(Some(&date(LOCAL - 20)), LOCAL, None), -20);
        assert_eq!(clock.now_at(LOCAL - 300, None), LOCAL - 320);
        // No server time yet: this computer's clock, set or not.
        let clock = ServerClock::new();
        assert_eq!(clock.now_at(LOCAL, steady(0)), LOCAL);
        assert_eq!(clock.now_at(LOCAL - 300, steady(1)), LOCAL - 300);
        assert_eq!(clock.observe_at(None, LOCAL - 300, steady(2)), 0);
    }

    #[test]
    fn the_steady_clock_runs() {
        if let Some(first) = steady_millis() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let second = steady_millis().expect("still there");
            assert!(second >= first + 15, "{first} {second}");
        }
        assert_eq!(
            cfg!(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)),
            steady_millis().is_some()
        );
    }

    #[test]
    fn the_corrected_clock_never_reaches_zero_or_wraps() {
        assert_eq!(corrected_now(10, -3_600), 1);
        assert_eq!(corrected_now(u64::MAX - 1, 3_600), u64::MAX);
    }
}
