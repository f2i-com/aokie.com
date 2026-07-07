// Streaming TTS Controller - Manages sentence-by-sentence TTS with barge-in support
// Tracks what text was actually spoken vs. interrupted
//
// Much of this module is legacy — the Bluetooth pipeline now inlines
// sentence extraction + streaming TTS directly. `extract_sentence` below is
// still used; everything else is retained for possible future reuse.
#![allow(dead_code)]

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A chunk of TTS audio ready to send
#[derive(Debug, Clone)]
pub struct TtsChunk {
    /// The text that was synthesized
    pub text: String,
    /// Audio samples (resampled for Bluetooth, typically 8kHz i16)
    pub samples: Vec<i16>,
    /// Original sample rate
    pub sample_rate: u32,
    /// Which sentence index this is (0, 1, 2, ...)
    pub sentence_idx: usize,
    /// Is this the last chunk of the response?
    pub is_last: bool,
}

/// Control signals for TTS playback
#[derive(Debug)]
pub enum TtsControl {
    /// Play this audio chunk
    Play(TtsChunk),
    /// Stop immediately (barge-in)
    Stop,
}

/// Tracks what text was actually spoken (for conversation history)
#[derive(Debug, Clone)]
pub struct SpokenResponse {
    /// Text that was fully spoken before any interruption
    pub spoken_text: String,
    /// True if the response was interrupted before completion
    pub was_interrupted: bool,
    /// Text that was generated but not spoken (cut off)
    pub unspoken_text: Option<String>,
}

impl SpokenResponse {
    /// Format for conversation history - includes [interrupted] marker if needed
    pub fn to_history_text(&self) -> String {
        if self.was_interrupted {
            if self.spoken_text.is_empty() {
                "[interrupted before speaking]".to_string()
            } else {
                format!("{} [interrupted]", self.spoken_text.trim())
            }
        } else {
            self.spoken_text.clone()
        }
    }
}

/// Controller for streaming TTS playback with barge-in support
pub struct StreamingTtsController {
    /// Channel to send TTS chunks to playback
    chunk_tx: mpsc::UnboundedSender<TtsControl>,
    /// Flag indicating if TTS is currently playing
    is_playing: Arc<AtomicBool>,
    /// Flag to signal cancellation (barge-in)
    is_cancelled: Arc<AtomicBool>,
    /// Tracks sentences that have been fully played
    spoken_sentences: Arc<Mutex<Vec<String>>>,
    /// Tracks sentences that were queued but may not have played
    queued_sentences: Arc<Mutex<Vec<String>>>,
}

impl StreamingTtsController {
    /// Create a new streaming TTS controller
    /// Returns the controller and the receiver for the playback task
    pub fn new() -> (Self, mpsc::UnboundedReceiver<TtsControl>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                chunk_tx: tx,
                is_playing: Arc::new(AtomicBool::new(false)),
                is_cancelled: Arc::new(AtomicBool::new(false)),
                spoken_sentences: Arc::new(Mutex::new(Vec::new())),
                queued_sentences: Arc::new(Mutex::new(Vec::new())),
            },
            rx,
        )
    }

    /// Check if TTS is currently playing
    pub fn is_playing(&self) -> bool {
        self.is_playing.load(Ordering::SeqCst)
    }

    /// Check if playback has been cancelled (barge-in)
    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::SeqCst)
    }

    /// Stop playback immediately (for barge-in)
    pub fn stop_immediately(&self) {
        println!("[StreamingTTS] STOP - Barge-in detected");
        self.is_cancelled.store(true, Ordering::SeqCst);
        self.is_playing.store(false, Ordering::SeqCst);
        let _ = self.chunk_tx.send(TtsControl::Stop);
    }

    /// Queue a TTS chunk for playback
    pub fn send_chunk(&self, chunk: TtsChunk) {
        if self.is_cancelled() {
            println!("[StreamingTTS] Ignoring chunk - cancelled");
            return;
        }

        // Track this sentence as queued
        {
            let mut queued = self.queued_sentences.lock();
            queued.push(chunk.text.clone());
        }

        self.is_playing.store(true, Ordering::SeqCst);
        let _ = self.chunk_tx.send(TtsControl::Play(chunk));
    }

    /// Mark a sentence as fully spoken (called by playback task)
    pub fn mark_sentence_spoken(&self, text: String) {
        let mut spoken = self.spoken_sentences.lock();
        spoken.push(text);
    }

    /// Reset for a new response
    pub fn reset(&self) {
        self.is_cancelled.store(false, Ordering::SeqCst);
        self.is_playing.store(false, Ordering::SeqCst);
        self.spoken_sentences.lock().clear();
        self.queued_sentences.lock().clear();
    }

    /// Get what was actually spoken (for conversation history)
    pub fn get_spoken_response(&self) -> SpokenResponse {
        let spoken = self.spoken_sentences.lock();
        let queued = self.queued_sentences.lock();

        let spoken_text = spoken.join(" ");
        let was_interrupted = self.is_cancelled();

        // Calculate unspoken text (queued but not spoken)
        let unspoken_text = if was_interrupted && queued.len() > spoken.len() {
            Some(queued[spoken.len()..].join(" "))
        } else {
            None
        };

        SpokenResponse {
            spoken_text,
            was_interrupted,
            unspoken_text,
        }
    }

    /// Clone the is_playing flag for sharing with playback task
    pub fn is_playing_flag(&self) -> Arc<AtomicBool> {
        self.is_playing.clone()
    }

    /// Clone the is_cancelled flag for sharing
    pub fn is_cancelled_flag(&self) -> Arc<AtomicBool> {
        self.is_cancelled.clone()
    }

    /// Clone spoken_sentences for sharing with playback task
    pub fn spoken_sentences_ref(&self) -> Arc<Mutex<Vec<String>>> {
        self.spoken_sentences.clone()
    }
}

impl Clone for StreamingTtsController {
    fn clone(&self) -> Self {
        Self {
            chunk_tx: self.chunk_tx.clone(),
            is_playing: self.is_playing.clone(),
            is_cancelled: self.is_cancelled.clone(),
            spoken_sentences: self.spoken_sentences.clone(),
            queued_sentences: self.queued_sentences.clone(),
        }
    }
}

/// Detect if text ends with a sentence boundary
pub fn is_sentence_end(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Check for sentence-ending punctuation
    trimmed.ends_with('.')
        || trimmed.ends_with('!')
        || trimmed.ends_with('?')
        || trimmed.ends_with('\n')
        // Also split on long pauses indicated by ellipsis
        || trimmed.ends_with("...")
}

/// Common English abbreviations whose terminal `.` should NOT be
/// treated as a sentence boundary. Lowercased for case-insensitive
/// matching. If the LLM emits "Dr. Smith called." we want to ship the
/// whole sentence to TTS, not "Dr." on its own — pre-fix the chunker
/// would break at the first period+space, sending fragments to the
/// receptionist's voice.
const SENTENCE_ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "inc", "ltd", "corp", "co", "etc", "vs",
    "e.g", "i.e", "u.s", "u.k", "a.m", "p.m", "no",
];

/// Returns true if the word ending at index `boundary` (inclusive of
/// the `.`) is one of `SENTENCE_ABBREVIATIONS`. Walks backwards from
/// the period to find the start of the preceding token.
fn looks_like_abbreviation(text: &str, boundary: usize) -> bool {
    let before = &text[..boundary];
    // Walk back through alphanumerics and embedded `.` (so "a.m" /
    // "i.e" survive intact). Stop at anything else — whitespace,
    // brackets, quotes, dashes — so "(Dr." extracts as "Dr" rather
    // than "(Dr", which used to leak the leading punctuation into
    // the comparison and break the match, splitting the sentence
    // mid-thought. Char-based walk so UTF-8 boundary chars don't
    // slice through a codepoint.
    let mut token_start = boundary;
    for (i, c) in before.char_indices().rev() {
        if c.is_ascii_alphanumeric() || c == '.' {
            token_start = i;
        } else {
            break;
        }
    }
    let word = &text[token_start..boundary];
    let lower = word.to_ascii_lowercase();
    SENTENCE_ABBREVIATIONS.iter().any(|abbr| lower == *abbr)
}

/// Extract a complete sentence from the buffer, leaving remainder
pub fn extract_sentence(buffer: &mut String) -> Option<String> {
    let text = buffer.trim();
    if text.is_empty() {
        return None;
    }

    // Find sentence boundary
    let mut last_boundary = None;
    for (i, c) in text.char_indices() {
        if c == '.' || c == '!' || c == '?' {
            // Skip abbreviations: "Dr." and "Inc." should not break a
            // sentence even though they end with `.`-then-space.
            if c == '.' && looks_like_abbreviation(text, i) {
                continue;
            }
            // Check if it's actually end of sentence (not abbreviation like "Dr.")
            let remaining = &text[i + 1..];
            if remaining.is_empty() || remaining.starts_with(' ') || remaining.starts_with('\n') {
                last_boundary = Some(i);
                break;
            }
        }
    }

    if let Some(boundary) = last_boundary {
        let sentence = text[..=boundary].trim().to_string();
        let remainder = text[boundary + 1..].trim().to_string();
        *buffer = remainder;
        Some(sentence)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sentence_detection() {
        assert!(is_sentence_end("Hello there."));
        assert!(is_sentence_end("How are you?"));
        assert!(is_sentence_end("Great!"));
        assert!(!is_sentence_end("Hello"));
        assert!(!is_sentence_end("Dr"));
    }

    #[test]
    fn test_extract_sentence() {
        let mut buffer = "Hello there. How are you?".to_string();
        let sentence = extract_sentence(&mut buffer);
        assert_eq!(sentence, Some("Hello there.".to_string()));
        assert_eq!(buffer, "How are you?");

        let sentence2 = extract_sentence(&mut buffer);
        assert_eq!(sentence2, Some("How are you?".to_string()));
        assert_eq!(buffer, "");
    }

    #[test]
    fn extract_sentence_skips_common_abbreviations() {
        // Pre-fix: this would yield "Dr." as the first sentence and
        // ship that to TTS, which then said "doctor" by itself.
        let mut buffer = "Dr. Smith called yesterday. He left a message.".to_string();
        assert_eq!(
            extract_sentence(&mut buffer),
            Some("Dr. Smith called yesterday.".to_string())
        );
        assert_eq!(buffer, "He left a message.");

        let mut buffer = "Acme Inc. and Beta Co. are merging.".to_string();
        assert_eq!(
            extract_sentence(&mut buffer),
            Some("Acme Inc. and Beta Co. are merging.".to_string())
        );

        // Sanity: a real sentence boundary still fires.
        let mut buffer = "Mr. Lee said hi. Mrs. Lee waved.".to_string();
        assert_eq!(
            extract_sentence(&mut buffer),
            Some("Mr. Lee said hi.".to_string())
        );
        assert_eq!(buffer, "Mrs. Lee waved.");
    }

    #[test]
    fn extract_sentence_skips_abbreviations_after_punctuation() {
        // Pre-fix: the rfind(whitespace) walk-back returned the token
        // "(Dr" / "'Mr" — the abbreviation match missed and the TTS
        // chunker split the sentence at "Dr." mid-thought.
        let mut buffer = "(Dr. Smith called) yesterday afternoon.".to_string();
        assert_eq!(
            extract_sentence(&mut buffer),
            Some("(Dr. Smith called) yesterday afternoon.".to_string())
        );

        let mut buffer = "She said 'Mr. Lee left' before noon.".to_string();
        assert_eq!(
            extract_sentence(&mut buffer),
            Some("She said 'Mr. Lee left' before noon.".to_string())
        );

        let mut buffer = "Meeting at 9 a.m. tomorrow.".to_string();
        assert_eq!(
            extract_sentence(&mut buffer),
            Some("Meeting at 9 a.m. tomorrow.".to_string())
        );
    }

    #[test]
    fn test_spoken_response_format() {
        let response = SpokenResponse {
            spoken_text: "Hello there.".to_string(),
            was_interrupted: true,
            unspoken_text: Some("I was going to say more.".to_string()),
        };
        assert_eq!(response.to_history_text(), "Hello there. [interrupted]");

        let complete = SpokenResponse {
            spoken_text: "Full response here.".to_string(),
            was_interrupted: false,
            unspoken_text: None,
        };
        assert_eq!(complete.to_history_text(), "Full response here.");
    }
}
