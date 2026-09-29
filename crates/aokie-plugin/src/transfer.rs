//! Transfer of a live call to the owner on the OAIY route: the `transfer_v1`
//! contract (`docs/contracts/transfer/`).
//!
//! OAIY's call agent asks for a transfer with the realtime tool
//! `transfer_to_owner`; the plugin rings the owner's accepting endpoints
//! through the assistance broker and the Companion gateway, and reports how it
//! ended with a `formlogic.realtime.transfer_outcome` frame. Nothing here
//! touches audio: the caller stays with the AI until an endpoint has won the
//! request and the existing v2 takeover path moves the caller.

use sha2::{Digest, Sha256};

mod phrase;
mod wire;

#[cfg(any(test, feature = "voice"))]
pub mod call;

pub use phrase::caller_asked;
pub use wire::*;

#[cfg(test)]
pub(crate) mod fixture_tests;

/// The realtime tool the OAIY call agent calls.
pub const TOOL_NAME: &str = "transfer_to_owner";

/// The feature name OAIY lists in `ready.features` when it implements the
/// contract for a call whose start said `allowTransfer`.
pub const FEATURE: &str = "transfer_v1";

// --- The reserved transfer offer id --------------------------------------

/// Every reserved transfer offer id starts with this.
pub const RESERVED_OFFER_PREFIX: &str = "toffer_";

/// The hash domain of a reserved offer id.
const OFFER_ID_DOMAIN: &str = "oaiy/transfer-offer/v1";

/// Length of the base32 part: 26 characters carry 130 bits of the digest.
const OFFER_ID_DIGEST_CHARS: usize = 26;

/// The id of the signed transfer offer the plugin publishes to one device.
///
/// It is derived, not random, so the ring hint that the OAIY host posts (which
/// knows the request id and the phone's endpoint key) names the same offer as
/// the signed offer that reaches the phone through the authoritative snapshot,
/// and the phone upgrades the placeholder in place instead of ringing twice:
///
/// ```text
/// "toffer_" + lower-case base32(SHA-256("oaiy/transfer-offer/v1" 0x00 requestId 0x00 holderThumbprint))[0..26]
/// ```
///
/// A retired offer is never published again under the same id (the phone
/// spends an offer before it answers), so the id carries a generation:
/// generation 0 hashes as above and generation `n >= 1` appends `0x00` and the
/// decimal `n` to the hashed input.
///
/// The id is an identifier and never a credential: the offer's signed token
/// and one-use `jti` are what authorise anything. `request_id` is a random
/// 128-bit value minted by the plugin, so a third party who knows a device's
/// thumbprint still cannot compute ids for requests it has not seen.
pub fn reserved_offer_id(request_id: &str, holder_thumbprint: &str, generation: u32) -> String {
    debug_assert!(!request_id.contains('\0') && !holder_thumbprint.contains('\0'));
    let mut hasher = Sha256::new();
    hasher.update(OFFER_ID_DOMAIN.as_bytes());
    hasher.update([0]);
    hasher.update(request_id.as_bytes());
    hasher.update([0]);
    hasher.update(holder_thumbprint.as_bytes());
    if generation > 0 {
        hasher.update([0]);
        hasher.update(generation.to_string().as_bytes());
    }
    let mut id = String::from(RESERVED_OFFER_PREFIX);
    id.push_str(&base32_lower(&hasher.finalize())[..OFFER_ID_DIGEST_CHARS]);
    id
}

/// Whether `id` has the shape of a reserved transfer offer id. Shape only: it
/// says nothing about whether the plugin issued it.
pub fn is_reserved_offer_id(id: &str) -> bool {
    id.strip_prefix(RESERVED_OFFER_PREFIX).is_some_and(|rest| {
        rest.len() == OFFER_ID_DIGEST_CHARS
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
    })
}

/// RFC 4648 base32 in lower case, without padding.
fn base32_lower(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Appendix A.2 of the mobile design (vector V2), recomputed with node:
    /// the two test phones (Ed25519 seeds of 32 bytes of 0x55 and 0x66).
    const REQUEST_ID: &str = "assist_0123456789abcdef0123456789abcdef";
    const PHONE_ONE: &str = "BLZLEktWlEkM8UgeGgeGY7D5ounEiW2ZqRuQkUbEHa8";
    const PHONE_TWO: &str = "Gec-ct57Gwltw072qu-aed20Ju93h-m8Qv8yjNgvMOM";

    #[test]
    fn base32_matches_rfc_4648_vectors_lower_cased() {
        for (input, expected) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32_lower(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn reserved_ids_match_the_design_vectors() {
        assert_eq!(
            reserved_offer_id(REQUEST_ID, PHONE_ONE, 0),
            "toffer_ertekigjycmj2scu4fikachllm"
        );
        assert_eq!(
            reserved_offer_id(REQUEST_ID, PHONE_TWO, 0),
            "toffer_cqalgrixultpirc3wxjy2jsrpb"
        );
        // A retired offer takes the next generation.
        assert_eq!(
            reserved_offer_id(REQUEST_ID, PHONE_ONE, 1),
            "toffer_ispwfvmwl7yv7vfn6s673qd64j"
        );
        assert_eq!(
            reserved_offer_id(REQUEST_ID, PHONE_ONE, 2),
            "toffer_j4td3x77lmwdaawb5xmi52t6xt"
        );
        assert_eq!(
            reserved_offer_id(REQUEST_ID, PHONE_TWO, 1),
            "toffer_vn6vxabupbjwhjb7gf372hj73t"
        );
    }

    #[test]
    fn reserved_ids_are_33_safe_characters_and_distinct_per_request_device_and_generation() {
        let mut seen = std::collections::HashSet::new();
        for request in [REQUEST_ID, "assist_ffffffffffffffffffffffffffffffff"] {
            for phone in [PHONE_ONE, PHONE_TWO] {
                for generation in 0..4 {
                    let id = reserved_offer_id(request, phone, generation);
                    assert_eq!(id.len(), 33, "{id}");
                    assert!(is_reserved_offer_id(&id), "{id}");
                    assert!(
                        id.bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
                        "{id}"
                    );
                    assert!(seen.insert(id), "an id repeated across request, device or generation");
                }
            }
        }
    }

    #[test]
    fn the_hash_input_is_unambiguous() {
        // 0x00 separates the fields, so moving a byte across the boundary
        // between request id and thumbprint cannot give the same id.
        assert_ne!(
            reserved_offer_id("assist_ab", "cd", 0),
            reserved_offer_id("assist_a", "bcd", 0)
        );
        // The generation is appended after a separator: generation 1 of a
        // thumbprint is not generation 0 of a thumbprint that ends in "1".
        assert_ne!(
            reserved_offer_id("assist_a", "thumb", 1),
            reserved_offer_id("assist_a", "thumb1", 0)
        );
    }

    #[test]
    fn a_random_offer_id_is_never_taken_for_a_reserved_one() {
        for ordinary in [
            "offer_0123456789abcdef0123456789abcdef",
            "toffer_",
            "toffer_short",
            "toffer_ERTEKIGJYCMJ2SCU4FIKACHLLM",
            "toffer_ertekigjycmj2scu4fikachllm0",
            "toffer_ertekigjycmj2scu4fikachll1",
            "toffer_ertekigjycmj2scu4fikachll-",
            "",
        ] {
            assert!(!is_reserved_offer_id(ordinary), "{ordinary}");
        }
    }
}
