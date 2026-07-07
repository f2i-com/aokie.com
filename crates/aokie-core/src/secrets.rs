//! OS-native secret-store wrapper.
//!
//! Aokie's `ai_providers.json` override file used to keep remote-LLM /
//! TTS / STT `api_key` strings inline in plaintext. Anyone (or any
//! malware) with read access to the user's `%APPDATA%` could exfiltrate
//! a paid OpenAI key — exactly what the security review flagged.
//!
//! We now stage every API key through the OS keyring:
//!
//! - Windows: Credential Manager (DPAPI-backed).
//! - macOS: Keychain.
//! - Linux: Secret Service (gnome-keyring / KWallet) via the kernel
//!   keyring fallback.
//!
//! The on-disk file keeps a `secret_ref` token (a generated UUID) and
//! an empty `api_key` field; the runtime hydrates `api_key` from the
//! keyring on load. Old files with plaintext keys are migrated on first
//! save (the plaintext is moved to keyring and the field is wiped).
//!
//! Service name (`AOKIE_KEYRING_SERVICE`) is constant so a user can
//! find / remove keys via Credential Manager directly. Account names
//! embed both the *host* the key was issued for and the `secret_ref`
//! UUID. Binding to host means a tampered `ai_providers.json` cannot
//! redirect a saved key to a different `base_url`: the keyring lookup
//! fires under the new host's account name and misses, so the adapter
//! gets no key and the operator must re-enter on a host change.
//!
//! Failures from the OS keyring are surfaced as `Result<…, String>` —
//! the config layer logs and falls back to the bundled default rather
//! than crashing the app over a transient keyring problem.

const SERVICE: &str = "Aokie";

/// Stable account-name format for keyring entries. The role
/// disambiguates the same provider id used for two different roles
/// (e.g. an OpenAI key shared between an LLM and STT provider). The
/// host-token prefix is the security boundary: a config rewrite that
/// changes `base_url` to a different host produces a different
/// account name, so a saved key under the old host is never handed
/// to the new one — the operator must re-enter it.
fn account_for(host_token: Option<&str>, secret_ref: &str) -> String {
    let host = host_token.unwrap_or("no-host");
    format!("aokie:provider:{}:{}", host, secret_ref)
}

/// Generate a new opaque secret reference. The on-disk config stores
/// only this string; the actual API key is held in the OS keyring.
/// UUIDs keep the value globally unique per provider entry — even if
/// two different boxes share an `ai_providers.json`, their keyring
/// entries don't collide on a stable name.
pub fn fresh_secret_ref() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Store `api_key` under `(host_token, secret_ref)` in the OS keyring.
/// Idempotent — repeated stores under the same pair overwrite. Returns
/// the platform error verbatim on failure.
///
/// `host_token` is the canonicalised `scheme://host[:port]` portion of
/// the provider's `base_url` (see `ai::config::host_token`). Pass
/// `None` for in-process providers that have no remote endpoint;
/// they shouldn't be storing keys, so the `no-host` slot exists only
/// as a defensive fallback.
pub fn store_api_key(
    host_token: Option<&str>,
    secret_ref: &str,
    api_key: &str,
) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, &account_for(host_token, secret_ref))
        .map_err(|e| format!("keyring entry: {}", e))?;
    entry
        .set_password(api_key)
        .map_err(|e| format!("keyring set: {}", e))
}

/// Load the API key for `(host_token, secret_ref)` from the OS keyring.
/// Returns `Ok(None)` when the entry is absent — the natural state
/// after `base_url` host changed since the key was stored, in which
/// case the operator is expected to re-enter rather than silently
/// rebinding the old key. `Err` for permission / IPC failures so the
/// caller can decide whether to surface or fall back.
pub fn load_api_key(host_token: Option<&str>, secret_ref: &str) -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(SERVICE, &account_for(host_token, secret_ref))
        .map_err(|e| format!("keyring entry: {}", e))?;
    match entry.get_password() {
        Ok(s) => Ok(Some(s)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("keyring get: {}", e)),
    }
}

/// Remove `(host_token, secret_ref)` from the OS keyring. Idempotent —
/// missing entries are not an error. Called by
/// `ai::config::clear_role_secret` when the user clears a stored
/// API key from the AI Providers UI, and by the host-change path in
/// `persist_role` when a save invalidates a stale binding.
pub fn delete_api_key(host_token: Option<&str>, secret_ref: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(SERVICE, &account_for(host_token, secret_ref))
        .map_err(|e| format!("keyring entry: {}", e))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keyring delete: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_secret_ref_generates_unique_uuids() {
        let a = fresh_secret_ref();
        let b = fresh_secret_ref();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn account_for_includes_host_and_ref() {
        let r = "abcd-1234";
        assert_eq!(
            account_for(Some("https://api.openai.com"), r),
            "aokie:provider:https://api.openai.com:abcd-1234"
        );
    }

    #[test]
    fn account_for_falls_back_to_no_host_for_in_process() {
        let r = "abcd-1234";
        assert_eq!(account_for(None, r), "aokie:provider:no-host:abcd-1234");
    }

    #[test]
    fn account_for_distinguishes_hosts() {
        // Same secret_ref, different hosts must produce different
        // account names — that's the whole point of the host binding.
        let r = "abcd-1234";
        let a = account_for(Some("https://api.openai.com"), r);
        let b = account_for(Some("https://evil.example"), r);
        assert_ne!(a, b);
    }
}
