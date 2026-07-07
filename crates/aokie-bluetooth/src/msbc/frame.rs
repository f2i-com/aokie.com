//! mSBC frame header layout and LOUDNESS bit allocation.
//!
//! Frame structure (57 bytes total):
//!
//! ```text
//! Byte 0  : sync (0xAD)
//! Byte 1  : 0x00 on the wire (Bluedroid/iPhone/BTstack convention —
//!           in spec terms this would have been the freq/blocks/mode/
//!           alloc/subbands bitfield, but mSBC's params are fixed and
//!           every real-world encoder zeros it before CRC).
//! Byte 2  : 0x00 on the wire (would have been the bitpool 26=0x1A,
//!           but is also zeroed — same convention as byte 1).
//! Byte 3  : CRC-8 over byte 1, byte 2, and the scale-factor nibbles
//!           that follow (32 bits = 4 bytes for mSBC). Since bytes 1
//!           and 2 are zero, the CRC effectively only covers the SF
//!           nibbles.
//! Byte 4..7 : 8 scale factors × 4 bits each, packed MSB-first.
//! Byte 8..56 : audio sample bits, packed MSB-first.
//! ```
//!
//! Receivers identify mSBC vs A2DP-SBC by the sync byte (`0xAD` vs
//! `0x9C`). The all-zero bytes 1 and 2 are what makes Pixel/Android's
//! mSBC pre-decoder validators accept the frame; sending the spec
//! values (or any non-zero value) was producing CRC-good frames that
//! Pixel still discarded as malformed and rendered as silence.

use super::tables::LOUDNESS_OFFSETS_8;
use super::{MSBC_BITPOOL, MSBC_NUM_SUBBANDS, MSBC_SYNC_BYTE};

/// The fixed second byte of an mSBC frame header on the wire.
///
/// **Always zero on the wire**, matching what every shipping mSBC
/// encoder actually emits — Bluedroid (Android/Pixel), iPhones, and
/// BTstack all overwrite this byte (and the bitpool byte at offset 2)
/// to zero immediately before computing the CRC and putting the frame
/// on the air. See Bluedroid's `sbc_packing.c` lines 244-248 (the
/// `if (reserved_ptr) { *reserved_ptr++ = 0; *reserved_ptr++ = 0; }`
/// block) for the canonical reference.
///
/// Why zero rather than the spec values? mSBC's parameters are fixed
/// (16 kHz mono, 8 subbands, 15 blocks, LOUDNESS, bitpool 26) so the
/// header bits would just re-encode information the decoder already
/// knows from "this is an mSBC stream". Bluedroid's mSBC decoder path
/// (`OI_SBC_ReadHeader_mSBC`) literally hardcodes those params and
/// stashes the wire bytes 1 and 2 into `reserved_for_future_use[]`
/// without parsing them — but the CRC is computed over those bytes,
/// so encoder and decoder have to agree on what's there. Bluedroid
/// agreed on zero.
///
/// Sending non-zero values (0x01, 0x03, etc.) was the cause of "wire
/// bytes look right, CRC computes correctly, but Pixel hears silence":
/// some pre-decoder validator on the receive path rejects frames
/// where bytes 1 and 2 don't match the all-zero wire convention.
pub const MSBC_HEADER_BYTE_1: u8 = 0x00;

/// The fixed third byte of an mSBC frame header on the wire — also
/// always zero, for the same reason as `MSBC_HEADER_BYTE_1`. The
/// actual bitpool used internally for bit allocation is `MSBC_BITPOOL`
/// (= 26); the wire slot is just a place to put it that all mSBC
/// implementations agreed to leave at zero.
pub const MSBC_WIRE_BYTE_2: u8 = 0x00;

/// Compute LOUDNESS bit allocation for the mSBC mono / 8-subband frame.
///
/// Faithful port of Bluedroid's `monoBitAllocation` /
/// `oneChannelBitAllocation` (decoder/srce/bitalloc.c). Cross-vendor
/// compatibility is non-negotiable: the encoder packs q-values using
/// these bit widths and the peer decoder unpacks at the same offsets,
/// so a divergence here scrambles the bitstream and produces audible
/// static even when SF / sync / CRC are all correct.
///
/// Steps:
/// 1. Per-subband `bitneed`. For LOUDNESS the formula is
///    `if sf > 0: bn = (sf - off)/2 + 5 if (sf - off) > 0 else (sf - off) + 5`
///    — note the integer division and the `+5` floor — and `bn = 0`
///    when the scale factor is zero. Plain `sf - off` (what we used
///    before, and what some references show) gives a different shape
///    and a different per-subband distribution at the same total bit
///    count, which is enough to break wire interop.
/// 2. Binary-search a global `bitadjust ∈ [-64, 63]` so that
///    `sum(clamp(bn[sb] + bitadjust))` lands at or just under
///    `bitpool`, with the LOUDNESS clamp `{0, 2..=16}` applied.
/// 3. First pass: assign `bn[sb] + bitadjust` to each subband, using
///    one excess bit per subband if available (and promoting illegal
///    1-bit allocations to 2 or 0 depending on excess).
/// 4. Second pass: round-robin remaining excess bits across subbands
///    that already have allocations < 16.
///
/// Returns an array of 8 entries with `bits[sb] ∈ {0, 2..=16}` summing
/// to at most `bitpool`.
pub fn loudness_bit_allocation(scale_factors: &[u8; 8]) -> [u8; 8] {
    let offsets = LOUDNESS_OFFSETS_8[0]; // mSBC = 16 kHz row
    let bitpool = MSBC_BITPOOL as i32;

    // Step 1: bitneed
    let mut bitneed = [0i32; 8];
    let mut bitcount: i32 = 0;
    for sb in 0..MSBC_NUM_SUBBANDS {
        let sf = scale_factors[sb] as i32;
        let mut bn = sf;
        if bn != 0 {
            bn -= offsets[sb];
            if bn > 0 {
                bn /= 2;
            }
            bn += 5;
        }
        bitneed[sb] = bn;
        // Bluedroid only adds bn to the initial bitcount when bn > 1
        // (the LOUDNESS "no single bit" rule means a 1-need subband
        // gets either 0 or 2 bits, not 1). This matters for the
        // adjustment binary search seed.
        if bn > 1 {
            bitcount += bn;
        }
    }

    // Step 2: binary-search the global bitadjust.
    let bitadjust = adjust_to_fit_bitpool(bitpool, &bitneed, bitcount);
    let mut excess = bitpool;
    for sb in 0..MSBC_NUM_SUBBANDS {
        excess -= clamp_bn(bitneed[sb] + bitadjust);
    }

    // Step 3: first-pass allocation.
    let mut bits = [0u8; MSBC_NUM_SUBBANDS];
    for sb in 0..MSBC_NUM_SUBBANDS {
        let raw = bitneed[sb] + bitadjust;
        excess = alloc_adjusted_bits(&mut bits[sb], raw, excess);
    }

    // Step 4: distribute remaining excess one bit per subband, in
    // subband order, capped at 16. Matches Bluedroid's
    // `allocExcessBits` second pass. With `bitpool ≤ 16 * subbands`
    // (always true for mSBC: 26 ≤ 128) and the first-pass invariant
    // that each subband has at most one excess bit consumed, the
    // residual excess after the first pass is always `< subbands`,
    // so a single sweep is enough.
    let mut sb = 0usize;
    while excess > 0 && sb < MSBC_NUM_SUBBANDS {
        if bits[sb] < 16 {
            bits[sb] += 1;
            excess -= 1;
        }
        sb += 1;
    }

    bits
}

/// Bluedroid's `clamp` rule used inside both the binary-search predicate
/// and the final allocator: bits in `{0, 2..=16}`, with 1 mapped to 0.
fn clamp_bn(n: i32) -> i32 {
    if n < 2 {
        0
    } else if n > 16 {
        16
    } else {
        n
    }
}

/// Faithful port of Bluedroid's `adjustToFitBitpool`. Binary-searches
/// `bitadjust` over an 8-step ladder so that
/// `sum(clamp_bn(bitneed[sb] + bitadjust)) ≤ bitpool`, returning the
/// largest `bitadjust` that still fits (i.e. the closest to bitpool
/// without exceeding it). The starting direction depends on whether
/// the unadjusted bitneed sum is already over or under bitpool.
fn adjust_to_fit_bitpool(bitpool: i32, bitneed: &[i32; 8], bitcount_seed: i32) -> i32 {
    let mut max_bitadjust = 0i32;
    let mut bitadjust: i32 = if bitcount_seed > bitpool { -8 } else { 8 };
    let mut chop: i32 = 8;
    let mut bitcount = bitcount_seed;

    while bitcount != bitpool && chop != 0 {
        let mut total = 0;
        for sb in 0..MSBC_NUM_SUBBANDS {
            total += clamp_bn(bitneed[sb] + bitadjust);
        }

        chop >>= 1;
        if total > bitpool {
            bitadjust -= chop;
        } else {
            max_bitadjust = bitadjust;
            bitcount = total;
            bitadjust += chop;
        }
    }

    max_bitadjust
}

/// Faithful port of Bluedroid's `allocAdjustedBits`. First-pass
/// allocator that uses up to one excess bit per subband (and handles
/// the LOUDNESS "no single bit" rule).
fn alloc_adjusted_bits(dest: &mut u8, mut bits: i32, mut excess: i32) -> i32 {
    if bits < 16 {
        if bits > 1 {
            if excess > 0 {
                bits += 1;
                excess -= 1;
            }
        } else if bits == 1 && excess > 1 {
            bits = 2;
            excess -= 2;
        } else {
            bits = 0;
        }
    } else {
        bits = 16;
    }
    *dest = bits as u8;
    excess
}

/// Sync byte + fixed second header byte + bitpool, in the form that
/// feeds into the CRC-8 calculation.
pub fn fixed_header_bytes() -> [u8; 2] {
    [MSBC_HEADER_BYTE_1, MSBC_WIRE_BYTE_2]
}

/// Compose the 4 nibbles of scale-factor data into 4 bytes (8 nibbles).
pub fn pack_scale_factors(scale_factors: &[u8; 8]) -> [u8; 4] {
    let mut out = [0u8; 4];
    for sb in 0..8 {
        let nibble = scale_factors[sb] & 0x0F;
        let pos = sb / 2;
        if sb % 2 == 0 {
            out[pos] |= nibble << 4;
        } else {
            out[pos] |= nibble;
        }
    }
    out
}

/// Reverse of [`pack_scale_factors`].
pub fn unpack_scale_factors(bytes: &[u8; 4]) -> [u8; 8] {
    let mut out = [0u8; 8];
    for sb in 0..8 {
        let pos = sb / 2;
        if sb % 2 == 0 {
            out[sb] = (bytes[pos] >> 4) & 0x0F;
        } else {
            out[sb] = bytes[pos] & 0x0F;
        }
    }
    out
}

/// Validate that the first 4 bytes of a frame are a syntactically valid
/// mSBC header.
///
/// Only the sync byte (`0xAD`) is checked. The spec fixes byte 1 to
/// `0x01` and byte 2 (bitpool) to `0x1A`, but real-world receivers
/// (iPhones in particular, observed on hardware) zero those bytes on
/// the wire even though the carried frame is still standard 8-subband /
/// 15-block / bitpool-26 mSBC. Validating them here would reject every
/// frame from such a sender. The CRC at byte 3 is computed over the
/// actual byte 1 / byte 2 values and the scale factors, so a real
/// corruption still gets caught downstream by the CRC check; the codec
/// parameters used during decode are the fixed mSBC ones regardless.
pub fn parse_header(frame: &[u8]) -> Result<(), &'static str> {
    if frame.len() < 4 {
        return Err("frame shorter than mSBC header");
    }
    if frame[0] != MSBC_SYNC_BYTE {
        return Err("not an mSBC sync byte");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loudness_allocation_sums_to_bitpool() {
        // Uniform scale factors should give an even-ish allocation that
        // sums up to bitpool.
        let bits = loudness_bit_allocation(&[5, 5, 5, 5, 5, 5, 5, 5]);
        let total: u32 = bits.iter().map(|&b| b as u32).sum();
        assert_eq!(total, MSBC_BITPOOL as u32);
    }

    #[test]
    fn loudness_allocation_matches_bluedroid_reference_cases() {
        // These pinned values come from tracing Bluedroid's
        // `monoBitAllocation` (decoder/srce/bitalloc.c) — the algorithm
        // that runs on every Android phone. Wire interop requires our
        // encoder + their decoder (and vice versa) to agree on bits per
        // subband for a given SF. Diverging here scrambles q-value
        // boundaries and produces audible static.
        //
        // Uniform SF=11 (typical voice mid-magnitude):
        assert_eq!(
            loudness_bit_allocation(&[11, 11, 11, 11, 11, 11, 11, 11]),
            [5, 3, 3, 3, 3, 3, 3, 3],
        );
        // Descending SF (high-frequency rolloff):
        assert_eq!(
            loudness_bit_allocation(&[12, 11, 10, 9, 8, 7, 6, 5]),
            [7, 5, 4, 3, 3, 2, 2, 0],
        );
        // Uniform SF=5 (quiet input):
        assert_eq!(
            loudness_bit_allocation(&[5, 5, 5, 5, 5, 5, 5, 5]),
            [5, 3, 3, 3, 3, 3, 3, 3],
        );
    }

    #[test]
    fn loudness_allocation_zero_scale_factors_terminates() {
        // Edge case: all-silent input. The allocator may put zero bits
        // everywhere, and that's fine — just check we don't loop.
        let bits = loudness_bit_allocation(&[0; 8]);
        let total: u32 = bits.iter().map(|&b| b as u32).sum();
        assert!(total <= MSBC_BITPOOL as u32);
        for b in bits {
            assert!(b == 0 || (2..=16).contains(&b), "bits[?] = {}", b);
        }
    }

    #[test]
    fn scale_factor_pack_unpack_round_trip() {
        let sf = [0, 1, 2, 3, 4, 5, 6, 15];
        let packed = pack_scale_factors(&sf);
        let unpacked = unpack_scale_factors(&packed);
        assert_eq!(unpacked, sf);
    }

    #[test]
    fn parse_rejects_wrong_sync() {
        let mut frame = [0u8; 4];
        frame[0] = 0x9C; // standard SBC, not mSBC
        frame[1] = MSBC_HEADER_BYTE_1;
        frame[2] = MSBC_WIRE_BYTE_2;
        assert!(parse_header(&frame).is_err());
    }

    #[test]
    fn parse_accepts_valid_header() {
        let frame = [MSBC_SYNC_BYTE, MSBC_HEADER_BYTE_1, MSBC_WIRE_BYTE_2, 0];
        assert!(parse_header(&frame).is_ok());
    }
}
