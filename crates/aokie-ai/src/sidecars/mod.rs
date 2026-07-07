//! Long-running child processes the app supervises (today: llama.cpp's
//! `llama-server`). Each sidecar owns a `Mutex<Option<Child>>` so the
//! Tauri commands can spawn / kill / probe it without surfacing the
//! process handle to the frontend.
//!
//! Sidecars are intentionally separate from the AI provider trait
//! surface — they're just local servers we manage. The LLM adapter
//! that talks to `llama-server` is the existing `openai-http` one
//! (Phase 2c) pointed at `http://127.0.0.1:<port>/v1`. Decoupling
//! lifecycle from data path means a remote LM Studio / Ollama
//! install works through the same adapter without us needing to know.

pub mod llama_server;
