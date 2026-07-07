// Notepad module - Continuous caller transcript storage
// Captures all caller speech, even during bot responses, for context.
//
// Some accessor methods below aren't wired to the current UI yet (the
// Bluetooth pipeline emits per-turn transcript events directly); they
// carry their own #[allow(dead_code)] so the rest of the module
// catches genuinely-dead additions instead of hiding under a blanket
// allow (R12-#12).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single entry in the notepad (caller speech only)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotepadEntry {
    /// The transcribed text
    pub text: String,
    /// When this speech occurred
    pub timestamp: DateTime<Utc>,
    /// Duration of the speech in milliseconds (if known)
    #[allow(dead_code)]
    pub duration_ms: Option<u64>,
    /// True if this speech occurred while bot was speaking (barge-in/overlap)
    pub was_during_bot_speech: bool,
}

/// Notepad for a phone call - accumulates all caller speech
#[derive(Debug, Clone)]
pub struct Notepad {
    /// All caller speech entries
    entries: Vec<NotepadEntry>,
    /// When the call started
    #[allow(dead_code)]
    call_start: DateTime<Utc>,
    /// Unique call identifier
    #[allow(dead_code)]
    call_id: String,
}

// All Notepad methods are exercised today only by the Windows BT
// call body (`bluetooth_commands.rs`) and the in-file unit tests.
// On a Linux non-test build, cargo flags both as dead. The struct
// already carries `#[allow(dead_code)]` on the call_id field for
// the same reason; mirror it across the impl methods.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl Notepad {
    /// Create a new notepad for a call
    pub fn new(call_id: String) -> Self {
        Self {
            entries: Vec::new(),
            call_start: Utc::now(),
            call_id,
        }
    }

    /// Add caller speech to the notepad
    pub fn add_caller_speech(&mut self, text: String, during_bot_speech: bool) {
        // Skip empty or noise-only entries
        let trimmed = text.trim();
        if trimmed.is_empty() || is_noise_text(trimmed) {
            return;
        }

        self.entries.push(NotepadEntry {
            text: trimmed.to_string(),
            timestamp: Utc::now(),
            duration_ms: None,
            was_during_bot_speech: during_bot_speech,
        });

        // Char-aware truncation: byte-slicing `&trimmed[..50]` panics if
        // byte 50 lands inside a multi-byte UTF-8 codepoint (Chinese,
        // Japanese, Cyrillic, accented Latin). Real Whisper transcripts
        // can contain any of these.
        let preview: String = trimmed.chars().take(50).collect();
        println!(
            "[Notepad] Added caller speech{}: \"{}\"",
            if during_bot_speech {
                " (during bot)"
            } else {
                ""
            },
            preview
        );
    }

    /// Add caller speech with duration info
    #[allow(dead_code)]
    pub fn add_caller_speech_with_duration(
        &mut self,
        text: String,
        duration_ms: u64,
        during_bot_speech: bool,
    ) {
        let trimmed = text.trim();
        if trimmed.is_empty() || is_noise_text(trimmed) {
            return;
        }

        self.entries.push(NotepadEntry {
            text: trimmed.to_string(),
            timestamp: Utc::now(),
            duration_ms: Some(duration_ms),
            was_during_bot_speech: during_bot_speech,
        });
    }

    /// Build context string for LLM prompt from recent entries
    #[allow(dead_code)]
    pub fn build_context(&self, max_entries: usize) -> String {
        if self.entries.is_empty() {
            return String::new();
        }

        let start_idx = self.entries.len().saturating_sub(max_entries);
        self.entries[start_idx..]
            .iter()
            .map(|entry| {
                let time = entry.timestamp.format("%H:%M:%S");
                let overlap_marker = if entry.was_during_bot_speech {
                    " [overlapping]"
                } else {
                    ""
                };
                format!("[{}]{} {}", time, overlap_marker, entry.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get all entries (for saving to database)
    #[allow(dead_code)]
    pub fn entries(&self) -> &[NotepadEntry] {
        &self.entries
    }

    /// Get the call ID
    #[allow(dead_code)]
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Get the call start time
    #[allow(dead_code)]
    pub fn call_start(&self) -> DateTime<Utc> {
        self.call_start
    }

    /// Get total number of entries
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if notepad is empty
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all entries (for testing)
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Get the last entry text (for checking duplicates)
    #[allow(dead_code)]
    pub fn last_text(&self) -> Option<&str> {
        self.entries.last().map(|e| e.text.as_str())
    }
}

/// Check if text is likely noise/artifacts rather than real speech
fn is_noise_text(text: &str) -> bool {
    let lower = text.to_lowercase();

    // Common STT noise patterns
    lower == "[silence]"
        || lower == "[noise]"
        || lower == "[music]"
        || lower == "[applause]"
        || lower == "..."
        || lower == "hmm"
        || lower == "uh"
        || lower == "um"
        || (text.len() < 3 && !text.chars().any(|c| c.is_alphabetic()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notepad_basic() {
        let mut notepad = Notepad::new("test-call-1".to_string());

        notepad.add_caller_speech("Hello, I need help".to_string(), false);
        notepad.add_caller_speech("Can you schedule an appointment?".to_string(), false);

        assert_eq!(notepad.len(), 2);

        let context = notepad.build_context(10);
        assert!(context.contains("Hello, I need help"));
        assert!(context.contains("Can you schedule an appointment?"));
    }

    #[test]
    fn test_notepad_overlap_marker() {
        let mut notepad = Notepad::new("test-call-2".to_string());

        notepad.add_caller_speech("Wait, stop".to_string(), true);

        let context = notepad.build_context(10);
        assert!(context.contains("[overlapping]"));
    }

    #[test]
    fn test_notepad_filters_noise() {
        let mut notepad = Notepad::new("test-call-3".to_string());

        notepad.add_caller_speech("[silence]".to_string(), false);
        notepad.add_caller_speech("uh".to_string(), false);
        notepad.add_caller_speech("".to_string(), false);
        notepad.add_caller_speech("Real speech here".to_string(), false);

        assert_eq!(notepad.len(), 1);
        assert_eq!(notepad.last_text(), Some("Real speech here"));
    }

    #[test]
    fn test_notepad_handles_multibyte_utf8_at_preview_boundary() {
        // 25 Chinese characters → 75 bytes (each 3 bytes in UTF-8).
        // The old code did `&trimmed[..trimmed.len().min(50)]`, which
        // would panic because byte 50 lands inside a codepoint.
        let mut notepad = Notepad::new("test-call-utf8".to_string());
        let chinese = "你好世界你好世界你好世界你好世界你好世界你好世界你好世";
        notepad.add_caller_speech(chinese.to_string(), false);
        assert_eq!(notepad.len(), 1);

        // Same hazard with accented Latin (2 bytes per char) — 30 chars,
        // 60 bytes, byte 50 lands mid-codepoint.
        let mut notepad2 = Notepad::new("test-call-utf8-2".to_string());
        let accented = "àáâãäåæçèéêëìíîïðñòóôõö÷øùúûü";
        notepad2.add_caller_speech(accented.to_string(), false);
        assert_eq!(notepad2.len(), 1);
    }

    #[test]
    fn test_notepad_context_limit() {
        let mut notepad = Notepad::new("test-call-4".to_string());

        for i in 0..10 {
            notepad.add_caller_speech(format!("Message {}", i), false);
        }

        let context = notepad.build_context(3);
        assert!(!context.contains("Message 0"));
        assert!(context.contains("Message 7"));
        assert!(context.contains("Message 8"));
        assert!(context.contains("Message 9"));
    }
}
