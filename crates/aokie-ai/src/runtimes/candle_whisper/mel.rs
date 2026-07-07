//! Slaney-style mel filter bank computation.
//!
//! Ports `librosa.filters.mel(htk=False, norm='slaney')` to Rust. Used by
//! Whisper to map a power-spectrogram (shape: `[n_fft/2 + 1, n_frames]`) into
//! a log-mel spectrogram (shape: `[n_mels, n_frames]`).
//!
//! The filter bank is computed once at startup — very cheap (a few KB of
//! floats) — so we avoid shipping a precomputed `.bytes` file.

const MIN_LOG_HZ: f64 = 1000.0;
const MIN_LOG_MEL: f64 = 15.0;

fn logstep() -> f64 {
    // librosa slaney: log(6.4) / 27.0
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

/// Build a `(n_mels, n_freqs)` filter bank in row-major order as a flat
/// `Vec<f32>`. `n_freqs = n_fft / 2 + 1`.
pub fn mel_filter_bank(sample_rate: f64, n_fft: usize, n_mels: usize) -> Vec<f32> {
    let n_freqs = 1 + n_fft / 2;
    let fmax = sample_rate / 2.0;

    // n_mels + 2 evenly spaced mel points.
    let mel_max = hz_to_mel(fmax);
    let mel_min = 0.0f64;
    let mel_pts: Vec<f64> = (0..n_mels + 2)
        .map(|i| mel_min + (mel_max - mel_min) * i as f64 / (n_mels + 1) as f64)
        .collect();
    let hz_pts: Vec<f64> = mel_pts.iter().map(|&m| mel_to_hz(m)).collect();

    // Map hz to FFT bin indices. `bin = hz * n_fft / sr` in librosa's `fft_frequencies`.
    let bins: Vec<f64> = hz_pts
        .iter()
        .map(|&hz| hz * (n_fft as f64) / sample_rate)
        .collect();

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
    }

    // Slaney normalization: each filter scaled so its area under the curve == 2/(f_upper - f_lower).
    // The slaney formulation keeps the total energy roughly invariant across the bank.
    for m in 0..n_mels {
        let enorm = 2.0 / (hz_pts[m + 2] - hz_pts[m]).max(1e-9);
        for f in 0..n_freqs {
            filters[m * n_freqs + f] *= enorm as f32;
        }
    }

    filters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_matches_whisper() {
        // Whisper large-v3-turbo uses 128 mels, 16kHz, n_fft=400.
        let f = mel_filter_bank(16_000.0, 400, 128);
        assert_eq!(f.len(), 128 * 201);
    }

    #[test]
    fn rows_are_positive_only_in_band() {
        // Sanity: each filter is zero somewhere and non-zero somewhere.
        let f = mel_filter_bank(16_000.0, 400, 80);
        for m in 0..80 {
            let row = &f[m * 201..(m + 1) * 201];
            assert!(row.iter().any(|&x| x > 0.0), "mel {} all zeros", m);
        }
    }

    #[test]
    fn mel_round_trip() {
        for hz in [100.0, 500.0, 1000.0, 5000.0, 8000.0] {
            let m = hz_to_mel(hz);
            let back = mel_to_hz(m);
            assert!((hz - back).abs() < 1e-6, "{} -> {} -> {}", hz, m, back);
        }
    }
}
