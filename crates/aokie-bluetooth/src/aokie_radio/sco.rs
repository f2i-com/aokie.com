use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoPacket<'a> {
    pub connection_handle: u16,
    pub packet_status_flag: u8,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScoStats {
    pub packets: usize,
    pub payload_bytes: usize,
    pub bad_packets: usize,
    pub last_connection_handle: Option<u16>,
    pub last_packet_status_flag: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LinearPcmStats {
    pub frames: usize,
    pub samples: usize,
    pub bad_frames: usize,
    pub min_sample: Option<i16>,
    pub max_sample: Option<i16>,
    pub last_frame_samples: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScoPacketAssembler {
    buffer: VecDeque<u8>,
    /// The SCO link's connection handle, once it is up ([`Self::set_handle`]): a packet whose header names another
    /// is a misaligned stream (an isochronous packet lost on the way in), and the bytes are slid past one at a time
    /// until a header lines up again. Without it, one lost USB packet shifted the framing for the rest of the call.
    handle: Option<u16>,
    resynced_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinearPcmTxQueue {
    samples: VecDeque<i16>,
    capacity_samples: usize,
    dropped_samples: usize,
    /// Last sample emitted by `pop_sco_packet`. Holds the queue's
    /// "current" output value across the audio→silence boundary so the
    /// in-packet zero-pad path can decay smoothly toward 0 instead of
    /// jumping there in a single sample. CVSD's adaptive step size
    /// can't follow a one-sample drop from full-scale to zero — the
    /// resulting slope-overload reads as static at every TTS-burst tail
    /// where the queue dries up mid-packet.
    last_emitted: i16,
    /// Samples left of the fade-in after the queue ran dry: audio that comes back mid-word starts from silence over
    /// [`FADE_IN_SAMPLES`] instead of with a jump (a click).
    fade_in: usize,
    /// The queue ran dry with audio still playing (the last sample not yet silence): how many times.
    underruns: usize,
}

/// The fade back in after the outgoing queue ran dry: 3 ms at 8 kHz, 1.5 ms at 16 kHz.
const FADE_IN_SAMPLES: usize = 24;

impl ScoStats {
    pub fn record_packet(&mut self, packet: &[u8]) -> Result<(), String> {
        match parse_sco_packet(packet) {
            Ok(parsed) => {
                self.packets += 1;
                self.payload_bytes += parsed.payload.len();
                self.last_connection_handle = Some(parsed.connection_handle);
                self.last_packet_status_flag = Some(parsed.packet_status_flag);
                Ok(())
            }
            Err(err) => {
                self.bad_packets += 1;
                Err(err)
            }
        }
    }
}

impl LinearPcmStats {
    pub fn record_sco_packet(&mut self, packet: &[u8]) -> Result<(), String> {
        match linear_pcm_samples_from_sco_packet(packet) {
            Ok(samples) => {
                self.frames += 1;
                self.samples += samples.len();
                self.last_frame_samples = Some(samples.len());
                for sample in samples {
                    self.min_sample = Some(self.min_sample.map_or(sample, |min| min.min(sample)));
                    self.max_sample = Some(self.max_sample.map_or(sample, |max| max.max(sample)));
                }
                Ok(())
            }
            Err(err) => {
                self.bad_frames += 1;
                Err(err)
            }
        }
    }
}

impl ScoPacketAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The SCO link's handle (`None` when it goes down): packets for it are what the stream carries.
    pub fn set_handle(&mut self, handle: Option<u16>) {
        self.handle = handle.map(|h| h & 0x0fff);
    }

    /// Bytes slid past to find a packet's header again, since this assembler was made.
    pub fn resynced_bytes(&self) -> usize {
        self.resynced_bytes
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.buffer.extend(bytes);
        let mut packets = Vec::new();

        loop {
            if self.buffer.len() < 3 {
                break;
            }

            if let Some(handle) = self.handle {
                if u16::from_le_bytes([self.buffer[0], self.buffer[1]]) & 0x0fff != handle {
                    self.buffer.pop_front();
                    self.resynced_bytes += 1;
                    continue;
                }
            }
            let payload_len = self.buffer[2] as usize;
            let packet_len = 3 + payload_len;
            if self.buffer.len() < packet_len {
                break;
            }

            let packet = self.buffer.drain(..packet_len).collect::<Vec<_>>();
            parse_sco_packet(&packet)?;
            packets.push(packet);
        }

        if self.buffer.len() > 512 {
            self.buffer.clear();
            return Err("HCI SCO stream assembler lost packet synchronization".to_string());
        }

        Ok(packets)
    }

    /// Drop any half-assembled bytes. Called on SCO link teardown so a
    /// partial packet from a previous call can't poison the framing of
    /// the first packet on the next link.
    pub fn reset(&mut self) {
        self.buffer.clear();
    }
}

impl LinearPcmTxQueue {
    pub fn new(capacity_samples: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity_samples),
            capacity_samples,
            dropped_samples: 0,
            last_emitted: 0,
            fade_in: 0,
            underruns: 0,
        }
    }

    /// The next sample to send: the queue's oldest, faded in where the queue had run dry; else a decay toward zero
    /// from the last one sent (~12.5% a sample, under 4 LSB snapping to 0: the noise floor well within one HCI
    /// packet from full scale), never a jump to it. CVSD's adaptive step cannot follow a one-sample drop from full
    /// scale to zero (its slope overload reads as static), nor a jump back up, and in mSBC both are clicks.
    fn next_sample(&mut self) -> i16 {
        match self.samples.pop_front() {
            Some(s) => {
                let s = if self.fade_in > 0 {
                    let gain = (FADE_IN_SAMPLES - self.fade_in + 1) as i32;
                    self.fade_in -= 1;
                    (s as i32 * gain / FADE_IN_SAMPLES as i32) as i16
                } else {
                    s
                };
                self.last_emitted = s;
                s
            }
            None => {
                if self.last_emitted != 0 && self.fade_in == 0 {
                    self.underruns += 1;
                }
                let next = (self.last_emitted as i32 * 7) / 8;
                self.last_emitted = if next.abs() < 4 { 0 } else { next as i16 };
                self.fade_in = FADE_IN_SAMPLES;
                self.last_emitted
            }
        }
    }

    /// How many times the queue ran dry with audio still playing (each one a short gap the caller may hear).
    pub fn underruns(&self) -> usize {
        self.underruns
    }

    pub fn push_samples(&mut self, samples: &[i16]) -> usize {
        let free = self.capacity_samples.saturating_sub(self.samples.len());
        let accepted = free.min(samples.len());
        self.samples.extend(samples[..accepted].iter().copied());
        self.dropped_samples += samples.len() - accepted;
        accepted
    }

    pub fn pop_sco_packet(
        &mut self,
        connection_handle: u16,
        payload_len: usize,
    ) -> Result<Vec<u8>, String> {
        build_linear_pcm_sco_packet(connection_handle, payload_len, |sample_count| {
            (0..sample_count).map(|_| self.next_sample()).collect()
        })
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn dropped_samples(&self) -> usize {
        self.dropped_samples
    }

    /// Drop any buffered samples without clearing the cumulative
    /// `dropped_samples` counter. Used on SCO link teardown so that
    /// half-played audio from a finished call doesn't leak into the
    /// start of the next one.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Fill `frame` with the oldest samples from the queue, decaying to
    /// silence if the queue runs dry (and fading back in after): the mSBC
    /// outbound path's. The encoder needs exactly `MSBC_SAMPLES_PER_FRAME`
    /// samples per frame and we'd rather emit silence than starve the
    /// controller's SCO FIFO. (It padded with zeros: a step, a click.)
    pub fn fill_frame(&mut self, frame: &mut [i16]) {
        for slot in frame.iter_mut() {
            *slot = self.next_sample();
        }
    }
}

pub fn parse_sco_packet(packet: &[u8]) -> Result<ScoPacket<'_>, String> {
    if packet.len() < 3 {
        return Err("HCI SCO packet too short".to_string());
    }
    let handle_status = u16::from_le_bytes([packet[0], packet[1]]);
    let len = packet[2] as usize;
    if packet.len() < 3 + len {
        return Err(format!(
            "HCI SCO packet declares {} bytes but packet has {} payload bytes",
            len,
            packet.len().saturating_sub(3)
        ));
    }
    Ok(ScoPacket {
        connection_handle: handle_status & 0x0fff,
        packet_status_flag: ((handle_status >> 12) & 0x03) as u8,
        payload: &packet[3..3 + len],
    })
}

pub fn build_sco_packet(
    connection_handle: u16,
    packet_status_flag: u8,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    if payload.len() > u8::MAX as usize {
        return Err(format!(
            "HCI SCO payload too large: {} bytes",
            payload.len()
        ));
    }
    let handle_status = (connection_handle & 0x0fff) | (((packet_status_flag as u16) & 0x03) << 12);
    let mut out = Vec::with_capacity(3 + payload.len());
    out.extend_from_slice(&handle_status.to_le_bytes());
    out.push(payload.len() as u8);
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn build_cvsd_silence_packet(
    connection_handle: u16,
    payload_len: usize,
) -> Result<Vec<u8>, String> {
    build_sco_packet(connection_handle, 0, &vec![0u8; payload_len])
}

pub fn build_linear_pcm_sco_packet<F>(
    connection_handle: u16,
    payload_len: usize,
    mut samples: F,
) -> Result<Vec<u8>, String>
where
    F: FnMut(usize) -> Vec<i16>,
{
    if payload_len % 2 != 0 {
        return Err(format!(
            "linear PCM SCO payload length must be even: {}",
            payload_len
        ));
    }

    let sample_count = payload_len / 2;
    let samples = samples(sample_count);
    if samples.len() != sample_count {
        return Err(format!(
            "linear PCM source returned {} samples, expected {}",
            samples.len(),
            sample_count
        ));
    }

    let mut payload = Vec::with_capacity(payload_len);
    for sample in samples {
        payload.extend_from_slice(&sample.to_le_bytes());
    }
    build_sco_packet(connection_handle, 0, &payload)
}

pub fn linear_pcm_samples_from_sco_packet(packet: &[u8]) -> Result<Vec<i16>, String> {
    let parsed = parse_sco_packet(packet)?;
    linear_pcm_samples_from_payload(parsed.payload)
}

pub fn linear_pcm_samples_from_payload(payload: &[u8]) -> Result<Vec<i16>, String> {
    if payload.len() % 2 != 0 {
        return Err(format!(
            "linear PCM SCO payload has odd byte length: {}",
            payload.len()
        ));
    }

    Ok(payload
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_that_lost_bytes_finds_its_packets_again() {
        let packet = |n: u8| build_sco_packet(0x002a, 0, &[n; 48]).unwrap();
        let mut assembler = ScoPacketAssembler::new();
        assembler.set_handle(Some(0x002a));
        assert_eq!(assembler.push_bytes(&packet(1)).unwrap().len(), 1);
        // an isochronous packet of the next lost: its first 17 bytes never came
        let mut stream = packet(2)[17..].to_vec();
        stream.extend(packet(3));
        stream.extend(packet(4));
        let got = assembler.push_bytes(&stream).unwrap();
        assert_eq!(
            got.iter().map(|p| p[3]).collect::<Vec<_>>(),
            [3, 4],
            "back in step from the next whole packet"
        );
        assert_eq!(assembler.resynced_bytes(), 34);
    }

    #[test]
    fn audio_that_runs_dry_decays_and_comes_back_faded_in() {
        let mut queue = LinearPcmTxQueue::new(4_800);
        queue.push_samples(&[10_000; 10]);
        let mut frame = [0i16; 24];
        queue.fill_frame(&mut frame);
        assert_eq!(&frame[..10], &[10_000; 10]);
        assert!(
            frame[10] > 0 && frame[10] < 10_000,
            "a decay, not a step to zero: {}",
            frame[10]
        );
        assert!(frame[10..].windows(2).all(|w| w[1] <= w[0]));
        assert_eq!(queue.underruns(), 1);
        // the audio comes back: from silence up, not with a jump
        queue.push_samples(&[10_000; 48]);
        queue.fill_frame(&mut frame);
        assert!(frame[0] < 1_000, "{}", frame[0]);
        assert!(frame.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(frame[23], 10_000);
        // a queue that was silent all along is no underrun
        let mut idle = LinearPcmTxQueue::new(100);
        idle.fill_frame(&mut frame);
        assert_eq!(idle.underruns(), 0);
    }

    #[test]
    fn parses_and_builds_sco_packets() {
        let packet = build_sco_packet(0x002a, 0x02, &[1, 2, 3]).unwrap();
        assert_eq!(packet, vec![0x2a, 0x20, 0x03, 1, 2, 3]);

        let parsed = parse_sco_packet(&packet).unwrap();
        assert_eq!(parsed.connection_handle, 0x002a);
        assert_eq!(parsed.packet_status_flag, 0x02);
        assert_eq!(parsed.payload, &[1, 2, 3]);
    }

    #[test]
    fn stats_records_good_and_bad_packets() {
        let mut stats = ScoStats::default();
        stats
            .record_packet(&build_sco_packet(0x000b, 0, &[0xaa, 0xbb]).unwrap())
            .unwrap();
        assert_eq!(stats.packets, 1);
        assert_eq!(stats.payload_bytes, 2);
        assert_eq!(stats.last_connection_handle, Some(0x000b));

        assert!(stats.record_packet(&[0x0b, 0x00, 0x04, 0xaa]).is_err());
        assert_eq!(stats.bad_packets, 1);
    }

    #[test]
    fn builds_cvsd_silence_packet() {
        let packet = build_cvsd_silence_packet(0x000c, 4).unwrap();
        assert_eq!(packet, vec![0x0c, 0x00, 0x04, 0, 0, 0, 0]);
    }

    #[test]
    fn builds_linear_pcm_sco_packet_from_samples() {
        let packet = build_linear_pcm_sco_packet(0x000c, 4, |_| vec![1, -1]).unwrap();
        assert_eq!(packet, vec![0x0c, 0x00, 0x04, 0x01, 0x00, 0xff, 0xff]);
        assert!(build_linear_pcm_sco_packet(0x000c, 3, |_| vec![0]).is_err());
        assert!(build_linear_pcm_sco_packet(0x000c, 4, |_| vec![0]).is_err());
    }

    #[test]
    fn fill_frame_drains_oldest_first_and_pads_with_silence() {
        let mut queue = LinearPcmTxQueue::new(8);
        queue.push_samples(&[1, 2, 3]);

        let mut frame = [99i16; 5];
        queue.fill_frame(&mut frame);
        assert_eq!(frame, [1, 2, 3, 0, 0]);
        assert!(queue.is_empty());
    }

    #[test]
    fn linear_pcm_tx_queue_packetizes_and_pads_silence() {
        let mut queue = LinearPcmTxQueue::new(3);
        assert_eq!(queue.push_samples(&[1, 2, 3, 4]), 3);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.dropped_samples(), 1);

        let packet = queue.pop_sco_packet(0x000c, 8).unwrap();
        assert_eq!(
            linear_pcm_samples_from_sco_packet(&packet).unwrap(),
            vec![1, 2, 3, 0]
        );
        assert!(queue.is_empty());

        let silence = queue.pop_sco_packet(0x000c, 4).unwrap();
        assert_eq!(
            linear_pcm_samples_from_sco_packet(&silence).unwrap(),
            vec![0, 0]
        );
    }

    #[test]
    fn extracts_linear_pcm_samples_from_sco_payloads() {
        let packet = build_sco_packet(0x000c, 0, &[0x01, 0x00, 0xff, 0xff]).unwrap();
        assert_eq!(
            linear_pcm_samples_from_sco_packet(&packet).unwrap(),
            vec![1, -1]
        );
        assert!(linear_pcm_samples_from_payload(&[1]).is_err());
    }

    #[test]
    fn linear_pcm_stats_track_sample_bounds() {
        let mut stats = LinearPcmStats::default();
        stats
            .record_sco_packet(&build_sco_packet(0x000c, 0, &[0x00, 0x80, 0xff, 0x7f]).unwrap())
            .unwrap();
        assert_eq!(stats.frames, 1);
        assert_eq!(stats.samples, 2);
        assert_eq!(stats.min_sample, Some(i16::MIN));
        assert_eq!(stats.max_sample, Some(i16::MAX));
        assert_eq!(stats.last_frame_samples, Some(2));

        assert!(stats.record_sco_packet(&[0x0c, 0x00, 0x01, 0x00]).is_err());
        assert_eq!(stats.bad_frames, 1);
    }

    #[test]
    fn assembler_rebuilds_sco_packets_from_isoch_fragments() {
        let mut assembler = ScoPacketAssembler::new();
        assert!(assembler.push_bytes(&[0x2a, 0x00]).unwrap().is_empty());
        assert!(assembler.push_bytes(&[0x03, 1]).unwrap().is_empty());
        let packets = assembler.push_bytes(&[2, 3, 0x2b, 0x00, 0x01, 4]).unwrap();
        assert_eq!(
            packets,
            vec![vec![0x2a, 0x00, 0x03, 1, 2, 3], vec![0x2b, 0x00, 0x01, 4]]
        );
    }

    #[test]
    fn fuzz_parse_sco_packet_does_not_panic_on_random_bytes() {
        // SCO packets stream off WinUSB; an out-of-sync accumulator
        // can hand the parser arbitrary bytes. Must never panic.
        use rand::rngs::StdRng;
        use rand::{RngCore, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0x5343_4f53_4f53_434f);
        for _ in 0..5_000 {
            let len = (rng.next_u32() % 256) as usize;
            let mut buf = vec![0u8; len];
            rng.fill_bytes(&mut buf);
            let _ = parse_sco_packet(&buf);
        }
    }
}
