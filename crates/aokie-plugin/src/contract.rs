//! The canonical aokie connector contract — the ONE in-repo source of
//! truth for event names, command names, and connector error codes
//! (AOKIE_PLUGIN_CONTRACT.md; audit INT-001).
//!
//! Every emit site in this crate MUST use the `events::*` constants —
//! never a raw `"aokie.*"` string literal — and `manifest.json` must
//! declare exactly [`events::ALL`] and [`commands::ALL`]. Both rules
//! are enforced by the tests at the bottom of this file, so an event
//! added at an emit site but missing from the manifest (which Desktop
//! would silently drop — audit C-03) fails `cargo test` instead of
//! failing on a live call.
//!
//! The FormLogic Desktop repo bundles a byte-for-byte copy of
//! `manifest.json` (`resources/plugins/aokie/manifest.json`) and the
//! pack's flow event catalog mirrors `events::ALL`; when this file
//! changes, ship those in the same change set.

/// Event names the plugin may emit (`DesktopEvent.name`).
pub mod events {
    // ── Dongle lifecycle ────────────────────────────────────────────
    pub const DONGLE_DETECTED: &str = "aokie.dongle.detected";
    pub const DONGLE_DRIVER_REQUIRED: &str = "aokie.dongle.driver_required";
    pub const DONGLE_READY: &str = "aokie.dongle.ready";
    pub const DONGLE_ERROR: &str = "aokie.dongle.error";
    // ── Phone link ──────────────────────────────────────────────────
    pub const PHONE_PAIRING_STARTED: &str = "aokie.phone.pairing_started";
    pub const PHONE_PAIRED: &str = "aokie.phone.paired";
    pub const PHONE_CONNECTED: &str = "aokie.phone.connected";
    pub const PHONE_DISCONNECTED: &str = "aokie.phone.disconnected";
    // ── Call lifecycle ──────────────────────────────────────────────
    pub const CALL_INCOMING: &str = "aokie.call.incoming";
    pub const CALL_RINGING: &str = "aokie.call.ringing";
    pub const CALL_ANSWERED: &str = "aokie.call.answered";
    pub const CALL_REJECTED: &str = "aokie.call.rejected";
    pub const CALL_AUDIO_CONNECTED: &str = "aokie.call.audio.connected";
    pub const CALL_AUDIO_DISCONNECTED: &str = "aokie.call.audio.disconnected";
    pub const CALL_TURN_PARTIAL: &str = "aokie.call.turn.partial";
    pub const CALL_TURN_FINAL: &str = "aokie.call.turn.final";
    pub const CALL_ENDED: &str = "aokie.call.ended";
    // ── SMS ─────────────────────────────────────────────────────────
    pub const SMS_RECEIVED: &str = "aokie.sms.received";
    pub const SMS_SENT: &str = "aokie.sms.sent";
    pub const SMS_FAILED: &str = "aokie.sms.failed";
    // ── Hardware ────────────────────────────────────────────────────
    pub const HARDWARE_ERROR: &str = "aokie.hardware.error";

    /// Every event this plugin may emit — MUST equal `manifest.json`'s
    /// `events` array (order-insensitive; the test below enforces it).
    pub const ALL: &[&str] = &[
        DONGLE_DETECTED,
        DONGLE_DRIVER_REQUIRED,
        DONGLE_READY,
        DONGLE_ERROR,
        PHONE_PAIRING_STARTED,
        PHONE_PAIRED,
        PHONE_CONNECTED,
        PHONE_DISCONNECTED,
        CALL_INCOMING,
        CALL_RINGING,
        CALL_ANSWERED,
        CALL_REJECTED,
        CALL_AUDIO_CONNECTED,
        CALL_AUDIO_DISCONNECTED,
        CALL_TURN_PARTIAL,
        CALL_TURN_FINAL,
        CALL_ENDED,
        SMS_RECEIVED,
        SMS_SENT,
        SMS_FAILED,
        HARDWARE_ERROR,
    ];

    /// Declared events with no live emit site yet (kept declared because
    /// pack flow bindings / the mock surface may reference them; audited
    /// so a "declared but never emitted" drift is deliberate, not silent):
    /// `driver_required`, `dongle.error`, `phone.paired`, `turn.partial`,
    /// `sms.failed` — see AOKIE_PLUGIN_CONTRACT.md §events.
    pub const DECLARED_NOT_YET_EMITTED: &[&str] = &[
        DONGLE_DRIVER_REQUIRED,
        DONGLE_ERROR,
        PHONE_PAIRED,
        CALL_TURN_PARTIAL,
        SMS_FAILED,
    ];
}

/// Connector command names (`connector.request` `command` field).
pub mod commands {
    /// Every command the dispatcher implements — MUST equal
    /// `manifest.json`'s `connectors[0].commands` (test-enforced).
    pub const ALL: &[&str] = &[
        "dongle.list",
        "dongle.getPreferred",
        "dongle.setPreferred",
        "dongle.installDriver",
        "dongle.diagnostics",
        "phone.status",
        "phone.startPairing",
        "phone.stopPairing",
        "phone.listPaired",
        "call.current",
        "call.answer",
        "call.reject",
        "call.hangup",
        "call.operatorSpeak",
        "sms.threads",
        "sms.thread",
        "sms.send",
        "settings.get",
        "settings.set",
    ];
}

/// Typed connector error codes (`error.data.code`).
pub mod errors {
    pub const COMMAND_FAILED: &str = "command_failed";
    pub const CONNECTOR_MISSING: &str = "connector_missing";
    /// A call-control command named a `callId` that is not the plugin's
    /// current call (stale browser tab / raced lifecycle) — the phone
    /// state was NOT touched (audit C-01).
    pub const STALE_CALL: &str = "stale_call";
}

/// Canonical `call.current` / event call states (audit C-02): the ONLY
/// values the `state` field may carry, shared by the real radio path,
/// the plugin mock, the browser mock, and the Live Call screen.
pub mod call_state {
    pub const RINGING: &str = "ringing";
    pub const ACTIVE: &str = "active";
    pub const ENDED: &str = "ended";
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    fn manifest() -> Value {
        serde_json::from_str(include_str!("../manifest.json")).expect("manifest.json parses")
    }

    fn string_set(v: &Value) -> BTreeSet<&str> {
        v.as_array()
            .expect("array")
            .iter()
            .map(|e| e.as_str().expect("string entry"))
            .collect()
    }

    /// INT-001/C-03 drift gate: the manifest declares exactly the events
    /// the contract knows. An event emitted under a name Desktop filters
    /// out can no longer ship.
    #[test]
    fn manifest_events_match_the_contract() {
        let m = manifest();
        let declared = string_set(&m["events"]);
        let ours: BTreeSet<&str> = super::events::ALL.iter().copied().collect();
        assert_eq!(
            declared, ours,
            "manifest.json events must equal contract::events::ALL"
        );
        assert_eq!(super::events::ALL.len(), declared.len(), "no duplicates");
    }

    /// The manifest's command list is exactly the contract's, and every
    /// command is capability-covered 1:1 (`connector.aokie.<command>`).
    #[test]
    fn manifest_commands_match_the_contract() {
        let m = manifest();
        let declared = string_set(&m["connectors"][0]["commands"]);
        let ours: BTreeSet<&str> = super::commands::ALL.iter().copied().collect();
        assert_eq!(
            declared, ours,
            "manifest.json connector commands must equal contract::commands::ALL"
        );
        let capabilities = string_set(&m["capabilities"]);
        for cmd in super::commands::ALL {
            let cap = format!("connector.aokie.{cmd}");
            assert!(
                capabilities.contains(cap.as_str()),
                "command {cmd} has no capability {cap}"
            );
        }
    }

    /// Emit sites must use the contract constants: no quoted `"aokie.*"`
    /// literal may appear anywhere in the event-emitting modules (source
    /// scan, tests included). New events start HERE, get declared in the
    /// manifest, and only then get an emit site.
    #[test]
    fn no_raw_event_literals_outside_the_contract() {
        for (file, src) in [
            ("radio.rs", include_str!("radio.rs")),
            ("connector.rs", include_str!("connector.rs")),
            ("event_bridge.rs", include_str!("event_bridge.rs")),
        ] {
            let raw = src.matches("\"aokie.").count();
            assert_eq!(
                raw, 0,
                "{file} contains {raw} raw \"aokie.*\" string literal(s) — use crate::contract::events::* instead"
            );
        }
    }

    #[test]
    fn declared_not_yet_emitted_is_a_subset_of_all() {
        for e in super::events::DECLARED_NOT_YET_EMITTED {
            assert!(super::events::ALL.contains(e), "{e} missing from ALL");
        }
    }
}
