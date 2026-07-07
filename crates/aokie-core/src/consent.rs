//! First-run consent record.
//!
//! Aokie is a privacy-sensitive product: it touches Bluetooth pairing,
//! call audio, contacts, SMS, transcripts, recordings, bookings, and
//! orders. Before any of that wakes up, the operator has to walk
//! through the consent wizard and explicitly accept the access /
//! retention / behavior summary. This module is the durable record of
//! that acceptance.
//!
//! Persisted to `<app_data>/consent.json`. The file's existence isn't
//! enough on its own — the consent stream is versioned, and the
//! frontend treats `accepted_version != CURRENT_CONSENT_VERSION` as
//! "not accepted yet". Bumping the version forces the wizard to re-
//! prompt when consent text changes materially (e.g. when a future
//! release ships call recording and the user needs a fresh chance to
//! opt in).
//!
//! The file deliberately does NOT mirror the user's runtime
//! preferences (auto-answer, retention windows). Those are owned by
//! their respective subsystems (`BluetoothState`, `retention.json`).
//! Mixing them here would create two sources of truth for the same
//! settings; instead the wizard applies those preferences via the
//! existing `configure_bluetooth` / `set_retention` IPC commands and
//! this file only records that consent itself was given.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILENAME: &str = "consent.json";

/// Bump this when the consent text changes materially — e.g. a new
/// access surface (recording, cloud LLM by default), a new retention
/// default that flows from the wizard, or a new auto-action. Bumping
/// makes the frontend re-show the wizard on next launch.
///
/// v2: R8/P0-3 ships the SCO-to-WAV recorder. The behaviour-step's
/// "Call recording" line is now a real opt-in toggle (default off)
/// instead of a "feature not enabled" marker, so existing v1 consenters
/// haven't yet been shown the recording opt-in or the
/// two-party-consent legal warning. Re-prompt forces them through it.
pub const CURRENT_CONSENT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsentRecord {
    /// Schema + content version the user agreed to. Compared against
    /// `CURRENT_CONSENT_VERSION` at status-check time; mismatches are
    /// treated as "needs re-consent".
    pub version: u32,
    /// ISO-8601 UTC timestamp the wizard's Accept button was clicked.
    /// Surfaced in the privacy section of Settings so the operator can
    /// see when consent was last refreshed.
    pub accepted_at: String,
}

fn config_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(FILENAME)
}

/// Read the consent record from disk. Returns `Ok(None)` for a missing
/// or unreadable / malformed file — callers treat any of those as
/// "not yet accepted" and show the wizard.
pub fn load(app_data_dir: &Path) -> Result<Option<ConsentRecord>, String> {
    let path = config_path(app_data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[consent] read {:?} failed: {} — treating as not-accepted",
                path, e
            );
            return Ok(None);
        }
    };
    match serde_json::from_str::<ConsentRecord>(&raw) {
        Ok(record) => Ok(Some(record)),
        Err(e) => {
            // A corrupted file should drop the user back into the
            // wizard, not block app launch.
            eprintln!(
                "[consent] parse {:?} failed: {} — treating as not-accepted",
                path, e
            );
            Ok(None)
        }
    }
}

/// Persist a fresh consent record (write-tmp + fsync + rename via
/// `paths::atomic_write`, which on Unix also drops the file to 0600).
/// `accepted_at` is generated here so the frontend can't lie about the
/// timestamp via IPC.
pub fn save(app_data_dir: &Path) -> Result<ConsentRecord, String> {
    let record = ConsentRecord {
        version: CURRENT_CONSENT_VERSION,
        accepted_at: now_iso8601(),
    };
    let path = config_path(app_data_dir);
    let json = serde_json::to_string_pretty(&record).map_err(|e| format!("serialize: {}", e))?;
    crate::paths::atomic_write(&path, json.as_bytes())?;
    Ok(record)
}

/// Best-effort delete of the consent file. Used by `revoke_consent`,
/// which puts the user back into the wizard on next launch. A missing
/// file isn't an error — that's already the "no consent recorded"
/// state we're trying to reach.
pub fn revoke(app_data_dir: &Path) -> Result<(), String> {
    let path = config_path(app_data_dir);
    match std::fs::remove_file(&path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {:?}: {}", path, e)),
    }
}

/// True when a consent record exists on disk AND its version matches
/// `CURRENT_CONSENT_VERSION`. Read errors / missing files / version
/// mismatches all collapse to `false` — the safe-by-default direction
/// for a backend gate. Cheap-but-not-free (one file read per call); the
/// volume of sensitive command invocations is low enough that caching
/// would be premature.
pub fn is_accepted(app_data_dir: &Path) -> bool {
    matches!(load(app_data_dir), Ok(Some(r)) if r.version == CURRENT_CONSENT_VERSION)
}

/// ISO-8601 UTC timestamp ("YYYY-MM-DDTHH:MM:SSZ", seconds precision).
/// Defers to `chrono` (already in deps) instead of rolling civil-day
/// math by hand — the previous home-grown version had an off-by-60-
/// days bug at the days→date step, so a freshly recorded acceptance
/// landed in late 1969 instead of 2026.
fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_via_save_load() {
        let dir = std::env::temp_dir().join(format!(
            "aokie_consent_test_{}_{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(load(&dir).unwrap(), None);

        let saved = save(&dir).unwrap();
        assert_eq!(saved.version, CURRENT_CONSENT_VERSION);
        assert!(saved.accepted_at.ends_with('Z'));

        let loaded = load(&dir).unwrap().unwrap();
        assert_eq!(loaded, saved);

        revoke(&dir).unwrap();
        assert_eq!(load(&dir).unwrap(), None);

        // Idempotent — revoking a missing file is fine.
        revoke(&dir).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_timestamp_is_in_a_sane_year() {
        let s = now_iso8601();
        // YYYY-MM-DDTHH:MM:SSZ → 20 chars.
        assert_eq!(s.len(), 20, "{}", s);
        let year: i32 = s[0..4].parse().unwrap();
        // Catches the previous off-by-60-days bug that produced 1969
        // for any current `SystemTime::now()`. Anything in
        // [2024, 2100) is "obviously now" without baking the test
        // host's exact date in.
        assert!(year >= 2024 && year < 2100, "year out of range: {}", year);
    }

    /// Spot-check the underlying chrono path against known timestamps
    /// — the previous custom date math returned 1969-11-02 for the
    /// unix epoch, so even a smoke test is worth having.
    #[test]
    fn iso_timestamp_for_known_unix_seconds() {
        use chrono::TimeZone;
        let cases: &[(i64, &str)] = &[
            (0, "1970-01-01T00:00:00Z"),
            // 2000-02-29 (leap day) — exercises the leap-year handling.
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_000_000_000, "2001-09-09T01:46:40Z"),
        ];
        for (secs, expected) in cases {
            let dt = chrono::Utc.timestamp_opt(*secs, 0).unwrap();
            let formatted = dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            assert_eq!(formatted, *expected, "secs={}", secs);
        }
    }

    #[test]
    fn iso_timestamp_format_is_well_formed() {
        let s = now_iso8601();
        assert_eq!(s.len(), 20, "{}", s);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert_eq!(&s[19..], "Z");
    }
}
