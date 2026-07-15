//! Device-wide, restart-safe manager PIN throttling.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const FILE: &str = "manager-auth.json";
const MAX_FAILURES: u32 = 5;
const LOCKOUT_SECS: u64 = 15 * 60;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    failures: u32,
    lockout_until: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Verified,
    Rejected { remaining: u32 },
    Locked { retry_after_secs: u64 },
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
    load(data_dir).lockout_until.saturating_sub(now_secs())
}

pub fn verify(data_dir: &Path, expected: &str, given: &str) -> Decision {
    let mut state = load(data_dir);
    let now = now_secs();
    if state.lockout_until > now {
        return Decision::Locked {
            retry_after_secs: state.lockout_until - now,
        };
    }
    if !expected.is_empty() && expected == given {
        state = State::default();
        save(data_dir, &state);
        return Decision::Verified;
    }

    state.failures = state.failures.saturating_add(1);
    eprintln!(
        "[aokie-plugin] manager authentication failed (device failure count {})",
        state.failures
    );
    if state.failures >= MAX_FAILURES {
        state.failures = 0;
        state.lockout_until = now.saturating_add(LOCKOUT_SECS);
        save(data_dir, &state);
        Decision::Locked {
            retry_after_secs: LOCKOUT_SECS,
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
