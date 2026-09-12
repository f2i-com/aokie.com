//! ONNX-Runtime-GenAI-style multi-modal LLM runtime. The default
//! bundled model is Gemma 4 E4B-it (`onnx-community/gemma-4-E4B-it-ONNX`)
//! but the loader / decode loop are model-agnostic apart from the
//! chat-template tokens hardcoded at the top of this file — those
//! move to config in Phase 2e so a different model with the same
//! 3-graph layout (Phi-3-mini ONNX, Llama-3-8B ONNX, etc.) can plug
//! in by editing `<app_data>/ai_providers.json`.
//!
//! Three cooperating ONNX graphs (this is the GenAI shape — common
//! to all multi-modal exports we'd target):
//!
//! | Graph                         | Role                                     |
//! |-------------------------------|------------------------------------------|
//! | `audio_encoder_q4f16.onnx`    | raw f32 mono audio → audio-soft-embeds   |
//! | `embed_tokens_q4f16.onnx`     | token ids → text embeds                  |
//! | `decoder_model_merged_q4f16`  | embeds (+ KV cache) → next-token logits  |
//!
//! Chat format follows the Gemma 4 template (see `chat_template.jinja`
//! in the bundled repo) — the special tokens are private constants
//! in this file and are the things Phase 2e will lift into config:
//!
//! ```text
//! <bos><|turn>user
//! {prompt}<turn|>
//! <|turn>model
//! ```
//!
//! Audio is injected into the user turn at the `<|audio|>` placeholder —
//! the audio path is gated behind `generate_stream_with_audio` which
//! splices the audio-encoder output into the embedding stream before
//! the shared decode loop runs.

mod audio;
mod decode;

pub use audio::{compute_mel, run_audio_encoder, splice_audio};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use ort::session::{builder::GraphOptimizationLevel, Session};
use tokenizers::Tokenizer;

use crate::config::ChatTemplate;
use decode::{decode_loop, DecodeConfig};

const DEFAULT_VARIANT: &str = "q4f16";

/// Bundled fallback chat template — Gemma 4 E4B-it. Used when
/// `LlmProviderConfig.chat_template` is absent (e.g. an old
/// override JSON missing the field). Bundled `providers.default.json`
/// always carries the explicit template; this keeps a `load()` from
/// crashing on a partial override.
fn default_template() -> ChatTemplate {
    ChatTemplate {
        start_of_turn: "<|turn>".to_string(),
        end_of_turn: "<turn|>".to_string(),
        end_of_sentence: "<eos>".to_string(),
        audio_token: "<|audio|>".to_string(),
        begin_of_sentence: "<bos>".to_string(),
    }
}

/// One turn of input.
#[derive(Debug, Clone)]
pub enum PromptSegment {
    /// Assistant or user text, wrapped in `<start_of_turn>role ... <end_of_turn>`.
    Text { role: String, text: String },
    /// Mono f32 audio @ 16 kHz to be encoded via audio_encoder.onnx and
    /// spliced into the most recent user turn at runtime.
    Audio(Vec<f32>),
}

pub struct OnnxGenAiRuntime {
    #[allow(dead_code)]
    root: PathBuf,
    pub(crate) tokenizer: Tokenizer,
    pub(crate) audio_encoder: Session,
    pub(crate) embed_tokens: Session,
    pub(crate) decoder: Session,
    /// Per-layer KV cache dimensions `(num_kv_heads, head_dim)`,
    /// introspected from each `past_key_values.N.value` input's static
    /// dims at load time. Different Gemma 3n variants have different
    /// shapes here — E4B uses (2 heads, 256 head_dim) on local-attn
    /// layers and (2 heads, 512) on global; E2B uses (1 head, 512).
    /// Storing per-layer means we can also handle hypothetical exports
    /// that mix dimensions across layers without a special case.
    pub(crate) kv_dims: Vec<(usize, usize)>,
    /// End-of-turn token id (model stops generating at this).
    pub(crate) end_of_turn_id: u32,
    /// End-of-sentence token id.
    pub(crate) eos_id: u32,
    /// Special tokens used to render the chat template + locate the
    /// audio placeholder during multimodal generation.
    pub(crate) template: ChatTemplate,
}

impl OnnxGenAiRuntime {
    /// Load with the bundled-default chat template. Convenience for
    /// callers that don't have a config in scope; production code
    /// goes through `load_with_template`.
    pub fn load(root: &Path) -> Result<Self, String> {
        Self::load_with_template(root, default_template())
    }

    pub fn load_with_template(root: &Path, template: ChatTemplate) -> Result<Self, String> {
        println!("[Gemma4] Loading ONNX sessions from {:?}", root);

        // Operator escape hatch: setting `AOKIE_GEMMA_CPU=1` pins every
        // Gemma session to the CPU EP regardless of `cuda` feature or
        // nosoftcap availability. Useful when the user's CUDA stack is
        // broken (driver bug, mismatched cuDNN, runtime DLL conflict)
        // and they need a working LLM while diagnosing — without this,
        // a silently-crashing CUDA load would leave them with no LLM
        // at all. Reads the env var once per load; flipping it requires
        // an app restart, which is fine for a recovery path.
        let force_cpu = std::env::var("AOKIE_GEMMA_CPU")
            .map(|v| !v.is_empty() && v != "0" && v.to_ascii_lowercase() != "false")
            .unwrap_or(false);
        if force_cpu {
            println!("[Gemma4] AOKIE_GEMMA_CPU set — forcing CPU EP on all sessions");
        }
        let preferred_ep = if force_cpu {
            Ep::CpuOnly
        } else {
            Ep::PreferCuda
        };

        let onnx_dir = root.join("onnx");
        let tokenizer_path = root.join("tokenizer.json");

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| format!("Failed to load tokenizer.json: {}", e))?;

        let audio_encoder = build_session(
            &onnx_dir.join(format!("audio_encoder_{}.onnx", DEFAULT_VARIANT)),
            preferred_ep,
        )?;
        let embed_tokens = build_session(
            &onnx_dir.join(format!("embed_tokens_{}.onnx", DEFAULT_VARIANT)),
            preferred_ep,
        )?;
        // The decoder uses GroupQueryAttention with an `attention_bias` input
        // (Gemma's softcapping). ORT 1.25's CUDA GQA kernel rejects that,
        // so we prefer a pre-stripped `*_nosoftcap.onnx` variant produced by
        // `scripts/strip_gqa_attention_bias.py` — identical weights, just
        // without the softcap input on the 20 GQA nodes. Run that script
        // once (after downloading the model) to enable full CUDA decode.
        let decoder_nosoftcap = onnx_dir.join(format!(
            "decoder_model_merged_{}_nosoftcap.onnx",
            DEFAULT_VARIANT
        ));
        let (decoder_path, decoder_ep) = if force_cpu {
            // CPU override takes precedence — a stripped decoder isn't
            // useful when we're not running on CUDA anyway.
            (
                onnx_dir.join(format!("decoder_model_merged_{}.onnx", DEFAULT_VARIANT)),
                Ep::CpuOnly,
            )
        } else if decoder_nosoftcap.exists() {
            println!("[Gemma4] using stripped-softcap decoder for CUDA");
            (decoder_nosoftcap, Ep::PreferCuda)
        } else {
            println!(
                "[Gemma4] nosoftcap decoder not found — falling back to CPU. \
                 Run `python scripts/strip_gqa_attention_bias.py` to enable CUDA."
            );
            (
                onnx_dir.join(format!("decoder_model_merged_{}.onnx", DEFAULT_VARIANT)),
                Ep::CpuOnly,
            )
        };
        let decoder = build_session(&decoder_path, decoder_ep)?;

        // Introspect the decoder's inputs to discover (a) how many
        // past_key_values.{i}.key entries are expected — that's the
        // layer count — and (b) the static `(num_kv_heads, head_dim)`
        // for each layer's `.value` input. Different Gemma 3n variants
        // have different KV geometry (E4B is 2 KV heads with mixed
        // 256/512 head_dim; E2B is 1 KV head with uniform 512), and
        // hardcoding either set crashes the other variant during
        // prefill with "Got invalid dimensions for input:
        // past_key_values.N.value". Per-layer introspection avoids the
        // mismatch without a per-variant code path.
        let mut kv_dims_by_idx: std::collections::BTreeMap<usize, (usize, usize)> =
            std::collections::BTreeMap::new();
        for input in decoder.inputs() {
            let name = input.name();
            let Some(rest) = name.strip_prefix("past_key_values.") else {
                continue;
            };
            let Some(idx_str) = rest.strip_suffix(".value") else {
                continue;
            };
            let Ok(idx) = idx_str.parse::<usize>() else {
                continue;
            };
            if let ort::value::ValueType::Tensor { shape, .. } = input.dtype() {
                // KV cache shape is `[batch, num_kv_heads, seq, head_dim]`.
                // batch and seq are dynamic (-1); the other two should
                // be concrete positive ints. Refuse to use anything
                // dynamic — better to fail loud than to feed a
                // mis-shaped tensor and corrupt the decode.
                if shape.len() >= 4 && shape[1] > 0 && shape[3] > 0 {
                    kv_dims_by_idx.insert(idx, (shape[1] as usize, shape[3] as usize));
                }
            }
        }
        let n_layers = kv_dims_by_idx.len();
        if n_layers == 0 {
            return Err(
                "decoder exposes no past_key_values inputs — is this a non-merged export?"
                    .to_string(),
            );
        }
        // Densify into a Vec ordered by layer index. The introspection
        // refuses to populate slots whose shape was dynamic, so a gap
        // here indicates an export we can't safely drive — fail loud
        // rather than silently miscount layers.
        let mut kv_dims: Vec<(usize, usize)> = Vec::with_capacity(n_layers);
        for i in 0..n_layers {
            let dims = kv_dims_by_idx.get(&i).copied().ok_or_else(|| {
                format!(
                    "decoder past_key_values.{}.value: missing or dynamic shape — \
                     can't determine KV head count / head dim",
                    i
                )
            })?;
            kv_dims.push(dims);
        }
        println!(
            "[Gemma4] decoder geometry: {} layer(s); per-layer (num_kv_heads, head_dim) = {:?}",
            n_layers, kv_dims
        );

        let end_of_turn_id = tokenizer
            .token_to_id(&template.end_of_turn)
            .ok_or_else(|| format!("tokenizer missing special token: {}", template.end_of_turn))?;
        let eos_id = tokenizer
            .token_to_id(&template.end_of_sentence)
            .ok_or_else(|| {
                format!(
                    "tokenizer missing special token: {}",
                    template.end_of_sentence
                )
            })?;

        // Quick sanity: every token the chat template references must
        // exist in the tokenizer. Validating up-front means a typo in
        // ai_providers.json surfaces at load with a clear error
        // ("tokenizer missing special token: <|audi|>") instead of
        // crashing later on the first multimodal turn.
        if tokenizer.token_to_id(&template.start_of_turn).is_none() {
            return Err(format!(
                "tokenizer missing special token: {}",
                template.start_of_turn
            ));
        }
        if tokenizer.token_to_id(&template.audio_token).is_none() {
            return Err(format!(
                "tokenizer missing special token: {}",
                template.audio_token
            ));
        }
        if !template.begin_of_sentence.is_empty()
            && tokenizer.token_to_id(&template.begin_of_sentence).is_none()
        {
            return Err(format!(
                "tokenizer missing special token: {}",
                template.begin_of_sentence
            ));
        }

        println!(
            "[Gemma4] Loaded. n_layers={} end_of_turn={} eos={}",
            n_layers, end_of_turn_id, eos_id
        );

        Ok(Self {
            root: root.to_path_buf(),
            tokenizer,
            audio_encoder,
            embed_tokens,
            decoder,
            kv_dims,
            end_of_turn_id,
            eos_id,
            template,
        })
    }

    /// Text-only streaming generation. The segments are rendered using the
    /// Gemma chat template, tokenized, embedded, and fed through the decoder
    /// with a KV cache.
    pub fn generate_stream<F>(
        &mut self,
        segments: &[PromptSegment],
        max_new_tokens: usize,
        temperature: f32,
        on_token: F,
    ) -> Result<String, String>
    where
        F: FnMut(&str) -> bool,
    {
        // Text-only path — any Audio segments are silently dropped here.
        // Use `generate_stream_with_audio` for multimodal input.
        let prompt = self.render_chat(segments);
        let input_ids = self
            .tokenizer
            .encode(prompt.as_str(), false)
            .map_err(|e| format!("tokenize: {}", e))?
            .get_ids()
            .to_vec();

        decode_loop(
            self,
            &input_ids,
            None,
            DecodeConfig {
                max_new_tokens,
                temperature,
            },
            on_token,
        )
    }

    /// Multimodal streaming generation: accepts a mix of Text turns and at
    /// most one Audio segment (which is spliced into the most recent user
    /// turn). Audio is encoded via `audio_encoder.onnx` and its features
    /// replace the embeddings of a run of `<|audio|>` placeholder tokens.
    pub fn generate_stream_with_audio<F>(
        &mut self,
        segments: &[PromptSegment],
        max_new_tokens: usize,
        temperature: f32,
        on_token: F,
    ) -> Result<String, String>
    where
        F: FnMut(&str) -> bool,
    {
        // Pull the single audio segment out (if any). More than one audio
        // segment isn't supported in this first pass.
        let audio_samples: Option<&[f32]> = segments.iter().find_map(|s| match s {
            PromptSegment::Audio(v) => Some(v.as_slice()),
            _ => None,
        });

        let Some(samples) = audio_samples else {
            // No audio — degrade to the text-only path.
            return self.generate_stream(segments, max_new_tokens, temperature, on_token);
        };

        // Sanity stats on what the user's mic actually delivered to Gemma.
        // Expected: f32 in [-1, 1] at 16 kHz. If peak is ~0 the audio is
        // silence (or normalized wrong); if peak ≫ 1 we're receiving i16
        // values by mistake and the mel will be nonsense.
        let n = samples.len();
        let mut peak = 0.0f32;
        let mut sum_sq = 0.0f32;
        for &s in samples {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
            sum_sq += s * s;
        }
        let rms = if n > 0 {
            (sum_sq / n as f32).sqrt()
        } else {
            0.0
        };
        println!(
            "[Gemma4] audio in: {} samples ({:.2} s @ 16 kHz), peak={:.3}, rms={:.4}",
            n,
            n as f32 / 16_000.0,
            peak,
            rms
        );
        if peak > 1.5 {
            eprintln!(
                "[Gemma4] WARNING: audio peak {:.2} > 1.5 — samples likely not in [-1, 1]. \
                 Gemma expects f32 PCM normalized to [-1, 1]; the mel will be meaningless.",
                peak
            );
        }
        if peak < 0.01 {
            eprintln!(
                "[Gemma4] WARNING: audio peak {:.4} < 0.01 — near-silent input. Gemma may \
                 hallucinate a response since it can't hear the speech.",
                peak
            );
        }

        // 1. Compute mel spectrogram + run through audio_encoder → features [K, 2560].
        let mel = audio::compute_mel(samples);
        let features = audio::run_audio_encoder(self, mel)?;
        let k = features.shape()[0];

        // Mel sanity stats — log-mel values are typically in [-10, 0]. If
        // we see all-zeros or wildly out-of-range values the filter bank
        // or scaling is off.
        let mut mel_min = f32::INFINITY;
        let mut mel_max = f32::NEG_INFINITY;
        let mut mel_sum = 0.0f32;
        let mut mel_count = 0usize;
        for v in features.iter() {
            if *v < mel_min {
                mel_min = *v;
            }
            if *v > mel_max {
                mel_max = *v;
            }
            mel_sum += *v;
            mel_count += 1;
        }
        let mel_mean = if mel_count > 0 {
            mel_sum / mel_count as f32
        } else {
            0.0
        };
        println!(
            "[Gemma4] {} audio tokens, features [{}x{}] min={:.2} max={:.2} mean={:.2}",
            k,
            features.shape()[0],
            features.shape()[1],
            mel_min,
            mel_max,
            mel_mean
        );

        // 2. Build the rendered prompt with K copies of the audio
        //    placeholder in place of each Audio segment.
        let prompt = self.render_chat_with_audio(segments, k);

        // 3. Tokenize and locate the audio region inside the token stream.
        let enc = self
            .tokenizer
            .encode(prompt.as_str(), false)
            .map_err(|e| format!("tokenize: {}", e))?;
        let ids: Vec<u32> = enc.get_ids().to_vec();
        let audio_id = self
            .tokenizer
            .token_to_id(&self.template.audio_token)
            .ok_or_else(|| format!("tokenizer missing {}", self.template.audio_token))?;
        let audio_start = ids
            .iter()
            .position(|&t| t == audio_id)
            .ok_or_else(|| "audio placeholder not found in tokenized prompt".to_string())?;

        // 4. Run embed_tokens over the full prompt (including placeholders).
        let (mut inputs_embeds, per_layer_inputs) = decode::run_embed_tokens(self, &ids)?;

        // 5. Splice the audio_encoder output into the embedding stream.
        inputs_embeds = audio::splice_audio(inputs_embeds, &features, audio_start, k)?;

        // 6. Hand off to the decode loop with the spliced embeds.
        decode_loop(
            self,
            &ids,
            Some((inputs_embeds, per_layer_inputs)),
            DecodeConfig {
                max_new_tokens,
                temperature,
            },
            on_token,
        )
    }
}

impl OnnxGenAiRuntime {
    /// Render for the multimodal path: each `Audio` segment is
    /// emitted as `K` audio-placeholder tokens inside the user turn
    /// so the decoder has room to hold the audio-encoder features
    /// after the splice.
    fn render_chat_with_audio(
        &self,
        segments: &[PromptSegment],
        audio_token_count: usize,
    ) -> String {
        let t = &self.template;
        let mut out = String::new();
        out.push_str(&t.begin_of_sentence);
        // Group consecutive audio segments with the next text turn
        // so the audio appears *inside* the user turn. For our call
        // pipeline the typical pattern is [Audio] then Text user
        // turn; we render as:
        //   <|turn>user\n<audio>(*K)\nTEXT<turn|>
        let mut i = 0usize;
        while i < segments.len() {
            match &segments[i] {
                PromptSegment::Text { role, text } => {
                    out.push_str(&t.start_of_turn);
                    out.push_str(role);
                    out.push('\n');
                    out.push_str(text);
                    out.push_str(&t.end_of_turn);
                    out.push('\n');
                    i += 1;
                }
                PromptSegment::Audio(_) => {
                    // Begin a user turn, splat K audio placeholders,
                    // then consume any immediately-following text
                    // segment into the same turn.
                    out.push_str(&t.start_of_turn);
                    out.push_str("user\n");
                    for _ in 0..audio_token_count {
                        out.push_str(&t.audio_token);
                    }
                    i += 1;
                    if let Some(PromptSegment::Text { text, .. }) = segments.get(i) {
                        out.push('\n');
                        out.push_str(text);
                        i += 1;
                    }
                    out.push_str(&t.end_of_turn);
                    out.push('\n');
                }
            }
        }
        out.push_str(&t.start_of_turn);
        out.push_str("model\n");
        out
    }

    /// Render a list of PromptSegment into the chat template.
    ///
    /// Format (Gemma 4 default):
    /// ```text
    /// <bos><|turn>user
    /// Hello<turn|>
    /// <|turn>model
    /// ```
    fn render_chat(&self, segments: &[PromptSegment]) -> String {
        let t = &self.template;
        let mut out = String::new();
        out.push_str(&t.begin_of_sentence);
        for seg in segments {
            if let PromptSegment::Text { role, text } = seg {
                out.push_str(&t.start_of_turn);
                out.push_str(role);
                out.push('\n');
                out.push_str(text);
                out.push_str(&t.end_of_turn);
                out.push('\n');
            }
        }
        out.push_str(&t.start_of_turn);
        out.push_str("model\n");
        out
    }
}

/// Per-session EP preference.
///
/// - `PreferCuda`: vanilla CUDA EP — fastest for vanilla transformer graphs
/// - `PreferCudaMath`: CUDA EP with the MATH attention backend forced
///   (kept for a future attempt at GQA+attention_bias on CUDA; MATH didn't
///   rescue Gemma 4 but the branch is worth preserving)
/// - `CpuOnly`: last-resort fallback
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // PreferCudaMath + CpuOnly are fallback-only, not always used
enum Ep {
    PreferCuda,
    PreferCudaMath,
    CpuOnly,
}

fn build_session(onnx_path: &Path, ep: Ep) -> Result<Session, String> {
    if !onnx_path.exists() {
        return Err(format!(
            "Gemma 4 ONNX file missing: {:?}. Run `download_gemma4` first.",
            onnx_path
        ));
    }

    // Log the file the runtime is about to commit BEFORE the call.
    // If ORT segfaults during graph compilation the last-printed line
    // names the offending file — without this we'd just see the
    // process die between "[Gemma4] Loading ONNX sessions from …" and
    // nothing else. Sidecar `.onnx_data` blobs (external initializers)
    // are reported alongside so a truncated download surfaces as a
    // size mismatch the operator can compare against the expected
    // hash.
    let onnx_size = std::fs::metadata(onnx_path).map(|m| m.len()).unwrap_or(0);
    let data_path = onnx_path.with_extension("onnx_data");
    let data_size_str = if data_path.exists() {
        format!(
            " + {} ({} bytes)",
            data_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::fs::metadata(&data_path).map(|m| m.len()).unwrap_or(0)
        )
    } else {
        String::new()
    };
    println!(
        "[Gemma4] build_session: {} ({} bytes){} ep={:?}",
        onnx_path.display(),
        onnx_size,
        data_size_str,
        ep
    );
    // Flush so the "about to commit" line lands on disk before
    // commit_from_file runs — if ORT segfaults inside graph
    // compilation, an unflushed line would die in the buffer and the
    // operator's log would just stop without naming the offender.
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // `commit_from_file` (called below at line ~654) takes `&mut self`
    // in ort 2.0.0-rc.12, so the binding must be mutable. In a CUDA
    // build the immediately-following `let mut builder = match ep`
    // shadows this and consumes the first builder via
    // `with_execution_providers(...)`, which makes rustc flag this
    // first `mut` as "unused" — that lint is wrong for non-CUDA
    // builds, where this is the only binding. Keep the `mut`.
    #[allow(unused_mut)]
    let mut builder = Session::builder()
        .map_err(|e| format!("ort builder: {}", e))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| format!("ort opt level: {}", e))?;

    // Register execution providers based on this session's preference.
    #[cfg(feature = "cuda")]
    let mut builder = match ep {
        Ep::PreferCuda => {
            use ort::execution_providers::{CPU, CUDA};
            builder
                .with_execution_providers([
                    CUDA::default().build().error_on_failure(),
                    CPU::default().build(),
                ])
                .map_err(|e| format!("ort eps (CUDA failed to register): {}", e))?
        }
        Ep::PreferCudaMath => {
            use ort::execution_providers::{cuda::AttentionBackend, CPU, CUDA};
            builder
                .with_execution_providers([
                    CUDA::default()
                        .with_attention_backend(AttentionBackend::MATH)
                        .build()
                        .error_on_failure(),
                    CPU::default().build(),
                ])
                .map_err(|e| format!("ort eps (CUDA MATH failed): {}", e))?
        }
        Ep::CpuOnly => {
            use ort::execution_providers::CPU;
            builder
                .with_execution_providers([CPU::default().build()])
                .map_err(|e| format!("ort eps CPU: {}", e))?
        }
    };
    #[cfg(not(feature = "cuda"))]
    let _ = ep;

    let session = builder
        .commit_from_file(onnx_path)
        .map_err(|e| format!("ort commit {:?}: {}", onnx_path, e))?;

    let name = onnx_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| onnx_path.display().to_string());
    let ep_label = match ep {
        Ep::PreferCuda => {
            #[cfg(feature = "cuda")]
            {
                "CUDA"
            }
            #[cfg(not(feature = "cuda"))]
            {
                "CPU"
            }
        }
        Ep::PreferCudaMath => {
            #[cfg(feature = "cuda")]
            {
                "CUDA (MATH attention)"
            }
            #[cfg(not(feature = "cuda"))]
            {
                "CPU"
            }
        }
        Ep::CpuOnly => "CPU (pinned)",
    };
    println!("[Gemma4] {} loaded ({})", name, ep_label);

    Ok(session)
}

pub struct OnnxGenAiState {
    pub instance: Arc<Mutex<Option<OnnxGenAiRuntime>>>,
}

impl Default for OnnxGenAiState {
    fn default() -> Self {
        Self {
            instance: Arc::new(Mutex::new(None)),
        }
    }
}

pub fn onnx_genai_models_dir(
    app_data_dir: &Path,
    resource_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    // The Gemma 4 family ships in two flavours: the default 4B (E4B)
    // and a 2B (E2B) sibling for 6-GB-class GPUs. The active LLM
    // provider config picks one via the `llm.model` field — the
    // dispatcher in `bundled_models::active_gemma_variant` reads it
    // and returns the right `BundleRole`. Reads then route through
    // the bundled-fallback helper so a "full" installer can ship the
    // chosen variant inside `<install>/resources/models/<dir>/` and
    // the runtime picks it up without copying into app_data.
    let role = crate::bundled_models::active_gemma_variant();
    crate::bundled_models::locate_models_dir(app_data_dir, resource_dir, role)
}

#[cfg(test)]
mod gemma_perf_tests {
    use super::*;

    /// Greedy (deterministic) generation A/B harness for the KV-cache
    /// refactor. Greedy == argmax, so the transcript MUST be byte-for-byte
    /// identical before/after the change (the refactor only moves WHERE the
    /// cache lives, not the math). Prints transcript + tok/s. Run with:
    ///   cargo test -p aokie-desktop --lib --no-default-features \
    ///     --features custom-protocol gemma_greedy -- --ignored --nocapture
    #[test]
    #[ignore = "loads the ~11GB Gemma model; run manually for the KV-cache A/B"]
    fn gemma_greedy_generation_ab() {
        if std::env::var_os("ORT_DYLIB_PATH").is_none() {
            std::env::set_var(
                "ORT_DYLIB_PATH",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    r"\resources\onnxruntime_1.25.0.dll"
                ),
            );
        }
        let dir = std::env::var("GEMMA_MODEL_DIR").unwrap_or_else(|_| {
            r"C:\Users\User\AppData\Roaming\com.aokie.app\models\gemma4".to_string()
        });
        let mut rt = OnnxGenAiRuntime::load(std::path::Path::new(&dir)).expect("load gemma");
        let segs = vec![PromptSegment::Text {
            role: "user".into(),
            text: "What is the capital of France? Reply in one short sentence.".into(),
        }];
        let mut count = 0usize;
        let t0 = std::time::Instant::now();
        let out = rt
            .generate_stream(&segs, 24, 0.0, |_| {
                count += 1;
                true
            })
            .expect("generate");
        let dt = t0.elapsed().as_secs_f32();
        println!(
            "GEMMA_AB tokens={count} secs={dt:.2} tok/s={:.2}",
            count as f32 / dt
        );
        println!("GEMMA_AB transcript={out:?}");
        // Greedy (temperature 0) is deterministic, so the device-resident
        // KV-cache refactor must keep producing the correct answer. This is
        // the regression guard for the decode loop.
        assert!(
            out.to_lowercase().contains("paris"),
            "expected the capital of France in the reply, got {out:?}"
        );
    }
}
