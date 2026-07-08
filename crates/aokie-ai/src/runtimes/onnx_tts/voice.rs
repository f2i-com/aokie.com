//! Voice conditioning from a wav reference.
//!
//! The original pocket-tts shipped predefined voice states as
//! `.safetensors` blobs in a gated HF repo (`kyutai/pocket-tts`). Rather
//! than make users go through HF auth + license acceptance we rebuild the
//! state ourselves at load time: read a reference wav, resample to the
//! bundle's sample rate (24 kHz), feed it through `mimi_encoder.onnx` to
//! get the latent voice embeddings, prepend `bos_before_voice.npy`, then
//! run `flow_lm_main` once on (empty_sequence, voice_embeddings, init_state)
//! so its KV cache ends up exactly like a preset load would have produced.
//!
//! Cost: one extra ~200 ms `mimi_encoder.run` + one ~100 ms `flow_lm_main`
//! call per voice, cached for reuse. No external downloads, no gated repo.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ndarray::{Array3, ArrayD, Axis, Ix3, IxDyn};
use ort::session::Session;
use ort::value::{DynTensor, Tensor};

use super::state::{state_inputs, update_state_from_outputs};
use super::{init_state, BundleConfig, StateBuffers, StateSlot, StateValue};

/// Build the `flow_lm_main` conditioning state for a given voice source.
/// Accepts either a `.wav` reference clip (cloned via `mimi_encoder`, the
/// Python `_condition_with_voice_embeddings` path) or a pre-conditioned
/// `.safetensors` voice state from the gated `kyutai/pocket-tts` repo.
pub fn build_voice_state(
    bundle_dir: &Path,
    cfg: &BundleConfig,
    mimi_encoder: &mut Session,
    flow_lm_main: &mut Session,
    voice_path: &Path,
) -> Result<StateBuffers, String> {
    // Dispatch on file extension.
    let ext = voice_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "safetensors" {
        eprintln!(
            "[pocket_tts_onnx] loading preset voice state from {}",
            voice_path.display()
        );
        return load_safetensors_voice_state(voice_path, &cfg.flow_lm_state_manifest);
    }

    // Default path: clone the voice from a wav reference via mimi_encoder.
    let wav_path = voice_path;
    // 1. Load + resample the wav to bundle sample_rate, mono f32 in [-1, 1].
    let samples = load_and_resample_wav(wav_path, cfg.sample_rate)?;
    if samples.is_empty() {
        return Err(format!("reference wav {} is empty", wav_path.display()));
    }
    eprintln!(
        "[pocket_tts_onnx] encoding voice from {} ({} samples @ {} Hz)",
        wav_path.display(),
        samples.len(),
        cfg.sample_rate
    );

    // 2. mimi_encoder expects audio shape (1, 1, num_samples).
    let audio = Array3::<f32>::from_shape_vec((1, 1, samples.len()), samples)
        .map_err(|e| format!("audio shape: {e}"))?;

    let mut enc_inputs: HashMap<String, DynTensor> = HashMap::new();
    enc_inputs.insert(
        "audio".into(),
        Tensor::from_array(audio)
            .map_err(|e| format!("wrap audio: {e}"))?
            .upcast(),
    );
    let voice_emb = {
        let enc_out = mimi_encoder
            .run(enc_inputs)
            .map_err(|e| format!("mimi_encoder: {e}"))?;
        let first = enc_out.iter().next().ok_or("mimi_encoder no outputs")?.1;
        let (shape, data) = first
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("extract voice emb: {e}"))?;
        let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
        let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
            .map_err(|e| format!("voice emb reshape: {e}"))?;
        let arr = if arr.ndim() == 2 {
            arr.insert_axis(Axis(0))
        } else {
            arr
        };
        arr.into_dimensionality::<Ix3>()
            .map_err(|e| format!("voice emb to 3d: {e}"))?
    };
    eprintln!(
        "[pocket_tts_onnx] voice embeddings: {:?}",
        voice_emb.shape()
    );

    // 3. Prepend bos_before_voice.npy if the bundle asks for it.
    let conditioned = if cfg.insert_bos_before_voice {
        let bos_path = bundle_dir.join(&cfg.bos_before_voice_file);
        let bos = load_npy_f32(&bos_path)?;
        concat_voice_embeddings(&bos, &voice_emb, cfg.conditioning_dim)?
    } else {
        voice_emb
    };

    // 4. Run flow_lm_main with (empty_seq, conditioned embeddings, init state).
    //    The state outputs are what we return — that's our voice-conditioned
    //    flow state.
    let mut state = init_state(&cfg.flow_lm_state_manifest)?;
    let empty_seq = Array3::<f32>::zeros((1, 0, cfg.latent_dim));

    let mut inputs = state_inputs(&state, &cfg.flow_lm_state_manifest)?;
    inputs.insert(
        "sequence".into(),
        Tensor::from_array(empty_seq)
            .map_err(|e| format!("wrap seq: {e}"))?
            .upcast(),
    );
    inputs.insert(
        "text_embeddings".into(),
        Tensor::from_array(conditioned)
            .map_err(|e| format!("wrap voice emb: {e}"))?
            .upcast(),
    );
    let outputs = flow_lm_main
        .run(inputs)
        .map_err(|e| format!("flow_lm_main voice pass: {e}"))?;
    update_state_from_outputs(&mut state, &outputs, &cfg.flow_lm_state_manifest, 2)?;
    Ok(state)
}

/* --- helpers ----------------------------------------------------------- */

/// Read a WAV file and return mono f32 samples at `target_rate` in [-1, 1].
fn load_and_resample_wav(path: &Path, target_rate: u32) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("open wav {}: {e}", path.display()))?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let src_rate = spec.sample_rate;

    // Collect samples as f32 regardless of encoding.
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let denom = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / denom))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("read int samples: {e}"))?
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(|e| format!("read f32 samples: {e}"))?,
    };

    // Downmix to mono by averaging channels.
    let mono: Vec<f32> = if channels == 1 {
        samples
    } else {
        samples
            .chunks_exact(channels)
            .map(|c| c.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    if src_rate == target_rate {
        return Ok(mono);
    }

    // Resample to target_rate via rubato's fast sinc interpolator.
    use rubato::{FftFixedInOut, Resampler};
    let mut resampler = FftFixedInOut::<f32>::new(src_rate as usize, target_rate as usize, 1024, 1)
        .map_err(|e| format!("resampler init: {e}"))?;
    let mut input = vec![mono.clone()];
    let chunk_size = resampler.input_frames_next();
    // Ensure length is a multiple of chunk_size by zero-padding the tail.
    let pad = (chunk_size - (input[0].len() % chunk_size)) % chunk_size;
    if pad > 0 {
        input[0].extend(std::iter::repeat(0.0f32).take(pad));
    }
    let mut out =
        Vec::<f32>::with_capacity(input[0].len() * target_rate as usize / src_rate as usize + 1024);
    let mut cursor = 0;
    while cursor + chunk_size <= input[0].len() {
        let slice = vec![input[0][cursor..cursor + chunk_size].to_vec()];
        let resampled = resampler
            .process(&slice, None)
            .map_err(|e| format!("resample: {e}"))?;
        out.extend_from_slice(&resampled[0]);
        cursor += chunk_size;
    }
    Ok(out)
}

/// Parse a NumPy `.npy` file containing a float32 array and return it as an
/// Array3 of shape (1, frames, conditioning_dim) — `bos_before_voice` is a
/// 1-D or 2-D float32 buffer in practice, but we promote it to 3D so it
/// concats cleanly with `voice_emb`.
fn load_npy_f32(path: &Path) -> Result<Array3<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    // NumPy .npy layout: "\x93NUMPY" + major(u8) + minor(u8) + header_len + header + data.
    if bytes.len() < 10 || &bytes[0..6] != b"\x93NUMPY" {
        return Err(format!("{}: bad npy magic", path.display()));
    }
    let major = bytes[6];
    // npy v1 stores header_len as u16 (10 bytes preamble); v2+ as u32
    // (12 bytes preamble). Bound-check before slicing — the v1 check
    // above only guarantees 10 bytes, which would OOB on a v2 file.
    let preamble_end = if major >= 2 { 12 } else { 10 };
    if bytes.len() < preamble_end {
        return Err(format!(
            "{}: npy preamble truncated (need {} bytes, got {})",
            path.display(),
            preamble_end,
            bytes.len()
        ));
    }
    let header_len: usize = if major >= 2 {
        // Little-endian u32
        u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize
    } else {
        // Little-endian u16
        u16::from_le_bytes([bytes[8], bytes[9]]) as usize
    };
    let header_end = if major >= 2 {
        12 + header_len
    } else {
        10 + header_len
    };
    if bytes.len() < header_end {
        return Err(format!("{}: npy header truncated", path.display()));
    }
    let header_str = std::str::from_utf8(&bytes[if major >= 2 { 12 } else { 10 }..header_end])
        .map_err(|e| format!("npy header utf8: {e}"))?;

    // Crude but sufficient parsing of the Python-dict header. We only need
    // dtype + shape + fortran_order.
    if !header_str.contains("'descr': '<f4'") && !header_str.contains("\"descr\": \"<f4\"") {
        return Err(format!(
            "{}: expected float32 (<f4), header was: {}",
            path.display(),
            header_str
        ));
    }
    if header_str.contains("'fortran_order': True")
        || header_str.contains("\"fortran_order\": true")
    {
        return Err(format!(
            "{}: fortran-order arrays not supported",
            path.display()
        ));
    }
    // shape: parse the tuple after `'shape':`
    let shape_start = header_str
        .find("'shape':")
        .or_else(|| header_str.find("\"shape\":"))
        .ok_or_else(|| format!("{}: no shape in npy header", path.display()))?;
    let shape_slice = &header_str[shape_start..];
    let lparen = shape_slice
        .find('(')
        .ok_or_else(|| format!("shape paren: {}", path.display()))?;
    let rparen = shape_slice
        .find(')')
        .ok_or_else(|| format!("shape paren close: {}", path.display()))?;
    let dims_str = &shape_slice[lparen + 1..rparen];
    let dims: Vec<usize> = dims_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<usize>()
                .map_err(|e| format!("parse dim '{s}': {e}"))
        })
        .collect::<Result<_, _>>()?;

    let data_bytes = &bytes[header_end..];
    let expected = dims.iter().product::<usize>() * 4;
    if data_bytes.len() < expected {
        return Err(format!(
            "{}: expected {} data bytes, got {}",
            path.display(),
            expected,
            data_bytes.len()
        ));
    }
    let mut floats: Vec<f32> = Vec::with_capacity(expected / 4);
    for chunk in data_bytes.chunks_exact(4).take(expected / 4) {
        floats.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }

    // Promote to 3D. Accept (N,), (N, D), or (1, N, D).
    let arr =
        ArrayD::from_shape_vec(IxDyn(&dims), floats).map_err(|e| format!("npy reshape: {e}"))?;
    match arr.ndim() {
        1 => {
            let d = arr.shape()[0];
            Ok(arr
                .into_shape_with_order(IxDyn(&[1, 1, d]))
                .map_err(|e| format!("npy 1d→3d: {e}"))?
                .into_dimensionality::<Ix3>()
                .map_err(|e| format!("npy 3d dim: {e}"))?)
        }
        2 => Ok(arr
            .insert_axis(Axis(0))
            .into_dimensionality::<Ix3>()
            .map_err(|e| format!("npy 2d→3d: {e}"))?),
        3 => arr
            .into_dimensionality::<Ix3>()
            .map_err(|e| format!("npy already 3d: {e}")),
        other => Err(format!("{}: unexpected npy ndim {}", path.display(), other)),
    }
}

fn concat_voice_embeddings(
    bos: &Array3<f32>,
    voice: &Array3<f32>,
    cond_dim: usize,
) -> Result<Array3<f32>, String> {
    if bos.shape()[2] != cond_dim {
        return Err(format!(
            "bos last dim {} != cond_dim {}",
            bos.shape()[2],
            cond_dim
        ));
    }
    if voice.shape()[2] != cond_dim {
        return Err(format!(
            "voice last dim {} != cond_dim {}",
            voice.shape()[2],
            cond_dim
        ));
    }
    ndarray::concatenate(Axis(1), &[bos.view(), voice.view()])
        .map_err(|e| format!("concat bos+voice: {e}"))
}

/// Read a pre-conditioned flow_lm_main state from a `.safetensors` blob
/// shipped with `kyutai/pocket-tts`. The schema is tolerant to version
/// drift: if a manifest slot's key is missing from the safetensors (this
/// variant has `cache`/`offset` per layer where the bundle expects
/// `cache`/`current_end`/`step`), we fall back to the manifest's default
/// fill value. Known alias: `step` slots in the manifest map to the
/// safetensors' `offset` tensor, since that's what the newer export
/// renamed it to.
fn load_safetensors_voice_state(
    path: &Path,
    manifest: &[StateSlot],
) -> Result<StateBuffers, String> {
    use safetensors::SafeTensors;

    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let st =
        SafeTensors::deserialize(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let names: Vec<String> = st.names().into_iter().map(|s| s.to_string()).collect();

    // Build candidate key lists per slot: the canonical path, the
    // dot-separated variant, and any known aliases.
    let candidates = |slot: &StateSlot| -> Vec<String> {
        let path = slot.path.clone();
        let dot = slot.path.replace('/', ".");
        let mut list = vec![path.clone(), dot];
        // Schema migration: newer exports call `step` → `offset`, and
        // drop the 0-element `current_end` placeholder entirely.
        if slot.key == "step" {
            list.push(path.replace("/step", "/offset"));
        }
        list
    };

    let mut slots = Vec::with_capacity(manifest.len());
    let mut matched = 0usize;
    let mut missing: Vec<&str> = Vec::new();
    for slot in manifest {
        let mut found: Option<safetensors::tensor::TensorView> = None;
        for key in candidates(slot) {
            if let Ok(t) = st.tensor(&key) {
                found = Some(t);
                break;
            }
        }
        let value = if let Some(t) = found {
            matched += 1;
            let loaded_shape: Vec<usize> = t.shape().to_vec();
            let expected_shape = &slot.shape;
            let raw = t.data();
            match slot.dtype.as_str() {
                "float32" => {
                    let loaded: Vec<f32> = raw
                        .chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    // The voice safetensors' cache is sized to the actual
                    // voice-conditioning sequence length (e.g. 126 frames);
                    // the ONNX graph wants the full 1000-slot buffer.
                    // Pad along any axis where the model expects more than
                    // the file provides.
                    match pad_f32_to_shape(&loaded, &loaded_shape, expected_shape, f32::NAN) {
                        Ok(data) => StateValue::F32 {
                            data,
                            shape: expected_shape.clone(),
                        },
                        Err(e) => {
                            eprintln!(
                                "[pocket_tts_onnx] {}: shape adapt failed ({e}) — using default fill",
                                slot.path
                            );
                            StateValue::from_manifest(slot)?
                        }
                    }
                }
                "int64" => {
                    let loaded: Vec<i64> = raw
                        .chunks_exact(8)
                        .map(|c| {
                            i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                        })
                        .collect();
                    if loaded_shape == *expected_shape {
                        StateValue::I64 {
                            data: loaded,
                            shape: expected_shape.clone(),
                        }
                    } else if loaded.len() == expected_shape.iter().product::<usize>() {
                        StateValue::I64 {
                            data: loaded,
                            shape: expected_shape.clone(),
                        }
                    } else {
                        // Most i64 slots are tiny (shape [1]) — if shapes
                        // diverge there's no meaningful pad we can do.
                        eprintln!(
                            "[pocket_tts_onnx] {}: i64 shape {:?} != expected {:?} — using default",
                            slot.path, loaded_shape, expected_shape
                        );
                        StateValue::from_manifest(slot)?
                    }
                }
                "bool" => {
                    let loaded: Vec<bool> = raw.iter().map(|&b| b != 0).collect();
                    if loaded_shape == *expected_shape {
                        StateValue::Bool {
                            data: loaded,
                            shape: expected_shape.clone(),
                        }
                    } else {
                        StateValue::from_manifest(slot)?
                    }
                }
                _ => StateValue::from_manifest(slot)?,
            }
        } else {
            missing.push(&slot.path);
            StateValue::from_manifest(slot)?
        };
        slots.push(value);
    }
    eprintln!(
        "[pocket_tts_onnx] voice .safetensors: {} / {} slots filled from file ({} defaulted), {} keys in file. Sample available: {:?}",
        matched,
        manifest.len(),
        manifest.len() - matched,
        names.len(),
        &names.iter().take(3).collect::<Vec<_>>()
    );
    if matched == 0 {
        return Err(format!(
            "voice file at {} has no keys matching our manifest — schema mismatch",
            path.display()
        ));
    }
    let _ = missing;
    Ok(StateBuffers { slots })
}

/// Rearrange a row-major (C-order) `loaded` tensor of shape `from` into a
/// larger row-major buffer of shape `to`, padding everywhere `to[i] >
/// from[i]` with `pad_val`. Requires that all loaded dims are ≤ expected
/// dims — the loaded region is copied into the `[0..from[i]]` slice of
/// each axis. This is the shape we need for autoregressive KV caches
/// whose first `N` slots are the voice-prompt sequence and the rest is
/// runtime workspace.
fn pad_f32_to_shape(
    loaded: &[f32],
    from: &[usize],
    to: &[usize],
    pad_val: f32,
) -> Result<Vec<f32>, String> {
    if from.len() != to.len() {
        return Err(format!(
            "rank mismatch: loaded {:?}, expected {:?}",
            from, to
        ));
    }
    // Identity case — just return a copy.
    if from == to {
        if loaded.len() != from.iter().product::<usize>() {
            return Err(format!(
                "element count {} doesn't match shape {:?}",
                loaded.len(),
                from
            ));
        }
        return Ok(loaded.to_vec());
    }
    for (a, b) in from.iter().zip(to.iter()) {
        if a > b {
            return Err(format!(
                "loaded dim {} exceeds expected {} ({:?} vs {:?})",
                a, b, from, to
            ));
        }
    }
    let expected_count = loaded.len();
    if expected_count != from.iter().product::<usize>() {
        return Err(format!(
            "loaded length {} != product of from {:?}",
            expected_count, from
        ));
    }

    let total: usize = to.iter().product();
    let mut out = vec![pad_val; total];

    // Compute strides for both shapes. Row-major: stride of dim i is the
    // product of all later dims' sizes in that shape.
    let rank = from.len();
    let mut from_strides = vec![1usize; rank];
    let mut to_strides = vec![1usize; rank];
    for i in (0..rank.saturating_sub(1)).rev() {
        from_strides[i] = from_strides[i + 1] * from[i + 1];
        to_strides[i] = to_strides[i + 1] * to[i + 1];
    }

    // Walk the loaded tensor as a multi-index iterator and copy each
    // element into the expanded buffer. Rank is small (≤5 for our cache
    // tensors) so the constant overhead is negligible.
    let mut idx = vec![0usize; rank];
    'outer: loop {
        // Compute flat indices.
        let mut src_flat = 0;
        let mut dst_flat = 0;
        for i in 0..rank {
            src_flat += idx[i] * from_strides[i];
            dst_flat += idx[i] * to_strides[i];
        }
        out[dst_flat] = loaded[src_flat];

        // Increment the multi-index in row-major order.
        for i in (0..rank).rev() {
            idx[i] += 1;
            if idx[i] < from[i] {
                continue 'outer;
            }
            idx[i] = 0;
        }
        break;
    }
    Ok(out)
}

/// Suppress unused-import warnings in older code paths.
#[allow(dead_code)]
pub fn dummy(_: &[StateSlot], _: PathBuf) {}
