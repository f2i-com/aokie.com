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

impl CommandJournal {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
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
        .map_err(|e| e.to_string())?;
        Ok(Self { conn })
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
}
