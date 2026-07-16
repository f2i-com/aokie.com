// Database utilities - some features prepared for future use
#![allow(dead_code)]

use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::thread;

const CURRENT_VERSION: u32 = 9;

/// Host services the database layer needs from whatever embeds it.
///
/// The legacy Tauri app threaded a `tauri::AppHandle` into every db
/// entry point to (a) resolve the app-data directory and (b) surface
/// fatal db errors to the UI. This crate is deliberately Tauri-free —
/// the caller supplies a `DbHost` instead, so `aokie-db` links no
/// webview stack and stays trivially testable.
pub trait DbHost {
    /// Absolute app-data directory. Everything the db layer touches —
    /// `aokie.db`, the `recordings/` folder, the retention config —
    /// lives under here. In the legacy app this was
    /// `app_handle.path().app_data_dir()`; the stock implementation is
    /// `aokie_core::paths::app_data_dir()` (the identical
    /// `<RoamingAppData>/com.aokie.app` path).
    fn data_dir(&self) -> std::path::PathBuf;

    /// Invoked when opening or migrating the database fails, so the
    /// host can surface it however it likes (the legacy app emitted a
    /// db-error event to the renderer). Called from
    /// [`open_migrated_connection`], the one connection-open ladder.
    fn on_db_error(&self, message: &str);
}

pub fn db_path(host: &dyn DbHost) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let app_data_dir = host.data_dir();
    std::fs::create_dir_all(&app_data_dir)?;
    Ok(app_data_dir.join("aokie.db"))
}

/// Open a fresh read-only connection and snapshot the contacts table
/// into an in-memory `ContactStore`. Called at app startup so the +CLIP
/// handler can resolve names without per-call DB latency. Subsequent
/// PBAP fetches refresh the store via `replace_all`.
pub fn load_contact_store(
    host: &dyn DbHost,
) -> Result<ContactStore, Box<dyn std::error::Error + Send + Sync>> {
    // Go through the central helper even though `init_database` has
    // already migrated the schema by the time we reach here — keeps
    // every connection-open path on one well-tested ladder.
    let conn = open_migrated_connection(host)?;
    ContactStore::load_from_db(&conn)
}

pub async fn init_database(
    host: &dyn DbHost,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = db_path(host)?;
    // open_migrated_connection runs the migration ladder + sets the
    // per-connection PRAGMAs we want everywhere — so the startup pass
    // and all subsequent ad-hoc opens share one well-tested path.
    let _ = open_migrated_connection(host)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    // First retention pass — runs once at startup. The daily timer in
    // lib.rs keeps this fresh while the app is running so a
    // long-uptime install (R11-#6) doesn't drift past the operator's
    // configured privacy window.
    if let Err(e) = run_retention_purge(host) {
        eprintln!("[Database] startup retention purge skipped: {}", e);
    }
    println!("Database initialized at: {:?}", path);
    Ok(())
}

/// Apply the operator's currently-configured retention windows once.
/// Pulled out of `init_database` (R11-#6) so the same pass can run on
/// a daily timer + an on-demand "Apply now" button without rebuilding
/// the connection plumbing each time. Errors are non-fatal and bubbled
/// up so the caller can decide whether to log or surface them.
pub fn run_retention_purge(
    host: &dyn DbHost,
) -> Result<RetentionPurgeReport, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = open_migrated_connection(host)?;
    let retention = aokie_core::retention::load(&host.data_dir());
    let mut report = RetentionPurgeReport::default();
    match purge_old_call_data(host, &mut conn, retention.call_history_days) {
        Ok(()) => report.calls_pass = true,
        Err(e) => {
            eprintln!("[Database] retention purge (calls) skipped: {}", e);
            report.calls_error = Some(e.to_string());
        }
    }
    match purge_old_sms_data(&mut conn, retention.sms_history_days) {
        Ok(_) => report.sms_pass = true,
        Err(e) => {
            eprintln!("[Database] retention purge (sms) skipped: {}", e);
            report.sms_error = Some(e.to_string());
        }
    }
    // R12-#9: also sweep for orphan transcripts (rows whose call_id no
    // longer matches any call_logs entry). The retention purge above
    // deletes transcripts by parent call_id so it shouldn't ever
    // leave orphans, but a crash mid-transaction or an external
    // tool that touches the DB could. The check is cheap (one
    // indexed delete) so we run it on every retention pass instead
    // of waiting for a separate job.
    match repair_orphan_transcripts(&conn) {
        Ok(n) => report.orphan_transcripts_removed = n,
        Err(e) => {
            eprintln!("[Database] orphan-transcript sweep skipped: {}", e);
            report.orphan_transcripts_error = Some(e.to_string());
        }
    }
    Ok(report)
}

/// Delete transcripts whose `call_id` no longer exists in `call_logs`.
/// Returns the number of rows removed. Cheap because both columns are
/// indexed; the LEFT JOIN scans the smaller side of the join.
///
/// The schema doesn't have an FK constraint between transcripts and
/// call_logs (SQLite would need a table rebuild to retrofit one), so
/// this is the lightweight alternative — a periodic integrity sweep
/// that catches anything the application-level cleanup contract
/// missed (R12-#9).
///
/// R14-#10: skip transcripts whose `timestamp` is newer than
/// `ORPHAN_GRACE_SECS` ago. The transcript writer inserts the row at
/// the moment the partial STT result lands; the call_logs row is
/// upserted separately by the call lifecycle. If those two writes
/// cross paths during a hot inbound call, the sweep can race the
/// parent insert and delete a still-being-written transcript row.
/// The grace window absorbs that race without slowing down the eventual
/// cleanup of genuinely abandoned rows (which only need to survive one
/// retention pass to be reaped).
pub fn repair_orphan_transcripts(
    conn: &Connection,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    let cutoff = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0))
    .saturating_sub(ORPHAN_GRACE_SECS);
    let removed = conn.execute(
        "DELETE FROM transcripts \
         WHERE call_id NOT IN (SELECT call_id FROM call_logs) \
         AND timestamp < ?1",
        [cutoff],
    )?;
    if removed > 0 {
        println!(
            "[Database] orphan-transcript sweep removed {} row(s)",
            removed
        );
    }
    Ok(removed)
}

/// How long a transcript row gets to wait for its parent `call_logs`
/// entry before the orphan sweep is allowed to delete it. Five minutes
/// is far longer than any plausible call-lifecycle insert race, but
/// short enough that a genuinely orphaned row is reaped on the very
/// next retention pass after the window closes.
const ORPHAN_GRACE_SECS: i64 = 300;

/// Per-pass outcome surfaced to callers (the daily timer logs it; the
/// `run_retention_purge_now` Tauri command returns it to the UI so the
/// "Apply now" button can show "Done" or the failing branch).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RetentionPurgeReport {
    pub calls_pass: bool,
    pub calls_error: Option<String>,
    pub sms_pass: bool,
    pub sms_error: Option<String>,
    /// R12-#9: orphan-transcript sweep — transcripts whose parent
    /// call_logs row was already gone. Should usually be 0; a non-zero
    /// value points at a previously crashed retention-purge transaction
    /// or external DB tampering.
    #[serde(default)]
    pub orphan_transcripts_removed: usize,
    #[serde(default)]
    pub orphan_transcripts_error: Option<String>,
}

/// Default retention used when the on-disk `retention.json` is missing
/// or malformed. Surfaced for tests / docs; runtime callers should
/// read `aokie_core::retention::load()` and use whatever the operator
/// configured.
pub const DEFAULT_CALL_RETENTION_DAYS: i64 = 90;

/// Delete call_logs and transcripts older than `retention_days`, plus
/// any recording wav files that belonged to the purged calls. Runs
/// once per app launch (from `init_database`); a failure here is
/// non-fatal — we log and continue rather than block startup, since
/// a purge issue mustn't keep the operator from taking calls.
///
/// `retention_days <= 0` is treated as "keep forever" and short-
/// circuits the entire pass — that matches `aokie_core::retention::
/// KEEP_FOREVER` and prevents a configuration mishap from wiping
/// the call_logs table.
///
/// Cleanup contract:
///
/// 1. Snapshot the `call_id`s we're about to retire (single SELECT).
/// 2. Open an IMMEDIATE transaction and DELETE transcripts by
///    `call_id IN (snapshot)` first, then the matching call_logs.
///    Deleting transcripts by their *own* `timestamp` (the previous
///    behaviour) drifts on clock-skew / delayed writes / late
///    imports: a transcript whose row was inserted with a slightly
///    later timestamp than its parent call_log would survive after
///    its call_log was purged, leaving an orphan that no call list
///    references. Deleting by parent `call_id` keeps the two tables
///    consistent regardless of timestamp ordering.
/// 3. Commit. Only AFTER the DB transaction has landed do we touch
///    the recordings directory. Doing it inside the transaction
///    would mean a fs-error on `remove_file` rolls back the DB
///    state, but a successful DB commit followed by a panic / power
///    loss would still leave behind the wav file — accept that
///    asymmetry rather than risk the DB delete being lost. We log
///    file-cleanup failures so support can see them, and the next
///    purge will retry.
pub fn purge_old_call_data(
    host: &dyn DbHost,
    conn: &mut Connection,
    retention_days: i64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if retention_days <= 0 {
        return Ok(());
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    // checked_mul defends against a hand-edited retention.json that
    // somehow bypassed `aokie_core::retention::clamp_days` — we'd rather
    // refuse the purge than overflow into the past and wipe the
    // table.
    let window_ms = retention_days
        .checked_mul(24 * 60 * 60 * 1000)
        .ok_or_else(|| {
            format!(
                "retention overflow: {}d * 86_400_000 ms exceeds i64",
                retention_days
            )
        })?;
    let cutoff_ms = now_ms - window_ms;

    let PurgeStats {
        purged_ids,
        logs_removed: purged_logs,
        transcripts_removed: purged_transcripts,
    } = purge_call_records_before(conn, cutoff_ms)?;
    if purged_ids.is_empty() {
        return Ok(());
    }

    let recordings_dir = Some(host.data_dir().join("recordings"));
    let mut recordings_removed = 0usize;
    if let Some(dir) = recordings_dir {
        for call_id in &purged_ids {
            let path = dir.join(format!("{}.wav", call_id));
            if path.exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    eprintln!("[Database] retention: couldn't remove {:?}: {}", path, e);
                } else {
                    recordings_removed += 1;
                }
            }
        }
    }

    println!(
        "[Database] retention purge (>{}d): {} call_logs, {} transcripts, {} recordings",
        retention_days, purged_logs, purged_transcripts, recordings_removed
    );
    Ok(())
}

/// SMS counterpart to `purge_old_call_data`. SMS history defaults to
/// "forever" — most operators expect conversation context to persist —
/// but the user can opt into a finite window via the retention UI.
/// Returns the row count purged so the caller can log / surface it;
/// `0` on a no-op pass.
pub fn purge_old_sms_data(
    conn: &mut Connection,
    retention_days: i64,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    if retention_days <= 0 {
        return Ok(0);
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let window_ms = retention_days
        .checked_mul(24 * 60 * 60 * 1000)
        .ok_or_else(|| {
            format!(
                "retention overflow: {}d * 86_400_000 ms exceeds i64",
                retention_days
            )
        })?;
    let cutoff_ms = now_ms - window_ms;

    let removed = conn.execute(
        "DELETE FROM sms_messages WHERE timestamp < ?1",
        params![cutoff_ms],
    )?;
    if removed > 0 {
        println!(
            "[Database] retention purge SMS (>{}d): {} rows",
            retention_days, removed
        );
    }
    Ok(removed)
}

/// Tally returned by the on-demand "delete now" actions so the UI can
/// surface what just happened.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct DeleteCounts {
    pub call_logs: usize,
    pub transcripts: usize,
    pub recordings: usize,
    pub sms: usize,
}

/// "Delete all call history now" — wipes every row from `call_logs` +
/// `transcripts` and removes every WAV file in the recordings dir.
/// Used by the retention UI's destructive button. SMS is untouched
/// (separate button).
pub fn delete_all_call_history(
    host: &dyn DbHost,
    conn: &mut Connection,
) -> Result<DeleteCounts, Box<dyn std::error::Error + Send + Sync>> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let transcripts_removed = tx.execute("DELETE FROM transcripts", [])?;
    let logs_removed = tx.execute("DELETE FROM call_logs", [])?;
    tx.commit()?;

    let recordings_removed = wipe_recordings_dir(host);

    println!(
        "[Database] delete-now call history: {} call_logs, {} transcripts, {} recordings",
        logs_removed, transcripts_removed, recordings_removed
    );
    Ok(DeleteCounts {
        call_logs: logs_removed,
        transcripts: transcripts_removed,
        recordings: recordings_removed,
        sms: 0,
    })
}

/// "Delete all SMS history now" — wipes every row from `sms_messages`.
pub fn delete_all_sms_history(
    conn: &mut Connection,
) -> Result<DeleteCounts, Box<dyn std::error::Error + Send + Sync>> {
    let removed = conn.execute("DELETE FROM sms_messages", [])?;
    println!("[Database] delete-now SMS history: {} rows", removed);
    Ok(DeleteCounts {
        sms: removed,
        ..Default::default()
    })
}

/// "Delete all recordings now" — removes every WAV in the recordings
/// directory but leaves call_logs and transcripts intact, so the call
/// list still shows the conversation history without the audio.
pub fn delete_all_recordings(host: &dyn DbHost) -> DeleteCounts {
    let recordings_removed = wipe_recordings_dir(host);
    println!(
        "[Database] delete-now recordings: {} files",
        recordings_removed
    );
    DeleteCounts {
        recordings: recordings_removed,
        ..Default::default()
    }
}

/// Best-effort wipe of every regular file in `<app_data>/recordings/`.
/// Returns the count actually removed; per-file IO errors are logged
/// (so support can see them) but don't abort the loop — partial
/// progress is better than nothing on a slow / contested disk.
///
/// R5-#6: drops the previous `*.wav` extension filter. The directory's
/// purpose is recordings; any future codec (Opus, Vorbis) or debug
/// dump (`.bin` from a SCO codec experiment, `.tmp` from an
/// interrupted writer) is still operator data and must not survive
/// the privacy-panic button. A non-recording living under
/// `recordings/` would be a symptom of misuse anyway, and removing
/// it is the safe action.
fn wipe_recordings_dir(host: &dyn DbHost) -> usize {
    let dir = host.data_dir().join("recordings");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        // Skip subdirs: a future per-day layout would want us to
        // recurse, but the current writer is flat. Surface that as
        // a distinct case in the eprintln so support can spot a
        // schema drift, rather than silently skipping it.
        match entry.file_type() {
            Ok(ft) if ft.is_file() => match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(e) => eprintln!("[Database] delete-now: couldn't remove {:?}: {}", path, e),
            },
            Ok(ft) if ft.is_dir() => {
                eprintln!(
                    "[Database] delete-now: {:?} is a directory — recordings layout changed; not recursing",
                    path
                );
            }
            Ok(_) => {}
            Err(e) => eprintln!("[Database] delete-now: stat {:?}: {}", path, e),
        }
    }
    removed
}

/// Result of a DB-only retention pass — extracted so unit tests can
/// exercise the SQL transitions without an `AppHandle` / recordings
/// directory.
struct PurgeStats {
    purged_ids: Vec<String>,
    logs_removed: usize,
    transcripts_removed: usize,
}

/// DB half of the retention purge. Single SELECT for the snapshot,
/// then one IMMEDIATE transaction that deletes transcripts by parent
/// `call_id` and call_logs by `start_time`. Splitting this from the
/// AppHandle-bearing wrapper keeps the SQL trivially testable against
/// `Connection::open_in_memory`.
fn purge_call_records_before(
    conn: &mut Connection,
    cutoff_ms: i64,
) -> Result<PurgeStats, Box<dyn std::error::Error + Send + Sync>> {
    let purged_ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT call_id FROM call_logs WHERE start_time < ?1")?;
        let rows = stmt.query_map([cutoff_ms], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()?
    };
    if purged_ids.is_empty() {
        return Ok(PurgeStats {
            purged_ids,
            logs_removed: 0,
            transcripts_removed: 0,
        });
    }

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut transcripts_removed = 0usize;
    {
        let mut stmt = tx.prepare("DELETE FROM transcripts WHERE call_id = ?1")?;
        for call_id in &purged_ids {
            transcripts_removed += stmt.execute(params![call_id])?;
        }
    }
    let logs_removed = tx.execute(
        "DELETE FROM call_logs WHERE start_time < ?1",
        params![cutoff_ms],
    )?;
    tx.commit()?;

    Ok(PurgeStats {
        purged_ids,
        logs_removed,
        transcripts_removed,
    })
}

/// One-stop helper to open the app database. Always sets
/// `foreign_keys = ON`, picks a 5 s `busy_timeout` (so the bot path
/// and a manual UI write don't deadlock each other on contention),
/// and runs the migration ladder. Migrations are no-op once
/// `user_version >= CURRENT_VERSION`, so this is cheap on the steady
/// state but defends against the case where a Tauri command fires
/// before `init_database` finished — without this, the command's
/// connection would be talking to an unmigrated schema.
pub fn open_migrated_connection(host: &dyn DbHost) -> Result<Connection, String> {
    // Build the connection in a closure so any failure along the ladder
    // (path resolution, open, PRAGMAs, migrations) is funnelled through
    // one `on_db_error` notification — the legacy app surfaced exactly
    // this to the renderer via an emitted event.
    let result = (|| -> Result<Connection, String> {
        let path = db_path(host).map_err(|e| format!("db_path: {}", e))?;
        let conn = Connection::open(&path).map_err(|e| format!("open db {:?}: {}", path, e))?;
        conn.execute("PRAGMA foreign_keys = ON", [])
            .map_err(|e| format!("foreign_keys: {}", e))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("busy_timeout: {}", e))?;
        run_migrations(&conn).map_err(|e| format!("migrations: {}", e))?;
        Ok(conn)
    })();
    if let Err(ref message) = result {
        host.on_db_error(message);
    }
    result
}

/// Run all migrations needed to bring the connection up to
/// `CURRENT_VERSION`. The whole upgrade runs under a SINGLE IMMEDIATE
/// transaction and bumps `PRAGMA user_version` per step, committing
/// once at the end, so the upgrade is atomic: a partial failure rolls
/// back to the starting version and the next launch re-applies from
/// there cleanly (never re-running an already-committed migration and
/// crashing on a duplicate column / table).
///
/// One transaction — not one per step — is deliberate for concurrency.
/// Several "cold-start" connections can open the fresh DB at once (a
/// Tauri command racing `init_database`). With a lock PER STEP, each of
/// N openers took up to `steps` write locks (~N×9 acquisitions); the
/// winner released and re-acquired the lock between every step, and a
/// loser could starve its `busy_timeout` racing for the lock between
/// those releases — surfacing as `SQLITE_BUSY` ("database is locked").
/// Holding ONE lock for the whole ladder means every loser blocks on a
/// SINGLE `BEGIN IMMEDIATE` (where `busy_timeout` applies cleanly), then
/// observes the winner's committed `user_version` and no-ops. The
/// steady state (already migrated) takes no write lock at all.
fn run_migrations(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Fast path: no write lock once the schema is current, so the hot
    // open path never contends.
    let user_version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if user_version >= CURRENT_VERSION {
        return Ok(());
    }

    println!(
        "Running database migrations from v{} to v{}",
        user_version, CURRENT_VERSION
    );

    let steps: &[(
        u32,
        fn(&Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>>,
    )] = &[
        // 0 -> 1: Initial schema (transcripts + call_logs + config).
        (1, migrate_v1),
        // 1 -> 2: Phase 3 contacts table for PBAP-fetched entries.
        // Used at +CLIP arrival to look up the caller's name and feed
        // it into the receptionist greeting.
        (2, migrate_v2),
        // 2 -> 3: Phase 4f sms_messages table. One row per inbound or
        // outbound SMS — threads are derived on read by grouping rows
        // on `thread_phone`.
        (3, migrate_v3),
        // 3 -> 4: Phase 4g — outbound delivery state and an
        // AI-vs-manual flag on `sms_messages`.
        (4, migrate_v4),
        // 4 -> 5: appointment calendar (services + appointments) and
        // an SMS relevance column so the spam filter can mark inbound
        // messages as 'ignored' without losing the row.
        (5, migrate_v5),
        // 5 -> 6: orders system (products + extras + orders +
        // order_items).
        (6, migrate_v6),
        // 6 -> 7: per-service `requires_address` flag so the bot
        // collects a customer address into notes for services that
        // need one (lawn mowing, plumbing, etc.) and skips the prompt
        // for services that don't (in-shop haircuts, phone consults).
        (7, migrate_v7),
        // 7 -> 8: FTS5 index over transcript text for free-text
        // search. Replaces the call_logs LIKE-on-transcripts join
        // that scaled badly with conversation history.
        (8, migrate_v8),
        // 8 -> 9: SMS approval mode (R11-#16) — extend the
        // sms_messages.status CHECK constraint with
        // 'awaiting_approval' and 'rejected' so the bot can park a
        // draft reply for operator review instead of auto-sending.
        (9, migrate_v9),
    ];

    // Take the write lock ONCE for the whole ladder (IMMEDIATE, so
    // `busy_timeout` covers the wait) — see the fn doc for why per-step
    // locking starved concurrent openers into SQLITE_BUSY. The whole
    // upgrade either fully applies + commits, or rolls back atomically:
    // an ALTER TABLE that fails partway (FK constraint, disk full) leaves
    // `user_version` unchanged, so the next launch re-applies cleanly
    // instead of hitting a half-modified schema.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    // R3-#17: re-read `user_version` INSIDE the lock. The loser of the
    // IMMEDIATE-lock race observes the winner's committed version and
    // skips the whole (already-applied) ladder rather than re-running
    // migrate_v1 and crashing on `duplicate table`.
    let current_version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current_version >= CURRENT_VERSION {
        tx.commit()?;
        return Ok(());
    }

    for (target, run) in steps {
        if current_version >= *target {
            continue;
        }
        run(&tx)?;
        tx.pragma_update(None, "user_version", *target)?;
        println!("[Database] migrated to v{}", target);
    }
    tx.commit()?;

    Ok(())
}

fn migrate_v1(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS transcripts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            call_id TEXT NOT NULL,
            speaker TEXT NOT NULL,
            text TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            is_final BOOLEAN NOT NULL
        );
        CREATE TABLE IF NOT EXISTS call_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            call_id TEXT UNIQUE NOT NULL,
            caller_id TEXT,
            caller_name TEXT,
            start_time INTEGER NOT NULL,
            end_time INTEGER,
            duration INTEGER,
            status TEXT NOT NULL,
            summary TEXT
        );
        CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_transcripts_call_id ON transcripts(call_id);
        CREATE INDEX IF NOT EXISTS idx_call_logs_call_id ON call_logs(call_id);
        CREATE INDEX IF NOT EXISTS idx_call_logs_start_time ON call_logs(start_time);",
    )?;
    Ok(())
}

fn migrate_v3(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // SMS messages — both inbound (received via MAP MNS + MapRuntime
    // GetMessage) and outbound (sent via MAP MAS PushMessage). The
    // thread is derived by grouping rows on `thread_phone`, which is
    // the *normalized* counterparty number — incoming uses the
    // sender's number, outgoing uses the recipient's. `handle` is the
    // MAP message handle for inbound rows (used for dedupe across
    // crashes); outbound rows leave it null.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sms_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            thread_phone TEXT NOT NULL,
            display_name TEXT,
            direction TEXT NOT NULL CHECK(direction IN ('in', 'out')),
            body TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            handle TEXT UNIQUE,
            msg_type TEXT,
            read_flag INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_sms_thread_phone ON sms_messages(thread_phone);
        CREATE INDEX IF NOT EXISTS idx_sms_timestamp ON sms_messages(timestamp);",
    )?;
    Ok(())
}

fn migrate_v4(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Add columns for outbound delivery state and an AI-vs-manual
    // flag. SQLite's ALTER TABLE only supports ADD COLUMN, but that
    // covers what we need. Existing rows default to status='sent'
    // (we never tracked pending/failed before, so the safest
    // assumption for legacy rows is "delivered") and is_ai_reply=0
    // (we can't retroactively know which were AI; conservatively
    // mark them as manual so the UI doesn't lie).
    conn.execute_batch(
        "ALTER TABLE sms_messages ADD COLUMN status TEXT NOT NULL DEFAULT 'sent' \
            CHECK(status IN ('pending','sent','failed'));
         ALTER TABLE sms_messages ADD COLUMN is_ai_reply INTEGER NOT NULL DEFAULT 0;",
    )?;
    Ok(())
}

fn migrate_v5(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Appointment calendar — opt-in feature toggled by the
    // `appointments_enabled` config key. Two tables:
    //
    // `services`: the bookable items (e.g. "lawn mow 30m", "garden
    // cleanup 2h"). Soft-deletable via `active` so a removed service
    // doesn't break historical appointments that referenced it.
    //
    // `appointments`: actual bookings. `start_time` is unix epoch
    // seconds in UTC (the rest of the schema is consistent on this);
    // the configured timezone is applied at render time. We
    // denormalize `service_name` so a row stays readable even after
    // its `service_id` foreign key resolves to NULL (service deleted).
    // `source` distinguishes manual operator entries from bot-driven
    // bookings so the UI can show provenance.
    //
    // `sms_messages.relevance` is added in this same migration so the
    // spam filter (phase 6) doesn't need its own migration. NULL =
    // legacy / outbound / pre-classifier; 'relevant' = passed; 'ignored'
    // = classifier said it's not about the business, so we kept the row
    // for visibility but didn't auto-reply.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS services (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            duration_minutes INTEGER NOT NULL CHECK(duration_minutes > 0),
            description TEXT,
            active INTEGER NOT NULL DEFAULT 1,
            sort_order INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
            -- requires_address column added by migrate_v7
        );
        CREATE INDEX IF NOT EXISTS idx_services_active ON services(active);

        CREATE TABLE IF NOT EXISTS appointments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            service_id INTEGER REFERENCES services(id) ON DELETE SET NULL,
            service_name TEXT NOT NULL,
            start_time INTEGER NOT NULL,
            duration_minutes INTEGER NOT NULL CHECK(duration_minutes > 0),
            customer_phone TEXT,
            customer_name TEXT,
            notes TEXT,
            status TEXT NOT NULL DEFAULT 'booked'
                CHECK(status IN ('booked','cancelled','completed')),
            source TEXT NOT NULL DEFAULT 'manual'
                CHECK(source IN ('sms','call','manual')),
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_appointments_start_time ON appointments(start_time);
        CREATE INDEX IF NOT EXISTS idx_appointments_status ON appointments(status);
        CREATE INDEX IF NOT EXISTS idx_appointments_customer_phone ON appointments(customer_phone);

        ALTER TABLE sms_messages ADD COLUMN relevance TEXT
            CHECK(relevance IS NULL OR relevance IN ('relevant','ignored'));",
    )?;
    Ok(())
}

fn migrate_v6(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Orders feature — opt-in toggle stored alongside calendar in the
    // config.json so a deployment can switch the bot from "books
    // appointments" to "takes orders" (or both) without code changes.
    //
    // Money is integer cents to avoid float drift. We persist the
    // original line_total at order time so a later price change to a
    // product doesn't retroactively rewrite the customer's bill.
    //
    // `order_items.extras_json` is a JSON array of objects of the
    // shape `[{"name":"Bacon","price_delta_cents":250}]`. We store it
    // as a string rather than a separate row table because:
    //   - the bot path only needs the names + deltas back as text for
    //     receipt rendering, not joined queries on extra ids;
    //   - extras don't need referential integrity to a master row
    //     (we copy name + delta at add time, same denormalisation
    //     pattern as `appointments.service_name`).
    //
    // `products` table mirrors `services`: name UNIQUE so the bot's
    // tool layer can look products up by name, soft-deletable via
    // `active`, and a denormalised name column on `order_items` so a
    // historical row stays readable after the product is removed.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS products (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            description TEXT,
            base_price_cents INTEGER NOT NULL CHECK(base_price_cents >= 0),
            active INTEGER NOT NULL DEFAULT 1,
            sort_order INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_products_active ON products(active);

        CREATE TABLE IF NOT EXISTS product_extras (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            product_id INTEGER NOT NULL REFERENCES products(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            price_delta_cents INTEGER NOT NULL,
            sort_order INTEGER NOT NULL DEFAULT 0,
            UNIQUE(product_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_product_extras_product
            ON product_extras(product_id);

        CREATE TABLE IF NOT EXISTS orders (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            customer_phone TEXT,
            customer_name TEXT,
            notes TEXT,
            status TEXT NOT NULL DEFAULT 'pending'
                CHECK(status IN ('pending','preparing','ready','completed','cancelled')),
            source TEXT NOT NULL DEFAULT 'manual'
                CHECK(source IN ('sms','call','manual')),
            total_cents INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_orders_status ON orders(status);
        CREATE INDEX IF NOT EXISTS idx_orders_customer_phone ON orders(customer_phone);
        CREATE INDEX IF NOT EXISTS idx_orders_created_at ON orders(created_at);

        CREATE TABLE IF NOT EXISTS order_items (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            order_id INTEGER NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
            product_id INTEGER REFERENCES products(id) ON DELETE SET NULL,
            product_name TEXT NOT NULL,
            base_price_cents INTEGER NOT NULL,
            quantity INTEGER NOT NULL CHECK(quantity > 0),
            extras_json TEXT NOT NULL DEFAULT '[]',
            line_total_cents INTEGER NOT NULL,
            notes TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_order_items_order ON order_items(order_id);",
    )?;
    Ok(())
}

fn migrate_v8(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // FTS5 virtual table mirroring `transcripts.text`. We use a
    // contentless-rowid FTS5 with explicit triggers (rather than the
    // `content=transcripts` external-content variant) so a stray
    // DELETE on `transcripts` doesn't leave the FTS index pointing at
    // a nonexistent rowid. Triggers handle insert/update/delete.
    //
    // The query layer in `search_call_logs` does the join — FTS5
    // returns matching transcript rowids, and we look them up against
    // `call_logs` to get the actual log entries.
    //
    // SQLite's bundled build (rusqlite "bundled" feature) ships with
    // FTS5 enabled by default — no extra build flag needed.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS transcripts_fts USING fts5(
            text,
            content='',
            tokenize='porter unicode61'
        );

        -- Backfill from any existing rows (no-op on a fresh install
        -- where the table just got created).
        INSERT INTO transcripts_fts (rowid, text)
        SELECT id, text FROM transcripts;

        CREATE TRIGGER IF NOT EXISTS transcripts_ai AFTER INSERT ON transcripts BEGIN
            INSERT INTO transcripts_fts (rowid, text) VALUES (new.id, new.text);
        END;
        CREATE TRIGGER IF NOT EXISTS transcripts_ad AFTER DELETE ON transcripts BEGIN
            INSERT INTO transcripts_fts (transcripts_fts, rowid, text)
                VALUES ('delete', old.id, old.text);
        END;
        CREATE TRIGGER IF NOT EXISTS transcripts_au AFTER UPDATE ON transcripts BEGIN
            INSERT INTO transcripts_fts (transcripts_fts, rowid, text)
                VALUES ('delete', old.id, old.text);
            INSERT INTO transcripts_fts (rowid, text) VALUES (new.id, new.text);
        END;",
    )?;
    Ok(())
}

fn migrate_v9(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // SMS approval mode (R11-#16). Extend sms_messages.status with
    // 'awaiting_approval' and 'rejected' so the auto-reply pipeline
    // can park a bot-drafted reply for operator review.
    //
    // SQLite has no `ALTER TABLE … MODIFY CHECK` — the only way to
    // change a CHECK constraint is to rebuild the table. The standard
    // workflow disables FK enforcement first, but PRAGMA foreign_keys
    // is a no-op inside an open transaction and we run inside the
    // migration ladder's IMMEDIATE tx anyway. That's fine here:
    // sms_messages has no inbound or outbound FKs, so the rebuild
    // doesn't disturb any referential integrity.
    conn.execute_batch(
        "CREATE TABLE sms_messages_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            thread_phone TEXT NOT NULL,
            display_name TEXT,
            direction TEXT NOT NULL CHECK(direction IN ('in', 'out')),
            body TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            handle TEXT UNIQUE,
            msg_type TEXT,
            read_flag INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'sent'
                CHECK(status IN ('pending','sent','failed','awaiting_approval','rejected')),
            is_ai_reply INTEGER NOT NULL DEFAULT 0,
            relevance TEXT
                CHECK(relevance IS NULL OR relevance IN ('relevant','ignored'))
         );
         INSERT INTO sms_messages_new
            (id, thread_phone, display_name, direction, body, timestamp, handle,
             msg_type, read_flag, status, is_ai_reply, relevance)
            SELECT id, thread_phone, display_name, direction, body, timestamp, handle,
                   msg_type, read_flag, status, is_ai_reply, relevance
            FROM sms_messages;
         DROP TABLE sms_messages;
         ALTER TABLE sms_messages_new RENAME TO sms_messages;
         CREATE INDEX IF NOT EXISTS idx_sms_thread_phone ON sms_messages(thread_phone);
         CREATE INDEX IF NOT EXISTS idx_sms_timestamp ON sms_messages(timestamp);",
    )?;
    Ok(())
}

fn migrate_v7(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Default 0 (no address needed) preserves prior behaviour for
    // existing rows; the operator opts services in via the settings
    // panel. The book tool checks this flag and bounces the booking
    // with a clear reason if the customer didn't supply an address —
    // see `calendar::tools::execute_tools_with_source`.
    conn.execute_batch(
        "ALTER TABLE services ADD COLUMN requires_address INTEGER NOT NULL DEFAULT 0;",
    )?;
    Ok(())
}

fn migrate_v2(conn: &Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // `normalized_number` is the canonical key — see `normalize_number`.
    // We keep the original `phone_number` for display so users see
    // numbers formatted as the phone stored them. `display_name`
    // duplicates across rows when one contact has multiple numbers; that
    // wastes a few bytes per row but keeps the lookup-by-number path
    // a single indexed read.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contacts (
            normalized_number TEXT PRIMARY KEY NOT NULL,
            phone_number TEXT NOT NULL,
            display_name TEXT NOT NULL,
            fetched_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_contacts_display_name ON contacts(display_name);",
    )?;
    Ok(())
}

/// Normalize a phone number to a canonical string for matching.
/// Strategy: keep a leading `+` if present, then strip everything that
/// isn't an ASCII digit. This collapses formats like `+1 (555) 123-4567`,
/// `+1-555-123-4567`, and `+15551234567` to the same key without
/// losing the country-code marker. Returns an empty string if no digits
/// were found — empty inputs are treated as no-match by `lookup`.
pub fn normalize_number(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars();
    if let Some(first) = chars.clone().next() {
        if first == '+' {
            out.push('+');
            chars.next();
        }
    }
    for c in chars {
        if c.is_ascii_digit() {
            out.push(c);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContactRecord {
    pub normalized_number: String,
    pub phone_number: String,
    pub display_name: String,
}

/// One SMS row — inbound or outbound. Used by the Tauri commands
/// `save_sms_message`, `get_sms_thread`, and the auto-reply pipeline.
/// `thread_phone` is the *normalized* counterparty number (see
/// `normalize_number`); the UI's thread list groups on this field.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmsRecord {
    pub id: Option<i64>,
    pub thread_phone: String,
    pub display_name: Option<String>,
    /// "in" for received, "out" for sent.
    pub direction: String,
    pub body: String,
    pub timestamp: i64,
    /// MAP message handle for inbound rows; lets us dedupe across
    /// app restarts if the AG re-pushes a notification we already
    /// handled. Null for outbound.
    pub handle: Option<String>,
    /// MAP msg_type ("SMS_GSM", "SMS_CDMA"). Null for outbound and
    /// for rows imported before we tracked it.
    pub msg_type: Option<String>,
    pub read_flag: bool,
    /// Delivery state for outbound rows: "pending" (queued, waiting
    /// for AG ack), "sent" (acknowledged), or "failed" (dispatch
    /// rejected synchronously). Inbound rows always carry "sent" —
    /// the message arrived, that's all we know.
    #[serde(default = "default_status")]
    pub status: String,
    /// True for outbound rows generated by `spawn_sms_auto_reply`
    /// (the Gemma 4 receptionist). Lets the UI show an "AI" tag on
    /// auto-replies vs manual sends. Defaults to false.
    #[serde(default)]
    pub is_ai_reply: bool,
}

fn default_status() -> String {
    "sent".to_string()
}

/// One row in the thread list — what the dashboard renders. Built by
/// `get_sms_threads` from a window over `sms_messages`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmsThreadSummary {
    pub thread_phone: String,
    pub display_name: Option<String>,
    pub last_body: String,
    pub last_timestamp: i64,
    pub last_direction: String,
    pub unread_count: i64,
}

/// In-memory map of normalized phone number → display name. Loaded once
/// at startup from the SQLite contacts table; refreshed atomically when
/// a PBAP fetch completes. Lookup is microseconds, so the runtime event
/// loop can call `lookup` directly inside the +CLIP handler without
/// adding DB latency to the greeting path.
#[derive(Debug, Default, Clone)]
pub struct ContactStore {
    by_number: HashMap<String, String>,
    /// Reverse index: last-7-digits suffix → display name. Catches the
    /// common case where the AG sends a national-format number while
    /// PBAP stored an international one (or vice versa). The 7-digit
    /// suffix is the smallest North-American local number; shorter
    /// matches start producing false positives (e.g. "1234" appears in
    /// many numbers).
    ///
    /// Suffix collisions across *different* people (e.g. an AU mobile
    /// and a US number that happen to share the last 7 digits) get
    /// recorded in `ambiguous_suffix7`; lookups for those suffixes
    /// return `None` rather than guess wrong. Multiple TEL entries for
    /// the same display name are NOT ambiguous — they all collapse to
    /// the same person.
    by_suffix7: HashMap<String, String>,
    ambiguous_suffix7: HashSet<String>,
}

impl ContactStore {
    /// Load all contacts from the database. Called once at startup — if
    /// the table is empty (first run before any PBAP fetch), returns an
    /// empty store.
    pub fn load_from_db(
        conn: &Connection,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut store = Self::default();
        let mut stmt = conn.prepare("SELECT normalized_number, display_name FROM contacts")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (normalized, display) = row?;
            store.insert_one(normalized, display);
        }
        Ok(store)
    }

    /// Look up a display name for a raw phone number. Tries an exact
    /// normalized match first, then falls back to a 7-digit suffix
    /// match. Returns `None` if no match, or if the suffix is
    /// ambiguous across distinct people — the receptionist then greets
    /// with just the number rather than guessing wrong.
    pub fn lookup(&self, raw: &str) -> Option<&str> {
        let normalized = normalize_number(raw);
        if normalized.is_empty() {
            return None;
        }
        if let Some(name) = self.by_number.get(&normalized) {
            return Some(name);
        }
        let suffix = last_n_digits(&normalized, 7)?;
        if self.ambiguous_suffix7.contains(&suffix) {
            return None;
        }
        self.by_suffix7.get(&suffix).map(String::as_str)
    }

    /// Replace the in-memory contents (callers should also persist via
    /// `DatabaseWriter::replace_all_contacts` so the next process start
    /// loads the same set).
    pub fn replace_all(&mut self, contacts: Vec<ContactRecord>) {
        self.by_number.clear();
        self.by_suffix7.clear();
        self.ambiguous_suffix7.clear();
        for record in contacts {
            self.insert_one(record.normalized_number, record.display_name);
        }
    }

    fn insert_one(&mut self, normalized: String, display: String) {
        if let Some(suffix) = last_n_digits(&normalized, 7) {
            if !self.ambiguous_suffix7.contains(&suffix) {
                match self.by_suffix7.get(&suffix) {
                    Some(existing) if existing == &display => {
                        // Same person with multiple TEL fields — not ambiguous.
                    }
                    Some(_) => {
                        // Different display name on the same suffix: refuse to guess.
                        self.by_suffix7.remove(&suffix);
                        self.ambiguous_suffix7.insert(suffix);
                    }
                    None => {
                        self.by_suffix7.insert(suffix, display.clone());
                    }
                }
            }
        }
        self.by_number.insert(normalized, display);
    }

    pub fn len(&self) -> usize {
        self.by_number.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_number.is_empty()
    }
}

fn last_n_digits(normalized: &str, n: usize) -> Option<String> {
    let digits: String = normalized.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < n {
        return None;
    }
    Some(digits[digits.len() - n..].to_string())
}

pub enum DbCommand {
    SaveTranscript {
        call_id: String,
        speaker: String,
        text: String,
        timestamp: i64,
        is_final: bool,
    },
    SaveCallLog {
        call_id: String,
        caller_id: String,
        caller_name: Option<String>,
        start_time: i64,
        status: String,
    },
    UpdateCallLog {
        call_id: String,
        end_time: i64,
        duration: i64,
        status: String,
        summary: Option<String>,
    },
    /// Atomically replace the entire contacts table. Used by the PBAP
    /// runtime when a phonebook fetch finishes — the previous fetch's
    /// rows are deleted in the same transaction so a partial fetch
    /// can't leave stale entries mixed with fresh ones.
    ReplaceAllContacts {
        contacts: Vec<ContactRecord>,
        fetched_at: i64,
    },
    Shutdown,
}

pub struct DatabaseWriter {
    sender: Sender<DbCommand>,
}

impl DatabaseWriter {
    /// Open the SQLite file on the spawning thread, then move the
    /// connection into a background writer thread. Returning a Result
    /// here (rather than `expect`-ing inside the thread) lets the
    /// caller surface a real error to the operator if the DB can't
    /// open — for a call/SMS app, silently dropping every subsequent
    /// SaveTranscript/SaveCallLog into a dead channel was the worst
    /// possible failure mode.
    pub fn new(db_path: PathBuf) -> Result<Self, String> {
        let conn =
            Connection::open(&db_path).map_err(|e| format!("open db {:?}: {}", db_path, e))?;
        let (sender, receiver) = channel();

        thread::spawn(move || {
            loop {
                match receiver.recv() {
                    Ok(DbCommand::SaveTranscript {
                        call_id,
                        speaker,
                        text,
                        timestamp,
                        is_final,
                    }) => {
                        if let Err(e) = conn.execute(
                            "INSERT INTO transcripts (call_id, speaker, text, timestamp, is_final)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![call_id, speaker, text, timestamp, is_final],
                        ) {
                            // Lost transcripts are diagnostic: bubble the SQL
                            // error to stderr so disk-full / locked-DB / schema
                            // drift doesn't silently eat the call's record.
                            eprintln!(
                                "[DB] SaveTranscript({}) failed: {}",
                                call_id, e
                            );
                        }
                    }
                    Ok(DbCommand::SaveCallLog {
                        call_id,
                        caller_id,
                        caller_name,
                        start_time,
                        status,
                    }) => {
                        // Same idempotency story as the Tauri command path:
                        // duplicate SaveCallLog deliveries (bluetooth_commands
                        // and a re-emit can race for the same call_id) update
                        // the existing row instead of failing the whole event.
                        if let Err(e) = conn.execute(
                            "INSERT INTO call_logs (call_id, caller_id, caller_name, start_time, status)
                             VALUES (?1, ?2, ?3, ?4, ?5)
                             ON CONFLICT(call_id) DO UPDATE SET
                                 caller_name = COALESCE(excluded.caller_name, call_logs.caller_name),
                                 status = excluded.status",
                            params![call_id, caller_id, caller_name, start_time, status],
                        ) {
                            eprintln!("[DB] SaveCallLog({}) failed: {}", call_id, e);
                        }
                    }
                    Ok(DbCommand::UpdateCallLog {
                        call_id,
                        end_time,
                        duration,
                        status,
                        summary,
                    }) => {
                        if let Err(e) = conn.execute(
                            "UPDATE call_logs SET end_time = ?1, duration = ?2, status = ?3, summary = ?4 WHERE call_id = ?5",
                            params![end_time, duration, status, summary, call_id],
                        ) {
                            eprintln!("[DB] UpdateCallLog({}) failed: {}", call_id, e);
                        }
                    }
                    Ok(DbCommand::ReplaceAllContacts {
                        contacts,
                        fetched_at,
                    }) => {
                        // Empty input most likely means a PBAP fetch
                        // failed — don't wipe an existing populated
                        // table over a transient transport error.
                        // Same defensive stance as the Tauri command
                        // sibling above.
                        if contacts.is_empty() {
                            eprintln!(
                                "[DB] ReplaceAllContacts received empty list — refusing to wipe."
                            );
                            continue;
                        }
                        // Wrap the delete + reinserts in a single
                        // IMMEDIATE transaction so a crash mid-write
                        // (or any per-row INSERT failure) can't leave
                        // the table in a half-replaced state. The
                        // earlier comment claimed this was already
                        // transactional but the code only used an
                        // IIFE for `?` early-return — no BEGIN was
                        // ever issued.
                        let tx_result: rusqlite::Result<()> = (|| {
                            let tx = conn.unchecked_transaction()?;
                            tx.execute("DELETE FROM contacts", [])?;
                            {
                                let mut stmt = tx.prepare(
                                    "INSERT OR REPLACE INTO contacts \
                                     (normalized_number, phone_number, display_name, fetched_at) \
                                     VALUES (?1, ?2, ?3, ?4)",
                                )?;
                                for record in &contacts {
                                    stmt.execute(params![
                                        record.normalized_number,
                                        record.phone_number,
                                        record.display_name,
                                        fetched_at,
                                    ])?;
                                }
                            }
                            tx.commit()?;
                            Ok(())
                        })();
                        if let Err(e) = tx_result {
                            eprintln!(
                                "[DB] ReplaceAllContacts ({} records) failed: {}",
                                contacts.len(),
                                e
                            );
                        }
                    }
                    Ok(DbCommand::Shutdown) | Err(_) => break,
                }
            }
        });

        Ok(Self { sender })
    }

    pub fn save_transcript(
        &self,
        call_id: &str,
        speaker: &str,
        text: &str,
        timestamp: i64,
        is_final: bool,
    ) {
        let _ = self.sender.send(DbCommand::SaveTranscript {
            call_id: call_id.to_string(),
            speaker: speaker.to_string(),
            text: text.to_string(),
            timestamp,
            is_final,
        });
    }

    pub fn save_call_log(
        &self,
        call_id: &str,
        caller_id: &str,
        caller_name: Option<&str>,
        start_time: i64,
        status: &str,
    ) {
        let _ = self.sender.send(DbCommand::SaveCallLog {
            call_id: call_id.to_string(),
            caller_id: caller_id.to_string(),
            caller_name: caller_name.map(String::from),
            start_time,
            status: status.to_string(),
        });
    }

    pub fn replace_all_contacts(&self, contacts: Vec<ContactRecord>, fetched_at: i64) {
        let _ = self.sender.send(DbCommand::ReplaceAllContacts {
            contacts,
            fetched_at,
        });
    }

    pub fn end_call(
        &self,
        call_id: &str,
        end_time: i64,
        duration: i64,
        status: &str,
        summary: Option<&str>,
    ) {
        let _ = self.sender.send(DbCommand::UpdateCallLog {
            call_id: call_id.to_string(),
            end_time,
            duration,
            status: status.to_string(),
            summary: summary.map(String::from),
        });
    }
}

impl Drop for DatabaseWriter {
    fn drop(&mut self) {
        let _ = self.sender.send(DbCommand::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_number_keeps_leading_plus_and_strips_separators() {
        assert_eq!(normalize_number("+1 (555) 123-4567"), "+15551234567");
        assert_eq!(normalize_number("+44 20 7946 0958"), "+442079460958");
        assert_eq!(normalize_number("(555) 123-4567"), "5551234567");
        assert_eq!(normalize_number("555.123.4567"), "5551234567");
        assert_eq!(normalize_number("  +1 555 1234 \t"), "+15551234");
    }

    #[test]
    fn normalize_number_handles_empty_and_garbage_input() {
        assert_eq!(normalize_number(""), "");
        assert_eq!(normalize_number("ABC"), "");
        assert_eq!(normalize_number("+abc"), "+");
        assert_eq!(normalize_number("+++123"), "+123"); // only first '+' kept
    }

    #[test]
    fn contact_store_lookup_exact_match_after_normalization() {
        let mut store = ContactStore::default();
        store.replace_all(vec![ContactRecord {
            normalized_number: "+15551234567".to_string(),
            phone_number: "+1 (555) 123-4567".to_string(),
            display_name: "Alice Example".to_string(),
        }]);
        assert_eq!(store.lookup("+1 (555) 123-4567"), Some("Alice Example"));
        assert_eq!(store.lookup("+1-555-123-4567"), Some("Alice Example"));
        assert_eq!(store.lookup("15551234567"), Some("Alice Example")); // suffix
    }

    #[test]
    fn contact_store_falls_back_to_seven_digit_suffix_for_format_skew() {
        // PBAP stored international, AG sends national. Same person.
        let mut store = ContactStore::default();
        store.replace_all(vec![ContactRecord {
            normalized_number: "+14155552671".to_string(),
            phone_number: "+1 415 555 2671".to_string(),
            display_name: "Bob Bay".to_string(),
        }]);
        assert_eq!(store.lookup("4155552671"), Some("Bob Bay")); // 10-digit
        assert_eq!(store.lookup("5552671"), Some("Bob Bay")); // 7-digit suffix
        assert_eq!(store.lookup("(415) 555-2671"), Some("Bob Bay"));
    }

    #[test]
    fn contact_store_returns_none_for_unknown_or_empty_numbers() {
        let mut store = ContactStore::default();
        store.replace_all(vec![ContactRecord {
            normalized_number: "+15551234567".to_string(),
            phone_number: "+1 (555) 123-4567".to_string(),
            display_name: "Alice".to_string(),
        }]);
        assert_eq!(store.lookup(""), None);
        assert_eq!(store.lookup("nope"), None);
        assert_eq!(store.lookup("+19999999999"), None);
    }

    #[test]
    fn contact_store_replace_all_removes_previous_entries() {
        let mut store = ContactStore::default();
        store.replace_all(vec![ContactRecord {
            normalized_number: "+15551234567".to_string(),
            phone_number: "+1 555 123 4567".to_string(),
            display_name: "Alice".to_string(),
        }]);
        store.replace_all(vec![ContactRecord {
            normalized_number: "+15559876543".to_string(),
            phone_number: "+1 555 987 6543".to_string(),
            display_name: "Carol".to_string(),
        }]);
        assert_eq!(store.lookup("+15551234567"), None);
        assert_eq!(store.lookup("+15559876543"), Some("Carol"));
    }

    /// Helper: build an in-memory DB with the v1 schema and seed a
    /// pair of call_logs + their transcripts. Returns the connection.
    fn seed_purge_fixture(rows: &[(&str, i64, &[(&str, i64)])]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate_v1(&conn).unwrap();
        for (call_id, start_time, transcripts) in rows {
            conn.execute(
                "INSERT INTO call_logs (call_id, caller_id, caller_name, start_time, status) \
                 VALUES (?1, NULL, NULL, ?2, 'completed')",
                params![call_id, start_time],
            )
            .unwrap();
            for (text, ts) in *transcripts {
                conn.execute(
                    "INSERT INTO transcripts (call_id, speaker, text, timestamp, is_final) \
                     VALUES (?1, 'caller', ?2, ?3, 1)",
                    params![call_id, text, ts],
                )
                .unwrap();
            }
        }
        conn
    }

    /// Regression: previously the purge deleted transcripts by
    /// `transcripts.timestamp < cutoff`, which orphaned rows whose
    /// transcript timestamp was newer than their parent call_log's
    /// `start_time` (clock skew, late writes, batch imports). The new
    /// path deletes by parent `call_id`, so the two tables stay
    /// consistent regardless of timestamp ordering.
    #[test]
    fn purge_drops_transcripts_with_timestamps_newer_than_their_parent() {
        let cutoff = 1_000_000;
        // call_id "old" started before cutoff but its transcript was
        // written AFTER cutoff (e.g. delayed flush). Old code: kept the
        // transcript, deleted the call_log → orphan.
        let mut conn = seed_purge_fixture(&[
            ("old", 500_000, &[("late transcript", 1_500_000)]),
            ("new", 1_500_000, &[("recent transcript", 1_600_000)]),
        ]);

        let stats = purge_call_records_before(&mut conn, cutoff).unwrap();
        assert_eq!(stats.purged_ids, vec!["old".to_string()]);
        assert_eq!(stats.logs_removed, 1);
        assert_eq!(
            stats.transcripts_removed, 1,
            "transcript with newer timestamp must be removed alongside its parent call_log"
        );

        // Confirm no orphans: every transcript still in the DB has a
        // matching call_log row.
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transcripts t \
                 LEFT JOIN call_logs c ON c.call_id = t.call_id \
                 WHERE c.call_id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0, "purge must not leave transcripts orphaned");
    }

    /// Symmetric case: a transcript timestamp older than the cutoff
    /// belongs to a call that started after the cutoff. Old code:
    /// deleted the transcript, kept the call_log → silently nuked
    /// caller speech for an in-window call. New code: keeps both.
    #[test]
    fn purge_keeps_old_transcripts_whose_parent_call_log_is_recent() {
        let cutoff = 1_000_000;
        let mut conn =
            seed_purge_fixture(&[("recent_call", 1_500_000, &[("but old text row", 800_000)])]);

        let stats = purge_call_records_before(&mut conn, cutoff).unwrap();
        assert!(stats.purged_ids.is_empty());
        assert_eq!(stats.logs_removed, 0);
        assert_eq!(
            stats.transcripts_removed, 0,
            "transcript with old timestamp must survive when its parent call_log is in-window"
        );

        let surviving_transcripts: i64 = conn
            .query_row("SELECT COUNT(*) FROM transcripts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(surviving_transcripts, 1);
    }

    /// Empty DB → no-op. The purge must not error on the "no call_logs
    /// found" path; the wrapper fast-returns before opening a tx.
    #[test]
    fn purge_is_noop_when_nothing_to_remove() {
        let mut conn = seed_purge_fixture(&[("recent", 1_500_000, &[("hello", 1_500_500)])]);
        let stats = purge_call_records_before(&mut conn, 1_000_000).unwrap();
        assert!(stats.purged_ids.is_empty());
        assert_eq!(stats.logs_removed, 0);
        assert_eq!(stats.transcripts_removed, 0);
    }

    /// R14-#10: orphan-transcript sweep must not delete recent
    /// transcripts even when their parent `call_logs` row is missing —
    /// the call lifecycle inserts the call_log row separately from the
    /// per-utterance transcript rows, and a hot inbound call can race
    /// the sweep with a still-being-written transcript. The grace
    /// window protects rows whose timestamp is within ORPHAN_GRACE_SECS
    /// of "now" so genuinely abandoned rows still get cleaned up but
    /// fresh rows survive the next sweep.
    #[test]
    fn repair_orphan_transcripts_respects_grace_period() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_v1(&conn).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // Old orphan (well outside the grace window) — deletable.
        conn.execute(
            "INSERT INTO transcripts (call_id, speaker, text, timestamp, is_final) \
             VALUES ('ghost_old', 'caller', 'long-gone utterance', ?1, 1)",
            params![now - ORPHAN_GRACE_SECS - 60],
        )
        .unwrap();
        // Recent orphan (inside grace window) — keep, the parent
        // call_logs row may still be in flight.
        conn.execute(
            "INSERT INTO transcripts (call_id, speaker, text, timestamp, is_final) \
             VALUES ('ghost_recent', 'caller', 'racing parent insert', ?1, 1)",
            params![now - 5],
        )
        .unwrap();
        // Live transcript with a parent — must always survive.
        conn.execute(
            "INSERT INTO call_logs (call_id, caller_id, caller_name, start_time, status) \
             VALUES ('live', NULL, NULL, ?1, 'completed')",
            params![now - 10],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transcripts (call_id, speaker, text, timestamp, is_final) \
             VALUES ('live', 'caller', 'belongs to live call', ?1, 1)",
            params![now - 5],
        )
        .unwrap();

        let removed = repair_orphan_transcripts(&conn).unwrap();
        assert_eq!(removed, 1, "only the old orphan should be reaped");

        let surviving_call_ids: Vec<String> = conn
            .prepare("SELECT call_id FROM transcripts ORDER BY call_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            surviving_call_ids,
            vec!["ghost_recent".to_string(), "live".to_string()],
            "recent orphan + live row must survive the sweep",
        );
    }

    /* =====================================================================
     * R3-#17: Cold-start race tests.
     *
     * Tauri commands fire as soon as the renderer mounts, which can be
     * milliseconds before / concurrent with the explicit
     * `init_database` call from `lib.rs::run`. The reviewer flagged
     * this as a "test it" item because a command observing an
     * unmigrated schema would silently misbehave (e.g. an INSERT into
     * a not-yet-created table fails with a rusqlite error the renderer
     * surfaces as a generic toast).
     *
     * The current invariant is: every `open_migrated_connection` call
     * runs the migration ladder, so even the very first command-fired
     * connection sees `user_version == CURRENT_VERSION`. The tests
     * below lock that invariant in: a single-thread cold call, plus
     * multiple threads racing the same fresh DB path.
     * ================================================================= */

    /// Open a fresh DB file (no AppHandle) and confirm
    /// `run_migrations` brings it to CURRENT_VERSION on the very first
    /// call. Mirrors what would happen if the renderer fired a Tauri
    /// command on a brand-new install before `init_database` ran.
    #[test]
    fn cold_start_first_connection_runs_migrations() {
        let tmp =
            std::env::temp_dir().join(format!("aokie-r3-17-cold-{}.sqlite", std::process::id()));
        // Make sure no leftover file from a prior aborted test run is
        // sitting on disk — would skew the version we observe.
        let _ = std::fs::remove_file(&tmp);

        let conn = Connection::open(&tmp).unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        run_migrations(&conn).unwrap();
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(
            version, CURRENT_VERSION,
            "first connection must observe a fully migrated schema"
        );
        // And the well-known tables exist — a half-applied schema
        // would be the failure mode we're guarding against.
        for table in ["call_logs", "transcripts", "contacts", "sms_messages"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table {} must exist post-migration", table);
        }

        drop(conn);
        let _ = std::fs::remove_file(&tmp);
    }

    /// Race a handful of "Tauri command opening a connection" threads
    /// against each other on a single fresh DB. SQLite's
    /// IMMEDIATE-transaction migrations serialise via the busy_timeout
    /// + PRAGMA user_version check, so all threads should converge on
    /// `CURRENT_VERSION` and observe the migrated schema. This test
    /// failing would mean a thread raced past the version check
    /// before another thread bumped user_version, then re-ran a
    /// migration and crashed on a duplicate column.
    #[test]
    fn cold_start_concurrent_connections_all_observe_migrated_schema() {
        use std::sync::Arc;
        let tmp =
            std::env::temp_dir().join(format!("aokie-r3-17-race-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let path = Arc::new(tmp.clone());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let p = Arc::clone(&path);
                std::thread::spawn(move || {
                    let conn = Connection::open(&*p).unwrap();
                    conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    run_migrations(&conn).unwrap();
                    let v: u32 = conn
                        .pragma_query_value(None, "user_version", |row| row.get(0))
                        .unwrap();
                    v
                })
            })
            .collect();

        for h in handles {
            let v = h.join().unwrap();
            assert_eq!(
                v, CURRENT_VERSION,
                "every concurrent opener must observe the migrated schema"
            );
        }

        let _ = std::fs::remove_file(&tmp);
    }

    /// Multiple calls + multiple transcripts each — confirm both
    /// counts are correct and only in-window rows survive.
    #[test]
    fn purge_removes_all_transcripts_for_each_purged_call_id() {
        let cutoff = 1_000_000;
        let mut conn = seed_purge_fixture(&[
            (
                "old_a",
                500_000,
                &[("a1", 500_100), ("a2", 500_200), ("a3", 500_300)],
            ),
            ("old_b", 700_000, &[("b1", 700_100)]),
            ("recent", 1_500_000, &[("r1", 1_500_100), ("r2", 1_500_200)]),
        ]);

        let stats = purge_call_records_before(&mut conn, cutoff).unwrap();
        assert_eq!(stats.logs_removed, 2);
        assert_eq!(stats.transcripts_removed, 4);

        let surviving_calls: i64 = conn
            .query_row("SELECT COUNT(*) FROM call_logs", [], |row| row.get(0))
            .unwrap();
        let surviving_transcripts: i64 = conn
            .query_row("SELECT COUNT(*) FROM transcripts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(surviving_calls, 1);
        assert_eq!(surviving_transcripts, 2);
    }

    #[test]
    fn migrate_v2_creates_contacts_table_and_supports_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_v1(&conn).unwrap();
        migrate_v2(&conn).unwrap();
        conn.execute(
            "INSERT INTO contacts (normalized_number, phone_number, display_name, fetched_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["+15551234567", "+1 555 123 4567", "Alice", 1700000000_i64],
        )
        .unwrap();
        let store = ContactStore::load_from_db(&conn).unwrap();
        assert_eq!(store.len(), 1);
        assert_eq!(store.lookup("+1-555-123-4567"), Some("Alice"));
    }

    #[test]
    fn migrate_v3_creates_sms_messages_and_supports_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_v1(&conn).unwrap();
        migrate_v2(&conn).unwrap();
        migrate_v3(&conn).unwrap();
        conn.execute(
            "INSERT INTO sms_messages \
             (thread_phone, display_name, direction, body, timestamp, handle, msg_type, read_flag) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "+15551234567",
                "Alice",
                "in",
                "Hi there!",
                1700000000_i64,
                "0001",
                "SMS_GSM",
                0i64,
            ],
        )
        .unwrap();
        // Read back as an SmsRecord.
        let mut stmt = conn
            .prepare(
                "SELECT id, thread_phone, display_name, direction, body, timestamp, \
                        handle, msg_type, read_flag FROM sms_messages",
            )
            .unwrap();
        let row: SmsRecord = stmt
            .query_row([], |row| {
                Ok(SmsRecord {
                    id: row.get(0)?,
                    thread_phone: row.get(1)?,
                    display_name: row.get(2)?,
                    direction: row.get(3)?,
                    body: row.get(4)?,
                    timestamp: row.get(5)?,
                    handle: row.get(6)?,
                    msg_type: row.get(7)?,
                    read_flag: row.get::<_, i64>(8)? != 0,
                    status: "sent".to_string(),
                    is_ai_reply: false,
                })
            })
            .unwrap();
        assert_eq!(row.thread_phone, "+15551234567");
        assert_eq!(row.display_name.as_deref(), Some("Alice"));
        assert_eq!(row.direction, "in");
        assert_eq!(row.body, "Hi there!");
        assert_eq!(row.handle.as_deref(), Some("0001"));
        assert!(!row.read_flag);

        // Direction CHECK constraint must reject anything else.
        let bad = conn.execute(
            "INSERT INTO sms_messages (thread_phone, direction, body, timestamp) VALUES (?1, ?2, ?3, ?4)",
            params!["+1", "sideways", "x", 1i64],
        );
        assert!(bad.is_err());
    }

    #[test]
    fn migrate_v3_dedupes_inbound_messages_by_map_handle() {
        // The MAP handle uniquely identifies an inbound message on the
        // AG. If the same EventReport replays (e.g. our session
        // dropped before we acked), the second insert must fail so we
        // don't double-process. Outbound rows have null handle and
        // SQLite's UNIQUE allows multiple null rows by spec.
        let conn = Connection::open_in_memory().unwrap();
        migrate_v1(&conn).unwrap();
        migrate_v2(&conn).unwrap();
        migrate_v3(&conn).unwrap();
        conn.execute(
            "INSERT INTO sms_messages (thread_phone, direction, body, timestamp, handle) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["+1", "in", "first", 1i64, "ABC123"],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO sms_messages (thread_phone, direction, body, timestamp, handle) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["+1", "in", "second", 2i64, "ABC123"],
        );
        assert!(dup.is_err(), "duplicate handle must be rejected");

        // Multiple null-handle outbound rows are allowed.
        for body in ["one", "two", "three"] {
            conn.execute(
                "INSERT INTO sms_messages (thread_phone, direction, body, timestamp) \
                 VALUES (?1, ?2, ?3, ?4)",
                params!["+1", "out", body, 1i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn migrate_v4_adds_status_and_is_ai_reply_columns() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_v1(&conn).unwrap();
        migrate_v2(&conn).unwrap();
        migrate_v3(&conn).unwrap();
        // Insert a legacy row before v4 runs — its status / is_ai_reply
        // must default to ('sent', 0) under the migration. The whole
        // point of the defaults is making old rows interpretable.
        conn.execute(
            "INSERT INTO sms_messages (thread_phone, direction, body, timestamp) \
             VALUES (?1, ?2, ?3, ?4)",
            params!["+1", "out", "hello legacy", 1i64],
        )
        .unwrap();
        migrate_v4(&conn).unwrap();
        let (status, is_ai_reply): (String, i64) = conn
            .query_row(
                "SELECT status, is_ai_reply FROM sms_messages WHERE body = ?1",
                params!["hello legacy"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "sent");
        assert_eq!(is_ai_reply, 0);
        // CHECK constraint must reject bogus status values.
        let bad = conn.execute(
            "INSERT INTO sms_messages (thread_phone, direction, body, timestamp, status) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["+1", "out", "x", 2i64, "weird"],
        );
        assert!(bad.is_err(), "status CHECK constraint must reject 'weird'");
    }

    #[test]
    fn contact_store_does_not_treat_repeated_tel_for_same_person_as_ambiguous() {
        // A vCard with multiple TEL fields produces multiple ContactRecords
        // sharing a display name. Suffix matching must still resolve.
        let mut store = ContactStore::default();
        store.replace_all(vec![
            ContactRecord {
                normalized_number: "+15551234567".to_string(),
                phone_number: "+1 555 123 4567".to_string(),
                display_name: "Alice".to_string(),
            },
            ContactRecord {
                normalized_number: "+15551234567".to_string(),
                phone_number: "+1 (555) 123-4567".to_string(),
                display_name: "Alice".to_string(),
            },
        ]);
        assert_eq!(store.lookup("5551234567"), Some("Alice"));
    }

    #[test]
    fn contact_store_refuses_ambiguous_suffix_match_across_distinct_people() {
        // Two unrelated numbers in different countries that share the
        // last 7 digits. Suffix lookup must NOT guess; it must return
        // None so the receptionist greets with just the number.
        let mut store = ContactStore::default();
        store.replace_all(vec![
            ContactRecord {
                normalized_number: "+442071234567".to_string(),
                phone_number: "+44 20 7123 4567".to_string(),
                display_name: "London Office".to_string(),
            },
            ContactRecord {
                normalized_number: "+15551234567".to_string(),
                phone_number: "+1 (555) 123-4567".to_string(),
                display_name: "Alice".to_string(),
            },
        ]);
        // Exact matches still resolve.
        assert_eq!(store.lookup("+442071234567"), Some("London Office"));
        assert_eq!(store.lookup("+15551234567"), Some("Alice"));
        // Bare 7-digit suffix is ambiguous → None.
        assert_eq!(store.lookup("1234567"), None);
        // 10-digit national-format AU number that doesn't exact-match either
        // record should also fall back to ambiguous (we don't carry country
        // codes through this path).
        assert_eq!(store.lookup("0212345678"), None);
    }

    #[test]
    fn last_n_digits_returns_none_when_too_short() {
        assert_eq!(
            last_n_digits("+15551234567", 7),
            Some("1234567".to_string())
        );
        assert_eq!(last_n_digits("+1234", 7), None);
        assert_eq!(last_n_digits("", 7), None);
        assert_eq!(
            last_n_digits("+5551234567", 10),
            Some("5551234567".to_string())
        );
    }
}
