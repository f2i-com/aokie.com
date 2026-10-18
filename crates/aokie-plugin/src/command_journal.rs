//! Crash-persistent idempotency journal for connector commands that can touch
//! the phone. Acceptance is committed before dispatch and the successful
//! result is committed afterwards. A completed retry replays that result; a
//! request-id collision or an acceptance whose completion is unknown fails
//! closed rather than risking a second physical effect.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, PartialEq)]
pub enum Prepare {
    New,
    Replay(Value),
    Pending,
    Collision,
}

pub struct CommandJournal {
    conn: Connection,
}

/// How long a completed command's result is kept (it holds the number that
/// was texted or rung): long past any retry of the same request.
pub const RETENTION_DAYS: u32 = 7;

impl CommandJournal {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    /// The plugin's open: a journal file that is damaged is set aside
    /// (`<file>.corrupt-<unix time>`) for a fresh one, as the outbox is, and
    /// results past [`RETENTION_DAYS`] are pruned. Returns where a damaged
    /// file went.
    pub fn open_or_quarantine(path: &Path) -> Result<(Self, Option<std::path::PathBuf>), String> {
        let (journal, moved) = crate::outbox::open_or_quarantine(path, |p| {
            let conn = Connection::open(p)?;
            crate::outbox::sqlite_quick_check(&conn)?;
            Self::schema(&conn)?;
            Ok(Self { conn })
        })?;
        match journal.prune_completed(RETENTION_DAYS) {
            Ok(0) => {}
            Ok(n) => eprintln!("[aokie-plugin] command journal: pruned {n} result(s) past {RETENTION_DAYS} days"),
            Err(e) => eprintln!("[aokie-plugin] command journal prune failed: {e}"),
        }
        Ok((journal, moved))
    }

    /// Completed results kept no longer than `days` days. A result is there
    /// for a retry of the same request to be answered with it; host retries
    /// come within minutes, and each holds the number that was texted or
    /// rung. An acceptance with no result yet is kept (its outcome is not
    /// known, and it holds no number).
    pub fn prune_completed(&self, days: u32) -> Result<usize, String> {
        self.conn
            .execute(
                "DELETE FROM command_journal
                  WHERE result_json IS NOT NULL
                    AND completed_at < datetime('now', ?1)",
                params![format!("-{days} days")],
            )
            .map_err(|e| e.to_string())
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        Self::schema(&conn).map_err(|e| e.to_string())?;
        Ok(Self { conn })
    }

    fn schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS command_journal (
                 request_id TEXT PRIMARY KEY,
                 command TEXT NOT NULL,
                 payload_hash TEXT NOT NULL,
                 result_json TEXT,
                 accepted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                 completed_at TEXT
             );",
        )
    }

    pub fn prepare(
        &mut self,
        request_id: &str,
        command: &str,
        payload: &Value,
    ) -> Result<Prepare, String> {
        let payload_hash = payload_hash(payload)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let existing: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT command, payload_hash, result_json
                   FROM command_journal WHERE request_id = ?1",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        let decision = match existing {
            Some((stored_command, stored_hash, _))
                if stored_command != command || stored_hash != payload_hash =>
            {
                Prepare::Collision
            }
            Some((_, _, Some(result))) => Prepare::Replay(
                serde_json::from_str(&result)
                    .map_err(|e| format!("invalid journal result: {e}"))?,
            ),
            Some((_, _, None)) => Prepare::Pending,
            None => {
                tx.execute(
                    "INSERT INTO command_journal(request_id, command, payload_hash)
                     VALUES (?1, ?2, ?3)",
                    params![request_id, command, payload_hash],
                )
                .map_err(|e| e.to_string())?;
                Prepare::New
            }
        };
        tx.commit().map_err(|e| e.to_string())?;
        Ok(decision)
    }

    pub fn complete(&mut self, request_id: &str, result: &Value) -> Result<(), String> {
        let encoded = serde_json::to_string(result).map_err(|e| e.to_string())?;
        let changed = self
            .conn
            .execute(
                "UPDATE command_journal
                    SET result_json = ?2, completed_at = CURRENT_TIMESTAMP
                  WHERE request_id = ?1 AND result_json IS NULL",
                params![request_id, encoded],
            )
            .map_err(|e| e.to_string())?;
        if changed == 1 {
            Ok(())
        } else {
            Err("journal acceptance disappeared or was already completed".to_string())
        }
    }

    /// The results of `command` completed in the last `days` days, oldest
    /// first: what a run said it did (review 3 #7: the texts it handed to the
    /// radio, each of which is owed a sent or failed event).
    pub fn completed_results(&self, command: &str, days: u32) -> Result<Vec<Value>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT result_json FROM command_journal
                  WHERE command = ?1
                    AND result_json IS NOT NULL
                    AND completed_at >= datetime('now', ?2)
                  ORDER BY completed_at",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![command, format!("-{days} days")], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut results = Vec::new();
        for row in rows {
            let text = row.map_err(|e| e.to_string())?;
            if let Ok(value) = serde_json::from_str(&text) {
                results.push(value);
            }
        }
        Ok(results)
    }

    /// Validation/refusal errors occur before an action is accepted. Remove
    /// that reservation so a corrected retry can use the same request id.
    pub fn abandon(&mut self, request_id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM command_journal
                  WHERE request_id = ?1 AND result_json IS NULL",
                params![request_id],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Prune the journal at `path` on a connection of its own: the outbox
/// replay thread's maintenance tick does, every ten minutes (review 4 #15:
/// results were pruned only at start, so a plugin that ran for weeks kept
/// every number it texted or rang). No journal is made where there is none.
/// Returns how many results went.
pub fn prune_file(path: &Path) -> usize {
    if !path.exists() {
        return 0;
    }
    match CommandJournal::open(path).and_then(|journal| journal.prune_completed(RETENTION_DAYS)) {
        Ok(0) => 0,
        Ok(n) => {
            eprintln!("[aokie-plugin] command journal: pruned {n} result(s) past {RETENTION_DAYS} days");
            n
        }
        Err(e) => {
            eprintln!("[aokie-plugin] command journal prune failed: {e}");
            0
        }
    }
}

fn payload_hash(payload: &Value) -> Result<String, String> {
    let canonical = canonicalize(payload);
    let encoded = serde_json::to_vec(&canonical).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonicalize(&object[key]));
            }
            Value::Object(sorted)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completed_request_replays_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("commands.sqlite3");
        {
            let mut journal = CommandJournal::open(&path).unwrap();
            assert_eq!(
                journal.prepare("req-1", "sms.send", &json!({"b": 2, "a": 1})),
                Ok(Prepare::New)
            );
            journal
                .complete("req-1", &json!({"queued": true, "messageId": "sms_1"}))
                .unwrap();
        }
        let mut journal = CommandJournal::open(&path).unwrap();
        assert_eq!(
            journal.prepare("req-1", "sms.send", &json!({"a": 1, "b": 2})),
            Ok(Prepare::Replay(
                json!({"queued": true, "messageId": "sms_1"})
            ))
        );
    }

    #[test]
    fn reused_request_id_with_other_payload_is_a_collision() {
        let mut journal = CommandJournal::open_in_memory().unwrap();
        assert_eq!(
            journal.prepare("req-1", "sms.send", &json!({"body": "one"})),
            Ok(Prepare::New)
        );
        assert_eq!(
            journal.prepare("req-1", "sms.send", &json!({"body": "two"})),
            Ok(Prepare::Collision)
        );
    }

    #[test]
    fn incomplete_acceptance_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("commands.sqlite3");
        CommandJournal::open(&path)
            .unwrap()
            .prepare("req-1", "call.dial", &json!({"number": "+61400000000"}))
            .unwrap();
        assert_eq!(
            CommandJournal::open(&path).unwrap().prepare(
                "req-1",
                "call.dial",
                &json!({"number": "+61400000000"})
            ),
            Ok(Prepare::Pending)
        );
    }

    /// Review 2026-10: results (each with the number texted or rung) were
    /// kept for ever. Completed ones past the retention go; an acceptance
    /// with no result yet stays (its outcome is unknown).
    #[test]
    fn completed_results_past_retention_are_pruned() {
        let mut journal = CommandJournal::open_in_memory().unwrap();
        journal.prepare("old", "sms.send", &json!({"to": "+61400000000"})).unwrap();
        journal.complete("old", &json!({"to": "+61400000000"})).unwrap();
        journal.prepare("new", "sms.send", &json!({"to": "+61400000001"})).unwrap();
        journal.complete("new", &json!({"to": "+61400000001"})).unwrap();
        journal.prepare("open", "call.dial", &json!({"number": "+61400000002"})).unwrap();
        journal
            .conn
            .execute(
                "UPDATE command_journal SET completed_at = datetime('now', '-8 days'),
                        accepted_at = datetime('now', '-8 days')
                  WHERE request_id IN ('old', 'open')",
                [],
            )
            .unwrap();
        assert_eq!(journal.prune_completed(RETENTION_DAYS).unwrap(), 1);
        assert_eq!(
            journal.prepare("old", "sms.send", &json!({"to": "+61400000000"})),
            Ok(Prepare::New),
            "the pruned result is gone"
        );
        assert!(matches!(
            journal.prepare("new", "sms.send", &json!({"to": "+61400000001"})),
            Ok(Prepare::Replay(_))
        ));
        assert_eq!(
            journal.prepare("open", "call.dial", &json!({"number": "+61400000002"})),
            Ok(Prepare::Pending)
        );
    }

    /// Review 4 #15: the replay thread's maintenance tick prunes the journal
    /// on a connection of its own, while the plugin's stays open; no journal
    /// is made where there is none.
    #[test]
    fn the_journal_is_pruned_while_the_plugin_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-journal.sqlite3");
        assert_eq!(prune_file(&path), 0);
        assert!(!path.exists(), "none made");
        let mut journal = CommandJournal::open(&path).unwrap();
        journal.prepare("old", "sms.send", &json!({"to": "+61400000000"})).unwrap();
        journal.complete("old", &json!({"to": "+61400000000"})).unwrap();
        journal.prepare("new", "sms.send", &json!({"to": "+61400000001"})).unwrap();
        journal.complete("new", &json!({"to": "+61400000001"})).unwrap();
        journal
            .conn
            .execute(
                "UPDATE command_journal SET completed_at = datetime('now', '-8 days') WHERE request_id = 'old'",
                [],
            )
            .unwrap();
        assert_eq!(prune_file(&path), 1);
        assert_eq!(journal.prepare("old", "sms.send", &json!({"to": "+61400000000"})), Ok(Prepare::New));
        assert!(matches!(
            journal.prepare("new", "sms.send", &json!({"to": "+61400000001"})),
            Ok(Prepare::Replay(_))
        ));
        let tick = include_str!("event_bridge.rs");
        assert!(tick.contains("crate::command_journal::prune_file("), "the maintenance tick prunes it");
    }

    /// Review 2026-10: a corrupt journal file stopped the plugin at every
    /// start. It is set aside and a fresh one opens.
    #[test]
    fn a_corrupt_journal_is_set_aside_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("command-journal.sqlite3");
        std::fs::write(&path, vec![0x5a; 4096]).unwrap();
        let (mut journal, moved) = CommandJournal::open_or_quarantine(&path).unwrap();
        assert!(moved.expect("set aside").exists());
        assert_eq!(journal.prepare("r", "sms.send", &json!({})), Ok(Prepare::New));
    }
}
