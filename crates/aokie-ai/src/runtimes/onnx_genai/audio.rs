//! Audio-input path for Gemma 4.
//!
//! Gemma 4 accepts audio by computing a 128-bin log-mel spectrogram with
//! the parameters pinned in `processor_config.json`:
//!
//! ```text
//! sampling_rate:   16000 Hz
//! frame_length:    320  samples (20 ms window)
//! hop_length:      160  samples (10 ms hop)
//! fft_length:      512
//! feature_size:    128  mel bins
//! min_frequency:   0
//! max_frequency:   8000
//! mel_floor:       0.001
//! audio_ms_per_token: 40  (one output token per 40 ms of audio)
//! ```
//!
//! The encoder output has shape `[num_audio_tokens, 2560]`. One token
//! corresponds to 40 ms, so 1 s of audio → ~25 tokens, and the encoder's max
//! input is `audio_seq_length = 750` tokens (~30 s).

use std::collections::HashMap;

use ndarray::{Array1, Array2, Array3, ArrayD, Axis, Ix2, IxDyn};
use ort::value::{DynTensor, Tensor};
use rustfft::{num_complex::Complex, FftPlanner};

use super::OnnxGenAiRuntime;
use crate::runtimes::candle_whisper::mel::mel_filter_bank;

pub const GEMMA_SR: usize = 16_000;
/// 20 ms window at 16 kHz. Gemma's feature extractor uses a 320-sample
/// Hann window, zero-padded to `GEMMA_N_FFT=512` before the FFT. Earlier
/// versions of this module used candle's whisper helper, which hardcodes
/// window=n_fft — subtly wrong features that made Gemma hallucinate
/// responses to clearly-spoken audio. We now window ourselves.
pub const GEMMA_FRAME_LEN: usize = 320;
pub const GEMMA_HOP_LEN: usize = 160;
pub const GEMMA_N_FFT: usize = 512;
pub const GEMMA_N_MELS: usize = 128;
/// `mel_floor` from Gemma's preprocessor_config.json. The floor is
/// applied to mel energies before `ln`, so log-mel values never go below
/// `ln(0.001) ≈ -6.91`.
pub const GEMMA_MEL_FLOOR: f32 = 0.001;

/// Compute the 128-bin log-mel spectrogram Gemma 4 expects.
/// Returns `[1, num_frames, 128]` so it plugs directly into the
/// `input_features` tensor.
///
/// Implementation note: this is a hand-rolled STFT (Hann over 320 samples,
/// zero-pad to 512, rustfft, power spectrum, slaney mel filters,
/// `ln(max(energy, 0.001))`). That mirrors Gemma's feature extractor
/// exactly. The candle whisper helper we used previously matched Whisper
/// (window=n_fft=400) but differed on window size for Gemma.
pub fn compute_mel(samples_16k: &[f32]) -> Array3<f32> {
    let n_spec = GEMMA_N_FFT / 2 + 1; // 257
    let filters = mel_filter_bank(GEMMA_SR as f64, GEMMA_N_FFT, GEMMA_N_MELS);
    debug_assert_eq!(filters.len(), GEMMA_N_MELS * n_spec);

    // Hann window of length frame_len (20 ms). Matches torch.hann_window
    // and transformers' default (periodic=False, symmetric).
    let hann: Vec<f32> = (0..GEMMA_FRAME_LEN)
        .map(|i| {
            0.5 - 0.5
                * (2.0 * std::f32::consts::PI * i as f32 / (GEMMA_FRAME_LEN as f32 - 1.0)).cos()
        })
        .collect();

    // Number of STFT frames. We use "centered=False" framing (no
    // reflective padding at the start/end) — matches the HF processor's
    // default when `return_attention_mask=True`.
    let n_frames = if samples_16k.len() >= GEMMA_FRAME_LEN {
        (samples_16k.len() - GEMMA_FRAME_LEN) / GEMMA_HOP_LEN + 1
    } else {
        0
    };

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(GEMMA_N_FFT);

    // Output: [n_frames, n_mels] row-major, then promoted to [1, frames, mels].
    let mut mel = vec![0.0f32; n_frames * GEMMA_N_MELS];
    let mut buf = vec![Complex::<f32>::new(0.0, 0.0); GEMMA_N_FFT];

    for f in 0..n_frames {
        // Window + zero-pad.
        let start = f * GEMMA_HOP_LEN;
        for i in 0..GEMMA_N_FFT {
            if i < GEMMA_FRAME_LEN {
                let s = samples_16k[start + i] * hann[i];
                buf[i] = Complex::new(s, 0.0);
            } else {
                buf[i] = Complex::new(0.0, 0.0);
            }
        }

        // FFT in-place.
        fft.process(&mut buf);

        // Power spectrum, first 257 bins. Mag² = re² + im².
        // Apply mel filter bank: energy[m] = Σ filter[m, s] * power[s].
        // Then log with floor 0.001.
        for m in 0..GEMMA_N_MELS {
            let row = &filters[m * n_spec..(m + 1) * n_spec];
            let mut e = 0.0f32;
            for s in 0..n_spec {
                let c = buf[s];
                let power = c.re * c.re + c.im * c.im;
                e += row[s] * power;
            }
            mel[f * GEMMA_N_MELS + m] = e.max(GEMMA_MEL_FLOOR).ln();
        }
    }

    let arr = Array2::from_shape_vec((n_frames, GEMMA_N_MELS), mel).expect("mel shape");
    arr.insert_axis(Axis(0))
}

/// Run `audio_encoder.onnx` over a 128-mel spectrogram.
/// Returns `[num_audio_tokens, 2560]` — one 2560-dim feature per 40 ms of
/// input audio.
pub fn run_audio_encoder(
    model: &mut OnnxGenAiRuntime,
    mel: Array3<f32>,
) -> Result<Array2<f32>, String> {
    let n_frames = mel.shape()[1];
    // Mask: all-true for now (we pass the full spectrogram, no padding).
    let mask = Array2::<bool>::from_elem((1, n_frames), true);

    let mut inputs: HashMap<String, DynTensor> = HashMap::new();
    inputs.insert(
        "input_features".into(),
        Tensor::from_array(mel)
            .map_err(|e| format!("wrap mel: {}", e))?
            .upcast(),
    );
    inputs.insert(
        "input_features_mask".into(),
        Tensor::from_array(mask)
            .map_err(|e| format!("wrap mask: {}", e))?
            .upcast(),
    );

    let outputs = model
        .audio_encoder
        .run(inputs)
        .map_err(|e| format!("audio_encoder run: {}", e))?;

    let (_, val) = outputs
        .into_iter()
        .next()
        .ok_or_else(|| "audio_encoder returned no outputs".to_string())?;
    let (shape, data) = val
        .try_extract_tensor::<f32>()
        .map_err(|e| format!("extract audio_features: {}", e))?;
    let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
    let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
        .map_err(|e| format!("reshape audio_features: {}", e))?;
    arr.into_dimensionality::<Ix2>()
        .map_err(|e| format!("audio_features dim: {}", e))
}

/// Splice `audio_features` `[K, 2560]` into `inputs_embeds` `[1, L, 2560]`
/// by replacing the contiguous block of K positions that hold the
/// `<|audio|>` repeat tokens. Returns the new (embeds, per_layer_inputs)
/// pair with length L (we pre-allocated K placeholder tokens).
pub fn splice_audio(
    mut embeds: Array3<f32>,
    audio_features: &Array2<f32>,
    audio_start: usize,
    audio_len: usize,
) -> Result<Array3<f32>, String> {
    let hidden = embeds.shape()[2];
    let k = audio_features.shape()[0];
    if k != audio_len {
        return Err(format!(
            "audio feature count {} != expected {}; regenerate placeholder tokens",
            k, audio_len
        ));
    }
    if audio_features.shape()[1] != hidden {
        return Err(format!(
            "audio hidden {} != decoder hidden {}",
            audio_features.shape()[1],
            hidden
        ));
    }
    // Overwrite the audio region in-place.
    let mut region = embeds.slice_mut(ndarray::s![0, audio_start..audio_start + audio_len, ..]);
    region.assign(audio_features);
    Ok(embeds)
}

#[allow(dead_code)] // placeholder helper for future use
fn zeros_like<D: ndarray::Dimension>(shape: D) -> Array1<f32> {
    Array1::<f32>::zeros(shape.size())
}
