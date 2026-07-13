//! Pitch-preserving time stretch (WSOLA) for spoken-audio rate control.
//!
//! Changing a speech span's speaking RATE by resampling shifts pitch (a
//! slowed-down phone number reads as a deeper, smeared voice). WSOLA
//! (waveform-similarity overlap-add) re-times the signal by splicing
//! correlation-aligned windows instead, so a number read at 0.7× keeps the
//! same voice — just slower.
//!
//! Consumers: the plugin's per-span TTS pacing (slow the digits, keep the
//! surrounding sentence at normal rate) and aokie-voice-server's
//! OpenAI-compatible `speed` parameter. Pure DSP — no I/O, no engines —
//! which is why it lives in aokie-core.

/// Speaking-speed multiplier bounds shared by every consumer: below 0.5 the
/// splice density makes speech watery, above 2.0 it turns choppy.
pub const MIN_RATE: f32 = 0.5;
pub const MAX_RATE: f32 = 2.0;

/// Clamp a requested speaking rate into the supported band. NaN and
/// non-positive values read as 1.0 (never trust wire input with audio).
pub fn clamp_rate(rate: f32) -> f32 {
    if !rate.is_finite() || rate <= 0.0 {
        return 1.0;
    }
    rate.clamp(MIN_RATE, MAX_RATE)
}

/// Time-stretch mono i16 PCM to a new speaking rate, preserving pitch.
///
/// `rate` is the speaking-speed multiplier: 1.0 = unchanged, 0.72 = slower
/// (output ≈ input/0.72 samples), 1.3 = faster. Rates outside
/// [`MIN_RATE`]..[`MAX_RATE`] are clamped; a rate within 1% of 1.0, an empty
/// input, or an input shorter than one analysis window returns the input
/// unchanged (nothing audible to gain, and WSOLA needs a full window).
pub fn stretch_i16(input: &[i16], sample_rate: u32, rate: f32) -> Vec<i16> {
    let rate = clamp_rate(rate);
    if (rate - 1.0).abs() < 0.01 || sample_rate == 0 {
        return input.to_vec();
    }
    // 25 ms analysis window, 50% overlap, ±7.5 ms similarity search.
    let frame = ((sample_rate as usize * 25) / 1000).max(64);
    let overlap = frame / 2;
    let hop_syn = frame - overlap;
    let search = ((sample_rate as usize * 15) / 2000).max(16); // 7.5 ms
    let n = input.len();
    if n < frame + search + 1 {
        return input.to_vec();
    }
    let hop_ana = hop_syn as f32 * rate;

    let mut out: Vec<i16> = Vec::with_capacity((n as f32 / rate) as usize + frame);
    out.extend_from_slice(&input[..frame]);
    // Start of the last analysis frame copied into `out`; its natural
    // continuation (prev + hop_syn) is the similarity template each new
    // frame must line up with for a click-free splice.
    let mut prev = 0usize;
    let mut k: usize = 1;
    loop {
        let nominal = (k as f32 * hop_ana).round() as usize;
        if nominal + frame + search >= n {
            break;
        }
        let natural = prev + hop_syn; // ideal continuation of what's played
        let lo = nominal.saturating_sub(search);
        let hi = (nominal + search).min(n - frame);
        let s = best_alignment(input, natural, lo, hi, overlap);

        // Crossfade the overlap region: the tail of `out` currently holds
        // input[prev+hop_syn .. prev+frame]; blend it into input[s .. s+overlap].
        let tail = out.len() - overlap;
        for i in 0..overlap {
            let w = (i + 1) as f32 / (overlap + 1) as f32;
            let a = out[tail + i] as f32;
            let b = input[s + i] as f32;
            out[tail + i] = (a * (1.0 - w) + b * w).round().clamp(-32768.0, 32767.0) as i16;
        }
        out.extend_from_slice(&input[s + overlap..s + frame]);
        prev = s;
        k += 1;
    }
    // Natural tail: whatever follows the last copied frame plays out as-is
    // (bounded by frame+search, so it can't distort the target duration).
    let tail_start = (prev + frame).min(n);
    out.extend_from_slice(&input[tail_start..]);
    out
}

/// Position in `[lo, hi]` whose next `overlap` samples best continue the
/// signal at `template` (normalized cross-correlation). Silence (a near-zero
/// template) has nothing to align — take the nominal midpoint.
fn best_alignment(input: &[i16], template: usize, lo: usize, hi: usize, overlap: usize) -> usize {
    let t = &input[template..(template + overlap).min(input.len())];
    let t_energy: f64 = t.iter().map(|&v| (v as f64) * (v as f64)).sum();
    if t_energy < 1.0 || lo >= hi {
        return lo.midpoint(hi);
    }
    let mut best = lo;
    let mut best_score = f64::NEG_INFINITY;
    for s in lo..=hi {
        let c = &input[s..s + t.len()];
        let mut dot = 0f64;
        let mut energy = 0f64;
        for (a, b) in t.iter().zip(c.iter()) {
            dot += (*a as f64) * (*b as f64);
            energy += (*b as f64) * (*b as f64);
        }
        let score = if energy < 1.0 {
            0.0
        } else {
            dot / (t_energy.sqrt() * energy.sqrt())
        };
        if score > best_score {
            best_score = score;
            best = s;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, sample_rate: u32, secs: f32) -> Vec<i16> {
        let n = (sample_rate as f32 * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((2.0 * std::f32::consts::PI * freq * t).sin() * 12000.0) as i16
            })
            .collect()
    }

    fn zero_crossings(pcm: &[i16]) -> usize {
        pcm.windows(2)
            .filter(|w| (w[0] < 0 && w[1] >= 0) || (w[0] >= 0 && w[1] < 0))
            .count()
    }

    #[test]
    fn rate_one_and_degenerate_inputs_pass_through() {
        let s = sine(440.0, 16_000, 0.5);
        assert_eq!(stretch_i16(&s, 16_000, 1.0), s);
        assert_eq!(stretch_i16(&s, 16_000, 1.004), s, "within 1% of unity");
        assert_eq!(stretch_i16(&[], 16_000, 0.7), Vec::<i16>::new());
        let tiny = vec![100i16; 40];
        assert_eq!(stretch_i16(&tiny, 16_000, 0.7), tiny, "shorter than a window");
        assert_eq!(stretch_i16(&s, 0, 0.7), s, "zero sample rate is a no-op");
    }

    #[test]
    fn clamp_rate_bounds_and_rejects_garbage() {
        assert_eq!(clamp_rate(0.1), MIN_RATE);
        assert_eq!(clamp_rate(5.0), MAX_RATE);
        assert_eq!(clamp_rate(0.72), 0.72);
        assert_eq!(clamp_rate(f32::NAN), 1.0);
        assert_eq!(clamp_rate(-1.0), 1.0);
        assert_eq!(clamp_rate(0.0), 1.0);
    }

    /// Duration must track 1/rate at both sample rates and in both directions.
    #[test]
    fn stretched_duration_tracks_the_rate() {
        for &sr in &[8_000u32, 16_000] {
            let s = sine(300.0, sr, 1.0);
            for &rate in &[0.65f32, 0.72, 0.85, 1.25, 1.5] {
                let out = stretch_i16(&s, sr, rate);
                let expect = s.len() as f32 / rate;
                let err = (out.len() as f32 - expect).abs() / expect;
                assert!(
                    err < 0.10,
                    "rate {rate} @ {sr}Hz: got {} samples, expected ~{expect} (err {err:.3})",
                    out.len()
                );
            }
        }
    }

    /// The whole point over naive resampling: the waveform's local frequency
    /// (pitch) must survive the stretch. A 440 Hz tone must still oscillate
    /// at ~440 Hz per second of OUTPUT audio.
    #[test]
    fn pitch_is_preserved_not_shifted() {
        let sr = 16_000u32;
        let s = sine(440.0, sr, 1.0);
        for &rate in &[0.7f32, 1.4] {
            let out = stretch_i16(&s, sr, rate);
            let secs = out.len() as f32 / sr as f32;
            let zc_per_sec = zero_crossings(&out) as f32 / secs;
            // 440 Hz → ~880 crossings/sec; a resample-based "stretch" would
            // read ~880*rate instead.
            let err = (zc_per_sec - 880.0).abs() / 880.0;
            assert!(
                err < 0.08,
                "rate {rate}: {zc_per_sec:.0} crossings/sec (expected ~880, err {err:.3})"
            );
        }
    }

    /// No splice may introduce a hard discontinuity (click) grossly out of
    /// scale with the waveform's own sample-to-sample movement.
    #[test]
    fn splices_do_not_click() {
        let sr = 16_000u32;
        let s = sine(350.0, sr, 0.8);
        let out = stretch_i16(&s, sr, 0.72);
        let max_step_in = s
            .windows(2)
            .map(|w| (w[1] as i32 - w[0] as i32).abs())
            .max()
            .unwrap();
        let max_step_out = out
            .windows(2)
            .map(|w| (w[1] as i32 - w[0] as i32).abs())
            .max()
            .unwrap();
        assert!(
            max_step_out <= max_step_in * 2,
            "stretch introduced a discontinuity: in-step {max_step_in}, out-step {max_step_out}"
        );
    }

    /// Loudness must survive: the stretch rearranges audio, it must not
    /// attenuate or amplify it.
    #[test]
    fn energy_is_roughly_preserved() {
        let sr = 8_000u32;
        let s = sine(300.0, sr, 1.0);
        let rms = |p: &[i16]| {
            (p.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / p.len() as f64).sqrt()
        };
        let out = stretch_i16(&s, sr, 0.7);
        let (a, b) = (rms(&s), rms(&out));
        assert!(
            (a - b).abs() / a < 0.15,
            "rms drifted: in {a:.0}, out {b:.0}"
        );
    }
}
