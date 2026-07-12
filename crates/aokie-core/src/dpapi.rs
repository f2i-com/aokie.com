//! Shared DPAPI (Windows Data Protection API) sealing for small
//! credentials that live inside JSON files on disk.
//!
//! The aokie-plugin outbox grew its own DPAPI wrapper for event
//! payloads (AOK-DUR-001); PAIR-001 needs the same primitive for
//! Bluetooth link keys in the pairing store. This module is the
//! shared, byte-level version: callers hand in raw secret bytes and
//! store the returned `dpapi:<base64>` token; only the same Windows
//! user on the same machine can open it again.
//!
//! Non-Windows builds get `Err` from both directions — the caller
//! decides its own dev fallback (the pairing store keeps 0600
//! plaintext hex on Linux, which only carries the dev libusb
//! transport; production dongle deployments are Windows-only).

/// Storage prefix marking a DPAPI-sealed value. Kept distinct from the
/// outbox's internal prefix use so a sealed pairing-store value is
/// self-describing in isolation.
pub const DPAPI_PREFIX: &str = "dpapi:";

/// True when `stored` carries a DPAPI-sealed value (vs legacy plaintext).
pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(DPAPI_PREFIX)
}

/// True when this platform can seal (i.e. `protect` can succeed).
pub fn platform_supported() -> bool {
    cfg!(windows)
}

/// Seal `plain` under the current user's DPAPI scope. Returns the
/// `dpapi:<base64>` storage token. `Err` on non-Windows platforms or
/// on a DPAPI failure — callers must treat that as "do not store
/// plaintext", never fall back silently.
#[cfg(windows)]
pub fn protect(plain: &[u8]) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        if CryptProtectData(
            &mut input,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut out,
        ) == 0
        {
            return Err("DPAPI protect failed".to_string());
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let encoded = format!("{DPAPI_PREFIX}{}", b64_encode(slice));
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut core::ffi::c_void);
        Ok(encoded)
    }
}

#[cfg(not(windows))]
pub fn protect(_plain: &[u8]) -> Result<String, String> {
    Err("DPAPI is not available on this platform".to_string())
}

/// Open a `dpapi:<base64>` token sealed by `protect`. `Err` when the
/// token is malformed, sealed under a different user/machine, or this
/// platform has no DPAPI — callers fail closed (re-pair / re-enter),
/// they never substitute an empty secret.
#[cfg(windows)]
pub fn unprotect(stored: &str) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let b64 = stored
        .strip_prefix(DPAPI_PREFIX)
        .ok_or_else(|| "value is not DPAPI-sealed".to_string())?;
    let bytes = b64_decode(b64).ok_or_else(|| "sealed value base64 is corrupt".to_string())?;
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        if CryptUnprotectData(
            &mut input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut out,
        ) == 0
        {
            return Err("DPAPI unprotect failed (different user context?)".to_string());
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let plain = slice.to_vec();
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut core::ffi::c_void);
        Ok(plain)
    }
}

#[cfg(not(windows))]
pub fn unprotect(_stored: &str) -> Result<Vec<u8>, String> {
    Err("DPAPI is not available on this platform".to_string())
}

// Minimal std-only base64 (standard alphabet, padded) — same shape as the
// outbox's; not worth a crate for two call sites.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg_attr(not(windows), allow(dead_code))]
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg_attr(not(windows), allow(dead_code))]
fn b64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        B64.iter().position(|&b| b == c).map(|i| i as u32)
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' {
                if i < 2 {
                    return None;
                }
                0
            } else {
                val(c)?
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_sealed_detects_prefix() {
        assert!(is_sealed("dpapi:AAAA"));
        assert!(!is_sealed("00112233"));
        assert!(!is_sealed(""));
    }

    #[cfg(windows)]
    #[test]
    fn protect_unprotect_round_trips_bytes() {
        let secret = [0u8, 1, 2, 3, 250, 251, 252, 253, 254, 255];
        let sealed = protect(&secret).unwrap();
        assert!(sealed.starts_with(DPAPI_PREFIX));
        assert!(
            !sealed.contains("000102"),
            "sealed token must not embed the plaintext"
        );
        assert_eq!(unprotect(&sealed).unwrap(), secret.to_vec());
    }

    #[cfg(windows)]
    #[test]
    fn unprotect_rejects_unsealed_and_corrupt_values() {
        assert!(unprotect("00112233445566778899aabbccddeeff").is_err());
        assert!(unprotect("dpapi:!!!not-base64!!!").is_err());
        // Valid base64 but not a DPAPI blob.
        assert!(unprotect("dpapi:AAAA").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_fails_closed_both_directions() {
        assert!(protect(b"secret").is_err());
        assert!(unprotect("dpapi:AAAA").is_err());
        assert!(!platform_supported());
    }

    #[test]
    fn b64_round_trips_various_lengths() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len as u8).collect();
            let encoded = b64_encode(&data);
            assert_eq!(b64_decode(&encoded).unwrap(), data, "len {}", len);
        }
    }
}
