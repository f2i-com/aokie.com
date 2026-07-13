//! FormLogic Desktop event envelope (`desktop-event.schema.json`).
//!
//! Every event flowing plugin → FormLogic Desktop → FormLogic Web
//! uses this envelope. The canonical schema lives in the
//! formlogic-app repo (`docs/contracts/desktop-event.schema.json`);
//! this repo keeps a validated local copy under `docs/contracts/`,
//! and `aokie-plugin`'s contract tests assert every mock event this
//! module produces validates against it.
//!
//! Aokie conventions (AOKIE_PLUGIN_CONTRACT.md §3):
//!
//! * `source` / `pluginId` = `"aokie"`; names are `aokie.*`.
//! * `correlationId` = call id (`call_<uuid>`), SMS handle, or
//!   pairing session id — shared by all events of one interaction.
//! * `idempotencyKey` = `aokie:<correlationId>:<step>:v1`, e.g.
//!   `aokie:call_abc:incoming:v1`; turn events append the turn index
//!   (`aokie:call_abc:turn.4.final:v1`). Consumers dedupe on it, so
//!   the key must be STABLE for a given occurrence — never derive it
//!   from a timestamp or a fresh uuid.

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// The plugin/source id every Aokie event carries.
pub const AOKIE_SOURCE: &str = "aokie";

/// Envelope schema version — bump only in lockstep with
/// `desktop-event.schema.json` (`schemaVersion` is `const 1` there).
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Event severity, mirroring the schema's `severity` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSeverity {
    Debug,
    Info,
    Warning,
    Error,
}

/// The `desktop-event.schema.json` envelope. Field names serialise
/// camelCase to match the wire format exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopEvent {
    pub schema_version: u32,
    /// Origin subsystem: plugin id (e.g. "aokie") or "desktop" |
    /// "flows" | "models".
    pub source: String,
    /// Dot-namespaced event name, e.g. "aokie.call.incoming".
    pub name: String,
    /// Call/session/run id shared by all events of one interaction.
    pub correlation_id: String,
    /// Stable unique key for THIS event occurrence; consumers dedupe.
    pub idempotency_key: String,
    /// ISO-8601 UTC timestamp.
    pub occurred_at: String,
    /// Event-specific payload; PII must already be minimised.
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<EventSeverity>,
}

/// Current time in the envelope's `occurredAt` format (ISO-8601 UTC,
/// millisecond precision, `Z` suffix).
pub fn now_iso8601() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `now + secs` (negative = past) in the SAME fixed-width RFC3339/UTC shape
/// as [`now_iso8601`]. Fixed width + UTC means these strings compare
/// lexicographically in chronological order — the outbox's retry/prune
/// queries rely on that instead of SQLite date parsing.
pub fn iso8601_after_secs(secs: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `now - ms` in the same shape — used to BACK-DATE events whose true start
/// is known from elapsed audio (an overlap-captured caller turn began about
/// its own duration before it was committed to the transcript).
pub fn iso8601_ago_ms(ms: u64) -> String {
    (Utc::now() - chrono::Duration::milliseconds(ms as i64))
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Build the Aokie idempotency key: `aokie:<correlationId>:<step>:v1`.
///
/// `step` is the event name minus the `aokie.` prefix, with the
/// additional convention that call-scoped events also drop `call.`
/// because the correlation id IS the call id (contract example:
/// `aokie:call_abc:incoming:v1`, `aokie:call_abc:turn.4.final:v1`).
/// Callers pass the step explicitly — this helper only assembles.
pub fn aokie_idempotency_key(correlation_id: &str, step: &str) -> String {
    format!("aokie:{correlation_id}:{step}:v1")
}

/// Derive the idempotency-key step from an `aokie.*` event name:
/// strips the `aokie.` prefix, and for `call.*` events also strips
/// `call.` (the correlation id already names the call). Turn events
/// must NOT use this — their step carries the turn index, which the
/// name alone doesn't (use [`aokie_turn_event`]).
pub fn step_for_event_name(name: &str) -> &str {
    let step = name.strip_prefix("aokie.").unwrap_or(name);
    step.strip_prefix("call.").unwrap_or(step)
}

/// Build an `aokie.*` event envelope with the standard conventions:
/// `source`/`pluginId` = "aokie", `schemaVersion` = 1, `occurredAt` =
/// now, `idempotencyKey` = `aokie:<correlationId>:<step>:v1` where
/// the step is derived from the name via [`step_for_event_name`].
///
/// ONLY for steps that occur at most once per correlation (e.g.
/// `incoming`/`ended` per call id). A step that can legitimately REPEAT
/// under one correlation — radio lifecycle under the shared `radio`
/// correlation, hardware errors, per-call audio up/down cycles — must
/// use [`aokie_event_occurrence`] instead, or every repeat collides on
/// the first occurrence's key and is silently dropped by the outbox and
/// the Desktop receipt dedupe (audit AOK-EVENT-001).
pub fn aokie_event(name: &str, correlation_id: &str, data: serde_json::Value) -> DesktopEvent {
    aokie_event_with_step(name, correlation_id, step_for_event_name(name), data)
}

/// Mint a compact immutable occurrence id (12 hex chars) for an incident
/// that can repeat under one correlation. Mint ONCE when the incident is
/// DETECTED and carry it in the event object for its whole life — never
/// re-mint per emission attempt — so re-delivery of one occurrence keeps
/// one key, while every new incident gets a fresh key (AOK-EVENT-001).
pub fn occurrence_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Build an `aokie.*` envelope whose idempotency-key step carries a
/// per-incident occurrence id: `aokie:<corr>:<step>.<occ>:v1`. Use for
/// steps that can repeat under one correlation; see [`aokie_event`].
pub fn aokie_event_occurrence(
    name: &str,
    correlation_id: &str,
    occurrence: &str,
    data: serde_json::Value,
) -> DesktopEvent {
    aokie_event_with_step(
        name,
        correlation_id,
        &format!("{}.{occurrence}", step_for_event_name(name)),
        data,
    )
}

/// Like [`aokie_event`] but with an explicit idempotency-key step —
/// needed when the step carries more than the name (turn indexes).
pub fn aokie_event_with_step(
    name: &str,
    correlation_id: &str,
    step: &str,
    data: serde_json::Value,
) -> DesktopEvent {
    DesktopEvent {
        schema_version: EVENT_SCHEMA_VERSION,
        source: AOKIE_SOURCE.to_string(),
        name: name.to_string(),
        correlation_id: correlation_id.to_string(),
        idempotency_key: aokie_idempotency_key(correlation_id, step),
        occurred_at: now_iso8601(),
        data,
        plugin_id: Some(AOKIE_SOURCE.to_string()),
        connector_id: Some(AOKIE_SOURCE.to_string()),
        severity: None,
    }
}

/// Build an `aokie.call.turn.partial` / `aokie.call.turn.final`
/// envelope. The step appends the 1-based turn index per the
/// contract: `aokie:call_abc:turn.4.final:v1`.
pub fn aokie_turn_event(
    final_turn: bool,
    correlation_id: &str,
    turn_index: u32,
    data: serde_json::Value,
) -> DesktopEvent {
    let kind = if final_turn { "final" } else { "partial" };
    aokie_event_with_step(
        &format!("aokie.call.turn.{kind}"),
        correlation_id,
        &format!("turn.{turn_index}.{kind}"),
        data,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idempotency_key_convention() {
        assert_eq!(
            aokie_idempotency_key("call_abc", "incoming"),
            "aokie:call_abc:incoming:v1"
        );
    }

    #[test]
    fn step_strips_aokie_and_call_prefixes() {
        assert_eq!(step_for_event_name("aokie.call.incoming"), "incoming");
        assert_eq!(step_for_event_name("aokie.call.answered"), "answered");
        assert_eq!(step_for_event_name("aokie.call.ended"), "ended");
        // Non-call events keep their subsystem prefix in the step.
        assert_eq!(step_for_event_name("aokie.dongle.detected"), "dongle.detected");
        assert_eq!(step_for_event_name("aokie.sms.received"), "sms.received");
        assert_eq!(step_for_event_name("aokie.hardware.error"), "hardware.error");
    }

    #[test]
    fn aokie_event_fills_conventions() {
        let ev = aokie_event("aokie.call.incoming", "call_abc", json!({"from": "+61...456"}));
        assert_eq!(ev.schema_version, 1);
        assert_eq!(ev.source, "aokie");
        assert_eq!(ev.plugin_id.as_deref(), Some("aokie"));
        assert_eq!(ev.connector_id.as_deref(), Some("aokie"));
        assert_eq!(ev.idempotency_key, "aokie:call_abc:incoming:v1");
        // occurredAt is ISO-8601 UTC with Z suffix.
        assert!(ev.occurred_at.ends_with('Z'), "got: {}", ev.occurred_at);
        assert!(ev.occurred_at.contains('T'));
    }

    /// Audit AOK-EVENT-001: repeatable incidents get distinct stable keys —
    /// two occurrences differ, one occurrence's key never changes.
    #[test]
    fn occurrence_events_are_distinct_per_incident_and_stable_per_occurrence() {
        let occ_a = occurrence_id();
        let occ_b = occurrence_id();
        assert_ne!(occ_a, occ_b, "each incident mints a fresh id");
        assert_eq!(occ_a.len(), 12);
        assert!(occ_a.chars().all(|c| c.is_ascii_hexdigit()));

        let first = aokie_event_occurrence("aokie.hardware.error", "radio", &occ_a, json!({"m": 1}));
        let second = aokie_event_occurrence("aokie.hardware.error", "radio", &occ_b, json!({"m": 2}));
        assert_eq!(first.idempotency_key, format!("aokie:radio:hardware.error.{occ_a}:v1"));
        assert_ne!(first.idempotency_key, second.idempotency_key);

        // Re-building the SAME occurrence (same id) reproduces the same key.
        let replay = aokie_event_occurrence("aokie.hardware.error", "radio", &occ_a, json!({"m": 1}));
        assert_eq!(replay.idempotency_key, first.idempotency_key);

        // call.* events still drop the call. prefix in the step.
        let audio = aokie_event_occurrence("aokie.call.audio.connected", "call_x", "abc123abc123", json!({}));
        assert_eq!(audio.idempotency_key, "aokie:call_x:audio.connected.abc123abc123:v1");
    }

    #[test]
    fn turn_event_appends_turn_index_to_step() {
        let ev = aokie_turn_event(true, "call_abc", 4, json!({"text": "hi"}));
        assert_eq!(ev.name, "aokie.call.turn.final");
        assert_eq!(ev.idempotency_key, "aokie:call_abc:turn.4.final:v1");
        let ev = aokie_turn_event(false, "call_abc", 2, json!({}));
        assert_eq!(ev.name, "aokie.call.turn.partial");
        assert_eq!(ev.idempotency_key, "aokie:call_abc:turn.2.partial:v1");
    }

    /// Wire format must be camelCase per desktop-event.schema.json —
    /// a snake_case field would be rejected (additionalProperties:
    /// false) and silently dropped by Desktop.
    #[test]
    fn envelope_serialises_camel_case() {
        let ev = aokie_event("aokie.call.incoming", "call_abc", json!(null));
        let v = serde_json::to_value(&ev).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "schemaVersion",
            "source",
            "name",
            "correlationId",
            "idempotencyKey",
            "occurredAt",
            "data",
            "pluginId",
            "connectorId",
        ] {
            assert!(obj.contains_key(key), "missing {key}: {v}");
        }
        assert!(!obj.contains_key("schema_version"));
        // severity is None → omitted entirely, not null.
        assert!(!obj.contains_key("severity"));
    }

    #[test]
    fn envelope_round_trips() {
        let ev = aokie_turn_event(true, "call_x", 1, json!({"text": "hello"}));
        let s = serde_json::to_string(&ev).unwrap();
        let back: DesktopEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }
}
