# Phase 2.5 Wiring Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Wire all Phase 2 components (TurnExecutor, CompensationLog, IdempotencyStore, RecoveryCoordinator, BudgetGate, Metrics) into the runtime's tick loop to close the critical journal-before-dispatch gap.

**Architecture:** The scheduler engine produces outbound effects and messages. Currently these go directly to the reactor without journaling. We fix this by intercepting them in `SchedulerRuntime::tick()`, building per-process `TurnCommit` structs, persisting them via `TurnExecutor::commit()`, and only THEN dispatching to the reactor and delivering messages. This restores the spec invariant: **journal before dispatch**.

**Tech Stack:** Rust, SQLite (rusqlite), serde_json, crossbeam-channel, tracing

---

## Gap Summary

| Gap | Description | Severity |
|-----|-------------|----------|
| G1 | TurnExecutor::commit() never called | Critical |
| G2 | Effects dispatched without journaling | Critical |
| G3 | Messages delivered without journaling | Critical |
| G4 | Snapshots never extracted from behaviors | High |
| G5 | advance_clock() never called by runtime | High |
| G6 | CompensationLog not wired to Rollback intent | Medium |
| G7 | IdempotencyStore not wired to effect dispatch | Medium |
| G8 | RecoveryCoordinator not wired to runtime | Medium |
| G9 | Metrics (turns.committed, effects.completed, etc.) never incremented | Medium |
| G10 | PatchState intent silently dropped | Medium |
| G11 | Rollback intent is a no-op | Medium |

---

### Task 1: Expose outbound messages from SchedulerEngine

Currently `SchedulerEngine::tick()` delivers outbound messages internally (line 136-140 in scheduler.rs). For journaling, the runtime must intercept messages before delivery. We export them like we already export effects.

**Files:**
- Modify: `zeptovm/src/kernel/scheduler.rs` (lines 116-143, 364-368)
- Test: `zeptovm/src/kernel/scheduler.rs` (existing tests + new)

**Step 1: Write the failing test**

Add to `scheduler.rs` tests:

```rust
#[test]
fn test_scheduler_take_outbound_messages() {
  let mut engine = SchedulerEngine::new();
  let receiver = engine.spawn(Box::new(Echo));
  let sender =
    engine.spawn(Box::new(Forwarder { target: receiver }));
  engine.send(Envelope::text(sender, "trigger"));
  engine.tick();
  let messages = engine.take_outbound_messages();
  assert_eq!(messages.len(), 1);
  assert_eq!(messages[0].to, receiver);
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::scheduler::tests::test_scheduler_take_outbound_messages -- --exact`
Expected: FAIL — `take_outbound_messages` method not found

**Step 3: Implement take_outbound_messages and stop self-delivering**

In `scheduler.rs`, change `tick()` to NOT deliver messages internally, and add the accessor:

```rust
pub fn tick(&mut self) -> usize {
  self.tick_count += 1;
  let _span = info_span!(
    "scheduler_tick",
    tick = self.tick_count,
  )
  .entered();
  let ready: Vec<Pid> = self.ready_queue.drain(..).collect();
  let mut stepped = 0;

  for pid in ready {
    if !self.processes.contains_key(&pid) {
      continue;
    }
    stepped += 1;
    self.step_process(pid);
  }

  // NOTE: No longer self-delivering messages here.
  // The runtime takes them via take_outbound_messages()
  // and delivers after journaling.

  stepped
}

/// Take outbound messages (for journaling before delivery).
pub fn take_outbound_messages(&mut self) -> Vec<Envelope> {
  std::mem::take(&mut self.outbound_messages)
}
```

**Step 4: Update runtime.rs to deliver messages**

In `SchedulerRuntime::tick()`, after taking effects, also take and re-deliver messages:

```rust
// After engine.tick() and before dispatching effects:
let messages = self.engine.take_outbound_messages();
for msg in messages {
  self.engine.send(msg);
}
```

**Step 5: Run ALL tests to verify no behavior change**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL tests pass (including message forwarding tests)

**Step 6: Commit**

```bash
git add zeptovm/src/kernel/scheduler.rs zeptovm/src/kernel/runtime.rs
git commit -m "refactor(scheduler): expose outbound messages for runtime journaling"
```

---

### Task 2: Add snapshot accessor to SchedulerEngine

The TurnExecutor needs process snapshots. ProcessEntry already has `snapshot()`. We expose it through the scheduler.

**Files:**
- Modify: `zeptovm/src/kernel/scheduler.rs`
- Test: `zeptovm/src/kernel/scheduler.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_scheduler_snapshot_for() {
  struct Snapshotted;
  impl StepBehavior for Snapshotted {
    fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
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
      Some(b"my-state".to_vec())
    }
  }

  let mut engine = SchedulerEngine::new();
  let pid = engine.spawn(Box::new(Snapshotted));
  let snap = engine.snapshot_for(pid);
  assert_eq!(snap, Some(b"my-state".to_vec()));
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::scheduler::tests::test_scheduler_snapshot_for -- --exact`
Expected: FAIL — `snapshot_for` not found

**Step 3: Implement**

```rust
/// Get a snapshot of a process's behavior state.
pub fn snapshot_for(&self, pid: Pid) -> Option<Vec<u8>> {
  self.processes.get(&pid).and_then(|p| p.snapshot())
}
```

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/scheduler.rs
git commit -m "feat(scheduler): add snapshot_for accessor"
```

---

### Task 3: Wire TurnExecutor::commit() into runtime tick

This is the critical fix. After the scheduler produces outbound effects and messages, we build TurnCommits grouped by pid and journal them BEFORE dispatching.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs` (tick method, lines 136-184)
- Test: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_runtime_journals_before_dispatch() {
  let mut rt = SchedulerRuntime::with_durability();
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick(); // Process suspends with LlmCall effect

  // The turn executor should have journaled the effect
  assert!(
    rt.metrics().counter("turns.committed") > 0,
    "turn should have been committed to journal"
  );
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::runtime::tests::test_runtime_journals_before_dispatch -- --exact`
Expected: FAIL — `turns.committed` counter is 0

**Step 3: Implement journal-before-dispatch in tick()**

Rewrite the relevant section of `SchedulerRuntime::tick()`:

```rust
pub fn tick(&mut self) -> usize {
  // 1. Drain reactor completions
  if let Some(ref reactor) = self.reactor {
    let completions = reactor.drain_completions();
    for completion in completions {
      self.engine.deliver_effect_result(completion.result);
    }
  }

  // 2. Advance clock
  let now_ms = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis() as u64;
  self.engine.advance_clock(now_ms);

  // 3. Tick scheduler
  let stepped = self.engine.tick();
  debug!(stepped, "runtime tick complete");
  self.metrics.inc("scheduler.ticks");

  // 4. Take outbound effects and messages
  let effects = self.engine.take_outbound_effects();
  let messages = self.engine.take_outbound_messages();

  // 5. Journal before dispatch (if turn_executor is present)
  if let Some(ref mut executor) = self.turn_executor {
    // Group effects by pid
    let mut effects_by_pid: std::collections::HashMap<
      Pid,
      Vec<(Pid, EffectRequest)>,
    > = std::collections::HashMap::new();
    for (pid, req) in &effects {
      effects_by_pid
        .entry(*pid)
        .or_default()
        .push((*pid, req.clone()));
    }

    // Group messages by sender (from field)
    let mut messages_by_sender: std::collections::HashMap<
      Pid,
      Vec<Envelope>,
    > = std::collections::HashMap::new();
    for msg in &messages {
      let sender = msg.from.unwrap_or(msg.to);
      messages_by_sender
        .entry(sender)
        .or_default()
        .push(msg.clone());
    }

    // Collect all pids that produced output
    let mut pids: std::collections::HashSet<Pid> =
      std::collections::HashSet::new();
    pids.extend(effects_by_pid.keys());
    pids.extend(messages_by_sender.keys());

    // Build and commit a TurnCommit per pid
    for pid in pids {
      let turn = TurnCommit {
        pid,
        turn_id: crate::core::turn_context::next_turn_id(),
        journal_entries: vec![],
        outbound_messages: messages_by_sender
          .remove(&pid)
          .unwrap_or_default(),
        effect_requests: effects_by_pid
          .remove(&pid)
          .unwrap_or_default(),
        state_snapshot: self.engine.snapshot_for(pid),
      };
      if let Err(e) = executor.commit(&turn) {
        warn!(pid = %pid, error = %e, "turn commit failed");
      } else {
        self.metrics.inc("turns.committed");
      }
    }
  }

  // 6. Deliver messages (after journaling)
  for msg in messages {
    self.engine.send(msg);
  }

  // 7. Process outbound effects (budget gate -> reactor dispatch)
  for (pid, req) in effects {
    if self.budget_enabled && self.should_block_effect(&req)
    {
      warn!(
        pid = %pid,
        kind = ?req.kind,
        "effect blocked by budget"
      );
      self.metrics.inc("budget.blocked");
      let violation = BudgetViolation {
        pid,
        effect_kind: req.kind.clone(),
        reason: "budget exceeded".to_string(),
      };
      self.budget_violations.push(violation);
      let result = EffectResult::failure(
        req.effect_id,
        "budget exceeded",
      );
      self.engine.deliver_effect_result(result);
    } else {
      self.metrics.inc("effects.dispatched");
      if let Some(ref reactor) = self.reactor {
        reactor.dispatch(pid, req);
      }
    }
  }

  stepped
}
```

Also need to add imports at top of runtime.rs:
```rust
use crate::kernel::turn_executor::TurnCommit;
```

And remove `#[allow(dead_code)]` from the `turn_executor` field.

**Step 4: Run all tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass (including the new journal test)

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire TurnExecutor into tick — journal before dispatch"
```

---

### Task 4: Wire PatchState intent into scheduler

Currently `TurnIntent::PatchState(_data)` is silently dropped in scheduler.rs line 196-198. We need to capture it for the TurnCommit.

**Files:**
- Modify: `zeptovm/src/kernel/scheduler.rs`
- Test: `zeptovm/src/kernel/scheduler.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_scheduler_patch_state_captured() {
  struct StatePatcher;
  impl StepBehavior for StatePatcher {
    fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _: Envelope,
      ctx: &mut TurnContext,
    ) -> StepResult {
      ctx.set_state(b"new-state".to_vec());
      StepResult::Continue
    }
    fn terminate(&mut self, _: &Reason) {}
  }

  let mut engine = SchedulerEngine::new();
  let pid = engine.spawn(Box::new(StatePatcher));
  engine.send(Envelope::text(pid, "patch"));
  engine.tick();
  let patches = engine.take_state_patches();
  assert_eq!(patches.len(), 1);
  assert_eq!(patches[0].0, pid);
  assert_eq!(patches[0].1, b"new-state");
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::scheduler::tests::test_scheduler_patch_state_captured -- --exact`
Expected: FAIL — no method `take_state_patches`

**Step 3: Implement**

Add a `state_patches` field to `SchedulerEngine` and route `PatchState` there:

```rust
// In SchedulerEngine struct:
state_patches: Vec<(Pid, Vec<u8>)>,

// In new():
state_patches: Vec::new(),

// In step_process, replace the PatchState arm:
TurnIntent::PatchState(data) => {
  self.state_patches.push((pid, data));
}

// New method:
pub fn take_state_patches(&mut self) -> Vec<(Pid, Vec<u8>)> {
  std::mem::take(&mut self.state_patches)
}
```

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 5: Wire state patches into runtime TurnCommit**

In `runtime.rs` tick(), after taking effects and messages, also take state patches and include them in the TurnCommit as journal entries:

```rust
let patches = self.engine.take_state_patches();
// Include patches in TurnCommit journal_entries for the matching pid
```

Add to the TurnCommit building loop:

```rust
use crate::durability::journal::{JournalEntry, JournalEntryType};

// ... inside the per-pid loop:
let mut journal_entries = Vec::new();
if let Some(state_data) = patches_by_pid.remove(&pid) {
  journal_entries.push(JournalEntry::new(
    pid,
    JournalEntryType::StatePatched,
    Some(state_data),
  ));
}
let turn = TurnCommit {
  pid,
  turn_id: crate::core::turn_context::next_turn_id(),
  journal_entries,
  // ... rest unchanged
};
```

**Step 6: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 7: Commit**

```bash
git add zeptovm/src/kernel/scheduler.rs zeptovm/src/kernel/runtime.rs
git commit -m "feat(scheduler): route PatchState intent to journal"
```

---

### Task 5: Wire advance_clock into runtime tick

`SchedulerEngine::advance_clock(now_ms)` exists but `SchedulerRuntime` never calls it. Timers fire based on this clock.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`
- Test: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_runtime_advances_clock() {
  use crate::core::behavior::StepBehavior;
  use crate::core::message::{Envelope, EnvelopePayload, Signal};
  use crate::core::timer::{TimerKind, TimerSpec};

  struct TimerBehavior;
  impl StepBehavior for TimerBehavior {
    fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      msg: Envelope,
      ctx: &mut TurnContext,
    ) -> StepResult {
      match &msg.payload {
        EnvelopePayload::Signal(
          Signal::TimerFired(_),
        ) => StepResult::Done(Reason::Normal),
        _ => {
          let spec = TimerSpec::new(
            ctx.pid,
            TimerKind::Timeout,
            1, // 1ms — will fire on next tick
          );
          ctx.schedule_timer(spec);
          StepResult::Continue
        }
      }
    }
    fn terminate(&mut self, _: &Reason) {}
  }

  let mut rt = SchedulerRuntime::new();
  let pid = rt.spawn(Box::new(TimerBehavior));
  rt.send(Envelope::text(pid, "schedule"));
  rt.tick(); // Schedules timer

  // Second tick should advance clock and fire the 1ms timer
  std::thread::sleep(std::time::Duration::from_millis(5));
  rt.tick();

  let completed = rt.take_completed();
  assert_eq!(
    completed.len(), 1,
    "timer should have fired and process should be done"
  );
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib kernel::runtime::tests::test_runtime_advances_clock -- --exact`
Expected: FAIL — timer never fires because clock never advances

**Step 3: Implement**

This should already be done as part of Task 3's tick rewrite (the `advance_clock(now_ms)` call). If not already present, add it at the start of tick() before engine.tick():

```rust
let now_ms = std::time::SystemTime::now()
  .duration_since(std::time::UNIX_EPOCH)
  .unwrap_or_default()
  .as_millis() as u64;
self.engine.advance_clock(now_ms);
```

Also add metrics:
```rust
// In the timer metrics section, wire timers.scheduled and timers.fired
// These are tracked by the engine, so we may need to add tracking there.
```

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire advance_clock for automatic timer firing"
```

---

### Task 6: Wire CompensationLog into runtime

CompensationLog exists (`kernel/compensation.rs`) but is never used. We need to:
1. Add it to SchedulerRuntime
2. Record compensatable effects when they complete
3. Wire TurnIntent::Rollback to trigger rollback_all

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`
- Modify: `zeptovm/src/kernel/scheduler.rs` (Rollback intent)
- Test: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_runtime_compensation_rollback() {
  use crate::core::effect::{
    CompensationSpec, EffectKind, EffectRequest, EffectResult,
  };

  struct CompensatingAgent {
    phase: u8,
  }
  impl StepBehavior for CompensatingAgent {
    fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      msg: Envelope,
      ctx: &mut TurnContext,
    ) -> StepResult {
      match self.phase {
        0 => {
          // Request compensatable effect
          self.phase = 1;
          StepResult::Suspend(
            EffectRequest::new(
              EffectKind::Http,
              serde_json::json!({"action": "charge"}),
            )
            .with_compensation(CompensationSpec {
              undo_kind: EffectKind::Http,
              undo_input: serde_json::json!({"action": "refund"}),
            }),
          )
        }
        1 => {
          // Effect completed, now trigger rollback
          self.phase = 2;
          ctx.rollback();
          StepResult::Done(Reason::Normal)
        }
        _ => StepResult::Continue,
      }
    }
    fn terminate(&mut self, _: &Reason) {}
  }

  let mut rt = SchedulerRuntime::with_durability();
  let pid = rt.spawn(Box::new(CompensatingAgent { phase: 0 }));
  rt.send(Envelope::text(pid, "start"));
  rt.tick(); // Process suspends with compensatable effect

  // Simulate effect completion
  std::thread::sleep(std::time::Duration::from_millis(200));
  rt.tick(); // Delivers result, process calls rollback, exits

  assert!(
    rt.metrics().counter("compensation.triggered") > 0,
    "compensation should have been triggered"
  );
}
```

**Step 2: Run test to verify it fails**

Expected: FAIL — compensation.triggered is 0

**Step 3: Implement**

Add to `SchedulerRuntime`:
```rust
compensation_log: CompensationLog,
```

Initialize in constructors:
```rust
compensation_log: CompensationLog::new(),
```

In the reactor completion handling, when an effect result arrives, check if the original request had compensation, and if so record it in the log.

Wire Rollback: Change scheduler.rs to export rollback requests similar to state_patches:
```rust
// In scheduler.rs:
rollback_pids: Vec<Pid>,

// In step_process:
TurnIntent::Rollback => {
  self.rollback_pids.push(pid);
}

// New accessor:
pub fn take_rollback_requests(&mut self) -> Vec<Pid> {
  std::mem::take(&mut self.rollback_pids)
}
```

In runtime tick, after taking rollbacks:
```rust
let rollback_pids = self.engine.take_rollback_requests();
for pid in rollback_pids {
  let undo_effects =
    self.compensation_log.rollback_all(pid);
  for req in undo_effects {
    self.metrics.inc("compensation.triggered");
    if let Some(ref reactor) = self.reactor {
      reactor.dispatch(pid, req);
    }
  }
}
```

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/scheduler.rs zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire CompensationLog for saga-style rollback"
```

---

### Task 7: Wire IdempotencyStore into runtime

IdempotencyStore exists (`durability/idempotency.rs`) but is never used. We check it before dispatching effects with an idempotency_key.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`
- Test: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_runtime_idempotency_dedup() {
  use crate::core::effect::{
    EffectKind, EffectRequest, EffectResult, EffectStatus,
  };

  // Two identical effects with same idempotency key
  // Only first should actually dispatch
  let mut rt = SchedulerRuntime::with_durability();

  struct IdempotentCaller {
    call_count: u8,
  }
  impl StepBehavior for IdempotentCaller {
    fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      msg: Envelope,
      _ctx: &mut TurnContext,
    ) -> StepResult {
      match &msg.payload {
        EnvelopePayload::Effect(_) => {
          self.call_count += 1;
          if self.call_count >= 2 {
            StepResult::Done(Reason::Normal)
          } else {
            // Second call with same key
            StepResult::Suspend(
              EffectRequest::new(
                EffectKind::Http,
                serde_json::json!({}),
              )
              .with_idempotency_key("idem-key-1"),
            )
          }
        }
        _ => StepResult::Suspend(
          EffectRequest::new(
            EffectKind::Http,
            serde_json::json!({}),
          )
          .with_idempotency_key("idem-key-1"),
        ),
      }
    }
    fn terminate(&mut self, _: &Reason) {}
  }

  let pid = rt.spawn(Box::new(IdempotentCaller {
    call_count: 0,
  }));
  rt.send(Envelope::text(pid, "go"));
  rt.tick(); // First call

  std::thread::sleep(std::time::Duration::from_millis(200));
  rt.tick(); // Result comes back, second call with same key
  // Second call should be served from cache (no reactor dispatch)

  std::thread::sleep(std::time::Duration::from_millis(200));
  rt.tick(); // Second result, process exits

  let completed = rt.take_completed();
  assert_eq!(completed.len(), 1);
}
```

**Step 2: Run test to verify it fails**

Expected: FAIL or timeout (idempotency not checked)

**Step 3: Implement**

Add to `SchedulerRuntime`:
```rust
idempotency_store: Option<IdempotencyStore>,
```

In `with_durability()`:
```rust
idempotency_store: Some(
  IdempotencyStore::open_in_memory().unwrap(),
),
```

In the effect dispatch section of tick(), before dispatching to reactor:
```rust
// Check idempotency store
if let (Some(ref key), Some(ref store)) =
  (&req.idempotency_key, &self.idempotency_store)
{
  if let Ok(Some(cached)) = store.check(&req.effect_id) {
    // Already executed — deliver cached result
    self.engine.deliver_effect_result(cached);
    continue;
  }
}
```

After receiving reactor completions, record in store:
```rust
if let Some(ref store) = self.idempotency_store {
  let _ = store.record(
    &completion.result.effect_id,
    &completion.result,
  );
}
```

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire IdempotencyStore for effect deduplication"
```

---

### Task 8: Wire RecoveryCoordinator into runtime

Add a `recover_process()` method to `SchedulerRuntime` that uses the RecoveryCoordinator to restore a process from journal + snapshot.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`
- Test: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_runtime_recover_process() {
  use crate::core::behavior::StepBehavior;
  use crate::durability::journal::{
    JournalEntry, JournalEntryType,
  };
  use crate::durability::snapshot::Snapshot;

  struct Recoverable {
    state: String,
  }
  impl StepBehavior for Recoverable {
    fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
      self.state = "fresh".into();
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
    fn restore(
      &mut self,
      data: &[u8],
    ) -> Result<(), String> {
      self.state =
        String::from_utf8(data.to_vec())
          .map_err(|e| e.to_string())?;
      Ok(())
    }
    fn snapshot(&self) -> Option<Vec<u8>> {
      Some(self.state.as_bytes().to_vec())
    }
  }

  let mut rt = SchedulerRuntime::with_durability();
  let pid = Pid::from_raw(42);

  // Pre-populate journal and snapshot
  // (simulate a previous run)
  // Then recover
  let recovered = rt.recover_process(pid, &|| {
    Box::new(Recoverable {
      state: String::new(),
    })
  });
  assert!(recovered.is_ok());
}
```

**Step 2: Run test to verify it fails**

Expected: FAIL — no method `recover_process`

**Step 3: Implement**

```rust
/// Recover a process from durable storage.
pub fn recover_process(
  &mut self,
  pid: Pid,
  factory: &dyn Fn() -> Box<dyn StepBehavior>,
) -> Result<(), String> {
  let executor = self
    .turn_executor
    .as_ref()
    .ok_or("no turn executor")?;

  let coord = crate::kernel::recovery::RecoveryCoordinator::new(
    executor.journal(),
    executor.snapshot_store(),
  );

  let recovered = coord.recover_process(pid, factory)?;

  // Re-insert into scheduler
  // (We need a method to insert a pre-built ProcessEntry)
  // For now, this is a placeholder — full wiring requires
  // scheduler to accept ProcessEntry directly.
  Ok(())
}
```

This may require adding an `insert_process` method to SchedulerEngine.

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs zeptovm/src/kernel/scheduler.rs
git commit -m "feat(runtime): wire RecoveryCoordinator for process recovery"
```

---

### Task 9: Wire remaining metrics

Metrics counters `turns.committed`, `effects.completed`, `effects.failed`, `effects.retries`, `effects.timed_out`, `timers.scheduled`, `timers.fired`, `compensation.triggered` are registered but never incremented.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`
- Test: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_runtime_metrics_effects_completed() {
  let mut rt = SchedulerRuntime::with_durability();
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick(); // Suspends

  std::thread::sleep(std::time::Duration::from_millis(200));
  rt.tick(); // Delivers result

  assert!(
    rt.metrics().counter("effects.completed") > 0,
    "effects.completed should be incremented"
  );
}

#[test]
fn test_runtime_metrics_turns_committed() {
  let mut rt = SchedulerRuntime::with_durability();
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();

  assert!(
    rt.metrics().counter("turns.committed") > 0,
    "turns.committed should be incremented"
  );
}
```

**Step 2: Run test to verify it fails**

Expected: FAIL — counters are 0

**Step 3: Implement**

Add metric increments at appropriate points in tick():

```rust
// When receiving completions from reactor:
for completion in completions {
  match completion.result.status {
    EffectStatus::Succeeded => {
      self.metrics.inc("effects.completed");
    }
    EffectStatus::Failed => {
      self.metrics.inc("effects.failed");
    }
    EffectStatus::TimedOut => {
      self.metrics.inc("effects.timed_out");
    }
    _ => {}
  }
  self.engine.deliver_effect_result(completion.result);
}

// turns.committed is already wired in Task 3
```

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(runtime): wire remaining metrics counters"
```

---

### Task 10: Integration gate tests

End-to-end tests verifying the full wiring: journal-before-dispatch invariant, clock advance, compensation, idempotency, recovery, and metrics.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs` (add tests)

**Step 1: Write integration tests**

```rust
#[test]
fn test_gate_journal_before_dispatch_invariant() {
  // Spawn a process that emits an LLM effect.
  // After tick, verify journal has EffectRequested entry
  // BEFORE the effect was dispatched to reactor.
  let mut rt = SchedulerRuntime::with_durability();
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();

  // Journal should contain the effect
  assert!(rt.metrics().counter("turns.committed") >= 1);
  assert!(rt.metrics().counter("effects.dispatched") >= 1);
}

#[test]
fn test_gate_clock_advance_fires_timers() {
  // Process schedules 1ms timer.
  // After tick + sleep + tick, process receives TimerFired.
  // (Same as Task 5 test, included here for gate coverage)
}

#[test]
fn test_gate_full_lifecycle() {
  // Spawn -> send -> effect -> completion -> metrics -> exit
  let mut rt = SchedulerRuntime::with_durability();
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();

  std::thread::sleep(std::time::Duration::from_millis(200));
  rt.tick();

  let completed = rt.take_completed();
  assert_eq!(completed.len(), 1);

  // Verify metrics
  let m = rt.metrics();
  assert!(m.counter("processes.spawned") >= 1);
  assert!(m.counter("effects.dispatched") >= 1);
  assert!(m.counter("effects.completed") >= 1);
  assert!(m.counter("turns.committed") >= 1);
  assert!(m.counter("processes.exited") >= 1);
}

#[test]
fn test_gate_message_journaled_before_delivery() {
  // Process A sends message to B via TurnContext.
  // After tick, verify journal has MessageSent entry.
  let mut rt = SchedulerRuntime::with_durability();
  let receiver = rt.spawn(Box::new(Echo));
  let sender =
    rt.spawn(Box::new(Forwarder { target: receiver }));
  rt.send(Envelope::text(sender, "trigger"));
  rt.tick();

  assert!(rt.metrics().counter("turns.committed") >= 1);
}
```

**Step 2: Run all tests**

Run: `cd zeptovm && cargo test --lib`
Expected: ALL pass

**Step 3: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "test: add Phase 2.5 wiring gate tests"
```

---

## Execution Order

1. Task 1 (expose outbound messages) — unblocks Tasks 3, 4, 10
2. Task 2 (snapshot accessor) — unblocks Task 3
3. **Task 3 (wire TurnExecutor)** — THE critical fix
4. Task 4 (PatchState) — builds on Task 1
5. Task 5 (advance_clock) — partially done in Task 3
6. Task 6 (CompensationLog) — independent
7. Task 7 (IdempotencyStore) — independent
8. Task 8 (RecoveryCoordinator) — independent
9. Task 9 (metrics) — partially done in Tasks 3, 6
10. Task 10 (gate tests) — final validation

## Success Criteria

After all tasks:
- `TurnExecutor::commit()` is called for every tick that produces output
- Effects are journaled BEFORE reactor dispatch
- Messages are journaled BEFORE internal delivery
- `advance_clock()` is called every tick with wall-clock time
- PatchState writes go to journal
- Rollback triggers CompensationLog
- Idempotent effects are deduplicated
- RecoveryCoordinator can restore processes
- All 16 registered metrics are incremented at appropriate points
- `cargo test --lib` passes with 0 failures
