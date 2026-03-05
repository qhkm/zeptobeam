use std::{
  collections::HashMap,
  fs,
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
};

/// Persists orchestration checkpoints by key.
pub trait CheckpointStore: Send + Sync {
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), String>;
  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, String>;
  fn delete(&self, key: &str) -> Result<(), String>;
}

/// In-memory checkpoint store for tests and ephemeral runs.
#[derive(Clone, Default)]
pub struct InMemoryCheckpointStore {
  inner: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

impl InMemoryCheckpointStore {
  pub fn new() -> Self {
    Self::default()
  }
}

impl CheckpointStore for InMemoryCheckpointStore {
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), String> {
    let mut map = self
      .inner
      .lock()
      .map_err(|_| "checkpoint store mutex poisoned".to_string())?;
    map.insert(key.to_string(), checkpoint.clone());
    Ok(())
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
    let map = self
      .inner
      .lock()
      .map_err(|_| "checkpoint store mutex poisoned".to_string())?;
    Ok(map.get(key).cloned())
  }

  fn delete(&self, key: &str) -> Result<(), String> {
    let mut map = self
      .inner
      .lock()
      .map_err(|_| "checkpoint store mutex poisoned".to_string())?;
    map.remove(key);
    Ok(())
  }
}

/// File-backed checkpoint store. Each key is one JSON file.
pub struct FileCheckpointStore {
  dir: PathBuf,
}

impl FileCheckpointStore {
  pub fn new(dir: impl AsRef<Path>) -> Result<Self, String> {
    let dir = dir.as_ref().to_path_buf();
    fs::create_dir_all(&dir)
      .map_err(|e| format!("create checkpoint dir failed: {}", e))?;
    Ok(Self { dir })
  }

  fn checkpoint_path(&self, key: &str) -> PathBuf {
    let safe = key
      .chars()
      .map(|c| {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
          c
        } else {
          '_'
        }
      })
      .collect::<String>();
    self.dir.join(format!("{}.json", safe))
  }
}

impl CheckpointStore for FileCheckpointStore {
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), String> {
    let path = self.checkpoint_path(key);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(checkpoint)
      .map_err(|e| format!("serialize checkpoint failed: {}", e))?;
    fs::write(&tmp, bytes).map_err(|e| format!("write checkpoint failed: {}", e))?;
    fs::rename(&tmp, &path).map_err(|e| format!("rename checkpoint failed: {}", e))?;
    Ok(())
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
    let path = self.checkpoint_path(key);
    if !path.exists() {
      return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|e| format!("read checkpoint failed: {}", e))?;
    let value = serde_json::from_slice::<serde_json::Value>(&bytes)
      .map_err(|e| format!("parse checkpoint failed: {}", e))?;
    Ok(Some(value))
  }

  fn delete(&self, key: &str) -> Result<(), String> {
    let path = self.checkpoint_path(key);
    if path.exists() {
      fs::remove_file(path).map_err(|e| format!("delete checkpoint failed: {}", e))?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_in_memory_checkpoint_roundtrip() {
    let store = InMemoryCheckpointStore::new();
    let key = "orchestrator-a";
    let payload = serde_json::json!({
      "goal": "ship",
      "pending_tasks": [{"task_id":"t1"}],
    });
    store.save(key, &payload).unwrap();
    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded, payload);
    store.delete(key).unwrap();
    assert!(store.load(key).unwrap().is_none());
  }
}
