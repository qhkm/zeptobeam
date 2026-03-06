use crate::{
  durability::{
    checkpoint::CheckpointStore,
    wal::{SqliteWalStore, WalEntry},
  },
  pid::Pid,
};
use std::collections::HashSet;
use tracing::warn;

/// Represents the recovery plan for a single process.
#[derive(Debug)]
pub struct RecoveryPlan {
  pub pid: Pid,
  /// Checkpoint data to pass to init(), if any (state only, header stripped).
  pub checkpoint: Option<Vec<u8>>,
  /// WAL entries to replay after init(), in wal_seq order, deduped by message_id.
  pub replay_entries: Vec<WalEntry>,
  /// Metrics about the recovery.
  pub metrics: RecoveryMetrics,
}

#[derive(Debug, Default)]
pub struct RecoveryMetrics {
  pub checkpoint_loaded: bool,
  pub wal_entries_total: usize,
  pub wal_entries_after_dedup: usize,
  pub last_acked_wal_seq: u64,
}

/// Encode a checkpoint with the WAL sequence metadata prepended.
///
/// Format:
/// - Bytes 0..8: `last_acked_wal_seq` as big-endian u64
/// - Bytes 8..: actual process state data
pub fn encode_checkpoint(last_acked_wal_seq: u64, state: &[u8]) -> Vec<u8> {
  let mut data = Vec::with_capacity(8 + state.len());
  data.extend_from_slice(&last_acked_wal_seq.to_be_bytes());
  data.extend_from_slice(state);
  data
}

/// Decode a checkpoint: extract (last_acked_wal_seq, state_data).
///
/// Returns `None` if the data is too short to contain the 8-byte header.
pub fn decode_checkpoint(data: &[u8]) -> Option<(u64, &[u8])> {
  if data.len() < 8 {
    return None;
  }
  let seq = u64::from_be_bytes(data[0..8].try_into().unwrap());
  Some((seq, &data[8..]))
}

/// Build a recovery plan for a single process.
///
/// Steps:
/// 1. Load checkpoint from store (if exists)
/// 2. Extract last_acked_wal_seq from checkpoint metadata
///    (first 8 bytes are the last_acked_wal_seq as big-endian u64,
///     remaining bytes are the actual state data)
/// 3. Replay WAL entries where wal_seq > last_acked_wal_seq AND recipient = pid
/// 4. Deduplicate by message_id (keep first occurrence by wal_seq order)
/// 5. Return RecoveryPlan with metrics
pub async fn build_recovery_plan(
  pid: Pid,
  checkpoint_store: &dyn CheckpointStore,
  wal_store: &SqliteWalStore,
) -> Result<RecoveryPlan, Box<dyn std::error::Error + Send + Sync>> {
  let mut metrics = RecoveryMetrics::default();

  // Step 1: Load checkpoint (if exists)
  let raw_checkpoint = checkpoint_store.load(pid).await?;

  // Step 2: Extract last_acked_wal_seq and state data from checkpoint
  let (last_acked_wal_seq, checkpoint_state) = match &raw_checkpoint {
    Some(data) => match decode_checkpoint(data) {
      Some((seq, state)) => {
        metrics.checkpoint_loaded = true;
        metrics.last_acked_wal_seq = seq;
        (seq, Some(state.to_vec()))
      }
      None => {
        warn!(pid = %pid, "checkpoint data corrupt or too short — discarding, replaying from WAL seq 0");
        (0, None)
      }
    },
    None => (0, None),
  };

  // Step 3: Replay WAL entries after last_acked_wal_seq for this pid
  let wal_entries = wal_store.replay_from(last_acked_wal_seq, pid).await?;
  metrics.wal_entries_total = wal_entries.len();

  // Step 4: Deduplicate by message_id (keep first occurrence by wal_seq order)
  let mut seen_ids = HashSet::new();
  let deduped_entries: Vec<WalEntry> = wal_entries
    .into_iter()
    .filter(|entry| seen_ids.insert(entry.message_id))
    .collect();
  metrics.wal_entries_after_dedup = deduped_entries.len();

  // Step 5: Return RecoveryPlan
  Ok(RecoveryPlan {
    pid,
    checkpoint: checkpoint_state,
    replay_entries: deduped_entries,
    metrics,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    durability::{
      checkpoint::SqliteCheckpointStore,
      wal::{SqliteWalStore, WalEntry},
    },
    pid::Pid,
  };

  #[tokio::test]
  async fn test_recovery_no_checkpoint_no_wal() {
    let cp_store = SqliteCheckpointStore::new(":memory:").unwrap();
    let wal_store = SqliteWalStore::new(":memory:").unwrap();
    let pid = Pid::from_raw(1);

    let plan = build_recovery_plan(pid, &cp_store, &wal_store)
      .await
      .unwrap();
    assert!(plan.checkpoint.is_none());
    assert!(plan.replay_entries.is_empty());
    assert!(!plan.metrics.checkpoint_loaded);
    assert_eq!(plan.metrics.wal_entries_total, 0);
  }

  #[tokio::test]
  async fn test_recovery_checkpoint_only() {
    let cp_store = SqliteCheckpointStore::new(":memory:").unwrap();
    let wal_store = SqliteWalStore::new(":memory:").unwrap();
    let pid = Pid::from_raw(1);

    // Save checkpoint with wal_seq=5, state="state-data"
    let cp_data = encode_checkpoint(5, b"state-data");
    cp_store.save(pid, &cp_data).await.unwrap();

    let plan = build_recovery_plan(pid, &cp_store, &wal_store)
      .await
      .unwrap();
    assert_eq!(plan.checkpoint, Some(b"state-data".to_vec()));
    assert!(plan.replay_entries.is_empty());
    assert!(plan.metrics.checkpoint_loaded);
    assert_eq!(plan.metrics.last_acked_wal_seq, 5);
  }

  #[tokio::test]
  async fn test_recovery_wal_only() {
    let cp_store = SqliteCheckpointStore::new(":memory:").unwrap();
    let wal_store = SqliteWalStore::new(":memory:").unwrap();
    let pid = Pid::from_raw(1);

    // Add 3 WAL entries for this pid
    for i in 1..=3 {
      wal_store
        .append(WalEntry {
          wal_seq: i,
          message_id: i as u128,
          sender: Pid::from_raw(99),
          sender_seq: i - 1,
          recipient: pid,
          payload: format!("msg-{i}").into_bytes(),
          timestamp: 1000 + i,
        })
        .await
        .unwrap();
    }

    let plan = build_recovery_plan(pid, &cp_store, &wal_store)
      .await
      .unwrap();
    assert!(plan.checkpoint.is_none());
    assert_eq!(plan.replay_entries.len(), 3);
    assert_eq!(plan.replay_entries[0].wal_seq, 1);
    assert_eq!(plan.replay_entries[2].wal_seq, 3);
  }

  #[tokio::test]
  async fn test_recovery_checkpoint_plus_wal_replay() {
    let cp_store = SqliteCheckpointStore::new(":memory:").unwrap();
    let wal_store = SqliteWalStore::new(":memory:").unwrap();
    let pid = Pid::from_raw(1);

    // Checkpoint at wal_seq=3
    let cp_data = encode_checkpoint(3, b"checkpoint-state");
    cp_store.save(pid, &cp_data).await.unwrap();

    // WAL has entries 1-5
    for i in 1..=5 {
      wal_store
        .append(WalEntry {
          wal_seq: i,
          message_id: i as u128,
          sender: Pid::from_raw(99),
          sender_seq: i - 1,
          recipient: pid,
          payload: format!("msg-{i}").into_bytes(),
          timestamp: 1000 + i,
        })
        .await
        .unwrap();
    }

    let plan = build_recovery_plan(pid, &cp_store, &wal_store)
      .await
      .unwrap();
    assert_eq!(plan.checkpoint, Some(b"checkpoint-state".to_vec()));
    // Should only replay entries 4, 5 (after wal_seq 3)
    assert_eq!(plan.replay_entries.len(), 2);
    assert_eq!(plan.replay_entries[0].wal_seq, 4);
    assert_eq!(plan.replay_entries[1].wal_seq, 5);
    assert_eq!(plan.metrics.last_acked_wal_seq, 3);
  }

  #[tokio::test]
  async fn test_recovery_deduplicates_by_message_id() {
    let cp_store = SqliteCheckpointStore::new(":memory:").unwrap();
    let wal_store = SqliteWalStore::new(":memory:").unwrap();
    let pid = Pid::from_raw(1);

    // Two WAL entries with the SAME message_id (duplicate delivery)
    wal_store
      .append(WalEntry {
        wal_seq: 1,
        message_id: 42,
        sender: Pid::from_raw(99),
        sender_seq: 0,
        recipient: pid,
        payload: b"original".to_vec(),
        timestamp: 1000,
      })
      .await
      .unwrap();
    wal_store
      .append(WalEntry {
        wal_seq: 2,
        message_id: 42, // same message_id!
        sender: Pid::from_raw(99),
        sender_seq: 0,
        recipient: pid,
        payload: b"duplicate".to_vec(),
        timestamp: 1001,
      })
      .await
      .unwrap();
    wal_store
      .append(WalEntry {
        wal_seq: 3,
        message_id: 100,
        sender: Pid::from_raw(99),
        sender_seq: 1,
        recipient: pid,
        payload: b"unique".to_vec(),
        timestamp: 1002,
      })
      .await
      .unwrap();

    let plan = build_recovery_plan(pid, &cp_store, &wal_store)
      .await
      .unwrap();
    // Should have 2 entries (42 deduped), keeping the first occurrence
    assert_eq!(plan.replay_entries.len(), 2);
    assert_eq!(plan.replay_entries[0].message_id, 42);
    assert_eq!(plan.replay_entries[0].payload, b"original");
    assert_eq!(plan.replay_entries[1].message_id, 100);
    assert_eq!(plan.metrics.wal_entries_total, 3);
    assert_eq!(plan.metrics.wal_entries_after_dedup, 2);
  }

  #[test]
  fn test_encode_decode_checkpoint() {
    let encoded = encode_checkpoint(42, b"my-state");
    let (seq, state) = decode_checkpoint(&encoded).unwrap();
    assert_eq!(seq, 42);
    assert_eq!(state, b"my-state");
  }

  #[test]
  fn test_decode_checkpoint_too_short() {
    assert!(decode_checkpoint(&[1, 2, 3]).is_none());
  }

  #[test]
  fn test_decode_empty_state() {
    let encoded = encode_checkpoint(0, b"");
    let (seq, state) = decode_checkpoint(&encoded).unwrap();
    assert_eq!(seq, 0);
    assert!(state.is_empty());
  }
}
