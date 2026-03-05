use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

use crate::agent_rt::checkpoint::CheckpointStore;
use crate::agent_rt::error::AgentRtError;

pub struct SqliteCheckpointStore {
  conn: Mutex<Connection>,
}

impl SqliteCheckpointStore {
  pub fn open(path: &str) -> Result<Self, AgentRtError> {
    let conn =
      Connection::open(path).map_err(|e| AgentRtError::Checkpoint(format!("sqlite open: {}", e)))?;
    conn
      .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite pragma: {}", e)))?;
    conn
      .execute(
        "CREATE TABLE IF NOT EXISTS checkpoints (
          key TEXT PRIMARY KEY,
          data BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL
        )",
        [],
      )
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite create table: {}", e)))?;
    Ok(Self {
      conn: Mutex::new(conn),
    })
  }

  pub fn in_memory() -> Result<Self, AgentRtError> {
    Self::open(":memory:")
  }
}

impl CheckpointStore for SqliteCheckpointStore {
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError> {
    let data = serde_json::to_vec(checkpoint)?;
    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_err(|e| AgentRtError::Checkpoint(format!("system time: {}", e)))?
      .as_secs() as i64;
    let conn = self
      .conn
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
    conn
      .execute(
        "INSERT OR REPLACE INTO checkpoints (key, data, created_at, updated_at)
         VALUES (?1, ?2, COALESCE((SELECT created_at FROM checkpoints WHERE key = ?1), ?3), ?3)",
        rusqlite::params![key, data, now],
      )
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite insert: {}", e)))?;
    Ok(())
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
    let mut stmt = conn
      .prepare("SELECT data FROM checkpoints WHERE key = ?1")
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite prepare: {}", e)))?;
    let result = stmt
      .query_row(rusqlite::params![key], |row| {
        let data: Vec<u8> = row.get(0)?;
        Ok(data)
      })
      .optional()
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite query: {}", e)))?;
    match result {
      Some(data) => {
        let value = serde_json::from_slice(&data)?;
        Ok(Some(value))
      }
      None => Ok(None),
    }
  }

  fn delete(&self, key: &str) -> Result<(), AgentRtError> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
    conn
      .execute("DELETE FROM checkpoints WHERE key = ?1", rusqlite::params![key])
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite delete: {}", e)))?;
    Ok(())
  }

  fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
    let mut stmt = conn
      .prepare("SELECT key FROM checkpoints")
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite prepare: {}", e)))?;
    let keys = stmt
      .query_map([], |row| row.get::<_, String>(0))
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite query: {}", e)))?
      .filter_map(|r| r.ok())
      .collect();
    Ok(keys)
  }

  fn prune_before(&self, epoch_secs: i64) -> Result<u64, AgentRtError> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("sqlite mutex poisoned".into()))?;
    let count = conn
      .execute(
        "DELETE FROM checkpoints WHERE updated_at < ?1",
        rusqlite::params![epoch_secs],
      )
      .map_err(|e| AgentRtError::Checkpoint(format!("sqlite prune: {}", e)))?;
    Ok(count as u64)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_sqlite_checkpoint_roundtrip() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    let key = "test-key";
    let payload = serde_json::json!({
      "goal": "test",
      "tasks": [1, 2, 3],
    });

    store.save(key, &payload).unwrap();
    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded, payload);

    store.delete(key).unwrap();
    assert!(store.load(key).unwrap().is_none());
  }

  #[test]
  fn test_sqlite_checkpoint_upsert_preserves_created_at() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    let key = "upsert-key";

    store.save(key, &serde_json::json!({"v": 1})).unwrap();
    store.save(key, &serde_json::json!({"v": 2})).unwrap();

    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded["v"], 2);
  }

  #[test]
  fn test_sqlite_checkpoint_load_missing_key() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    assert!(store.load("nonexistent").unwrap().is_none());
  }

  #[test]
  fn test_sqlite_list_keys() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    store.save("x", &serde_json::json!(1)).unwrap();
    store.save("y", &serde_json::json!(2)).unwrap();
    let mut keys = store.list_keys().unwrap();
    keys.sort();
    assert_eq!(keys, vec!["x", "y"]);
  }

  #[test]
  fn test_sqlite_prune_before() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    store.save("old", &serde_json::json!(1)).unwrap();
    store.save("new", &serde_json::json!(2)).unwrap();

    // Prune with future timestamp should delete everything
    let future = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs() as i64
      + 3600;
    let pruned = store.prune_before(future).unwrap();
    assert_eq!(pruned, 2);
    assert!(store.load("old").unwrap().is_none());
    assert!(store.load("new").unwrap().is_none());
  }
}
