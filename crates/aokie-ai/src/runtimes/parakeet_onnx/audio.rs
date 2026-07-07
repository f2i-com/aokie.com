//! Audio-input path for NVIDIA Parakeet-Unified-EN-0.6B (RNN-T).
//!
//! NeMo's `AudioToMelSpectrogramPreprocessor` was NOT exported into the
//! ONNX graph by `eschmidbauer/parakeet-unified-en-0.6b-onnx` —
//! `m.encoder` was exported directly, so the encoder expects already-
//! computed log-mel features with per-feature normalisation. We
//! replicate that here.
//!
//! Pinned parameters (from the model card + NeMo defaults for
//! parakeet-unified):
//!
//! ```text
//! sampling_rate:   16000 Hz
//! window:          hann, length 400 (25 ms)
//! n_fft:           512
//! hop_length:      160 (10 ms)
//! n_mels:          128
//! mel_norm:        slaney
//! mag_power:       2.0    (power spectrum, |X|²)
//! center:          True   (reflect-pad by n_fft/2 each side)
//! log:             True, log(x + 2^-24) zero-guard add
//! normalize:       per_feature (subtract mean, divide std per mel bin)
//! ```
//!
//! Encoder I/O signature:
//!   input  audio_signal:   f32 [1, 128, T]
//!   input  length:         i64 [1]   (= T)
//!   output outputs:        f32 [1, 1024, T']
//!   output encoded_lengths:i64 [1]

use ndarray::{Array2, Array3, Axis};
use rustfft::{num_complex::Complex, FftPlanner};

use crate::runtimes::candle_whisper::mel::mel_filter_bank;

pub const PARAKEET_SR: usize = 16_000;
pub const PARAKEET_WIN_LEN: usize = 400;
pub const PARAKEET_HOP_LEN: usize = 160;
pub const PARAKEET_N_FFT: usize = 512;
pub const PARAKEET_N_MELS: usize = 128;
/// NeMo's `log_zero_guard_value` default for `add` mode. `log(x + eps)`
/// keeps quiet frames numerically sane without skewing the loud ones.
pub const PARAKEET_LOG_EPS: f32 = 5.960_464_5e-8; // 2^-24

/// Compute the 128-bin log-mel spectrogram Parakeet's encoder expects,
/// with per-feature normalisation. Returns `[1, 128, T]` so it plugs
/// directly into the `audio_signal` tensor.
///
/// Two-pass: first frame the STFT and produce `(T, 128)` log-mel, then
/// normalise each mel bin (column) to zero mean / unit std across T
/// (NeMo's `normalize="per_feature"`). The transpose to `(128, T)`
/// happens at the end so the caller can hand the array straight to
/// `Tensor::from_array`.
pub fn compute_mel(samples_16k: &[f32]) -> Array3<f32> {
    let n_spec = PARAKEET_N_FFT / 2 + 1; // 257
    let filters = mel_filter_bank(PARAKEET_SR as f64, PARAKEET_N_FFT, PARAKEET_N_MELS);
    debug_assert_eq!(filters.len(), PARAKEET_N_MELS * n_spec);

    // Hann window of length win_len (matches torch.hann_window with
    // periodic=True default — `2π·i / N`, NOT `2π·i / (N-1)`).
    let hann: Vec<f32> = (0..PARAKEET_WIN_LEN)
        .map(|i| {
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (PARAKEET_WIN_LEN as f32)).cos()
        })
        .collect();

    // center=True: reflect-pad by n_fft/2 each side so the first STFT
    // frame is centered at original-sample 0. Matches torch.stft's
    // default and librosa.stft(center=True).
    let pad = PARAKEET_N_FFT / 2;
    let mut padded: Vec<f32> = Vec::with_capacity(samples_16k.len() + 2 * pad);
    // Left reflect: samples_16k[1..pad+1].reverse()
    for i in 1..=pad {
        let idx = i.min(samples_16k.len().saturating_sub(1));
        padded.push(samples_16k.get(idx).copied().unwrap_or(0.0));
    }
    padded.extend_from_slice(samples_16k);
    // Right reflect: samples_16k[len-2..len-2-pad].reverse()
    let n = samples_16k.len();
    for i in 1..=pad {
        let idx = n.saturating_sub(1 + i);
        padded.push(samples_16k.get(idx).copied().unwrap_or(0.0));
    }

    // librosa-style centered STFT frame count: 1 + len // hop. The "+1"
    // accounts for the centered-at-zero frame.
    let n_frames = if samples_16k.is_empty() {
        0
    } else {
        1 + samples_16k.len() / PARAKEET_HOP_LEN
    };

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(PARAKEET_N_FFT);

    // Output: row-major (T, n_mels) — easy to per-feature-normalise.
    let mut mel = vec![0.0f32; n_frames * PARAKEET_N_MELS];
    let mut buf = vec![Complex::<f32>::new(0.0, 0.0); PARAKEET_N_FFT];

    for f in 0..n_frames {
        // Centered windowing: frame f starts at `padded[f*hop]` and
        // spans win_len bytes. The win is centered inside an n_fft-
        // wide buffer, so we offset writes by (n_fft - win_len)/2.
        let start = f * PARAKEET_HOP_LEN;
        let win_offset = (PARAKEET_N_FFT - PARAKEET_WIN_LEN) / 2;
        for i in 0..PARAKEET_N_FFT {
            buf[i] = Complex::new(0.0, 0.0);
        }
        for i in 0..PARAKEET_WIN_LEN {
            let p = start + i;
            if p < padded.len() {
                let s = padded[p] * hann[i];
                buf[win_offset + i] = Complex::new(s, 0.0);
            }
        }

        fft.process(&mut buf);

        // Power spectrum × mel filter, then log(x + eps).
        for m in 0..PARAKEET_N_MELS {
            let row = &filters[m * n_spec..(m + 1) * n_spec];
            let mut e = 0.0f32;
            for s in 0..n_spec {
                let c = buf[s];
                let power = c.re * c.re + c.im * c.im;
                e += row[s] * power;
            }
            mel[f * PARAKEET_N_MELS + m] = (e + PARAKEET_LOG_EPS).ln();
        }
    }

    // Per-feature normalise: each mel bin (column m) → (x - mean)/std
    // computed across all T frames. NeMo's `normalize="per_feature"`
    // uses biased std (divide by N, not N-1) to match training. Single-
    // frame inputs would divide by zero — clamp std to 1e-5 like NeMo.
    let mel_arr =
        Array2::from_shape_vec((n_frames, PARAKEET_N_MELS), mel).expect("mel frame shape");
    let normalised = if n_frames == 0 {
        mel_arr
    } else {
        let mean = mel_arr.mean_axis(Axis(0)).expect("mean axis 0");
        let var = mel_arr.var_axis(Axis(0), 0.0); // 0.0 = biased (divide by N)
        let std: ndarray::Array1<f32> = var.mapv(|v| (v.max(1e-10)).sqrt().max(1e-5));
        let mut out = mel_arr;
        for mut row in out.axis_iter_mut(Axis(0)) {
            for (m, x) in row.iter_mut().enumerate() {
                *x = (*x - mean[m]) / std[m];
            }
        }
        out
    };

    // Transpose (T, n_mels) → (1, n_mels, T) for the encoder.
    let (t, n_mels) = (normalised.shape()[0], normalised.shape()[1]);
    let mut out = Array3::<f32>::zeros((1, n_mels, t));
    for f in 0..t {
        for m in 0..n_mels {
            out[[0, m, f]] = normalised[[f, m]];
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_output() {
        let mel = compute_mel(&[]);
        assert_eq!(mel.shape(), &[1, PARAKEET_N_MELS, 0]);
    }

    #[test]
    fn one_second_sine_produces_expected_frame_count() {
        // 16000 samples (1 s @ 16 kHz). With hop=160 and center=True
        // librosa-style, frame count is 1 + 16000/160 = 101.
        let samples: Vec<f32> = (0..16_000)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin())
            .collect();
        let mel = compute_mel(&samples);
        assert_eq!(mel.shape()[0], 1);
        assert_eq!(mel.shape()[1], PARAKEET_N_MELS);
        assert_eq!(mel.shape()[2], 101);
    }

    #[test]
    fn per_feature_normalisation_is_zero_mean_unit_std() {
        // Random-ish input — after per-feature norm each mel bin should
        // have mean ~0 and std ~1 across time. Sanity that the
        // normalisation step actually fires.
        let samples: Vec<f32> = (0..32_000)
            .map(|i| ((i as f32 * 0.013).sin() + (i as f32 * 0.041).cos()) * 0.3)
            .collect();
        let mel = compute_mel(&samples);
        let t = mel.shape()[2];
        for m in 0..PARAKEET_N_MELS {
            let mut sum = 0.0f32;
            let mut sq = 0.0f32;
            for f in 0..t {
                let v = mel[[0, m, f]];
                sum += v;
                sq += v * v;
            }
            let mean = sum / t as f32;
            let std = (sq / t as f32 - mean * mean).max(0.0).sqrt();
            // Mean within ε; std within ε of 1 (or 0 for constant bins).
            assert!(mean.abs() < 1e-4, "mel {m} mean = {mean}");
            assert!(
                (std - 1.0).abs() < 1e-3 || std < 1e-3,
                "mel {m} std = {std}"
            );
        }
    }
}
