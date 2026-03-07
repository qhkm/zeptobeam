use rusqlite::{params, Connection, Result as SqlResult};
use serde::{Deserialize, Serialize};

use crate::pid::Pid;

/// Types of journal entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JournalEntryType {
  ProcessSpawned,
  MessageReceived,
  TurnStarted,
  StatePatched,
  EffectRequested,
  EffectResultRecorded,
  MessageSent,
  TimerScheduled,
  TimerFired,
  ProcessSuspended,
  ProcessResumed,
  ProcessFailed,
  ProcessExited,
  TimerCancelled,
  ChildStarted,
  ChildRestarted,
  Linked,
  Unlinked,
  MonitorCreated,
  MonitorRemoved,
  EffectRetried,
  EffectStateChanged,
  CompensationRecorded,
  CompensationStarted,
  CompensationStepCompleted,
  CompensationFinished,
}

impl JournalEntryType {
  pub(crate) fn as_str(&self) -> &'static str {
    match self {
      Self::ProcessSpawned => "process_spawned",
      Self::MessageReceived => "message_received",
      Self::TurnStarted => "turn_started",
      Self::StatePatched => "state_patched",
      Self::EffectRequested => "effect_requested",
      Self::EffectResultRecorded => "effect_result_recorded",
      Self::MessageSent => "message_sent",
      Self::TimerScheduled => "timer_scheduled",
      Self::TimerFired => "timer_fired",
      Self::ProcessSuspended => "process_suspended",
      Self::ProcessResumed => "process_resumed",
      Self::ProcessFailed => "process_failed",
      Self::ProcessExited => "process_exited",
      Self::TimerCancelled => "timer_cancelled",
      Self::ChildStarted => "child_started",
      Self::ChildRestarted => "child_restarted",
      Self::Linked => "linked",
      Self::Unlinked => "unlinked",
      Self::MonitorCreated => "monitor_created",
      Self::MonitorRemoved => "monitor_removed",
      Self::EffectRetried => "effect_retried",
      Self::EffectStateChanged => "effect_state_changed",
      Self::CompensationRecorded => "compensation_recorded",
      Self::CompensationStarted => "compensation_started",
      Self::CompensationStepCompleted => "compensation_step_completed",
      Self::CompensationFinished => "compensation_finished",
    }
  }

  pub(crate) fn from_str(s: &str) -> Option<Self> {
    match s {
      "process_spawned" => Some(Self::ProcessSpawned),
      "message_received" => Some(Self::MessageReceived),
      "turn_started" => Some(Self::TurnStarted),
      "state_patched" => Some(Self::StatePatched),
      "effect_requested" => Some(Self::EffectRequested),
      "effect_result_recorded" => Some(Self::EffectResultRecorded),
      "message_sent" => Some(Self::MessageSent),
      "timer_scheduled" => Some(Self::TimerScheduled),
      "timer_fired" => Some(Self::TimerFired),
      "process_suspended" => Some(Self::ProcessSuspended),
      "process_resumed" => Some(Self::ProcessResumed),
      "process_failed" => Some(Self::ProcessFailed),
      "process_exited" => Some(Self::ProcessExited),
      "timer_cancelled" => Some(Self::TimerCancelled),
      "child_started" => Some(Self::ChildStarted),
      "child_restarted" => Some(Self::ChildRestarted),
      "linked" => Some(Self::Linked),
      "unlinked" => Some(Self::Unlinked),
      "monitor_created" => Some(Self::MonitorCreated),
      "monitor_removed" => Some(Self::MonitorRemoved),
      "effect_retried" => Some(Self::EffectRetried),
      "effect_state_changed" => Some(Self::EffectStateChanged),
      "compensation_recorded" => Some(Self::CompensationRecorded),
      "compensation_started" => Some(Self::CompensationStarted),
      "compensation_step_completed" => Some(Self::CompensationStepCompleted),
      "compensation_finished" => Some(Self::CompensationFinished),
      _ => None,
    }
  }
}

/// A single journal entry.
#[derive(Debug, Clone)]
pub struct JournalEntry {
  pub id: Option<i64>,
  pub pid: Pid,
  pub entry_type: JournalEntryType,
  pub payload: Option<Vec<u8>>,
}

impl JournalEntry {
  pub fn new(
    pid: Pid,
    entry_type: JournalEntryType,
    payload: Option<Vec<u8>>,
  ) -> Self {
    Self {
      id: None,
      pid,
      entry_type,
      payload,
    }
  }
}

/// SQLite-backed append-only journal for durable turn events.
pub struct Journal {
  conn: Connection,
}

impl Journal {
  /// Create a new journal. Uses `:memory:` for testing.
  pub fn open_in_memory() -> SqlResult<Self> {
    let conn = Connection::open_in_memory()?;
    let journal = Self { conn };
    journal.init_schema()?;
    Ok(journal)
  }

  /// Create a journal backed by a file.
  pub fn open(path: &str) -> SqlResult<Self> {
    let conn = Connection::open(path)?;
    let journal = Self { conn };
    journal.init_schema()?;
    Ok(journal)
  }

  /// Initialise the journal schema on an existing connection.
  /// Used for shared-connection atomic commit.
  pub fn from_connection(conn: &Connection) -> SqlResult<()> {
    conn.execute_batch(
      "CREATE TABLE IF NOT EXISTS journal (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          pid INTEGER NOT NULL,
          entry_type TEXT NOT NULL,
          payload BLOB,
          created_at TEXT NOT NULL \
            DEFAULT (datetime('now'))
      );
      CREATE INDEX IF NOT EXISTS idx_journal_pid
        ON journal(pid);",
    )?;
    Ok(())
  }

  fn init_schema(&self) -> SqlResult<()> {
    self.conn.execute_batch(
      "CREATE TABLE IF NOT EXISTS journal (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          pid INTEGER NOT NULL,
          entry_type TEXT NOT NULL,
          payload BLOB,
          created_at TEXT NOT NULL DEFAULT (datetime('now'))
      );
      CREATE INDEX IF NOT EXISTS idx_journal_pid
        ON journal(pid);",
    )?;
    Ok(())
  }

  /// Append a single journal entry.
  pub fn append(&self, entry: &JournalEntry) -> SqlResult<i64> {
    self.conn.execute(
      "INSERT INTO journal (pid, entry_type, payload) \
       VALUES (?1, ?2, ?3)",
      params![
        entry.pid.raw() as i64,
        entry.entry_type.as_str(),
        entry.payload,
      ],
    )?;
    Ok(self.conn.last_insert_rowid())
  }

  /// Append a batch of entries atomically (single transaction).
  pub fn append_batch(
    &self,
    entries: &[JournalEntry],
  ) -> SqlResult<Vec<i64>> {
    let tx = self.conn.unchecked_transaction()?;
    let mut ids = Vec::with_capacity(entries.len());
    for entry in entries {
      tx.execute(
        "INSERT INTO journal (pid, entry_type, payload) \
         VALUES (?1, ?2, ?3)",
        params![
          entry.pid.raw() as i64,
          entry.entry_type.as_str(),
          entry.payload,
        ],
      )?;
      ids.push(tx.last_insert_rowid());
    }
    tx.commit()?;
    Ok(ids)
  }

  /// Replay journal entries for a process since a given offset (id).
  pub fn replay(
    &self,
    pid: Pid,
    since_id: i64,
  ) -> SqlResult<Vec<JournalEntry>> {
    let mut stmt = self.conn.prepare(
      "SELECT id, pid, entry_type, payload FROM journal \
       WHERE pid = ?1 AND id > ?2 ORDER BY id",
    )?;
    let entries = stmt
      .query_map(params![pid.raw() as i64, since_id], |row| {
        let id: i64 = row.get(0)?;
        let pid_raw: i64 = row.get(1)?;
        let entry_type_str: String = row.get(2)?;
        let payload: Option<Vec<u8>> = row.get(3)?;
        Ok(JournalEntry {
          id: Some(id),
          pid: Pid::from_raw(pid_raw as u64),
          entry_type: JournalEntryType::from_str(&entry_type_str)
            .unwrap_or(JournalEntryType::TurnStarted),
          payload,
        })
      })?
      .collect::<SqlResult<Vec<_>>>()?;
    Ok(entries)
  }

  /// Count entries for a process.
  pub fn count(&self, pid: Pid) -> SqlResult<i64> {
    self.conn.query_row(
      "SELECT COUNT(*) FROM journal WHERE pid = ?1",
      params![pid.raw() as i64],
      |row| row.get(0),
    )
  }

  /// Truncate entries before a given id (for compaction after snapshot).
  pub fn truncate_before(
    &self,
    pid: Pid,
    before_id: i64,
  ) -> SqlResult<usize> {
    self.conn.execute(
      "DELETE FROM journal WHERE pid = ?1 AND id < ?2",
      params![pid.raw() as i64, before_id],
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::pid::Pid;

  #[test]
  fn test_journal_append_and_replay() {
    let journal = Journal::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let entry =
      JournalEntry::new(pid, JournalEntryType::ProcessSpawned, None);
    let id = journal.append(&entry).unwrap();
    assert!(id > 0);

    let entries = journal.replay(pid, 0).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(matches!(
      entries[0].entry_type,
      JournalEntryType::ProcessSpawned
    ));
  }

  #[test]
  fn test_journal_append_batch() {
    let journal = Journal::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let entries = vec![
      JournalEntry::new(pid, JournalEntryType::TurnStarted, None),
      JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(b"llm-call".to_vec()),
      ),
      JournalEntry::new(
        pid,
        JournalEntryType::StatePatched,
        Some(b"state-v1".to_vec()),
      ),
    ];

    let ids = journal.append_batch(&entries).unwrap();
    assert_eq!(ids.len(), 3);

    let replayed = journal.replay(pid, 0).unwrap();
    assert_eq!(replayed.len(), 3);
  }

  #[test]
  fn test_journal_replay_since() {
    let journal = Journal::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let e1 =
      JournalEntry::new(pid, JournalEntryType::ProcessSpawned, None);
    let id1 = journal.append(&e1).unwrap();

    let e2 =
      JournalEntry::new(pid, JournalEntryType::TurnStarted, None);
    let _id2 = journal.append(&e2).unwrap();

    let e3 =
      JournalEntry::new(pid, JournalEntryType::MessageReceived, None);
    let _id3 = journal.append(&e3).unwrap();

    // Replay since id1 -- should get entries 2 and 3
    let entries = journal.replay(pid, id1).unwrap();
    assert_eq!(entries.len(), 2);
  }

  #[test]
  fn test_journal_count() {
    let journal = Journal::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    assert_eq!(journal.count(pid).unwrap(), 0);

    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::ProcessSpawned,
        None,
      ))
      .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::TurnStarted,
        None,
      ))
      .unwrap();

    assert_eq!(journal.count(pid).unwrap(), 2);
  }

  #[test]
  fn test_journal_truncate() {
    let journal = Journal::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let ids: Vec<i64> = (0..5)
      .map(|_| {
        journal
          .append(&JournalEntry::new(
            pid,
            JournalEntryType::TurnStarted,
            None,
          ))
          .unwrap()
      })
      .collect();

    // Truncate entries before ids[3]
    let deleted = journal.truncate_before(pid, ids[3]).unwrap();
    assert_eq!(deleted, 3);

    let remaining = journal.replay(pid, 0).unwrap();
    assert_eq!(remaining.len(), 2);
  }

  #[test]
  fn test_journal_isolates_pids() {
    let journal = Journal::open_in_memory().unwrap();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);

    journal
      .append(&JournalEntry::new(
        pid1,
        JournalEntryType::ProcessSpawned,
        None,
      ))
      .unwrap();
    journal
      .append(&JournalEntry::new(
        pid2,
        JournalEntryType::ProcessSpawned,
        None,
      ))
      .unwrap();
    journal
      .append(&JournalEntry::new(
        pid1,
        JournalEntryType::TurnStarted,
        None,
      ))
      .unwrap();

    assert_eq!(journal.count(pid1).unwrap(), 2);
    assert_eq!(journal.count(pid2).unwrap(), 1);

    let entries = journal.replay(pid1, 0).unwrap();
    assert_eq!(entries.len(), 2);
  }

  #[test]
  fn test_journal_payload_roundtrip() {
    let journal = Journal::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);
    let payload = serde_json::to_vec(&serde_json::json!({
      "effect": "llm",
      "tokens": 100
    }))
    .unwrap();

    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(payload.clone()),
      ))
      .unwrap();

    let entries = journal.replay(pid, 0).unwrap();
    assert_eq!(entries[0].payload.as_ref().unwrap(), &payload);
  }

  #[test]
  fn test_journal_entry_type_effect_state_changed() {
    let t = JournalEntryType::EffectStateChanged;
    assert_eq!(t.as_str(), "effect_state_changed");
    assert_eq!(
      JournalEntryType::from_str("effect_state_changed"),
      Some(JournalEntryType::EffectStateChanged)
    );
  }
}
