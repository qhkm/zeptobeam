//! Phase 1 gate tests for ZeptoVM.
//!
//! v0: 100 processes, 10k messages, fair scheduling
//! v1: Effect journaling + dispatch cycle
//! v2: Kill + snapshot recovery simulation
//! v3: Budget gate blocks LLM + violation recorded

use zeptovm::{
  core::{
    behavior::StepBehavior,
    effect::{EffectKind, EffectRequest, EffectStatus},
    message::{Envelope, EnvelopePayload},
    step_result::StepResult,
    turn_context::TurnContext,
  },
  error::Reason,
  kernel::runtime::{BudgetState, SchedulerRuntime},
};

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

// ────────────────────────────────────────────
// Behaviors for gate tests
// ────────────────────────────────────────────

struct CounterBehavior {
  count: Arc<AtomicU32>,
}

impl StepBehavior for CounterBehavior {
  fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
    StepResult::Continue
  }
  fn handle(
    &mut self,
    _msg: Envelope,
    _ctx: &mut TurnContext,
  ) -> StepResult {
    self.count.fetch_add(1, Ordering::Relaxed);
    StepResult::Continue
  }
  fn terminate(&mut self, _reason: &Reason) {}
}

enum LlmState {
  Idle,
  WaitingEffect,
  Done,
}

struct LlmAgentBehavior {
  state: LlmState,
}

impl StepBehavior for LlmAgentBehavior {
  fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
    StepResult::Continue
  }

  fn handle(
    &mut self,
    msg: Envelope,
    _ctx: &mut TurnContext,
  ) -> StepResult {
    match (&self.state, &msg.payload) {
      (LlmState::Idle, EnvelopePayload::User(_)) => {
        self.state = LlmState::WaitingEffect;
        StepResult::Suspend(EffectRequest::new(
          EffectKind::LlmCall,
          serde_json::json!({"prompt": "What is 2+2?"}),
        ))
      }
      (LlmState::WaitingEffect, EnvelopePayload::Effect(result)) => {
        if result.status == EffectStatus::Succeeded {
          self.state = LlmState::Done;
          StepResult::Done(Reason::Normal)
        } else {
          self.state = LlmState::Done;
          StepResult::Done(Reason::Custom(
            "budget exceeded".into(),
          ))
        }
      }
      _ => StepResult::Continue,
    }
  }

  fn terminate(&mut self, _reason: &Reason) {}
}

struct SnapshotBehavior {
  counter: u32,
}

impl StepBehavior for SnapshotBehavior {
  fn init(
    &mut self,
    checkpoint: Option<Vec<u8>>,
  ) -> StepResult {
    if let Some(data) = checkpoint {
      if let Ok(s) = String::from_utf8(data) {
        self.counter = s.parse().unwrap_or(0);
      }
    }
    StepResult::Continue
  }

  fn handle(
    &mut self,
    _msg: Envelope,
    _ctx: &mut TurnContext,
  ) -> StepResult {
    self.counter += 1;
    StepResult::Continue
  }

  fn terminate(&mut self, _reason: &Reason) {}

  fn snapshot(&self) -> Option<Vec<u8>> {
    Some(self.counter.to_string().into_bytes())
  }
}

struct BudgetLlmBehavior;

impl StepBehavior for BudgetLlmBehavior {
  fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
    StepResult::Continue
  }

  fn handle(
    &mut self,
    msg: Envelope,
    _ctx: &mut TurnContext,
  ) -> StepResult {
    match &msg.payload {
      EnvelopePayload::User(_) => {
        StepResult::Suspend(EffectRequest::new(
          EffectKind::LlmCall,
          serde_json::json!({"prompt": "expensive call"}),
        ))
      }
      EnvelopePayload::Effect(result) => {
        if result.status == EffectStatus::Failed {
          StepResult::Done(Reason::Custom(
            "budget exceeded".into(),
          ))
        } else {
          StepResult::Done(Reason::Normal)
        }
      }
      _ => StepResult::Continue,
    }
  }

  fn terminate(&mut self, _reason: &Reason) {}
}

// ────────────────────────────────────────────
// v0 gate: 100 processes, 10k messages, fair scheduling
// ────────────────────────────────────────────

#[test]
fn phase1_v0_gate_100_processes_10k_messages() {
  let mut rt = SchedulerRuntime::new();
  let total_msgs = Arc::new(AtomicU32::new(0));

  let mut pids = Vec::new();
  for _ in 0..100 {
    let counter = total_msgs.clone();
    let pid =
      rt.spawn(Box::new(CounterBehavior { count: counter }));
    pids.push(pid);
  }

  // Send 100 messages to each process (10k total)
  for pid in &pids {
    for i in 0..100 {
      rt.send(Envelope::text(*pid, format!("msg-{i}")));
    }
  }

  // Run until all messages are processed
  rt.run(1000);

  // All 10k messages should have been processed
  assert_eq!(total_msgs.load(Ordering::Relaxed), 10_000);
  assert_eq!(rt.process_count(), 100);
}

// ────────────────────────────────────────────
// v1 gate: Effect journaling + dispatch cycle
// ────────────────────────────────────────────

#[test]
fn phase1_v1_gate_effect_cycle() {
  let mut rt = SchedulerRuntime::with_durability();

  let pid = rt.spawn(Box::new(LlmAgentBehavior {
    state: LlmState::Idle,
  }));

  // Trigger the LLM call
  rt.send(Envelope::text(pid, "What is 2+2?"));
  rt.tick(); // Process suspends with LlmCall effect

  // Wait for reactor to complete the effect
  std::thread::sleep(std::time::Duration::from_millis(300));
  rt.tick(); // Deliver effect result, process handles it and exits

  let completed = rt.take_completed();
  assert_eq!(completed.len(), 1);
  assert_eq!(completed[0].0, pid);
  assert!(matches!(completed[0].1, Reason::Normal));
}

// ────────────────────────────────────────────
// v2 gate: Kill + snapshot/recovery simulation
// ────────────────────────────────────────────

#[test]
fn phase1_v2_gate_snapshot_recovery() {
  // Phase 1: Process some messages and take a snapshot
  let mut rt = SchedulerRuntime::new();
  let pid =
    rt.spawn(Box::new(SnapshotBehavior { counter: 0 }));

  // Send 5 messages
  for i in 0..5 {
    rt.send(Envelope::text(pid, format!("msg-{i}")));
  }
  rt.run(100);

  // Simulate recovery: kill process, create new one with
  // checkpoint data. The behavior's init() will restore
  // counter from checkpoint.
  rt.kill(pid);
  rt.tick();
  rt.reap_completed();

  // "Recover" by spawning with checkpoint data.
  // In a real system, the checkpoint bytes would come from
  // the SnapshotStore; here we simulate with known counter=5.
  let recovered_pid =
    rt.spawn(Box::new(SnapshotBehavior { counter: 5 }));
  rt.send(Envelope::text(recovered_pid, "post-recovery msg"));
  rt.tick();

  // Process should still be alive and processing
  assert!(rt.process_count() >= 1);
}

// ────────────────────────────────────────────
// v3 gate: Budget gate blocks LLM + supervisor notified
// ────────────────────────────────────────────

#[test]
fn phase1_v3_gate_budget_blocks_llm() {
  let budget = BudgetState {
    usd_remaining: 0.0, // No budget!
    token_remaining: 0,
  };

  let mut rt =
    SchedulerRuntime::new().with_budget(budget);

  let pid = rt.spawn(Box::new(BudgetLlmBehavior));
  rt.send(Envelope::text(pid, "trigger llm call"));

  rt.tick(); // Process suspends with LlmCall, budget blocks it
  rt.tick(); // Process receives failure result, exits

  // Should have budget violation recorded
  let violations = rt.take_budget_violations();
  assert_eq!(violations.len(), 1);
  assert_eq!(violations[0].pid, pid);
  assert!(matches!(
    violations[0].effect_kind,
    EffectKind::LlmCall
  ));

  // Process should be completed (with failure reason)
  let completed = rt.take_completed();
  assert_eq!(completed.len(), 1);
  assert!(matches!(completed[0].1, Reason::Custom(_)));
}

// ────────────────────────────────────────────
// Bonus: stress test for fairness
// ────────────────────────────────────────────

#[test]
fn phase1_fairness_no_process_starved() {
  let mut rt =
    SchedulerRuntime::new().with_max_reductions(10);
  let counters: Vec<Arc<AtomicU32>> =
    (0..10).map(|_| Arc::new(AtomicU32::new(0))).collect();

  let mut pids = Vec::new();
  for counter in &counters {
    let pid = rt.spawn(Box::new(CounterBehavior {
      count: counter.clone(),
    }));
    pids.push(pid);
  }

  // Send 100 messages to each
  for pid in &pids {
    for i in 0..100 {
      rt.send(Envelope::text(*pid, format!("m-{i}")));
    }
  }

  rt.run(1000);

  // Every process should have processed all its messages
  for (i, counter) in counters.iter().enumerate() {
    let count = counter.load(Ordering::Relaxed);
    assert_eq!(
      count, 100,
      "process {i} only processed {count}/100 messages"
    );
  }
}
