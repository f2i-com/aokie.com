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

use aokie_core::events::{iso8601_after_secs, now_iso8601, DesktopEvent};
use rusqlite::{params, Connection, OptionalExtension};

/// After this many failed emission attempts a row goes `dead` and
/// stays until an operator intervenes (post-MVP: re-drive UI).
pub const MAX_ATTEMPTS: u32 = 8;

/// Acknowledged (`sent`) rows older than this are pruned by the replay
/// loop — the outbox is a delivery ledger, not an archive, and sent rows
/// hold transcript/SMS text (audit C-06: bounded PII retention).
pub const SENT_RETENTION_DAYS: i64 = 7;

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
    /// Open (creating if needed) the outbox database at `path`. File-backed
    /// databases run in WAL mode with a busy timeout so the plugin's RPC
    /// thread, radio thread and replay thread (each with its own connection)
    /// can share the file without `SQLITE_BUSY` failures (audit INT-003).
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // journal_mode returns the resulting mode as a row — query it.
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL;")?;
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
        // v2 migration (ack mode): when the row may next be (re-)emitted,
        // RFC3339 UTC like created_at/updated_at (lexicographic-comparable).
        // NULL = due immediately. ALTER is idempotent-by-error: a duplicate
        // column just means the migration already ran.
        match conn.execute_batch("ALTER TABLE aokie_outbox ADD COLUMN next_attempt_at TEXT;") {
            Ok(()) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e),
        }
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
        let next = iso8601_after_secs(Self::next_backoff_secs(self.attempts_of(idempotency_key)?));
        self.conn.execute(
            "UPDATE aokie_outbox SET
                attempts = attempts + 1,
                last_error = ?2,
                status = CASE WHEN attempts + 1 >= ?3 THEN 'dead' ELSE 'failed' END,
                updated_at = ?4,
                next_attempt_at = ?5
             WHERE idempotency_key = ?1",
            params![idempotency_key, error, MAX_ATTEMPTS, now_iso8601(), next],
        )?;
        Ok(self
            .status_of(idempotency_key)?
            .unwrap_or(OutboxStatus::Failed))
    }

    /// Ack mode (audit INT-003): record that the event was WRITTEN to the
    /// host but not yet acknowledged. Increments `attempts` and schedules the
    /// next re-emission with exponential backoff; the row stays `pending`
    /// until the host's `event.ack` marks it `sent` — or goes `dead` after
    /// [`MAX_ATTEMPTS`] unacknowledged emissions. Returns the new status.
    pub fn mark_emitted(&self, idempotency_key: &str) -> rusqlite::Result<OutboxStatus> {
        let next = iso8601_after_secs(Self::next_backoff_secs(self.attempts_of(idempotency_key)?));
        self.conn.execute(
            "UPDATE aokie_outbox SET
                attempts = attempts + 1,
                status = CASE WHEN attempts + 1 >= ?2 THEN 'dead' ELSE status END,
                updated_at = ?3,
                next_attempt_at = ?4
             WHERE idempotency_key = ?1",
            params![idempotency_key, MAX_ATTEMPTS, now_iso8601(), next],
        )?;
        Ok(self
            .status_of(idempotency_key)?
            .unwrap_or(OutboxStatus::Pending))
    }

    fn attempts_of(&self, idempotency_key: &str) -> rusqlite::Result<u32> {
        Ok(self
            .conn
            .query_row(
                "SELECT attempts FROM aokie_outbox WHERE idempotency_key = ?1",
                params![idempotency_key],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Seconds to wait before the attempt AFTER the current count.
    fn next_backoff_secs(attempts: u32) -> i64 {
        Self::backoff_secs(attempts) as i64
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

    /// Rows DUE for (re-)emission by the ack-mode replay loop: `pending` or
    /// `failed`, whose `next_attempt_at` is unset (never emitted) or in the
    /// past. Oldest first. `sent`/`dead` rows never re-emit.
    pub fn due_for_retry(&self, limit: u32) -> rusqlite::Result<Vec<OutboxRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_name, correlation_id, idempotency_key, target,
                    payload_json, status, attempts, last_error, created_at, updated_at
             FROM aokie_outbox
             WHERE status IN ('pending', 'failed')
               AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
             ORDER BY id ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![now_iso8601(), limit], |row| {
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

    /// Delete acknowledged rows older than `days` (bounded PII retention —
    /// audit C-06). Returns how many rows were removed.
    pub fn prune_sent(&self, days: i64) -> rusqlite::Result<usize> {
        let cutoff = iso8601_after_secs(-days * 86_400);
        let n = self.conn.execute(
            "DELETE FROM aokie_outbox WHERE status = 'sent' AND updated_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }

    /// TEST-ONLY: rewind a row's `next_attempt_at` to the past so retry
    /// tests can cross the backoff window without sleeping.
    #[cfg(test)]
    pub(crate) fn rewind_next_attempt_for_tests(&self, idempotency_key: &str) {
        let _ = self.conn.execute(
            "UPDATE aokie_outbox SET next_attempt_at = ?2 WHERE idempotency_key = ?1",
            params![idempotency_key, iso8601_after_secs(-60)],
        );
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

    /// Operator redrive (audit OBS-001): dead-lettered rows go back to
    /// `pending` with a fresh attempt budget, so the replay thread delivers
    /// them again. Explicitly operator-triggered — never automatic, or the
    /// dead-letter state would mean nothing. Returns how many rows revived.
    pub fn redrive_dead(&self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE aokie_outbox
                SET status = 'pending', attempts = 0, next_attempt_at = NULL, last_error = NULL
              WHERE status = 'dead'",
            [],
        )
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

    /// Audit OBS-001: operator redrive revives dead rows into the normal
    /// retry pipeline; sent/pending rows are untouched.
    #[test]
    fn redrive_revives_only_dead_rows() {
        let ob = Outbox::open_in_memory().unwrap();
        let dead = event("corr-dead", "aokie.call.ended");
        let sent = event("corr-sent", "aokie.call.ended");
        ob.insert_pending(&dead, "desktop").unwrap();
        ob.insert_pending(&sent, "desktop").unwrap();
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&dead.idempotency_key, "boom").unwrap();
        }
        ob.mark_emitted(&sent.idempotency_key).unwrap();
        ob.mark_sent(&sent.idempotency_key).unwrap();
        assert_eq!(ob.counts().unwrap().dead, 1);

        assert_eq!(ob.redrive_dead().unwrap(), 1);

        let counts = ob.counts().unwrap();
        assert_eq!((counts.dead, counts.pending, counts.sent), (0, 1, 1));
        assert_eq!(
            ob.due_for_retry(10).unwrap().len(),
            1,
            "a redriven row re-enters the retry pipeline immediately"
        );
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

    /// Ack mode (audit INT-003): an emitted-but-unacknowledged row stays
    /// `pending`, is NOT due again until its backoff elapses, and becomes
    /// `sent` only via the host's ack (`mark_sent`).
    #[test]
    fn mark_emitted_keeps_pending_until_acked_and_gates_on_backoff() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_g", "aokie.call.incoming");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();

        // Never emitted → due immediately (next_attempt_at is NULL).
        assert_eq!(ob.due_for_retry(10).unwrap().len(), 1);

        // Emitted once: still pending (awaiting ack), but no longer due —
        // backoff(0) = 1s is in the future.
        assert_eq!(
            ob.mark_emitted(&ev.idempotency_key).unwrap(),
            OutboxStatus::Pending
        );
        assert!(ob.due_for_retry(10).unwrap().is_empty(), "backoff gates re-emission");

        // The host's ack arrives → sent, and never due again.
        ob.mark_sent(&ev.idempotency_key).unwrap();
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
        assert!(ob.due_for_retry(10).unwrap().is_empty());
    }

    /// A row that is never acknowledged dead-letters after MAX_ATTEMPTS
    /// emissions instead of re-delivering forever.
    #[test]
    fn unacknowledged_rows_dead_letter_after_max_attempts() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_h", "aokie.call.ended");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        for _ in 0..(MAX_ATTEMPTS - 1) {
            assert_eq!(
                ob.mark_emitted(&ev.idempotency_key).unwrap(),
                OutboxStatus::Pending
            );
        }
        assert_eq!(
            ob.mark_emitted(&ev.idempotency_key).unwrap(),
            OutboxStatus::Dead
        );
        assert!(ob.due_for_retry(10).unwrap().is_empty(), "dead rows never re-emit");
    }

    /// Acked rows past retention are pruned (bounded PII — audit C-06);
    /// recent and undelivered rows are untouched.
    #[test]
    fn prune_sent_removes_only_old_acknowledged_rows() {
        let ob = Outbox::open_in_memory().unwrap();
        let old = event("call_i", "aokie.call.incoming");
        let fresh = event("call_j", "aokie.call.incoming");
        let pending = event("call_k", "aokie.call.incoming");
        for ev in [&old, &fresh, &pending] {
            ob.insert_pending(ev, TARGET_DESKTOP).unwrap();
        }
        ob.mark_sent(&old.idempotency_key).unwrap();
        ob.mark_sent(&fresh.idempotency_key).unwrap();
        // Backdate the old row past retention.
        ob.conn
            .execute(
                "UPDATE aokie_outbox SET updated_at = ?2 WHERE idempotency_key = ?1",
                params![
                    old.idempotency_key,
                    iso8601_after_secs(-(SENT_RETENTION_DAYS + 1) * 86_400)
                ],
            )
            .unwrap();

        assert_eq!(ob.prune_sent(SENT_RETENTION_DAYS).unwrap(), 1);
        assert_eq!(ob.status_of(&old.idempotency_key).unwrap(), None, "pruned");
        assert_eq!(
            ob.status_of(&fresh.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
        assert_eq!(
            ob.status_of(&pending.idempotency_key).unwrap(),
            Some(OutboxStatus::Pending),
            "undelivered rows are never pruned"
        );
    }

    /// The v2 column migration is idempotent: re-opening an existing
    /// database (simulated by reusing a file path) must not fail.
    #[test]
    fn reopening_a_migrated_database_is_idempotent() {
        let path = std::env::temp_dir().join(format!(
            "aokie-outbox-migrate-{}-{}.sqlite",
            std::process::id(),
            now_iso8601().replace(':', "-")
        ));
        {
            let ob = Outbox::open(&path).unwrap();
            let ev = event("call_l", "aokie.call.incoming");
            ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        }
        {
            let ob = Outbox::open(&path).unwrap();
            assert_eq!(ob.counts().unwrap().pending, 1, "data survives reopen");
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
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
