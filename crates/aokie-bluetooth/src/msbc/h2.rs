//! H2 framing for mSBC over HFP/SCO.
//!
//! Each isochronous slot carries a 60-byte payload:
//!
//! ```text
//! Byte 0  : H2 sync byte 0 = 0x01
//! Byte 1  : H2 sync byte 1, rotates through {0x08, 0x38, 0xC8, 0xF8}
//! Byte 2..58 : 57-byte mSBC frame
//! Byte 59 : trailing padding byte = 0x00
//! ```
//!
//! The receiver uses byte 1's pattern to recover sync after a packet
//! drop and to detect lost frames (a non-monotonic sequence implies a
//! gap).

use super::{
    decoder::DecodeError, MsbcDecoder, MsbcEncoder, MSBC_FRAME_SIZE, MSBC_H2_PACKET_SIZE,
    MSBC_SAMPLES_PER_FRAME, MSBC_SYNC_BYTE,
};
use std::collections::VecDeque;

/// First sync byte of every H2-framed packet.
pub const H2_SYNC_BYTE_0: u8 = 0x01;

/// Rotating second sync byte, indexed by the 2-bit frame counter.
pub const H2_SYNC_BYTE_1: [u8; 4] = [0x08, 0x38, 0xC8, 0xF8];

/// H2 trailing padding byte.
pub const H2_PADDING_BYTE: u8 = 0x00;

/// Wraps an [`MsbcEncoder`] and adds the rotating H2 header + padding so
/// each emitted packet is exactly 60 bytes.
pub struct H2Encoder {
    inner: MsbcEncoder,
    seq: u8,
}

impl Default for H2Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl H2Encoder {
    pub fn new() -> Self {
        Self {
            inner: MsbcEncoder::new(),
            seq: 0,
        }
    }

    pub fn encode_packet(
        &mut self,
        pcm: &[i16; MSBC_SAMPLES_PER_FRAME],
    ) -> [u8; MSBC_H2_PACKET_SIZE] {
        let mut out = [0u8; MSBC_H2_PACKET_SIZE];
        out[0] = H2_SYNC_BYTE_0;
        out[1] = H2_SYNC_BYTE_1[self.seq as usize];
        let frame = self.inner.encode(pcm);
        out[2..2 + MSBC_FRAME_SIZE].copy_from_slice(&frame);
        out[MSBC_H2_PACKET_SIZE - 1] = H2_PADDING_BYTE;
        self.seq = (self.seq + 1) & 0x03;
        out
    }
}

/// Wraps an [`MsbcDecoder`] and parses the H2 header before handing the
/// SBC payload to the codec. Surfaces lost packets via the returned
/// `lost` flag (next-expected sequence didn't match).
pub struct H2Decoder {
    inner: MsbcDecoder,
    /// `None` until we see the first valid H2 packet, then `Some(next)`
    /// of the expected sequence index.
    expected_seq: Option<u8>,
    /// Most recent gap (number of fully missed packets immediately
    /// preceding the last header parse). Carried so callers can fill
    /// the right number of silence frames even when the codec rejects
    /// the visible frame — without it, multi-packet losses bracketing a
    /// CRC failure would shrink the timeline.
    last_gap: u8,
    /// Most recently decoded PCM frame, retained for packet-loss
    /// concealment. `None` until the first successful decode; from
    /// then on, [`conceal_lost`] uses it as the source for fade-out
    /// frames whenever the runtime sees a sequence gap. A naive
    /// zero-fill produces an audible click on every dropped iso slot
    /// (Win32 error 87 bursts during heavy TTS); fading from the
    /// last good frame masks short losses entirely and softens longer
    /// ones to a brief muffle instead of a pop.
    last_pcm: Option<[i16; MSBC_SAMPLES_PER_FRAME]>,
}

impl Default for H2Decoder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum H2DecodeError {
    BadSync0,
    BadSync1,
    Truncated,
    Codec(DecodeError),
}

#[derive(Debug, Clone)]
pub struct DecodedH2 {
    pub samples: [i16; MSBC_SAMPLES_PER_FRAME],
    /// `true` if the H2 sequence number jumped, indicating one or more
    /// missing packets between this one and the previous decode.
    pub lost: bool,
    /// Number of fully missed H2 packets immediately preceding this
    /// one (0..=3). Distinct from `lost` so callers can fill the right
    /// number of silence frames — a 2-packet gap was previously masked
    /// as a single-frame click because the runtime PLC only emitted one
    /// silence frame regardless of the actual gap.
    pub gap: u8,
    /// Concealment frames to emit BEFORE `samples` to bridge the gap.
    /// Empty when `gap == 0`. Computed inside `decode_packet` so the
    /// fade source is the prior good frame, not the just-decoded one
    /// (calling `conceal_lost` after the decode would fade from the
    /// wrong frame because `last_pcm` has already been updated).
    pub conceal: Vec<[i16; MSBC_SAMPLES_PER_FRAME]>,
}

impl H2Decoder {
    pub fn new() -> Self {
        Self {
            inner: MsbcDecoder::new(),
            expected_seq: None,
            last_gap: 0,
            last_pcm: None,
        }
    }

    pub fn decode_packet(
        &mut self,
        packet: &[u8; MSBC_H2_PACKET_SIZE],
    ) -> Result<DecodedH2, H2DecodeError> {
        if packet[0] != H2_SYNC_BYTE_0 {
            return Err(H2DecodeError::BadSync0);
        }
        let seq = match H2_SYNC_BYTE_1.iter().position(|&b| b == packet[1]) {
            Some(s) => s as u8,
            None => return Err(H2DecodeError::BadSync1),
        };

        // Advance the H2 sequence-number tracker BEFORE running the
        // codec so a CRC failure can't desync the loss detector. The
        // previous order updated `expected_seq` only on Ok, which made
        // the next valid packet falsely report `lost = true` (and the
        // runtime double-fill silence) after every codec error.
        let gap = match self.expected_seq {
            Some(exp) => ((seq + 4 - exp) & 3) as u8,
            None => 0,
        };
        self.expected_seq = Some((seq + 1) & 0x03);
        self.last_gap = gap;

        // Build concealment frames BEFORE updating `last_pcm` so the
        // fade source is the prior good frame, not the one we're about
        // to decode. If the codec then rejects this packet, `last_pcm`
        // stays at the prior frame and the runtime can call
        // `conceal_lost(gap + 1)` from the error path to cover the same
        // gap plus the rejected slot.
        let conceal = if gap > 0 {
            self.conceal_lost(gap)
        } else {
            Vec::new()
        };

        let mut frame = [0u8; MSBC_FRAME_SIZE];
        frame.copy_from_slice(&packet[2..2 + MSBC_FRAME_SIZE]);

        let samples = self.inner.decode(&frame).map_err(H2DecodeError::Codec)?;
        self.last_pcm = Some(samples);

        Ok(DecodedH2 {
            samples,
            lost: gap > 0,
            gap,
            conceal,
        })
    }

    /// Build `n` packet-loss-concealment frames for a sequence gap.
    ///
    /// Returns `n` frames at 16 kHz / 120 samples each. Without a prior
    /// successful decode (or when `n == 0`), returns zero-fill frames so
    /// the runtime can still advance its timeline by the correct number
    /// of slots.
    ///
    /// PLC strategy: linear fade from the last good frame to silence over
    /// the gap. A single missed slot becomes a copy of the last good
    /// frame (gap=1 → scale 1.0); two missed slots fade through 1.0,
    /// 0.5; three through 1.0, 0.5, 0.25; longer gaps zero-fill from
    /// frame 4 onward. This masks the brief one-slot drops we see during
    /// heavy TTS bursts entirely, and softens longer drops to a muffled
    /// fade instead of an abrupt click.
    pub fn conceal_lost(&self, n: u8) -> Vec<[i16; MSBC_SAMPLES_PER_FRAME]> {
        const FADE_SCALES: [f32; 4] = [1.0, 0.5, 0.25, 0.0];
        let count = n as usize;
        let mut out = Vec::with_capacity(count);
        let last = match self.last_pcm {
            Some(p) => p,
            None => {
                for _ in 0..count {
                    out.push([0i16; MSBC_SAMPLES_PER_FRAME]);
                }
                return out;
            }
        };
        for i in 0..count {
            let scale = FADE_SCALES.get(i).copied().unwrap_or(0.0);
            if scale == 0.0 {
                out.push([0i16; MSBC_SAMPLES_PER_FRAME]);
                continue;
            }
            let mut frame = [0i16; MSBC_SAMPLES_PER_FRAME];
            for (slot, sample) in frame.iter_mut().zip(last.iter()) {
                *slot = (*sample as f32 * scale)
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
            out.push(frame);
        }
        out
    }

    /// Gap reported by the most recent `decode_packet` call (number of
    /// fully missed H2 packets immediately before the last parsed
    /// header). Updated even when the codec rejects the visible frame,
    /// so a runtime PLC can fill `last_gap() + 1` silence frames on
    /// codec errors without re-parsing the H2 header itself.
    pub fn last_gap(&self) -> u8 {
        self.last_gap
    }
}

/// Check whether `b` is a valid H2 sync byte 1 — i.e. one of
/// `{0x08, 0x38, 0xC8, 0xF8}`.
///
/// The structural check (rather than membership in `H2_SYNC_BYTE_1`)
/// matches BTstack's `find_h2_sync`: the lower nibble must be `0x08`,
/// and the upper nibble must satisfy `(hn>>1) & 5 == hn & 5` (a
/// parity-redundancy property: bits {1,3} mirror bits {0,2}). This
/// rejects accidental `0x01 0x?? 0xAD` triplets in random audio data
/// far more aggressively than a 4-element table lookup, since the
/// upper nibble has only 4 valid values out of 16.
fn is_valid_h2_sequence_byte(b: u8) -> bool {
    if (b & 0x0F) != 0x08 {
        return false;
    }
    let hn = b >> 4;
    ((hn >> 1) & 0x05) == (hn & 0x05)
}

/// Byte-stream re-framer for mSBC over HCI SCO transport.
///
/// HCI SCO packets are sized by the controller's HCI buffer
/// (`sco_data_packet_length` from Read_Buffer_Size), which is typically
/// 48 bytes on USB dongles regardless of the negotiated air codec.
/// 60-byte mSBC H2 frames therefore straddle HCI packet boundaries —
/// `MsbcStreamFramer` accepts arbitrary-length byte chunks and emits
/// 60-byte H2 frames as they're recovered, resyncing on the H2 sync
/// byte pair after any framing drift.
pub struct MsbcStreamFramer {
    buffer: VecDeque<u8>,
}

impl Default for MsbcStreamFramer {
    fn default() -> Self {
        Self::new()
    }
}

impl MsbcStreamFramer {
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::with_capacity(4 * MSBC_H2_PACKET_SIZE),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend(bytes);
        // Bound the buffer so a stuck stream (no sync ever found) can't
        // grow without limit. Ten frames of slack is generous; anything
        // beyond means resync is hopeless and we should drop the
        // backlog.
        const MAX_BUFFERED: usize = 10 * MSBC_H2_PACKET_SIZE;
        if self.buffer.len() > MAX_BUFFERED {
            let drop = self.buffer.len() - MAX_BUFFERED;
            self.buffer.drain(..drop);
        }
    }

    /// Try to extract the next 60-byte H2 frame from the buffer.
    ///
    /// Anchors on the mSBC sync byte (`0xAD`) at H2 frame byte 2 rather
    /// than the H2 sync byte 0 (`0x01`) at byte 0 — `0xAD` collides much
    /// less often with random audio bytes inside an mSBC frame, so we
    /// avoid the false-sync stream that the forward `0x01` search hits
    /// in production. Once we find `0xAD`, the two preceding bytes must
    /// match a valid H2 header (`0x01` + a sequence byte that satisfies
    /// the parity-redundancy check used by BTstack's
    /// `find_h2_sync`). All three bytes match → start of a real H2
    /// frame; we drop everything before it and emit the next 60 bytes.
    pub fn next_frame(&mut self) -> Option<[u8; MSBC_H2_PACKET_SIZE]> {
        loop {
            if self.buffer.len() < 3 {
                return None;
            }
            // Walk forward looking for the mSBC sync byte at offset i
            // with valid H2 header bytes at i-2 and i-1.
            let mut found_at: Option<usize> = None;
            for i in 2..self.buffer.len() {
                if self.buffer[i] != MSBC_SYNC_BYTE {
                    continue;
                }
                let h2_byte_0 = self.buffer[i - 2];
                let h2_byte_1 = self.buffer[i - 1];
                if h2_byte_0 == H2_SYNC_BYTE_0 && is_valid_h2_sequence_byte(h2_byte_1) {
                    found_at = Some(i - 2);
                    break;
                }
            }
            let start = match found_at {
                Some(start) => start,
                None => {
                    // No valid sync visible. Keep the last 2 bytes —
                    // they could be the leading H2 header of a sync we
                    // haven't seen the third byte of yet.
                    let n = self.buffer.len().saturating_sub(2);
                    self.buffer.drain(..n);
                    return None;
                }
            };
            // Drop the prefix that wasn't part of a valid frame.
            self.buffer.drain(..start);
            if self.buffer.len() < MSBC_H2_PACKET_SIZE {
                return None;
            }
            let mut frame = [0u8; MSBC_H2_PACKET_SIZE];
            for slot in frame.iter_mut() {
                *slot = self.buffer.pop_front().unwrap();
            }
            return Some(frame);
        }
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
    }
}

/// Byte-stream packager for the mSBC TX path.
///
/// Owns the [`H2Encoder`] and a small backlog of encoded bytes that
/// haven't been drained yet. The runtime calls [`Self::pop_bytes`] with
/// the controller's HCI SCO buffer size to produce one HCI SCO payload
/// at a time; the packager encodes additional 60-byte frames from the
/// PCM sample queue on demand (zero-padding when the queue runs short
/// — silence is preferable to underrunning the controller's SCO FIFO).
pub struct MsbcStreamPackager {
    encoder: H2Encoder,
    pending: VecDeque<u8>,
}

impl Default for MsbcStreamPackager {
    fn default() -> Self {
        Self::new()
    }
}

impl MsbcStreamPackager {
    pub fn new() -> Self {
        Self {
            encoder: H2Encoder::new(),
            pending: VecDeque::with_capacity(2 * MSBC_H2_PACKET_SIZE),
        }
    }

    /// Drain `n` bytes of the encoded mSBC stream, encoding additional
    /// frames from `samples` as needed. `samples` is consumed 120
    /// elements per frame; a closure shape (rather than a queue
    /// reference) keeps this module decoupled from `aokie_radio::sco`.
    pub fn pop_bytes<F>(&mut self, n: usize, mut next_frame: F) -> Vec<u8>
    where
        F: FnMut() -> [i16; MSBC_SAMPLES_PER_FRAME],
    {
        while self.pending.len() < n {
            let pcm = next_frame();
            let h2 = self.encoder.encode_packet(&pcm);
            self.pending.extend(h2);
        }
        self.pending.drain(..n).collect()
    }

    pub fn reset(&mut self) {
        self.encoder = H2Encoder::new();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msbc::MSBC_SYNC_BYTE;

    #[test]
    fn h2_packet_has_correct_layout() {
        let mut h2 = H2Encoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let pkt = h2.encode_packet(&pcm);
        assert_eq!(pkt.len(), MSBC_H2_PACKET_SIZE);
        assert_eq!(pkt[0], H2_SYNC_BYTE_0);
        assert_eq!(pkt[1], H2_SYNC_BYTE_1[0]);
        assert_eq!(pkt[2], MSBC_SYNC_BYTE); // mSBC frame starts here
        assert_eq!(pkt[MSBC_H2_PACKET_SIZE - 1], H2_PADDING_BYTE);
    }

    #[test]
    fn h2_sequence_rotates_through_4_values() {
        let mut h2 = H2Encoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let mut sync_bytes = Vec::new();
        for _ in 0..8 {
            let pkt = h2.encode_packet(&pcm);
            sync_bytes.push(pkt[1]);
        }
        // Two cycles through the rotation.
        assert_eq!(
            sync_bytes,
            vec![0x08, 0x38, 0xC8, 0xF8, 0x08, 0x38, 0xC8, 0xF8]
        );
    }

    #[test]
    fn round_trip_through_h2_preserves_silence() {
        let mut e = H2Encoder::new();
        let mut d = H2Decoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        for i in 0..6 {
            let pkt = e.encode_packet(&pcm);
            let r = d.decode_packet(&pkt).expect("decode");
            assert!(!r.lost, "no losses expected on iter {}", i);
        }
    }

    #[test]
    fn h2_decoder_flags_packet_loss() {
        let mut e = H2Encoder::new();
        let mut d = H2Decoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];

        // Encode 3 packets, drop packet #2 on the wire.
        let p0 = e.encode_packet(&pcm);
        let _p1_dropped = e.encode_packet(&pcm);
        let p2 = e.encode_packet(&pcm);

        let r0 = d.decode_packet(&p0).unwrap();
        assert!(!r0.lost);
        assert_eq!(r0.gap, 0);
        let r2 = d.decode_packet(&p2).unwrap();
        assert!(r2.lost, "decoder should flag the missing packet");
        assert_eq!(r2.gap, 1, "exactly one packet was missing");
    }

    #[test]
    fn h2_decoder_reports_gap_for_multi_packet_loss() {
        let mut e = H2Encoder::new();
        let mut d = H2Decoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];

        // Encode 4 packets, drop the middle two so the surviving
        // packets are at seq 0 and seq 3 — a 2-packet gap that the
        // mod-4 H2 counter would have masked as a 1-packet loss before
        // the gap-aware fix.
        let p0 = e.encode_packet(&pcm);
        let _ = e.encode_packet(&pcm);
        let _ = e.encode_packet(&pcm);
        let p3 = e.encode_packet(&pcm);

        d.decode_packet(&p0).unwrap();
        let r3 = d.decode_packet(&p3).unwrap();
        assert!(r3.lost);
        assert_eq!(r3.gap, 2, "two packets were missing between p0 and p3");
    }

    #[test]
    fn h2_decoder_advances_seq_on_codec_error() {
        // Reproduces the "double silence after CRC failure" bug. The
        // old decoder bailed out via `?` on Codec error and never
        // updated `expected_seq`, so the next *valid* packet falsely
        // reported lost = true. Now the seq tracker advances before
        // the codec call, so a CRC failure doesn't desync the loss
        // detector.
        let mut e = H2Encoder::new();
        let mut d = H2Decoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];

        let p0 = e.encode_packet(&pcm);
        let mut p1_corrupt = e.encode_packet(&pcm);
        // Flip the mSBC sync byte (H2 offset 2) so the H2 header
        // parses but the SBC frame is rejected by the codec.
        p1_corrupt[2] ^= 0xff;
        let p2 = e.encode_packet(&pcm);

        d.decode_packet(&p0).unwrap();
        assert!(matches!(
            d.decode_packet(&p1_corrupt),
            Err(H2DecodeError::Codec(_))
        ));
        assert_eq!(d.last_gap(), 0, "p1 followed p0 with no gap");
        let r2 = d.decode_packet(&p2).unwrap();
        assert!(!r2.lost, "p2 follows p1 directly — no spurious loss");
        assert_eq!(r2.gap, 0);
    }

    #[test]
    fn valid_h2_sequence_bytes_match_table() {
        for &good in &H2_SYNC_BYTE_1 {
            assert!(
                is_valid_h2_sequence_byte(good),
                "{:#04x} should be valid",
                good
            );
        }
        // Spot-check rejections: anything without 0x08 in the lower
        // nibble fails immediately, and several upper nibbles fail the
        // parity check.
        for bad in [
            0x00, 0x01, 0x07, 0x09, 0x18, 0x28, 0x48, 0x58, 0x68, 0x78, 0x88, 0x98,
        ] {
            assert!(
                !is_valid_h2_sequence_byte(bad),
                "{:#04x} should be invalid",
                bad
            );
        }
    }

    #[test]
    fn stream_framer_anchors_on_msbc_sync_not_h2_byte_0() {
        // Construct a stream that contains a fake 0x01 followed by a
        // valid sequence byte, but no real mSBC sync after — the framer
        // must NOT latch onto it.
        let mut framer = MsbcStreamFramer::new();
        framer.push(&[0x01, 0x08, 0x42, 0xAA, 0xBB]);
        // Without a 0xAD anchor, no frame should be produced.
        assert_eq!(framer.next_frame(), None);

        // Now follow with a real frame — it should be found.
        let mut e = H2Encoder::new();
        let real = e.encode_packet(&[0i16; MSBC_SAMPLES_PER_FRAME]);
        framer.push(&real);
        assert_eq!(framer.next_frame(), Some(real));
    }

    #[test]
    fn stream_framer_recovers_frames_split_across_chunks() {
        // Encode three frames, then deliver them as 48-byte chunks
        // (the typical USB-dongle HCI SCO buffer size). The framer
        // must reassemble the original 60-byte frames verbatim.
        let mut e = H2Encoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let frames: Vec<[u8; MSBC_H2_PACKET_SIZE]> =
            (0..3).map(|_| e.encode_packet(&pcm)).collect();

        let mut stream = Vec::new();
        for frame in &frames {
            stream.extend_from_slice(frame);
        }

        let mut framer = MsbcStreamFramer::new();
        let mut emitted = Vec::new();
        for chunk in stream.chunks(48) {
            framer.push(chunk);
            while let Some(out) = framer.next_frame() {
                emitted.push(out);
            }
        }
        assert_eq!(emitted, frames);
    }

    #[test]
    fn stream_framer_resyncs_after_garbage_prefix() {
        // Start with random non-sync bytes, then a real frame. The
        // framer should drop the prefix and surface the frame.
        let mut e = H2Encoder::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        let frame = e.encode_packet(&pcm);

        let mut framer = MsbcStreamFramer::new();
        framer.push(&[0xAA, 0xBB, 0xCC, 0x00]); // not a sync pair
        framer.push(&frame);

        assert_eq!(framer.next_frame(), Some(frame));
        assert_eq!(framer.next_frame(), None);
    }

    #[test]
    fn stream_packager_drains_arbitrary_chunk_sizes() {
        // Decoder should see the encoded byte stream regardless of how
        // we slice it for the HCI SCO transport.
        let mut packager = MsbcStreamPackager::new();
        let mut decoder = H2Decoder::new();

        // Encode three frames worth of bytes via the packager, draining
        // in 48-byte chunks. Reassemble through a framer and decode.
        let mut framer = MsbcStreamFramer::new();
        let pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        for _ in 0..(3 * MSBC_H2_PACKET_SIZE / 48 + 1) {
            let chunk = packager.pop_bytes(48, || pcm);
            framer.push(&chunk);
        }

        let mut decoded_count = 0;
        while let Some(pkt) = framer.next_frame() {
            let r = decoder.decode_packet(&pkt).expect("decode");
            assert!(!r.lost, "packager output should not look like loss");
            decoded_count += 1;
        }
        assert!(decoded_count >= 3);
    }

    #[test]
    fn conceal_lost_returns_zero_fill_before_first_decode() {
        // Without a prior successful decode the decoder has no source
        // material to fade from, so it must zero-fill but still produce
        // exactly `n` frames so the runtime advances its timeline.
        let d = H2Decoder::new();
        let frames = d.conceal_lost(2);
        assert_eq!(frames.len(), 2);
        for f in &frames {
            assert!(f.iter().all(|&s| s == 0));
        }
    }

    #[test]
    fn conceal_lost_zero_returns_empty() {
        let d = H2Decoder::new();
        assert!(d.conceal_lost(0).is_empty());
    }

    #[test]
    fn conceal_lost_fades_from_last_decoded_frame() {
        // Decode a non-silent frame, then ask for 4 concealment frames.
        // Frame 0 should be a near-copy (scale 1.0), frame 1 half
        // amplitude (0.5), frame 2 quarter (0.25), frame 3 silence.
        let mut e = H2Encoder::new();
        let mut d = H2Decoder::new();
        let mut pcm = [0i16; MSBC_SAMPLES_PER_FRAME];
        // Use a sine-ish wave so post-fade amplitude is meaningful.
        for (i, s) in pcm.iter_mut().enumerate() {
            *s = ((i as i32 * 200) % 8000) as i16;
        }
        let pkt = e.encode_packet(&pcm);
        let decoded = d.decode_packet(&pkt).expect("decode");

        let frames = d.conceal_lost(4);
        assert_eq!(frames.len(), 4);

        // Compare RMS amplitude of each concealment frame against the
        // decoded source. mSBC is lossy so we can't expect bit-equality
        // even at scale 1.0, but the relative ratios should hold.
        fn rms(frame: &[i16; MSBC_SAMPLES_PER_FRAME]) -> f64 {
            let sum: f64 = frame.iter().map(|&s| (s as f64).powi(2)).sum();
            (sum / frame.len() as f64).sqrt()
        }
        let src_rms = rms(&decoded.samples);
        assert!(src_rms > 0.0, "source frame must be non-silent for ratios");

        // Frame 0: scale 1.0 — RMS should match the source frame within
        // floating-point rounding (every sample is multiplied by 1.0).
        assert!((rms(&frames[0]) - src_rms).abs() < 1.0);
        // Frame 1: scale 0.5
        assert!((rms(&frames[1]) - src_rms * 0.5).abs() < 1.0);
        // Frame 2: scale 0.25
        assert!((rms(&frames[2]) - src_rms * 0.25).abs() < 1.0);
        // Frame 3: silence
        assert!(frames[3].iter().all(|&s| s == 0));
    }

    #[test]
    fn h2_decoder_rejects_bad_sync() {
        let mut d = H2Decoder::new();
        let mut pkt = [0u8; MSBC_H2_PACKET_SIZE];
        pkt[0] = 0x02; // wrong
        pkt[1] = 0x08;
        assert!(matches!(
            d.decode_packet(&pkt),
            Err(H2DecodeError::BadSync0)
        ));

        let mut pkt = [0u8; MSBC_H2_PACKET_SIZE];
        pkt[0] = H2_SYNC_BYTE_0;
        pkt[1] = 0x55; // not in the rotation
        assert!(matches!(
            d.decode_packet(&pkt),
            Err(H2DecodeError::BadSync1)
        ));
    }
}
