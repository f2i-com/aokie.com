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
