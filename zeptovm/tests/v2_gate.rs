//! v2 Gate Test: durability — checkpoint recovery and WAL replay.

use std::{
  sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
  },
  time::Duration,
};

use async_trait::async_trait;
use zeptovm::{
  behavior::Behavior,
  durability::{
    checkpoint::{CheckpointStore, SqliteCheckpointStore},
    recovery::{build_recovery_plan, decode_checkpoint, encode_checkpoint},
    wal::{SqliteWalStore, WalEntry},
  },
  error::{Action, Message, Reason, UserPayload},
  pid::Pid,
  process::spawn_process,
};

// ---------------------------------------------------------------------------
// Test 1: checkpoint save and recover
// ---------------------------------------------------------------------------

/// A behavior that maintains a counter. Every message increments the counter.
/// Always checkpoints (should_checkpoint returns true).
/// init() restores counter from checkpoint bytes if present.
struct CounterBehavior {
  counter: u32,
  /// Shared observable so the test can read the counter from outside.
  observable: Arc<AtomicU32>,
}

#[async_trait]
impl Behavior for CounterBehavior {
  async fn init(
    &mut self,
    checkpoint: Option<Vec<u8>>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(data) = checkpoint {
      if data.len() >= 4 {
        self.counter = u32::from_be_bytes(data[..4].try_into().unwrap());
        self.observable.store(self.counter, Ordering::SeqCst);
      }
    }
    Ok(())
  }

  async fn handle(&mut self, _msg: Message) -> Action {
    self.counter += 1;
    self.observable.store(self.counter, Ordering::SeqCst);
    Action::Continue
  }

  async fn terminate(&mut self, _reason: &Reason) {}

  fn should_checkpoint(&self) -> bool {
    true // checkpoint after every message
  }

  fn checkpoint(&self) -> Option<Vec<u8>> {
    Some(self.counter.to_be_bytes().to_vec())
  }
}

#[tokio::test]
async fn gate_v2_checkpoint_save_and_recover() {
  let store = Arc::new(SqliteCheckpointStore::new(":memory:").unwrap());
  let observable = Arc::new(AtomicU32::new(0));

  // --- Phase 1: spawn, send 5 messages, kill ---
  let behavior = CounterBehavior {
    counter: 0,
    observable: observable.clone(),
  };
  let (pid1, handle1, join1) = spawn_process(behavior, 16, None, Some(store.clone()), None);

  for _ in 0..5 {
    handle1.send_user(Message::text("inc")).await.unwrap();
  }
  tokio::time::sleep(Duration::from_millis(100)).await;

  // Observable counter should be 5
  assert_eq!(
    observable.load(Ordering::SeqCst),
    5,
    "counter should be 5 after 5 messages"
  );

  handle1.kill();
  let exit1 = join1.await.unwrap();
  assert!(matches!(exit1.reason, Reason::Kill));

  // --- Verify checkpoint was persisted ---
  let saved = store
    .load(pid1)
    .await
    .unwrap()
    .expect("checkpoint should exist after processing 5 messages");
  let (seq, state_bytes) = decode_checkpoint(&saved).unwrap();
  assert_eq!(seq, 0, "process loop encodes wal_seq as 0");
  let stored_counter = u32::from_be_bytes(state_bytes[..4].try_into().unwrap());
  assert_eq!(stored_counter, 5, "checkpoint should reflect counter = 5");

  // --- Phase 2: recover into a new process ---
  let observable2 = Arc::new(AtomicU32::new(0));
  let behavior2 = CounterBehavior {
    counter: 0,
    observable: observable2.clone(),
  };
  // Pass the raw state bytes (without header) as the checkpoint
  // because spawn_process passes this directly to init().
  let (_pid2, handle2, join2) = spawn_process(
    behavior2,
    16,
    Some(state_bytes.to_vec()),
    Some(store.clone()),
    None,
  );

  // Give init time to run
  tokio::time::sleep(Duration::from_millis(50)).await;
  assert_eq!(
    observable2.load(Ordering::SeqCst),
    5,
    "recovered process should start with counter = 5"
  );

  // --- Phase 3: send 3 more, kill, verify counter = 8 ---
  for _ in 0..3 {
    handle2.send_user(Message::text("inc")).await.unwrap();
  }
  tokio::time::sleep(Duration::from_millis(100)).await;
  assert_eq!(
    observable2.load(Ordering::SeqCst),
    8,
    "counter should be 8 after 3 more messages"
  );

  handle2.kill();
  let exit2 = join2.await.unwrap();
  assert!(matches!(exit2.reason, Reason::Kill));
}

// ---------------------------------------------------------------------------
// Test 2: recovery plan with WAL replay (no process spawning)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gate_v2_recovery_plan_with_wal_replay() {
  let cp_store = SqliteCheckpointStore::new(":memory:").unwrap();
  let wal_store = SqliteWalStore::new(":memory:").unwrap();
  let pid = Pid::from_raw(500);

  // Save a checkpoint at wal_seq=3 with state "counter=3"
  let cp_data = encode_checkpoint(3, b"counter=3");
  cp_store.save(pid, &cp_data).await.unwrap();

  // Add WAL entries 1 through 7
  for i in 1..=7u64 {
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

  // Build recovery plan
  let plan = build_recovery_plan(pid, &cp_store, &wal_store)
    .await
    .unwrap();

  // Verify checkpoint state is present (header stripped)
  assert_eq!(
    plan.checkpoint,
    Some(b"counter=3".to_vec()),
    "plan should contain decoded checkpoint state"
  );

  // Replay entries should be 4, 5, 6, 7 (after wal_seq 3)
  assert_eq!(
    plan.replay_entries.len(),
    4,
    "should replay 4 entries after checkpoint seq 3"
  );
  let replay_seqs: Vec<u64> = plan.replay_entries.iter().map(|e| e.wal_seq).collect();
  assert_eq!(replay_seqs, vec![4, 5, 6, 7]);

  // Verify metrics
  assert!(plan.metrics.checkpoint_loaded);
  assert_eq!(plan.metrics.last_acked_wal_seq, 3);
  assert_eq!(plan.metrics.wal_entries_total, 4);
  assert_eq!(plan.metrics.wal_entries_after_dedup, 4);
}

// ---------------------------------------------------------------------------
// Test 3: full lifecycle — checkpoint, kill, recover
// ---------------------------------------------------------------------------

/// A behavior that tracks messages as a Vec<String>.
/// Checkpoints serialize the Vec as JSON.
/// init() restores from JSON checkpoint.
/// should_checkpoint() returns true every 3 messages.
struct JournalBehavior {
  messages: Vec<String>,
  msg_count: u32,
  /// Shared clone for test observability.
  observable: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Behavior for JournalBehavior {
  async fn init(
    &mut self,
    checkpoint: Option<Vec<u8>>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(data) = checkpoint {
      let restored: Vec<String> = serde_json::from_slice(&data)?;
      self.msg_count = restored.len() as u32;
      self.messages = restored.clone();
      *self.observable.lock().unwrap() = restored;
    }
    Ok(())
  }

  async fn handle(&mut self, msg: Message) -> Action {
    let text = match msg {
      Message::User(UserPayload::Text(t)) => t,
      _ => return Action::Continue,
    };
    self.messages.push(text);
    self.msg_count += 1;
    // Update observable
    *self.observable.lock().unwrap() = self.messages.clone();
    Action::Continue
  }

  async fn terminate(&mut self, _reason: &Reason) {}

  fn should_checkpoint(&self) -> bool {
    self.msg_count > 0 && self.msg_count % 3 == 0
  }

  fn checkpoint(&self) -> Option<Vec<u8>> {
    serde_json::to_vec(&self.messages).ok()
  }
}

#[tokio::test]
async fn gate_v2_full_lifecycle_checkpoint_kill_recover() {
  let store = Arc::new(SqliteCheckpointStore::new(":memory:").unwrap());
  let observable1 = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

  // --- Phase 1: send a, b, c, d, e ---
  let behavior1 = JournalBehavior {
    messages: Vec::new(),
    msg_count: 0,
    observable: observable1.clone(),
  };
  let (pid1, handle1, join1) = spawn_process(behavior1, 16, None, Some(store.clone()), None);

  for label in &["a", "b", "c", "d", "e"] {
    handle1.send_user(Message::text(*label)).await.unwrap();
  }
  tokio::time::sleep(Duration::from_millis(150)).await;

  // Verify all messages processed
  {
    let obs = observable1.lock().unwrap();
    assert_eq!(
      *obs,
      vec!["a", "b", "c", "d", "e"],
      "all 5 messages should be processed"
    );
  }

  handle1.kill();
  let exit1 = join1.await.unwrap();
  assert!(matches!(exit1.reason, Reason::Kill));

  // --- Read checkpoint ---
  let saved = store
    .load(pid1)
    .await
    .unwrap()
    .expect("checkpoint should exist");
  let (_seq, state_bytes) = decode_checkpoint(&saved).unwrap();
  let checkpoint_msgs: Vec<String> = serde_json::from_slice(state_bytes).unwrap();

  // should_checkpoint fires at msg_count 3 (after "c") and possibly
  // again at msg_count 6 if we sent that many. With 5 messages the
  // last checkpoint fires at count=3, but then messages "d" and "e"
  // arrive (count 4 and 5, neither triggers checkpoint).
  // However, with should_checkpoint checked after each Continue, the
  // checkpoint at count=3 captures ["a","b","c"]. No further
  // checkpoint until count=6 which never happens.
  // So checkpoint should be ["a","b","c"].
  assert_eq!(
    checkpoint_msgs,
    vec!["a", "b", "c"],
    "checkpoint should fire after 3rd message"
  );

  // --- Phase 2: recover and continue ---
  let observable2 = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
  let behavior2 = JournalBehavior {
    messages: Vec::new(),
    msg_count: 0,
    observable: observable2.clone(),
  };
  let (_pid2, handle2, join2) = spawn_process(
    behavior2,
    16,
    Some(state_bytes.to_vec()),
    Some(store.clone()),
    None,
  );

  // Give init time to restore state
  tokio::time::sleep(Duration::from_millis(50)).await;
  {
    let obs = observable2.lock().unwrap();
    assert_eq!(
      *obs,
      vec!["a", "b", "c"],
      "recovered process should start with [a, b, c]"
    );
  }

  // Send "f" and "g"
  handle2.send_user(Message::text("f")).await.unwrap();
  handle2.send_user(Message::text("g")).await.unwrap();
  tokio::time::sleep(Duration::from_millis(100)).await;

  {
    let obs = observable2.lock().unwrap();
    assert_eq!(
      *obs,
      vec!["a", "b", "c", "f", "g"],
      "after recovery + 2 new messages"
    );
  }

  handle2.kill();
  let exit2 = join2.await.unwrap();
  assert!(matches!(exit2.reason, Reason::Kill));

  // After "f" and "g", msg_count went from 3 (restored) to 5.
  // No new checkpoint triggered (would need msg_count=6).
  // The checkpoint in the store for pid2 may or may not exist
  // depending on whether a new checkpoint was saved. Let's verify
  // the observable state is correct — that's the contract.
  let obs_final = observable2.lock().unwrap().clone();
  assert_eq!(obs_final, vec!["a", "b", "c", "f", "g"]);
}
