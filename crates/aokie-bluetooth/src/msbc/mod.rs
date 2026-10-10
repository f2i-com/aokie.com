//! Modified Subband Codec (mSBC) for HFP wide-band speech.
//!
//! mSBC is a constrained variant of the A2DP SBC codec used for the
//! wide-band (16 kHz) HFP voice path. The frame parameters are fixed:
//!
//! - 1 channel (MONO)
//! - 8 subbands
//! - 15 blocks per frame
//! - LOUDNESS bit allocation
//! - bitpool = 26
//! - 16 kHz sample rate (encoded as `0` in the SBC frame)
//! - Sync byte = `0xAD` (instead of `0x9C` for standard SBC)
//!
//! Each frame consumes 120 input samples (15 blocks × 8 subbands) and
//! produces a 57-byte SBC frame. The HFP transport additionally wraps
//! that frame with a 2-byte H2 sync header and a trailing padding byte
//! (60 bytes total), see [`h2`].
//!
//! The implementation is pure Rust (no FFI) and platform-agnostic so it
//! can be unit-tested on any host. The integration with the SCO TX/RX
//! path lives under `aokie_radio::sco` (Windows-only).

pub mod crc;
pub mod frame;
pub mod h2;
pub mod tables;

mod decoder;
mod encoder;

pub use decoder::MsbcDecoder;
pub use encoder::MsbcEncoder;
pub use h2::{H2Decoder, H2Encoder, MsbcStreamFramer, MsbcStreamPackager};

/// Bytes per encoded mSBC frame.
pub const MSBC_FRAME_SIZE: usize = 57;

/// PCM samples consumed per encoded frame (15 blocks × 8 subbands × 1 channel).
pub const MSBC_SAMPLES_PER_FRAME: usize = 120;

/// Bytes per H2-wrapped mSBC packet on the SCO link.
pub const MSBC_H2_PACKET_SIZE: usize = 60;

/// mSBC sync byte. Standard SBC uses `0x9C`; this variant uses `0xAD` so
/// receivers can disambiguate the two on the wire.
pub const MSBC_SYNC_BYTE: u8 = 0xAD;

pub const MSBC_NUM_BLOCKS: usize = 15;
pub const MSBC_NUM_SUBBANDS: usize = 8;
pub const MSBC_BITPOOL: u8 = 26;

/// Against BlueZ's SBC (`sbc_init_msbc`, the reference phones' stacks follow): `testdata/bluez_tones.msbc` is its
/// encoding of [`reference::tones`], 20 frames of 1, 0.3 and 3.1 kHz at 16 kHz (peak 13,733).
#[cfg(test)]
mod reference {
    use super::*;

    const BLUEZ: &[u8] = include_bytes!("testdata/bluez_tones.msbc");

    pub fn tones() -> Vec<i16> {
        let tau = std::f64::consts::TAU;
        (0..20 * MSBC_SAMPLES_PER_FRAME)
            .map(|i| {
                let t = i as f64 / 16_000.0;
                (8000.0 * (tau * 1000.0 * t).sin()
                    + 4000.0 * (tau * 300.0 * t).sin()
                    + 2000.0 * (tau * 3100.0 * t).sin())
                .round() as i16
            })
            .collect()
    }

    fn rms(x: &[i16]) -> f64 {
        (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn a_phones_frames_are_heard_at_their_level() {
        let mut decoder = decoder::MsbcDecoder::new();
        let mut heard = Vec::new();
        for frame in BLUEZ.chunks_exact(MSBC_FRAME_SIZE) {
            heard.extend(decoder.decode(frame.try_into().unwrap()).unwrap());
        }
        // (past the filters' first frames)
        let (want, got) = (rms(&tones()[240..]), rms(&heard[240..]));
        let db = 20.0 * (got / want).log10();
        assert!(
            db.abs() < 0.5,
            "BlueZ's frames heard {db:+.2} dB from what it encoded"
        );
    }

    #[test]
    fn our_frames_carry_the_scale_factors_a_phones_decoder_expects() {
        let mut encoder = encoder::MsbcEncoder::new();
        let input = tones();
        let (mut same, mut all) = (0, 0);
        for (ours, theirs) in input
            .chunks_exact(MSBC_SAMPLES_PER_FRAME)
            .map(|pcm| encoder.encode(pcm.try_into().unwrap()))
            .zip(BLUEZ.chunks_exact(MSBC_FRAME_SIZE))
            .skip(2)
        {
            let (a, b) = (
                frame::unpack_scale_factors(&ours[4..8].try_into().unwrap()),
                frame::unpack_scale_factors(&theirs[4..8].try_into().unwrap()),
            );
            same += a.iter().zip(&b).filter(|(x, y)| x == y).count();
            all += a.len();
        }
        // (a float analysis filter against BlueZ's fixed point: a peak at a power of two may round either way)
        assert!(
            same * 100 >= all * 95,
            "{same} of {all} scale factors are BlueZ's"
        );
    }
}
