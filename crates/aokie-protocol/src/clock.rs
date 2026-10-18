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

use std::sync::atomic::{AtomicI64, Ordering};

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
    offset: AtomicI64,
}

impl ServerClock {
    pub const fn new() -> Self {
        Self {
            offset: AtomicI64::new(0),
        }
    }

    /// Seconds the server's clock is ahead of this one.
    pub fn offset(&self) -> i64 {
        self.offset.load(Ordering::Relaxed)
    }

    /// Take the server's clock from the `Date` of a response that arrived at
    /// `local_now`. Returns the offset now in use.
    ///
    /// A reading within the dead band of the current offset is the same clock
    /// seen through different network latency, and is ignored so "now" does
    /// not jitter by a second each time a challenge arrives.
    pub fn observe_http_date(&self, date: Option<&str>, local_now: u64) -> i64 {
        let current = self.offset();
        let Some(observed) = date.and_then(|date| offset_from_http_date(date, local_now)) else {
            return current;
        };
        if (observed - current).abs() <= CLOCK_OFFSET_DEAD_BAND_SECONDS {
            return current;
        }
        self.offset.store(observed, Ordering::Relaxed);
        observed
    }

    /// `local_now` on the server's clock.
    pub fn now(&self, local_now: u64) -> u64 {
        corrected_now(local_now, self.offset())
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

    #[test]
    fn latency_jitter_does_not_move_the_clock_but_a_real_change_does() {
        let clock = ServerClock::new();
        assert_eq!(clock.observe_http_date(Some(&http_date(LOCAL as i64 - 20)), LOCAL), -20);
        assert_eq!(clock.now(LOCAL), LOCAL - 20);
        // The same clock a second or two later through a slower network.
        assert_eq!(clock.observe_http_date(Some(&http_date(LOCAL as i64 - 21)), LOCAL), -20);
        assert_eq!(clock.observe_http_date(Some(&http_date(LOCAL as i64 - 18)), LOCAL), -20);
        // The phone's clock was set right: it is back in step, and the
        // correction goes.
        let fixed = LOCAL - 20;
        assert_eq!(clock.observe_http_date(Some(&http_date(fixed as i64 + 1)), fixed), 0);
        assert_eq!(clock.now(fixed), fixed);
        // A real jump is taken.
        assert_eq!(clock.observe_http_date(Some(&http_date(LOCAL as i64 + 45)), LOCAL), 45);
    }

    #[test]
    fn the_corrected_clock_never_reaches_zero_or_wraps() {
        assert_eq!(corrected_now(10, -3_600), 1);
        assert_eq!(corrected_now(u64::MAX - 1, 3_600), u64::MAX);
    }
}
