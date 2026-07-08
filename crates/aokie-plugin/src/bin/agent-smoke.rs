//! Standalone smoke test for the in-plugin streaming LLM agent. Discovers the
//! local LLM (llama.cpp/ollama), streams a receptionist reply, and prints each
//! sentence with its timing — so we can verify sentence-by-sentence streaming +
//! time-to-first-sentence against the real model before wiring it to a call.
//!
//!   cargo run -p aokie-plugin --features voice --bin agent-smoke

#[cfg(feature = "voice")]
fn main() {
    let ep = match aokie_plugin::agent::discover_endpoint(None) {
        Some(e) => e,
        None => {
            eprintln!("[agent-smoke] no local LLM reachable on :8080 / :11434");
            std::process::exit(1);
        }
    };
    eprintln!("[agent-smoke] endpoint: {ep}");
    let client = aokie_plugin::agent::LlmClient::new(ep, None);
    eprintln!("[agent-smoke] model: {:?}", client.model());

    for user in [
        "Hi, can you hear me?",
        "Great — are you open on Sunday, and how late on weekdays?",
    ] {
        let messages = serde_json::json!([
            {"role": "system", "content": "You are a warm phone receptionist for a dental clinic. Reply with ONE short spoken sentence."},
            {"role": "user", "content": user}
        ]);
        let t0 = std::time::Instant::now();
        eprintln!("\n[agent-smoke] user: {user:?}");
        let full = client
            .stream_reply(messages, |s| {
                eprintln!("  [+{:?}] sentence: {s:?}", t0.elapsed());
                true
            })
            .unwrap_or_else(|e| {
                eprintln!("[agent-smoke] FAILED: {e}");
                String::new()
            });
        eprintln!("  full reply ({:?}): {full:?}", t0.elapsed());
    }
}

#[cfg(not(feature = "voice"))]
fn main() {
    eprintln!("[agent-smoke] built without the `voice` feature");
}
