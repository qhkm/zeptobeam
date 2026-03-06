use tracing::info_span;

use crate::{
  core::effect::EffectRequest,
  core::message::Envelope,
  durability::{
    journal::{Journal, JournalEntry, JournalEntryType},
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
    }
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

    // Persist journal entries atomically
    self
      .journal
      .append_batch(&entries)
      .map_err(|e| format!("journal commit failed: {e}"))?;

    // Check if we should take a snapshot
    let counter =
      self.turn_counter.entry(turn.pid.raw()).or_insert(0);
    *counter += 1;

    if *counter % self.snapshot_interval == 0 {
      if let Some(ref state_blob) = turn.state_snapshot {
        let snapshot = Snapshot {
          pid: turn.pid,
          version: *counter as i64,
          state_blob: state_blob.clone(),
          mailbox_cursor: 0,
          pending_effects: None,
        };
        self
          .snapshot_store
          .save(&snapshot)
          .map_err(|e| format!("snapshot save failed: {e}"))?;
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
}
