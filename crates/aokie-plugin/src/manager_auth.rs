//! Device-wide, restart-safe manager PIN throttling.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const FILE: &str = "manager-auth.json";
const MAX_FAILURES: u32 = 5;
/// The lockout after each run of [`MAX_FAILURES`] wrong PINs, growing: a
/// fixed 15 minutes let anyone try about 480 PINs a day for ever, and a
/// four-digit PIN fell in about ten days. Kept across restarts, cleared by
/// a right PIN (or a week with no wrong one).
const LOCKOUT_STEPS_SECS: [u64; 4] = [15 * 60, 60 * 60, 4 * 60 * 60, 24 * 60 * 60];
/// A week with no wrong PIN starts the steps again from the first.
const LOCKOUT_MEMORY_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct State {
    failures: u32,
    lockout_until: u64,
    /// Lockouts so far (which step the next one takes).
    lockouts: u32,
    /// When the last wrong PIN was heard.
    last_failure: u64,
}

/// The lockout that follows `lockouts` earlier ones.
fn lockout_secs(lockouts: u32) -> u64 {
    LOCKOUT_STEPS_SECS[(lockouts as usize).min(LOCKOUT_STEPS_SECS.len() - 1)]
}

/// Seconds left of the lockout at `now`, never more than the longest step: a
/// clock set back (or a lockout written under a wrong clock) must not lock
/// the manager out for a year.
fn remaining(state: &State, now: u64) -> u64 {
    let longest = LOCKOUT_STEPS_SECS[LOCKOUT_STEPS_SECS.len() - 1];
    state.lockout_until.saturating_sub(now).min(longest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Verified,
    Rejected { remaining: u32 },
    Locked { retry_after_secs: u64 },
}

/// Constant-time byte-string equality (AOK-304A). No early return on the first
/// differing byte, so a caller can't learn a correct PIN prefix from response
/// timing. A length mismatch fails immediately — a spoken PIN's LENGTH is not
/// the secret its digits are, and the device-wide 5-attempt lockout already
/// bounds guessing regardless.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

fn load(data_dir: &Path) -> State {
    std::fs::read(path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(data_dir: &Path, state: &State) {
    if let Ok(bytes) = serde_json::to_vec(state) {
        if let Err(error) = aokie_core::paths::atomic_write(&path(data_dir), &bytes) {
            eprintln!("[aokie-plugin] manager auth throttle persistence failed: {error}");
        }
    }
}

pub fn lockout_remaining_secs(data_dir: &Path) -> u64 {
    remaining(&load(data_dir), now_secs())
}

pub fn verify(data_dir: &Path, expected: &str, given: &str) -> Decision {
    verify_at(data_dir, expected, given, now_secs())
}

fn verify_at(data_dir: &Path, expected: &str, given: &str, now: u64) -> Decision {
    let mut state = load(data_dir);
    let left = remaining(&state, now);
    if left > 0 {
        if state.lockout_until.saturating_sub(now) > left {
            // (clamped: keep what is on disk truthful too)
            state.lockout_until = now.saturating_add(left);
            save(data_dir, &state);
        }
        return Decision::Locked {
            retry_after_secs: left,
        };
    }
    if !expected.is_empty() && ct_eq(expected.as_bytes(), given.as_bytes()) {
        state = State::default();
        save(data_dir, &state);
        return Decision::Verified;
    }

    if state.last_failure != 0 && now.saturating_sub(state.last_failure) > LOCKOUT_MEMORY_SECS {
        state.lockouts = 0;
    }
    state.last_failure = now;
    state.failures = state.failures.saturating_add(1);
    eprintln!(
        "[aokie-plugin] manager authentication failed (device failure count {})",
        state.failures
    );
    if state.failures >= MAX_FAILURES {
        let secs = lockout_secs(state.lockouts);
        state.failures = 0;
        state.lockouts = state.lockouts.saturating_add(1);
        state.lockout_until = now.saturating_add(secs);
        eprintln!(
            "[aokie-plugin] manager PIN locked for {} min after {} lockout(s) — wrong PINs keep lengthening it until a right one",
            secs / 60,
            state.lockouts
        );
        save(data_dir, &state);
        Decision::Locked {
            retry_after_secs: secs,
        }
    } else {
        let remaining = MAX_FAILURES - state.failures;
        save(data_dir, &state);
        Decision::Rejected { remaining }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_and_lockout_survive_reloads() {
        let dir = tempfile::tempdir().unwrap();
        for expected_remaining in [4, 3, 2, 1] {
            assert_eq!(
                verify(dir.path(), "731905", "000000"),
                Decision::Rejected {
                    remaining: expected_remaining
                }
            );
        }
        assert!(matches!(
            verify(dir.path(), "731905", "000000"),
            Decision::Locked { .. }
        ));
        assert!(lockout_remaining_secs(dir.path()) > 0);
        assert!(matches!(
            verify(dir.path(), "731905", "731905"),
            Decision::Locked { .. }
        ));
    }

    /// Review 2026-10: each lockout is longer than the last (15 min, 1 h,
    /// 4 h, 24 h), kept across restarts; a right PIN, or a quiet week, starts
    /// over. A clock set back cannot lock the manager out past the longest.
    #[test]
    fn lockouts_escalate_persist_and_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let t0 = 1_800_000_000u64;
        let lock_run = |now: u64| {
            for _ in 0..(MAX_FAILURES - 1) {
                assert!(matches!(
                    verify_at(dir.path(), "731905", "000000", now),
                    Decision::Rejected { .. }
                ));
            }
            match verify_at(dir.path(), "731905", "000000", now) {
                Decision::Locked { retry_after_secs } => retry_after_secs,
                other => panic!("expected a lockout, got {other:?}"),
            }
        };
        assert_eq!(lock_run(t0), 15 * 60);
        let t1 = t0 + 15 * 60;
        assert_eq!(lock_run(t1), 60 * 60);
        let t2 = t1 + 60 * 60;
        assert_eq!(lock_run(t2), 4 * 60 * 60);
        let t3 = t2 + 4 * 60 * 60;
        assert_eq!(lock_run(t3), 24 * 60 * 60);
        let t4 = t3 + 24 * 60 * 60;
        assert_eq!(lock_run(t4), 24 * 60 * 60, "the longest step holds");
        // Locked: even the right PIN waits.
        assert!(matches!(
            verify_at(dir.path(), "731905", "731905", t4 + 60),
            Decision::Locked { .. }
        ));
        // A clock set back a year: the wait is clamped to the longest step.
        match verify_at(dir.path(), "731905", "731905", t4 - 365 * 24 * 60 * 60) {
            Decision::Locked { retry_after_secs } => assert!(retry_after_secs <= 24 * 60 * 60),
            other => panic!("expected a clamped lockout, got {other:?}"),
        }
        // After it, the right PIN verifies and the steps start over.
        let t5 = t4 + 24 * 60 * 60;
        assert_eq!(verify_at(dir.path(), "731905", "731905", t5), Decision::Verified);
        assert_eq!(lock_run(t5), 15 * 60);
        // A quiet week also starts over.
        let t6 = t5 + 15 * 60;
        assert_eq!(lock_run(t6), 60 * 60);
        let t7 = t6 + 60 * 60 + LOCKOUT_MEMORY_SECS + 1;
        assert_eq!(lock_run(t7), 15 * 60);
    }

    #[test]
    fn ct_eq_matches_only_identical_bytes() {
        assert!(ct_eq(b"731905", b"731905"));
        assert!(!ct_eq(b"731905", b"731906")); // last digit differs
        assert!(!ct_eq(b"731905", b"831905")); // first digit differs
        assert!(!ct_eq(b"731905", b"73190")); // length differs
        assert!(!ct_eq(b"", b"0")); // empty vs non-empty
        assert!(ct_eq(b"", b"")); // both empty (verify() gates empty separately)
    }

    #[test]
    fn empty_expected_pin_never_verifies() {
        // A blank managerPin is a read-only manager line — an empty spoken PIN
        // must never authenticate, even though ct_eq("","") is true.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            verify(dir.path(), "", ""),
            Decision::Rejected { .. }
        ));
    }

    #[test]
    fn successful_auth_clears_accumulated_failures() {
        let dir = tempfile::tempdir().unwrap();
        let _ = verify(dir.path(), "731905", "111111");
        assert_eq!(verify(dir.path(), "731905", "731905"), Decision::Verified);
        assert_eq!(
            verify(dir.path(), "731905", "111111"),
            Decision::Rejected { remaining: 4 }
        );
    }
}
