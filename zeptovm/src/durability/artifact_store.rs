use std::sync::Mutex;

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

use crate::core::object::{ObjectId, ObjectRef};

/// Backend trait for artifact storage.
pub trait ArtifactBackend: Send + Sync {
  /// Store bytes, return an ObjectRef handle.
  fn store(
    &self,
    data: &[u8],
    media_type: &str,
    ttl_ms: Option<u64>,
  ) -> Result<ObjectRef, String>;

  /// Retrieve artifact bytes and metadata by ObjectId.
  fn retrieve(
    &self,
    object_id: &ObjectId,
  ) -> Result<(ObjectRef, Vec<u8>), String>;

  /// Delete an artifact. Returns true if it existed.
  fn delete(
    &self,
    object_id: &ObjectId,
  ) -> Result<bool, String>;

  /// Remove expired artifacts. Returns count removed.
  fn cleanup_expired(&self) -> Result<u64, String>;
}

/// SQLite-backed artifact store.
///
/// Uses `Mutex<Connection>` to satisfy `Send + Sync`.
/// The reactor shares `Arc<dyn ArtifactBackend>` across
/// concurrent tokio tasks — the mutex serializes DB access.
pub struct SqliteArtifactStore {
  conn: Mutex<Connection>,
}

impl SqliteArtifactStore {
  pub fn open(path: &str) -> Result<Self, String> {
    let conn = Connection::open(path)
      .map_err(|e| format!("artifact store open: {e}"))?;
    let store = Self {
      conn: Mutex::new(conn),
    };
    store.init_schema()?;
    Ok(store)
  }

  pub fn open_in_memory() -> Result<Self, String> {
    let conn = Connection::open_in_memory()
      .map_err(|e| format!("artifact store open: {e}"))?;
    let store = Self {
      conn: Mutex::new(conn),
    };
    store.init_schema()?;
    Ok(store)
  }

  fn init_schema(&self) -> Result<(), String> {
    let conn =
      self.conn.lock().map_err(|e| format!("lock error: {e}"))?;
    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS artifacts (
          object_id INTEGER PRIMARY KEY,
          content_hash TEXT NOT NULL,
          media_type TEXT NOT NULL,
          size_bytes INTEGER NOT NULL,
          data BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          expires_at INTEGER
        );",
      )
      .map_err(|e| format!("schema init: {e}"))?;
    Ok(())
  }

  fn now_ms() -> u64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_millis() as u64
  }
}

impl ArtifactBackend for SqliteArtifactStore {
  fn store(
    &self,
    data: &[u8],
    media_type: &str,
    ttl_ms: Option<u64>,
  ) -> Result<ObjectRef, String> {
    let hash = format!("{:x}", Sha256::digest(data));
    let now = Self::now_ms();
    let expires_at = ttl_ms.map(|ttl| now + ttl);

    let conn =
      self.conn.lock().map_err(|e| format!("lock error: {e}"))?;

    // Insert with NULL object_id — SQLite assigns it.
    conn
      .execute(
        "INSERT INTO artifacts
         (content_hash, media_type,
          size_bytes, data, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
          hash,
          media_type,
          data.len() as i64,
          data,
          now as i64,
          expires_at.map(|e| e as i64),
        ],
      )
      .map_err(|e| format!("store: {e}"))?;

    let object_id =
      ObjectId::from_raw(conn.last_insert_rowid() as u64);

    Ok(ObjectRef {
      object_id,
      content_hash: hash,
      media_type: media_type.to_string(),
      size_bytes: data.len() as u64,
      created_at_ms: now,
      ttl_ms,
    })
  }

  fn retrieve(
    &self,
    object_id: &ObjectId,
  ) -> Result<(ObjectRef, Vec<u8>), String> {
    let conn =
      self.conn.lock().map_err(|e| format!("lock error: {e}"))?;

    let mut stmt = conn
      .prepare(
        "SELECT content_hash, media_type, size_bytes,
                data, created_at, expires_at
         FROM artifacts WHERE object_id = ?1",
      )
      .map_err(|e| format!("retrieve prepare: {e}"))?;

    let result = stmt
      .query_row(params![object_id.raw() as i64], |row| {
        let hash: String = row.get(0)?;
        let media: String = row.get(1)?;
        let size: i64 = row.get(2)?;
        let data: Vec<u8> = row.get(3)?;
        let created: i64 = row.get(4)?;
        let expires: Option<i64> = row.get(5)?;
        Ok((hash, media, size, data, created, expires))
      })
      .map_err(|_| {
        format!("object not found: {}", object_id.raw())
      })?;

    let (hash, media, size, data, created, expires) = result;
    let ttl_ms = expires.map(|e| (e - created).max(0) as u64);

    let obj_ref = ObjectRef {
      object_id: *object_id,
      content_hash: hash,
      media_type: media,
      size_bytes: size as u64,
      created_at_ms: created as u64,
      ttl_ms,
    };
    Ok((obj_ref, data))
  }

  fn delete(
    &self,
    object_id: &ObjectId,
  ) -> Result<bool, String> {
    let conn =
      self.conn.lock().map_err(|e| format!("lock error: {e}"))?;
    let rows = conn
      .execute(
        "DELETE FROM artifacts WHERE object_id = ?1",
        params![object_id.raw() as i64],
      )
      .map_err(|e| format!("delete: {e}"))?;
    Ok(rows > 0)
  }

  fn cleanup_expired(&self) -> Result<u64, String> {
    let now = Self::now_ms() as i64;
    let conn =
      self.conn.lock().map_err(|e| format!("lock error: {e}"))?;
    let rows = conn
      .execute(
        "DELETE FROM artifacts
         WHERE expires_at IS NOT NULL
           AND expires_at < ?1",
        params![now],
      )
      .map_err(|e| format!("cleanup: {e}"))?;
    Ok(rows as u64)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_store_and_retrieve() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let data = b"hello world";
    let obj_ref =
      store.store(data, "text/plain", None).unwrap();

    assert_eq!(obj_ref.media_type, "text/plain");
    assert_eq!(obj_ref.size_bytes, 11);
    assert!(obj_ref.ttl_ms.is_none());
    assert!(!obj_ref.content_hash.is_empty());

    let (retrieved_ref, retrieved_data) =
      store.retrieve(&obj_ref.object_id).unwrap();
    assert_eq!(retrieved_data, data);
    assert_eq!(
      retrieved_ref.content_hash,
      obj_ref.content_hash
    );
  }

  #[test]
  fn test_store_assigns_unique_ids() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let a =
      store.store(b"one", "text/plain", None).unwrap();
    let b =
      store.store(b"two", "text/plain", None).unwrap();
    assert_ne!(a.object_id.raw(), b.object_id.raw());
    // IDs are DB-assigned (1, 2, ...)
    assert_eq!(a.object_id.raw(), 1);
    assert_eq!(b.object_id.raw(), 2);
  }

  #[test]
  fn test_retrieve_nonexistent() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let result =
      store.retrieve(&ObjectId::from_raw(9999));
    assert!(result.is_err());
    assert!(
      result.unwrap_err().contains("object not found")
    );
  }

  #[test]
  fn test_delete_existing() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref =
      store.store(b"data", "text/plain", None).unwrap();
    let existed =
      store.delete(&obj_ref.object_id).unwrap();
    assert!(existed);

    let result = store.retrieve(&obj_ref.object_id);
    assert!(result.is_err());
  }

  #[test]
  fn test_delete_nonexistent_is_ok() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let existed =
      store.delete(&ObjectId::from_raw(9999)).unwrap();
    assert!(!existed);
  }

  #[test]
  fn test_cleanup_expired() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref = store
      .store(b"temp", "text/plain", Some(0))
      .unwrap();

    std::thread::sleep(
      std::time::Duration::from_millis(10),
    );

    let removed = store.cleanup_expired().unwrap();
    assert_eq!(removed, 1);

    let result = store.retrieve(&obj_ref.object_id);
    assert!(result.is_err());
  }

  #[test]
  fn test_no_ttl_survives_cleanup() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref = store
      .store(b"permanent", "text/plain", None)
      .unwrap();

    let removed = store.cleanup_expired().unwrap();
    assert_eq!(removed, 0);

    let (_, data) =
      store.retrieve(&obj_ref.object_id).unwrap();
    assert_eq!(data, b"permanent");
  }

  #[test]
  fn test_content_hash_is_sha256() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let obj_ref =
      store.store(b"test", "text/plain", None).unwrap();
    let expected =
      format!("{:x}", Sha256::digest(b"test"));
    assert_eq!(obj_ref.content_hash, expected);
  }

  #[test]
  fn test_store_binary_data() {
    let store =
      SqliteArtifactStore::open_in_memory().unwrap();
    let binary: Vec<u8> =
      vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A];
    let obj_ref = store
      .store(&binary, "image/png", None)
      .unwrap();

    let (_, retrieved) =
      store.retrieve(&obj_ref.object_id).unwrap();
    assert_eq!(retrieved, binary);
  }

  #[test]
  fn test_concurrent_access() {
    use std::sync::Arc;
    let store = Arc::new(
      SqliteArtifactStore::open_in_memory().unwrap(),
    );
    let handles: Vec<_> = (0..4)
      .map(|i| {
        let s = Arc::clone(&store);
        std::thread::spawn(move || {
          s.store(
            format!("data-{i}").as_bytes(),
            "text/plain",
            None,
          )
          .unwrap()
        })
      })
      .collect();
    let refs: Vec<_> = handles
      .into_iter()
      .map(|h| h.join().unwrap())
      .collect();
    let ids: std::collections::HashSet<_> =
      refs.iter().map(|r| r.object_id.raw()).collect();
    assert_eq!(ids.len(), 4);
  }
}
