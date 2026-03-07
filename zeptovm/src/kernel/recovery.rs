use crate::core::behavior::StepBehavior;
use crate::core::timer::TimerSpec;
use crate::durability::journal::{Journal, JournalEntryType};
use crate::durability::snapshot::SnapshotStore;
use crate::durability::timer_store::TimerStore;
use crate::kernel::process_table::{
  ProcessEntry, ProcessRuntimeState,
};
use crate::pid::Pid;

/// Coordinates process recovery from durable storage.
pub struct RecoveryCoordinator<'a> {
  journal: &'a Journal,
  snapshots: &'a SnapshotStore,
  timer_store: Option<&'a TimerStore>,
}

/// Result of recovering a single process.
pub struct RecoveredProcess {
  pub entry: ProcessEntry,
  pub pending_effect_ids: Vec<u64>,
  pub timers: Vec<TimerSpec>,
}

impl<'a> RecoveryCoordinator<'a> {
  pub fn new(
    journal: &'a Journal,
    snapshots: &'a SnapshotStore,
  ) -> Self {
    Self {
      journal,
      snapshots,
      timer_store: None,
    }
  }

  pub fn with_timer_store(
    mut self,
    store: &'a TimerStore,
  ) -> Self {
    self.timer_store = Some(store);
    self
  }

  /// Recover a single process from snapshot + journal replay.
  pub fn recover_process(
    &self,
    pid: Pid,
    factory: &dyn Fn() -> Box<dyn StepBehavior>,
  ) -> Result<RecoveredProcess, String> {
    let mut behavior = factory();
    let mut snapshot_seq = 0i64;

    // Step 1: Load latest snapshot
    let snapshot = self
      .snapshots
      .load_latest(pid)
      .map_err(|e| format!("snapshot load failed: {e}"))?;

    if let Some(ref snap) = snapshot {
      behavior.restore(&snap.state_blob)?;
      snapshot_seq = snap.version;
    } else {
      behavior.init(None);
    }

    // Step 2: Replay journal entries after snapshot
    let entries = self
      .journal
      .replay(pid, snapshot_seq)
      .map_err(|e| format!("journal replay failed: {e}"))?;

    let mut pending_effects = Vec::new();
    let mut completed_effects =
      std::collections::HashSet::new();

    for entry in &entries {
      match &entry.entry_type {
        JournalEntryType::EffectRequested => {
          if let Some(ref payload) = entry.payload {
            if let Ok(req) =
              serde_json::from_slice::<
                crate::core::effect::EffectRequest,
              >(payload)
            {
              pending_effects.push(req.effect_id.raw());
            }
          }
        }
        JournalEntryType::EffectResultRecorded => {
          if let Some(ref payload) = entry.payload {
            if let Ok(id) =
              serde_json::from_slice::<u64>(payload)
            {
              completed_effects.insert(id);
            }
          }
        }
        _ => {}
      }
    }

    // Remove completed effects from pending
    pending_effects
      .retain(|id| !completed_effects.contains(id));

    // Step 3: Load durable timers
    let timers = if let Some(store) = self.timer_store {
      store
        .load_for_pid(pid)
        .map_err(|e| format!("timer load failed: {e}"))?
    } else {
      Vec::new()
    };

    // Step 4: Build ProcessEntry
    let entry = ProcessEntry::new(pid, behavior);

    Ok(RecoveredProcess {
      entry,
      pending_effect_ids: pending_effects,
      timers,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::behavior::StepBehavior;
  use crate::core::effect::{EffectKind, EffectRequest};
  use crate::core::message::Envelope;
  use crate::core::step_result::StepResult;
  use crate::core::turn_context::TurnContext;
  use crate::durability::journal::{
    Journal, JournalEntry, JournalEntryType,
  };
  use crate::durability::snapshot::{Snapshot, SnapshotStore};
  use crate::error::Reason;

  struct Restorable {
    state: String,
  }
  impl StepBehavior for Restorable {
    fn init(
      &mut self,
      _: Option<Vec<u8>>,
    ) -> StepResult {
      self.state = "initialized".into();
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _: Envelope,
      _: &mut TurnContext,
    ) -> StepResult {
      StepResult::Continue
    }
    fn terminate(&mut self, _: &Reason) {}
    fn snapshot(&self) -> Option<Vec<u8>> {
      Some(self.state.as_bytes().to_vec())
    }
    fn restore(
      &mut self,
      state: &[u8],
    ) -> Result<(), String> {
      self.state = String::from_utf8(state.to_vec())
        .map_err(|e| e.to_string())?;
      Ok(())
    }
  }

  #[test]
  fn test_recover_fresh_no_snapshot() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);

    let pid = Pid::from_raw(1);
    let result = coord.recover_process(pid, &|| {
      Box::new(Restorable {
        state: String::new(),
      })
    });
    assert!(result.is_ok());
    let recovered = result.unwrap();
    assert_eq!(
      recovered.entry.state,
      ProcessRuntimeState::Ready
    );
    assert!(recovered.pending_effect_ids.is_empty());
  }

  #[test]
  fn test_recover_from_snapshot() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    snapshots
      .save(&Snapshot {
        pid,
        version: 5,
        state_blob: b"recovered-state".to_vec(),
        mailbox_cursor: 0,
        pending_effects: None,
      })
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord.recover_process(pid, &|| {
      Box::new(Restorable {
        state: String::new(),
      })
    });
    assert!(result.is_ok());
  }

  #[test]
  fn test_recover_with_pending_effects() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({"prompt": "test"}),
    );
    let payload = serde_json::to_vec(&req).unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(payload),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord
      .recover_process(pid, &|| {
        Box::new(Restorable {
          state: String::new(),
        })
      })
      .unwrap();
    assert_eq!(result.pending_effect_ids.len(), 1);
  }

  #[test]
  fn test_recover_completed_effects_filtered() {
    let journal = Journal::open_in_memory().unwrap();
    let snapshots =
      SnapshotStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);

    let req = EffectRequest::new(
      EffectKind::LlmCall,
      serde_json::json!({}),
    );
    let effect_id_raw = req.effect_id.raw();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectRequested,
        Some(serde_json::to_vec(&req).unwrap()),
      ))
      .unwrap();
    journal
      .append(&JournalEntry::new(
        pid,
        JournalEntryType::EffectResultRecorded,
        Some(
          serde_json::to_vec(&effect_id_raw).unwrap(),
        ),
      ))
      .unwrap();

    let coord =
      RecoveryCoordinator::new(&journal, &snapshots);
    let result = coord
      .recover_process(pid, &|| {
        Box::new(Restorable {
          state: String::new(),
        })
      })
      .unwrap();
    assert!(result.pending_effect_ids.is_empty());
  }
}
