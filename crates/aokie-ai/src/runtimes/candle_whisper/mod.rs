//! Candle-based Whisper STT runtime. The default bundled model is
//! `openai/whisper-large-v3-turbo`; the loader / decoder loop are
//! generic across the Whisper family — different sizes and
//! checkpoints can plug in by editing
//! `<app_data>/ai_providers.json`.
//!
//! Produces text transcripts for call history/display. Whisper is *not* on the
//! critical path for assistant response generation — during calls the raw
//! audio is fed into Gemma 4 ONNX directly (see [`crate::runtimes::onnx_genai`]). This
//! module exists purely to keep a human-readable log of what the caller said.
//!
//! Runtime layout (under `<app_data>/models/whisper/`):
//!
//! ```text
//! config.json
//! tokenizer.json
//! model.safetensors
//! generation_config.json
//! preprocessor_config.json
//! ```
//!
//! Source: `openai/whisper-large-v3-turbo` on Hugging Face.

pub(crate) mod mel;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::whisper::{self as m, model::Whisper, Config};
use tokenizers::Tokenizer;

use mel::mel_filter_bank;

/// Special-token strings we pin manually. HF tokenizers already know these ids
/// but we look them up by string so the module is tolerant of tokenizer
/// version drift.
const LANG_EN_TOKEN: &str = "<|en|>";
/// Whisper's "previous text" marker. Tokens placed between SOP and SOT
/// give the decoder context to anchor against, dramatically reducing
/// hallucinations on short / quiet clips. Not exported as a constant
/// from candle_transformers, but every HF Whisper tokenizer has it.
const SOP_TOKEN: &str = "<|startofprev|>";
/// Phone-conversation prompt. Steers Whisper away from its training-data
/// hallucinations ("1.", "Thank you.", "Subscribe to my channel") by
/// anchoring on a domain-realistic context. Kept short so it leaves
/// most of the 224-token prompt budget for actual previous transcript
/// turns if we wire those in later.
const INITIAL_PROMPT_TEXT: &str =
    " Phone conversation with a customer asking about appointments, prices, or services.";

pub struct CandleWhisperRuntime {
    model: parking_lot::Mutex<Whisper>,
    tokenizer: Tokenizer,
    mel_filters: Vec<f32>,
    device: Device,
    /// dtype the model was loaded in. F16 on CUDA (matches the
    /// fp16 safetensors and dodges candle-cuda's "no F32 conv1d
    /// kernel" panic), F32 elsewhere. Used to cast the mel input
    /// tensor to a compatible precision before encoder.forward.
    model_dtype: candle_core::DType,
    /// Pre-resolved token ids for the forced prefix.
    sot: u32,
    lang_en: u32,
    transcribe: u32,
    no_timestamps: u32,
    eot: u32,
    /// `<|nospeech|>` (or `<|nocaptions|>` on older tokenizers). When
    /// present, the first decoder step's softmax over vocab gives us
    /// `no_speech_prob`; clips above [`m::NO_SPEECH_THRESHOLD`] are
    /// dropped before the model gets a chance to hallucinate.
    no_speech: Option<u32>,
    /// `<|startofprev|>`. When present and [`Self::initial_prompt_ids`]
    /// is non-empty, the decoder prefix becomes
    /// `[SOP, prompt..., SOT, lang, transcribe, no_timestamps]` instead
    /// of just the SOT block.
    sop: Option<u32>,
    /// BPE-encoded prompt text (no special tokens). Empty if SOP isn't
    /// in the tokenizer or the encode failed at load time.
    initial_prompt_ids: Vec<u32>,
}

// SAFETY: candle tensors aren't Send/Sync by default on GPU, but we serialize
// all model access behind `parking_lot::Mutex` and never hand out raw refs.
unsafe impl Send for CandleWhisperRuntime {}
unsafe impl Sync for CandleWhisperRuntime {}

impl CandleWhisperRuntime {
    /// Load Whisper from a directory containing `config.json`, `tokenizer.json`,
    /// and `model.safetensors`.
    pub fn load(model_dir: &Path) -> Result<Self, String> {
        let config_path = model_dir.join("config.json");
        let tokenizer_path = model_dir.join("tokenizer.json");
        let weights_path = model_dir.join("model.safetensors");

        for p in [&config_path, &tokenizer_path, &weights_path] {
            if !p.exists() {
                return Err(format!("Whisper file missing: {:?}", p));
            }
        }

        // --- Config ---
        let config_bytes =
            std::fs::read(&config_path).map_err(|e| format!("read config: {}", e))?;
        let config: Config =
            serde_json::from_slice(&config_bytes).map_err(|e| format!("parse config: {}", e))?;

        // --- Tokenizer ---
        let tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| format!("load tokenizer: {}", e))?;

        // --- Device (prefer CUDA when the `cuda` feature is on) ---
        let device = pick_device();
        // F16 on CUDA: the HF Whisper-large-v3-turbo safetensors are
        // already fp16 on disk (so no precision loss casting back),
        // and candle-cuda 0.8 has no F32 kernel for conv1d — feeding
        // F32 weights into the audio encoder panics with "no kernel
        // found for F32 conv1d". F32 stays the default on CPU where
        // the F16 ops are mostly software-emulated and slow.
        let model_dtype = match device {
            Device::Cuda(_) => candle_core::DType::F16,
            _ => m::DTYPE,
        };
        println!(
            "[Whisper] Candle device: {:?}, dtype: {:?}, mel bins: {}",
            device, model_dtype, config.num_mel_bins
        );

        // --- Mel filter bank ---
        let mel_filters = mel_filter_bank(m::SAMPLE_RATE as f64, m::N_FFT, config.num_mel_bins);

        // --- Model weights ---
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&weights_path], model_dtype, &device)
                .map_err(|e| format!("mmap safetensors: {}", e))?
        };
        let model = Whisper::load(&vb, config).map_err(|e| format!("Whisper::load: {}", e))?;

        // --- Special token ids ---
        let sot = token_id(&tokenizer, m::SOT_TOKEN)?;
        let lang_en = token_id(&tokenizer, LANG_EN_TOKEN)?;
        let transcribe = token_id(&tokenizer, m::TRANSCRIBE_TOKEN)?;
        let no_timestamps = token_id(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?;
        let eot = token_id(&tokenizer, m::EOT_TOKEN)?;
        // Optional tokens — every modern HF Whisper has them, but the
        // module degrades gracefully if a custom checkpoint is missing
        // either one.
        let no_speech = m::NO_SPEECH_TOKENS
            .iter()
            .find_map(|t| tokenizer.token_to_id(t));
        let sop = tokenizer.token_to_id(SOP_TOKEN);
        let initial_prompt_ids: Vec<u32> = if sop.is_some() {
            match tokenizer.encode(INITIAL_PROMPT_TEXT, false) {
                Ok(enc) => {
                    let ids = enc.get_ids();
                    // Whisper caps prompt at n_text_ctx/2 - 1 = 223
                    // tokens. Defensive cap for hypothetical longer
                    // prompts; our default is ~14 tokens.
                    const MAX_PROMPT_TOKENS: usize = 223;
                    if ids.len() > MAX_PROMPT_TOKENS {
                        ids[ids.len() - MAX_PROMPT_TOKENS..].to_vec()
                    } else {
                        ids.to_vec()
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[Whisper] initial-prompt tokenize failed: {} — running without prompt",
                        e
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        if no_speech.is_none() {
            eprintln!("[Whisper] tokenizer has no <|nospeech|>/<|nocaptions|> token — no_speech filter disabled");
        }
        if sop.is_none() {
            eprintln!("[Whisper] tokenizer has no <|startofprev|> token — initial-prompt anchoring disabled");
        }

        Ok(Self {
            model: parking_lot::Mutex::new(model),
            tokenizer,
            mel_filters,
            device,
            model_dtype,
            sot,
            lang_en,
            transcribe,
            no_timestamps,
            eot,
            no_speech,
            sop,
            initial_prompt_ids,
        })
    }

    /// Resolve a BCP-47 language hint to the right `<|xx|>` token id
    /// in the Whisper tokenizer. Empty / unknown hints fall back to
    /// English, matching the previous hardcoded behaviour. Whisper
    /// large-v3-turbo (the default checkpoint) is multilingual, so
    /// any language token present in the tokenizer is fair game.
    fn lang_token_for(&self, hint: &str) -> u32 {
        let trimmed = hint.trim();
        if trimmed.is_empty() {
            return self.lang_en;
        }
        // Take the BCP-47 language subtag — "en-AU" / "en_AU" → "en".
        let base = trimmed
            .split(|c: char| c == '-' || c == '_')
            .next()
            .unwrap_or(trimmed)
            .to_ascii_lowercase();
        if base == "en" {
            return self.lang_en;
        }
        match self.tokenizer.token_to_id(&format!("<|{}|>", base)) {
            Some(id) => id,
            None => {
                eprintln!(
                    "[Whisper] language hint {:?} not in tokenizer — falling back to English",
                    hint
                );
                self.lang_en
            }
        }
    }

    /// Transcribe a mono f32 audio buffer at 16 kHz. Blocking on CPU/GPU
    /// compute — call from a tokio `spawn_blocking` context.
    pub fn transcribe(&self, samples_16k: &[f32], language: &str) -> Result<String, String> {
        if samples_16k.is_empty() {
            return Ok(String::new());
        }

        // Pad or trim to 30s (Whisper's fixed input window).
        let mut padded: Vec<f32> = Vec::with_capacity(m::N_SAMPLES);
        if samples_16k.len() >= m::N_SAMPLES {
            padded.extend_from_slice(&samples_16k[..m::N_SAMPLES]);
        } else {
            padded.extend_from_slice(samples_16k);
            padded.resize(m::N_SAMPLES, 0.0);
        }

        // PCM → mel spectrogram. Both `pcm_to_mel` and `log_mel_spectrogram_`
        // in candle produced 4500 frames for 30 s of audio on this
        // Whisper-v3-turbo config (effective hop ≈ 107 samples) instead
        // of the 3000 (10 ms hop) the encoder's 1500-slot positional
        // embedding expects. Hand-rolling the STFT with Whisper's canonical
        // parameters fixes both the frame count and the value scaling.
        let mut model = self.model.lock();
        let num_mel_bins = model.config.num_mel_bins;
        let mel_flat = compute_whisper_mel(&padded, &self.mel_filters, num_mel_bins);
        let mel_frames = mel_flat.len() / num_mel_bins;
        let mel = Tensor::from_vec(mel_flat, (1, num_mel_bins, mel_frames), &self.device)
            .map_err(|e| format!("mel tensor: {}", e))?
            .to_dtype(self.model_dtype)
            .map_err(|e| format!("mel to_dtype: {}", e))?;

        // Reset KV cache for a fresh utterance (important — the last call's
        // kv state is not valid for new audio).
        model.reset_kv_cache();

        // Encoder forward.
        let audio_features = model
            .encoder
            .forward(&mel, true)
            .map_err(|e| format!("encoder: {}", e))?;

        // Decoder prefix: optional [SOP, <prompt>...] anchor, then the
        // mandatory [SOT, <|lang|>, TRANSCRIBE, NO_TIMESTAMPS] block.
        // The prompt anchors Whisper toward a phone-conversation
        // distribution and away from training-data hallucinations
        // ("1.", "Thank you.", YouTube-style filler) on short or quiet
        // clips. The language token honours `SttRequest::language`; an
        // empty / unknown hint falls back to English.
        let lang_token = self.lang_token_for(language);
        let mut tokens: Vec<u32> = Vec::with_capacity(self.initial_prompt_ids.len() + 8);
        let mut sot_index: usize = 0;
        if let Some(sop) = self.sop {
            if !self.initial_prompt_ids.is_empty() {
                tokens.push(sop);
                tokens.extend_from_slice(&self.initial_prompt_ids);
                sot_index = tokens.len();
            }
        }
        let prefix_len = sot_index + 4; // SOT, lang, transcribe, no_timestamps
        tokens.extend_from_slice(&[self.sot, lang_token, self.transcribe, self.no_timestamps]);
        let max_tokens = m::N_FRAMES; // safely bounded; Whisper caps at 448

        for i in 0..max_tokens.saturating_sub(tokens.len()) {
            let input = Tensor::new(&tokens[..], &self.device)
                .map_err(|e| format!("token tensor: {}", e))?
                .unsqueeze(0)
                .map_err(|e| format!("unsqueeze: {}", e))?;

            let out = model
                .decoder
                .forward(&input, &audio_features, i == 0)
                .map_err(|e| format!("decoder: {}", e))?;

            // First step: read no_speech_prob from the SOT-position
            // softmax distribution and bail out if Whisper thinks this
            // clip is silent. Catches the "VAD triggered on noise"
            // case before the decoder gets a chance to invent text.
            // The SOT-position output is the prediction *for what
            // follows SOT* — `<|nospeech|>` is the canonical signal
            // that no speech is present.
            if i == 0 {
                if let Some(no_speech_id) = self.no_speech {
                    let sot_slot = out
                        .narrow(1, sot_index, 1)
                        .map_err(|e| format!("no_speech narrow: {}", e))?;
                    let sot_logits = model
                        .decoder
                        .final_linear(&sot_slot)
                        .map_err(|e| format!("no_speech final_linear: {}", e))?;
                    let prob = no_speech_prob_from(&sot_logits, no_speech_id)?;
                    if prob as f64 > m::NO_SPEECH_THRESHOLD {
                        eprintln!(
                            "[Whisper] dropped: no_speech_prob={:.3} > {:.2}",
                            prob,
                            m::NO_SPEECH_THRESHOLD
                        );
                        return Ok(String::new());
                    }
                }
            }

            // Take the last-timestep hidden state and project to vocab.
            // `final_linear` tied-weights path broadcasts the token embedding
            // matrix to [batch, hidden, vocab] (3D), so the input must also
            // be 3D — we use `narrow` to keep the sequence dim as size-1
            // instead of `.i()` (which rank-reduces to [batch, hidden]).
            let (_b, seq_len, _v) = out.dims3().map_err(|e| format!("out dims: {}", e))?;
            let last = out
                .narrow(1, seq_len - 1, 1)
                .map_err(|e| format!("last step: {}", e))?; // [1, 1, hidden]
            let logits = model
                .decoder
                .final_linear(&last)
                .map_err(|e| format!("final linear: {}", e))?; // [1, 1, vocab]
            let next = argmax_last_dim(&logits)?;
            if next == self.eot {
                break;
            }
            tokens.push(next);
        }

        // Decode text, skipping the prompt + forced prefix.
        let text_tokens: Vec<u32> = tokens.into_iter().skip(prefix_len).collect();
        let text = self
            .tokenizer
            .decode(&text_tokens, true)
            .map_err(|e| format!("decode: {}", e))?;
        let trimmed = text.trim().to_string();
        // Diagnostic: empty transcripts on what is clearly speech
        // (per-segment WAV dumps verified intelligible audio in the
        // RX path) point at the encoder seeing the buffer as silence
        // or the decoder hitting EOT after the forced prefix. Log
        // the audio energy + emitted token count + raw text so we can
        // tell those cases apart on the next run.
        if trimmed.is_empty() {
            let n = samples_16k.len();
            let peak = samples_16k.iter().fold(0.0_f32, |acc, &s| acc.max(s.abs()));
            let rms = if n > 0 {
                let sum_sq: f64 = samples_16k.iter().map(|&s| (s as f64) * (s as f64)).sum();
                (sum_sq / n as f64).sqrt() as f32
            } else {
                0.0
            };
            eprintln!(
                "[Whisper] empty transcript: in={} samples peak={:.3} rms={:.3} emitted_tokens={} raw_text={:?}",
                n,
                peak,
                rms,
                text_tokens.len(),
                text
            );
        }
        Ok(trimmed)
    }
}

/// Compute Whisper's log-mel spectrogram. Mirrors `openai/whisper`'s
/// `log_mel_spectrogram` exactly so the features Whisper sees match what
/// it was trained on:
///
///   * **Periodic** Hann-400 window (the PyTorch default — `cos(2πn/N)`,
///     not `cos(2πn/(N-1))`).
///   * `center=True` reflection padding by `n_fft/2` on each side before
///     framing (also the torch.stft default).
///   * Drop the final frame (matches Python's `stft[..., :-1]`), leaving
///     exactly 3000 frames per 30 s input.
///   * Power spectrum → slaney mel filter bank → `log10(max(·, 1e-10))`.
///   * Post-norm `max(log, log_max - 8)` then `(log + 4) / 4`.
///
/// Output is row-major `[n_mels, n_frames]` with 3000 frames for 30 s of
/// input at 16 kHz.
fn compute_whisper_mel(samples_padded: &[f32], filters: &[f32], n_mels: usize) -> Vec<f32> {
    use rustfft::{num_complex::Complex, FftPlanner};

    const N_FFT: usize = 400;
    const HOP: usize = 160;
    let n_spec = N_FFT / 2 + 1; // 201
    let pad = N_FFT / 2; // 200 — center=True reflection pad per side

    // Periodic Hann (divide by N_FFT, not N_FFT-1). This is the default
    // `torch.hann_window(N_FFT)` outputs and what torch.stft uses.
    let hann: Vec<f32> = (0..N_FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N_FFT as f32).cos())
        .collect();

    // Reflection-pad the samples: `[s_pad, s_{pad-1}, …, s_1, s_0, s_1, …, s_{N-2}, s_{N-1}, s_{N-2}, …]`.
    let n = samples_padded.len();
    let mut padded = Vec::<f32>::with_capacity(n + 2 * pad);
    for i in 0..pad {
        // mirror around index 0 — skip the sample at index 0 itself
        // (standard reflect-mode).
        padded.push(samples_padded[pad - i]);
    }
    padded.extend_from_slice(samples_padded);
    for i in 0..pad {
        // mirror around index n-1, again skipping the endpoint.
        padded.push(samples_padded[n - 2 - i]);
    }

    // Frames = 1 + (padded_len - N_FFT) / HOP. For 480000 samples
    // reflection-padded to 480400 this gives 3001; we drop the last to
    // match Whisper's `stft[..., :-1]`.
    let total_frames = if padded.len() >= N_FFT {
        (padded.len() - N_FFT) / HOP + 1
    } else {
        0
    };
    let n_frames = total_frames.saturating_sub(1);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(N_FFT);
    let mut buf = vec![Complex::<f32>::new(0.0, 0.0); N_FFT];

    // [n_mels, n_frames] row-major, matching the `[n_mel, n_frames]`
    // layout the encoder input expects (after being wrapped as
    // `(1, n_mel, n_frames)` by the caller).
    let mut mel = vec![0.0f32; n_mels * n_frames];

    for f in 0..n_frames {
        let start = f * HOP;
        for i in 0..N_FFT {
            let s = padded[start + i] * hann[i];
            buf[i] = Complex::new(s, 0.0);
        }
        fft.process(&mut buf);
        for m in 0..n_mels {
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

    let log_max = mel.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let floor = log_max - 8.0;
    for v in mel.iter_mut() {
        if *v < floor {
            *v = floor;
        }
        *v = (*v + 4.0) / 4.0;
    }
    mel
}

fn token_id(tokenizer: &Tokenizer, tok: &str) -> Result<u32, String> {
    tokenizer
        .token_to_id(tok)
        .ok_or_else(|| format!("tokenizer missing special token: {}", tok))
}

/// Compute the softmax probability of a single vocab id from a
/// `[1, 1, vocab]` logits tensor. Done manually (rather than via
/// candle_nn::ops::softmax) so we don't pull in another dependency
/// module just for one scalar lookup; the vocab is ~51k so the
/// linear pass is trivial cost compared to the encoder we just ran.
fn no_speech_prob_from(logits: &Tensor, no_speech_id: u32) -> Result<f32, String> {
    let logits = logits
        .to_dtype(candle_core::DType::F32)
        .map_err(|e| format!("no_speech to_dtype: {}", e))?;
    let v = logits
        .flatten_all()
        .map_err(|e| format!("no_speech flatten: {}", e))?
        .to_vec1::<f32>()
        .map_err(|e| format!("no_speech vec: {}", e))?;
    let max_logit = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let target = no_speech_id as usize;
    if target >= v.len() {
        return Ok(0.0);
    }
    let mut sum = 0.0_f32;
    for &logit in v.iter() {
        sum += (logit - max_logit).exp();
    }
    if sum > 0.0 {
        Ok((v[target] - max_logit).exp() / sum)
    } else {
        Ok(0.0)
    }
}

#[allow(dead_code)]
fn argmax_u32(logits: &Tensor) -> Result<u32, String> {
    // legacy 2D helper — kept in case we need it again.
    let logits = logits.squeeze(0).map_err(|e| format!("squeeze: {}", e))?;
    let idx = logits
        .argmax(0)
        .map_err(|e| format!("argmax: {}", e))?
        .to_scalar::<u32>()
        .map_err(|e| format!("scalar: {}", e))?;
    Ok(idx)
}

/// Argmax on the last dimension of a 3D logits tensor `[1, 1, vocab]`.
fn argmax_last_dim(logits: &Tensor) -> Result<u32, String> {
    let rank = logits.rank();
    let idx = logits
        .argmax(rank - 1)
        .map_err(|e| format!("argmax: {}", e))?;
    // Flatten any leading singleton dims to get a scalar u32.
    let scalar = idx
        .flatten_all()
        .map_err(|e| format!("flatten argmax: {}", e))?
        .to_vec1::<u32>()
        .map_err(|e| format!("to_vec argmax: {}", e))?;
    scalar
        .first()
        .copied()
        .ok_or_else(|| "argmax returned empty".to_string())
}

fn pick_device() -> Device {
    #[cfg(feature = "cuda")]
    {
        if let Ok(d) = Device::new_cuda(0) {
            return d;
        }
    }
    Device::Cpu
}

pub struct CandleWhisperState {
    pub instance: Arc<Mutex<Option<CandleWhisperRuntime>>>,
}

impl Default for CandleWhisperState {
    fn default() -> Self {
        Self {
            instance: Arc::new(Mutex::new(None)),
        }
    }
}

pub fn candle_whisper_models_dir(
    app_data_dir: &Path,
    resource_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    // Reads come from the bundled resource dir when the installer
    // shipped pre-staged weights, and from app_data otherwise. The
    // download path explicitly targets app_data via
    // `bundled_models::app_data_models_dir` so a partial bundle never
    // permission-faults into the read-only resource folder.
    crate::bundled_models::locate_models_dir(
        app_data_dir,
        resource_dir,
        crate::bundled_models::BundleRole::Whisper,
    )
}

/// Path to the file we treat as proof-of-presence for the Whisper
/// bundle. We moved off the candle safetensors path (driver 596+
/// PTX JIT failure on candle 0.8 cast kernels) onto the sherpa-onnx
/// ORT bundle, so the sentinel is now the encoder ONNX rather than
/// the safetensors. Symbol kept under its old name so the download
/// helpers compile against the same import.
pub fn candle_whisper_model_path(
    app_data_dir: &Path,
    resource_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    Ok(candle_whisper_models_dir(app_data_dir, resource_dir)?.join("turbo-encoder.int8.onnx"))
}
