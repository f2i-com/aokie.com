//! Realtime provider tools: what may run, and what a refusal looks like.
//!
//! OAIY forwards one model tool call at a time and waits for exactly one
//! result. Until this module a call the plugin could not run right now was a
//! fatal realtime failure: a second tool while one was pending, a ninth in one
//! call, a name the plugin did not know. The session ended, the caller heard
//! the fixed apology and the plugin hung up. That made the model's next words
//! able to end the phone call.
//!
//! Every such case is now an ordinary `ok: false` result the model can read
//! (`busy`, `tool_limit`, `unsupported`), and the call carries on. A protocol
//! violation that cannot be answered at all (a tool call before the call was
//! begun, an unusable id or name) stays fatal: that is a broken bridge, not a
//! model that asked for something odd.
//!
//! Tools are of two classes. A **slot** tool takes the call's one tool slot
//! while it runs (`lookup_business_data` polls the host for seconds;
//! `request_appointment` writes a durable record). Only one is in flight. An
//! **instant** tool answers within a loop turn and may run while a lookup
//! polls (`finish_call`, `transfer_to_owner`).

#[allow(unused_imports)]
use super::*;

/// A call that has asked for this many tools gets `tool_limit` for the next
/// one. Every call counts, refused ones too, so a model that loops on a
/// refusal cannot keep the bridge busy for the whole call.
pub(super) const MAX_TOOL_CALLS_PER_CALL: usize = 24;

/// Tool-call ids remembered for duplicate detection. A little more than the
/// cap: past it every call is refused anyway.
const REMEMBERED_TOOL_CALL_IDS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolClass {
    /// Takes the call's one tool slot for as long as it runs.
    Slot,
    /// Answers within a loop turn; never holds the slot.
    Instant,
}

/// The class of a tool the plugin knows, or `None` for one it does not.
/// `transfer_to_owner` exists only on a session that negotiated `transfer_v1`.
pub(super) fn tool_class(name: &str, transfer_enabled: bool) -> Option<ToolClass> {
    match name {
        "lookup_business_data" | "request_appointment" => Some(ToolClass::Slot),
        "finish_call" => Some(ToolClass::Instant),
        crate::transfer::TOOL_NAME if transfer_enabled => Some(ToolClass::Instant),
        _ => None,
    }
}

/// Why a tool call was refused without running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolRefusal {
    /// Not a tool this plugin (or this session) has.
    Unsupported,
    /// Another tool holds the slot, or the one queue place is taken.
    Busy,
    /// The call has already asked for its allowance of tools.
    ToolLimit,
}

impl ToolRefusal {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Busy => "busy",
            Self::ToolLimit => "tool_limit",
        }
    }

    /// The `output` of the `ok: false` result. The code and nothing else: the
    /// refusal carries no text from the call.
    pub(super) fn output(self) -> serde_json::Value {
        serde_json::json!({ "error": self.code() })
    }
}

/// What to do with one incoming tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolAdmission {
    /// Run it, in this class.
    Run(ToolClass),
    /// Complete it with this refusal.
    Refuse(ToolRefusal),
    /// The same tool-call id was already seen on this call. It is dropped
    /// without a second result: OAIY takes one result per call id and would
    /// treat a second as a stale one.
    Duplicate,
}

/// The tools a call has asked for so far.
#[derive(Debug, Default)]
pub(super) struct ToolLedger {
    seen: std::collections::VecDeque<String>,
    asked: usize,
}

impl ToolLedger {
    /// Decide one call and remember it. `slot_busy` is true while a slot tool
    /// is queued or running; `queue_busy` while the one queue place for a
    /// tool waiting to run is taken (by a tool of either class).
    pub(super) fn admit(
        &mut self,
        tool_call_id: &str,
        name: &str,
        transfer_enabled: bool,
        slot_busy: bool,
        queue_busy: bool,
    ) -> ToolAdmission {
        if self.seen.iter().any(|seen| seen == tool_call_id) {
            return ToolAdmission::Duplicate;
        }
        self.remember(tool_call_id);
        self.asked += 1;
        if self.asked > MAX_TOOL_CALLS_PER_CALL {
            return ToolAdmission::Refuse(ToolRefusal::ToolLimit);
        }
        let Some(class) = tool_class(name, transfer_enabled) else {
            return ToolAdmission::Refuse(ToolRefusal::Unsupported);
        };
        let taken = match class {
            ToolClass::Slot => slot_busy || queue_busy,
            ToolClass::Instant => queue_busy,
        };
        if taken {
            ToolAdmission::Refuse(ToolRefusal::Busy)
        } else {
            ToolAdmission::Run(class)
        }
    }

    fn remember(&mut self, tool_call_id: &str) {
        if self.seen.len() >= REMEMBERED_TOOL_CALL_IDS {
            self.seen.pop_front();
        }
        self.seen.push_back(tool_call_id.to_string());
    }

    /// How many tool calls this call has asked for.
    #[cfg(test)]
    pub(super) fn asked(&self) -> usize {
        self.asked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admit(ledger: &mut ToolLedger, id: &str, name: &str) -> ToolAdmission {
        ledger.admit(id, name, false, false, false)
    }

    #[test]
    fn the_three_existing_tools_keep_their_classes_and_transfer_needs_negotiation() {
        assert_eq!(tool_class("lookup_business_data", false), Some(ToolClass::Slot));
        assert_eq!(tool_class("request_appointment", false), Some(ToolClass::Slot));
        assert_eq!(tool_class("finish_call", false), Some(ToolClass::Instant));
        assert_eq!(tool_class("transfer_to_owner", false), None);
        assert_eq!(tool_class("transfer_to_owner", true), Some(ToolClass::Instant));
        for other in ["", "arbitrary_desktop_action", "Finish_Call", "finish_call "] {
            assert_eq!(tool_class(other, true), None, "{other:?}");
        }
    }

    #[test]
    fn an_unknown_tool_is_unsupported_and_leaves_no_slot_taken() {
        let mut ledger = ToolLedger::default();
        assert_eq!(
            admit(&mut ledger, "t1", "arbitrary_desktop_action"),
            ToolAdmission::Refuse(ToolRefusal::Unsupported)
        );
        assert_eq!(ToolRefusal::Unsupported.output(), serde_json::json!({"error": "unsupported"}));
        // The next, ordinary tool is unaffected.
        assert_eq!(
            admit(&mut ledger, "t2", "lookup_business_data"),
            ToolAdmission::Run(ToolClass::Slot)
        );
        // transfer_to_owner on a session that did not negotiate it is unsupported too.
        assert_eq!(
            ledger.admit("t3", "transfer_to_owner", false, false, false),
            ToolAdmission::Refuse(ToolRefusal::Unsupported)
        );
    }

    #[test]
    fn a_second_slot_tool_is_busy_but_an_instant_tool_may_run_beside_a_lookup() {
        let mut ledger = ToolLedger::default();
        // A lookup is polling: the slot is taken, the queue place is free.
        assert_eq!(
            ledger.admit("t1", "request_appointment", false, true, false),
            ToolAdmission::Refuse(ToolRefusal::Busy)
        );
        assert_eq!(
            ledger.admit("t2", "lookup_business_data", false, true, false),
            ToolAdmission::Refuse(ToolRefusal::Busy)
        );
        assert_eq!(
            ledger.admit("t3", "finish_call", false, true, false),
            ToolAdmission::Run(ToolClass::Instant)
        );
        assert_eq!(
            ledger.admit("t4", "transfer_to_owner", true, true, false),
            ToolAdmission::Run(ToolClass::Instant)
        );
        assert_eq!(ToolRefusal::Busy.output(), serde_json::json!({"error": "busy"}));
    }

    #[test]
    fn one_queue_place_means_a_second_tool_of_either_class_is_busy() {
        let mut ledger = ToolLedger::default();
        for name in ["finish_call", "lookup_business_data", "transfer_to_owner"] {
            assert_eq!(
                ledger.admit(&format!("t_{name}"), name, true, false, true),
                ToolAdmission::Refuse(ToolRefusal::Busy),
                "{name}"
            );
        }
    }

    #[test]
    fn the_twenty_fifth_tool_call_gets_tool_limit_and_refusals_count() {
        let mut ledger = ToolLedger::default();
        for n in 0..MAX_TOOL_CALLS_PER_CALL {
            // Half of them are refusals: they use the allowance all the same.
            let name = if n % 2 == 0 { "finish_call" } else { "nonsense_tool" };
            let admitted = admit(&mut ledger, &format!("t{n}"), name);
            assert_ne!(admitted, ToolAdmission::Refuse(ToolRefusal::ToolLimit), "call {n}");
        }
        assert_eq!(ledger.asked(), 24);
        assert_eq!(
            admit(&mut ledger, "t24", "lookup_business_data"),
            ToolAdmission::Refuse(ToolRefusal::ToolLimit)
        );
        // The limit wins over everything else, and it stays.
        assert_eq!(
            admit(&mut ledger, "t25", "nonsense_tool"),
            ToolAdmission::Refuse(ToolRefusal::ToolLimit)
        );
        assert_eq!(ToolRefusal::ToolLimit.output(), serde_json::json!({"error": "tool_limit"}));
    }

    #[test]
    fn a_repeated_tool_call_id_is_dropped_without_a_second_result_or_a_charge() {
        let mut ledger = ToolLedger::default();
        assert_eq!(admit(&mut ledger, "same", "finish_call"), ToolAdmission::Run(ToolClass::Instant));
        assert_eq!(admit(&mut ledger, "same", "finish_call"), ToolAdmission::Duplicate);
        // A repeat of a refused call is a repeat too.
        assert_eq!(
            admit(&mut ledger, "odd", "nonsense_tool"),
            ToolAdmission::Refuse(ToolRefusal::Unsupported)
        );
        assert_eq!(admit(&mut ledger, "odd", "nonsense_tool"), ToolAdmission::Duplicate);
        // Repeats do not spend the allowance.
        assert_eq!(ledger.asked(), 2);
    }

    #[test]
    fn duplicate_memory_is_bounded() {
        let mut ledger = ToolLedger::default();
        for n in 0..(REMEMBERED_TOOL_CALL_IDS * 3) {
            let _ = admit(&mut ledger, &format!("t{n}"), "finish_call");
        }
        assert!(ledger.seen.len() <= REMEMBERED_TOOL_CALL_IDS);
    }

    #[test]
    fn the_shared_fixture_shows_the_refusals_the_plugin_gives() {
        let fixture = crate::transfer::fixture_tests::fixture("transfer-v1.tool-result.fixture.json");
        let cases = fixture["toolRefusals"]["cases"].as_array().unwrap();
        let ours = [ToolRefusal::Busy, ToolRefusal::ToolLimit, ToolRefusal::Unsupported];
        assert_eq!(cases.len(), ours.len());
        for refusal in ours {
            assert!(
                cases.iter().any(|case| case["output"] == refusal.output()),
                "{refusal:?} is not in the fixture"
            );
        }
        assert_eq!(MAX_TOOL_CALLS_PER_CALL, 24, "the fixture says the 25th call is refused");
    }

    #[test]
    fn refusal_output_never_carries_call_text() {
        for refusal in [ToolRefusal::Unsupported, ToolRefusal::Busy, ToolRefusal::ToolLimit] {
            let output = refusal.output();
            assert_eq!(output.as_object().unwrap().len(), 1);
            assert_eq!(output["error"], refusal.code());
        }
    }
}
