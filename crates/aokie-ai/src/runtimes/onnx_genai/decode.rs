//! KV-cached decode loop for Gemma 3n / Gemma 4 ONNX.
//!
//! Graph shape (verified against `onnx-community/gemma-4-E4B-it-ONNX`):
//! - decoder has **24 layers**, **2 KV heads**
//! - head_dim is **512** on layers 5/11/17/23 (global attn) and **256** elsewhere (local attn)
//! - KV cache and logits are **FLOAT16**; embeddings are FLOAT32
//! - decoder inputs: `inputs_embeds`, `attention_mask`, `position_ids`,
//!   `num_logits_to_keep`, `per_layer_inputs`, `past_key_values.N.{key,value}`
//! - **no `use_cache_branch`** — prefill vs. step is distinguished by the
//!   `past_sequence_length` dim being 0 or >0
//! - embed_tokens outputs both `inputs_embeds` AND a `per_layer_inputs`
//!   tensor that must be piped into the decoder alongside the embeddings
//!
//! Per-layer `(num_kv_heads, head_dim)` is read from the loaded model's
//! input metadata at load time (see `OnnxGenAiRuntime::load_with_template`)
//! and threaded through here as `kv_dims`.
//!
//! ## KV cache stays on-device across steps
//!
//! The KV cache is carried between decoder runs as **owned ORT `DynValue`s**
//! (`KvCache`), NOT host ndarrays. Each step we feed the previous run's
//! `present.N.{key,value}` output values straight back as the next run's
//! `past_key_values.N.{key,value}` inputs. Under the CUDA EP those values
//! never leave the GPU, so the only host transfer per token is the single
//! logits row we need for sampling.
//!
//! The previous implementation extracted every `present.N` tensor to a host
//! `Array4<f16>` (`try_extract` + `to_vec`) and re-uploaded it (`from_array`)
//! the next step. For E4B that copied all 24 layers × 2 of a *growing* cache
//! GPU↔host every token — O(n²) PCIe traffic that dominated decode latency.

use std::collections::HashMap;

use half::f16;
use ndarray::{Array2, Array3, Array4, ArrayD, Axis, Ix3, Ix4, IxDyn};
use ort::value::{DynTensor, DynValue, Tensor};

use super::OnnxGenAiRuntime;

pub struct DecodeConfig {
    pub max_new_tokens: usize,
    pub temperature: f32,
}

/// Per-layer KV cache, kept as owned ORT `DynValue`s so it stays resident
/// on the decoder's device (GPU under the CUDA EP) across decode steps.
/// One `(key, value)` pair per layer.
type KvCache = Vec<(DynValue, DynValue)>;

/// Drive the decoder with either:
/// * prompt_ids (standard text decode — we pipe them through embed_tokens)
/// * audio_embeds (pre-computed, skip embed_tokens)
///
/// Returns the full generated text; also streams tokens through `on_token`.
pub fn decode_loop<F>(
    model: &mut OnnxGenAiRuntime,
    prompt_ids: &[u32],
    audio_embeds: Option<(Array3<f32>, Array4<f32>)>,
    cfg: DecodeConfig,
    mut on_token: F,
) -> Result<String, String>
where
    F: FnMut(&str) -> bool,
{
    // 1. Compute (inputs_embeds, per_layer_inputs) for the prompt.
    let (inputs_embeds, per_layer_inputs) = if let Some(pre) = audio_embeds {
        pre
    } else {
        run_embed_tokens(model, prompt_ids)?
    };

    let mut total_seq_len = inputs_embeds.shape()[1];

    // 2. Prefill: single call with the whole prompt. `attention_mask` covers
    //    the full (past + current) sequence; here past is 0, so == current.
    //    The past_key_values inputs are empty (0-length) host tensors.
    let attention_mask = Array2::<i64>::ones((1, total_seq_len));
    let position_ids = Array2::<i64>::from_shape_fn((1, total_seq_len), |(_, j)| j as i64);

    let prefill_inputs = build_decoder_inputs(
        &inputs_embeds,
        &per_layer_inputs,
        &attention_mask,
        &position_ids,
        1, // num_logits_to_keep — we only need the last-position logits
        empty_past_kv(&model.kv_dims)?,
    )?;
    let outputs = model
        .decoder
        .run(prefill_inputs)
        .map_err(|e| format!("decoder prefill: {}", e))?;
    let (logits, mut cache) = take_logits_and_cache(outputs, model.kv_dims.len())?;

    let mut generated = String::new();
    let mut next_id = sample(&last_row_f16_to_f32(&logits)?, cfg.temperature);

    for _step in 0..cfg.max_new_tokens {
        if next_id == model.end_of_turn_id || next_id == model.eos_id {
            break;
        }

        let piece = model
            .tokenizer
            .decode(&[next_id], true)
            .map_err(|e| format!("decode piece: {}", e))?;
        if !piece.is_empty() {
            generated.push_str(&piece);
            if !on_token(&piece) {
                break;
            }
        }

        // 3. Incremental step: embed only the new token, feed the
        //    device-resident cache straight back as past_key_values.
        let (step_embeds, step_per_layer) = run_embed_tokens(model, &[next_id])?;

        total_seq_len += 1;
        let attention_mask = Array2::<i64>::ones((1, total_seq_len));
        let position_ids = Array2::<i64>::from_elem((1, 1), (total_seq_len - 1) as i64);

        let step_inputs = build_decoder_inputs(
            &step_embeds,
            &step_per_layer,
            &attention_mask,
            &position_ids,
            1,
            cache_to_inputs(cache),
        )?;
        let step_outputs = model
            .decoder
            .run(step_inputs)
            .map_err(|e| format!("decoder step: {}", e))?;
        let (step_logits, step_cache) = take_logits_and_cache(step_outputs, model.kv_dims.len())?;
        cache = step_cache;

        next_id = sample(&last_row_f16_to_f32(&step_logits)?, cfg.temperature);
    }

    Ok(generated)
}

/// Build the empty (0-length) `past_key_values.N.{key,value}` inputs for the
/// prefill call: `[1, num_kv_heads_i, 0, head_dim_i]` per layer, f16. Layer
/// dims come from `OnnxGenAiRuntime::kv_dims`, introspected at load time.
fn empty_past_kv(kv_dims: &[(usize, usize)]) -> Result<Vec<(String, DynValue)>, String> {
    let mut v = Vec::with_capacity(kv_dims.len() * 2);
    for (i, &(kv_heads, head_dim)) in kv_dims.iter().enumerate() {
        let mk = || Array4::<f16>::from_shape_simple_fn((1, kv_heads, 0, head_dim), || f16::ZERO);
        v.push((
            format!("past_key_values.{}.key", i),
            Tensor::from_array(mk())
                .map_err(|e| format!("empty kv key {}: {}", i, e))?
                .into_dyn(),
        ));
        v.push((
            format!("past_key_values.{}.value", i),
            Tensor::from_array(mk())
                .map_err(|e| format!("empty kv val {}: {}", i, e))?
                .into_dyn(),
        ));
    }
    Ok(v)
}

/// Move the device-resident cache into `past_key_values.N.{key,value}` input
/// pairs for the next decoder run. The `DynValue`s are passed by value (no
/// host copy); under CUDA they're already on-device for the same session.
fn cache_to_inputs(cache: KvCache) -> Vec<(String, DynValue)> {
    let mut v = Vec::with_capacity(cache.len() * 2);
    for (i, (k, val)) in cache.into_iter().enumerate() {
        v.push((format!("past_key_values.{}.key", i), k));
        v.push((format!("past_key_values.{}.value", i), val));
    }
    v
}

/// Run `embed_tokens.onnx` over a list of token ids. Returns both outputs:
/// `(inputs_embeds [1, seq, 2560], per_layer_inputs [1, seq, 42, 256])`.
pub(crate) fn run_embed_tokens(
    model: &mut OnnxGenAiRuntime,
    ids: &[u32],
) -> Result<(Array3<f32>, Array4<f32>), String> {
    let seq = ids.len();
    let ids_i64: Vec<i64> = ids.iter().map(|&x| x as i64).collect();
    let input_ids =
        Array2::from_shape_vec((1, seq), ids_i64).map_err(|e| format!("input_ids shape: {}", e))?;

    let mut em_inputs: HashMap<String, DynTensor> = HashMap::new();
    em_inputs.insert(
        "input_ids".into(),
        Tensor::from_array(input_ids)
            .map_err(|e| format!("wrap ids: {}", e))?
            .upcast(),
    );

    let outputs = model
        .embed_tokens
        .run(em_inputs)
        .map_err(|e| format!("embed_tokens run: {}", e))?;

    let mut embeds: Option<Array3<f32>> = None;
    let mut per_layer: Option<Array4<f32>> = None;
    for (name, val) in outputs {
        let (shape, data) = val
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("extract {}: {}", name, e))?;
        let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
        let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
            .map_err(|e| format!("reshape {}: {}", name, e))?;
        match name.as_ref() {
            "inputs_embeds" => {
                embeds = Some(
                    arr.into_dimensionality::<Ix3>()
                        .map_err(|e| format!("embeds dim: {}", e))?,
                );
            }
            "per_layer_inputs" => {
                per_layer = Some(
                    arr.into_dimensionality::<Ix4>()
                        .map_err(|e| format!("per_layer dim: {}", e))?,
                );
            }
            _ => {}
        }
    }
    Ok((
        embeds.ok_or_else(|| "embed_tokens missing inputs_embeds".to_string())?,
        per_layer.ok_or_else(|| "embed_tokens missing per_layer_inputs".to_string())?,
    ))
}

/// Assemble the decoder input list: the per-step host tensors (embeds,
/// per_layer, mask, positions, num_logits_to_keep) plus the
/// `past_key_values` entries (`past_kv`, moved in — either the empty
/// prefill tensors or the device-resident cache).
fn build_decoder_inputs(
    inputs_embeds: &Array3<f32>,
    per_layer_inputs: &Array4<f32>,
    attention_mask: &Array2<i64>,
    position_ids: &Array2<i64>,
    num_logits_to_keep: i64,
    past_kv: Vec<(String, DynValue)>,
) -> Result<Vec<(String, DynValue)>, String> {
    let mut m: Vec<(String, DynValue)> = Vec::with_capacity(5 + past_kv.len());

    m.push((
        "inputs_embeds".into(),
        Tensor::from_array(inputs_embeds.clone())
            .map_err(|e| format!("wrap embeds: {}", e))?
            .into_dyn(),
    ));
    m.push((
        "per_layer_inputs".into(),
        Tensor::from_array(per_layer_inputs.clone())
            .map_err(|e| format!("wrap per_layer: {}", e))?
            .into_dyn(),
    ));
    m.push((
        "attention_mask".into(),
        Tensor::from_array(attention_mask.clone())
            .map_err(|e| format!("wrap mask: {}", e))?
            .into_dyn(),
    ));
    m.push((
        "position_ids".into(),
        Tensor::from_array(position_ids.clone())
            .map_err(|e| format!("wrap positions: {}", e))?
            .into_dyn(),
    ));
    // num_logits_to_keep is a 0-D scalar i64.
    let nlk = Array2::<i64>::from_elem((1, 1), num_logits_to_keep)
        .into_shape_with_order(IxDyn(&[]))
        .map_err(|e| format!("nlk reshape: {}", e))?;
    m.push((
        "num_logits_to_keep".into(),
        Tensor::from_array(nlk)
            .map_err(|e| format!("wrap nlk: {}", e))?
            .into_dyn(),
    ));

    m.extend(past_kv);
    Ok(m)
}

/// Pull the `logits` row to host (needed for CPU sampling) and keep every
/// `present.N.{key,value}` output as an owned `DynValue` for the next step's
/// cache — NOT extracted to host. Returns `(logits, cache)`.
fn take_logits_and_cache(
    outputs: ort::session::SessionOutputs,
    n_layers: usize,
) -> Result<(Array3<f16>, KvCache), String> {
    let mut logits: Option<Array3<f16>> = None;
    let mut keys: Vec<Option<DynValue>> = (0..n_layers).map(|_| None).collect();
    let mut vals: Vec<Option<DynValue>> = (0..n_layers).map(|_| None).collect();

    for (name, val) in outputs {
        if name == "logits" {
            let (shape, data) = val
                .try_extract_tensor::<f16>()
                .map_err(|e| format!("extract logits: {}", e))?;
            let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
            let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
                .map_err(|e| format!("reshape logits: {}", e))?;
            logits = Some(
                arr.into_dimensionality::<Ix3>()
                    .map_err(|e| format!("logits dim: {}", e))?,
            );
        } else if let Some(rest) = name.strip_prefix("present.") {
            let parts: Vec<&str> = rest.splitn(2, '.').collect();
            if parts.len() != 2 {
                continue;
            }
            let Ok(idx) = parts[0].parse::<usize>() else {
                continue;
            };
            if idx >= n_layers {
                continue;
            }
            match parts[1] {
                "key" => keys[idx] = Some(val),
                "value" => vals[idx] = Some(val),
                _ => {}
            }
        }
    }

    let logits = logits.ok_or_else(|| "decoder output missing `logits`".to_string())?;
    let cache: KvCache = (0..n_layers)
        .map(|i| {
            let k = keys[i]
                .take()
                .ok_or_else(|| format!("missing present.{}.key", i))?;
            let v = vals[i]
                .take()
                .ok_or_else(|| format!("missing present.{}.value", i))?;
            Ok((k, v))
        })
        .collect::<Result<_, String>>()?;

    Ok((logits, cache))
}

/// Take the last-position logits row from an f16 `[1, seq, vocab]` tensor,
/// converted to f32 for sampling.
fn last_row_f16_to_f32(logits: &Array3<f16>) -> Result<Vec<f32>, String> {
    let (_b, seq_len, _v) = logits.dim();
    if seq_len == 0 {
        return Err("logits seq_len == 0".to_string());
    }
    Ok(logits
        .index_axis(Axis(1), seq_len - 1)
        .iter()
        .map(|x| x.to_f32())
        .collect())
}

fn sample(logits: &[f32], temperature: f32) -> u32 {
    if temperature <= 0.0 {
        return argmax(logits);
    }
    let inv_t = 1.0 / temperature;
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let mut probs: Vec<f32> = logits.iter().map(|&x| ((x - max) * inv_t).exp()).collect();
    let sum: f32 = probs.iter().sum();
    if sum > 0.0 {
        for p in probs.iter_mut() {
            *p /= sum;
        }
    } else {
        return argmax(logits);
    }
    let r: f32 = rand::random::<f32>();
    let mut acc = 0.0;
    for (i, &p) in probs.iter().enumerate() {
        acc += p;
        if r <= acc {
            return i as u32;
        }
    }
    (probs.len() - 1) as u32
}

fn argmax(logits: &[f32]) -> u32 {
    let mut best = 0usize;
    let mut best_v = f32::NEG_INFINITY;
    let mut found_finite = false;
    for (i, &v) in logits.iter().enumerate() {
        if v.is_finite() && v > best_v {
            best_v = v;
            best = i;
            found_finite = true;
        }
    }
    if !found_finite {
        // All logits are NaN/inf. Pre-fix this returned 0 silently —
        // which the decode loop would then emit as a token (typically
        // <pad> or <bos>) and keep going, producing gibberish. Log
        // loudly so this surfaces in real runs; the caller still gets
        // index 0 so callers without error handling don't panic.
        eprintln!(
            "[Gemma4] argmax: no finite logit found across {} positions — model likely diverged (NaN/inf logits)",
            logits.len()
        );
    }
    best as u32
}
