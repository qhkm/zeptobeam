use std::{
  collections::HashMap,
  fs,
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
};

use crate::agent_rt::error::AgentRtError;

/// Persists orchestration checkpoints by key.
pub trait CheckpointStore: Send + Sync {
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError>;
  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError>;
  fn delete(&self, key: &str) -> Result<(), AgentRtError>;

  fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    Ok(vec![])
  }

  /// Delete checkpoints with updated_at before the given epoch timestamp (seconds).
  /// Returns count of pruned entries.
  fn prune_before(&self, _epoch_secs: i64) -> Result<u64, AgentRtError> {
    Ok(0)
  }
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
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError> {
    let mut map = self
      .inner
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
    map.insert(key.to_string(), checkpoint.clone());
    Ok(())
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError> {
    let map = self
      .inner
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
    Ok(map.get(key).cloned())
  }

  fn delete(&self, key: &str) -> Result<(), AgentRtError> {
    let mut map = self
      .inner
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
    map.remove(key);
    Ok(())
  }

  fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    let map = self
      .inner
      .lock()
      .map_err(|_| AgentRtError::Checkpoint("mutex poisoned".into()))?;
    Ok(map.keys().cloned().collect())
  }
}

/// File-backed checkpoint store. Each key is one JSON file.
pub struct FileCheckpointStore {
  dir: PathBuf,
}

impl FileCheckpointStore {
  pub fn new(dir: impl AsRef<Path>) -> Result<Self, AgentRtError> {
    let dir = dir.as_ref().to_path_buf();
    fs::create_dir_all(&dir)?;
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
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), AgentRtError> {
    let path = self.checkpoint_path(key);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(checkpoint)?;
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(())
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, AgentRtError> {
    let path = self.checkpoint_path(key);
    if !path.exists() {
      return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let value = serde_json::from_slice::<serde_json::Value>(&bytes)?;
    Ok(Some(value))
  }

  fn delete(&self, key: &str) -> Result<(), AgentRtError> {
    let path = self.checkpoint_path(key);
    if path.exists() {
      fs::remove_file(path)?;
    }
    Ok(())
  }

  fn list_keys(&self) -> Result<Vec<String>, AgentRtError> {
    let mut keys = Vec::new();
    for entry in fs::read_dir(&self.dir)? {
      let entry = entry?;
      let path = entry.path();
      if path.extension().map_or(false, |ext| ext == "json") {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
          keys.push(stem.to_string());
        }
      }
    }
    Ok(keys)
  }

  fn prune_before(&self, epoch_secs: i64) -> Result<u64, AgentRtError> {
    let threshold = std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs as u64);
    let mut pruned = 0u64;
    for entry in fs::read_dir(&self.dir)? {
      let entry = entry?;
      let path = entry.path();
      if path.extension().map_or(false, |ext| ext == "json") {
        if let Ok(meta) = fs::metadata(&path) {
          if let Ok(modified) = meta.modified() {
            if modified < threshold {
              fs::remove_file(&path)?;
              pruned += 1;
            }
          }
        }
      }
    }
    Ok(pruned)
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

  #[test]
  fn test_in_memory_list_keys() {
    let store = InMemoryCheckpointStore::new();
    store.save("a", &serde_json::json!(1)).unwrap();
    store.save("b", &serde_json::json!(2)).unwrap();
    let mut keys = store.list_keys().unwrap();
    keys.sort();
    assert_eq!(keys, vec!["a", "b"]);
  }

  #[test]
  fn test_file_checkpoint_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    let key = "test-roundtrip";
    let payload = serde_json::json!({"goal": "ship", "tasks": [1, 2, 3]});

    store.save(key, &payload).unwrap();
    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded, payload);

    store.delete(key).unwrap();
    assert!(store.load(key).unwrap().is_none());
  }

  #[test]
  fn test_file_checkpoint_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    let key = "overwrite-key";

    store.save(key, &serde_json::json!({"v": 1})).unwrap();
    store.save(key, &serde_json::json!({"v": 2})).unwrap();

    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded["v"], 2);
  }

  #[test]
  fn test_file_checkpoint_load_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    assert!(store.load("nonexistent").unwrap().is_none());
  }

  #[test]
  fn test_file_checkpoint_delete_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    // Should not error when deleting a key that doesn't exist
    store.delete("nonexistent").unwrap();
  }

  #[test]
  fn test_file_checkpoint_key_sanitization() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    // Keys with special chars should be sanitized but still work
    let key = "test/key:with<special>chars";
    let payload = serde_json::json!({"sanitized": true});

    store.save(key, &payload).unwrap();
    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded, payload);
  }

  #[test]
  fn test_file_checkpoint_atomic_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path()).unwrap();
    let key = "atomic-test";
    let payload = serde_json::json!({"data": "important"});

    store.save(key, &payload).unwrap();

    // Verify no .tmp files remain after save
    let tmp_files: Vec<_> = std::fs::read_dir(dir.path())
      .unwrap()
      .filter_map(|e| e.ok())
      .filter(|e| e.path().extension().map_or(false, |ext| ext == "tmp"))
      .collect();
    assert!(tmp_files.is_empty(), "No .tmp files should remain after save");
  }
}
