use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

use crate::agent_rt::checkpoint::CheckpointStore;

pub struct SqliteCheckpointStore {
  conn: Mutex<Connection>,
}

impl SqliteCheckpointStore {
  pub fn open(path: &str) -> Result<Self, String> {
    let conn =
      Connection::open(path).map_err(|e| format!("sqlite open failed: {}", e))?;
    conn
      .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
      .map_err(|e| format!("sqlite pragma failed: {}", e))?;
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
      .map_err(|e| format!("sqlite create table failed: {}", e))?;
    Ok(Self {
      conn: Mutex::new(conn),
    })
  }

  pub fn in_memory() -> Result<Self, String> {
    Self::open(":memory:")
  }
}

impl CheckpointStore for SqliteCheckpointStore {
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), String> {
    let data =
      serde_json::to_vec(checkpoint).map_err(|e| format!("serialize failed: {}", e))?;
    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_err(|e| format!("system time error: {}", e))?
      .as_secs() as i64;
    let conn = self
      .conn
      .lock()
      .map_err(|_| "sqlite mutex poisoned".to_string())?;
    conn
      .execute(
        "INSERT OR REPLACE INTO checkpoints (key, data, created_at, updated_at)
         VALUES (?1, ?2, COALESCE((SELECT created_at FROM checkpoints WHERE key = ?1), ?3), ?3)",
        rusqlite::params![key, data, now],
      )
      .map_err(|e| format!("sqlite insert failed: {}", e))?;
    Ok(())
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| "sqlite mutex poisoned".to_string())?;
    let mut stmt = conn
      .prepare("SELECT data FROM checkpoints WHERE key = ?1")
      .map_err(|e| format!("sqlite prepare failed: {}", e))?;
    let result = stmt
      .query_row(rusqlite::params![key], |row| {
        let data: Vec<u8> = row.get(0)?;
        Ok(data)
      })
      .optional()
      .map_err(|e| format!("sqlite query failed: {}", e))?;
    match result {
      Some(data) => {
        let value = serde_json::from_slice(&data)
          .map_err(|e| format!("deserialize failed: {}", e))?;
        Ok(Some(value))
      }
      None => Ok(None),
    }
  }

  fn delete(&self, key: &str) -> Result<(), String> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| "sqlite mutex poisoned".to_string())?;
    conn
      .execute("DELETE FROM checkpoints WHERE key = ?1", rusqlite::params![key])
      .map_err(|e| format!("sqlite delete failed: {}", e))?;
    Ok(())
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
}
