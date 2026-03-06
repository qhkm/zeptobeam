use rusqlite::{params, Connection, Result as SqlResult};

use crate::pid::Pid;

/// A process snapshot for recovery.
#[derive(Debug, Clone)]
pub struct Snapshot {
  pub pid: Pid,
  pub version: i64,
  pub state_blob: Vec<u8>,
  pub mailbox_cursor: i64,
  pub pending_effects: Option<String>,
}

/// SQLite-backed snapshot store for process recovery.
pub struct SnapshotStore {
  conn: Connection,
}

impl SnapshotStore {
  pub fn open_in_memory() -> SqlResult<Self> {
    let conn = Connection::open_in_memory()?;
    let store = Self { conn };
    store.init_schema()?;
    Ok(store)
  }

  pub fn open(path: &str) -> SqlResult<Self> {
    let conn = Connection::open(path)?;
    let store = Self { conn };
    store.init_schema()?;
    Ok(store)
  }

  /// Initialise the snapshot schema on an existing connection.
  /// Used for shared-connection atomic commit.
  pub fn init_on_connection(
    conn: &Connection,
  ) -> SqlResult<()> {
    conn.execute_batch(
      "CREATE TABLE IF NOT EXISTS snapshots (
                pid INTEGER NOT NULL,
                version INTEGER NOT NULL,
                state_blob BLOB NOT NULL,
                mailbox_cursor INTEGER NOT NULL,
                pending_effects TEXT,
                created_at TEXT NOT NULL \
                  DEFAULT (datetime('now')),
                PRIMARY KEY (pid, version)
            );",
    )?;
    Ok(())
  }

  fn init_schema(&self) -> SqlResult<()> {
    self.conn.execute_batch(
      "CREATE TABLE IF NOT EXISTS snapshots (
                pid INTEGER NOT NULL,
                version INTEGER NOT NULL,
                state_blob BLOB NOT NULL,
                mailbox_cursor INTEGER NOT NULL,
                pending_effects TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (pid, version)
            );",
    )?;
    Ok(())
  }

  /// Save a snapshot (upsert by pid+version).
  pub fn save(&self, snapshot: &Snapshot) -> SqlResult<()> {
    self.conn.execute(
      "INSERT OR REPLACE INTO snapshots \
       (pid, version, state_blob, mailbox_cursor, pending_effects) \
       VALUES (?1, ?2, ?3, ?4, ?5)",
      params![
        snapshot.pid.raw() as i64,
        snapshot.version,
        snapshot.state_blob,
        snapshot.mailbox_cursor,
        snapshot.pending_effects,
      ],
    )?;
    Ok(())
  }

  /// Load the latest snapshot for a process.
  pub fn load_latest(
    &self,
    pid: Pid,
  ) -> SqlResult<Option<Snapshot>> {
    let mut stmt = self.conn.prepare(
      "SELECT pid, version, state_blob, mailbox_cursor, \
       pending_effects FROM snapshots \
       WHERE pid = ?1 ORDER BY version DESC LIMIT 1",
    )?;
    let mut rows =
      stmt.query_map(params![pid.raw() as i64], |row| {
        let pid_raw: i64 = row.get(0)?;
        let version: i64 = row.get(1)?;
        let state_blob: Vec<u8> = row.get(2)?;
        let mailbox_cursor: i64 = row.get(3)?;
        let pending_effects: Option<String> = row.get(4)?;
        Ok(Snapshot {
          pid: Pid::from_raw(pid_raw as u64),
          version,
          state_blob,
          mailbox_cursor,
          pending_effects,
        })
      })?;

    match rows.next() {
      Some(Ok(snap)) => Ok(Some(snap)),
      Some(Err(e)) => Err(e),
      None => Ok(None),
    }
  }

  /// Load a specific version.
  pub fn load_version(
    &self,
    pid: Pid,
    version: i64,
  ) -> SqlResult<Option<Snapshot>> {
    let mut stmt = self.conn.prepare(
      "SELECT pid, version, state_blob, mailbox_cursor, \
       pending_effects FROM snapshots \
       WHERE pid = ?1 AND version = ?2",
    )?;
    let mut rows = stmt
      .query_map(params![pid.raw() as i64, version], |row| {
        let pid_raw: i64 = row.get(0)?;
        let version: i64 = row.get(1)?;
        let state_blob: Vec<u8> = row.get(2)?;
        let mailbox_cursor: i64 = row.get(3)?;
        let pending_effects: Option<String> = row.get(4)?;
        Ok(Snapshot {
          pid: Pid::from_raw(pid_raw as u64),
          version,
          state_blob,
          mailbox_cursor,
          pending_effects,
        })
      })?;

    match rows.next() {
      Some(Ok(snap)) => Ok(Some(snap)),
      Some(Err(e)) => Err(e),
      None => Ok(None),
    }
  }

  /// Delete snapshots older than a given version (for compaction).
  pub fn delete_before(
    &self,
    pid: Pid,
    before_version: i64,
  ) -> SqlResult<usize> {
    self.conn.execute(
      "DELETE FROM snapshots WHERE pid = ?1 AND version < ?2",
      params![pid.raw() as i64, before_version],
    )
  }

  /// Count snapshots for a process.
  pub fn count(&self, pid: Pid) -> SqlResult<i64> {
    self.conn.query_row(
      "SELECT COUNT(*) FROM snapshots WHERE pid = ?1",
      params![pid.raw() as i64],
      |row| row.get(0),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::pid::Pid;

  fn make_snapshot(pid: Pid, version: i64) -> Snapshot {
    Snapshot {
      pid,
      version,
      state_blob: format!("state-v{version}").into_bytes(),
      mailbox_cursor: version * 10,
      pending_effects: None,
    }
  }

  #[test]
  fn test_snapshot_save_and_load() {
    let store = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);
    let snap = make_snapshot(pid, 1);
    store.save(&snap).unwrap();

    let loaded = store.load_latest(pid).unwrap().unwrap();
    assert_eq!(loaded.pid, pid);
    assert_eq!(loaded.version, 1);
    assert_eq!(loaded.state_blob, b"state-v1");
    assert_eq!(loaded.mailbox_cursor, 10);
  }

  #[test]
  fn test_snapshot_load_latest() {
    let store = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    store.save(&make_snapshot(pid, 1)).unwrap();
    store.save(&make_snapshot(pid, 2)).unwrap();
    store.save(&make_snapshot(pid, 3)).unwrap();

    let latest = store.load_latest(pid).unwrap().unwrap();
    assert_eq!(latest.version, 3);
  }

  #[test]
  fn test_snapshot_load_specific_version() {
    let store = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    store.save(&make_snapshot(pid, 1)).unwrap();
    store.save(&make_snapshot(pid, 2)).unwrap();

    let v1 = store.load_version(pid, 1).unwrap().unwrap();
    assert_eq!(v1.version, 1);

    let v2 = store.load_version(pid, 2).unwrap().unwrap();
    assert_eq!(v2.version, 2);

    let v3 = store.load_version(pid, 3).unwrap();
    assert!(v3.is_none());
  }

  #[test]
  fn test_snapshot_delete_before() {
    let store = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    for v in 1..=5 {
      store.save(&make_snapshot(pid, v)).unwrap();
    }

    let deleted = store.delete_before(pid, 4).unwrap();
    assert_eq!(deleted, 3);

    assert_eq!(store.count(pid).unwrap(), 2);
    assert!(store.load_version(pid, 1).unwrap().is_none());
    assert!(store.load_version(pid, 4).unwrap().is_some());
  }

  #[test]
  fn test_snapshot_not_found() {
    let store = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(999);
    assert!(store.load_latest(pid).unwrap().is_none());
  }

  #[test]
  fn test_snapshot_isolates_pids() {
    let store = SnapshotStore::open_in_memory().unwrap();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);

    store.save(&make_snapshot(pid1, 1)).unwrap();
    store.save(&make_snapshot(pid2, 1)).unwrap();
    store.save(&make_snapshot(pid1, 2)).unwrap();

    assert_eq!(store.count(pid1).unwrap(), 2);
    assert_eq!(store.count(pid2).unwrap(), 1);
  }

  #[test]
  fn test_snapshot_with_pending_effects() {
    let store = SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let snap = Snapshot {
      pid,
      version: 1,
      state_blob: b"state".to_vec(),
      mailbox_cursor: 0,
      pending_effects: Some(
        "[\"eff-1\",\"eff-2\"]".to_string(),
      ),
    };
    store.save(&snap).unwrap();

    let loaded = store.load_latest(pid).unwrap().unwrap();
    assert_eq!(
      loaded.pending_effects.as_deref(),
      Some("[\"eff-1\",\"eff-2\"]")
    );
  }
}
