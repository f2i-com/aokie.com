//! Local SQLite outbox (AOKIE_PLUGIN_CONTRACT.md §5).
//!
//! Every **essential raw record** event (`call.incoming`,
//! `call.answered`, `call.turn.final`, `call.ended`, `sms.received`,
//! `sms.sent`, `hardware.error`) is written here BEFORE being emitted
//! to Desktop, so a Desktop crash/restart can never lose the record.
//! Successful emission marks the row `sent`; failures increment
//! `attempts` with exponential-backoff bookkeeping until
//! [`MAX_ATTEMPTS`] over [`RETRY_WINDOW_SECS`], after which the row goes `dead` (surfaced via
//! `dongle.diagnostics`). `idempotency_key` is UNIQUE — re-inserting
//! the same occurrence is a no-op, matching consumer-side dedupe.

use std::path::Path;

use aokie_core::events::{iso8601_after_secs, now_iso8601, DesktopEvent};
use rusqlite::{params, Connection, OptionalExtension};

/// After this many failed emission attempts a row goes `dead` and
/// stays until an operator intervenes (post-MVP: re-drive UI) — but only
/// once it is also older than [`RETRY_WINDOW_SECS`]. Eight attempts at a
/// 1-64 s backoff were over in about two minutes, so a host busy for that
/// long dead-lettered every event, where the contract says an event is
/// re-delivered until the host acknowledges it.
pub const MAX_ATTEMPTS: u32 = 8;

/// How long an unacknowledged row keeps being re-delivered (the backoff
/// caps at 5 minutes, so about 860 tries) before it may go `dead`: three
/// days, a long weekend with the host's journal broken.
pub const RETRY_WINDOW_SECS: i64 = 3 * 86_400;

/// Acknowledged (`sent`) rows older than this are pruned by the replay
/// loop — the outbox is a delivery ledger, not an archive, and sent rows
/// hold transcript/SMS text (audit C-06: bounded PII retention).
pub const SENT_RETENTION_DAYS: i64 = 7;

/// How long dead-lettered rows keep their (protected) payloads before they
/// are deleted (audit AOK-OUTBOX-001): long enough for an operator redrive
/// after a bad week, short enough that failed transcript/SMS payloads do not
/// accumulate indefinitely.
pub const DEAD_RETENTION_DAYS: i64 = 14;

// ── Payload protection at rest (audit AOK-OUTBOX-001 / AOK-DUR-001) ────────
// Transcript and SMS bodies must not be readable by opening the SQLite file.
// Windows DPAPI (per-user scope): no key management, decryptable only in the
// operator's own user context; DPAPI master keys are rotated by the OS.
// Stored as "dpapi1:<base64>". Legacy plaintext rows are MIGRATED (sealed in
// place, per-row crash-safe/resumable, then the file is vacuumed so no
// plaintext pages survive) at open.
//
// AOK-DUR-001: there is NO silent plaintext fallback. A protect failure
// QUARANTINES the event (typed dead row, metadata retained, payload absent)
// instead of writing plaintext; a decrypt failure is a typed dead-letter
// (`payload_unreadable`), never an empty payload emitted as though real.
// macOS seals with a key held in the user's login Keychain
// (`aokie_core::dpapi`'s macOS half), stored as "keychain1:<base64>", under
// the same rules: a Keychain that will not give the key quarantines the event.
// A quarantine at write is not for good while the process runs: the event is
// held in memory (never on disk) and sealed again once the key is given (a
// prompt answered late, a Keychain unlocked after login), when its row is
// filled in and delivered as any other ([`HELD_SEALS`]).
// Linux builds have no OS payload protection: file-backed outboxes
// refuse sensitive writes unless `AOKIE_ALLOW_UNPROTECTED_OUTBOX=1` makes
// the dev trade-off EXPLICIT; in-memory (test) outboxes use dev plaintext.

const DPAPI_PREFIX: &str = "dpapi1:";
/// Marker payload for rows whose event could not be protected at write time
/// (AOK-DUR-001 quarantine): the row keeps its non-sensitive metadata for
/// repair, but there is deliberately nothing to emit or redrive.
const QUARANTINED_PAYLOAD: &str = "quarantined:";

/// An event whose payload could not be sealed when it was written: its
/// quarantine row is in the file, and the event itself is here, in this
/// process's memory only, until a seal works.
struct HeldSeal {
    /// Which outbox file (or in-memory outbox) the row is in.
    store: String,
    event: DesktopEvent,
    target: String,
}

/// The events waiting for a seal, for every connection of this process: the
/// radio's connection writes most of them and the replay thread's, which
/// reads every second, is the one that finds the key back. Lost if the
/// process ends first: the quarantine row then stays dead, as it says.
static HELD_SEALS: std::sync::Mutex<Vec<HeldSeal>> = std::sync::Mutex::new(Vec::new());

/// At most this many events wait for a seal; one past it stays quarantined.
const MAX_HELD_SEALS: usize = 1000;

/// How often a read asks again for the seal of what is held (a write whose
/// own seal worked seals them at once).
const RESEAL_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Names each in-memory outbox (tests), whose held events are its own.
static MEMORY_STORES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn held_seals() -> std::sync::MutexGuard<'static, Vec<HeldSeal>> {
    HELD_SEALS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How this outbox seals payloads at rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadProtection {
    /// Windows DPAPI (per-user). The production mode.
    Dpapi,
    /// macOS: a key in the user's login Keychain (`aokie_core::dpapi`).
    Keychain,
    /// Explicit dev/test plaintext (non-Windows with the env override, and
    /// in-memory test outboxes). Loud, never the silent default for files.
    DevPlaintext,
    /// No protection available: sensitive writes are refused (quarantined).
    Unavailable,
}

/// Protection mode for a FILE-backED outbox on this platform.
fn platform_protection() -> PayloadProtection {
    #[cfg(windows)]
    {
        PayloadProtection::Dpapi
    }
    #[cfg(not(windows))]
    {
        // A test's file-backed outbox is there for what SQLite does (two
        // connections to one file, a reopen), not for sealing: it gets what
        // `open_in_memory` gets off Windows. On a Mac that also keeps a test
        // run out of the developer's Keychain.
        if cfg!(test) {
            return PayloadProtection::DevPlaintext;
        }
        if std::env::var("AOKIE_ALLOW_UNPROTECTED_OUTBOX").as_deref() == Ok("1") {
            eprintln!(
                "[aokie-plugin] AOKIE_ALLOW_UNPROTECTED_OUTBOX=1 — outbox payloads stored PLAINTEXT (dev override)"
            );
            PayloadProtection::DevPlaintext
        } else if cfg!(any(target_os = "macos", target_os = "linux")) {
            // (the Keychain's key on a Mac, the Secret Service's on Linux)
            PayloadProtection::Keychain
        } else {
            PayloadProtection::Unavailable
        }
    }
}

#[cfg(windows)]
fn dpapi_protect(plain: &str) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        if CryptProtectData(
            &mut input,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // Never a dialog: this runs headless, and a prompt nobody sees would hang the call.
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        ) == 0
        {
            return Err("DPAPI protect failed".to_string());
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let encoded = format!("{DPAPI_PREFIX}{}", b64_encode(slice));
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut core::ffi::c_void);
        Ok(encoded)
    }
}

#[cfg(windows)]
fn dpapi_unprotect(b64: &str) -> Result<String, String> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let bytes = b64_decode(b64).ok_or_else(|| "payload base64 is corrupt".to_string())?;
    unsafe {
        let mut input = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        if CryptUnprotectData(
            &mut input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // Never a dialog: this runs headless, and a prompt nobody sees would hang the call.
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        ) == 0
        {
            return Err("DPAPI unprotect failed (different user context?)".to_string());
        }
        let slice = std::slice::from_raw_parts(out.pbData, out.cbData as usize);
        let plain = String::from_utf8_lossy(slice).into_owned();
        windows_sys::Win32::Foundation::LocalFree(out.pbData as *mut core::ffi::c_void);
        Ok(plain)
    }
}

/// Seal a payload for storage. `Err` = the caller must QUARANTINE the event —
/// plaintext is never written as a fallback (AOK-DUR-001).
fn protect_payload(mode: PayloadProtection, plain: &str) -> Result<String, String> {
    match mode {
        #[cfg(windows)]
        PayloadProtection::Dpapi => dpapi_protect(plain),
        #[cfg(not(windows))]
        PayloadProtection::Dpapi => Err("DPAPI is not available on this platform".to_string()),
        // (the core's sealing: on a Mac the Keychain's key, on Linux the Secret Service's; an error on a system that
        // has none)
        PayloadProtection::Keychain => aokie_core::dpapi::protect(plain.as_bytes()),
        PayloadProtection::DevPlaintext => Ok(plain.to_string()),
        PayloadProtection::Unavailable => Err(
            "no OS payload protection on this platform (set AOKIE_ALLOW_UNPROTECTED_OUTBOX=1 to accept plaintext in dev)"
                .to_string(),
        ),
    }
}

/// Open a stored payload. `Err` = typed unreadable (quarantined at write,
/// corrupt, or undecryptable) — the caller dead-letters, it never emits an
/// empty replacement (AOK-DUR-001).
fn unprotect_payload(stored: &str) -> Result<String, String> {
    if let Some(reason) = stored.strip_prefix(QUARANTINED_PAYLOAD) {
        return Err(format!("payload was quarantined at write ({reason})"));
    }
    if stored.starts_with(aokie_core::dpapi::KEYCHAIN_PREFIX) {
        // Sealed on a Mac or on Linux. Elsewhere this is an error, never plaintext.
        let bytes = aokie_core::dpapi::unprotect(stored)?;
        return String::from_utf8(bytes)
            .map_err(|_| "sealed payload is not text once opened".to_string());
    }
    if let Some(_b64) = stored.strip_prefix(DPAPI_PREFIX) {
        #[cfg(windows)]
        {
            return dpapi_unprotect(_b64);
        }
        #[cfg(not(windows))]
        {
            return Err("protected payload requires Windows DPAPI".to_string());
        }
    }
    Ok(stored.to_string()) // legacy plaintext row (pre-migration)
}

// Minimal std-only base64 (standard alphabet, padded) — not worth a crate.
// (The DPAPI token's: the other systems' sealed tokens are made in aokie-core.)
#[cfg_attr(not(windows), allow(dead_code))]
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg_attr(not(windows), allow(dead_code))]
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg_attr(not(windows), allow(dead_code))]
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
///
/// `sent` is TERMINAL (audit AOK-EVENT-001): once the host has
/// acknowledged durable receipt, no replay/failure bookkeeping may move
/// the row backward — [`mark_failed`](Outbox::mark_failed) and
/// [`mark_emitted`](Outbox::mark_emitted) only touch `pending`/`failed`
/// rows. `dead` only leaves via an explicit operator
/// [`redrive_dead`](Outbox::redrive_dead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxStatus {
    Pending,
    Sent,
    Failed,
    Dead,
}

/// Outcome of an acknowledgement (audit AK-05): `sent` and `dead` are both
/// terminal — see [`Outbox::mark_sent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// pending|failed → sent: this ack delivered the event.
    Marked,
    /// The row was already `sent` — a duplicate ack; harmless and idempotent.
    AlreadySent,
    /// The row is terminal (`dead`/quarantined) — the ack changed NOTHING.
    RefusedTerminal,
    /// No outbox row carries this key — the ack referenced nothing we emitted.
    Unknown,
}

/// Outcome of [`Outbox::insert_pending`] (audit AOK-EVENT-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// New row created — first sighting of this occurrence.
    Inserted,
    /// The idempotency key already exists with the SAME payload content:
    /// a crash/retry duplicate of one occurrence. The existing row,
    /// whatever its status, is authoritative.
    Duplicate,
    /// The idempotency key already exists with DIFFERENT payload content —
    /// a key-derivation bug upstream (two distinct occurrences colliding).
    /// The existing row is kept, the new event is REJECTED, and the
    /// collision is counted for diagnostics. Never silent.
    PayloadCollision,
    /// AOK-DUR-001: payload protection failed (or is unavailable), so the
    /// event was QUARANTINED — a typed dead row with metadata but NO payload
    /// was written instead of plaintext. The caller must not emit the event
    /// as though it were durably stored. The event is held in memory and its
    /// row filled in once a seal works, while this process runs.
    QuarantinedProtectFailed,
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

/// Whether `e` says the file itself is damaged (not a database, or a
/// malformed one) rather than busy, locked or out of reach.
fn is_corrupt_database(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    )
}

/// SQLite's quick integrity check; a damaged file is reported as corrupt.
pub(crate) fn sqlite_quick_check(conn: &Connection) -> rusqlite::Result<()> {
    let verdict: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if verdict.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(format!("quick_check: {verdict}")),
        ))
    }
}

/// Open a database with `open`; when the file is damaged, move it (and its
/// WAL and shared-memory files) aside to `<file>.corrupt-<unix time>`, and
/// open a fresh one in its place. Any other error is returned unchanged.
pub(crate) fn open_or_quarantine<T>(
    path: &Path,
    open: impl Fn(&Path) -> rusqlite::Result<T>,
) -> Result<(T, Option<std::path::PathBuf>), String> {
    match open(path) {
        Ok(db) => Ok((db, None)),
        Err(e) if is_corrupt_database(&e) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let aside = |p: &Path| {
                let mut name = p.as_os_str().to_owned();
                name.push(format!(".corrupt-{stamp}"));
                std::path::PathBuf::from(name)
            };
            let moved = aside(path);
            std::fs::rename(path, &moved).map_err(|re| {
                format!("{} is corrupt ({e}) and could not be set aside: {re}", path.display())
            })?;
            for suffix in ["-wal", "-shm"] {
                let mut side = path.as_os_str().to_owned();
                side.push(suffix);
                let side = std::path::PathBuf::from(side);
                if side.exists() {
                    let _ = std::fs::rename(&side, aside(&side));
                }
            }
            eprintln!(
                "[aokie-plugin] {} was corrupt ({e}) — set aside as {} and started fresh",
                path.display(),
                moved.display()
            );
            let db = open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((db, Some(moved)))
        }
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub struct Outbox {
    conn: Connection,
    protection: PayloadProtection,
    /// While the sealing key could not be had: when to ask again, and how
    /// many times it has failed (see `open_payloads`).
    key_wait: std::cell::Cell<Option<(std::time::Instant, u32)>>,
    /// Which file this is (its full path), or `memory:<n>`: the key of the
    /// events it holds for a seal ([`HELD_SEALS`]).
    store: String,
    /// When a read last asked for the seal of what is held.
    resealed_at: std::cell::Cell<Option<std::time::Instant>>,
}

impl Drop for Outbox {
    fn drop(&mut self) {
        // An in-memory outbox's rows go with it, so do the events it held. A file's stay for its other
        // connections (the replay thread's, the next radio's).
        if self.store.starts_with("memory:") {
            held_seals().retain(|held| held.store != self.store);
        }
    }
}

impl Outbox {
    /// Open (creating if needed) the outbox database at `path`. File-backed
    /// databases run in WAL mode with a busy timeout so the plugin's RPC
    /// thread, radio thread and replay thread (each with its own connection)
    /// can share the file without `SQLITE_BUSY` failures (audit INT-003).
    /// Legacy plaintext payloads are sealed in place at open (AOK-DUR-001).
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // journal_mode returns the resulting mode as a row — query it.
        let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        // FULL, not NORMAL (audit AOK-OUTBOX-002): this ledger IS the
        // "never lose a business event" claim — NORMAL can lose the most
        // recent commits on power loss. The outbox writes a handful of rows
        // per call; the extra fsync is noise here.
        conn.execute_batch("PRAGMA synchronous=FULL;")?;
        let store = std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let outbox = Self::with_connection(conn, platform_protection(), store)?;
        outbox.migrate_legacy_plaintext();
        Ok(outbox)
    }

    /// [`open`](Self::open), and when the file is not a database or is
    /// corrupt (SQLite says so, or its quick check does), set it aside as
    /// `<file>.corrupt-<unix time>` (with its `-wal`/`-shm`) and start a fresh
    /// one: a damaged ledger must not stop the plugin from starting every
    /// time it is launched. Returns where the old file went, to be said in
    /// health. A busy, locked or unreadable file is NOT set aside (another
    /// process may hold it): that error is returned as before.
    pub fn open_or_quarantine(path: &Path) -> Result<(Self, Option<std::path::PathBuf>), String> {
        open_or_quarantine(path, |p| {
            let outbox = Self::open(p)?;
            sqlite_quick_check(&outbox.conn)?;
            Ok(outbox)
        })
    }

    /// In-memory outbox for tests: DPAPI on Windows (the real path),
    /// explicit dev plaintext elsewhere so the suite runs cross-platform.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let mode = if cfg!(windows) {
            PayloadProtection::Dpapi
        } else {
            PayloadProtection::DevPlaintext
        };
        Self::with_connection(Connection::open_in_memory()?, mode, Self::memory_store())
    }

    /// TEST-ONLY: an in-memory outbox with an explicit protection mode, so
    /// the protect-failure quarantine path is exercisable on every platform.
    #[cfg(test)]
    pub(crate) fn open_in_memory_with_protection(
        mode: PayloadProtection,
    ) -> rusqlite::Result<Self> {
        Self::with_connection(Connection::open_in_memory()?, mode, Self::memory_store())
    }

    /// TEST-ONLY: seal (or fail to) from here on as `mode` does: the key coming back, or going.
    #[cfg(test)]
    pub(crate) fn set_protection_for_tests(&mut self, mode: PayloadProtection) {
        self.protection = mode;
    }

    fn memory_store() -> String {
        let n = MEMORY_STORES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("memory:{n}")
    }

    fn with_connection(
        conn: Connection,
        protection: PayloadProtection,
        store: String,
    ) -> rusqlite::Result<Self> {
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
        // NULL = due immediately. Deterministic (audit AOK-OUTBOX-002): the
        // column's existence is CHECKED via table_info rather than matching
        // a locale/version-dependent error string.
        let has_next_attempt: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('aokie_outbox') WHERE name = 'next_attempt_at'")?
            .query_row([], |r| r.get::<_, i64>(0))
            .map(|n| n > 0)?;
        if !has_next_attempt {
            conn.execute_batch("ALTER TABLE aokie_outbox ADD COLUMN next_attempt_at TEXT;")?;
        }
        // v3 migration (audit AOK-EVENT-001): content fingerprint for the
        // collision tripwire — same key + different content is a key-derivation
        // bug, not a harmless duplicate. NULL on legacy rows (no comparison
        // possible). The meta table holds durable diagnostic counters.
        let has_payload_hash: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('aokie_outbox') WHERE name = 'payload_hash'")?
            .query_row([], |r| r.get::<_, i64>(0))
            .map(|n| n > 0)?;
        if !has_payload_hash {
            conn.execute_batch("ALTER TABLE aokie_outbox ADD COLUMN payload_hash TEXT;")?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS aokie_outbox_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        Ok(Outbox {
            conn,
            protection,
            key_wait: std::cell::Cell::new(None),
            store,
            resealed_at: std::cell::Cell::new(None),
        })
    }

    /// AOK-DUR-001 item 4: seal legacy plaintext rows in place. Per-row
    /// UPDATE with a decrypt-verify before commit — crash-safe and resumable
    /// (an interrupted run simply migrates the remainder next open). After a
    /// successful pass the WAL is checkpointed and the file vacuumed so no
    /// plaintext survives in old pages.
    fn migrate_legacy_plaintext(&self) {
        if !matches!(
            self.protection,
            PayloadProtection::Dpapi | PayloadProtection::Keychain
        ) {
            return; // nothing stronger to migrate TO on this platform/mode
        }
        let rows: Vec<(i64, String)> = {
            let Ok(mut stmt) = self.conn.prepare(
                "SELECT id, payload_json FROM aokie_outbox
                 WHERE payload_json NOT LIKE 'dpapi1:%'
                   AND payload_json NOT LIKE 'keychain1:%'
                   AND payload_json NOT LIKE 'quarantined:%'",
            ) else {
                return;
            };
            match stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map(|it| it.collect::<rusqlite::Result<Vec<_>>>())
            {
                Ok(Ok(rows)) => rows,
                _ => return,
            }
        };
        if rows.is_empty() {
            return;
        }
        let mut migrated = 0usize;
        for (id, plain) in &rows {
            let Ok(sealed) = protect_payload(self.protection, plain) else {
                eprintln!("[aokie-plugin] outbox migration: protect failed for row {id} — leaving as-is for the next attempt");
                continue;
            };
            // Reversible-until-verified: the plaintext row is only replaced
            // once the sealed copy provably reads back identical.
            match unprotect_payload(&sealed) {
                Ok(back) if back == *plain => {
                    let _ = self.conn.execute(
                        "UPDATE aokie_outbox SET payload_json = ?2 WHERE id = ?1",
                        params![id, sealed],
                    );
                    migrated += 1;
                }
                _ => eprintln!(
                    "[aokie-plugin] outbox migration: verify failed for row {id} — plaintext kept"
                ),
            }
        }
        if migrated > 0 {
            // Scrub old page images so no plaintext survives outside live
            // rows. Order matters in WAL mode: VACUUM rebuilds the database
            // (through the WAL), and only the FOLLOWING checkpoint replaces
            // the main file's old pages and truncates the WAL — a checkpoint
            // before the vacuum alone leaves stale plaintext pages behind.
            let _ = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_r| Ok(()));
            let _ = self.conn.execute_batch("VACUUM;");
            let _ = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_r| Ok(()));
            eprintln!(
                "[aokie-plugin] outbox migration: sealed {migrated} legacy plaintext payload(s) at rest"
            );
        }
    }

    /// Typed dead-letter transition (AOK-DUR-001): move a pending/failed row
    /// straight to `dead` with a machine-readable reason. `sent` rows are
    /// never touched.
    pub fn mark_dead_typed(&self, idempotency_key: &str, reason: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE aokie_outbox SET status = 'dead', last_error = ?2, updated_at = ?3
             WHERE idempotency_key = ?1 AND status IN ('pending', 'failed')",
            params![idempotency_key, reason, now_iso8601()],
        )?;
        Ok(())
    }

    /// Content fingerprint for the collision tripwire: name + correlation +
    /// payload data, EXCLUDING the envelope's `occurredAt` and any top-level
    /// `at` inside `data` — both are observation timestamps that legitimately
    /// differ when one occurrence is rebuilt (e.g. a re-fetched MAP message),
    /// while every other difference means two distinct occurrences collided
    /// on one key. FNV-1a 64 (tripwire, not crypto).
    fn payload_fingerprint(event: &DesktopEvent) -> String {
        let mut data = event.data.clone();
        if let Some(obj) = data.as_object_mut() {
            obj.remove("at");
        }
        let canonical = format!(
            "{}\n{}\n{}",
            event.name,
            event.correlation_id,
            serde_json::to_string(&data).unwrap_or_default()
        );
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in canonical.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("fnv1a64:{hash:016x}")
    }

    fn bump_meta_counter(&self, key: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO aokie_outbox_meta (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            params![key],
        )?;
        Ok(())
    }

    /// How many same-key/different-payload collisions this outbox has
    /// rejected — non-zero means a key-derivation bug upstream. Surfaced
    /// through `dongle.diagnostics`.
    pub fn collision_count(&self) -> rusqlite::Result<u64> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM aokie_outbox_meta WHERE key = 'payload_collisions'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v.and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    /// Write-before-emit: insert the event as `pending`.
    ///
    /// An existing row with the same idempotency key is authoritative:
    /// same content → [`InsertOutcome::Duplicate`] (crash/retry replay of
    /// one occurrence), different content →
    /// [`InsertOutcome::PayloadCollision`] (key-derivation bug — rejected,
    /// logged, counted; audit AOK-EVENT-001).
    pub fn insert_pending(
        &self,
        event: &DesktopEvent,
        target: &str,
    ) -> rusqlite::Result<InsertOutcome> {
        let now = now_iso8601();
        let hash = Self::payload_fingerprint(event);
        let plain = serde_json::to_string(event).expect("DesktopEvent serialises");
        // AOK-DUR-001: a protect failure must never fall back to plaintext.
        // The event is QUARANTINED instead: a typed dead row retaining the
        // non-sensitive metadata (name, correlation, key, timestamps) with a
        // marker payload — visible in diagnostics, never emitted or redriven
        // as it is. The event itself is held in memory and sealed again once
        // the key is given (`reseal_held`), when the row is filled in.
        let sealed = protect_payload(self.protection, &plain);
        if sealed.is_ok() {
            // The key is here: what waited for it goes in first.
            self.reseal_held();
        }
        let (payload, status, last_error) = match sealed {
            Ok(sealed) => (sealed, "pending", None::<String>),
            Err(e) => {
                eprintln!(
                    "[aokie-plugin] OUTBOX QUARANTINE: payload protection failed for {} ({e}) — event held in memory without a payload on disk, sealed again once the key is given",
                    event.idempotency_key
                );
                (
                    QUARANTINED_PAYLOAD.to_string(),
                    "dead",
                    Some(format!("payload_protect_failed: {e}")),
                )
            }
        };
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO aokie_outbox
                (event_name, correlation_id, idempotency_key, target,
                 payload_json, payload_hash, status, attempts, last_error, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?9)",
            params![
                event.name,
                event.correlation_id,
                event.idempotency_key,
                target,
                payload,
                hash,
                status,
                last_error,
                now
            ],
        )?;
        if inserted == 1 {
            return Ok(if status == "dead" {
                self.bump_meta_counter("protect_failures")?;
                self.hold_for_seal(event, target);
                InsertOutcome::QuarantinedProtectFailed
            } else {
                InsertOutcome::Inserted
            });
        }
        let existing: Option<(Option<String>, String)> = self
            .conn
            .query_row(
                "SELECT payload_hash, payload_json FROM aokie_outbox WHERE idempotency_key = ?1",
                params![event.idempotency_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let quarantined = existing
            .as_ref()
            .is_some_and(|(_, stored)| stored.starts_with(QUARANTINED_PAYLOAD));
        match existing.and_then(|(h, _)| h) {
            // Legacy row (pre-v3, no fingerprint): no comparison possible —
            // treat as the harmless duplicate it almost certainly is.
            None => Ok(InsertOutcome::Duplicate),
            // The same event again over its own quarantine row (a caller
            // retrying it): filled in now if this seal worked, else still
            // quarantined. Never a duplicate of a row with nothing behind it.
            Some(h) if h == hash && quarantined => {
                if status == "pending" && self.fill_quarantined(&event.idempotency_key, &payload)? == 1 {
                    self.forget_held(&event.idempotency_key);
                    Ok(InsertOutcome::Inserted)
                } else {
                    self.hold_for_seal(event, target);
                    Ok(InsertOutcome::QuarantinedProtectFailed)
                }
            }
            Some(h) if h == hash => Ok(InsertOutcome::Duplicate),
            Some(_) => {
                self.bump_meta_counter("payload_collisions")?;
                eprintln!(
                    "[aokie-plugin] OUTBOX KEY COLLISION: {} ({}) re-used with DIFFERENT content — \
                     new event rejected, existing row kept. This is a key-derivation bug.",
                    event.idempotency_key, event.name
                );
                Ok(InsertOutcome::PayloadCollision)
            }
        }
    }

    /// Whether a payload can be sealed now. On a Mac or Linux this asks the
    /// Keychain (or Secret Service) for the key, and may wait on its prompt.
    pub fn can_seal(&self) -> bool {
        protect_payload(self.protection, "").is_ok()
    }

    /// Whether a seal here may wait on a person: the Keychain's (or Secret
    /// Service's) prompt, up to 15 s. DPAPI answers at once.
    pub fn seal_may_wait(&self) -> bool {
        self.protection == PayloadProtection::Keychain
    }

    /// Keep `event` for a seal later (its quarantine row is written).
    fn hold_for_seal(&self, event: &DesktopEvent, target: &str) {
        let mut held = held_seals();
        if let Some(slot) = held
            .iter_mut()
            .find(|h| h.store == self.store && h.event.idempotency_key == event.idempotency_key)
        {
            slot.event = event.clone();
            return;
        }
        if held.len() >= MAX_HELD_SEALS {
            eprintln!(
                "[aokie-plugin] OUTBOX QUARANTINE: {} events already wait for a seal; {} stays quarantined",
                held.len(),
                event.idempotency_key
            );
            return;
        }
        held.push(HeldSeal {
            store: self.store.clone(),
            event: event.clone(),
            target: target.to_string(),
        });
    }

    fn forget_held(&self, idempotency_key: &str) {
        held_seals().retain(|h| !(h.store == self.store && h.event.idempotency_key == idempotency_key));
    }

    /// Put a sealed payload into an event's quarantine row, which then waits
    /// for delivery as any new row does. Rows changed: 0 when the row is gone
    /// (pruned) or no longer a quarantine marker.
    fn fill_quarantined(&self, idempotency_key: &str, sealed: &str) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE aokie_outbox
                SET payload_json = ?2, status = 'pending', attempts = 0, next_attempt_at = NULL,
                    last_error = NULL, updated_at = ?3
              WHERE idempotency_key = ?1 AND status = 'dead' AND payload_json LIKE 'quarantined:%'",
            params![idempotency_key, sealed, now_iso8601()],
        )
    }

    /// Seal what this file's connections held for a seal, oldest first, and
    /// fill in their rows; stop at the first seal that fails (the key is not
    /// here yet) and keep the rest.
    fn reseal_held(&self) {
        let mine: Vec<HeldSeal> = {
            let mut held = held_seals();
            if !held.iter().any(|h| h.store == self.store) {
                return;
            }
            let (mine, others) = std::mem::take(&mut *held)
                .into_iter()
                .partition(|h| h.store == self.store);
            *held = others;
            mine
        };
        let mut waiting = Vec::new();
        let mut sealing = true;
        for held in mine {
            if !sealing {
                waiting.push(held);
                continue;
            }
            let plain = serde_json::to_string(&held.event).expect("DesktopEvent serialises");
            let Ok(sealed) = protect_payload(self.protection, &plain) else {
                sealing = false;
                waiting.push(held);
                continue;
            };
            let key = &held.event.idempotency_key;
            let filled = self.fill_quarantined(key, &sealed).and_then(|n| {
                if n == 1 {
                    return Ok(1);
                }
                // (its row was pruned meanwhile: the event goes in as new, unless another row now holds its key)
                self.conn.execute(
                    "INSERT OR IGNORE INTO aokie_outbox
                        (event_name, correlation_id, idempotency_key, target,
                         payload_json, payload_hash, status, attempts, last_error, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, NULL, ?7, ?7)",
                    params![
                        held.event.name,
                        held.event.correlation_id,
                        key,
                        held.target,
                        sealed,
                        Self::payload_fingerprint(&held.event),
                        now_iso8601()
                    ],
                )
            });
            match filled {
                Ok(_) => eprintln!(
                    "[aokie-plugin] outbox: {key} was sealed once the key was given; it is delivered as any other event"
                ),
                Err(e) => {
                    eprintln!("[aokie-plugin] outbox: {key} was sealed, but its row was not written ({e}); held again");
                    waiting.push(held);
                }
            }
        }
        if !waiting.is_empty() {
            let mut held = held_seals();
            waiting.append(&mut held);
            *held = waiting;
        }
    }

    /// [`reseal_held`](Self::reseal_held) from a read, at most every
    /// [`RESEAL_EVERY`] and not while the key is known to be away.
    fn reseal_held_now_and_then(&self) {
        let now = std::time::Instant::now();
        if self.key_wait.get().is_some_and(|(until, _)| now < until)
            || self.resealed_at.get().is_some_and(|at| now < at + RESEAL_EVERY)
            || !held_seals().iter().any(|h| h.store == self.store)
        {
            return;
        }
        self.resealed_at.set(Some(now));
        self.reseal_held();
    }

    /// The host durably acknowledged this event. `sent` is terminal, and so is
    /// `dead` (audit AK-05): the ONLY transition into `sent` is from
    /// `pending`/`failed` — a stale or stray acknowledgement can no longer
    /// rewrite a dead/quarantined row into "successfully delivered" (hiding an
    /// undelivered event), and an ack for an unknown key changes zero rows.
    /// The typed outcome lets callers log exactly what happened.
    pub fn mark_sent(&self, idempotency_key: &str) -> rusqlite::Result<AckOutcome> {
        let changed = self.conn.execute(
            "UPDATE aokie_outbox SET status = 'sent', last_error = NULL, updated_at = ?2
             WHERE idempotency_key = ?1 AND status IN ('pending', 'failed')",
            params![idempotency_key, now_iso8601()],
        )?;
        if changed == 1 {
            return Ok(AckOutcome::Marked);
        }
        Ok(match self.status_of(idempotency_key)? {
            Some(OutboxStatus::Sent) => AckOutcome::AlreadySent,
            Some(_) => AckOutcome::RefusedTerminal,
            None => AckOutcome::Unknown,
        })
    }

    /// Record an emission failure: increments `attempts`, stores the
    /// error, and flips to `dead` once [`MAX_ATTEMPTS`] is reached and the
    /// row is older than [`RETRY_WINDOW_SECS`]. Returns the resulting status.
    ///
    /// Guarded (audit AOK-EVENT-001): only `pending`/`failed` rows move —
    /// a raced late failure can never drag an acknowledged (`sent`) row
    /// backward. `expected_attempts` is the attempt GENERATION the caller
    /// read before emitting: when supplied, the update applies only if the
    /// row's attempts still match, so two concurrent bookkeepers for the
    /// same read cannot double-count an attempt.
    pub fn mark_failed(
        &self,
        idempotency_key: &str,
        error: &str,
        expected_attempts: Option<u32>,
    ) -> rusqlite::Result<OutboxStatus> {
        let next = iso8601_after_secs(Self::next_backoff_secs(self.attempts_of(idempotency_key)?));
        self.conn.execute(
            "UPDATE aokie_outbox SET
                attempts = attempts + 1,
                last_error = ?2,
                status = CASE WHEN attempts + 1 >= ?3 AND created_at < ?7 THEN 'dead' ELSE 'failed' END,
                updated_at = ?4,
                next_attempt_at = ?5
             WHERE idempotency_key = ?1
               AND status IN ('pending', 'failed')
               AND (?6 IS NULL OR attempts = ?6)",
            params![
                idempotency_key,
                error,
                MAX_ATTEMPTS,
                now_iso8601(),
                next,
                expected_attempts,
                Self::retry_window_cutoff()
            ],
        )?;
        Ok(self
            .status_of(idempotency_key)?
            .unwrap_or(OutboxStatus::Failed))
    }

    /// Ack mode (audit INT-003): record that the event was WRITTEN to the
    /// host but not yet acknowledged. Increments `attempts` and schedules the
    /// next re-emission with exponential backoff; the row stays `pending`
    /// until the host's `event.ack` marks it `sent` — or goes `dead` after
    /// [`MAX_ATTEMPTS`] unacknowledged emissions over [`RETRY_WINDOW_SECS`]
    /// (re-delivery is bounded by time, not by how often the backoff came
    /// round). Returns the new status.
    ///
    /// Same guards as [`mark_failed`](Self::mark_failed): `sent`/`dead` rows
    /// are never touched (previously a late `mark_emitted` on a row the ack
    /// thread had just marked `sent` could push attempts over the limit and
    /// flip it to `dead` — audit AOK-EVENT-001), and `expected_attempts`
    /// makes the bookkeeping conditional on the generation the caller read.
    pub fn mark_emitted(
        &self,
        idempotency_key: &str,
        expected_attempts: Option<u32>,
    ) -> rusqlite::Result<OutboxStatus> {
        let next = iso8601_after_secs(Self::next_backoff_secs(self.attempts_of(idempotency_key)?));
        self.conn.execute(
            "UPDATE aokie_outbox SET
                attempts = attempts + 1,
                status = CASE WHEN attempts + 1 >= ?2 AND created_at < ?6 THEN 'dead' ELSE 'pending' END,
                updated_at = ?3,
                next_attempt_at = ?4
             WHERE idempotency_key = ?1
               AND status IN ('pending', 'failed')
               AND (?5 IS NULL OR attempts = ?5)",
            params![
                idempotency_key,
                MAX_ATTEMPTS,
                now_iso8601(),
                next,
                expected_attempts,
                Self::retry_window_cutoff()
            ],
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

    /// A row created before this instant has been retried for the whole
    /// [`RETRY_WINDOW_SECS`] and may go `dead`.
    fn retry_window_cutoff() -> String {
        iso8601_after_secs(-RETRY_WINDOW_SECS)
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

    /// Map raw fetched rows to emittable ones: a payload that fails to open
    /// (quarantined at write, corrupt, undecryptable) becomes a TYPED dead
    /// letter — `payload_unreadable: …` — and is excluded, so no caller can
    /// ever emit an empty or plaintext replacement (AOK-DUR-001).
    ///
    /// A sealing key the system will not give right now (a locked Keychain,
    /// a Secret Service not yet unlocked after login, a prompt answered
    /// "Deny") fails every row alike and may pass: then nothing is
    /// dead-lettered, the rows wait, and the key is asked for again only after
    /// a growing pause (so the user is not prompted every second). Only a row
    /// that does not open while sealing works is unreadable for good.
    fn open_payloads(&self, raw: Vec<(OutboxRow, String)>) -> Vec<OutboxRow> {
        let mut out = Vec::with_capacity(raw.len());
        if let Some((until, _)) = self.key_wait.get() {
            if std::time::Instant::now() < until {
                return out;
            }
        }
        for (mut row, stored) in raw {
            match unprotect_payload(&stored) {
                Ok(plain) => {
                    self.key_wait.set(None);
                    row.payload_json = plain;
                    out.push(row);
                }
                Err(e) if self.sealing_key_unavailable(&stored) => {
                    let tries = self.key_wait.get().map_or(0, |(_, tries)| tries) + 1;
                    let pause = std::time::Duration::from_secs((15u64 << tries.min(5)).min(300));
                    self.key_wait
                        .set(Some((std::time::Instant::now() + pause, tries)));
                    eprintln!(
                        "[aokie-plugin] outbox payloads cannot be opened now ({e}): the sealing key is unavailable — the rows wait, asked again in {}s",
                        pause.as_secs()
                    );
                    break;
                }
                Err(e) => {
                    eprintln!(
                        "[aokie-plugin] outbox row {} payload is unreadable ({e}) — dead-lettering",
                        row.idempotency_key
                    );
                    let _ = self
                        .mark_dead_typed(&row.idempotency_key, &format!("payload_unreadable: {e}"));
                }
            }
        }
        out
    }

    /// Whether `stored` failed to open because the key is not to be had (as
    /// opposed to this one row being damaged): a sealed row, and sealing
    /// itself fails too. A quarantine marker or a plaintext row is never
    /// that.
    fn sealing_key_unavailable(&self, stored: &str) -> bool {
        let sealed = stored.starts_with(aokie_core::dpapi::KEYCHAIN_PREFIX)
            || stored.starts_with(DPAPI_PREFIX);
        sealed && protect_payload(self.protection, "").is_err()
    }

    fn fetch_raw(
        &self,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> rusqlite::Result<Vec<(OutboxRow, String)>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params, |row| {
                let status: String = row.get(6)?;
                let stored: String = row.get(5)?;
                Ok((
                    OutboxRow {
                        id: row.get(0)?,
                        event_name: row.get(1)?,
                        correlation_id: row.get(2)?,
                        idempotency_key: row.get(3)?,
                        target: row.get(4)?,
                        payload_json: String::new(), // filled by open_payloads
                        status: OutboxStatus::from_str(&status).unwrap_or(OutboxStatus::Failed),
                        attempts: row.get(7)?,
                        last_error: row.get(8)?,
                        created_at: row.get(9)?,
                        updated_at: row.get(10)?,
                    },
                    stored,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Rows eligible for (re-)emission: `pending` or `failed`,
    /// oldest first. `dead` rows are excluded — they need operator
    /// attention, not another timer.
    pub fn retryable(&self, limit: u32) -> rusqlite::Result<Vec<OutboxRow>> {
        self.reseal_held_now_and_then();
        let raw = self.fetch_raw(
            "SELECT id, event_name, correlation_id, idempotency_key, target,
                    payload_json, status, attempts, last_error, created_at, updated_at
             FROM aokie_outbox
             WHERE status IN ('pending', 'failed')
             ORDER BY id ASC
             LIMIT ?1",
            &[&limit],
        )?;
        Ok(self.open_payloads(raw))
    }

    /// Rows DUE for (re-)emission by the ack-mode replay loop: `pending` or
    /// `failed`, whose `next_attempt_at` is unset (never emitted) or in the
    /// past. Oldest first. `sent`/`dead` rows never re-emit.
    pub fn due_for_retry(&self, limit: u32) -> rusqlite::Result<Vec<OutboxRow>> {
        self.reseal_held_now_and_then();
        let now = now_iso8601();
        let raw = self.fetch_raw(
            "SELECT id, event_name, correlation_id, idempotency_key, target,
                    payload_json, status, attempts, last_error, created_at, updated_at
             FROM aokie_outbox
             WHERE status IN ('pending', 'failed')
               AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
             ORDER BY id ASC
             LIMIT ?2",
            &[&now, &limit],
        )?;
        Ok(self.open_payloads(raw))
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

    /// TEST-ONLY: take the table away, so every write fails as a full disk
    /// or a lock held past the busy timeout would.
    #[cfg(test)]
    pub(crate) fn break_for_tests(&self) {
        self.conn.execute_batch("DROP TABLE aokie_outbox;").unwrap();
    }

    /// TEST-ONLY: make a row as old as `secs` seconds, for the retry window.
    #[cfg(test)]
    pub(crate) fn age_row_for_tests(&self, idempotency_key: &str, secs: i64) {
        let _ = self.conn.execute(
            "UPDATE aokie_outbox SET created_at = ?2 WHERE idempotency_key = ?1",
            params![idempotency_key, iso8601_after_secs(-secs)],
        );
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
    ///
    /// AOK-DUR-001: a row whose payload cannot be opened (quarantined at
    /// write, corrupt, undecryptable) is NEVER revived — there is nothing
    /// deliverable behind it, and reviving it would only churn it back to
    /// dead through the replay loop (or worse, emit an empty replacement).
    pub fn redrive_dead(&self, idempotency_key: Option<&str>) -> rusqlite::Result<usize> {
        let candidates: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT idempotency_key, payload_json FROM aokie_outbox
                 WHERE status = 'dead' AND (?1 IS NULL OR idempotency_key = ?1)",
            )?;
            let rows = stmt
                .query_map(params![idempotency_key], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut revived = 0usize;
        for (key, stored) in candidates {
            if let Err(e) = unprotect_payload(&stored) {
                eprintln!("[aokie-plugin] redrive skipped {key}: payload is undeliverable ({e})");
                continue;
            }
            revived += self.conn.execute(
                "UPDATE aokie_outbox
                    SET status = 'pending', attempts = 0, next_attempt_at = NULL, last_error = NULL
                  WHERE status = 'dead' AND idempotency_key = ?1",
                params![key],
            )?;
        }
        Ok(revived)
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
        assert_eq!(
            ob.insert_pending(&ev, TARGET_DESKTOP).unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Pending)
        );
        // Same occurrence again (crash/retry): ignored, still one row.
        assert_eq!(
            ob.insert_pending(&ev, TARGET_DESKTOP).unwrap(),
            InsertOutcome::Duplicate
        );
        let counts = ob.counts().unwrap();
        assert_eq!(counts.pending, 1);
    }

    /// Audit AOK-EVENT-001: a rebuilt sighting of the SAME occurrence (only
    /// its observation timestamps differ) dedupes; DIFFERENT content under
    /// one key is a key-derivation bug — rejected, counted, existing row kept.
    #[test]
    fn same_key_same_content_dedupes_but_different_content_is_a_collision() {
        let ob = Outbox::open_in_memory().unwrap();
        let mut first = event("sms_dev1_h42", "aokie.sms.received");
        first.data = json!({"from": "+61", "body": "hello", "at": "2026-07-11T00:00:00.000Z"});
        assert_eq!(
            ob.insert_pending(&first, TARGET_DESKTOP).unwrap(),
            InsertOutcome::Inserted
        );

        // The same MAP message re-fetched: fresh envelope timestamps, same content.
        let mut refetch = first.clone();
        refetch.occurred_at = now_iso8601();
        refetch.data = json!({"from": "+61", "body": "hello", "at": "2026-07-11T00:05:00.000Z"});
        assert_eq!(
            ob.insert_pending(&refetch, TARGET_DESKTOP).unwrap(),
            InsertOutcome::Duplicate
        );
        assert_eq!(ob.collision_count().unwrap(), 0);

        // A different message colliding on the key: rejected + counted.
        let mut clash = first.clone();
        clash.data =
            json!({"from": "+61", "body": "TRANSFER $9000 NOW", "at": "2026-07-11T00:06:00.000Z"});
        assert_eq!(
            ob.insert_pending(&clash, TARGET_DESKTOP).unwrap(),
            InsertOutcome::PayloadCollision
        );
        assert_eq!(ob.collision_count().unwrap(), 1);

        // The stored row still carries the FIRST occurrence's content.
        let rows = ob.due_for_retry(10).unwrap();
        let back: DesktopEvent = serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(back.data["body"], "hello");
        assert_eq!(ob.counts().unwrap().pending, 1, "no second row");
    }

    /// Audit AOK-EVENT-001 acceptance: `sent` is terminal — raced replay
    /// bookkeeping from a SECOND connection to the same file can never drag
    /// an acknowledged row back to failed/dead or over the attempt limit.
    #[test]
    fn two_connection_ack_replay_race_cannot_move_sent_backward() {
        let path = std::env::temp_dir().join(format!(
            "aokie-outbox-race-{}-{}.sqlite",
            std::process::id(),
            now_iso8601().replace(':', "-")
        ));
        let ack_conn = Outbox::open(&path).unwrap();
        let replay_conn = Outbox::open(&path).unwrap();

        let ev = event("call_race", "aokie.call.ended");
        ack_conn.insert_pending(&ev, TARGET_DESKTOP).unwrap();

        // The replay connection reads the row (generation 0), emits it, and —
        // before its bookkeeping lands — the ack connection marks it sent.
        let due = replay_conn.due_for_retry(1).unwrap();
        let row = &due[0];
        assert_eq!(row.attempts, 0);
        ack_conn.mark_sent(&ev.idempotency_key).unwrap();

        // Late bookkeeping from the replay side: all no-ops against `sent`.
        replay_conn
            .mark_emitted(&ev.idempotency_key, Some(row.attempts))
            .unwrap();
        replay_conn
            .mark_failed(&ev.idempotency_key, "late failure", Some(row.attempts))
            .unwrap();
        for _ in 0..MAX_ATTEMPTS {
            replay_conn.mark_emitted(&ev.idempotency_key, None).unwrap();
            replay_conn
                .mark_failed(&ev.idempotency_key, "storm", None)
                .unwrap();
        }
        assert_eq!(
            ack_conn.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent),
            "sent is terminal under ack/replay races"
        );

        // And the generation guard: two bookkeepers for the SAME read apply once.
        let ev2 = event("call_race2", "aokie.call.ended");
        ack_conn.insert_pending(&ev2, TARGET_DESKTOP).unwrap();
        let gen0 = ack_conn.due_for_retry(10).unwrap().last().unwrap().attempts;
        ack_conn
            .mark_emitted(&ev2.idempotency_key, Some(gen0))
            .unwrap();
        replay_conn
            .mark_emitted(&ev2.idempotency_key, Some(gen0))
            .unwrap(); // stale generation
        let rows = replay_conn.retryable(10).unwrap();
        let row2 = rows
            .iter()
            .find(|r| r.idempotency_key == ev2.idempotency_key)
            .unwrap();
        assert_eq!(row2.attempts, 1, "stale-generation bookkeeping is a no-op");

        drop(ack_conn);
        drop(replay_conn);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }

    #[test]
    fn mark_sent_transitions() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_b", "aokie.call.ended");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        assert_eq!(
            ob.mark_sent(&ev.idempotency_key).unwrap(),
            AckOutcome::Marked
        );
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
        assert_eq!(ob.counts().unwrap().sent, 1);
    }

    /// Audit AK-05: `sent` and `dead` are both TERMINAL for acknowledgements —
    /// a stray/duplicate ack can never rewrite a dead (undelivered) row into
    /// "delivered", and an ack for a key we never emitted changes zero rows.
    #[test]
    fn acks_cannot_rewrite_terminal_rows_or_invent_them() {
        let ob = Outbox::open_in_memory().unwrap();

        // Duplicate ack on a sent row: idempotent, still sent.
        let sent = event("ack-sent", "aokie.call.ended");
        ob.insert_pending(&sent, TARGET_DESKTOP).unwrap();
        assert_eq!(ob.mark_sent(&sent.idempotency_key).unwrap(), AckOutcome::Marked);
        assert_eq!(
            ob.mark_sent(&sent.idempotency_key).unwrap(),
            AckOutcome::AlreadySent
        );

        // Dead row: an ack must NOT paper over the failure.
        let dead = event("ack-dead", "aokie.call.ended");
        ob.insert_pending(&dead, TARGET_DESKTOP).unwrap();
        ob.age_row_for_tests(&dead.idempotency_key, RETRY_WINDOW_SECS + 60);
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&dead.idempotency_key, "boom", None).unwrap();
        }
        assert_eq!(
            ob.mark_sent(&dead.idempotency_key).unwrap(),
            AckOutcome::RefusedTerminal
        );
        assert_eq!(
            ob.status_of(&dead.idempotency_key).unwrap(),
            Some(OutboxStatus::Dead),
            "the dead row must stay dead"
        );

        // Unknown key: nothing moved, typed outcome says so.
        assert_eq!(
            ob.mark_sent("never-emitted-key").unwrap(),
            AckOutcome::Unknown
        );

        // failed → sent still works (the normal retry-then-ack path).
        let retried = event("ack-retried", "aokie.call.ended");
        ob.insert_pending(&retried, TARGET_DESKTOP).unwrap();
        ob.mark_failed(&retried.idempotency_key, "transient", None)
            .unwrap();
        assert_eq!(
            ob.mark_sent(&retried.idempotency_key).unwrap(),
            AckOutcome::Marked
        );
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
        ob.age_row_for_tests(&dead.idempotency_key, RETRY_WINDOW_SECS + 60);
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&dead.idempotency_key, "boom", None).unwrap();
        }
        ob.mark_emitted(&sent.idempotency_key, None).unwrap();
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
        ob.age_row_for_tests(&ev.idempotency_key, RETRY_WINDOW_SECS + 60);
        for attempt in 1..MAX_ATTEMPTS {
            let status = ob
                .mark_failed(&ev.idempotency_key, "desktop down", None)
                .unwrap();
            assert_eq!(status, OutboxStatus::Failed, "attempt {attempt}");
        }
        let status = ob
            .mark_failed(&ev.idempotency_key, "desktop down", None)
            .unwrap();
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
        ob.mark_failed(&alive.idempotency_key, "once", None)
            .unwrap();
        ob.age_row_for_tests(&dying.idempotency_key, RETRY_WINDOW_SECS + 60);
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&dying.idempotency_key, "always", None)
                .unwrap();
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
            ob.mark_emitted(&ev.idempotency_key, None).unwrap(),
            OutboxStatus::Pending
        );
        assert!(
            ob.due_for_retry(10).unwrap().is_empty(),
            "backoff gates re-emission"
        );

        // The host's ack arrives → sent, and never due again.
        ob.mark_sent(&ev.idempotency_key).unwrap();
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Sent)
        );
        assert!(ob.due_for_retry(10).unwrap().is_empty());
    }

    /// A row that is never acknowledged dead-letters after MAX_ATTEMPTS
    /// emissions over the retry window instead of re-delivering forever.
    #[test]
    fn unacknowledged_rows_dead_letter_after_max_attempts() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_h", "aokie.call.ended");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        ob.age_row_for_tests(&ev.idempotency_key, RETRY_WINDOW_SECS + 60);
        for _ in 0..(MAX_ATTEMPTS - 1) {
            assert_eq!(
                ob.mark_emitted(&ev.idempotency_key, None).unwrap(),
                OutboxStatus::Pending
            );
        }
        assert_eq!(
            ob.mark_emitted(&ev.idempotency_key, None).unwrap(),
            OutboxStatus::Dead
        );
        assert!(
            ob.due_for_retry(10).unwrap().is_empty(),
            "dead rows never re-emit"
        );
    }

    /// Review 2026-10: a host that did not ack for two minutes (eight tries at
    /// 1-64 s) used to dead-letter every event. A young row is re-delivered
    /// past MAX_ATTEMPTS; only one past the retry window goes dead.
    #[test]
    fn a_slow_host_does_not_dead_letter_young_rows() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_slow", "aokie.call.ended");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        for _ in 0..(MAX_ATTEMPTS * 3) {
            assert_eq!(
                ob.mark_emitted(&ev.idempotency_key, None).unwrap(),
                OutboxStatus::Pending
            );
        }
        ob.rewind_next_attempt_for_tests(&ev.idempotency_key);
        assert_eq!(ob.due_for_retry(10).unwrap().len(), 1, "still re-delivered");
        ob.age_row_for_tests(&ev.idempotency_key, RETRY_WINDOW_SECS + 60);
        assert_eq!(
            ob.mark_emitted(&ev.idempotency_key, None).unwrap(),
            OutboxStatus::Dead
        );
    }

    /// A key that cannot be had right now leaves the rows waiting (not dead),
    /// and is not asked for again at once. A damaged row is still dead.
    #[test]
    fn an_unavailable_sealing_key_leaves_rows_pending() {
        let ob = Outbox::open_in_memory_with_protection(PayloadProtection::Unavailable).unwrap();
        // A row sealed elsewhere (as the Keychain seals), read while sealing fails.
        ob.conn
            .execute(
                "INSERT INTO aokie_outbox (event_name, correlation_id, idempotency_key, target,
                     payload_json, status, attempts, created_at, updated_at)
                 VALUES ('aokie.call.ended', 'c', 'k1', 'desktop', 'keychain1:AAAA', 'pending', 0, ?1, ?1)",
                params![now_iso8601()],
            )
            .unwrap();
        assert!(ob.due_for_retry(10).unwrap().is_empty());
        assert_eq!(ob.status_of("k1").unwrap(), Some(OutboxStatus::Pending));
        assert!(ob.key_wait.get().is_some(), "the next ask waits");
        // With sealing working, the same unreadable row is damaged for good.
        let ok = Outbox::open_in_memory_with_protection(PayloadProtection::DevPlaintext).unwrap();
        ok.conn
            .execute(
                "INSERT INTO aokie_outbox (event_name, correlation_id, idempotency_key, target,
                     payload_json, status, attempts, created_at, updated_at)
                 VALUES ('aokie.call.ended', 'c', 'k2', 'desktop', 'keychain1:AAAA', 'pending', 0, ?1, ?1)",
                params![now_iso8601()],
            )
            .unwrap();
        assert!(ok.due_for_retry(10).unwrap().is_empty());
        assert_eq!(ok.status_of("k2").unwrap(), Some(OutboxStatus::Dead));
    }

    /// A seal that failed at write (the Keychain's prompt not answered yet) is
    /// not a quarantine for good: the event waits in memory, a retry of it is
    /// not taken for a delivered duplicate, and once the key is given its row
    /// is filled in and delivered. Nothing of it is on disk before then.
    #[test]
    fn a_seal_that_failed_at_write_is_made_again_once_the_key_is_given() {
        let mut ob = Outbox::open_in_memory_with_protection(PayloadProtection::Unavailable).unwrap();
        let mut ev = event("call_late_key", "aokie.call.turn.final");
        ev.data = json!({"text": "SECRET-HELD-BODY"});
        assert_eq!(
            ob.insert_pending(&ev, TARGET_DESKTOP).unwrap(),
            InsertOutcome::QuarantinedProtectFailed
        );
        // The caller's retry while the key is still away: still quarantined, never a duplicate.
        assert_eq!(
            ob.insert_pending(&ev, TARGET_DESKTOP).unwrap(),
            InsertOutcome::QuarantinedProtectFailed
        );
        assert!(ob.due_for_retry(10).unwrap().is_empty());
        let raw: String = ob
            .conn
            .query_row("SELECT payload_json FROM aokie_outbox", [], |r| r.get(0))
            .unwrap();
        assert!(!raw.contains("SECRET-HELD-BODY"));

        // The key is given: the next read seals it from memory.
        ob.set_protection_for_tests(PayloadProtection::DevPlaintext);
        ob.resealed_at.set(None);
        let rows = ob.due_for_retry(10).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].payload_json.contains("SECRET-HELD-BODY"));
        assert_eq!(ob.status_of(&ev.idempotency_key).unwrap(), Some(OutboxStatus::Pending));
        assert!(!held_seals().iter().any(|h| h.store == ob.store), "no longer held");
        // The caller's own retry now is the duplicate it is.
        assert_eq!(ob.insert_pending(&ev, TARGET_DESKTOP).unwrap(), InsertOutcome::Duplicate);

        // A write whose own seal works seals what waited first, on the spot.
        ob.set_protection_for_tests(PayloadProtection::Unavailable);
        let waiting = event("call_waiting", "aokie.call.turn.final");
        assert_eq!(
            ob.insert_pending(&waiting, TARGET_DESKTOP).unwrap(),
            InsertOutcome::QuarantinedProtectFailed
        );
        ob.set_protection_for_tests(PayloadProtection::DevPlaintext);
        assert_eq!(
            ob.insert_pending(&event("call_next", "aokie.call.turn.final"), TARGET_DESKTOP).unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(ob.status_of(&waiting.idempotency_key).unwrap(), Some(OutboxStatus::Pending));
    }

    /// The event one connection could not seal (the radio's) is sealed by
    /// another to the same file (the replay thread's), which reads every second.
    #[test]
    fn another_connection_to_the_file_seals_what_one_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.sqlite");
        let mut radio = Outbox::open(&path).unwrap();
        radio.set_protection_for_tests(PayloadProtection::Unavailable);
        let replay = Outbox::open(&path).unwrap();
        let ev = event("call_two_connections", "aokie.call.ended");
        assert_eq!(
            radio.insert_pending(&ev, TARGET_DESKTOP).unwrap(),
            InsertOutcome::QuarantinedProtectFailed
        );
        drop(radio);
        let rows = replay.due_for_retry(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].idempotency_key, ev.idempotency_key);
    }

    /// A file that is not a database is set aside and a fresh outbox opens.
    #[test]
    fn a_corrupt_outbox_file_is_set_aside_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.sqlite");
        std::fs::write(&path, b"this is not a sqlite database, not at all, not even a header....").unwrap();
        let (ob, moved) = Outbox::open_or_quarantine(&path).unwrap();
        let moved = moved.expect("the damaged file was set aside");
        assert!(moved.exists());
        assert!(moved.to_string_lossy().contains("outbox.sqlite.corrupt-"));
        ob.insert_pending(&event("c", "aokie.call.ended"), TARGET_DESKTOP).unwrap();
        // A healthy file opens as it is.
        drop(ob);
        let (_, again) = Outbox::open_or_quarantine(&path).unwrap();
        assert!(again.is_none());
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
            .query_row("SELECT payload_json FROM aokie_outbox LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        if cfg!(windows) {
            assert!(raw.starts_with(DPAPI_PREFIX), "stored protected: {raw:.20}");
            assert!(
                !raw.contains("4111"),
                "PII must not be readable in the file"
            );
        }
        let rows = ob.due_for_retry(10).unwrap();
        let back: DesktopEvent = serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(
            back.data["text"], "my card number is 4111",
            "reads round-trip"
        );
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
                params![
                    ev.name,
                    ev.correlation_id,
                    ev.idempotency_key,
                    plain,
                    now_iso8601()
                ],
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
        ob.age_row_for_tests(&ev.idempotency_key, RETRY_WINDOW_SECS + 60);
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&ev.idempotency_key, "boom", None).unwrap();
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

    // ── AOK-DUR-001 ─────────────────────────────────────────────────────────

    /// Item 1 acceptance: an induced protect failure cannot write plaintext
    /// (or emit an empty replacement) as though successful — the event is
    /// QUARANTINED: typed dead row, metadata kept, nothing emittable.
    #[test]
    fn protect_failure_quarantines_instead_of_storing_plaintext() {
        let ob = Outbox::open_in_memory_with_protection(PayloadProtection::Unavailable).unwrap();
        let mut ev = event("call_q", "aokie.call.turn.final");
        ev.data = serde_json::json!({"text": "SECRET-QUARANTINE-BODY"});

        assert_eq!(
            ob.insert_pending(&ev, TARGET_DESKTOP).unwrap(),
            InsertOutcome::QuarantinedProtectFailed
        );
        // No plaintext hit the file — the payload column holds only the marker.
        let raw: String = ob
            .conn
            .query_row("SELECT payload_json FROM aokie_outbox LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!raw.contains("SECRET-QUARANTINE-BODY"));
        assert!(raw.starts_with(QUARANTINED_PAYLOAD));
        // Typed dead letter with repair metadata (name/key/timestamps survive).
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Dead)
        );
        let err: String = ob
            .conn
            .query_row("SELECT last_error FROM aokie_outbox LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(err.starts_with("payload_protect_failed:"), "typed: {err}");
        // Never emitted, never redriven.
        assert!(ob.due_for_retry(10).unwrap().is_empty());
        assert!(ob.retryable(10).unwrap().is_empty());
        assert_eq!(
            ob.redrive_dead(None).unwrap(),
            0,
            "nothing deliverable to revive"
        );
        assert_eq!(ob.counts().unwrap().dead, 1);
    }

    /// Item 2 acceptance: a corrupt/undecryptable payload becomes a TYPED
    /// dead letter at read time — it is excluded from emission (no empty
    /// payload ever leaves) and redrive refuses it, while its non-sensitive
    /// metadata stays for repair.
    #[test]
    fn unreadable_payload_dead_letters_with_type_and_metadata() {
        let ob = Outbox::open_in_memory().unwrap();
        let ev = event("call_u", "aokie.sms.received");
        ob.insert_pending(&ev, TARGET_DESKTOP).unwrap();
        // Corrupt the stored payload the way a damaged file / foreign user
        // context presents: a protected prefix that cannot decrypt.
        ob.conn
            .execute(
                "UPDATE aokie_outbox SET payload_json = 'dpapi1:!!!not-base64!!!'",
                [],
            )
            .unwrap();

        assert!(ob.due_for_retry(10).unwrap().is_empty(), "never emitted");
        assert_eq!(
            ob.status_of(&ev.idempotency_key).unwrap(),
            Some(OutboxStatus::Dead)
        );
        let (name, err): (String, String) = ob
            .conn
            .query_row(
                "SELECT event_name, last_error FROM aokie_outbox LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "aokie.sms.received", "repair metadata retained");
        assert!(err.starts_with("payload_unreadable:"), "typed: {err}");
        assert_eq!(
            ob.redrive_dead(None).unwrap(),
            0,
            "undeliverable rows are not revived"
        );
        // A HEALTHY dead row alongside it still redrives fine.
        let ok = event("call_u2", "aokie.call.ended");
        ob.insert_pending(&ok, TARGET_DESKTOP).unwrap();
        ob.age_row_for_tests(&ok.idempotency_key, RETRY_WINDOW_SECS + 60);
        for _ in 0..MAX_ATTEMPTS {
            ob.mark_failed(&ok.idempotency_key, "down", None).unwrap();
        }
        assert_eq!(
            ob.redrive_dead(None).unwrap(),
            1,
            "deliverable dead rows revive"
        );
    }

    /// Item 4 acceptance (Windows): legacy plaintext rows are sealed in place
    /// at open — verified before replacement, resumable per row — and after
    /// the migration + vacuum the database file contains no plaintext copy.
    #[cfg(windows)]
    #[test]
    fn legacy_plaintext_rows_are_sealed_at_open_leaving_no_plaintext() {
        let path = std::env::temp_dir().join(format!(
            "aokie-outbox-seal-{}-{}.sqlite",
            std::process::id(),
            now_iso8601().replace(':', "-")
        ));
        let mut ev = event("corr-mig", "aokie.call.turn.final");
        ev.data = serde_json::json!({"text": "LEGACY-MIGRATION-SECRET"});
        let plain = serde_json::to_string(&ev).unwrap();
        {
            // A pre-protection install: plaintext payload row.
            let ob = Outbox::open(&path).unwrap();
            ob.conn
                .execute(
                    "INSERT INTO aokie_outbox (event_name, correlation_id, idempotency_key, target,
                        payload_json, status, attempts, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'desktop', ?4, 'pending', 0, ?5, ?5)",
                    params![
                        ev.name,
                        ev.correlation_id,
                        ev.idempotency_key,
                        plain,
                        now_iso8601()
                    ],
                )
                .unwrap();
        }
        let no_plaintext_anywhere = |tag: &str| {
            let needle = b"LEGACY-MIGRATION-SECRET";
            for f in [path.clone(), path.with_extension("sqlite-wal")] {
                if let Ok(bytes) = std::fs::read(&f) {
                    assert!(
                        !bytes.windows(needle.len()).any(|w| w == needle),
                        "{tag}: plaintext survives in {}",
                        f.display()
                    );
                }
            }
        };
        {
            // Reopen: the migration seals it (and vacuums old pages away).
            let ob = Outbox::open(&path).unwrap();
            let stored: String = ob
                .conn
                .query_row("SELECT payload_json FROM aokie_outbox LIMIT 1", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(stored.starts_with(DPAPI_PREFIX), "sealed: {stored:.20}");
            // Round-trips for delivery.
            let rows = ob.due_for_retry(10).unwrap();
            let back: DesktopEvent = serde_json::from_str(&rows[0].payload_json).unwrap();
            assert_eq!(back.data["text"], "LEGACY-MIGRATION-SECRET");
            // Crucially: no plaintext in ANY db file while the connection is
            // still OPEN — the live plugin never closes its handle, so the
            // scrub cannot rely on a close-time checkpoint.
            no_plaintext_anywhere("connection open");
        }
        no_plaintext_anywhere("after close");
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
