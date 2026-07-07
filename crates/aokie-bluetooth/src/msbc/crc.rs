//! CRC-8 used by the SBC frame header.
//!
//! The SBC spec (A2DP §12.4.1.4) specifies a CRC-8 with polynomial
//! `x⁸ + x⁴ + x³ + x² + 1` (`0x1D`), MSB-first, initial value `0x0F`,
//! no final XOR. The CRC covers a specific bit range that depends on
//! the channel mode (joint stereo adds extra bits); for mSBC's mono
//! configuration the covered range is fixed (header byte 1 and the
//! bitpool byte, plus all scale factor nibbles).

/// CRC-8 table for polynomial `0x1D`, MSB-first.
const CRC8_TABLE: [u8; 256] = build_crc8_table();

const fn build_crc8_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut byte: usize = 0;
    while byte < 256 {
        let mut crc = byte as u8;
        let mut bit = 0;
        while bit < 8 {
            if (crc & 0x80) != 0 {
                crc = (crc << 1) ^ 0x1D;
            } else {
                crc <<= 1;
            }
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
}

/// Run the SBC CRC-8 over a byte stream, with optional trailing bits to
/// fold in (used by joint-stereo headers — for mSBC the trailing-bit
/// argument is always `(0, 0)`).
///
/// Returns the final CRC byte. Initial value is the SBC-spec `0x0F`.
pub fn crc8_with_trailing_bits(bytes: &[u8], trailing: u32, trailing_bits: u32) -> u8 {
    let mut crc = 0x0Fu8;
    for &b in bytes {
        crc = CRC8_TABLE[(crc ^ b) as usize];
    }
    // Fold up to 7 trailing bits MSB-first into the running CRC. Generic
    // bit-by-bit because the SBC spec only ever needs ≤ 4 trailing bits
    // and the table-driven path is byte-aligned.
    if trailing_bits > 0 {
        debug_assert!(trailing_bits < 8);
        let mut bits_left = trailing_bits;
        // Align trailing bits to the high end of an 8-bit word so each
        // shift consumes the next bit MSB-first.
        let mut t = (trailing << (8 - trailing_bits)) as u8;
        while bits_left > 0 {
            let bit = (t & 0x80) != 0;
            let high = (crc & 0x80) != 0;
            crc <<= 1;
            t <<= 1;
            if high ^ bit {
                crc ^= 0x1D;
            }
            bits_left -= 1;
        }
    }
    crc
}

/// Convenience wrapper for whole-byte CRC.
pub fn crc8(bytes: &[u8]) -> u8 {
    crc8_with_trailing_bits(bytes, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_of_empty_is_initial_value() {
        assert_eq!(crc8(&[]), 0x0F);
    }

    #[test]
    fn crc_table_first_few_entries_known() {
        // Reference values worked out by hand for poly 0x1D, MSB-first,
        // initial value 0 (the table-build initial; the runtime uses 0x0F):
        //   0x00 → 0x00 (no high bits set)
        //   0x01 → 0x1D (single bit shifts up to 0x80, then poly XOR)
        //   0x02 → 0x3A (= 2 * 0x1D)
        //   0x80 → 0x26 (high bit triggers XOR on the first shift; chain
        //                of further high bits during the 8-shift loop ends
        //                at 0x26)
        assert_eq!(CRC8_TABLE[0x00], 0x00);
        assert_eq!(CRC8_TABLE[0x01], 0x1D);
        assert_eq!(CRC8_TABLE[0x02], 0x3A);
        assert_eq!(CRC8_TABLE[0x80], 0x26);
    }

    #[test]
    fn crc_no_trailing_bits_matches_byte_loop() {
        // Spot-check against an explicit bit-by-bit reference.
        let data = [0x11u8, 0x22, 0x33, 0x44];
        let table_crc = crc8(&data);

        let mut ref_crc = 0x0Fu8;
        for &b in &data {
            ref_crc ^= b;
            for _ in 0..8 {
                if (ref_crc & 0x80) != 0 {
                    ref_crc = (ref_crc << 1) ^ 0x1D;
                } else {
                    ref_crc <<= 1;
                }
            }
        }
        assert_eq!(table_crc, ref_crc);
    }

    #[test]
    fn crc_with_4_trailing_bits_matches_bitwise_reference() {
        let data = [0xABu8, 0xCD];
        let trailing = 0b1010u32;

        let with = crc8_with_trailing_bits(&data, trailing, 4);

        // Reference: same byte loop, then 4 bits.
        let mut r = 0x0Fu8;
        for &b in &data {
            r ^= b;
            for _ in 0..8 {
                if (r & 0x80) != 0 {
                    r = (r << 1) ^ 0x1D;
                } else {
                    r <<= 1;
                }
            }
        }
        let mut t = (trailing << 4) as u8;
        for _ in 0..4 {
            let bit = (t & 0x80) != 0;
            let hi = (r & 0x80) != 0;
            r <<= 1;
            t <<= 1;
            if hi ^ bit {
                r ^= 0x1D;
            }
        }
        assert_eq!(with, r);
    }
}
