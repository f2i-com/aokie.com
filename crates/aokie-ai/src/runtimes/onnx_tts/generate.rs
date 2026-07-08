//! The actual synthesis pipeline. Mirrors
//! <https://huggingface.co/KevinAHM/pocket-tts-onnx/blob/main/pocket_tts_onnx.py>
//! closely enough that anyone debugging against the reference Python can
//! read this file without friction.
//!
//! Five ONNX sessions in sequence per utterance:
//!
//!   prepare_text → SentencePiece → text_conditioner → text_embeddings
//!                                                          │
//!                                 voice .safetensors ──────┼──→ flow_lm_main
//!                                                          │       (conditioning pass)
//!   ┌──── loop until EOS logit fires + frames_after_eos ───┘
//!   │    flow_lm_main(curr_latent) → conditioning, eos_logit
//!   │    flow_lm_flow × lsd_steps  → Euler-integrate noise → latent
//!   │    curr_latent := latent
//!   │    accumulate latent
//!   └──→ mimi_decoder(latent_chunk) → 24 kHz PCM  → on_chunk()
//!
//! The mimi decoder is stateful so latents *must* be fed in order; we chunk
//! them (default 15 frames ≈ 1.2 s) and invoke `on_chunk` per decode so the
//! frontend hears audio before the whole utterance is finished.

use std::collections::HashMap;
use std::time::Instant;

use ndarray::{Array2, Array3, ArrayD, Axis, Ix3, IxDyn};
use ort::session::Session;
use ort::value::{DynTensor, Tensor};
use rand::distributions::Distribution;

use super::state::{state_inputs, update_state_from_outputs};
use super::voice::build_voice_state;
use super::{CachedVoiceState, OnnxTtsRuntime, StateBuffers, StateSlot, StreamStats};

/// Euler sub-steps per latent frame. Python ships with 1, i.e. the flow
/// field is sampled once at s=0→t=1 per frame and applied in a single
/// Euler update. Bumping this higher nominally improves ODE accuracy but
/// also pushes `flow_lm_flow` into (s, t) sub-intervals it wasn't trained
/// on, which empirically produces garbled speech. Stick with 1 until we
/// have evidence otherwise.
const LSD_STEPS: usize = 1;

/// Noise temperature for flow sampling. 0.7 matches the Python default.
const TEMPERATURE: f32 = 0.7;

/// EOS threshold from pocket_tts_onnx.py (logit space).
const EOS_THRESHOLD: f32 = -4.0;

/// Decode chunk size. 15 latent frames × 1920 samples = 28800 samples at
/// 24 kHz = 1.2 s — long enough that mimi has context, short enough that
/// the first audio lands in well under a second.
const DECODE_CHUNK: usize = 15;

/// Bail-out cap for generation (~12 s of audio at 12.5 frames/s).
/// Real stop is driven by the EOS logit; this is a safety net.
const MAX_GEN_FRAMES_HARD_CAP: usize = 150;

impl OnnxTtsRuntime {
    /// Full synthesis. Emits f32 PCM chunks at `cfg.sample_rate` via
    /// `on_chunk`; returning `false` from the callback stops synthesis.
    pub fn synthesize_stream_impl<F>(
        &mut self,
        text: &str,
        voice: &str,
        mut on_chunk: F,
    ) -> Result<StreamStats, String>
    where
        F: FnMut(&[f32], u32) -> bool,
    {
        let started = Instant::now();
        let sample_rate = self.cfg.sample_rate;
        let latent_dim = self.cfg.latent_dim;
        let cond_dim = self.cfg.conditioning_dim;

        // --- 1. Prepare text + tokenize ----------------------------------
        let (prepared, frames_after_eos_guess) = prepare_text(
            text,
            self.cfg.remove_semicolons,
            self.cfg.pad_with_spaces_for_short_inputs,
        );
        let token_ids = self.tokenize(&prepared)?;
        if token_ids.is_empty() {
            return Err("tokenizer produced empty output".into());
        }
        let token_seq_len = token_ids.len();
        // One-shot diagnostic so we can sanity-check that the Viterbi
        // tokenizer produces something like what Python's SentencePiece
        // would for the same input.
        static TOKEN_LOG_ONCE: std::sync::Once = std::sync::Once::new();
        let prepared_for_log = prepared.clone();
        let ids_preview: Vec<i64> = token_ids.iter().take(16).copied().collect();
        let len_for_log = token_ids.len();
        TOKEN_LOG_ONCE.call_once(move || {
            eprintln!(
                "[pocket_tts_onnx] tokenize({:?}) → {} ids, first 16 = {:?}",
                prepared_for_log, len_for_log, ids_preview
            );
        });

        // --- 2. text_conditioner -----------------------------------------
        let token_arr = Array2::<i64>::from_shape_vec((1, token_seq_len), token_ids)
            .map_err(|e| format!("token_ids shape: {e}"))?;
        // Scope the session borrow so the SessionOutputs doesn't survive
        // across the resolve_voice_path() call that needs &self again.
        let text_embeddings = {
            let mut tc_inputs: HashMap<String, DynTensor> = HashMap::new();
            tc_inputs.insert(
                "token_ids".into(),
                Tensor::from_array(token_arr)
                    .map_err(|e| format!("wrap tokens: {e}"))?
                    .upcast(),
            );
            let tc_out = self
                .text_conditioner
                .run(tc_inputs)
                .map_err(|e| format!("text_conditioner: {e}"))?;
            first_output_f32_3d(&tc_out, cond_dim)?
        };

        // --- 3. Voice state — encode the reference wav on demand, cache
        //    by path so subsequent turns don't pay mimi_encoder twice.
        //    The cache entry tracks the file's mtime; if the user
        //    overwrites the voice file in place we rebuild rather than
        //    serving the stale state.
        let voice_path = self.resolve_voice_path(voice)?;
        let current_mtime = std::fs::metadata(&voice_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let cache_hit = self
            .voice_cache
            .get(&voice_path)
            .map(|c| c.mtime == current_mtime)
            .unwrap_or(false);
        if !cache_hit {
            let mimi_encoder = self
                .mimi_encoder
                .as_mut()
                .ok_or_else(|| "mimi_encoder not loaded".to_string())?;
            let state = build_voice_state(
                &self.bundle_dir,
                &self.cfg,
                mimi_encoder,
                &mut self.flow_lm_main,
                &voice_path,
            )?;
            self.voice_cache.insert(
                voice_path.clone(),
                CachedVoiceState {
                    state,
                    mtime: current_mtime,
                },
            );
        }
        let mut flow_state = clone_state(
            &self
                .voice_cache
                .get(&voice_path)
                .expect("just inserted above")
                .state,
        );

        // --- 4. flow_lm_main conditioning pass with the text embeddings --
        // Sequence is empty here ((1,0,latent_dim)); the model consumes the
        // text and writes into its KV cache. We don't care about the two
        // non-state outputs from this pass, only the updated state.
        let empty_seq = Array3::<f32>::zeros((1, 0, latent_dim));
        run_flow_main(
            &mut self.flow_lm_main,
            &empty_seq,
            &text_embeddings,
            &mut flow_state,
            &self.cfg.flow_lm_state_manifest,
        )?;

        // --- 5. Autoregressive generation loop ---------------------------
        let frame_limit = estimate_max_frames(token_seq_len).min(MAX_GEN_FRAMES_HARD_CAP);
        let frames_after_eos = self
            .cfg
            .model_recommended_frames_after_eos
            .unwrap_or(frames_after_eos_guess + 2);

        // Start from a NaN-filled "sentinel" latent — the model's training
        // relies on this to mark the very first step.
        let mut curr = Array3::<f32>::from_elem((1, 1, latent_dim), f32::NAN);
        let empty_text = Array3::<f32>::zeros((1, 0, cond_dim));
        let mut eos_step: Option<usize> = None;
        let mut all_latents: Vec<f32> = Vec::with_capacity(frame_limit * latent_dim);
        let mut n_frames = 0usize;
        let mut rng = rand::thread_rng();
        let std_dev = TEMPERATURE.sqrt();
        let normal =
            rand_distr::Normal::new(0.0f32, std_dev).map_err(|e| format!("normal distr: {e}"))?;

        // --- Streaming decode state (shared across the loop) ------------
        let mut mimi_state = super::init_state(&self.cfg.mimi_state_manifest)?;
        let mut decoded_frames = 0usize;
        let mut first_chunk_ms = 0u64;
        let mut total_samples = 0usize;
        let mut caller_stopped = false;

        for step in 0..frame_limit {
            // 5a. flow_lm_main(sequence = curr, text = empty, state = flow_state)
            let (cond, eos_logit) = run_flow_main_step(
                &mut self.flow_lm_main,
                &curr,
                &empty_text,
                &mut flow_state,
                &self.cfg.flow_lm_state_manifest,
            )?;

            // 5b. EOS: first-crossing step is noted, then we emit a few
            // extra frames (frames_after_eos) so the tail has room to fade.
            if eos_logit > EOS_THRESHOLD && eos_step.is_none() {
                eos_step = Some(step);
            }
            if let Some(es) = eos_step {
                if step >= es + frames_after_eos {
                    break;
                }
            }

            // 5c. Sample noise for this frame.
            let mut x: Vec<f32> = (0..latent_dim).map(|_| normal.sample(&mut rng)).collect();

            // 5d. Euler integration over LSD_STEPS sub-steps.
            let dt = 1.0f32 / LSD_STEPS as f32;
            for j in 0..LSD_STEPS {
                let s = j as f32 / LSD_STEPS as f32;
                let t = (j as f32 + 1.0) / LSD_STEPS as f32;
                let velocity = run_flow_step(&mut self.flow_lm_flow, &cond, s, t, &x)?;
                for k in 0..latent_dim {
                    x[k] += velocity[k] * dt;
                }
            }

            // 5e. latent becomes the next step's input sequence.
            curr = Array3::<f32>::from_shape_vec((1, 1, latent_dim), x.clone())
                .map_err(|e| format!("curr reshape: {e}"))?;
            all_latents.extend_from_slice(&x);
            n_frames += 1;

            // 5f. Opportunistically decode whenever enough latents have
            // piled up — keeps time-to-first-audio low without making the
            // decoder thrash on 1-frame chunks.
            while n_frames - decoded_frames >= DECODE_CHUNK {
                let end = (decoded_frames + DECODE_CHUNK).min(n_frames);
                let n = end - decoded_frames;
                let chunk = Array3::<f32>::from_shape_vec(
                    (1, n, latent_dim),
                    all_latents[decoded_frames * latent_dim..end * latent_dim].to_vec(),
                )
                .map_err(|e| format!("latent chunk: {e}"))?;
                let audio = run_mimi_decoder(
                    &mut self.mimi_decoder,
                    &chunk,
                    &mut mimi_state,
                    &self.cfg.mimi_state_manifest,
                )?;
                if first_chunk_ms == 0 {
                    first_chunk_ms = started.elapsed().as_millis() as u64;
                }
                total_samples += audio.len();
                if !on_chunk(&audio, sample_rate) {
                    caller_stopped = true;
                    break;
                }
                decoded_frames = end;
            }
            if caller_stopped {
                break;
            }
        }

        // 6. Final decode — flush any latents the streaming decode didn't
        //    pick up (generation loop ends well short of DECODE_CHUNK most
        //    of the time).
        if !caller_stopped && decoded_frames < n_frames {
            let n = n_frames - decoded_frames;
            let chunk = Array3::<f32>::from_shape_vec(
                (1, n, latent_dim),
                all_latents[decoded_frames * latent_dim..].to_vec(),
            )
            .map_err(|e| format!("tail chunk: {e}"))?;
            let audio = run_mimi_decoder(
                &mut self.mimi_decoder,
                &chunk,
                &mut mimi_state,
                &self.cfg.mimi_state_manifest,
            )?;
            if first_chunk_ms == 0 {
                first_chunk_ms = started.elapsed().as_millis() as u64;
            }
            total_samples += audio.len();
            let _ = on_chunk(&audio, sample_rate);
        }

        Ok(StreamStats {
            total_samples,
            first_chunk_ms,
            synth_ms: started.elapsed().as_millis() as u64,
            sample_rate,
        })
    }

    /// SentencePiece tokenize. Returns int64 token ids.
    fn tokenize(&self, text: &str) -> Result<Vec<i64>, String> {
        let ids = self
            .tokenizer
            .encode(text)
            .map_err(|e| format!("tokenize: {e}"))?;
        Ok(ids.into_iter().map(|id| id as i64).collect())
    }

    /// Resolve a voice id to a path on disk. Accepts:
    ///   * `"default"` / `""` → bundle's `reference_sample.wav`
    ///   * a preset name (`"alba"`, …) → `voices/<name>.safetensors` if
    ///     present, else `voices/<name>.wav`
    ///   * an absolute path to a wav or `.safetensors`
    ///   * a relative path under the bundle dir
    /// Unknown strings fall back to the default wav rather than erroring,
    /// so a stale config can't silence TTS.
    fn resolve_voice_path(&self, voice: &str) -> Result<std::path::PathBuf, String> {
        let v = voice.trim();
        let reference = self.bundle_dir.join("reference_sample.wav");

        if v.is_empty() || v == "default" {
            if !reference.exists() {
                return Err(format!(
                    "default voice missing: {}. Re-run Download.",
                    reference.display()
                ));
            }
            return Ok(reference);
        }

        // Preset lookup: try the gated-repo safetensors first (best
        // fidelity — these are the real Kyutai voice states), fall back
        // to the public tts-voices wav (cloned via mimi_encoder).
        let voices_dir = self.bundle_dir.join("voices");
        let safe = voices_dir.join(format!("{v}.safetensors"));
        if safe.exists() {
            eprintln!("[pocket_tts_onnx] voice '{v}' → using .safetensors (native Kyutai state)");
            return Ok(safe);
        }
        let wav = voices_dir.join(format!("{v}.wav"));
        if wav.exists() {
            eprintln!("[pocket_tts_onnx] voice '{v}' → using .wav (cloned via mimi_encoder)");
            return Ok(wav);
        }

        // Absolute / relative user-provided path.
        let p = std::path::PathBuf::from(v);
        if p.is_absolute() && p.exists() {
            return Ok(p);
        }
        let rel = self.bundle_dir.join(v);
        if rel.exists() {
            return Ok(rel);
        }

        eprintln!(
            "[pocket_tts_onnx] voice '{}' not found; falling back to default",
            voice
        );
        if !reference.exists() {
            return Err(format!(
                "voice '{}' not found and default missing at {}",
                voice,
                reference.display()
            ));
        }
        Ok(reference)
    }
}

/* --- Text preparation --------------------------------------------------- */

fn prepare_text(text: &str, remove_semicolons: bool, pad_short: bool) -> (String, usize) {
    let mut t = text
        .replace('\n', " ")
        .replace('\r', " ")
        .replace("  ", " ");
    if remove_semicolons {
        t = t.replace(';', ",");
    }
    t = t.trim().to_string();
    if t.is_empty() {
        return (t, 1);
    }

    // Capitalize the first char (ASCII — the Python code also only looks at
    // the single leading code point).
    if let Some(first) = t.chars().next() {
        if first.is_lowercase() {
            t = first.to_uppercase().chain(t.chars().skip(1)).collect();
        }
    }

    // Add terminal punctuation if missing.
    if let Some(last) = t.chars().last() {
        if last.is_alphanumeric() {
            t.push('.');
        }
    }

    let word_count = t.split_whitespace().count();
    let frames_after_eos_guess = if word_count <= 4 { 3 } else { 1 };

    if pad_short && word_count < 5 {
        t = format!("        {t}");
    }

    (t, frames_after_eos_guess)
}

/// Heuristic from the reference: ~4 frames per token is a conservative
/// upper bound. Real generation stops earlier via EOS detection.
fn estimate_max_frames(token_seq_len: usize) -> usize {
    token_seq_len * 4 + 8
}

/* --- ORT plumbing: flow_lm_main ---------------------------------------- */

/// Pure-conditioning call: sequence is empty, text_embeddings carries the
/// prompt or voice embeddings. Only the state is updated; the two non-state
/// outputs are discarded.
fn run_flow_main(
    session: &mut Session,
    sequence: &Array3<f32>,
    text_embeddings: &Array3<f32>,
    state: &mut StateBuffers,
    manifest: &[StateSlot],
) -> Result<(), String> {
    let mut inputs = state_inputs(state, manifest)?;
    inputs.insert(
        "sequence".into(),
        Tensor::from_array(sequence.clone())
            .map_err(|e| format!("wrap sequence: {e}"))?
            .upcast(),
    );
    inputs.insert(
        "text_embeddings".into(),
        Tensor::from_array(text_embeddings.clone())
            .map_err(|e| format!("wrap text_emb: {e}"))?
            .upcast(),
    );
    let outputs = session
        .run(inputs)
        .map_err(|e| format!("flow_lm_main (cond pass): {e}"))?;
    update_state_from_outputs(state, &outputs, manifest, 2)
}

/// Generation step: sequence carries the current latent, text_embeddings is
/// empty. Returns (conditioning, eos_logit); state is updated in place.
fn run_flow_main_step(
    session: &mut Session,
    sequence: &Array3<f32>,
    text_embeddings: &Array3<f32>,
    state: &mut StateBuffers,
    manifest: &[StateSlot],
) -> Result<(Array3<f32>, f32), String> {
    let mut inputs = state_inputs(state, manifest)?;
    inputs.insert(
        "sequence".into(),
        Tensor::from_array(sequence.clone())
            .map_err(|e| format!("wrap sequence: {e}"))?
            .upcast(),
    );
    inputs.insert(
        "text_embeddings".into(),
        Tensor::from_array(text_embeddings.clone())
            .map_err(|e| format!("wrap text_emb (step): {e}"))?
            .upcast(),
    );
    let outputs = session
        .run(inputs)
        .map_err(|e| format!("flow_lm_main step: {e}"))?;

    // Figure out which output is conditioning and which is the EOS logit.
    // The reference Python reads them positionally (index 0, 1) but ORT's
    // `SessionOutputs` iter ordering is not guaranteed to match the graph
    // declaration order. We identify them by shape instead: the cond
    // tensor's last dim is `cond_dim` and it has more than one element,
    // while the eos logit is a scalar (or shape like (1, 1)).
    //
    // Also one-shot log the names the first time through a process, so if
    // we need to debug further we can see them in the terminal.
    static OUTPUT_NAMES_LOGGED: std::sync::Once = std::sync::Once::new();
    OUTPUT_NAMES_LOGGED.call_once(|| {
        let names: Vec<String> = outputs.iter().map(|(name, _)| name.to_string()).collect();
        eprintln!("[pocket_tts_onnx] flow_lm_main output names: {names:?}");
    });

    let mut cond: Option<Array3<f32>> = None;
    let mut eos_logit: Option<f32> = None;
    for (name, val) in outputs.iter() {
        // Skip state outputs — they go back through update_state_from_outputs.
        if name.starts_with("out_state_") {
            continue;
        }
        let (shape, data) = val
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("extract {name}: {e}"))?;
        let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
        let numel: usize = shape.iter().product();
        let last = *shape.last().unwrap_or(&0);
        if last > 1 && cond.is_none() {
            let vec = data.to_vec();
            if vec.len() != last {
                return Err(format!(
                    "cond output '{name}' has shape {shape:?} but {} values (expected {})",
                    vec.len(),
                    last
                ));
            }
            cond = Some(
                Array3::<f32>::from_shape_vec((1, 1, last), vec)
                    .map_err(|e| format!("cond ({name}) reshape: {e} from {shape:?}"))?,
            );
        } else if numel <= 2 && eos_logit.is_none() {
            eos_logit = Some(*data.first().ok_or("empty eos logits")?);
        }
    }
    let cond = cond.ok_or("flow_lm_main: no cond output identified")?;
    let eos_logit = eos_logit.ok_or("flow_lm_main: no eos logit output identified")?;

    update_state_from_outputs(state, &outputs, manifest, 2)?;
    Ok((cond, eos_logit))
}

/* --- ORT plumbing: flow_lm_flow ---------------------------------------- */

/// One Euler sub-step. Returns the velocity (a latent_dim vector).
fn run_flow_step(
    session: &mut Session,
    cond: &Array3<f32>,
    s: f32,
    t: f32,
    x: &[f32],
) -> Result<Vec<f32>, String> {
    let x_arr = Array2::<f32>::from_shape_vec((1, x.len()), x.to_vec())
        .map_err(|e| format!("x shape: {e}"))?;
    let s_arr = Array2::<f32>::from_elem((1, 1), s);
    let t_arr = Array2::<f32>::from_elem((1, 1), t);

    // flow_lm_flow wants `c` as rank-2 (1, cond_dim). flow_lm_main outputs
    // cond as rank-3 (1, 1, cond_dim), so squeeze the middle axis.
    let cond_dim = cond.shape()[2];
    let cond_2d = Array2::<f32>::from_shape_vec((1, cond_dim), cond.iter().copied().collect())
        .map_err(|e| format!("cond squeeze: {e}"))?;

    let mut inputs: HashMap<String, DynTensor> = HashMap::new();
    inputs.insert(
        "c".into(),
        Tensor::from_array(cond_2d)
            .map_err(|e| format!("wrap c: {e}"))?
            .upcast(),
    );
    inputs.insert(
        "s".into(),
        Tensor::from_array(s_arr)
            .map_err(|e| format!("wrap s: {e}"))?
            .upcast(),
    );
    inputs.insert(
        "t".into(),
        Tensor::from_array(t_arr)
            .map_err(|e| format!("wrap t: {e}"))?
            .upcast(),
    );
    inputs.insert(
        "x".into(),
        Tensor::from_array(x_arr)
            .map_err(|e| format!("wrap x: {e}"))?
            .upcast(),
    );
    let outputs = session
        .run(inputs)
        .map_err(|e| format!("flow_lm_flow: {e}"))?;
    let first = outputs.iter().next().ok_or("flow out empty")?.1;
    let (_sh, data) = first
        .try_extract_tensor::<f32>()
        .map_err(|e| format!("extract flow: {e}"))?;
    Ok(data.to_vec())
}

/* --- ORT plumbing: mimi_decoder ---------------------------------------- */

fn run_mimi_decoder(
    session: &mut Session,
    latents: &Array3<f32>,
    state: &mut StateBuffers,
    manifest: &[StateSlot],
) -> Result<Vec<f32>, String> {
    let mut inputs = state_inputs(state, manifest)?;
    inputs.insert(
        "latent".into(),
        Tensor::from_array(latents.clone())
            .map_err(|e| format!("wrap latent: {e}"))?
            .upcast(),
    );
    let outputs = session
        .run(inputs)
        .map_err(|e| format!("mimi_decoder: {e}"))?;

    // Audio is the single non-state output; state outputs begin at offset 1.
    let audio = outputs.iter().next().ok_or("mimi no outputs")?.1;
    let (_shape, data) = audio
        .try_extract_tensor::<f32>()
        .map_err(|e| format!("extract audio: {e}"))?;
    let pcm: Vec<f32> = data.to_vec();

    update_state_from_outputs(state, &outputs, manifest, 1)?;
    Ok(pcm)
}

/* --- helpers ----------------------------------------------------------- */

/// Deep-copy a StateBuffers so the per-utterance flow_state doesn't mutate
/// the cached voice state in place.
fn clone_state(src: &StateBuffers) -> StateBuffers {
    use super::StateValue;
    let slots = src
        .slots
        .iter()
        .map(|v| match v {
            StateValue::F32 { data, shape } => StateValue::F32 {
                data: data.clone(),
                shape: shape.clone(),
            },
            StateValue::I64 { data, shape } => StateValue::I64 {
                data: data.clone(),
                shape: shape.clone(),
            },
            StateValue::Bool { data, shape } => StateValue::Bool {
                data: data.clone(),
                shape: shape.clone(),
            },
        })
        .collect();
    StateBuffers { slots }
}

fn first_output_f32_3d(
    outputs: &ort::session::SessionOutputs,
    cond_dim: usize,
) -> Result<Array3<f32>, String> {
    let first = outputs.iter().next().ok_or("empty outputs")?.1;
    let (shape, data) = first
        .try_extract_tensor::<f32>()
        .map_err(|e| format!("extract: {e}"))?;
    let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
    let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
        .map_err(|e| format!("reshape: {e}"))?;
    // text_conditioner sometimes outputs (seq_len, cond_dim) — promote to
    // (1, seq_len, cond_dim) so downstream shapes line up.
    let arr = if arr.ndim() == 2 {
        arr.insert_axis(Axis(0))
    } else {
        arr
    };
    let arr = arr
        .into_dimensionality::<Ix3>()
        .map_err(|e| format!("text_emb to 3d: {e}"))?;
    if arr.shape()[2] != cond_dim {
        return Err(format!(
            "text_emb last dim = {}, expected {cond_dim}",
            arr.shape()[2]
        ));
    }
    Ok(arr)
}
