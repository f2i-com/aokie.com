//! Local SQLite outbox (AOKIE_PLUGIN_CONTRACT.md §5).
//!
//! Every **essential raw record** event (`call.incoming`,
//! `call.answered`, `call.turn.final`, `call.ended`, `sms.received`,
//! `sms.sent`, `hardware.error`) is written here BEFORE being emitted
//! to Desktop, so a Desktop crash/restart can never lose the record.
//! Successful emission marks the row `sent`; failures increment
//! `attempts` with exponential-backoff bookkeeping until
//! [`MAX_ATTEMPTS`], after which the row goes `dead` (surfaced via
//! `dongle.diagnostics`). `idempotency_key` is UNIQUE — re-inserting
//! the same occurrence is a no-op, matching consumer-side dedupe.

use std::path::Path;

use aokie_core::events::{now_iso8601, DesktopEvent};
use rusqlite::{params, Connection, OptionalExtension};

/// After this many failed emission attempts a row goes `dead` and
/// stays until an operator intervenes (post-MVP: re-drive UI).
pub const MAX_ATTEMPTS: u32 = 8;

/// Default emission target. Post-MVP the outbox gains a second
/// target ("formlogic") for direct API submission.
pub const TARGET_DESKTOP: &str = "desktop";

/// Row status lifecycle: `pending → sent` on success, `pending →
/// failed → … → dead` on repeated failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxStatus {
    Pending,
    Sent,
    Failed,
    Dead,
}

impl OutboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxStatus::Pending => "pending",
            OutboxStatus::Sent => "sent",
            OutboxStatus::Failed => "failed",
            OutboxStatus::Dead => "dead",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => OutboxStatus::Pending,
            "sent" => OutboxStatus::Sent,
            "failed" => OutboxStatus::Failed,
            "dead" => OutboxStatus::Dead,
            _ => return None,
        })
    }
}

/// One outbox row, as read back for retry loops / diagnostics.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub id: i64,
    pub event_name: String,
    pub correlation_id: String,
    pub idempotency_key: String,
    pub target: String,
    pub payload_json: String,
    pub status: OutboxStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Per-status row counts for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboxCounts {
    pub pending: u64,
    pub sent: u64,
    pub failed: u64,
    pub dead: u64,
}

pub struct Outbox {
    conn: Connection,
}

impl Outbox {
    /// Open (creating if needed) the outbox database at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::with_connection(conn)
    }

    /// In-memory outbox for tests.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS aokie_outbox (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                event_name      TEXT NOT NULL,
                correlation_id  TEXT NOT NULL,
                idempotency_key TEXT NOT NULL UNIQUE,
                target          TEXT NOT NULL DEFAULT 'desktop',
                payload_json    TEXT NOT NULL,
                status          TEXT NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending','sent','failed','dead')),
                attempts        INTEGER NOT NULL DEFAULT 0,
                last_error      TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_aokie_outbox_status
                ON aokie_outbox (status, updated_at);",
        )?;
        Ok(Outbox { conn })
    }

    /// Write-before-emit: insert the event as `pending`. Returns
    /// `true` if a new row was created, `false` when the
    /// idempotency key already exists (crash/retry duplicate — the
    /// existing row, whatever its status, is authoritative).
    pub fn insert_pending(&self, event: &DesktopEvent, target: &str) -> rusqlite::Result<bool> {
        let now = now_iso8601();
        let payload = serde_json::to_string(event).expect("DesktopEvent serialises");
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO aokie_outbox
                (event_name, correlation_id, idempotency_key, target,
                 payload_json, status, attempts, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6)",
            params![
                event.name,
                event.correlation_id,
                event.idempotency_key,
                target,
                payload,
                now
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Mark an emission success.
    pub fn mark_sent(&self, idempotency_key: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE aokie_outbox SET status = 'sent', last_error = NULL, updated_at = ?2
             WHERE idempotency_key = ?1",
            params![idempotency_key, now_iso8601()],
        )?;
        Ok(())
    }

    /// Record an emission failure: increments `attempts`, stores the
    /// error, and flips to `dead` once [`MAX_ATTEMPTS`] is reached.
    /// Returns the resulting status.
    pub fn mark_failed(
        &self,
        idempotency_key: &str,
        error: &str,
    ) -> rusqlite::Result<OutboxStatus> {
        self.conn.execute(
            "UPDATE aokie_outbox SET
                attempts = attempts + 1,
                last_error = ?2,
                status = CASE WHEN attempts + 1 >= ?3 THEN 'dead' ELSE 'failed' END,
                updated_at = ?4
             WHERE idempotency_key = ?1",
            params![idempotency_key, error, MAX_ATTEMPTS, now_iso8601()],
        )?;
        Ok(self
            .status_of(idempotency_key)?
            .unwrap_or(OutboxStatus::Failed))
    }

    /// Backoff before retry attempt `attempts + 1`: 1s, 2s, 4s …
    /// capped at 5 minutes. Pure bookkeeping — the caller owns the
    /// timer (the plugin's retry loop, post-MVP a background thread).
    pub fn backoff_secs(attempts: u32) -> u64 {
        1u64.checked_shl(attempts).unwrap_or(u64::MAX).min(300)
    }

    /// Rows eligible for (re-)emission: `pending` or `failed`,
    /// oldest first. `dead` rows are excluded — they need operator
    /// attention, not another timer.
    pub fn retryable(&self, limit: u32) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_name, correlation_id, idempotency_key, target,
                    payload_json, status, attempts, last_error, created_at, updated_at
             FROM aokie_outbox
             WHERE status IN ('pending', 'failed')
             ORDER BY id ASC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |row| {
                let status: String = row.get(6)?;
                Ok(OutboxRow {
                    id: row.get(0)?,
                    event_name: row.get(1)?,
                    correlation_id: row.get(2)?,
                    idempotency_key: row.get(3)?,
                    target: row.get(4)?,
                    payload_json: row.get(5)?,
                    status: OutboxStatus::from_str(&status).unwrap_or(OutboxStatus::Failed),
                    attempts: row.get(7)?,
                    last_error: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn status_of(&self, idempotency_key: &str) -> rusqlite::Result<Option<OutboxStatus>> {
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM aokie_outbox WHERE idempotency_key = ?1",
                params![idempotency_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(status.as_deref().and_then(OutboxStatus::from_str))
    }

    /// Per-status counts — surfaced through `dongle.diagnostics` so
    /// `dead` rows are visible without opening the DB by hand.
    pub fn counts(&self) -> rusqlite::Result<OutboxCounts> {
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM aokie_outbox GROUP BY status")?;
        let mut counts = OutboxCounts::default();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (status, n) = row?;
            match status.as_str() {
                "pending" => counts.pending = n,
                "sent" => counts.sent = n,
                "failed" => counts.failed = n,
                "dead" => counts.dead = n,
                _ => {}
            }
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aokie_core::events::aokie_event;
    use serde_json::json;

    fn event(corr: &str, name: &str) -> DesktopEvent {
        aokie_event(name, corr, json!({"t": 1}))
    }

    #[test]
    fn insert_starts_pending_and_dedupes_on_idempotency_key() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_a", "aokie.call.incoming");
        assert!(ob.insert_pending(&ev, TARGET_DESKTOP).unwrap());
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Pending)
        );
        // Same occurrence again (crash/retry): ignored, still one row.
        assert!(!ob.insert_pending(&ev, TARGET_DESKTOP).unwrap());
        let counts = ob.counts().unwrap();
        assert_eq!(counts.pending, 1);
    }

    #[test]
    fn mark_sent_transitions() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_b", "aokie.call.ended");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        ob.mark_sent(&ev.idempotency_key).unwrap();
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
        assert_eq!(ob.counts().unwrap().sent, 1);
    }

    #[test]
    fn repeated_failures_go_dead_at_max_attempts() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_c", "aokie.sms.sent");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        for attempt in 1..MAX_ATTEMPTS {
            let status = ob.mark_failed(&ev.idempotency_key, "desktop down").unwrap();
            assert_eq!(status, OutboxStatus::Failed, "attempt {attempt}");
        }
        let status = ob.mark_failed(&ev.idempotency_key, "desktop down").unwrap();
        assert_eq!(status, OutboxStatus::Dead);
        let counts = ob.counts().unwrap();
        assert_eq!(counts.dead, 1);
        assert_eq!(counts.failed, 0);
    }

    #[test]
    fn failed_rows_are_retryable_dead_rows_are_not() {
        let ob = Outbox::open_in_memory().unwrap();
        let alive = event("call_d", "aokie.call.incoming");
        let dying = event("call_e", "aokie.call.incoming");
        ob.insert_pending(&alive, TARGET_DESKTOP).unwrap();
        ob.insert_pending(&dying, TARGET_DESKTOP).unwrap();
        ob.mark_failed(&alive.idempotency_key, "once").unwrap();
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&dying.idempotency_key, "always").unwrap();
        }
        let retryable = ob.retryable(10).unwrap();
        let keys: Vec<_> = retryable
            .iter()
            .map(|r| r.idempotency_key.as_str())
            .collect();
        assert!(keys.contains(&alive.idempotency_key.as_str()));
        assert!(!keys.contains(&dying.idempotency_key.as_str()));
        assert_eq!(retryable[0].attempts, 1);
        assert_eq!(retryable[0].last_error.as_deref(), Some("once"));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(Outbox::backoff_secs(0), 1);
        assert_eq!(Outbox::backoff_secs(1), 2);
        assert_eq!(Outbox::backoff_secs(3), 8);
        assert_eq!(Outbox::backoff_secs(20), 300);
        assert_eq!(Outbox::backoff_secs(200), 300); // no shl overflow
    }

    #[test]
    fn payload_round_trips_through_the_outbox() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_f", "aokie.call.turn.final");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        let rows = ob.retryable(1).unwrap();
        let back: DesktopEvent = serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(back, ev);
        assert_eq!(rows[0].target, TARGET_DESKTOP);
    }
}
