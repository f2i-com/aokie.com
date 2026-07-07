//! Qwen3-ASR-0.6B (Alibaba) encoder-decoder ASR ported from
//! `andrewleech/qwen3-asr-onnx` (the export-tool reference Python).
//!
//! Architecture: a Whisper-style log-mel front-end feeds an
//! audio-tower encoder; encoder outputs are scattered into a Qwen3
//! decoder prompt at the `<|audio_pad|>` slots; greedy decode runs
//! through `decoder_init` (prefill, owns input_ids) then
//! `decoder_step` (autoregressive, takes pre-looked-up `input_embeds`)
//! until EOS.
//!
//! The published int4 ONNX bundle is ~2 GB on disk
//! (encoder 746 MB + decoder weights 962 MB + embed_tokens 311 MB +
//! tokenizer 11 MB), but greedy decode runs at low single-digit
//! seconds per utterance on CPU thanks to int4 MatMul quantization.
//!
//! This is a `qwen3-asr` cargo feature gate so the heavy dep tree
//! (HF `tokenizers`, `memmap2`) only lands when the engine is wanted.

mod mel;
mod model;

pub use mel::compute_log_mel;
pub use model::{Qwen3AsrModel, Qwen3AsrParams, Qwen3AsrVariant};
