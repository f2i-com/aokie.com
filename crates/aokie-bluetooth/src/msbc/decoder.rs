//! mSBC decoder.
//!
//! Reverses what [`super::encoder`] does: parses the frame header,
//! reconstructs scale factors and bit allocation, dequantizes the
//! sample bits, and runs the SBC synthesis filter to produce 120 PCM
//! samples per frame.

use super::crc::crc8;
use super::frame::{loudness_bit_allocation, parse_header, unpack_scale_factors};
use super::tables::{synthesis_cosine_matrix, synthesis_window_8};
use super::{MSBC_FRAME_SIZE, MSBC_NUM_BLOCKS, MSBC_NUM_SUBBANDS, MSBC_SAMPLES_PER_FRAME};

const NUM_SUBBANDS: usize = MSBC_NUM_SUBBANDS;
const NUM_BLOCKS: usize = MSBC_NUM_BLOCKS;

/// Stateful mSBC decoder. Hold one per direction.
pub struct MsbcDecoder {
    /// Synthesis filter history `V[0..159]`. Conceptually a 160-sample
    /// shift register that the synthesis filter writes 16 fresh values
    /// into per block of 8 PCM samples produced.
    v: [f32; 160],
    /// Pre-computed inverse-DCT cosine matrix `N[i][k]`.
    cos_n: [[f32; 8]; 16],
    /// Pre-computed synthesis window `D[i]`.
    window: [f32; 80],
}

impl Default for MsbcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    BadHeader(&'static str),
    BadCrc,
    Truncated,
}

impl MsbcDecoder {
    pub fn new() -> Self {
        Self {
            v: [0.0; 160],
            cos_n: synthesis_cosine_matrix(),
            window: synthesis_window_8(),
        }
    }

    /// Decode one mSBC frame into 120 PCM samples.
    pub fn decode(
        &mut self,
        frame: &[u8; MSBC_FRAME_SIZE],
    ) -> Result<[i16; MSBC_SAMPLES_PER_FRAME], DecodeError> {
        // Header / CRC.
        parse_header(frame).map_err(DecodeError::BadHeader)?;

        let mut crc_in = [0u8; 6];
        crc_in[0] = frame[1];
        crc_in[1] = frame[2];
        crc_in[2..6].copy_from_slice(&frame[4..8]);
        if crc8(&crc_in) != frame[3] {
            return Err(DecodeError::BadCrc);
        }

        // Scale factors + bit allocation.
        let mut sf_bytes = [0u8; 4];
        sf_bytes.copy_from_slice(&frame[4..8]);
        let sf = unpack_scale_factors(&sf_bytes);
        let bits = loudness_bit_allocation(&sf);

        // Read sample bits.
        let mut reader = BitReader::new(&frame[8..]);
        let mut subbands = [[0.0f32; NUM_SUBBANDS]; NUM_BLOCKS];
        for block in 0..NUM_BLOCKS {
            for sb in 0..NUM_SUBBANDS {
                let nbits = bits[sb] as u32;
                if nbits == 0 {
                    continue;
                }
                let q = reader.read(nbits);
                let levels = (1u32 << nbits) - 1;
                // SBC dequantization (A2DP §12.6.5 / Bluedroid):
                //   subband = ((2*q + 1) / levels - 1) * 2^sf
                // Note the scale is 2^sf (NOT 2^(sf+1)). The encoder
                // normalizes a subband sample with peak < 2^sf into
                // [-1, 1] by dividing by 2^sf, so the decoder uses the
                // same factor to undo it. Using 2^(sf+1) produces values
                // 2× too large and clips when the encoder fills the
                // full quantized range, as Bluedroid (Android / Pixel)
                // does on the wire.
                let normalized = (q as f32 + 0.5) * 2.0 / (levels as f32) - 1.0;
                let scale = (1u32 << sf[sb] as u32) as f32;
                subbands[block][sb] = normalized * scale;
            }
        }

        // Synthesis filter — produce 8 output samples per block.
        let mut pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        for block in 0..NUM_BLOCKS {
            // Shift V back by 2M = 16 each block, preserving 10 blocks of
            // history. V[16..159] = V[0..143]; V[0..15] is then overwritten
            // with this block's IDCT output.
            for i in (16..160).rev() {
                self.v[i] = self.v[i - 16];
            }
            // V[i] for i ∈ 0..15 = Σ_k N[i][k] * S[k]
            for i in 0..16 {
                let mut acc = 0.0f32;
                for k in 0..NUM_SUBBANDS {
                    acc += self.cos_n[i][k] * subbands[block][k];
                }
                self.v[i] = acc;
            }
            // The intermediate U[80] is built by gathering specific
            // entries from V; per A2DP §12.4.2.1, the indices are:
            //   U[j*16 + n] = V[j*32 + n] for n ∈ 0..7
            //   U[j*16 + n] = V[j*32 + 24 + n] for n ∈ 8..15
            //   for j ∈ 0..4 (giving 80 U entries)
            let mut u = [0.0f32; 80];
            for j in 0..5 {
                for n in 0..8 {
                    u[j * 16 + n] = self.v[j * 32 + n];
                }
                for n in 0..8 {
                    u[j * 16 + 8 + n] = self.v[j * 32 + 24 + n];
                }
            }
            // W = D · U (synthesis window), then output sample i is
            // sum of W[i + j*8] for j ∈ 0..9.
            let mut w = [0.0f32; 80];
            for j in 0..80 {
                w[j] = self.window[j] * u[j];
            }
            for i in 0..NUM_SUBBANDS {
                let mut acc = 0.0f32;
                for j in 0..10 {
                    acc += w[i + j * 8];
                }
                // The encoder feeds raw i16 PCM (no /32768 normalization)
                // through the analysis filter, so subband samples are in
                // i16-absolute scale (Bluedroid's convention). Synthesis
                // reverses that with unity gain, producing PCM in i16
                // range directly — no post-multiply needed. Just clamp.
                let s = acc.clamp(i16::MIN as f32, i16::MAX as f32);
                pcm[block * 8 + i] = s as i16;
            }
        }

        Ok(pcm)
    }
}

/// MSB-first bit reader for the audio-sample portion of the frame.
struct BitReader<'a> {
    buf: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, bit_pos: 0 }
    }

    fn read(&mut self, nbits: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..nbits {
            let byte = self.bit_pos / 8;
            let shift = 7 - (self.bit_pos % 8);
            let bit = if byte < self.buf.len() {
                (self.buf[byte] >> shift) & 1
            } else {
                0
            };
            value = (value << 1) | (bit as u32);
            self.bit_pos += 1;
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::super::encoder::MsbcEncoder;
    use super::*;

    #[test]
    fn decoding_silence_round_trips_to_zero_ish() {
        let mut enc = MsbcEncoder::new();
        let mut dec = MsbcDecoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let frame = enc.encode(&pcm);
        let decoded = dec.decode(&frame).expect("decode silence");
        // Silence in → near-silence out (exact zero unlikely due to
        // dequantization noise, but the magnitude should be tiny).
        let max_abs = decoded.iter().map(|&s| s.unsigned_abs()).max().unwrap_or(0);
        assert!(
            max_abs < 1024,
            "silence decoded too loud: max = {}",
            max_abs
        );
    }

    #[test]
    fn decode_rejects_bad_crc() {
        let mut enc = MsbcEncoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let mut frame = enc.encode(&pcm);
        frame[3] ^= 0xFF;
        let mut dec = MsbcDecoder::new();
        assert_eq!(dec.decode(&frame), Err(DecodeError::BadCrc));
    }

    #[test]
    fn decode_rejects_bad_sync() {
        let mut enc = MsbcEncoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let mut frame = enc.encode(&pcm);
        frame[0] = 0x9C;
        let mut dec = MsbcDecoder::new();
        assert!(matches!(dec.decode(&frame), Err(DecodeError::BadHeader(_))));
    }

    #[test]
    fn round_trip_preserves_low_frequency_tone_envelope() {
        // Generate a 400 Hz sine at 16 kHz, run it through the encoder
        // and decoder for several frames, and check the peak amplitude
        // doesn't collapse to zero.
        let mut enc = MsbcEncoder::new();
        let mut dec = MsbcDecoder::new();

        let mut phase: f32 = 0.0;
        let dphi: f32 = 2.0 * std::f32::consts::PI * 400.0 / 16_000.0;
        let amplitude: f32 = 0.4 * 32_767.0;

        // i32 to dodge `i16::MIN.abs()` overflow if a sample happens to
        // land exactly at the negative clamp.
        let mut peak_decoded: i32 = 0;
        // Skip the first couple frames — the synthesis filter needs ~80
        // samples of state before its output settles.
        for frame_idx in 0..6 {
            let mut pcm_in = [0i16; MSBC_SAMPLES_PER_FRAME];
            for s in pcm_in.iter_mut() {
                *s = (amplitude * phase.sin()) as i16;
                phase += dphi;
            }
            let frame = enc.encode(&pcm_in);
            let decoded = dec.decode(&frame).expect("decode");
            if frame_idx >= 2 {
                for &s in &decoded {
                    let mag = (s as i32).abs();
                    if mag > peak_decoded {
                        peak_decoded = mag;
                    }
                }
            }
        }
        assert!(
            peak_decoded > 8_000,
            "peak after round-trip = {} (expected > 8000)",
            peak_decoded
        );
        // Upper bound: input amplitude was 0.4 * 32767 ≈ 13107. A
        // correctly scaled round-trip lands close to that. If it pegs
        // at 32767/32768 the codec is silently clipping, which happens
        // when the polyphase 1/NS factor is missing from synthesis.
        assert!(
            peak_decoded < 20_000,
            "peak after round-trip = {} (expected < 20000 — clipping?)",
            peak_decoded
        );
    }

    #[test]
    fn encoder_sf_matches_bluedroid_range_for_typical_voice() {
        // Bluedroid produces SF ≈ 11-12 for typical voice signals (we
        // observed this in real Pixel/Android wire data). Our encoder
        // must produce comparable SF values, otherwise our wire format
        // is inconsistent with Bluedroid's at the magnitude level and
        // the receiving side decodes silence or saturated noise.
        let mut enc = MsbcEncoder::new();
        let mut phase: f32 = 0.0;
        let dphi: f32 = 2.0 * std::f32::consts::PI * 400.0 / 16_000.0;
        let amplitude: f32 = 0.4 * 32_767.0;
        let mut last_sf = [0u8; 8];
        for _ in 0..6 {
            let mut pcm_in = [0i16; MSBC_SAMPLES_PER_FRAME];
            for s in pcm_in.iter_mut() {
                *s = (amplitude * phase.sin()) as i16;
                phase += dphi;
            }
            let frame = enc.encode(&pcm_in);
            last_sf = super::super::frame::unpack_scale_factors(&frame[4..8].try_into().unwrap());
        }
        // For a 0.4-amplitude tone in subband 0, SF[0] should be in the
        // double-digit range (Bluedroid sends SF=11-12 typically).
        assert!(
            last_sf[0] >= 8,
            "SF[0] = {} (too low — encoder probably still normalizing)",
            last_sf[0]
        );
        assert!(
            last_sf[0] <= 14,
            "SF[0] = {} (too high — encoder probably overdriving)",
            last_sf[0]
        );
    }

    #[test]
    fn round_trip_gain_is_unity_within_tolerance() {
        // The polyphase analysis/synthesis filters should give near-unity
        // gain across the audio band. Tolerate up to ~30% gain variation
        // (real SBC has some subband-edge ripple), but flag anything
        // wildly off — that's the symptom of a wrong window or matrix.
        for &(freq, amp_frac) in &[
            (200.0f32, 0.4f32),
            (400.0, 0.4),
            (500.0, 0.4),
            (800.0, 0.4),
            (1500.0, 0.4),
            (2500.0, 0.4),
            (3500.0, 0.4),
        ] {
            let mut enc = MsbcEncoder::new();
            let mut dec = MsbcDecoder::new();
            let mut phase: f32 = 0.0;
            let dphi: f32 = 2.0 * std::f32::consts::PI * freq / 16_000.0;
            let amplitude: f32 = amp_frac * 32_767.0;
            let mut peak_decoded: i32 = 0;
            // Run many frames for steady state, skip first 5 for filter warmup.
            for frame_idx in 0..30 {
                let mut pcm_in = [0i16; MSBC_SAMPLES_PER_FRAME];
                for s in pcm_in.iter_mut() {
                    *s = (amplitude * phase.sin()) as i16;
                    phase += dphi;
                }
                let frame = enc.encode(&pcm_in);
                let decoded = dec.decode(&frame).expect("decode");
                if frame_idx >= 5 {
                    for &s in &decoded {
                        let mag = (s as i32).abs();
                        if mag > peak_decoded {
                            peak_decoded = mag;
                        }
                    }
                }
            }
            let expected = amplitude as i32;
            let gain = peak_decoded as f32 / expected as f32;
            assert!(
                (0.6..1.4).contains(&gain),
                "round-trip {freq}Hz amp={amp_frac} expected={expected} got={peak_decoded} gain={gain:.3} — outside [0.6, 1.4]",
            );
        }
    }

    #[test]
    #[ignore]
    fn print_round_trip_gain_at_many_frequencies() {
        // Diagnostic: prints gain + spectral SNR at fine frequency steps
        // so we can see exactly where the codec adds noise. SNR is
        // measured in the frequency domain (signal energy at the input
        // bin vs all other bins), which is delay-invariant — a far
        // cleaner metric than time-domain difference. Run with
        // `cargo test --lib print_round_trip_gain_at_many_frequencies
        //  -- --ignored --nocapture`.
        for f in (100..=4000).step_by(100) {
            let freq = f as f32;
            let mut enc = MsbcEncoder::new();
            let mut dec = MsbcDecoder::new();
            let mut phase: f32 = 0.0;
            let dphi: f32 = 2.0 * std::f32::consts::PI * freq / 16_000.0;
            let amplitude: f32 = 0.4 * 32_767.0;
            let mut peak_decoded: i32 = 0;
            let mut decoded_samples: Vec<f64> = Vec::new();
            for frame_idx in 0..50 {
                let mut pcm_in = [0i16; MSBC_SAMPLES_PER_FRAME];
                for s in pcm_in.iter_mut() {
                    *s = (amplitude * phase.sin()) as i16;
                    phase += dphi;
                }
                let frame = enc.encode(&pcm_in);
                let decoded = dec.decode(&frame).expect("decode");
                if frame_idx >= 10 {
                    for &s in &decoded {
                        decoded_samples.push(s as f64);
                        let mag = (s as i32).abs();
                        if mag > peak_decoded {
                            peak_decoded = mag;
                        }
                    }
                }
            }
            // Hann window to reduce spectral leakage from non-integer
            // period truncation.
            let n = decoded_samples.len();
            let windowed: Vec<f64> = decoded_samples
                .iter()
                .enumerate()
                .map(|(i, &s)| {
                    let w =
                        0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
                    s * w
                })
                .collect();
            // Naive O(N^2) DFT — fine for a few thousand samples in a
            // diagnostic behind --ignored.
            let nyquist_bin = n / 2;
            let bin_hz = 16_000.0 / n as f64;
            let signal_bin = (freq as f64 / bin_hz).round() as usize;
            let signal_band: i64 = 3;
            let mut signal_pwr = 0.0;
            let mut total_pwr = 0.0;
            for k in 1..nyquist_bin {
                let mut re = 0.0;
                let mut im = 0.0;
                let theta_step = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
                for (i, &s) in windowed.iter().enumerate() {
                    let theta = theta_step * i as f64;
                    re += s * theta.cos();
                    im -= s * theta.sin();
                }
                let p = re * re + im * im;
                total_pwr += p;
                if (k as i64 - signal_bin as i64).abs() <= signal_band {
                    signal_pwr += p;
                }
            }
            let noise_pwr = (total_pwr - signal_pwr).max(1e-9);
            let snr_db = 10.0 * (signal_pwr / noise_pwr).log10();
            let gain = peak_decoded as f32 / amplitude as f32;
            println!("freq={freq:>5.0}Hz peak={peak_decoded:>5} gain={gain:.3} snr={snr_db:.1}dB",);
        }
    }

    #[test]
    fn round_trip_full_scale_does_not_clip() {
        // Loud speech / cellular downlink audio can push close to full
        // scale. If our codec amplifies, this will clip and saturate
        // (peak hits 32767 / 32768 with high RMS) — that's what we saw
        // on hardware before.
        let mut enc = MsbcEncoder::new();
        let mut dec = MsbcDecoder::new();
        let mut phase: f32 = 0.0;
        let dphi: f32 = 2.0 * std::f32::consts::PI * 600.0 / 16_000.0;
        let amplitude: f32 = 0.9 * 32_767.0;
        let mut peak_decoded: i32 = 0;
        for frame_idx in 0..6 {
            let mut pcm_in = [0i16; MSBC_SAMPLES_PER_FRAME];
            for s in pcm_in.iter_mut() {
                *s = (amplitude * phase.sin()) as i16;
                phase += dphi;
            }
            let frame = enc.encode(&pcm_in);
            let decoded = dec.decode(&frame).expect("decode");
            if frame_idx >= 2 {
                for &s in &decoded {
                    let mag = (s as i32).abs();
                    if mag > peak_decoded {
                        peak_decoded = mag;
                    }
                }
            }
        }
        // Input amplitude was 0.9 * 32767 ≈ 29490. Allow a generous
        // upper bound (~37k) for filter overshoot, but it should NOT
        // clip at 32767/32768 — that means saturation, which is what
        // hardware was doing before the codec fix.
        assert!(
            peak_decoded < 32_767,
            "peak after full-scale round-trip = {} (clipping at i16 rail)",
            peak_decoded
        );
    }
}
