//! Manager PIN at-rest sealing + write-only handling (AOK-304A).
//!
//! The spoken manager PIN unlocks WRITE actions on the receptionist line
//! (confirm/cancel/move a booking). It used to be persisted as PLAINTEXT in
//! `settings.json` and — worse — returned verbatim by `settings.get`, which the
//! desktop console and the cloud relay both read. Anyone who could read a
//! settings snapshot learned the PIN.
//!
//! Now the PIN is:
//!   * sealed at rest (DPAPI on Windows — unreadable by any other user/machine),
//!   * write-only across `settings.get` (redacted to a `managerPinSet` boolean),
//!   * revealed to plaintext only inside THIS process, at PIN-check time, in the
//!     `AOKIE_MANAGER_PIN` env the radio already reads (process memory, the same
//!     trust boundary that already holds the live call audio).
//!
//! Non-Windows builds have no DPAPI; they keep the PIN unsealed on disk (dev
//! only — production dongle deployments are Windows) but STILL redact it from
//! every read path, which is the surface that actually leaked.

use aokie_core::dpapi;

/// Seal a plaintext manager PIN for storage. On Windows the returned token is
/// DPAPI-sealed (`dpapi:<base64>`) and is never itself the PIN. An empty PIN
/// clears the lock and stores an empty string (blank = read-only manager line).
///
/// `Err` only on a platform that CAN seal but the seal failed — the caller
/// fails the `settings.set` rather than fall back to storing plaintext.
pub fn seal(plain: &str) -> Result<String, String> {
    let plain = plain.trim();
    if plain.is_empty() {
        return Ok(String::new());
    }
    if dpapi::platform_supported() {
        dpapi::protect(plain.as_bytes())
    } else {
        // Dev fallback: no DPAPI. Store as-is; the readback redaction (the real
        // exposure) still applies.
        Ok(plain.to_string())
    }
}

/// Reveal a stored manager PIN to plaintext for comparison. A sealed token is
/// unsealed; legacy/dev plaintext is returned as-is. A seal that cannot be
/// opened (different user/machine, corrupt) reveals as EMPTY — fail closed: the
/// PIN then never matches, so manager writes are refused rather than the process
/// crashing or, worse, matching an empty PIN.
pub fn reveal(stored: &str) -> String {
    if stored.is_empty() {
        return String::new();
    }
    if dpapi::is_sealed(stored) {
        match dpapi::unprotect(stored) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => {
                eprintln!(
                    "[aokie-plugin] manager PIN could not be unsealed ({e}); \
                     manager writes disabled until it is re-set"
                );
                String::new()
            }
        }
    } else {
        stored.to_string()
    }
}

/// True when a non-empty PIN is configured — the value `settings.get` exposes in
/// place of the PIN itself. Works on the sealed token or legacy plaintext alike
/// (both are non-empty when a PIN is set).
pub fn is_set(stored: Option<&str>) -> bool {
    stored.map(|s| !s.trim().is_empty()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pin_seals_and_reveals_empty() {
        assert_eq!(seal("").unwrap(), "");
        assert_eq!(seal("   ").unwrap(), "");
        assert_eq!(reveal(""), "");
        assert!(!is_set(Some("")));
        assert!(!is_set(Some("  ")));
        assert!(!is_set(None));
    }

    #[test]
    fn seal_then_reveal_round_trips() {
        // On Windows the token is DPAPI-sealed; on dev it is stored as-is. Either
        // way reveal() must return the original PIN, and is_set() must be true.
        let sealed = seal("731905").unwrap();
        assert!(is_set(Some(&sealed)));
        assert_eq!(reveal(&sealed), "731905");
        if dpapi::platform_supported() {
            assert!(dpapi::is_sealed(&sealed), "windows stores a sealed token");
            assert_ne!(sealed, "731905", "the PIN is not stored in the clear");
        }
    }

    #[test]
    fn legacy_plaintext_reveals_as_is() {
        // A pre-migration value (never sealed) still reveals correctly, so the
        // seal-in-place migration and the runtime path agree during rollout.
        assert_eq!(reveal("731905"), "731905");
        assert!(is_set(Some("731905")));
    }
}
