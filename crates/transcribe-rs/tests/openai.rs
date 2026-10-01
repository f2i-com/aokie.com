use std::path::PathBuf;

use transcribe_rs::{
    remote::openai::{self, OpenAIRequestParams},
    RemoteTranscriptionEngine,
};

// This test calls the real OpenAI API with OPENAI_API_KEY from the environment, so it is skipped
// unless asked for: `cargo test -p transcribe-rs --features openai -- --ignored`. The request shape
// is checked against a loopback server, with no network, in `tests/openai_mock.rs`.
#[tokio::test]
#[ignore = "calls the real OpenAI API and needs OPENAI_API_KEY"]
async fn test_dots_transcription() {
    let engine = openai::default_engine();

    // Load the JFK audio file
    let audio_path = PathBuf::from("samples/dots.wav");

    // Transcribe with temperature 0
    let result = engine
        .transcribe_file(
            &audio_path,
            OpenAIRequestParams::builder()
                .temperature(0.0)
                .build()
                .expect("Default parameters shoul be valid"),
        )
        .await
        .expect("Failed to transcribe");

    let text = result.text.trim();
    assert!(!text.is_empty(), "Transcription should not be empty");
    // Check key phrases rather than exact match — remote API punctuation varies between calls
    for phrase in [
        "connect the dots",
        "looking forward",
        "looking backwards",
        "trust in something",
        "follow your heart",
        "make all the difference",
    ] {
        assert!(
            text.contains(phrase),
            "Transcription missing expected phrase '{}'\nActual: '{}'",
            phrase,
            text
        );
    }
}
