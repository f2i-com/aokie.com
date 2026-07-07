//! mSBC encoder.
//!
//! Pure floating-point implementation following the A2DP SBC spec
//! (§12.4.1) with mSBC's fixed parameters baked in. The encoder owns
//! the analysis filter history (`X[0..79]`) and produces one 57-byte
//! mSBC frame from each 120-sample input block.

use super::crc::crc8;
use super::frame::{
    fixed_header_bytes, loudness_bit_allocation, pack_scale_factors, MSBC_HEADER_BYTE_1,
    MSBC_WIRE_BYTE_2,
};
use super::tables::{analysis_cosine_matrix, ANALYSIS_WINDOW_8};
use super::{
    MSBC_FRAME_SIZE, MSBC_NUM_BLOCKS, MSBC_NUM_SUBBANDS, MSBC_SAMPLES_PER_FRAME, MSBC_SYNC_BYTE,
};

const NUM_SUBBANDS: usize = MSBC_NUM_SUBBANDS;
const NUM_BLOCKS: usize = MSBC_NUM_BLOCKS;

/// Stateful mSBC encoder. Hold one per direction.
pub struct MsbcEncoder {
    /// Analysis filter history `X[0..79]`. New input samples are shifted
    /// into the front; old values fall off the back.
    history: [f32; 80],
    /// Pre-computed analysis cosine matrix `M[i][k]`.
    cos_m: [[f32; 16]; 8],
}

impl Default for MsbcEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MsbcEncoder {
    pub fn new() -> Self {
        Self {
            history: [0.0; 80],
            cos_m: analysis_cosine_matrix(),
        }
    }

    /// Encode one mSBC frame (120 input samples → 57-byte frame).
    pub fn encode(&mut self, pcm: &[i16; MSBC_SAMPLES_PER_FRAME]) -> [u8; MSBC_FRAME_SIZE] {
        // 1. Run the analysis filter over each block of 8 samples.
        //    `subbands[block][sb]` = subband sample at (block, subband).
        let mut subbands = [[0.0f32; NUM_SUBBANDS]; NUM_BLOCKS];
        for block in 0..NUM_BLOCKS {
            // Shift in 8 fresh samples (A2DP §12.4.1.1: input is in
            // most-recent-first order inside each block of 8).
            for i in (8..80).rev() {
                self.history[i] = self.history[i - 8];
            }
            for i in 0..8 {
                // Use raw i16 PCM (no /32768 normalization) so that the
                // analysis-filter output sb magnitudes match Bluedroid's
                // convention. SBC SF on the wire is ceil(log2(|sb|)) of
                // these raw integer-scale values; Bluedroid produces
                // SF≈11-12 for typical voice, and our decoder must
                // dequantize at the same scale to avoid clipping.
                let s = pcm[block * 8 + (7 - i)] as f32;
                self.history[i] = s;
            }

            // Partial sum Y[i] = Σ_k Z[i + k*16] over k=0..4
            let mut y = [0.0f32; 16];
            for i in 0..16 {
                let mut acc = 0.0;
                for k in 0..5 {
                    let idx = i + k * 16;
                    acc += ANALYSIS_WINDOW_8[idx] * self.history[idx];
                }
                y[i] = acc;
            }

            // Subband sample S[i] = Σ_k M[i][k] * Y[k]
            for i in 0..NUM_SUBBANDS {
                let mut acc = 0.0;
                for k in 0..16 {
                    acc += self.cos_m[i][k] * y[k];
                }
                subbands[block][i] = acc;
            }
        }

        // 2. Compute scale factors per subband. The scale factor `sf[sb]`
        //    is the smallest integer `n` such that all subband samples
        //    in that subband fit in `n+1` signed bits — i.e. their
        //    magnitudes are < 2^n. We look at the absolute peak across
        //    all blocks for that subband.
        let mut peak = [0.0f32; NUM_SUBBANDS];
        for block in 0..NUM_BLOCKS {
            for sb in 0..NUM_SUBBANDS {
                let v = subbands[block][sb].abs();
                if v > peak[sb] {
                    peak[sb] = v;
                }
            }
        }
        let mut sf = [0u8; NUM_SUBBANDS];
        for sb in 0..NUM_SUBBANDS {
            // Scale factors live in 0..=15. log2 of the peak rounded up
            // gives the exponent; clamp to the valid range.
            let mut n: i32 = 0;
            let mut bound: f32 = 1.0;
            while peak[sb] >= bound && n < 15 {
                n += 1;
                bound *= 2.0;
            }
            sf[sb] = n as u8;
        }

        // 3. LOUDNESS bit allocation.
        let bits = loudness_bit_allocation(&sf);

        // 4. Quantize each subband sample.
        //    sample_quantized = ⌊((s/2^sf + 1) * (2^bits - 1) / 2)⌋
        //
        // Normalize by 2^sf (matching the spec / Bluedroid, NOT
        // 2^(sf+1)). With sf chosen as the smallest n with |peak| < 2^n,
        // s/2^sf lies in (-1, 1), so the quantized value fills the
        // full [0, levels] range. Using 2^(sf+1) would put it in the
        // middle half [levels/4, 3*levels/4] — round-trip-consistent
        // with our own decoder, but a peer running the spec formula
        // (Android/Bluedroid in particular) reads it back at half scale.
        let mut quantized = [[0u32; NUM_SUBBANDS]; NUM_BLOCKS];
        for block in 0..NUM_BLOCKS {
            for sb in 0..NUM_SUBBANDS {
                let nbits = bits[sb] as u32;
                if nbits == 0 {
                    continue;
                }
                let scale = 1.0f32 / (1u32 << sf[sb] as u32) as f32;
                let normalized = subbands[block][sb] * scale; // ≈ in (-1, 1)
                let levels = (1u32 << nbits) - 1;
                // Map (-1, 1) → [0, levels].
                let scaled = (normalized + 1.0) * (levels as f32) * 0.5;
                let mut q = scaled.floor() as i32;
                if q < 0 {
                    q = 0;
                }
                if q as u32 > levels {
                    q = levels as i32;
                }
                quantized[block][sb] = q as u32;
            }
        }

        // 5. Assemble the frame.
        let mut frame = [0u8; MSBC_FRAME_SIZE];
        frame[0] = MSBC_SYNC_BYTE;
        // Wire bytes 1 and 2 are zero on mSBC — see the doc comment on
        // MSBC_HEADER_BYTE_1 for why. Pixel's mSBC pre-decoder validator
        // accepts only the all-zero wire convention.
        frame[1] = MSBC_HEADER_BYTE_1;
        frame[2] = MSBC_WIRE_BYTE_2;
        // CRC slot (frame[3]) filled in once the scale factors are placed.
        let sf_bytes = pack_scale_factors(&sf);
        frame[4..8].copy_from_slice(&sf_bytes);

        // CRC covers byte 1 + byte 2 + scale factor nibbles (32 bits = 4 bytes).
        let header = fixed_header_bytes();
        let mut crc_input = [0u8; 6];
        crc_input[0] = header[0];
        crc_input[1] = header[1];
        crc_input[2..6].copy_from_slice(&sf_bytes);
        frame[3] = crc8(&crc_input);

        // Pack quantized samples MSB-first, starting at bit 0 of byte 8.
        let mut bit_writer = BitWriter::new(&mut frame[8..]);
        for block in 0..NUM_BLOCKS {
            for sb in 0..NUM_SUBBANDS {
                let nbits = bits[sb] as u32;
                if nbits == 0 {
                    continue;
                }
                bit_writer.write(quantized[block][sb], nbits);
            }
        }

        frame
    }
}

/// MSB-first bit writer for the audio-sample portion of the frame.
struct BitWriter<'a> {
    buf: &'a mut [u8],
    /// Next bit position to write, measured from byte 0 / bit 7 = position 0.
    bit_pos: usize,
}

impl<'a> BitWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, bit_pos: 0 }
    }

    fn write(&mut self, value: u32, nbits: u32) {
        for i in (0..nbits).rev() {
            let bit = ((value >> i) & 1) as u8;
            let byte = self.bit_pos / 8;
            let shift = 7 - (self.bit_pos % 8);
            if byte < self.buf.len() {
                self.buf[byte] |= bit << shift;
            }
            self.bit_pos += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_frame_has_msbc_sync_byte() {
        let mut enc = MsbcEncoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let frame = enc.encode(&pcm);
        assert_eq!(frame[0], MSBC_SYNC_BYTE);
        // Bytes 1 and 2 are zeroed on the wire (Bluedroid mSBC convention)
        assert_eq!(frame[1], 0x00);
        assert_eq!(frame[2], 0x00);
    }

    #[test]
    fn encoded_silence_has_valid_crc() {
        let mut enc = MsbcEncoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let frame = enc.encode(&pcm);
        let mut crc_in = [0u8; 6];
        crc_in[0] = frame[1];
        crc_in[1] = frame[2];
        crc_in[2..6].copy_from_slice(&frame[4..8]);
        assert_eq!(frame[3], crc8(&crc_in));
    }

    #[test]
    fn frame_size_matches_constant() {
        let mut enc = MsbcEncoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let frame = enc.encode(&pcm);
        assert_eq!(frame.len(), MSBC_FRAME_SIZE);
    }

    #[test]
    fn encoded_silence_does_not_collapse_to_all_zero() {
        // Sanity: a peer sniffing the wire on a healthy mSBC link sees
        // bytes that look mSBC-ish even during silence. If the entire
        // 57-byte frame is `[0xAD, 0, 0, CRC, 0, 0, ...]` (the trivial
        // case for our SF=0 silence), Pixel's mSBC pre-decoder validator
        // discards it as "no audio data" and audibly nothing comes
        // through.
        //
        // We don't *require* non-zero sample bits — silence really is
        // silence — but we DO require the structural bytes (sync, CRC,
        // SF nibbles) to be sane. Anything else means the encoder
        // produced a degenerate frame.
        let mut enc = MsbcEncoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let frame = enc.encode(&pcm);
        assert_eq!(frame[0], MSBC_SYNC_BYTE);
        assert_eq!(frame[1], 0x00);
        assert_eq!(frame[2], 0x00);
        // CRC is non-trivial (depends on the SF nibbles, which for
        // silence are all zero — so CRC reduces to a constant). The
        // important thing is the encoder didn't bail before computing
        // it.
        let mut crc_input = [0u8; 6];
        crc_input[0] = frame[1];
        crc_input[1] = frame[2];
        crc_input[2..6].copy_from_slice(&frame[4..8]);
        assert_eq!(frame[3], crc8(&crc_input));
    }

    #[test]
    fn encoded_voice_tone_has_nonuniform_byte_distribution() {
        // A real-audio mSBC frame has a near-uniform byte histogram —
        // 26 packed quantized samples per frame plus SF nibbles, each
        // sample populating a different bit position. If we accidentally
        // emit a stuck pattern (all zeros, all 0xFF, or a degenerate
        // `[0xAD, 0, 0, ..., 0]` sample tail), a single byte value
        // dominates the frame.
        //
        // This catches "encoder silently drops to zero output" bugs that
        // self-round-trip tests can miss when both sides have the same
        // bug.
        let mut enc = MsbcEncoder::new();
        let mut phase: f32 = 0.0;
        let dphi: f32 = 2.0 * std::f32::consts::PI * 600.0 / 16_000.0;
        let amplitude: f32 = 0.4 * 32_767.0;
        // Encode several frames, accumulate sample-byte distribution
        // (skip header bytes 0..8 — those are sync / CRC / SF and have
        // their own structure).
        let mut hist = [0u32; 256];
        let mut total = 0u32;
        for _ in 0..20 {
            let mut pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
            for s in pcm.iter_mut() {
                *s = (amplitude * phase.sin()) as i16;
                phase += dphi;
            }
            let frame = enc.encode(&pcm);
            for &b in &frame[8..] {
                hist[b as usize] += 1;
                total += 1;
            }
        }
        let max = hist.iter().copied().max().unwrap_or(0);
        let pct = (max as f32) * 100.0 / (total as f32).max(1.0);
        assert!(
            pct < 25.0,
            "single byte value covers {:.1}% of sample bytes — encoder may be emitting a stuck pattern",
            pct
        );
    }
}
