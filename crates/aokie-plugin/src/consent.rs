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
/// camelCase on the wire + at rest, like the rest of the plugin surface
/// (the Desktop wizard issues camelCase grants).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

/// The durable consent record. camelCase like the rest of the wire surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentGrant {
    /// Consent surface version the operator accepted; compared to
    /// [`CURRENT_CONSENT_VERSION`] at gate time.
    pub version: u32,
    pub scopes: ConsentScopes,
    /// ISO-8601 UTC timestamp the grant was recorded. Legacy unsigned path:
    /// set by the plugin, not the caller, so it can't be back-dated over the
    /// wire. Signed path: part of the Desktop-signed payload (the Desktop is
    /// the trusted issuer).
    pub accepted_at: String,
    /// The FormLogic identity that accepted (user id / email), if the
    /// control plane supplied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_by: Option<String>,
    /// ISO-8601 UTC expiry (CONSENT-001): a grant past this no longer
    /// satisfies the gate — the operator must re-consent. Absent = no
    /// expiry (legacy grants).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Legacy field from the pre-signature era (recorded, never verified).
    /// Signed grants use the [`SignedConsent`] envelope instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// CONSENT-001: the Desktop-signed consent envelope. `payload_b64` carries
/// the exact [`ConsentGrant`] JSON bytes that were signed (raw-byte signing —
/// no canonicalisation surface), signature is Ed25519 by the DESKTOP-held
/// per-install key whose public half arrives in this process's environment
/// (`FORMLOGIC_CONSENT_VERIFY_KEY`, set by the parent Desktop at spawn).
/// Because the key is per-install, a grant signed on another machine can
/// never satisfy this device's gate — the device binding IS the key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedConsent {
    pub format: u32,
    pub alg: String,
    pub key_id: String,
    pub payload_b64: String,
    pub signature: String,
}

/// Verify a signed envelope against the Desktop's verify key and return the
/// signed grant. Errors name the exact failure (surfaced to the operator).
pub fn verify_envelope(envelope: &SignedConsent, verify_key_b64: &str) -> Result<ConsentGrant, String> {
    use base64::Engine as _;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    if envelope.alg != "Ed25519" {
        return Err(format!("unsupported consent signature alg {:?}", envelope.alg));
    }
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(envelope.payload_b64.trim())
        .map_err(|e| format!("consent payload base64: {e}"))?;
    let sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(envelope.signature.trim().trim_end_matches('='))
        .map_err(|e| format!("consent signature base64: {e}"))?;
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(verify_key_b64.trim())
        .map_err(|e| format!("consent verify key base64: {e}"))?;
    let key = VerifyingKey::from_bytes(
        &<[u8; 32]>::try_from(key_bytes.as_slice()).map_err(|_| "consent verify key must be 32 bytes")?,
    )
    .map_err(|e| format!("consent verify key invalid: {e}"))?;
    let sig = Signature::from_bytes(
        &<[u8; 64]>::try_from(sig_bytes.as_slice()).map_err(|_| "consent signature must be 64 bytes")?,
    );
    key.verify(&payload_bytes, &sig)
        .map_err(|_| "consent grant signature verification failed".to_string())?;
    serde_json::from_slice(&payload_bytes).map_err(|e| format!("consent payload malformed: {e}"))
}

/// Enforcement posture, from the `consentMode` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentMode {
    Off,
    Warn,
    Enforce,
}

impl ConsentMode {
    /// Parse the `consentMode` setting. CONSENT-001: production default is
    /// **Enforce** — anything unrecognised (incl. unset) enforces. `warn` is
    /// the EXPLICIT developer/beta override (keeps a pre-wizard deployment
    /// running while its operator completes consent), `off` the explicit
    /// legacy escape hatch. Neither is ever the silent default.
    pub fn from_setting(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("warn") => ConsentMode::Warn,
            Some("off") => ConsentMode::Off,
            _ => ConsentMode::Enforce,
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
/// may proceed. A missing grant, a version mismatch (re-consent needed), an
/// EXPIRED grant, or an ungranted scope is a refusal under `Enforce`, a
/// warning under `Warn`, and ignored under `Off`. `now_iso` is the current
/// ISO-8601 UTC instant (lexicographic comparison is temporal for this
/// format), injected so the gate stays pure/testable.
pub fn evaluate(
    grant: Option<&ConsentGrant>,
    required_version: u32,
    mode: ConsentMode,
    scope: Scope,
    now_iso: &str,
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
        Some(g) if g.expires_at.as_deref().is_some_and(|exp| exp <= now_iso) => Some(format!(
            "consent expired at {} — re-consent needed",
            g.expires_at.as_deref().unwrap_or("")
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
/// Legacy reader: does NOT verify signatures — use [`load_verified`] at
/// gate time so a signing Desktop's grants are always re-verified.
pub fn load(data_dir: &Path) -> Option<ConsentGrant> {
    load_verified(data_dir, None).grant
}

/// The outcome of reading + verifying the stored consent.
#[derive(Debug, Clone)]
pub struct LoadedConsent {
    /// The usable grant, or `None` when nothing valid is recorded.
    pub grant: Option<ConsentGrant>,
    /// True when the grant carried a valid Desktop signature.
    pub signed: bool,
    /// Why a stored record was refused (tamper, unsigned under a signing
    /// desktop, parse failure) — surfaced to the operator.
    pub note: Option<String>,
}

/// Read + VERIFY the consent record (CONSENT-001). When `verify_key_b64` is
/// present (a signing Desktop spawned us), ONLY a validly-signed envelope
/// satisfies — an unsigned/legacy record or a bad signature yields
/// `grant: None` with a re-consent note (fail closed). Without a verify key
/// (legacy host), plain records are accepted as before.
pub fn load_verified(data_dir: &Path, verify_key_b64: Option<&str>) -> LoadedConsent {
    let path = config_path(data_dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => {
            return LoadedConsent { grant: None, signed: false, note: None };
        }
    };
    // Signed envelope first (has payloadB64 + signature keys).
    if let Ok(envelope) = serde_json::from_str::<SignedConsent>(&raw) {
        if !envelope.payload_b64.is_empty() && !envelope.signature.is_empty() {
            let Some(key) = verify_key_b64 else {
                return LoadedConsent {
                    grant: None,
                    signed: false,
                    note: Some(
                        "a signed consent grant is recorded but no verify key was provided by the host"
                            .into(),
                    ),
                };
            };
            return match verify_envelope(&envelope, key) {
                Ok(grant) => LoadedConsent { grant: Some(grant), signed: true, note: None },
                Err(e) => LoadedConsent {
                    grant: None,
                    signed: false,
                    note: Some(format!("stored consent grant failed verification: {e}")),
                },
            };
        }
    }
    // Legacy plain grant.
    match serde_json::from_str::<ConsentGrant>(&raw) {
        Ok(grant) => {
            if verify_key_b64.is_some() {
                // A signing Desktop requires signed grants — a legacy record
                // no longer satisfies the gate (re-consent through the wizard).
                LoadedConsent {
                    grant: None,
                    signed: false,
                    note: Some(
                        "an unsigned (legacy) consent record is present but this Desktop signs grants — re-consent required"
                            .into(),
                    ),
                }
            } else {
                LoadedConsent { grant: Some(grant), signed: false, note: None }
            }
        }
        Err(e) => {
            eprintln!("[aokie-plugin] consent.json parse failed ({e}) — treating as not-accepted");
            LoadedConsent {
                grant: None,
                signed: false,
                note: Some(format!("consent record unreadable: {e}")),
            }
        }
    }
}

/// Persist a Desktop-signed envelope VERBATIM (the raw signed bytes are what
/// the gate re-verifies on every load — file tampering breaks the signature).
pub fn save_signed(data_dir: &Path, envelope: &SignedConsent) -> Result<(), String> {
    let json =
        serde_json::to_string_pretty(envelope).map_err(|e| format!("serialize consent: {e}"))?;
    aokie_core::paths::atomic_write(&config_path(data_dir), json.as_bytes())
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

    const NOW: &str = "2026-07-12T00:00:00Z";

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
            expires_at: None,
            signature: None,
        }
    }

    #[test]
    fn mode_defaults_to_enforce_and_warn_is_explicit() {
        assert_eq!(ConsentMode::from_setting(Some("enforce")), ConsentMode::Enforce);
        assert_eq!(ConsentMode::from_setting(Some("Off")), ConsentMode::Off);
        assert_eq!(ConsentMode::from_setting(Some("warn")), ConsentMode::Warn);
        // CONSENT-001: production default is ENFORCE — unset/garbage never
        // silently degrades to warn or off.
        assert_eq!(ConsentMode::from_setting(None), ConsentMode::Enforce);
        assert_eq!(ConsentMode::from_setting(Some("banana")), ConsentMode::Enforce);
    }

    #[test]
    fn off_mode_always_allows() {
        assert_eq!(
            evaluate(None, CURRENT_CONSENT_VERSION, ConsentMode::Off, Scope::Bluetooth, NOW),
            ConsentDecision::Allow
        );
    }

    #[test]
    fn enforce_denies_missing_grant() {
        let d = evaluate(None, CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth, NOW);
        assert!(d.is_denied());
        assert!(d.reason().contains("no consent"));
    }

    #[test]
    fn warn_allows_but_flags_missing_grant() {
        let d = evaluate(None, CURRENT_CONSENT_VERSION, ConsentMode::Warn, Scope::Bluetooth, NOW);
        assert!(matches!(d, ConsentDecision::Warn(_)));
        assert!(!d.is_denied());
    }

    #[test]
    fn enforce_allows_a_full_current_grant() {
        let g = full_grant();
        assert_eq!(
            evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth, NOW),
            ConsentDecision::Allow
        );
        assert_eq!(
            evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Sms, NOW),
            ConsentDecision::Allow
        );
    }

    #[test]
    fn enforce_denies_an_ungranted_scope() {
        let g = full_grant(); // recording is false
        let d = evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Recording, NOW);
        assert!(d.is_denied());
        assert!(d.reason().contains("recording"));
    }

    #[test]
    fn version_mismatch_forces_reconsent() {
        let mut g = full_grant();
        g.version = CURRENT_CONSENT_VERSION + 1; // a future or stale version
        let d = evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth, NOW);
        assert!(d.is_denied());
        assert!(d.reason().contains("re-consent"));
        // In warn mode the same mismatch is a warning, not a block.
        let d = evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Warn, Scope::Bluetooth, NOW);
        assert!(matches!(d, ConsentDecision::Warn(_)));
    }

    #[test]
    fn expired_grant_forces_reconsent() {
        let mut g = full_grant();
        g.expires_at = Some("2026-07-01T00:00:00Z".into()); // already past NOW
        let d = evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth, NOW);
        assert!(d.is_denied());
        assert!(d.reason().contains("expired"));
        // A still-valid expiry allows.
        g.expires_at = Some("2027-07-01T00:00:00Z".into());
        assert_eq!(
            evaluate(Some(&g), CURRENT_CONSENT_VERSION, ConsentMode::Enforce, Scope::Bluetooth, NOW),
            ConsentDecision::Allow
        );
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

    // ---- CONSENT-001: Desktop-signed grants ----

    fn test_keypair() -> (ed25519_dalek::SigningKey, String) {
        use base64::Engine as _;
        let key = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
        let pub_b64 =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
        (key, pub_b64)
    }

    fn signed_envelope(grant: &ConsentGrant, key: &ed25519_dalek::SigningKey) -> SignedConsent {
        use base64::Engine as _;
        use ed25519_dalek::Signer as _;
        let payload = serde_json::to_vec(grant).unwrap();
        let sig = key.sign(&payload);
        SignedConsent {
            format: 1,
            alg: "Ed25519".into(),
            key_id: "desktop-test".into(),
            payload_b64: base64::engine::general_purpose::STANDARD.encode(&payload),
            signature: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes()),
        }
    }

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "aokie-consent-signed-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn signed_grant_round_trip_verifies() {
        let (key, pub_b64) = test_keypair();
        let dir = tmp_dir();
        let envelope = signed_envelope(&full_grant(), &key);
        save_signed(&dir, &envelope).unwrap();
        let loaded = load_verified(&dir, Some(&pub_b64));
        assert!(loaded.signed);
        assert_eq!(loaded.grant.unwrap().scopes, full_grant().scopes);
        assert!(loaded.note.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_signed_grant_fails_closed() {
        let (key, pub_b64) = test_keypair();
        let dir = tmp_dir();
        let mut envelope = signed_envelope(&full_grant(), &key);
        // Forge a payload claiming MORE scopes than were signed.
        let mut forged = full_grant();
        forged.scopes.recording = true;
        use base64::Engine as _;
        envelope.payload_b64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&forged).unwrap());
        save_signed(&dir, &envelope).unwrap();
        let loaded = load_verified(&dir, Some(&pub_b64));
        assert!(loaded.grant.is_none(), "forged payload must not load");
        assert!(loaded.note.unwrap().contains("verification"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsigned_record_refused_under_a_signing_desktop() {
        let (_, pub_b64) = test_keypair();
        let dir = tmp_dir();
        save(&dir, full_grant()).unwrap(); // legacy plain record
        let loaded = load_verified(&dir, Some(&pub_b64));
        assert!(loaded.grant.is_none(), "legacy record must not satisfy a signing desktop");
        assert!(loaded.note.unwrap().contains("re-consent"));
        // …but a legacy host (no key) still accepts it.
        let legacy = load_verified(&dir, None);
        assert!(legacy.grant.is_some());
        assert!(!legacy.signed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_key_grant_is_refused() {
        let (key, _) = test_keypair();
        let dir = tmp_dir();
        save_signed(&dir, &signed_envelope(&full_grant(), &key)).unwrap();
        use base64::Engine as _;
        let other = base64::engine::general_purpose::STANDARD
            .encode(ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes());
        let loaded = load_verified(&dir, Some(&other));
        assert!(loaded.grant.is_none(), "a grant signed by ANOTHER install must not verify");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
