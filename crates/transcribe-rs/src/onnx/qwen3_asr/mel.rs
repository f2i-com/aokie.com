//! Whisper-style log-mel spectrogram for Qwen3-ASR.
//!
//! Per `andrewleech/qwen3-asr-onnx/src/mel.py` and
//! `preprocessor_config.json`:
//!
//! ```text
//! sampling_rate: 16000
//! n_fft:          400
//! hop_length:     160
//! n_mels:         128
//! window:         hann (periodic, length=n_fft)
//! mel_norm:       slaney
//! mel_scale:      slaney
//! fmin / fmax:    0 / 8000
//! center=True (reflect-pad by n_fft/2 each side), drop final frame
//! log_clamp = 1e-10
//! log_floor = log_max - 8.0
//! normalize: (log + 4) / 4
//! ```
//!
//! Identical math to our `candle_whisper::mel`, just exposed under
//! transcribe-rs's namespace so the engine doesn't reach back into
//! the consuming app.

use ndarray::{Array2, Array3};
use rustfft::{num_complex::Complex, FftPlanner};

pub const SAMPLE_RATE: usize = 16_000;
pub const N_FFT: usize = 400;
pub const HOP: usize = 160;
pub const N_MELS: usize = 128;

// --- Slaney mel filter bank ----------------------------------------
const MIN_LOG_HZ: f64 = 1000.0;
const MIN_LOG_MEL: f64 = 15.0;

fn logstep() -> f64 {
    6.4f64.ln() / 27.0
}

fn hz_to_mel(hz: f64) -> f64 {
    if hz < MIN_LOG_HZ {
        hz / (200.0 / 3.0)
    } else {
        MIN_LOG_MEL + (hz / MIN_LOG_HZ).ln() / logstep()
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    if mel < MIN_LOG_MEL {
        mel * (200.0 / 3.0)
    } else {
        MIN_LOG_HZ * ((mel - MIN_LOG_MEL) * logstep()).exp()
    }
}

fn mel_filter_bank_slaney(sr: f64, n_fft: usize, n_mels: usize) -> Vec<f32> {
    let n_freqs = 1 + n_fft / 2;
    let fmax = sr / 2.0;
    let mel_max = hz_to_mel(fmax);
    let mel_pts: Vec<f64> = (0..n_mels + 2)
        .map(|i| (mel_max * i as f64) / (n_mels + 1) as f64)
        .collect();
    let hz_pts: Vec<f64> = mel_pts.iter().map(|&m| mel_to_hz(m)).collect();
    let bins: Vec<f64> = hz_pts.iter().map(|&hz| hz * (n_fft as f64) / sr).collect();
    let mut filters = vec![0.0f32; n_mels * n_freqs];
    for m in 0..n_mels {
        let lower = bins[m];
        let center = bins[m + 1];
        let upper = bins[m + 2];
        for f in 0..n_freqs {
            let f_hz = f as f64;
            let v = if f_hz >= lower && f_hz <= center {
                (f_hz - lower) / (center - lower).max(1e-9)
            } else if f_hz >= center && f_hz <= upper {
                (upper - f_hz) / (upper - center).max(1e-9)
            } else {
                0.0
            };
            filters[m * n_freqs + f] = v as f32;
        }
        // Slaney area-norm: 2/(f_upper - f_lower)
        let enorm = 2.0 / (hz_pts[m + 2] - hz_pts[m]).max(1e-9);
        for f in 0..n_freqs {
            filters[m * n_freqs + f] *= enorm as f32;
        }
    }
    filters
}

/// Returns `[1, n_mels, T]` log-mel matching the WhisperFeatureExtractor
/// preprocessor Qwen3-ASR was exported with. Padding mode is reflect
/// (center=True), final frame dropped to match Python `stft[..., :-1]`.
pub fn compute_log_mel(samples_16k: &[f32]) -> Array3<f32> {
    let n_spec = N_FFT / 2 + 1; // 201
    let pad = N_FFT / 2; // 200
    let filters = mel_filter_bank_slaney(SAMPLE_RATE as f64, N_FFT, N_MELS);

    // Periodic Hann (cos(2π·i / N), not /(N-1)).
    let hann: Vec<f32> = (0..N_FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N_FFT as f32).cos())
        .collect();

    // Reflect pad samples[1..pad+1] reversed | samples | samples[n-2..n-2-pad] reversed
    let n = samples_16k.len();
    let mut padded: Vec<f32> = Vec::with_capacity(n + 2 * pad);
    if n == 0 {
        return Array3::<f32>::zeros((1, N_MELS, 0));
    }
    for i in 0..pad {
        let idx = if pad - i < n { pad - i } else { 0 };
        padded.push(samples_16k[idx]);
    }
    padded.extend_from_slice(samples_16k);
    for i in 0..pad {
        let idx = if n >= 2 + i { n - 2 - i } else { 0 };
        padded.push(samples_16k[idx]);
    }

    let total_frames = if padded.len() >= N_FFT {
        (padded.len() - N_FFT) / HOP + 1
    } else {
        0
    };
    let n_frames = total_frames.saturating_sub(1);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(N_FFT);
    let mut buf = vec![Complex::<f32>::new(0.0, 0.0); N_FFT];
    let mut mel = vec![0.0f32; n_frames * N_MELS];

    for f in 0..n_frames {
        let start = f * HOP;
        for i in 0..N_FFT {
            let s = padded[start + i] * hann[i];
            buf[i] = Complex::new(s, 0.0);
        }
        fft.process(&mut buf);
        for m in 0..N_MELS {
            let row = &filters[m * n_spec..(m + 1) * n_spec];
            let mut e = 0.0f32;
            for s in 0..n_spec {
                let c = buf[s];
                let power = c.re * c.re + c.im * c.im;
                e += row[s] * power;
            }
            let v = e.max(1e-10);
            mel[m * n_frames + f] = v.log10();
        }
    }

    // Whisper post-norm: max(log, log_max - 8), then (log + 4)/4.
    let log_max = mel.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let floor = if log_max.is_finite() {
        log_max - 8.0
    } else {
        -8.0
    };
    for v in mel.iter_mut() {
        if *v < floor {
            *v = floor;
        }
        *v = (*v + 4.0) / 4.0;
    }

    // Reshape from row-major [n_mels, n_frames] (in `mel`) to
    // ndarray [1, n_mels, n_frames].
    let mel_arr = Array2::from_shape_vec((N_MELS, n_frames), mel).expect("mel shape");
    let mut out = Array3::<f32>::zeros((1, N_MELS, n_frames));
    for m in 0..N_MELS {
        for f in 0..n_frames {
            out[[0, m, f]] = mel_arr[[m, f]];
        }
    }
    out
}
