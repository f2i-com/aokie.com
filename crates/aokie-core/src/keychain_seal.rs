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
//! Linux has the same in the Secret Service (GNOME Keyring, KWallet),
//! which the desktop unlocks at log-in: the key is kept there in the
//! same way, and a session with no Secret Service running (a server with
//! no desktop) is an error the caller fails closed on, as a locked
//! Keychain is.
//!
//! The cipher is plain code with the key passed in, so it is tested on
//! every system; only the fetching of the key is macOS's and Linux's.

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

/// The Keychain (or Secret Service) item that holds the data key.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const KEYCHAIN_SERVICE: &str = "com.aokie.sealed-values";
#[cfg(any(target_os = "macos", target_os = "linux"))]
const KEYCHAIN_ACCOUNT: &str = "data-key-v1";

/// The store the key is in, for what is said when it will not give it.
#[cfg(target_os = "macos")]
const STORE: &str = "Keychain";
#[cfg(target_os = "linux")]
const STORE: &str = "Secret Service (the desktop's keyring)";

/// How long, counted from when the store was asked, a seal or an open waits
/// for its answer. The store may be showing the user a prompt, and the
/// threads that seal are the ones a call runs on (the radio's, the command
/// loop's): past this a caller is told the key is not here yet and fails
/// closed, as it does when the store refuses, while the store goes on being
/// waited on in the background. The wait is one for the whole process, not
/// one per caller, so a prompt left open stalls a call once, not each event.
const KEY_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
/// After the store did not give the key: how long before it is asked again,
/// doubled after each failure up to [`KEY_RETRY_MAX`]. Until then a seal or
/// an open is refused at once with the store's last answer, so a locked
/// Keychain or a refused prompt does not ask the user again on every event.
const KEY_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(30);
const KEY_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(600);

/// Where the asking for the data key is.
enum KeyState {
    /// Not asked yet in this process.
    Unasked,
    /// A thread of its own is asking the store, since `since`.
    Asking {
        since: std::time::Instant,
        failures: u32,
    },
    Ready([u8; KEY_LEN]),
    /// The store did not give it: what was said, and when to ask again.
    Failed {
        error: String,
        failures: u32,
        retry_at: std::time::Instant,
    },
}

/// The data key and the asking for it. The store is asked on a thread of its
/// own and only one at a time: two threads that both found no key would each
/// make one and each store it (a store replaces), the process would go on
/// with the first while the store kept the second, and after a restart
/// nothing sealed in that run would open. A key that was good once stays the
/// key for every value this process seals.
struct KeyCell {
    state: std::sync::Mutex<KeyState>,
    answered: std::sync::Condvar,
    wait: std::time::Duration,
    retry_first: std::time::Duration,
    retry_max: std::time::Duration,
}

/// What asks the store for the key (made and stored there the first time).
type FetchKey = fn() -> Result<[u8; KEY_LEN], String>;

impl KeyCell {
    const fn new(
        wait: std::time::Duration,
        retry_first: std::time::Duration,
        retry_max: std::time::Duration,
    ) -> Self {
        KeyCell {
            state: std::sync::Mutex::new(KeyState::Unasked),
            answered: std::sync::Condvar::new(),
            wait,
            retry_first,
            retry_max,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, KeyState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Start asking the store, unless the key is here, it is being asked
    /// for, or the last failure is too recent. Does not wait.
    fn ask(&'static self, fetch: FetchKey) {
        let mut state = self.lock();
        self.begin(&mut state, fetch);
    }

    fn begin(&'static self, state: &mut KeyState, fetch: FetchKey) {
        let failures = match state {
            KeyState::Unasked => 0,
            KeyState::Failed {
                failures, retry_at, ..
            } if std::time::Instant::now() >= *retry_at => *failures,
            _ => return,
        };
        *state = KeyState::Asking {
            since: std::time::Instant::now(),
            failures,
        };
        let spawned = std::thread::Builder::new()
            .name("aokie-sealing-key".into())
            .spawn(move || {
                let answer = fetch();
                self.answer(answer);
            });
        if let Err(e) = spawned {
            *state = self.failed(format!("the sealing key could not be asked for: {e}"), failures);
        }
    }

    fn answer(&self, answer: Result<[u8; KEY_LEN], String>) {
        let mut state = self.lock();
        let failures = match *state {
            KeyState::Asking { failures, .. } => failures,
            _ => 0,
        };
        *state = match answer {
            Ok(key) => KeyState::Ready(key),
            Err(error) => self.failed(error, failures),
        };
        self.answered.notify_all();
    }

    fn failed(&self, error: String, failures_before: u32) -> KeyState {
        let failures = failures_before.saturating_add(1);
        let backoff = self
            .retry_first
            .saturating_mul(1u32 << (failures - 1).min(16))
            .min(self.retry_max);
        KeyState::Failed {
            error,
            failures,
            retry_at: std::time::Instant::now() + backoff,
        }
    }

    /// The key: at once when it is here, or the store's last refusal while
    /// it is too soon to ask again; else the store is asked (once, on its
    /// own thread) and waited on until [`KeyCell::wait`] after it was asked.
    fn get(&'static self, fetch: FetchKey) -> Result<[u8; KEY_LEN], String> {
        let mut state = self.lock();
        loop {
            let deadline = match &*state {
                KeyState::Ready(key) => return Ok(*key),
                KeyState::Failed {
                    error, retry_at, ..
                } => {
                    let now = std::time::Instant::now();
                    if now < *retry_at {
                        return Err(format!(
                            "{error} (asked again in {} s)",
                            retry_at.duration_since(now).as_secs().max(1)
                        ));
                    }
                    None
                }
                KeyState::Unasked => None,
                KeyState::Asking { since, .. } => Some(*since + self.wait),
            };
            let Some(deadline) = deadline else {
                self.begin(&mut state, fetch);
                continue;
            };
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(
                    "the sealing key has not come yet: its store is still being asked for it (it may be waiting on the user)"
                        .to_string(),
                );
            }
            state = self
                .answered
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

/// The process's data key (see [`KeyCell`]).
#[cfg(any(target_os = "macos", target_os = "linux"))]
static DATA_KEY: KeyCell = KeyCell::new(KEY_WAIT, KEY_RETRY_FIRST, KEY_RETRY_MAX);

/// The data key, from the user's login Keychain (or Secret Service).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn data_key() -> Result<[u8; KEY_LEN], String> {
    DATA_KEY.get(fetch_data_key)
}

/// Start asking the store for the data key now, on its own thread, so that
/// a prompt it shows comes at start and not at the first call, and the
/// seals and opens that follow find the key here. Does not wait.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn prefetch_data_key() {
    DATA_KEY.ask(fetch_data_key);
}

/// Ask the store for the data key; made and stored there the first time.
///
/// macOS ties "always allow" to the program's code signature. A program
/// rebuilt without a stable signature is a new program to the Keychain,
/// which then asks again. A release's plugin is signed with Aokie's
/// Developer ID as `com.aokie.plugin` (ci.yml, sign-macos), which every
/// version keeps, so it is asked once; a local build keeps one signature
/// when bundle-unix.sh is given an identity (AOKIE_CODESIGN_IDENTITY).
/// (The Secret Service gives an unlocked collection's items to the user's
/// programs without asking.)
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn fetch_data_key() -> Result<[u8; KEY_LEN], String> {
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
        return Ok(fresh);
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("{STORE} item: {e}"))?;
    let read = |entry: &keyring::Entry| -> Result<Option<[u8; KEY_LEN]>, String> {
        match entry.get_secret() {
            Ok(bytes) => <[u8; KEY_LEN]>::try_from(bytes.as_slice())
                .map(Some)
                .map_err(|_| format!("the {STORE}'s sealing key has the wrong length")),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("the {STORE} did not give the sealing key: {e}")),
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
                .map_err(|e| format!("the {STORE} did not take the sealing key: {e}"))?;
            // What the Keychain holds is the key, not what was handed to
            // it: should another process have stored one in the same
            // moment, both go by the one that is there.
            read(&entry)?.ok_or_else(|| format!("the {STORE} lost the sealing key"))?
        }
    };
    Ok(key)
}

/// Seal `plain` under the key in the user's login Keychain (or Secret Service).
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn protect(plain: &[u8]) -> Result<String, String> {
    seal_with_key(&data_key()?, plain)
}

/// Open a token [`protect`] made, as the same user on the same computer.
#[cfg(any(target_os = "macos", target_os = "linux"))]
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

    fn leaked_cell(wait_ms: u64, retry_ms: u64) -> &'static KeyCell {
        let ms = std::time::Duration::from_millis;
        Box::leak(Box::new(KeyCell::new(ms(wait_ms), ms(retry_ms), ms(retry_ms * 4))))
    }

    #[test]
    fn the_store_is_asked_once_and_its_key_kept() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static ASKED: AtomicU32 = AtomicU32::new(0);
        fn fetch() -> Result<[u8; KEY_LEN], String> {
            ASKED.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(30));
            Ok(KEY)
        }
        let cell = leaked_cell(2_000, 1_000);
        cell.ask(fetch);
        let callers: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(move || cell.get(fetch)))
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), Ok(KEY));
        }
        assert_eq!(cell.get(fetch), Ok(KEY));
        assert_eq!(ASKED.load(Ordering::SeqCst), 1, "the store was asked more than once");
    }

    #[test]
    fn a_store_that_does_not_answer_holds_a_caller_only_so_long() {
        fn fetch() -> Result<[u8; KEY_LEN], String> {
            // A prompt the user takes a while over.
            std::thread::sleep(std::time::Duration::from_millis(400));
            Ok(KEY)
        }
        let cell = leaked_cell(60, 1_000);
        let started = std::time::Instant::now();
        let first = cell.get(fetch);
        assert!(first.is_err(), "the key cannot be here yet");
        assert!(started.elapsed() < std::time::Duration::from_millis(300));
        // Past the wait, a caller is not held again while the store is still asked.
        let again = std::time::Instant::now();
        assert!(cell.get(fetch).is_err());
        assert!(again.elapsed() < std::time::Duration::from_millis(30));
        // The answer still comes, and is kept.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(cell.get(fetch), Ok(KEY));
    }

    #[test]
    fn a_refusal_is_not_asked_again_until_its_backoff_is_over() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static ASKED: AtomicU32 = AtomicU32::new(0);
        fn fetch() -> Result<[u8; KEY_LEN], String> {
            ASKED.fetch_add(1, Ordering::SeqCst);
            Err("the user refused".to_string())
        }
        let cell = leaked_cell(1_000, 150);
        let refused = cell.get(fetch).unwrap_err();
        assert!(refused.contains("the user refused"), "got: {refused}");
        for _ in 0..5 {
            assert!(cell.get(fetch).unwrap_err().contains("asked again in"));
        }
        assert_eq!(ASKED.load(Ordering::SeqCst), 1, "a refusal was asked again at once");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(cell.get(fetch).is_err());
        assert_eq!(ASKED.load(Ordering::SeqCst), 2, "the store was not asked again after the backoff");
        // The second failure waits twice as long.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(cell.get(fetch).is_err());
        assert_eq!(ASKED.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn what_is_not_a_keychain_token_is_refused() {
        assert!(open_with_key(&KEY, "00112233445566778899aabbccddeeff").is_err());
        assert!(open_with_key(&KEY, "dpapi:AAAA").is_err());
        assert!(open_with_key(&KEY, "keychain1:!!!not-base64!!!").is_err());
        assert!(open_with_key(&KEY, "keychain1:").is_err());
    }
}
