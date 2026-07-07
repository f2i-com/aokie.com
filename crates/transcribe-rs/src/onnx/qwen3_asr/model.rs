//! Qwen3-ASR-0.6B greedy decode loop.
//!
//! Three ONNX sessions:
//!   - `encoder.onnx`         : mel `[1, 128, T]` → audio_features `[1, T', H]`
//!   - `decoder_init.onnx`    : prefill — input_ids + position_ids +
//!                              audio_features + audio_offset → logits +
//!                              present_keys + present_values
//!   - `decoder_step.onnx`    : autoregressive — input_embeds + position_ids
//!                              + past_keys + past_values → logits + present_*
//!
//! Plus an external f16 token-embedding matrix `embed_tokens.bin` of
//! shape `[vocab_size, hidden]` that the Python reference loads with
//! `np.fromfile` and indexes with `embed_tokens[token_id]` to build the
//! per-step input embedding for `decoder_step`.
//!
//! The decoder's int4 file (~962 MB) shares external weights via
//! `.data` next to the ONNX graph; ORT loads those automatically when
//! `commit_from_file` resolves the relative path. We don't need to do
//! anything special on the Rust side beyond pointing at the right
//! `.onnx` file.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use half::f16;
use memmap2::Mmap;
use ndarray::{Array1, Array2, Array3};
use ort::session::Session;
use ort::value::TensorRef;

use crate::onnx::{session, Quantization};
use crate::{
    ModelCapabilities, SpeechModel, TranscribeError, TranscribeOptions, TranscriptionResult,
};

use super::mel;

// ---- Special token IDs (from src/prompt.py) ----
const ENDOFTEXT_TOKEN_ID: i64 = 151643;
const IM_START_TOKEN_ID: i64 = 151644;
const IM_END_TOKEN_ID: i64 = 151645;
const AUDIO_START_TOKEN_ID: i64 = 151669;
const AUDIO_END_TOKEN_ID: i64 = 151670;
const AUDIO_PAD_TOKEN_ID: i64 = 151676;
const NEWLINE_TOKEN_ID: i64 = 198;

// Qwen3 chat template: "system" / "user" / "assistant" each get a
// fixed three-token sequence in the byte-level BPE. Captured from the
// reference tokenizer.json.
const SYSTEM_TOKEN_ID: i64 = 8948;
const USER_TOKEN_ID: i64 = 872;
const ASSISTANT_TOKEN_ID: i64 = 77091;

const EOS_TOKENS: [i64; 2] = [ENDOFTEXT_TOKEN_ID, IM_END_TOKEN_ID];

const VOCAB_SIZE: usize = 151_936;
const HIDDEN_SIZE: usize = 1024;

// Encoder token-rate constants from the reference encoder_wrapper.py.
const CONV_WINDOW: usize = 100;
const TOKENS_PER_WINDOW: usize = 13;

const CAPABILITIES: ModelCapabilities = ModelCapabilities {
    name: "Qwen3-ASR-0.6B",
    engine_id: "qwen3_asr",
    sample_rate: 16_000,
    languages: &["en", "zh", "ja", "ko", "fr", "de", "es", "ru", "pt", "ar"],
    supports_timestamps: false,
    supports_translation: false,
    supports_streaming: false,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Qwen3AsrVariant {
    /// 0.6B parameter Qwen3 decoder. Currently the only published
    /// variant; the enum is here so a follow-up can add a 1.5B/3B
    /// sibling without breaking the API.
    Size0_6B,
}

impl Default for Qwen3AsrVariant {
    fn default() -> Self {
        Qwen3AsrVariant::Size0_6B
    }
}

#[derive(Debug, Clone, Default)]
pub struct Qwen3AsrParams {
    /// Optional language hint (BCP-47 code, e.g. "en", "zh").
    /// When set, the engine extends the assistant prefix with
    /// `language <LangName>` so the model skips its built-in
    /// language-ID step. `None` falls back to auto-detection.
    pub language: Option<String>,
    /// Cap on generated tokens. Default 256, mirroring the reference
    /// inference.py.
    pub max_tokens: Option<usize>,
}

pub struct Qwen3AsrModel {
    encoder: Session,
    decoder_init: Session,
    decoder_step: Session,
    /// `tokenizer.json` parsed by HF tokenizers — only used for the
    /// detokenize step. Encoding runs through the static prompt
    /// template, so we don't pay the encode cost per call.
    tokenizer: tokenizers::Tokenizer,
    /// Memory-mapped `embed_tokens.bin`. f16 row-major, shape
    /// `[VOCAB_SIZE, HIDDEN_SIZE]`. Held as a Mmap so we don't pay
    /// 311 MB of allocator pressure per process.
    embed_tokens: EmbedTokens,
    #[allow(dead_code)]
    variant: Qwen3AsrVariant,
}

/// Read-only view over the f16 embed_tokens.bin file. Lookups copy a
/// single row into a small `Vec<f32>` so the per-step decoder input is
/// a contiguous owned buffer.
struct EmbedTokens {
    mmap: Arc<Mmap>,
}

impl EmbedTokens {
    fn open(path: &Path) -> Result<Self, TranscribeError> {
        let file = File::open(path).map_err(|e| {
            TranscribeError::Io(std::io::Error::new(
                e.kind(),
                format!("open embed_tokens.bin {}: {}", path.display(), e),
            ))
        })?;
        let expected_bytes = VOCAB_SIZE * HIDDEN_SIZE * std::mem::size_of::<u16>();
        let actual_bytes = file
            .metadata()
            .map(|m| m.len() as usize)
            .unwrap_or(expected_bytes);
        if actual_bytes != expected_bytes {
            return Err(TranscribeError::Config(format!(
                "embed_tokens.bin size mismatch: expected {} bytes ([{}, {}] f16), got {}",
                expected_bytes, VOCAB_SIZE, HIDDEN_SIZE, actual_bytes
            )));
        }
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| {
            TranscribeError::Io(std::io::Error::new(
                e.kind(),
                format!("mmap embed_tokens.bin: {}", e),
            ))
        })?;
        Ok(Self {
            mmap: Arc::new(mmap),
        })
    }

    /// Pull row `id` out as `[1, 1, HIDDEN_SIZE]` f32. Out-of-range
    /// returns zeros — same fail-safe behaviour the reference uses
    /// implicitly via numpy bounds-clamping at read time.
    fn lookup(&self, id: i64) -> Array3<f32> {
        let mut row = Array3::<f32>::zeros((1, 1, HIDDEN_SIZE));
        if id < 0 || (id as usize) >= VOCAB_SIZE {
            return row;
        }
        let stride_bytes = HIDDEN_SIZE * std::mem::size_of::<u16>();
        let start = (id as usize) * stride_bytes;
        let end = start + stride_bytes;
        let bytes = &self.mmap[start..end];
        for (i, chunk) in bytes.chunks_exact(2).enumerate() {
            let bits = u16::from_le_bytes([chunk[0], chunk[1]]);
            row[[0, 0, i]] = f16::from_bits(bits).to_f32();
        }
        row
    }
}

impl Qwen3AsrModel {
    /// Load the three ONNX sessions, the tokenizer, and the embed
    /// table out of `model_dir`. `quantization` selects the
    /// `.int4`/`.fp16`/no-suffix variants of `encoder.onnx` /
    /// `decoder_init.onnx` / `decoder_step.onnx`.
    pub fn load(
        model_dir: &Path,
        variant: Qwen3AsrVariant,
        quantization: &Quantization,
    ) -> Result<Self, TranscribeError> {
        let encoder_path = session::resolve_model_path(model_dir, "encoder", quantization);
        let init_path = session::resolve_model_path(model_dir, "decoder_init", quantization);
        let step_path = session::resolve_model_path(model_dir, "decoder_step", quantization);
        for path in [&encoder_path, &init_path, &step_path] {
            if !path.exists() {
                return Err(TranscribeError::ModelNotFound(path.clone()));
            }
        }
        let tokenizer_path = model_dir.join("tokenizer.json");
        if !tokenizer_path.exists() {
            return Err(TranscribeError::ModelNotFound(tokenizer_path));
        }
        let embed_path = model_dir.join("embed_tokens.bin");
        if !embed_path.exists() {
            return Err(TranscribeError::ModelNotFound(embed_path));
        }

        log::info!("Loading Qwen3-ASR encoder from {:?}", encoder_path);
        let encoder = session::create_session(&encoder_path)?;
        log::info!("Loading Qwen3-ASR decoder_init from {:?}", init_path);
        let decoder_init = session::create_session(&init_path)?;
        log::info!("Loading Qwen3-ASR decoder_step from {:?}", step_path);
        let decoder_step = session::create_session(&step_path)?;

        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| TranscribeError::Config(format!("load tokenizer.json: {}", e)))?;

        let embed_tokens = EmbedTokens::open(&embed_path)?;

        Ok(Self {
            encoder,
            decoder_init,
            decoder_step,
            tokenizer,
            embed_tokens,
            variant,
        })
    }

    /// Transcribe with model-specific parameters.
    pub fn transcribe_with(
        &mut self,
        samples: &[f32],
        params: &Qwen3AsrParams,
    ) -> Result<TranscriptionResult, TranscribeError> {
        let lang_priming = params.language.as_deref().and_then(bcp47_to_qwen_lang_name);
        self.infer(samples, params.max_tokens.unwrap_or(256), lang_priming)
    }

    fn infer(
        &mut self,
        samples: &[f32],
        max_tokens: usize,
        lang_priming: Option<&str>,
    ) -> Result<TranscriptionResult, TranscribeError> {
        if samples.is_empty() {
            return Ok(TranscriptionResult {
                text: String::new(),
                segments: None,
            });
        }

        // 1. Mel features → encoder.
        let mel_arr = mel::compute_log_mel(samples);
        let t_in = mel_arr.shape()[2];
        if t_in == 0 {
            return Ok(TranscriptionResult {
                text: String::new(),
                segments: None,
            });
        }
        let audio_features = self.run_encoder(mel_arr)?;
        let audio_token_count = audio_features.shape()[1];

        // 2. Build prompt token IDs. Replaces audio_pad slots with
        //    encoder-output indices in the v3 decoder format. When the
        //    caller supplied a language hint, we extend the assistant
        //    prefix with `language <LangName>` so the model is locked
        //    into that language instead of running its built-in
        //    language-ID step. The training-time prompt template (see
        //    andrewleech/qwen3-asr-onnx prompt.py) primes the assistant
        //    with `language ` and lets the model emit the language
        //    name; appending `English` here turns that auto-detection
        //    into a hardcode. Without this, short / accented English
        //    audio routinely tokenized into Chinese (`大概大概。`).
        let mut prompt_ids = build_prompt_ids(audio_token_count);
        if let Some(lang) = lang_priming {
            let priming = format!("language {}", lang);
            let encoded = self
                .tokenizer
                .encode(priming, false)
                .map_err(|e| TranscribeError::Inference(format!("encode lang priming: {}", e)))?;
            for id in encoded.get_ids() {
                prompt_ids.push(*id as i64);
            }
        }
        let position_ids = (0..prompt_ids.len() as i64).collect::<Vec<i64>>();
        let position_ids = Array2::from_shape_vec((1, prompt_ids.len()), position_ids)
            .map_err(|e| TranscribeError::Inference(format!("position_ids shape: {}", e)))?;

        let audio_offset = prompt_ids
            .iter()
            .position(|&t| t == AUDIO_PAD_TOKEN_ID)
            .ok_or_else(|| TranscribeError::Inference("prompt missing audio_pad".into()))?
            as i64;

        // 3. decoder_init prefill.
        let (mut next_token, mut past_keys, mut past_values) =
            self.run_decoder_init(&prompt_ids, &position_ids, &audio_features, audio_offset)?;

        let mut tokens: Vec<i64> = vec![next_token];
        if EOS_TOKENS.contains(&next_token) {
            return Ok(TranscriptionResult {
                text: self.detokenize(&tokens),
                segments: None,
            });
        }

        // 4. Greedy step loop.
        let mut pos = prompt_ids.len() as i64;
        for _ in 0..max_tokens.saturating_sub(1) {
            let token_embed = self.embed_tokens.lookup(next_token);
            let step_pos = Array2::<i64>::from_elem((1, 1), pos);
            let (logit_id, k, v) =
                self.run_decoder_step(&token_embed, &step_pos, &past_keys, &past_values)?;
            past_keys = k;
            past_values = v;
            next_token = logit_id;
            tokens.push(next_token);
            pos += 1;
            if EOS_TOKENS.contains(&next_token) {
                break;
            }
        }

        Ok(TranscriptionResult {
            text: self.detokenize(&tokens),
            segments: None,
        })
    }

    fn run_encoder(&mut self, mel_arr: Array3<f32>) -> Result<Array3<f32>, TranscribeError> {
        let mel_dyn = mel_arr.into_dyn();
        let t_input = TensorRef::from_array_view(mel_dyn.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap mel: {}", e)))?;
        let outputs = self
            .encoder
            .run(ort::inputs![
                "mel" => t_input,
            ])
            .map_err(|e| TranscribeError::Inference(format!("encoder run: {}", e)))?;
        let audio = outputs
            .get("audio_features")
            .ok_or_else(|| {
                TranscribeError::Inference("encoder missing 'audio_features' output".into())
            })?
            .try_extract_array::<f32>()
            .map_err(|e| TranscribeError::Inference(format!("extract audio_features: {}", e)))?;
        let owned = audio.to_owned();
        owned
            .into_dimensionality::<ndarray::Ix3>()
            .map_err(|e| TranscribeError::Inference(format!("audio_features dim: {}", e)))
    }

    fn run_decoder_init(
        &mut self,
        prompt_ids: &[i64],
        position_ids: &Array2<i64>,
        audio_features: &Array3<f32>,
        audio_offset: i64,
    ) -> Result<(i64, ndarray::ArrayD<f32>, ndarray::ArrayD<f32>), TranscribeError> {
        let input_ids = Array2::from_shape_vec((1, prompt_ids.len()), prompt_ids.to_vec())
            .map_err(|e| TranscribeError::Inference(format!("input_ids shape: {}", e)))?;
        let audio_offset_t = Array1::<i64>::from_elem(1, audio_offset);

        let input_ids_dyn = input_ids.into_dyn();
        let position_ids_dyn = position_ids.clone().into_dyn();
        let audio_dyn = audio_features.clone().into_dyn();
        let audio_offset_dyn = audio_offset_t.into_dyn();

        let t_input = TensorRef::from_array_view(input_ids_dyn.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap input_ids: {}", e)))?;
        let t_pos = TensorRef::from_array_view(position_ids_dyn.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap position_ids: {}", e)))?;
        let t_audio = TensorRef::from_array_view(audio_dyn.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap audio_features: {}", e)))?;
        let t_offset = TensorRef::from_array_view(audio_offset_dyn.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap audio_offset: {}", e)))?;

        let outputs = self
            .decoder_init
            .run(ort::inputs![
                "input_ids" => t_input,
                "position_ids" => t_pos,
                "audio_features" => t_audio,
                "audio_offset" => t_offset,
            ])
            .map_err(|e| TranscribeError::Inference(format!("decoder_init run: {}", e)))?;

        let logits = outputs
            .get("logits")
            .ok_or_else(|| TranscribeError::Inference("decoder_init missing 'logits'".into()))?
            .try_extract_array::<f32>()
            .map_err(|e| TranscribeError::Inference(format!("extract init logits: {}", e)))?;
        let next_token = argmax_last_step(&logits)?;

        let present_keys = outputs
            .get("present_keys")
            .ok_or_else(|| {
                TranscribeError::Inference("decoder_init missing 'present_keys'".into())
            })?
            .try_extract_array::<f32>()
            .map_err(|e| TranscribeError::Inference(format!("extract present_keys: {}", e)))?
            .to_owned();
        let present_values = outputs
            .get("present_values")
            .ok_or_else(|| {
                TranscribeError::Inference("decoder_init missing 'present_values'".into())
            })?
            .try_extract_array::<f32>()
            .map_err(|e| TranscribeError::Inference(format!("extract present_values: {}", e)))?
            .to_owned();
        Ok((next_token, present_keys, present_values))
    }

    fn run_decoder_step(
        &mut self,
        input_embeds: &Array3<f32>,
        position_ids: &Array2<i64>,
        past_keys: &ndarray::ArrayD<f32>,
        past_values: &ndarray::ArrayD<f32>,
    ) -> Result<(i64, ndarray::ArrayD<f32>, ndarray::ArrayD<f32>), TranscribeError> {
        let input_embeds_dyn = input_embeds.clone().into_dyn();
        let position_ids_dyn = position_ids.clone().into_dyn();

        let t_embeds = TensorRef::from_array_view(input_embeds_dyn.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap input_embeds: {}", e)))?;
        let t_pos = TensorRef::from_array_view(position_ids_dyn.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap step position_ids: {}", e)))?;
        let t_pk = TensorRef::from_array_view(past_keys.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap past_keys: {}", e)))?;
        let t_pv = TensorRef::from_array_view(past_values.view())
            .map_err(|e| TranscribeError::Inference(format!("wrap past_values: {}", e)))?;

        let outputs = self
            .decoder_step
            .run(ort::inputs![
                "input_embeds" => t_embeds,
                "position_ids" => t_pos,
                "past_keys" => t_pk,
                "past_values" => t_pv,
            ])
            .map_err(|e| TranscribeError::Inference(format!("decoder_step run: {}", e)))?;

        let logits = outputs
            .get("logits")
            .ok_or_else(|| TranscribeError::Inference("decoder_step missing 'logits'".into()))?
            .try_extract_array::<f32>()
            .map_err(|e| TranscribeError::Inference(format!("extract step logits: {}", e)))?;
        let next_token = argmax_last_step(&logits)?;

        let present_keys = outputs
            .get("present_keys")
            .ok_or_else(|| {
                TranscribeError::Inference("decoder_step missing 'present_keys'".into())
            })?
            .try_extract_array::<f32>()
            .map_err(|e| TranscribeError::Inference(format!("extract step present_keys: {}", e)))?
            .to_owned();
        let present_values = outputs
            .get("present_values")
            .ok_or_else(|| {
                TranscribeError::Inference("decoder_step missing 'present_values'".into())
            })?
            .try_extract_array::<f32>()
            .map_err(|e| TranscribeError::Inference(format!("extract step present_values: {}", e)))?
            .to_owned();
        Ok((next_token, present_keys, present_values))
    }

    fn detokenize(&self, tokens: &[i64]) -> String {
        // Strip EOS / control tokens so the returned text is just the
        // assistant reply. The HF tokenizer's `decode(skip_special_tokens=true)`
        // handles `<|endoftext|>` / `<|im_end|>` automatically.
        let ids_u32: Vec<u32> = tokens
            .iter()
            .filter(|&&t| !EOS_TOKENS.contains(&t))
            .map(|&t| t as u32)
            .collect();
        let raw = match self.tokenizer.decode(&ids_u32, true) {
            Ok(text) => text,
            Err(e) => {
                log::warn!("[Qwen3-ASR] tokenizer decode failed: {}", e);
                return String::new();
            }
        };
        extract_asr_text(&raw)
    }
}

/// Pull the transcript out of a Qwen3-ASR assistant turn.
///
/// Reference inference (andrewleech/qwen3-asr-onnx prompt.py) primes
/// the assistant with `language ` and asks the model to emit
/// `<LangName><asr_text>{transcript}<|im_end|>`. The HF tokenizer
/// strips `<|im_end|>` under `skip_special_tokens=true` but keeps
/// `<asr_text>` literal because it's an added_token, not a registered
/// special token in `tokenizer.json`. So a raw decode comes out as
/// `English<asr_text>Hi, I'd like to book an appointment.` (or, with
/// the prompt template's leading word, `language English<asr_text>…`).
///
/// Postprocess: if the marker is present, return everything after it;
/// otherwise hand back the raw text (so a future export that drops
/// the marker still works). Also strips a trailing `</asr_text>` in
/// case a closing tag leaks through.
fn extract_asr_text(raw: &str) -> String {
    const OPEN: &str = "<asr_text>";
    const CLOSE: &str = "</asr_text>";
    let body = match raw.find(OPEN) {
        Some(idx) => &raw[idx + OPEN.len()..],
        None => raw,
    };
    let body = match body.rfind(CLOSE) {
        Some(idx) => &body[..idx],
        None => body,
    };
    body.trim().to_string()
}

impl SpeechModel for Qwen3AsrModel {
    fn capabilities(&self) -> ModelCapabilities {
        CAPABILITIES
    }

    fn transcribe_raw(
        &mut self,
        samples: &[f32],
        options: &TranscribeOptions,
    ) -> Result<TranscriptionResult, TranscribeError> {
        let lang_priming = options
            .language
            .as_deref()
            .and_then(bcp47_to_qwen_lang_name);
        // 256 tokens matches the reference inference.py and is enough for
        // any single phone-call utterance the receptionist sees in
        // practice (a verbose caller turn is ~30 tokens). The trait
        // surface is intentionally narrow — engines with a real reason
        // to accept a runtime cap should expose it via `transcribe_with`
        // on the concrete type, not by widening `TranscribeOptions`.
        self.infer(samples, 256, lang_priming)
    }
}

/// Map a BCP-47 language code to the language name Qwen3-ASR's training
/// data uses in its assistant priming. Returning `None` means the model
/// will language-ID the audio itself (the previous behaviour). The
/// match list mirrors `CAPABILITIES.languages` — extending one without
/// the other will cause a silent fall-through.
///
/// Region-tagged inputs (`en-AU`, `pt-BR`, `zh-CN`, `zh_HK`) are
/// reduced to the primary subtag (the part before the first `-` /
/// `_`) before lookup. Without that reduction every realistic browser-
/// or-OS-supplied language code missed and the model fell back to
/// auto-detection — which is exactly the priming the receptionist is
/// trying to bypass for short / accented English.
fn bcp47_to_qwen_lang_name(code: &str) -> Option<&'static str> {
    let trimmed = code.trim().to_ascii_lowercase();
    let primary = trimmed
        .split(|c: char| c == '-' || c == '_')
        .next()
        .unwrap_or(trimmed.as_str());
    match primary {
        "en" => Some("English"),
        "zh" => Some("Chinese"),
        "ja" => Some("Japanese"),
        "ko" => Some("Korean"),
        "fr" => Some("French"),
        "de" => Some("German"),
        "es" => Some("Spanish"),
        "ru" => Some("Russian"),
        "pt" => Some("Portuguese"),
        "ar" => Some("Arabic"),
        _ => None,
    }
}

/// Build the prompt token ids matching `src/prompt.py:build_prompt_ids`:
///
/// `<|im_start|>system\n<|im_end|>\n<|im_start|>user\n<|audio_start|><|audio_pad|>...<|audio_end|><|im_end|>\n<|im_start|>assistant\n`
fn build_prompt_ids(audio_token_count: usize) -> Vec<i64> {
    let mut ids = Vec::with_capacity(audio_token_count + 32);
    // system\n
    ids.push(IM_START_TOKEN_ID);
    ids.push(SYSTEM_TOKEN_ID);
    ids.push(NEWLINE_TOKEN_ID);
    ids.push(IM_END_TOKEN_ID);
    ids.push(NEWLINE_TOKEN_ID);
    // user\n<|audio_start|><|audio_pad|>...<|audio_end|><|im_end|>\n
    ids.push(IM_START_TOKEN_ID);
    ids.push(USER_TOKEN_ID);
    ids.push(NEWLINE_TOKEN_ID);
    ids.push(AUDIO_START_TOKEN_ID);
    for _ in 0..audio_token_count {
        ids.push(AUDIO_PAD_TOKEN_ID);
    }
    ids.push(AUDIO_END_TOKEN_ID);
    ids.push(IM_END_TOKEN_ID);
    ids.push(NEWLINE_TOKEN_ID);
    // assistant\n
    ids.push(IM_START_TOKEN_ID);
    ids.push(ASSISTANT_TOKEN_ID);
    ids.push(NEWLINE_TOKEN_ID);
    ids
}

/// Argmax over the vocab dim of the last sequence position. Logits are
/// `[1, seq, V]` for init and `[1, 1, V]` for step.
fn argmax_last_step(logits: &ndarray::ArrayViewD<f32>) -> Result<i64, TranscribeError> {
    let shape = logits.shape();
    if shape.len() != 3 {
        return Err(TranscribeError::Inference(format!(
            "logits must be rank-3, got {:?}",
            shape
        )));
    }
    let seq = shape[1];
    let vocab = shape[2];
    if seq == 0 || vocab == 0 {
        return Err(TranscribeError::Inference("logits empty".into()));
    }
    let last_pos = seq - 1;
    let mut best_i = 0usize;
    let mut best = f32::NEG_INFINITY;
    for v in 0..vocab {
        let x = logits[[0, last_pos, v]];
        if x > best {
            best = x;
            best_i = v;
        }
    }
    Ok(best_i as i64)
}

// ---- diagnostics: how many encoder tokens this many input samples produces ----
//
// The encoder applies `(input_lengths % CONV_WINDOW) → conv-out twice
// → conv-out once → + (input_lengths // CONV_WINDOW) * TOKENS_PER_WINDOW`.
// We expose this as a free function so callers (e.g. a future chunked
// transcriber) can pre-compute the audio_pad slot count without
// running the encoder.
#[allow(dead_code)]
pub fn feat_extract_output_lengths(mel_frames: usize) -> usize {
    fn conv_out(t: usize) -> usize {
        (t + 1) / 2
    }
    let leave = mel_frames % CONV_WINDOW;
    let t = conv_out(conv_out(conv_out(leave)));
    t + (mel_frames / CONV_WINDOW) * TOKENS_PER_WINDOW
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_asr_text_strips_language_prefix_and_marker() {
        // Reference output from the andrewleech int4 export.
        let raw = "language English<asr_text>Hi, I'd like to make an appointment. Please.";
        assert_eq!(
            extract_asr_text(raw),
            "Hi, I'd like to make an appointment. Please."
        );
    }

    #[test]
    fn extract_asr_text_handles_closing_tag_if_present() {
        let raw = "Mandarin<asr_text>你好</asr_text>";
        assert_eq!(extract_asr_text(raw), "你好");
    }

    #[test]
    fn extract_asr_text_falls_back_when_marker_absent() {
        // Defensive: a future export that drops `<asr_text>` should
        // still produce a usable transcript instead of an empty string.
        let raw = "Hello world";
        assert_eq!(extract_asr_text(raw), "Hello world");
    }

    #[test]
    fn bcp47_primary_subtag_resolves() {
        // The values browsers / OS speech APIs / Aokie's own settings
        // panel hand to us in real installs — `en-AU`, `pt-BR`,
        // `zh-CN`, even underscore-separated `zh_HK`. Each must
        // resolve to the same priming string as the bare primary
        // subtag, otherwise the call answerer falls back to the
        // model's built-in language ID (which on short / accented
        // English routinely mis-routes into Mandarin).
        assert_eq!(bcp47_to_qwen_lang_name("en-AU"), Some("English"));
        assert_eq!(bcp47_to_qwen_lang_name("en-US"), Some("English"));
        assert_eq!(bcp47_to_qwen_lang_name("EN-GB"), Some("English"));
        assert_eq!(bcp47_to_qwen_lang_name("pt-BR"), Some("Portuguese"));
        assert_eq!(bcp47_to_qwen_lang_name("pt-PT"), Some("Portuguese"));
        assert_eq!(bcp47_to_qwen_lang_name("zh-CN"), Some("Chinese"));
        assert_eq!(bcp47_to_qwen_lang_name("zh_HK"), Some("Chinese"));
        // Bare primary subtags still work.
        assert_eq!(bcp47_to_qwen_lang_name("en"), Some("English"));
        assert_eq!(bcp47_to_qwen_lang_name("ja"), Some("Japanese"));
        // Whitespace + casing tolerated.
        assert_eq!(bcp47_to_qwen_lang_name("  fr-CA  "), Some("French"));
        // Unknown language falls through to None — model will language-ID.
        assert_eq!(bcp47_to_qwen_lang_name("eo"), None);
        assert_eq!(bcp47_to_qwen_lang_name(""), None);
        assert_eq!(bcp47_to_qwen_lang_name("xx-YY"), None);
    }

    #[test]
    fn build_prompt_includes_audio_pad_run() {
        let ids = build_prompt_ids(5);
        let pads = ids.iter().filter(|&&t| t == AUDIO_PAD_TOKEN_ID).count();
        assert_eq!(pads, 5);
        // Audio start sits immediately before the first audio_pad.
        let first_pad = ids
            .iter()
            .position(|&t| t == AUDIO_PAD_TOKEN_ID)
            .expect("has pad");
        assert_eq!(ids[first_pad - 1], AUDIO_START_TOKEN_ID);
        // Audio end sits immediately after the last audio_pad.
        let last_pad = ids
            .iter()
            .rposition(|&t| t == AUDIO_PAD_TOKEN_ID)
            .expect("has pad");
        assert_eq!(ids[last_pad + 1], AUDIO_END_TOKEN_ID);
    }
}
