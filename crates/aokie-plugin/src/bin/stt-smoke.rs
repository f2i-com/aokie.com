//! Standalone smoke test for the in-process Parakeet STT engine used by the
//! voice receptionist. Round-trips through TTS: synthesize a phrase to 16 kHz
//! PCM, then transcribe it back and print what STT heard. A transcript that
//! resembles the input proves both engines load + run. Requires the `voice`
//! feature and a resolvable ONNX Runtime DLL (set ORT_DYLIB_PATH).
//!
//!   ORT_DYLIB_PATH=...\onnxruntime_1.25.0.dll \
//!     cargo run -p aokie-plugin --features voice --bin stt-smoke -- "testing one two three"

#[cfg(feature = "voice")]
fn main() {
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "The quick brown fox jumps over the lazy dog".to_string());

    eprintln!("[stt-smoke] loading TTS to synthesize the reference phrase...");
    let mut tts = aokie_plugin::voice::TtsEngine::load().expect("TTS load");
    let pcm = tts.synthesize(&text, "", 16000).expect("TTS synth"); // i16 @ 16 kHz
    let f32_16k: Vec<f32> = pcm.iter().map(|&s| s as f32 / 32768.0).collect();
    eprintln!(
        "[stt-smoke] reference: {:?} ({} samples, {:.2}s)",
        text,
        f32_16k.len(),
        f32_16k.len() as f32 / 16000.0
    );

    eprintln!("[stt-smoke] loading Parakeet STT (~600 MB of ONNX graphs)...");
    let t0 = std::time::Instant::now();
    let mut stt = match aokie_plugin::voice::SttEngine::load() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[stt-smoke] STT LOAD FAILED: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[stt-smoke] loaded in {:?}; transcribing...", t0.elapsed());
    let t1 = std::time::Instant::now();
    match stt.transcribe(&f32_16k) {
        Ok(heard) => {
            eprintln!("[stt-smoke] heard {:?} in {:?}", heard, t1.elapsed());
            if heard.is_empty() {
                eprintln!("[stt-smoke] EMPTY transcript — STT ran but produced nothing");
                std::process::exit(2);
            }
            eprintln!("[stt-smoke] OK — STT produced a transcript");
        }
        Err(e) => {
            eprintln!("[stt-smoke] TRANSCRIBE FAILED: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "voice"))]
fn main() {
    eprintln!("[stt-smoke] built without the `voice` feature — nothing to do");
}
