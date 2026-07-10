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

/// How long dead-lettered rows keep their (protected) payloads before they
/// are deleted (audit AOK-OUTBOX-001): long enough for an operator redrive
/// after a bad week, short enough that failed transcript/SMS payloads do not
/// accumulate indefinitely.
pub const DEAD_RETENTION_DAYS: i64 = 14;

// ── Payload protection at rest (audit AOK-OUTBOX-001) ──────────────────────
// Transcript and SMS bodies must not be readable by opening the SQLite file.
// Windows DPAPI (per-user scope): no key management, decryptable only in the
// operator's own user context. Stored as "dpapi1:<base64>"; rows WITHOUT the
// prefix are legacy plaintext and stay readable (migration-free) — every new
// write is protected. Durability beats secrecy for the business record, so a
// DPAPI failure stores plaintext with a LOUD log rather than losing the event.

const DPAPI_PREFIX: &str = "dpapi1:";

#[cfg(windows)]
fn protect_payload(plain: &str) -> String {
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        if CryptProtectData(
            &mut input,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut out,
        ) == 0
        {
            eprintln!("[aokie-plugin] DPAPI protect FAILED — storing outbox payload unprotected");
            return plain.to_string();
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let encoded = format!("{DPAPI_PREFIX}{}", b64_encode(slice));
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut core::ffi::c_void);
        encoded
    }
}

#[cfg(windows)]
fn unprotect_payload(stored: &str) -> String {
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let Some(b64) = stored.strip_prefix(DPAPI_PREFIX) else {
        return stored.to_string(); // legacy plaintext row
    };
    let Some(bytes) = b64_decode(b64) else {
        eprintln!("[aokie-plugin] outbox payload base64 is corrupt — treating as undecryptable");
        return String::new();
    };
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        if CryptUnprotectData(
            &mut input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut out,
        ) == 0
        {
            eprintln!("[aokie-plugin] DPAPI unprotect FAILED (different user context?) — payload unreadable");
            return String::new();
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let plain = String::from_utf8_lossy(slice).into_owned();
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut core::ffi::c_void);
        plain
    }
}

// The plugin ships Windows-only; non-Windows dev builds pass through.
#[cfg(not(windows))]
fn protect_payload(plain: &str) -> String {
    plain.to_string()
}
#[cfg(not(windows))]
fn unprotect_payload(stored: &str) -> String {
    stored.to_string()
}

// Minimal std-only base64 (standard alphabet, padded) — not worth a crate.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        B64.iter().position(|&b| b == c).map(|i| i as u32)
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 || bytes.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' { 0 } else { val(c)? };
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

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
        let payload = protect_payload(&serde_json::to_string(event).expect("DesktopEvent serialises"));
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
                    payload_json: unprotect_payload(&row.get::<_, String>(5)?),
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
                    payload_json: unprotect_payload(&row.get::<_, String>(5)?),
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
    /// TARGETED by default (audit AOK-OUTBOX-001): pass an idempotency key
    /// to revive one row; `None` (an explicit "all") revives the whole dead
    /// set, which [`prune_dead`](Self::prune_dead) keeps bounded.
    pub fn redrive_dead(&self, idempotency_key: Option<&str>) -> rusqlite::Result<usize> {
        match idempotency_key {
            Some(key) => self.conn.execute(
                "UPDATE aokie_outbox
                    SET status = 'pending', attempts = 0, next_attempt_at = NULL, last_error = NULL
                  WHERE status = 'dead' AND idempotency_key = ?1",
                params![key],
            ),
            None => self.conn.execute(
                "UPDATE aokie_outbox
                    SET status = 'pending', attempts = 0, next_attempt_at = NULL, last_error = NULL
                  WHERE status = 'dead'",
                [],
            ),
        }
    }

    /// Dead-letter retention (audit AOK-OUTBOX-001): failed transcript/SMS
    /// payloads must not sit in the file forever. Runs beside prune_sent.
    pub fn prune_dead(&self, retention_days: i64) -> rusqlite::Result<usize> {
        let cutoff = iso8601_after_secs(-(retention_days * 86_400));
        self.conn.execute(
            "DELETE FROM aokie_outbox WHERE status = 'dead' AND updated_at < ?1",
            params![cutoff],
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

        // Targeted redrive misses a wrong key, hits the right one, and the
        // explicit all-form works (audit AOK-OUTBOX-001).
        assert_eq!(ob.redrive_dead(Some("no-such-key")).unwrap(), 0);
        assert_eq!(ob.redrive_dead(Some(&dead.idempotency_key)).unwrap(), 1);
        assert_eq!(ob.redrive_dead(None).unwrap(), 0, "nothing left dead");

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
    /// Audit AOK-OUTBOX-001: payloads are protected at rest — the raw column
    /// must not contain the transcript text, while reads round-trip it.
    #[test]
    fn payloads_are_protected_at_rest_and_round_trip() {
        let ob = Outbox::open_in_memory().unwrap();
        let mut ev = event("corr-secret", "aokie.call.turn.final");
        ev.data = serde_json::json!({"text": "my card number is 4111"});
        ob.insert_pending(&ev, "desktop").unwrap();

        let raw: String = ob
            .conn
            .query_row("SELECT payload_json FROM aokie_outbox LIMIT 1", [], |r| r.get(0))
            .unwrap();
        if cfg!(windows) {
            assert!(raw.starts_with(DPAPI_PREFIX), "stored protected: {raw:.20}");
            assert!(!raw.contains("4111"), "PII must not be readable in the file");
        }
        let rows = ob.due_for_retry(10).unwrap();
        let back: DesktopEvent = serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(back.data["text"], "my card number is 4111", "reads round-trip");
    }

    /// Legacy plaintext rows (pre-protection installs) stay readable.
    #[test]
    fn legacy_plaintext_rows_still_read() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("corr-legacy", "aokie.call.ended");
        let plain = serde_json::to_string(&ev).unwrap();
        ob.conn
            .execute(
                "INSERT INTO aokie_outbox (event_name, correlation_id, idempotency_key, target,
                    payload_json, status, attempts, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'desktop', ?4, 'pending', 0, ?5, ?5)",
                params![ev.name, ev.correlation_id, ev.idempotency_key, plain, now_iso8601()],
            )
            .unwrap();
        let rows = ob.due_for_retry(10).unwrap();
        let back: DesktopEvent = serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(back.idempotency_key, ev.idempotency_key);
    }

    /// Audit AOK-OUTBOX-001: dead rows expire after retention; fresh ones stay.
    #[test]
    fn prune_dead_expires_only_old_rows() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("corr-dead-old", "aokie.call.ended");
        ob.insert_pending(&ev, "desktop").unwrap();
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&ev.idempotency_key, "boom").unwrap();
        }
        assert_eq!(ob.counts().unwrap().dead, 1);
        // Fresh dead row survives…
        assert_eq!(ob.prune_dead(DEAD_RETENTION_DAYS).unwrap(), 0);
        // …an aged one goes.
        let old = iso8601_after_secs(-((DEAD_RETENTION_DAYS + 1) * 86_400));
        ob.conn
            .execute("UPDATE aokie_outbox SET updated_at = ?1", params![old])
            .unwrap();
        assert_eq!(ob.prune_dead(DEAD_RETENTION_DAYS).unwrap(), 1);
        assert_eq!(ob.counts().unwrap().dead, 0);
    }

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
