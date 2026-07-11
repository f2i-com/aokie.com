//! AOK-CONSENT-001 — consent gate for sensitive processing.
//!
//! Aokie touches Bluetooth pairing, call audio, contacts (PBAP), SMS
//! (MAP), transcription and (future) recording. Before any of that runs,
//! the operator must have accepted a versioned, scoped consent grant. The
//! grant is issued by the FormLogic control plane (the operator accepts a
//! wizard; FormLogic sends `consent.set` to this plugin) and persisted
//! here as `<plugin-data>/consent.json`.
//!
//! This module owns the DURABLE RECORD and the PURE GATE
//! ([`evaluate`]) — it decides nothing about wording (that's a product /
//! legal decision surfaced in FormLogic). The gate is enforced at the
//! plugin's sensitive entry points: the radio never starts (so no
//! pairing / call / STT) without the `bluetooth` scope, and `sms.*` needs
//! the `sms` scope.
//!
//! ## Mode
//!
//! The `consentMode` setting picks the posture:
//!
//! - `off` — gate disabled (legacy behaviour; not recommended).
//! - `warn` — **default**. A missing / stale / unscoped grant is logged and
//!   surfaced in `plugin.health`, but sensitive work still runs. This keeps an
//!   already-deployed receptionist working through the upgrade; it does NOT
//!   satisfy the audit's hard-gate acceptance.
//! - `enforce` — production posture. A missing / stale / unscoped grant DENIES
//!   the sensitive operation (radio won't start; `sms.*` refuses). This is what
//!   the FormLogic consent wizard flips on once a grant has been issued.
//!
//! Bumping [`CURRENT_CONSENT_VERSION`] forces re-consent: a grant recorded
//! against an older version no longer satisfies the gate (a materially new
//! access surface / destination must be re-accepted).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bump when the consent surface changes materially (a new access scope, a
/// new default destination, a new retention posture) so operators are
/// forced to re-accept. v1 is the initial scoped grant.
pub const CURRENT_CONSENT_VERSION: u32 = 1;

const FILENAME: &str = "consent.json";

/// The access surfaces a grant can cover. Absent (false) scopes are NOT
/// granted — the gate refuses the corresponding operation under `enforce`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConsentScopes {
    #[serde(default)]
    pub bluetooth: bool,
    #[serde(default)]
    pub contacts: bool,
    #[serde(default)]
    pub sms: bool,
    #[serde(default)]
    pub transcription: bool,
    #[serde(default)]
    pub recording: bool,
    /// Operator-chosen retention window for captured records, if any. Not
    /// gated here (retention is enforced by FormLogic's record TTL); kept
    /// on the grant so a material change forces re-consent via the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
    /// Configured destinations the operator consented to (e.g. the AI /
    /// speech endpoints, SMS/booking targets). Advisory record; a change
    /// is a material change → version bump → re-consent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub destinations: Vec<String>,
}

/// The durable consent record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentGrant {
    /// Consent surface version the operator accepted; compared to
    /// [`CURRENT_CONSENT_VERSION`] at gate time.
    pub version: u32,
    pub scopes: ConsentScopes,
    /// ISO-8601 UTC timestamp the grant was recorded (set by the plugin,
    /// not the caller, so it can't be back-dated over the wire).
    pub accepted_at: String,
    /// The FormLogic identity that accepted (user id / email), if the
    /// control plane supplied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_by: Option<String>,
    /// Reserved: a FormLogic-issued signature over the grant. When a
    /// verification key is provisioned the gate will additionally require a
    /// valid signature; until then it is recorded but not verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Enforcement posture, from the `consentMode` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentMode {
    Off,
    Warn,
    Enforce,
}

impl ConsentMode {
    /// Parse the `consentMode` setting. Anything unrecognised (incl. unset)
    /// falls back to the SAFE default `Warn` — never silently `Off`.
    pub fn from_setting(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("enforce") => ConsentMode::Enforce,
            Some("off") => ConsentMode::Off,
            _ => ConsentMode::Warn,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ConsentMode::Off => "off",
            ConsentMode::Warn => "warn",
            ConsentMode::Enforce => "enforce",
        }
    }
}

/// A sensitive access surface the gate protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Bluetooth,
    Contacts,
    Sms,
    Transcription,
    Recording,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Bluetooth => "bluetooth",
            Scope::Contacts => "contacts",
            Scope::Sms => "sms",
            Scope::Transcription => "transcription",
            Scope::Recording => "recording",
        }
    }
}

fn scope_granted(scopes: &ConsentScopes, scope: Scope) -> bool {
    match scope {
        Scope::Bluetooth => scopes.bluetooth,
        Scope::Contacts => scopes.contacts,
        Scope::Sms => scopes.sms,
        Scope::Transcription => scopes.transcription,
        Scope::Recording => scopes.recording,
    }
}

/// The gate outcome. `Warn` means "allowed, but consent is not properly
/// recorded" — the caller proceeds but should surface the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentDecision {
    Allow,
    Warn(String),
    Deny(String),
}

impl ConsentDecision {
    pub fn is_denied(&self) -> bool {
        matches!(self, ConsentDecision::Deny(_))
    }
    /// The human reason for a warn/deny (empty for allow).
    pub fn reason(&self) -> &str {
        match self {
            ConsentDecision::Allow => "",
            ConsentDecision::Warn(r) | ConsentDecision::Deny(r) => r,
        }
    }
}

/// The pure consent gate: given the recorded grant (if any), the required
/// version, the mode, and the scope, decide whether a sensitive operation
/// may proceed. A missing grant, a version mismatch (re-consent needed), or
/// an ungranted scope is a refusal under `Enforce`, a warning under `Warn`,
/// and ignored under `Off`.
pub fn evaluate(
    grant: Option<&ConsentGrant>,
    required_version: u32,
    mode: ConsentMode,
    scope: Scope,
) -> ConsentDecision {
    if mode == ConsentMode::Off {
        return ConsentDecision::Allow;
    }
    let reason = match grant {
        None => Some("no consent has been recorded for this device".to_string()),
        Some(g) if g.version != required_version => Some(format!(
            "consent version {} was accepted but {} is now required — re-consent needed",
            g.version, required_version
        )),
        Some(g) if !scope_granted(&g.scopes, scope) => {
            Some(format!("consent does not cover {}", scope.as_str()))
        }
        Some(_) => None,
    };
    match (reason, mode) {
        (None, _) => ConsentDecision::Allow,
        (Some(r), ConsentMode::Enforce) => ConsentDecision::Deny(r),
        (Some(r), _) => ConsentDecision::Warn(r),
    }
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILENAME)
}

/// Read the consent record. Returns `None` for a missing / unreadable /
/// malformed file — the gate treats any of those as "not accepted".
pub fn load(data_dir: &Path) -> Option<ConsentGrant> {
    let path = config_path(data_dir);
    let raw = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<ConsentGrant>(&raw) {
        Ok(grant) => Some(grant),
        Err(e) => {
            eprintln!("[aokie-plugin] consent.json parse failed ({e}) — treating as not-accepted");
            None
        }
    }
}

/// Persist a consent grant (atomic write). `accepted_at` is stamped here so
/// a caller can't back-date it over the wire.
pub fn save(data_dir: &Path, mut grant: ConsentGrant) -> Result<ConsentGrant, String> {
    grant.accepted_at = aokie_core::events::now_iso8601();
    let json = serde_json::to_string_pretty(&grant).map_err(|e| format!("serialize consent: {e}"))?;
    aokie_core::paths::atomic_write(&config_path(data_dir), json.as_bytes())?;
    Ok(grant)
}

/// Delete the consent record (revocation). A missing file is not an error —
/// that is already the "no consent" state.
pub fn revoke(data_dir: &Path) -> Result<(), String> {
    let path = config_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {path:?}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_grant() -> ConsentGrant {
        ConsentGrant {
            version: CURRENT_CONSENT_VERSION,
            scopes: ConsentScopes {
                bluetooth: true,
                contacts: true,
                sms: true,
                transcription: true,
                recording: false,
                retention_days: Some(90),
                destinations: vec!["http://127.0.0.1:17920".to_string()],
            },
            accepted_at: "2026-07-11T00:00:00Z".to_string(),
            accepted_by: Some("op@example.com".to_string()),
            signature: None,
        }
    }

    #[test]
    fn mode_parses_safely_defaulting_to_warn() {
        assert_eq!(ConsentMode::from_setting(Some("enforce")), ConsentMode::Enforce);
        assert_eq!(ConsentMode::from_setting(Some("Off")), ConsentMode::Off);
        assert_eq!(ConsentMode::from_setting(Some("warn")), ConsentMode::Warn);
        // Unset / garbage → the safe default, never Off.
        assert_eq!(ConsentMode::from_setting(None), ConsentMode::Warn);
        assert_eq!(ConsentMode::from_setting(Some("banana")), ConsentMode::Warn);
    }

    #[test]
    fn off_mode_always_allows() {
        assert_eq!(
            evaluate(None, CURRENT_CONSENT_VERSION, ConsentMode::Off, Scope::Bluetooth),
            ConsentDecision::Allow
        );
    }

    #[test]
    fn enforce_denies_missing_grant() {
        let d = evaluate(None, CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth);
        assert!(d.is_denied());
        assert!(d.reason().contains("no consent"));
    }

    #[test]
    fn warn_allows_but_flags_missing_grant() {
        let d = evaluate(None, CURRENT_CONSENT_VERSION, ConsentMode::Warn, Scope::Bluetooth);
        assert!(matches!(d, ConsentDecision::Warn(_)));
        assert!(!d.is_denied());
    }

    #[test]
    fn enforce_allows_a_full_current_grant() {
        let g = full_grant();
        assert_eq!(
            evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth),
            ConsentDecision::Allow
        );
        assert_eq!(
            evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Sms),
            ConsentDecision::Allow
        );
    }

    #[test]
    fn enforce_denies_an_ungranted_scope() {
        let g = full_grant(); // recording is false
        let d = evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Recording);
        assert!(d.is_denied());
        assert!(d.reason().contains("recording"));
    }

    #[test]
    fn version_mismatch_forces_reconsent() {
        let mut g = full_grant();
        g.version = CURRENT_CONSENT_VERSION + 1; // a future or stale version
        let d = evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth);
        assert!(d.is_denied());
        assert!(d.reason().contains("re-consent"));
        // In warn mode the same mismatch is a warning, not a block.
        let d = evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Warn, Scope::Bluetooth);
        assert!(matches!(d, ConsentDecision::Warn(_)));
    }

    #[test]
    fn save_load_revoke_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "aokie-consent-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(load(&dir), None);

        let saved = save(&dir, full_grant()).unwrap();
        // accepted_at is stamped by save(), not taken from the input.
        assert!(saved.accepted_at.ends_with('Z'));
        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.scopes, saved.scopes);
        assert_eq!(loaded.version, CURRENT_CONSENT_VERSION);

        revoke(&dir).unwrap();
        assert_eq!(load(&dir), None);
        // Idempotent.
        revoke(&dir).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
