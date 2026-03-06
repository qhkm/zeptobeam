use crate::pid::Pid;
use serde::{Deserialize, Serialize};
use std::sync::{
  atomic::{AtomicU64, Ordering},
  Arc, Mutex,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A single entry in the Write-Ahead Log.
///
/// `wal_seq` is the single source of truth for replay and acknowledgment.
/// Recovery replays from `last_acked_wal_seq + 1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalEntry {
  pub wal_seq: u64,
  /// Globally unique message ID (UUID v7), for consumer idempotency.
  pub message_id: u128,
  pub sender: Pid,
  /// Per-sender monotonic sequence, diagnostic only.
  pub sender_seq: u64,
  pub recipient: Pid,
  pub payload: Vec<u8>,
  pub timestamp: u64,
}

/// SQLite-backed WAL store.
///
/// Uses `rusqlite` under the hood. Because `rusqlite::Connection` is `Send`
/// but not `Sync`, we wrap it in `Arc<Mutex<Connection>>` and execute all
/// operations inside `tokio::task::spawn_blocking`.
pub struct SqliteWalStore {
  conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteWalStore {
  /// Create a new WAL store. `path` may be `":memory:"` for tests.
  pub fn new(path: &str) -> Result<Self, BoxError> {
    let conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(
      "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS wal_entries (
                wal_seq     INTEGER PRIMARY KEY,
                message_id  BLOB NOT NULL,
                sender      INTEGER NOT NULL,
                sender_seq  INTEGER NOT NULL,
                recipient   INTEGER NOT NULL,
                payload     BLOB NOT NULL,
                timestamp   INTEGER NOT NULL
            );",
    )?;
    Ok(Self {
      conn: Arc::new(Mutex::new(conn)),
    })
  }

  /// Append a WAL entry. The caller must set `wal_seq` before calling.
  pub async fn append(&self, entry: WalEntry) -> Result<(), BoxError> {
    let conn = Arc::clone(&self.conn);
    tokio::task::spawn_blocking(move || {
      let conn = conn.lock().expect("wal store lock poisoned");
      conn.execute(
        "INSERT INTO wal_entries (wal_seq, message_id, sender, sender_seq, recipient, payload, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
          entry.wal_seq as i64,
          entry.message_id.to_be_bytes().to_vec(),
          entry.sender.raw() as i64,
          entry.sender_seq as i64,
          entry.recipient.raw() as i64,
          entry.payload,
          entry.timestamp as i64,
        ],
      )?;
      Ok::<(), BoxError>(())
    })
    .await??;
    Ok(())
  }

  /// Returns `MAX(wal_seq) + 1` or 1 if the WAL is empty.
  pub async fn next_seq(&self) -> Result<u64, BoxError> {
    let conn = Arc::clone(&self.conn);
    let seq = tokio::task::spawn_blocking(move || {
      let conn = conn.lock().expect("wal store lock poisoned");
      let mut stmt = conn.prepare("SELECT COALESCE(MAX(wal_seq), 0) FROM wal_entries")?;
      let max_seq: i64 = stmt.query_row([], |row| row.get(0))?;
      Ok::<u64, BoxError>((max_seq as u64) + 1)
    })
    .await??;
    Ok(seq)
  }

  /// Replay WAL entries for `recipient` with `wal_seq > after_seq`,
  /// ordered by `wal_seq` ascending.
  pub async fn replay_from(
    &self,
    after_seq: u64,
    recipient: Pid,
  ) -> Result<Vec<WalEntry>, BoxError> {
    let conn = Arc::clone(&self.conn);
    let entries = tokio::task::spawn_blocking(move || {
      let conn = conn.lock().expect("wal store lock poisoned");
      let mut stmt = conn.prepare(
        "SELECT wal_seq, message_id, sender, sender_seq, recipient, payload, timestamp
                 FROM wal_entries
                 WHERE wal_seq > ?1 AND recipient = ?2
                 ORDER BY wal_seq ASC",
      )?;
      let rows = stmt.query_map(
        rusqlite::params![after_seq as i64, recipient.raw() as i64],
        |row| {
          let wal_seq: i64 = row.get(0)?;
          let message_id_bytes: Vec<u8> = row.get(1)?;
          let sender_raw: i64 = row.get(2)?;
          let sender_seq: i64 = row.get(3)?;
          let recipient_raw: i64 = row.get(4)?;
          let payload: Vec<u8> = row.get(5)?;
          let timestamp: i64 = row.get(6)?;

          let message_id_arr: [u8; 16] = message_id_bytes.try_into().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
              1,
              rusqlite::types::Type::Blob,
              "message_id must be 16 bytes".into(),
            )
          })?;
          let message_id = u128::from_be_bytes(message_id_arr);

          Ok(WalEntry {
            wal_seq: wal_seq as u64,
            message_id,
            sender: Pid::from_raw(sender_raw as u64),
            sender_seq: sender_seq as u64,
            recipient: Pid::from_raw(recipient_raw as u64),
            payload,
            timestamp: timestamp as u64,
          })
        },
      )?;

      let mut entries = Vec::new();
      for row in rows {
        entries.push(row?);
      }
      Ok::<Vec<WalEntry>, BoxError>(entries)
    })
    .await??;
    Ok(entries)
  }

  /// Delete all WAL entries with `wal_seq < below_seq`.
  /// Returns the number of rows deleted.
  pub async fn compact(&self, below_seq: u64) -> Result<u64, BoxError> {
    let conn = Arc::clone(&self.conn);
    let deleted = tokio::task::spawn_blocking(move || {
      let conn = conn.lock().expect("wal store lock poisoned");
      let deleted = conn.execute(
        "DELETE FROM wal_entries WHERE wal_seq < ?1",
        rusqlite::params![below_seq as i64],
      )?;
      Ok::<u64, BoxError>(deleted as u64)
    })
    .await??;
    Ok(deleted)
  }
}

/// Convenience wrapper that auto-assigns monotonic `wal_seq` values
/// and timestamps.
pub struct WalWriter {
  store: Arc<SqliteWalStore>,
  next_seq: AtomicU64,
}

impl WalWriter {
  /// Create a new writer, loading the next sequence number from the store.
  pub async fn new(store: Arc<SqliteWalStore>) -> Result<Self, BoxError> {
    let seq = store.next_seq().await?;
    Ok(Self {
      store,
      next_seq: AtomicU64::new(seq),
    })
  }

  /// Append a message to the WAL with auto-assigned `wal_seq` and timestamp.
  /// Returns the assigned `wal_seq`.
  pub async fn append(
    &self,
    message_id: u128,
    sender: Pid,
    sender_seq: u64,
    recipient: Pid,
    payload: Vec<u8>,
  ) -> Result<u64, BoxError> {
    let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
    let timestamp = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;
    let entry = WalEntry {
      wal_seq: seq,
      message_id,
      sender,
      sender_seq,
      recipient,
      payload,
      timestamp,
    };
    self.store.append(entry).await?;
    Ok(seq)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::pid::Pid;

  fn make_store() -> SqliteWalStore {
    SqliteWalStore::new(":memory:").unwrap()
  }

  #[tokio::test]
  async fn test_append_and_replay() {
    let store = make_store();
    let entry = WalEntry {
      wal_seq: 1,
      message_id: 100,
      sender: Pid::from_raw(1),
      sender_seq: 0,
      recipient: Pid::from_raw(2),
      payload: b"hello".to_vec(),
      timestamp: 1000,
    };
    store.append(entry).await.unwrap();

    let entries = store.replay_from(0, Pid::from_raw(2)).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].wal_seq, 1);
    assert_eq!(entries[0].payload, b"hello");
  }

  #[tokio::test]
  async fn test_replay_filters_by_recipient() {
    let store = make_store();
    // Entry for pid 2
    store
      .append(WalEntry {
        wal_seq: 1,
        message_id: 100,
        sender: Pid::from_raw(1),
        sender_seq: 0,
        recipient: Pid::from_raw(2),
        payload: b"for-2".to_vec(),
        timestamp: 1000,
      })
      .await
      .unwrap();
    // Entry for pid 3
    store
      .append(WalEntry {
        wal_seq: 2,
        message_id: 101,
        sender: Pid::from_raw(1),
        sender_seq: 1,
        recipient: Pid::from_raw(3),
        payload: b"for-3".to_vec(),
        timestamp: 1001,
      })
      .await
      .unwrap();

    let entries = store.replay_from(0, Pid::from_raw(2)).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].payload, b"for-2");
  }

  #[tokio::test]
  async fn test_replay_from_after_seq() {
    let store = make_store();
    let pid = Pid::from_raw(2);
    for i in 1..=5 {
      store
        .append(WalEntry {
          wal_seq: i,
          message_id: i as u128,
          sender: Pid::from_raw(1),
          sender_seq: i - 1,
          recipient: pid,
          payload: format!("msg-{i}").into_bytes(),
          timestamp: 1000 + i,
        })
        .await
        .unwrap();
    }

    // Replay from seq 3 (should get 4, 5)
    let entries = store.replay_from(3, pid).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].wal_seq, 4);
    assert_eq!(entries[1].wal_seq, 5);
  }

  #[tokio::test]
  async fn test_compact() {
    let store = make_store();
    let pid = Pid::from_raw(2);
    for i in 1..=5 {
      store
        .append(WalEntry {
          wal_seq: i,
          message_id: i as u128,
          sender: Pid::from_raw(1),
          sender_seq: i - 1,
          recipient: pid,
          payload: format!("msg-{i}").into_bytes(),
          timestamp: 1000 + i,
        })
        .await
        .unwrap();
    }

    // Compact entries below seq 4 (removes 1, 2, 3)
    let deleted = store.compact(4).await.unwrap();
    assert_eq!(deleted, 3);

    // Only entries 4, 5 remain
    let entries = store.replay_from(0, pid).await.unwrap();
    assert_eq!(entries.len(), 2);
  }

  #[tokio::test]
  async fn test_next_seq_empty() {
    let store = make_store();
    let seq = store.next_seq().await.unwrap();
    assert_eq!(seq, 1);
  }

  #[tokio::test]
  async fn test_next_seq_after_appends() {
    let store = make_store();
    store
      .append(WalEntry {
        wal_seq: 1,
        message_id: 100,
        sender: Pid::from_raw(1),
        sender_seq: 0,
        recipient: Pid::from_raw(2),
        payload: b"x".to_vec(),
        timestamp: 1000,
      })
      .await
      .unwrap();
    store
      .append(WalEntry {
        wal_seq: 2,
        message_id: 101,
        sender: Pid::from_raw(1),
        sender_seq: 1,
        recipient: Pid::from_raw(2),
        payload: b"y".to_vec(),
        timestamp: 1001,
      })
      .await
      .unwrap();

    let seq = store.next_seq().await.unwrap();
    assert_eq!(seq, 3);
  }

  #[tokio::test]
  async fn test_wal_writer_auto_assigns_seq() {
    let store = Arc::new(make_store());
    let writer = WalWriter::new(store.clone()).await.unwrap();

    let seq1 = writer
      .append(100, Pid::from_raw(1), 0, Pid::from_raw(2), b"a".to_vec())
      .await
      .unwrap();
    let seq2 = writer
      .append(101, Pid::from_raw(1), 1, Pid::from_raw(2), b"b".to_vec())
      .await
      .unwrap();

    assert_eq!(seq1, 1);
    assert_eq!(seq2, 2);

    let entries = store.replay_from(0, Pid::from_raw(2)).await.unwrap();
    assert_eq!(entries.len(), 2);
  }
}
