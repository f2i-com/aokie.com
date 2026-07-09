//! Standalone smoke test for the in-process Pocket-TTS engine used by the voice
//! receptionist. Loads the model, synthesizes a phrase to 16 kHz i16 PCM, and
//! reports timing + peak amplitude (nonzero = real audio). Requires the `voice`
//! feature and a resolvable ONNX Runtime DLL (set ORT_DYLIB_PATH).
//!
//!   ORT_DYLIB_PATH=...\onnxruntime_1.25.0.dll \
//!     cargo run -p aokie-plugin --features voice --bin tts-smoke -- "Hello there"

#[cfg(feature = "voice")]
fn main() {
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Hello, thanks for calling. How can I help you today?".to_string());
    eprintln!("[tts-smoke] loading Pocket-TTS engine (~200 MB of ONNX graphs)...");
    let t0 = std::time::Instant::now();
    let mut eng = match aokie_plugin::voice::TtsEngine::load() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[tts-smoke] LOAD FAILED: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "[tts-smoke] loaded in {:?}; synthesizing {:?}",
        t0.elapsed(),
        text
    );
    let t1 = std::time::Instant::now();
    match eng.synthesize(&text, "", 16000) {
        Ok(pcm) => {
            let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            eprintln!(
                "[tts-smoke] OK: {} i16 samples @16kHz ({:.2}s audio) in {:?}; peak={} ({})",
                pcm.len(),
                pcm.len() as f32 / 16000.0,
                t1.elapsed(),
                peak,
                if peak > 100 {
                    "REAL AUDIO"
                } else {
                    "SILENT/near-silent"
                }
            );
        }
        Err(e) => {
            eprintln!("[tts-smoke] SYNTH FAILED: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "voice"))]
fn main() {
    eprintln!("[tts-smoke] built without the `voice` feature — nothing to do");
}
