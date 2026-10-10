//! Sealing small secrets at rest on macOS: the Mac's half of
//! [`crate::dpapi`].
//!
//! Windows has DPAPI: hand it bytes and only the same user on the same
//! machine can open them again, with no key for the program to look
//! after. macOS has no call of that shape. What it has is the login
//! Keychain, which keeps a secret for a user and gives it to the programs
//! that user allows. So one random 256-bit key is made on first use and
//! kept in the Keychain, and every value is sealed under it with
//! AES-256-GCM: a fresh random nonce each time, and a tag that fails the
//! open if a single bit of the stored token was changed.
//!
//! The rule the DPAPI side keeps holds here too: no plaintext fallback. A
//! Keychain that will not give the key (locked, or the user refused the
//! prompt) is an error the caller fails closed on, never a reason to
//! store or return something unsealed.
//!
//! The cipher is plain code with the key passed in, so it is tested on
//! every system; only the fetching of the key is macOS's.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

use crate::dpapi::{b64_decode, b64_encode, KEYCHAIN_PREFIX};

/// The data key's length: AES-256.
pub const KEY_LEN: usize = 32;

/// Bound into every seal, so that a token made for something else under
/// the same key (should the key ever be put to another use) does not open
/// here.
const CONTEXT: &[u8] = b"aokie sealed value v1";

/// Seal `plain` under `key`: `keychain1:<base64(nonce || ciphertext || tag)>`.
pub fn seal_with_key(key: &[u8; KEY_LEN], plain: &[u8]) -> Result<String, String> {
    let cipher = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, key).map_err(|_| "the sealing key is unusable".to_string())?,
    );
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| "the system gave no random bytes for a nonce".to_string())?;
    let mut sealed = Vec::with_capacity(NONCE_LEN + plain.len() + AES_256_GCM.tag_len());
    sealed.extend_from_slice(&nonce);
    let mut body = plain.to_vec();
    cipher
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(CONTEXT),
            &mut body,
        )
        .map_err(|_| "sealing failed".to_string())?;
    sealed.extend_from_slice(&body);
    Ok(format!("{KEYCHAIN_PREFIX}{}", b64_encode(&sealed)))
}

/// Open a token [`seal_with_key`] made. `Err` when it is not such a
/// token, was sealed under another key, or has been changed since.
pub fn open_with_key(key: &[u8; KEY_LEN], stored: &str) -> Result<Vec<u8>, String> {
    let b64 = stored
        .strip_prefix(KEYCHAIN_PREFIX)
        .ok_or_else(|| "value is not Keychain-sealed".to_string())?;
    let sealed = b64_decode(b64).ok_or_else(|| "sealed value base64 is corrupt".to_string())?;
    if sealed.len() < NONCE_LEN + AES_256_GCM.tag_len() {
        return Err("sealed value is too short".to_string());
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce's length");
    let cipher = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, key).map_err(|_| "the sealing key is unusable".to_string())?,
    );
    let mut body = body.to_vec();
    let plain = cipher
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(CONTEXT),
            &mut body,
        )
        .map_err(|_| {
            "sealed value does not open (another user's Keychain, or it was changed)".to_string()
        })?;
    Ok(plain.to_vec())
}

/// The Keychain item that holds the data key.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.aokie.sealed-values";
#[cfg(target_os = "macos")]
const KEYCHAIN_ACCOUNT: &str = "data-key-v1";

/// The data key, from the user's login Keychain; made and stored there the
/// first time. Asked of the Keychain once per process: the user is not
/// prompted over and over, and a key that was good at start stays the key
/// for every value this process seals.
///
/// One lock is held from the look to the store. Two threads that both
/// found no key would each make one and each store it (a store replaces):
/// the process would go on with the first while the Keychain kept the
/// second, and after a restart nothing sealed in that run would open.
///
/// macOS ties "always allow" to the program's code signature. A program
/// rebuilt without a stable signature is a new program to the Keychain,
/// which then asks again.
#[cfg(target_os = "macos")]
fn data_key() -> Result<[u8; KEY_LEN], String> {
    use std::sync::Mutex;
    static KEY: Mutex<Option<[u8; KEY_LEN]>> = Mutex::new(None);
    let mut held = KEY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(key) = *held {
        return Ok(key);
    }
    // A test run must neither put a key into the developer's Keychain nor
    // wait on its prompt (every test program is a new program to it). A
    // debug build started with AOKIE_SEAL_EPHEMERAL set seals under a key
    // that lives in this process only: still sealed, never plaintext, and
    // gone when the process ends. Release builds do not have this door.
    #[cfg(debug_assertions)]
    if std::env::var_os("AOKIE_SEAL_EPHEMERAL").is_some() {
        let mut fresh = [0u8; KEY_LEN];
        SystemRandom::new()
            .fill(&mut fresh)
            .map_err(|_| "the system gave no random bytes for a key".to_string())?;
        *held = Some(fresh);
        return Ok(fresh);
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("Keychain item: {e}"))?;
    let read = |entry: &keyring::Entry| -> Result<Option<[u8; KEY_LEN]>, String> {
        match entry.get_secret() {
            Ok(bytes) => <[u8; KEY_LEN]>::try_from(bytes.as_slice())
                .map(Some)
                .map_err(|_| "the Keychain's sealing key has the wrong length".to_string()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("the Keychain did not give the sealing key: {e}")),
        }
    };
    let key = match read(&entry)? {
        Some(key) => key,
        None => {
            let mut fresh = [0u8; KEY_LEN];
            SystemRandom::new()
                .fill(&mut fresh)
                .map_err(|_| "the system gave no random bytes for a key".to_string())?;
            entry
                .set_secret(&fresh)
                .map_err(|e| format!("the Keychain did not take the sealing key: {e}"))?;
            // What the Keychain holds is the key, not what was handed to
            // it: should another process have stored one in the same
            // moment, both go by the one that is there.
            read(&entry)?.ok_or_else(|| "the Keychain lost the sealing key".to_string())?
        }
    };
    *held = Some(key);
    Ok(key)
}

/// Seal `plain` under the key in the user's login Keychain.
#[cfg(target_os = "macos")]
pub fn protect(plain: &[u8]) -> Result<String, String> {
    seal_with_key(&data_key()?, plain)
}

/// Open a token [`protect`] made, as the same user on the same Mac.
#[cfg(target_os = "macos")]
pub fn unprotect(stored: &str) -> Result<Vec<u8>, String> {
    open_with_key(&data_key()?, stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; KEY_LEN] = [7u8; KEY_LEN];

    #[test]
    fn what_is_sealed_opens_to_the_same_bytes() {
        for plain in [&b""[..], b"x", b"a link key: 0123456789abcdef", &[0u8, 255, 1, 254][..]] {
            let sealed = seal_with_key(&KEY, plain).unwrap();
            assert!(sealed.starts_with(KEYCHAIN_PREFIX));
            assert!(crate::dpapi::is_sealed(&sealed));
            assert_eq!(open_with_key(&KEY, &sealed).unwrap(), plain);
        }
    }

    #[test]
    fn the_token_does_not_carry_the_plaintext() {
        let plain = b"0123456789abcdef0123456789abcdef";
        let sealed = seal_with_key(&KEY, plain).unwrap();
        let raw = b64_decode(sealed.strip_prefix(KEYCHAIN_PREFIX).unwrap()).unwrap();
        assert!(
            !raw.windows(8).any(|w| plain.windows(8).any(|p| p == w)),
            "eight plaintext bytes in a row are in the sealed token"
        );
        // Nonce, the bytes, the tag: nothing else is stored.
        assert_eq!(raw.len(), NONCE_LEN + plain.len() + 16);
    }

    #[test]
    fn the_same_bytes_seal_differently_each_time() {
        let a = seal_with_key(&KEY, b"the same").unwrap();
        let b = seal_with_key(&KEY, b"the same").unwrap();
        assert_ne!(a, b, "a nonce was used twice");
    }

    #[test]
    fn another_key_does_not_open_it() {
        let sealed = seal_with_key(&KEY, b"secret").unwrap();
        let other = [8u8; KEY_LEN];
        assert!(open_with_key(&other, &sealed).is_err());
    }

    #[test]
    fn a_changed_token_does_not_open() {
        let sealed = seal_with_key(&KEY, b"a payload of some length").unwrap();
        let mut raw = b64_decode(sealed.strip_prefix(KEYCHAIN_PREFIX).unwrap()).unwrap();
        // Every byte matters: the nonce, the body and the tag.
        for at in [0, NONCE_LEN, raw.len() - 1] {
            raw[at] ^= 0x01;
            let changed = format!("{KEYCHAIN_PREFIX}{}", b64_encode(&raw));
            assert!(open_with_key(&KEY, &changed).is_err(), "byte {at} was changed");
            raw[at] ^= 0x01;
        }
        // Cut short, it is refused, not read past.
        let cut = format!("{KEYCHAIN_PREFIX}{}", b64_encode(&raw[..NONCE_LEN + 3]));
        assert!(open_with_key(&KEY, &cut).is_err());
    }

    #[test]
    fn what_is_not_a_keychain_token_is_refused() {
        assert!(open_with_key(&KEY, "00112233445566778899aabbccddeeff").is_err());
        assert!(open_with_key(&KEY, "dpapi:AAAA").is_err());
        assert!(open_with_key(&KEY, "keychain1:!!!not-base64!!!").is_err());
        assert!(open_with_key(&KEY, "keychain1:").is_err());
    }
}
