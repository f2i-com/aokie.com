//! NVIDIA Parakeet-Unified-EN-0.6B (FastConformer + RNN-T) runtime.
//!
//! Two ORT sessions:
//!   - `encoder.onnx` consumes a `[1, 128, T]` log-mel spectrogram and
//!     produces `[1, 1024, T']` encoder embeddings.
//!   - `decoder_joint.onnx` runs the RNN-T predictor + joint network as
//!     a single graph; we drive it in a greedy loop with up to 10 inner
//!     iterations per encoder frame, breaking on the blank token.
//!
//! The model card publishes the I/O signature; we mirror it exactly.
//! See `audio.rs` for the mel front-end and `tokenizer.rs` for the
//! SentencePiece detokenizer.
//!
//! Why an in-house adapter rather than sherpa-onnx: sherpa-rs's
//! `TransducerRecognizer` expects three separate ONNX files (encoder /
//! decoder / joiner) plus `tokens.txt`, but the eschmidbauer export of
//! parakeet-unified merges decoder + joiner into one ONNX and ships a
//! SentencePiece `tokenizer.model`. Going through ORT directly is a few
//! hundred lines and lets us skip a sherpa-side rewrite.

pub mod audio;
pub mod tokenizer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ndarray::{Array1, Array2, Array3, ArrayD, Ix3, IxDyn};
use ort::execution_providers::CPUExecutionProvider;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::{DynTensor, Tensor};
use tokio::sync::Mutex;

use self::audio::compute_mel;
use self::tokenizer::Detokenizer;

/// Blank token id for Parakeet RNN-T (vocab_size = 1024 SentencePiece
/// pieces; blank sits at index 1024 per the model card).
const BLANK_ID: u32 = 1024;
/// Max greedy predictions per encoder frame before forcing the loop to
/// advance. Matches the model card's reference Python; without this an
/// "all-non-blank" pathology could spin indefinitely on garbage audio.
const MAX_SYMBOLS_PER_FRAME: usize = 10;
/// Predictor LSTM hidden size (per the I/O signature).
const PRED_HIDDEN: usize = 640;
const PRED_LAYERS: usize = 2;

/// Loaded Parakeet runtime. The two `Session`s, the detokenizer, and
/// the paths we loaded from (kept for diagnostic logs).
pub struct ParakeetOnnxRuntime {
    encoder: Session,
    decoder_joint: Session,
    detok: Detokenizer,
    #[allow(dead_code)]
    encoder_path: PathBuf,
    #[allow(dead_code)]
    decoder_path: PathBuf,
}

#[derive(Default)]
pub struct ParakeetOnnxState {
    pub inner: Arc<Mutex<Option<ParakeetOnnxRuntime>>>,
}

impl ParakeetOnnxRuntime {
    /// Build both sessions and the detokenizer. The session builds run
    /// CPU-only — Parakeet's encoder is heavy enough on CUDA that we'd
    /// want a separate GPU pre-flight (cuDNN init lag, fp16 quirks),
    /// but for v1 the 654 MB int8 encoder runs fast enough on CPU at
    /// ~0.1 RTF on a Ryzen-class chip. CUDA EP can come later.
    pub fn load(
        encoder_path: &Path,
        decoder_path: &Path,
        tokenizer_path: &Path,
        num_threads: i16,
    ) -> Result<Self, String> {
        for (label, p) in [
            ("encoder", encoder_path),
            ("decoder_joint", decoder_path),
            ("tokenizer.model", tokenizer_path),
        ] {
            if !p.exists() {
                return Err(format!("parakeet load: {label} {:?} not on disk", p));
            }
        }

        let detok = Detokenizer::open(tokenizer_path)?;
        println!(
            "[Parakeet] tokenizer.model: {} pieces (blank id={})",
            detok.vocab_size(),
            BLANK_ID
        );

        let encoder = build_session(encoder_path, num_threads, "encoder")?;
        let decoder_joint = build_session(decoder_path, num_threads, "decoder_joint")?;

        Ok(Self {
            encoder,
            decoder_joint,
            detok,
            encoder_path: encoder_path.to_path_buf(),
            decoder_path: decoder_path.to_path_buf(),
        })
    }

    /// Transcribe a 16 kHz mono f32 utterance. Empty input → empty
    /// transcript. Errors propagate as `String` so the caller can log
    /// them without an extra wrapper.
    pub fn transcribe(&mut self, samples_16k: &[f32]) -> Result<String, String> {
        if samples_16k.is_empty() {
            return Ok(String::new());
        }

        // 1. Mel features → [1, 128, T].
        let mel = compute_mel(samples_16k);
        let t = mel.shape()[2];
        if t == 0 {
            return Ok(String::new());
        }

        // 2. Encoder forward.
        let encoded = self.run_encoder(mel, t)?;
        let enc_t = encoded.shape()[2];

        // 3. Greedy RNN-T decode over encoded frames.
        let ids = self.greedy_decode(&encoded, enc_t)?;
        Ok(self.detok.decode(&ids))
    }

    fn run_encoder(&mut self, mel: Array3<f32>, t: usize) -> Result<Array3<f32>, String> {
        // length: i64 [1] = T (the un-normalised mel frame count).
        let length = Array1::<i64>::from_elem(1, t as i64);

        let mut inputs: HashMap<String, DynTensor> = HashMap::new();
        inputs.insert(
            "audio_signal".into(),
            Tensor::from_array(mel)
                .map_err(|e| format!("wrap audio_signal: {}", e))?
                .upcast(),
        );
        inputs.insert(
            "length".into(),
            Tensor::from_array(length)
                .map_err(|e| format!("wrap length: {}", e))?
                .upcast(),
        );

        let outputs = self
            .encoder
            .run(inputs)
            .map_err(|e| format!("parakeet encoder run: {}", e))?;

        // We only care about `outputs`; `encoded_lengths` is the
        // post-subsampling frame count, which equals outputs.shape()[2]
        // when batch=1, so reading it from the tensor shape avoids a
        // separate i64 extract.
        let mut encoded: Option<Array3<f32>> = None;
        for (name, val) in outputs {
            match name.as_ref() {
                "outputs" => {
                    let (shape, data) = val
                        .try_extract_tensor::<f32>()
                        .map_err(|e| format!("extract encoder outputs: {}", e))?;
                    let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
                    let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
                        .map_err(|e| format!("reshape encoder outputs: {}", e))?;
                    encoded = Some(
                        arr.into_dimensionality::<Ix3>()
                            .map_err(|e| format!("encoder outputs dim: {}", e))?,
                    );
                }
                _ => {}
            }
        }
        encoded.ok_or_else(|| "parakeet encoder produced no `outputs`".to_string())
    }

    fn greedy_decode(&mut self, encoded: &Array3<f32>, enc_t: usize) -> Result<Vec<u32>, String> {
        // Predictor LSTM state: [num_layers=2, batch=1, hidden=640].
        let mut state1 = Array3::<f32>::zeros((PRED_LAYERS, 1, PRED_HIDDEN));
        let mut state2 = Array3::<f32>::zeros((PRED_LAYERS, 1, PRED_HIDDEN));
        let mut last_token = BLANK_ID;
        let mut tokens: Vec<u32> = Vec::new();

        for t in 0..enc_t {
            // enc_t_slice: [1, 1024, 1]. ndarray slice copies the column.
            let mut enc_slice = Array3::<f32>::zeros((1, encoded.shape()[1], 1));
            for c in 0..encoded.shape()[1] {
                enc_slice[[0, c, 0]] = encoded[[0, c, t]];
            }

            for _inner in 0..MAX_SYMBOLS_PER_FRAME {
                let (logits, s1, s2) =
                    self.run_decoder_joint(&enc_slice, last_token, &state1, &state2)?;

                // logits is already 1D ([1,1,1,V+1] flattened on extract),
                // which matches V+1 = 1025. argmax over the full vector.
                debug_assert_eq!(logits.len(), logits_total_len());
                let idx = argmax(&logits);

                if idx as u32 == BLANK_ID {
                    // Advance encoder time step. Predictor state is
                    // NOT updated on blank (model card's Python loop
                    // doesn't update s1/s2 either) — RNN-T's predictor
                    // only steps forward when a real token emits.
                    break;
                }

                tokens.push(idx as u32);
                last_token = idx as u32;
                state1 = s1;
                state2 = s2;
            }
        }

        Ok(tokens)
    }

    fn run_decoder_joint(
        &mut self,
        encoder_outputs: &Array3<f32>,
        last_token: u32,
        state1: &Array3<f32>,
        state2: &Array3<f32>,
    ) -> Result<(Array1<f32>, Array3<f32>, Array3<f32>), String> {
        // Inputs per the model card:
        //   encoder_outputs:  f32 [1, 1024, 1]
        //   targets:          i32 [1, 1]
        //   target_length:    i32 [1]
        //   input_states_1/2: f32 [2, 1, 640]
        let targets = Array2::<i32>::from_elem((1, 1), last_token as i32);
        let target_length = Array1::<i32>::from_elem(1, 1);

        let mut inputs: HashMap<String, DynTensor> = HashMap::new();
        inputs.insert(
            "encoder_outputs".into(),
            Tensor::from_array(encoder_outputs.clone())
                .map_err(|e| format!("wrap encoder_outputs: {}", e))?
                .upcast(),
        );
        inputs.insert(
            "targets".into(),
            Tensor::from_array(targets)
                .map_err(|e| format!("wrap targets: {}", e))?
                .upcast(),
        );
        inputs.insert(
            "target_length".into(),
            Tensor::from_array(target_length)
                .map_err(|e| format!("wrap target_length: {}", e))?
                .upcast(),
        );
        inputs.insert(
            "input_states_1".into(),
            Tensor::from_array(state1.clone())
                .map_err(|e| format!("wrap input_states_1: {}", e))?
                .upcast(),
        );
        inputs.insert(
            "input_states_2".into(),
            Tensor::from_array(state2.clone())
                .map_err(|e| format!("wrap input_states_2: {}", e))?
                .upcast(),
        );

        let outputs = self
            .decoder_joint
            .run(inputs)
            .map_err(|e| format!("parakeet decoder_joint run: {}", e))?;

        let mut logits: Option<Array1<f32>> = None;
        let mut s1_out: Option<Array3<f32>> = None;
        let mut s2_out: Option<Array3<f32>> = None;
        for (name, val) in outputs {
            match name.as_ref() {
                "outputs" => {
                    let (_shape, data) = val
                        .try_extract_tensor::<f32>()
                        .map_err(|e| format!("extract logits: {}", e))?;
                    // [1, 1, 1, V+1] is contiguous f32 — flatten to 1D
                    // V+1 (=1025) for the argmax lookup below.
                    logits = Some(Array1::from_vec(data.to_vec()));
                }
                "output_states_1" => {
                    let (shape, data) = val
                        .try_extract_tensor::<f32>()
                        .map_err(|e| format!("extract states_1: {}", e))?;
                    let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
                    let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
                        .map_err(|e| format!("reshape states_1: {}", e))?;
                    s1_out = Some(
                        arr.into_dimensionality::<Ix3>()
                            .map_err(|e| format!("states_1 dim: {}", e))?,
                    );
                }
                "output_states_2" => {
                    let (shape, data) = val
                        .try_extract_tensor::<f32>()
                        .map_err(|e| format!("extract states_2: {}", e))?;
                    let shape: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
                    let arr = ArrayD::from_shape_vec(IxDyn(&shape), data.to_vec())
                        .map_err(|e| format!("reshape states_2: {}", e))?;
                    s2_out = Some(
                        arr.into_dimensionality::<Ix3>()
                            .map_err(|e| format!("states_2 dim: {}", e))?,
                    );
                }
                _ => {} // prednet_lengths: ignored (it's just [1] = 1)
            }
        }

        Ok((
            logits.ok_or_else(|| "decoder_joint produced no logits".to_string())?,
            s1_out.ok_or_else(|| "decoder_joint produced no output_states_1".to_string())?,
            s2_out.ok_or_else(|| "decoder_joint produced no output_states_2".to_string())?,
        ))
    }
}

/// Logits length: 1024 SP pieces + 1 blank = 1025. Wrapped so the
/// magic number lives next to the BLANK_ID constant.
fn logits_total_len() -> usize {
    BLANK_ID as usize + 1
}

fn argmax(v: &Array1<f32>) -> usize {
    let mut best_i = 0usize;
    let mut best = f32::NEG_INFINITY;
    for (i, &x) in v.iter().enumerate() {
        if x > best {
            best = x;
            best_i = i;
        }
    }
    best_i
}

fn build_session(path: &Path, num_threads: i16, label: &str) -> Result<Session, String> {
    use std::io::Write;
    println!(
        "[Parakeet] build_session: {} ({} bytes) ep=CPU threads={}",
        path.display(),
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        num_threads,
    );
    let _ = std::io::stdout().flush();

    let session = Session::builder()
        .map_err(|e| format!("ort builder: {}", e))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| format!("ort opt level: {}", e))?
        .with_intra_threads(num_threads as usize)
        .map_err(|e| format!("ort intra threads: {}", e))?
        .with_execution_providers([CPUExecutionProvider::default().build()])
        .map_err(|e| format!("ort eps CPU: {}", e))?
        .commit_from_file(path)
        .map_err(|e| format!("ort commit {label}: {}", e))?;
    println!("[Parakeet] {label} loaded");
    Ok(session)
}
