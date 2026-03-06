use async_trait::async_trait;
use crate::pid::Pid;
use std::sync::{Arc, Mutex};

/// Trait for persisting process checkpoints.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn save(
        &self,
        pid: Pid,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    async fn load(
        &self,
        pid: Pid,
    ) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>>;

    async fn delete(
        &self,
        pid: Pid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// SQLite-backed checkpoint store.
///
/// Uses `rusqlite` under the hood. Because `rusqlite::Connection` is `Send`
/// but not `Sync`, we wrap it in `Arc<Mutex<Connection>>` and execute all
/// operations inside `tokio::task::spawn_blocking`.
pub struct SqliteCheckpointStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteCheckpointStore {
    /// Create a new store. `path` may be `":memory:"` for tests.
    pub fn new(path: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                pid      INTEGER PRIMARY KEY,
                data     BLOB NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl CheckpointStore for SqliteCheckpointStore {
    async fn save(
        &self,
        pid: Pid,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let conn = Arc::clone(&self.conn);
        let pid_raw = pid.raw() as i64;
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("checkpoint store lock poisoned");
            conn.execute(
                "INSERT OR REPLACE INTO checkpoints (pid, data, updated_at)
                 VALUES (?1, ?2, datetime('now'))",
                rusqlite::params![pid_raw, data],
            )?;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await??;
        Ok(())
    }

    async fn load(
        &self,
        pid: Pid,
    ) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
        let conn = Arc::clone(&self.conn);
        let pid_raw = pid.raw() as i64;
        let result = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("checkpoint store lock poisoned");
            let mut stmt = conn.prepare("SELECT data FROM checkpoints WHERE pid = ?1")?;
            let mut rows = stmt.query(rusqlite::params![pid_raw])?;
            match rows.next()? {
                Some(row) => {
                    let data: Vec<u8> = row.get(0)?;
                    Ok::<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>>(Some(data))
                }
                None => Ok(None),
            }
        })
        .await??;
        Ok(result)
    }

    async fn delete(
        &self,
        pid: Pid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let conn = Arc::clone(&self.conn);
        let pid_raw = pid.raw() as i64;
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("checkpoint store lock poisoned");
            conn.execute(
                "DELETE FROM checkpoints WHERE pid = ?1",
                rusqlite::params![pid_raw],
            )?;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;

    fn make_store() -> SqliteCheckpointStore {
        SqliteCheckpointStore::new(":memory:").unwrap()
    }

    #[tokio::test]
    async fn test_save_and_load() {
        let store = make_store();
        let pid = Pid::from_raw(1);
        store.save(pid, b"hello").await.unwrap();
        let data = store.load(pid).await.unwrap();
        assert_eq!(data, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_load_nonexistent() {
        let store = make_store();
        let pid = Pid::from_raw(999);
        let data = store.load(pid).await.unwrap();
        assert_eq!(data, None);
    }

    #[tokio::test]
    async fn test_save_overwrites() {
        let store = make_store();
        let pid = Pid::from_raw(1);
        store.save(pid, b"first").await.unwrap();
        store.save(pid, b"second").await.unwrap();
        let data = store.load(pid).await.unwrap();
        assert_eq!(data, Some(b"second".to_vec()));
    }

    #[tokio::test]
    async fn test_delete() {
        let store = make_store();
        let pid = Pid::from_raw(1);
        store.save(pid, b"data").await.unwrap();
        store.delete(pid).await.unwrap();
        let data = store.load(pid).await.unwrap();
        assert_eq!(data, None);
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let store = make_store();
        let pid = Pid::from_raw(999);
        store.delete(pid).await.unwrap();
    }
}
