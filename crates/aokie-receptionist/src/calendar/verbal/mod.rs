//! Verbal-tool harness — extract structured tool intent from natural
//! conversation, both directions:
//!
//!  - `user_intent` fills `BookingDraft` fields from caller utterances
//!    ("my name is Lance", "yes 2pm works", "12 Park Street").
//!  - `bot_intent` extracts `ToolCall`s from bot replies ("let me check
//!    Friday for the lawn mow" → `CheckAvailability`).
//!  - `dates` resolves "next Friday" / "May 8th" / "the 12th" to
//!    `NaiveDate` against a `today` anchor.
//!  - `services` matches free-form service names against the
//!    configured catalogue.
//!
//! Replaces (but does not delete) the `<TOOL>...</TOOL>` XML emission
//! path in the live BT call flow. The XML path stays compiled in
//! because the model very occasionally emits one and we'd rather
//! honour it than discard it.

pub mod bot_intent;
pub mod dates;
pub mod services;
pub mod user_intent;

// Convenience re-exports. Consumed by the live BT call body in
// `commands/bluetooth_commands.rs`, which is `#[cfg(target_os =
// "windows")]`-gated; on Linux the call body is stubbed out and the
// re-exports look unused — `#[allow(unused_imports)]` keeps the
// surface available for the Windows path without warning the Linux
// developer-preview build.
#[allow(unused_imports)]
pub use bot_intent::detect_verbal_tools;
#[allow(unused_imports)]
pub use user_intent::apply_user_utterance;
