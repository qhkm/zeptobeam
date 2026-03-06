use rusqlite::{params, Connection};
use tracing::info_span;

use crate::{
  core::effect::EffectRequest,
  core::message::Envelope,
  durability::{
    journal::{
      Journal, JournalEntry, JournalEntryType,
    },
    snapshot::{Snapshot, SnapshotStore},
  },
  pid::Pid,
};

/// A committed turn's outputs, ready for dispatch.
pub struct TurnCommit {
  pub pid: Pid,
  pub turn_id: u64,
  pub journal_entries: Vec<JournalEntry>,
  pub outbound_messages: Vec<Envelope>,
  pub effect_requests: Vec<(Pid, EffectRequest)>,
  pub state_snapshot: Option<Vec<u8>>,
}

/// The turn executor wraps scheduler operations with durable commit
/// semantics.
///
/// Flow:
/// 1. Scheduler steps a process (produces StepResult + TurnContext
///    with intents)
/// 2. Turn executor collects intents -> builds TurnCommit
/// 3. Persists journal entries + snapshot atomically
/// 4. Only after commit: dispatches effects, delivers messages
pub struct TurnExecutor {
  journal: Journal,
  snapshot_store: SnapshotStore,
  snapshot_interval: u64,
  turn_counter: std::collections::HashMap<u64, u64>,
  /// When `Some`, journal + snapshot writes go through
  /// a single shared connection for atomic commit.
  shared_conn: Option<Connection>,
}

impl TurnExecutor {
  pub fn new(
    journal: Journal,
    snapshot_store: SnapshotStore,
  ) -> Self {
    Self {
      journal,
      snapshot_store,
      snapshot_interval: 10,
      turn_counter: std::collections::HashMap::new(),
      shared_conn: None,
    }
  }

  /// Create a TurnExecutor backed by a single in-memory
  /// SQLite connection.  Both journal and snapshot tables
  /// live on `shared_conn`, so `commit()` writes them in
  /// one atomic transaction.
  pub fn open_in_memory() -> Result<Self, String> {
    let conn = Connection::open_in_memory()
      .map_err(|e| format!("open_in_memory: {e}"))?;
    Journal::from_connection(&conn)
      .map_err(|e| format!("journal schema: {e}"))?;
    SnapshotStore::init_on_connection(&conn)
      .map_err(|e| format!("snapshot schema: {e}"))?;

    // Dummy stores for backward-compat accessor methods
    let journal = Journal::open_in_memory()
      .map_err(|e| format!("dummy journal: {e}"))?;
    let snapshot_store = SnapshotStore::open_in_memory()
      .map_err(|e| format!("dummy snapshot: {e}"))?;

    Ok(Self {
      journal,
      snapshot_store,
      snapshot_interval: 10,
      turn_counter: std::collections::HashMap::new(),
      shared_conn: Some(conn),
    })
  }

  pub fn with_snapshot_interval(mut self, interval: u64) -> Self {
    self.snapshot_interval = interval;
    self
  }

  /// Commit a turn's results durably.
  ///
  /// Returns Ok(()) if the commit succeeded, Err if it failed
  /// (turn should be retried or aborted).
  pub fn commit(
    &mut self,
    turn: &TurnCommit,
  ) -> Result<(), String> {
    let _span = info_span!(
      "turn_commit",
      pid = turn.pid.raw(),
      turn_id = turn.turn_id,
      intents = turn.journal_entries.len(),
    )
    .entered();

    // Build journal entries
    let mut entries = Vec::new();

    // Record turn start
    entries.push(JournalEntry::new(
      turn.pid,
      JournalEntryType::TurnStarted,
      None,
    ));

    // Record any effect requests
    for (_pid, req) in &turn.effect_requests {
      let payload = serde_json::to_vec(req).ok();
      entries.push(JournalEntry::new(
        turn.pid,
        JournalEntryType::EffectRequested,
        payload,
      ));
    }

    // Record outbound messages
    for _msg in &turn.outbound_messages {
      entries.push(JournalEntry::new(
        turn.pid,
        JournalEntryType::MessageSent,
        None,
      ));
    }

    // Append user-provided journal entries
    entries.extend(turn.journal_entries.clone());

    // Bump turn counter
    let counter = self
      .turn_counter
      .entry(turn.pid.raw())
      .or_insert(0);
    *counter += 1;
    let counter_val = *counter;

    if let Some(ref conn) = self.shared_conn {
      // ---- Atomic path: single transaction ----
      Self::commit_atomic(
        conn,
        turn,
        &entries,
        counter_val,
        self.snapshot_interval,
      )
    } else {
      // ---- Legacy path: separate stores ----
      Self::commit_legacy(
        &self.journal,
        &self.snapshot_store,
        turn,
        &entries,
        counter_val,
        self.snapshot_interval,
      )
    }
  }

  /// Atomic commit: journal + snapshot in one transaction.
  fn commit_atomic(
    conn: &Connection,
    turn: &TurnCommit,
    entries: &[JournalEntry],
    counter: u64,
    snapshot_interval: u64,
  ) -> Result<(), String> {
    let tx = conn
      .unchecked_transaction()
      .map_err(|e| format!("begin tx: {e}"))?;

    let mut first_id: Option<i64> = None;
    for entry in entries {
      tx.execute(
        "INSERT INTO journal \
         (pid, entry_type, payload) \
         VALUES (?1, ?2, ?3)",
        params![
          entry.pid.raw() as i64,
          entry.entry_type.as_str(),
          entry.payload,
        ],
      )
      .map_err(|e| {
        format!("journal insert: {e}")
      })?;
      if first_id.is_none() {
        first_id = Some(tx.last_insert_rowid());
      }
    }

    // Conditionally write snapshot
    if counter % snapshot_interval == 0 {
      if let Some(ref state_blob) = turn.state_snapshot {
        tx.execute(
          "INSERT OR REPLACE INTO snapshots \
           (pid, version, state_blob, \
            mailbox_cursor, pending_effects) \
           VALUES (?1, ?2, ?3, ?4, ?5)",
          params![
            turn.pid.raw() as i64,
            counter as i64,
            state_blob,
            0i64,
            Option::<String>::None,
          ],
        )
        .map_err(|e| {
          format!("snapshot insert: {e}")
        })?;

        // Compact journal
        if let Some(fid) = first_id {
          tx.execute(
            "DELETE FROM journal \
             WHERE pid = ?1 AND id < ?2",
            params![turn.pid.raw() as i64, fid],
          )
          .map_err(|e| {
            format!("journal compact: {e}")
          })?;
        }
      }
    }

    tx.commit()
      .map_err(|e| format!("commit tx: {e}"))?;
    Ok(())
  }

  /// Legacy commit: separate journal + snapshot stores.
  fn commit_legacy(
    journal: &Journal,
    snapshot_store: &SnapshotStore,
    turn: &TurnCommit,
    entries: &[JournalEntry],
    counter: u64,
    snapshot_interval: u64,
  ) -> Result<(), String> {
    let batch_ids = journal
      .append_batch(entries)
      .map_err(|e| format!("journal commit failed: {e}"))?;

    if counter % snapshot_interval == 0 {
      if let Some(ref state_blob) = turn.state_snapshot {
        let snapshot = Snapshot {
          pid: turn.pid,
          version: counter as i64,
          state_blob: state_blob.clone(),
          mailbox_cursor: 0,
          pending_effects: None,
        };
        snapshot_store.save(&snapshot).map_err(|e| {
          format!("snapshot save failed: {e}")
        })?;

        // Compact journal
        if let Some(&first_id) = batch_ids.first() {
          if let Err(e) =
            journal.truncate_before(turn.pid, first_id)
          {
            tracing::warn!(
              pid = turn.pid.raw(),
              error = %e,
              "journal compaction failed (non-fatal)"
            );
          }
        }
      }
    }

    Ok(())
  }

  /// Force a snapshot for a specific process.
  pub fn force_snapshot(
    &mut self,
    pid: Pid,
    state_blob: Vec<u8>,
    mailbox_cursor: i64,
    pending_effects: Option<String>,
  ) -> Result<(), String> {
    let counter =
      self.turn_counter.entry(pid.raw()).or_insert(0);
    let version = *counter as i64 + 1;
    *counter = version as u64;

    let snapshot = Snapshot {
      pid,
      version,
      state_blob,
      mailbox_cursor,
      pending_effects,
    };
    self
      .snapshot_store
      .save(&snapshot)
      .map_err(|e| format!("snapshot save failed: {e}"))?;
    Ok(())
  }

  /// Get the journal (for replay/inspection).
  pub fn journal(&self) -> &Journal {
    &self.journal
  }

  /// Get the snapshot store (for recovery).
  pub fn snapshot_store(&self) -> &SnapshotStore {
    &self.snapshot_store
  }

  /// Replay journal entries.
  ///
  /// When `shared_conn` is `Some`, reads directly from the
  /// shared connection; otherwise delegates to
  /// `self.journal.replay()`.
  pub fn replay_journal(
    &self,
    pid: Pid,
    since_id: i64,
  ) -> Result<Vec<JournalEntry>, String> {
    if let Some(ref conn) = self.shared_conn {
      let mut stmt = conn
        .prepare(
          "SELECT id, pid, entry_type, payload \
           FROM journal \
           WHERE pid = ?1 AND id > ?2 \
           ORDER BY id",
        )
        .map_err(|e| format!("{e}"))?;
      let entries = stmt
        .query_map(
          params![pid.raw() as i64, since_id],
          |row| {
            let id: i64 = row.get(0)?;
            let pid_raw: i64 = row.get(1)?;
            let et: String = row.get(2)?;
            let payload: Option<Vec<u8>> =
              row.get(3)?;
            Ok(JournalEntry {
              id: Some(id),
              pid: Pid::from_raw(pid_raw as u64),
              entry_type:
                JournalEntryType::from_str(&et)
                  .unwrap_or(
                    JournalEntryType::TurnStarted,
                  ),
              payload,
            })
          },
        )
        .map_err(|e| format!("{e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e}"))?;
      Ok(entries)
    } else {
      self
        .journal
        .replay(pid, since_id)
        .map_err(|e| format!("{e}"))
    }
  }

  /// Load the latest snapshot for a process.
  ///
  /// When `shared_conn` is `Some`, reads directly from the
  /// shared connection; otherwise delegates to
  /// `self.snapshot_store.load_latest()`.
  pub fn load_latest_snapshot(
    &self,
    pid: Pid,
  ) -> Result<Option<Snapshot>, String> {
    if let Some(ref conn) = self.shared_conn {
      let mut stmt = conn
        .prepare(
          "SELECT pid, version, state_blob, \
           mailbox_cursor, pending_effects \
           FROM snapshots \
           WHERE pid = ?1 \
           ORDER BY version DESC LIMIT 1",
        )
        .map_err(|e| format!("{e}"))?;
      let mut rows = stmt
        .query_map(
          params![pid.raw() as i64],
          |row| {
            let pid_raw: i64 = row.get(0)?;
            let version: i64 = row.get(1)?;
            let state_blob: Vec<u8> = row.get(2)?;
            let mailbox_cursor: i64 = row.get(3)?;
            let pending_effects: Option<String> =
              row.get(4)?;
            Ok(Snapshot {
              pid: Pid::from_raw(pid_raw as u64),
              version,
              state_blob,
              mailbox_cursor,
              pending_effects,
            })
          },
        )
        .map_err(|e| format!("{e}"))?;
      match rows.next() {
        Some(Ok(snap)) => Ok(Some(snap)),
        Some(Err(e)) => Err(format!("{e}")),
        None => Ok(None),
      }
    } else {
      self
        .snapshot_store
        .load_latest(pid)
        .map_err(|e| format!("{e}"))
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::effect::{EffectKind, EffectRequest};
  use crate::core::message::Envelope;
  use crate::durability::journal::Journal;
  use crate::durability::snapshot::SnapshotStore;
  use crate::pid::Pid;

  fn make_executor() -> TurnExecutor {
    let journal = Journal::open_in_memory().unwrap();
    let snapshot_store =
      SnapshotStore::open_in_memory().unwrap();
    TurnExecutor::new(journal, snapshot_store)
  }

  #[test]
  fn test_turn_commit_basic() {
    let mut executor = make_executor();
    let pid = Pid::from_raw(1);

    let turn = TurnCommit {
      pid,
      turn_id: 1,
      journal_entries: vec![],
      outbound_messages: vec![],
      effect_requests: vec![],
      state_snapshot: None,
    };

    executor.commit(&turn).unwrap();

    // Should have at least the TurnStarted entry
    let entries =
      executor.journal().replay(pid, 0).unwrap();
    assert!(entries.len() >= 1);
  }

  #[test]
  fn test_turn_commit_with_effects() {
    let mut executor = make_executor();
    let pid = Pid::from_raw(1);

    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({"prompt": "hi"}),
    );

    let turn = TurnCommit {
      pid,
      turn_id: 1,
      journal_entries: vec![],
      outbound_messages: vec![],
      effect_requests: vec![(pid, req)],
      state_snapshot: None,
    };

    executor.commit(&turn).unwrap();

    let entries =
      executor.journal().replay(pid, 0).unwrap();
    // TurnStarted + EffectRequested = 2
    assert_eq!(entries.len(), 2);
  }

  #[test]
  fn test_turn_commit_with_messages() {
    let mut executor = make_executor();
    let pid = Pid::from_raw(1);
    let target = Pid::from_raw(2);

    let turn = TurnCommit {
      pid,
      turn_id: 1,
      journal_entries: vec![],
      outbound_messages: vec![Envelope::text(target, "hello")],
      effect_requests: vec![],
      state_snapshot: None,
    };

    executor.commit(&turn).unwrap();

    let entries =
      executor.journal().replay(pid, 0).unwrap();
    // TurnStarted + MessageSent = 2
    assert_eq!(entries.len(), 2);
  }

  #[test]
  fn test_turn_commit_snapshot_interval() {
    let mut executor =
      make_executor().with_snapshot_interval(3);
    let pid = Pid::from_raw(1);

    // Commit 3 turns with state
    for i in 1..=3 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    // After 3 turns, should have a snapshot
    let snap = executor
      .snapshot_store()
      .load_latest(pid)
      .unwrap();
    assert!(snap.is_some());
    assert_eq!(snap.unwrap().state_blob, b"state-v3");
  }

  #[test]
  fn test_turn_commit_no_snapshot_before_interval() {
    let mut executor =
      make_executor().with_snapshot_interval(5);
    let pid = Pid::from_raw(1);

    // Commit 3 turns
    for i in 1..=3 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    // Should NOT have a snapshot yet
    let snap = executor
      .snapshot_store()
      .load_latest(pid)
      .unwrap();
    assert!(snap.is_none());
  }

  #[test]
  fn test_force_snapshot() {
    let mut executor = make_executor();
    let pid = Pid::from_raw(1);

    executor
      .force_snapshot(
        pid,
        b"forced-state".to_vec(),
        42,
        Some("[\"eff-1\"]".into()),
      )
      .unwrap();

    let snap = executor
      .snapshot_store()
      .load_latest(pid)
      .unwrap()
      .unwrap();
    assert_eq!(snap.state_blob, b"forced-state");
    assert_eq!(snap.mailbox_cursor, 42);
    assert_eq!(
      snap.pending_effects.as_deref(),
      Some("[\"eff-1\"]")
    );
  }

  #[test]
  fn test_multiple_turns_journal_grows() {
    let mut executor = make_executor();
    let pid = Pid::from_raw(1);

    for i in 1..=5 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: None,
      };
      executor.commit(&turn).unwrap();
    }

    // 5 turns x 1 TurnStarted entry each = 5
    let entries =
      executor.journal().replay(pid, 0).unwrap();
    assert_eq!(entries.len(), 5);
  }

  #[test]
  fn test_journal_compaction_after_snapshot() {
    let mut executor =
      make_executor().with_snapshot_interval(3);
    let pid = Pid::from_raw(1);

    // Commit 6 turns with state (snapshots at turns 3 and 6)
    for i in 1..=6 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    // After 6 turns with compaction, journal should be
    // shorter than 6 TurnStarted entries (some were
    // truncated at snapshot boundaries).
    let entries =
      executor.journal().replay(pid, 0).unwrap();
    assert!(
      entries.len() < 6,
      "journal should have been compacted, got {} entries",
      entries.len()
    );

    // But we should still have a valid latest snapshot
    let snap = executor
      .snapshot_store()
      .load_latest(pid)
      .unwrap();
    assert!(snap.is_some());
    assert_eq!(snap.unwrap().state_blob, b"state-v6");
  }

  #[test]
  fn test_journal_compaction_preserves_current_batch() {
    let mut executor =
      make_executor().with_snapshot_interval(2);
    let pid = Pid::from_raw(1);

    // Commit 2 turns — snapshot + compaction at turn 2
    for i in 1..=2 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    // After compaction at turn 2, the entry from turn 1
    // should be gone, but the entry from turn 2 should
    // remain (it was written in the same batch as the
    // snapshot).
    let entries =
      executor.journal().replay(pid, 0).unwrap();
    assert_eq!(
      entries.len(),
      1,
      "only the current batch entry should remain"
    );
  }

  #[test]
  fn test_journal_compaction_isolates_pids() {
    let mut executor =
      make_executor().with_snapshot_interval(2);
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);

    // Commit 3 turns for pid1 (snapshot at turn 2)
    for i in 1..=3 {
      let turn = TurnCommit {
        pid: pid1,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    // Commit 1 turn for pid2 (no snapshot yet)
    let turn = TurnCommit {
      pid: pid2,
      turn_id: 1,
      journal_entries: vec![],
      outbound_messages: vec![],
      effect_requests: vec![],
      state_snapshot: Some(b"pid2-state".to_vec()),
    };
    executor.commit(&turn).unwrap();

    // pid2's journal should be untouched (1 entry)
    let entries2 =
      executor.journal().replay(pid2, 0).unwrap();
    assert_eq!(
      entries2.len(),
      1,
      "pid2 journal should be untouched"
    );
  }

  // ------ Atomic commit tests ------

  #[test]
  fn test_atomic_commit_basic() {
    let mut executor =
      TurnExecutor::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let turn = TurnCommit {
      pid,
      turn_id: 1,
      journal_entries: vec![],
      outbound_messages: vec![],
      effect_requests: vec![],
      state_snapshot: None,
    };
    executor.commit(&turn).unwrap();

    let entries =
      executor.replay_journal(pid, 0).unwrap();
    assert!(entries.len() >= 1);
  }

  #[test]
  fn test_atomic_commit_both_journal_and_snapshot() {
    let executor =
      TurnExecutor::open_in_memory().unwrap();
    let mut executor =
      executor.with_snapshot_interval(1);
    let pid = Pid::from_raw(1);

    let turn = TurnCommit {
      pid,
      turn_id: 1,
      journal_entries: vec![],
      outbound_messages: vec![],
      effect_requests: vec![],
      state_snapshot: Some(
        b"atomic-state".to_vec(),
      ),
    };
    executor.commit(&turn).unwrap();

    // Journal entry present via shared conn
    let entries =
      executor.replay_journal(pid, 0).unwrap();
    assert!(
      entries.len() >= 1,
      "expected journal entries, got {}",
      entries.len()
    );

    // Snapshot present via shared conn
    let snap =
      executor.load_latest_snapshot(pid).unwrap();
    assert!(snap.is_some());
    assert_eq!(
      snap.unwrap().state_blob,
      b"atomic-state"
    );
  }

  #[test]
  fn test_atomic_commit_snapshot_interval() {
    let executor =
      TurnExecutor::open_in_memory().unwrap();
    let mut executor =
      executor.with_snapshot_interval(3);
    let pid = Pid::from_raw(1);

    for i in 1..=3 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    let snap =
      executor.load_latest_snapshot(pid).unwrap();
    assert!(snap.is_some());
    assert_eq!(
      snap.unwrap().state_blob,
      b"state-v3"
    );
  }

  #[test]
  fn test_atomic_commit_no_snapshot_before_interval() {
    let executor =
      TurnExecutor::open_in_memory().unwrap();
    let mut executor =
      executor.with_snapshot_interval(5);
    let pid = Pid::from_raw(1);

    for i in 1..=3 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    let snap =
      executor.load_latest_snapshot(pid).unwrap();
    assert!(snap.is_none());
  }

  #[test]
  fn test_atomic_compaction_after_snapshot() {
    let executor =
      TurnExecutor::open_in_memory().unwrap();
    let mut executor =
      executor.with_snapshot_interval(2);
    let pid = Pid::from_raw(1);

    for i in 1..=2 {
      let turn = TurnCommit {
        pid,
        turn_id: i,
        journal_entries: vec![],
        outbound_messages: vec![],
        effect_requests: vec![],
        state_snapshot: Some(
          format!("state-v{i}").into_bytes(),
        ),
      };
      executor.commit(&turn).unwrap();
    }

    // After compaction at turn 2, only the current
    // batch entry should remain.
    let entries =
      executor.replay_journal(pid, 0).unwrap();
    assert_eq!(
      entries.len(),
      1,
      "expected 1 entry after compaction, got {}",
      entries.len()
    );

    let snap =
      executor.load_latest_snapshot(pid).unwrap();
    assert!(snap.is_some());
    assert_eq!(
      snap.unwrap().state_blob,
      b"state-v2"
    );
  }

  #[test]
  fn test_atomic_commit_with_effects_and_messages() {
    let mut executor =
      TurnExecutor::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);
    let target = Pid::from_raw(2);

    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({"prompt": "hi"}),
    );

    let turn = TurnCommit {
      pid,
      turn_id: 1,
      journal_entries: vec![],
      outbound_messages: vec![
        Envelope::text(target, "hello"),
      ],
      effect_requests: vec![(pid, req)],
      state_snapshot: None,
    };
    executor.commit(&turn).unwrap();

    // TurnStarted + EffectRequested + MessageSent = 3
    let entries =
      executor.replay_journal(pid, 0).unwrap();
    assert_eq!(entries.len(), 3);
  }
}
