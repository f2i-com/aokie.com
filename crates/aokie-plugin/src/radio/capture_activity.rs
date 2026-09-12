//! Call-local speech gate and pre-roll. Constant line noise must not keep a
//! caller turn open forever; quiet word onsets must survive the energy gate.
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct CaptureActivity {
    vad: Option<aokie_audio::vad::StreamingVad>,
    vad_checked: bool,
    levels: VecDeque<f32>,
    noise: f32,
    frames: usize,
    speech_frames: usize,
    peak: f32,
    pre_roll: VecDeque<f32>,
}

impl CaptureActivity {
    pub fn detect(&mut self, samples: &[i16], samples_16k: &[f32], continuing: bool) -> bool {
        if !self.vad_checked {
            self.vad_checked = true;
            let path = aokie_core::paths::app_data_dir().map(|dir| dir.join("models/vad/silero_vad.onnx"));
            if let Some(path) = path.filter(|path| path.is_file()) {
                let config = aokie_audio::vad::VadConfig {
                    model_path: path.to_string_lossy().into_owned(),
                    min_silence_duration: 0.064,
                    min_speech_duration: 0.064,
                    max_speech_duration: 15.0,
                    threshold: 0.4,
                    ..Default::default()
                };
                match aokie_audio::vad::StreamingVad::new(config) {
                    Ok(vad) => { self.vad = Some(vad); eprintln!("[aokie-plugin] capture detector: local Silero VAD, Parakeet transcription, 200ms pre-roll"); }
                    Err(error) => eprintln!("[aokie-plugin] Silero unavailable; using adaptive energy detector: {error}"),
                }
            } else {
                eprintln!("[aokie-plugin] Silero model absent; using adaptive energy detector");
            }
        }
        // Keep content-free level telemetry even when the neural detector owns
        // the decision, so real hardware problems can be diagnosed without WAVs.
        let energy = self.is_speech(samples, continuing);
        if let Some(vad) = self.vad.as_mut() {
            vad.accept_f32_samples(samples_16k);
            let speech = vad.is_speech();
            while vad.pop_segment().is_some() {}
            return speech;
        }
        energy
    }

    fn is_speech(&mut self, samples: &[i16], continuing: bool) -> bool {
        if samples.is_empty() { return false; }
        // DC offsets in a telephone stream are not speech. Measure the
        // alternating component without altering the PCM sent to Parakeet.
        let mean = samples.iter().map(|&s| s as f64).sum::<f64>() / samples.len() as f64;
        let rms = (samples.iter().map(|&s| (s as f64 - mean).powi(2)).sum::<f64>() / samples.len() as f64).sqrt() as f32;
        self.levels.push_back(rms);
        if self.levels.len() > 200 { self.levels.pop_front(); }
        self.frames += 1;
        self.peak = self.peak.max(rms);
        // Lower-percentile energy follows the line's background, not its
        // louder syllables. Recompute only every 100 ms (AEC frames are 10 ms).
        if self.frames % 10 == 0 && self.levels.len() >= 20 {
            let mut sorted: Vec<f32> = self.levels.iter().copied().collect();
            sorted.sort_by(f32::total_cmp);
            self.noise = sorted[sorted.len() / 5];
        }
        let threshold = (self.noise * if continuing { 1.5 } else { 1.8 } + 35.0).max(120.0);
        let speech = rms > threshold;
        if speech { self.speech_frames += 1; }
        if self.frames % 200 == 0 {
            eprintln!("[aokie-plugin] capture activity: speech={}/200 noise={:.0} threshold={threshold:.0} peak={:.0}", self.speech_frames, self.noise, self.peak);
            self.speech_frames = 0;
            self.peak = 0.0;
        }
        speech
    }

    pub fn remember_quiet(&mut self, samples_16k: &[f32]) {
        self.pre_roll.extend(samples_16k);
        while self.pre_roll.len() > 3200 { self.pre_roll.pop_front(); }
    }

    pub fn prepend_onset(&mut self, utterance: &mut Vec<f32>) {
        utterance.extend(self.pre_roll.drain(..));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(amplitude: f32) -> Vec<i16> {
        (0..80).map(|i| ((i as f32 * 0.71).sin() * amplitude) as i16).collect()
    }
    #[test]
    fn continuous_line_noise_releases_the_floor_after_a_reply() {
        let mut gate = CaptureActivity::default();
        for _ in 0..200 { gate.is_speech(&frame(600.0), false); }
        let mut silence = 0;
        for i in 0..100 {
            let signal = if i < 30 { 2200.0 } else { 600.0 };
            if gate.is_speech(&frame(signal), true) { silence = 0; } else { silence += 10; }
        }
        assert!(silence >= 450, "background above the old 350 RMS gate must still endpoint");
        assert!(gate.is_speech(&frame(2200.0), false), "next turn must still be heard");
    }
    #[test]
    fn digital_offset_is_not_speech_and_quiet_voice_can_start_a_turn() {
        let mut gate = CaptureActivity::default();
        for _ in 0..200 { assert!(!gate.is_speech(&[1800; 80], false)); }
        assert!(gate.is_speech(&frame(250.0), false), "quiet hello below the old fixed threshold");
    }
    #[test]
    fn pre_roll_preserves_soft_word_onsets_once_and_stays_bounded() {
        let mut gate = CaptureActivity::default();
        gate.remember_quiet(&vec![0.01; 5000]);
        let mut utterance = Vec::new();
        gate.prepend_onset(&mut utterance);
        assert_eq!(utterance.len(), 3200);
        gate.prepend_onset(&mut utterance);
        assert_eq!(utterance.len(), 3200, "no replay of a previous onset");
    }
}
