use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedAudioFrame {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u32,
}

impl OwnedAudioFrame {
    pub fn is_ten_milliseconds(&self) -> bool {
        self.channels > 0
            && self.samples.len() == ((self.sample_rate / 100) * self.channels) as usize
    }
}

/// Converts arbitrarily chunked mono PCM from the Bluetooth loop into the
/// exact 10 ms frames required by libwebrtc's zero-buffer native source.
pub struct PcmPacketizer {
    sample_rate: u32,
    channels: u32,
    frame_samples: usize,
    pending: VecDeque<i16>,
    max_pending_samples: usize,
    dropped_samples: u64,
}

impl PcmPacketizer {
    pub fn new(sample_rate: u32, channels: u32, max_buffer_ms: u32) -> Option<Self> {
        if sample_rate == 0
            || !sample_rate.is_multiple_of(100)
            || channels == 0
            || max_buffer_ms < 10
        {
            return None;
        }
        let frame_samples = ((sample_rate / 100) * channels) as usize;
        let max_pending_samples = ((sample_rate * channels * max_buffer_ms) / 1000) as usize;
        Some(Self {
            sample_rate,
            channels,
            frame_samples,
            pending: VecDeque::with_capacity(max_pending_samples),
            max_pending_samples,
            dropped_samples: 0,
        })
    }

    pub fn push(&mut self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        self.pending.extend(samples.iter().copied());
        while self.pending.len() > self.max_pending_samples {
            if self.pending.pop_front().is_some() {
                self.dropped_samples = self.dropped_samples.saturating_add(1);
            }
        }
    }

    pub fn next_frame(&mut self) -> Option<OwnedAudioFrame> {
        if self.pending.len() < self.frame_samples {
            return None;
        }
        let samples = self.pending.drain(..self.frame_samples).collect();
        Some(OwnedAudioFrame {
            samples,
            sample_rate: self.sample_rate,
            channels: self.channels,
        })
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }

    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packetizer_emits_only_exact_ten_millisecond_frames() {
        let mut packetizer = PcmPacketizer::new(16_000, 1, 100).unwrap();
        packetizer.push(&vec![1; 159]);
        assert!(packetizer.next_frame().is_none());
        packetizer.push(&[2]);
        let frame = packetizer.next_frame().unwrap();
        assert!(frame.is_ten_milliseconds());
        assert_eq!(frame.samples.len(), 160);
    }

    #[test]
    fn bounded_packetizer_drops_oldest_audio() {
        let mut packetizer = PcmPacketizer::new(16_000, 1, 20).unwrap();
        packetizer.push(&vec![1; 480]);
        assert_eq!(packetizer.dropped_samples(), 160);
        assert_eq!(packetizer.next_frame().unwrap().samples, vec![1; 160]);
        assert_eq!(packetizer.next_frame().unwrap().samples, vec![1; 160]);
        assert!(packetizer.next_frame().is_none());
    }
}
