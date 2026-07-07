//! Retention configuration — how long call_logs / transcripts /
//! recordings / SMS are kept before automatic purge.
//!
//! Persisted to `<app_data>/retention.json` so operators can adjust
//! without rebuilding the app. `database::init_database` reads the
//! config at startup and uses it to drive the once-per-launch purge.
//!
//! The on-disk schema is conservative on purpose: a missing or
//! malformed file falls back to the bundled defaults (90-day call
//! retention, SMS kept forever) rather than blocking app launch.
//! Negative values are clamped to 0 (`KEEP_FOREVER`) on read; that
//! defends against a hand-edited file with a typo silently
//! triggering a "delete everything" purge.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const FILENAME: &str = "retention.json";

/// Sentinel value meaning "never auto-purge". Stored as the integer 0
/// in the on-disk file. The purge functions short-circuit when the
/// configured retention is `KEEP_FOREVER`, so the SQL never sees a
/// negative or zero cutoff that would scoop up the entire table.
pub const KEEP_FOREVER: i64 = 0;

/// Upper bound clamp on `*_history_days`. 10 years is a comfortable
/// ceiling for any realistic retention policy and keeps
/// `days * 24 * 60 * 60 * 1000` well inside i64 — without a cap, a
/// hand-edited file or a malformed IPC payload could push the cutoff
/// math toward `i64::MAX` and overflow. Defensive: the purge
/// functions also use `checked_mul`, so a value past this cap would
/// fail the multiplication rather than overflow silently.
pub const MAX_RETENTION_DAYS: i64 = 3650;

/// Default retention bounds.
///
/// Calls: 90 days, matching the previous hard-coded `RETENTION_DAYS`.
///
/// SMS: 90 days, flipped from `KEEP_FOREVER` per reviewer R3-#13.
/// "Forever" was the original default on the assumption that
/// operators expect long conversation history, but a privacy review
/// flagged silent-forever as a real risk for normal businesses (every
/// SMS body sits on disk indefinitely). The wizard still surfaces a
/// "Forever" option in the retention step — operators who genuinely
/// want unbounded retention can pick it explicitly. Existing installs
/// with a saved retention.json keep whatever value they wrote (only
/// the no-file fresh-install path reaches this default).
pub fn default_call_history_days() -> i64 {
    90
}
pub fn default_sms_history_days() -> i64 {
    90
}
/// R8/P0-3: call recording defaults to OFF. The runtime checks this
/// flag at every CallAnswered before opening a `recording::CallRecorder`,
/// so legacy installs that never wrote `recordings_enabled` (the
/// `#[serde(default)]` falls through to `false`) keep the historical
/// "no recording" behaviour. Two-party-consent jurisdictions (NSW,
/// most of the US, EU) require caller notification before recording —
/// that's an operator-policy duty we surface in the toggle copy
/// (consent wizard + Settings → Privacy & retention) but don't
/// enforce mechanically.
pub fn default_recordings_enabled() -> bool {
    false
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionConfig {
    /// Keep call_logs / transcripts / recording WAVs for this many
    /// days. `KEEP_FOREVER` (0) disables the call-side purge entirely.
    #[serde(default = "default_call_history_days")]
    pub call_history_days: i64,
    /// Keep SMS thread bodies for this many days. `KEEP_FOREVER` (0)
    /// disables the SMS purge — operators tend to expect long history.
    #[serde(default = "default_sms_history_days")]
    pub sms_history_days: i64,
    /// R8/P0-3: master switch for the SCO-to-WAV recorder. Off by
    /// default. When true, every CallAnswered opens a per-call
    /// `recording::CallRecorder` that writes caller-side PCM to
    /// `<app_data>/recordings/<call_id>.wav` until CallTerminated.
    /// Surface in two places: the consent wizard's behaviour step
    /// (first-run opt-in with the legal warning) and Settings →
    /// Privacy & retention (ongoing toggle for operators who change
    /// their mind later).
    #[serde(default = "default_recordings_enabled")]
    pub recordings_enabled: bool,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            call_history_days: default_call_history_days(),
            sms_history_days: default_sms_history_days(),
            recordings_enabled: default_recordings_enabled(),
        }
    }
}

fn config_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join(FILENAME)
}

/// Read the retention config from disk. Returns the default
/// (90-day calls, SMS forever) when the file is missing, unreadable,
/// or malformed — we never want a parse error to block app launch.
/// Negative values are clamped to `KEEP_FOREVER`.
pub fn load(app_data_dir: &Path) -> RetentionConfig {
    let path = config_path(app_data_dir);
    if !path.exists() {
        return RetentionConfig::default();
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[retention] read {:?} failed: {} — using defaults", path, e);
            return RetentionConfig::default();
        }
    };
    match serde_json::from_str::<RetentionConfig>(&raw) {
        Ok(mut cfg) => {
            cfg.call_history_days = clamp_days(cfg.call_history_days);
            cfg.sms_history_days = clamp_days(cfg.sms_history_days);
            cfg
        }
        Err(e) => {
            eprintln!(
                "[retention] parse {:?} failed: {} — using defaults",
                path, e
            );
            RetentionConfig::default()
        }
    }
}

/// Clamp an `*_history_days` value to `[KEEP_FOREVER, MAX_RETENTION_DAYS]`.
/// Negative → `KEEP_FOREVER` (treats a typo as the safe interpretation
/// rather than as an immediate-purge cutoff). Above-cap → `MAX_RETENTION_DAYS`
/// so the purge math stays well inside i64. Public so the IPC boundary
/// (`set_retention`) can apply the same rule before writing.
pub fn clamp_days(days: i64) -> i64 {
    days.clamp(KEEP_FOREVER, MAX_RETENTION_DAYS)
}

/// Atomic write of the retention config (write-tmp + fsync + rename).
/// A crash mid-save would otherwise leave a half-written file that
/// the next launch would silently fall back from to defaults — and
/// that's a privacy regression, since defaults purge call_logs at
/// 90d which the operator may have lengthened intentionally.
pub fn save(app_data_dir: &Path, cfg: &RetentionConfig) -> Result<(), String> {
    let path = config_path(app_data_dir);
    let json = serde_json::to_string_pretty(cfg).map_err(|e| format!("serialize: {}", e))?;
    crate::paths::atomic_write(&path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let p =
                std::env::temp_dir().join(format!("aokie_retention_test_{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_load_when_file_absent() {
        let tmp = TestDir::new();
        let cfg = load(tmp.path());
        // R3-#13: SMS default flipped to 90 days (same as calls). The
        // wizard still surfaces a "Forever" option for operators who
        // need the full conversation log on hand.
        assert_eq!(cfg.call_history_days, 90);
        assert_eq!(cfg.sms_history_days, 90);
        // R8/P0-3: recording defaults to OFF — must never become opt-out.
        // Legal exposure if a fresh install starts recording without the
        // operator explicitly clicking the toggle.
        assert!(!cfg.recordings_enabled);
    }

    #[test]
    fn round_trip() {
        let tmp = TestDir::new();
        let cfg = RetentionConfig {
            call_history_days: 30,
            sms_history_days: 180,
            recordings_enabled: true,
        };
        save(tmp.path(), &cfg).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded, cfg);
    }

    /// R8/P0-3: a retention.json from a previous install has no
    /// `recordings_enabled` field (the schema was added later).
    /// `#[serde(default)]` must keep that legacy file readable AND
    /// land on the safe `false`, NOT inherit from any in-memory
    /// default that might flip later.
    #[test]
    fn legacy_file_without_recordings_enabled_loads_off() {
        let tmp = TestDir::new();
        let path = config_path(tmp.path());
        fs::write(&path, r#"{"call_history_days":90,"sms_history_days":90}"#).unwrap();
        let cfg = load(tmp.path());
        assert!(!cfg.recordings_enabled);
        assert_eq!(cfg.call_history_days, 90);
        assert_eq!(cfg.sms_history_days, 90);
    }

    #[test]
    fn negative_values_clamp_to_keep_forever() {
        let tmp = TestDir::new();
        let path = config_path(tmp.path());
        fs::write(
            &path,
            r#"{"call_history_days":-5,"sms_history_days":-9,"recordings_enabled":false}"#,
        )
        .unwrap();
        let cfg = load(tmp.path());
        assert_eq!(cfg.call_history_days, KEEP_FOREVER);
        assert_eq!(cfg.sms_history_days, KEEP_FOREVER);
    }

    #[test]
    fn huge_values_clamp_to_max_retention() {
        let tmp = TestDir::new();
        let path = config_path(tmp.path());
        // i64::MAX-shaped value would overflow days*86_400_000 in the
        // purge cutoff math. Clamp to MAX_RETENTION_DAYS (10 years)
        // defends against a hand-edited / malicious file.
        fs::write(
            &path,
            r#"{"call_history_days":9223372036854775000,"sms_history_days":1000000000,"recordings_enabled":false}"#,
        )
        .unwrap();
        let cfg = load(tmp.path());
        assert_eq!(cfg.call_history_days, MAX_RETENTION_DAYS);
        assert_eq!(cfg.sms_history_days, MAX_RETENTION_DAYS);
    }

    #[test]
    fn clamp_days_handles_extremes() {
        assert_eq!(clamp_days(-1), KEEP_FOREVER);
        assert_eq!(clamp_days(0), KEEP_FOREVER);
        assert_eq!(clamp_days(90), 90);
        assert_eq!(clamp_days(MAX_RETENTION_DAYS), MAX_RETENTION_DAYS);
        assert_eq!(clamp_days(MAX_RETENTION_DAYS + 1), MAX_RETENTION_DAYS);
        assert_eq!(clamp_days(i64::MAX), MAX_RETENTION_DAYS);
        assert_eq!(clamp_days(i64::MIN), KEEP_FOREVER);
    }

    #[test]
    fn malformed_falls_back_to_default() {
        let tmp = TestDir::new();
        let path = config_path(tmp.path());
        fs::write(&path, "not json").unwrap();
        let cfg = load(tmp.path());
        assert_eq!(cfg, RetentionConfig::default());
    }
}
