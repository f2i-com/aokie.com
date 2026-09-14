//! Brief listening acknowledgements, without closing the caller's turn.
use super::*;

#[cfg(feature = "voice")]
pub(super) fn acknowledgement_due(
    elapsed: Duration,
    audio_samples: usize,
    silence: Duration,
    endpoint: Duration,
    hypothesis: &str,
    hypothesis_age: Duration,
) -> bool {
    let lower = hypothesis.to_lowercase();
    let caller_reserves_floor = ["let me finish", "let me speak", "let me talk",
        "hold on", "hang on", "please wait", "stop talking"]
        .iter().any(|phrase| lower.contains(phrase));
    elapsed >= Duration::from_secs(20)
        && audio_samples >= 16_000 * 6
        && silence >= Duration::from_millis(160)
        && silence < endpoint
        && hypothesis_age < Duration::from_millis(1500)
        && hypothesis.split_whitespace().count() >= 8
        && !turn_looks_unfinished(hypothesis)
        && !caller_reserves_floor
        && matches!(crate::duplex::parse_caller_intent(hypothesis), crate::duplex::CallerIntent::Content)
}

#[cfg(all(test, feature = "voice"))]
mod tests {
    use super::*;
    fn due(text: &str, elapsed: u64, pause: u64, age: u64) -> bool {
        acknowledgement_due(Duration::from_secs(elapsed), 16_000 * 7,
            Duration::from_millis(pause), Duration::from_millis(450), text,
            Duration::from_millis(age))
    }
    #[test]
    fn acknowledges_a_long_explanation_at_a_brief_pause_only() {
        let text = "I was hoping to talk about the appointment next week";
        assert!(due(text, 20, 180, 400));
        assert!(!due(text, 19, 180, 400));
        assert!(!due(text, 20, 0, 400));
        assert!(!due(text, 20, 450, 400));
        assert!(!due(text, 20, 180, 1600));
        assert!(!due("Yes please", 20, 180, 400));
    }
    #[test]
    fn leaves_details_and_floor_commands_uninterrupted() {
        for text in ["My number for the appointment next week is 0412",
            "I wanted to explain the problem to you because",
            "wait", "let me finish", "stop talking",
            "Before you answer anything please let me finish this thought"] {
            assert!(!due(text, 20, 180, 400), "{text}");
        }
    }
}
