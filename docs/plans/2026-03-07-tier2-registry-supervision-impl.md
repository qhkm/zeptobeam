# Tier 2 — Name Registry + Supervision Strategies Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add name-based process registry (X1) and OneForAll/RestForOne supervision strategies (G5) to ZeptoVM.

**Architecture:** NameRegistry is a new struct in `kernel/` with bidirectional HashMap. It integrates with SchedulerEngine for auto-cleanup on exit and with TurnContext for intent-based registration. SupervisorBehavior gets a SupervisionStrategy enum and a ShuttingDown state machine for coordinated restarts.

**Tech Stack:** Rust, existing ZeptoVM kernel (scheduler, process_table, supervisor_behavior)

---

### Task 1: NameRegistry struct with basic API

**Files:**
- Create: `zeptovm/src/kernel/name_registry.rs`
- Modify: `zeptovm/src/kernel/mod.rs`

**Step 1: Write the failing tests**

Create `zeptovm/src/kernel/name_registry.rs`:

```rust
use std::collections::HashMap;

use crate::pid::Pid;

/// Global flat name-based process registry.
///
/// Provides O(1) lookup in both directions (name->Pid, Pid->name).
/// Names are unique — registering a duplicate returns an error.
pub struct NameRegistry {
  names: HashMap<String, Pid>,
  pid_names: HashMap<Pid, String>,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_register_and_whereis() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    assert_eq!(reg.whereis("logger"), Some(pid));
  }

  #[test]
  fn test_register_duplicate_name_fails() {
    let mut reg = NameRegistry::new();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);
    reg.register("logger".into(), pid1).unwrap();
    let err = reg.register("logger".into(), pid2);
    assert!(err.is_err());
  }

  #[test]
  fn test_unregister() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    let old = reg.unregister("logger");
    assert_eq!(old, Some(pid));
    assert_eq!(reg.whereis("logger"), None);
  }

  #[test]
  fn test_unregister_unknown_returns_none() {
    let mut reg = NameRegistry::new();
    assert_eq!(reg.unregister("nope"), None);
  }

  #[test]
  fn test_unregister_by_pid() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    let name = reg.unregister_pid(pid);
    assert_eq!(name, Some("logger".to_string()));
    assert_eq!(reg.whereis("logger"), None);
  }

  #[test]
  fn test_registered_name() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    assert_eq!(
      reg.registered_name(pid),
      Some("logger")
    );
  }

  #[test]
  fn test_reregister_after_unregister() {
    let mut reg = NameRegistry::new();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);
    reg.register("logger".into(), pid1).unwrap();
    reg.unregister("logger");
    reg.register("logger".into(), pid2).unwrap();
    assert_eq!(reg.whereis("logger"), Some(pid2));
  }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm name_registry`
Expected: FAIL — methods don't exist yet.

**Step 3: Implement NameRegistry**

Add to `zeptovm/src/kernel/name_registry.rs` (above `#[cfg(test)]`):

```rust
impl NameRegistry {
  pub fn new() -> Self {
    Self {
      names: HashMap::new(),
      pid_names: HashMap::new(),
    }
  }

  /// Register a name for a pid. Fails if name already taken.
  pub fn register(
    &mut self,
    name: String,
    pid: Pid,
  ) -> Result<(), String> {
    if self.names.contains_key(&name) {
      return Err(format!(
        "name '{}' already registered",
        name
      ));
    }
    self.names.insert(name.clone(), pid);
    self.pid_names.insert(pid, name);
    Ok(())
  }

  /// Unregister a name. Returns the old pid if it existed.
  pub fn unregister(&mut self, name: &str) -> Option<Pid> {
    if let Some(pid) = self.names.remove(name) {
      self.pid_names.remove(&pid);
      Some(pid)
    } else {
      None
    }
  }

  /// Unregister by pid (for auto-cleanup on exit).
  /// Returns the name if it was registered.
  pub fn unregister_pid(
    &mut self,
    pid: Pid,
  ) -> Option<String> {
    if let Some(name) = self.pid_names.remove(&pid) {
      self.names.remove(&name);
      Some(name)
    } else {
      None
    }
  }

  /// Lookup a pid by name.
  pub fn whereis(&self, name: &str) -> Option<Pid> {
    self.names.get(name).copied()
  }

  /// Reverse lookup: get the registered name for a pid.
  pub fn registered_name(
    &self,
    pid: Pid,
  ) -> Option<&str> {
    self.pid_names.get(&pid).map(|s| s.as_str())
  }
}
```

**Step 4: Add module to kernel/mod.rs**

Add `pub mod name_registry;` to `zeptovm/src/kernel/mod.rs`.

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm name_registry`
Expected: All 7 tests pass.

**Step 6: Commit**

```bash
git add zeptovm/src/kernel/name_registry.rs zeptovm/src/kernel/mod.rs
git commit -m "feat(registry): add NameRegistry with name->Pid mapping (X1)"
```

---

### Task 2: Integrate NameRegistry into SchedulerEngine

**Files:**
- Modify: `zeptovm/src/kernel/scheduler.rs`
- Modify: `zeptovm/src/core/turn_context.rs`

**Step 1: Write the failing tests**

Add to `zeptovm/src/kernel/scheduler.rs` tests:

```rust
#[test]
fn test_scheduler_register_and_whereis() {
  let mut engine = SchedulerEngine::new();
  let pid = engine.spawn(Box::new(Echo));
  engine.register_name("worker".into(), pid).unwrap();
  assert_eq!(engine.whereis("worker"), Some(pid));
}

#[test]
fn test_scheduler_send_named() {
  let mut engine = SchedulerEngine::new();
  let pid = engine.spawn(Box::new(Echo));
  engine.register_name("worker".into(), pid).unwrap();
  let result = engine.send_named(
    "worker",
    Envelope::text(pid, "hello"),
  );
  assert!(result.is_ok());
  engine.tick();
}

#[test]
fn test_scheduler_send_named_unknown_fails() {
  let mut engine = SchedulerEngine::new();
  let pid = Pid::from_raw(1);
  let result = engine.send_named(
    "nope",
    Envelope::text(pid, "hello"),
  );
  assert!(result.is_err());
}

#[test]
fn test_scheduler_auto_unregister_on_exit() {
  let mut engine = SchedulerEngine::new();
  let pid = engine.spawn(Box::new(Stopper));
  engine.register_name("worker".into(), pid).unwrap();
  assert_eq!(engine.whereis("worker"), Some(pid));

  engine.send(Envelope::text(pid, "stop"));
  engine.tick();

  // After process exits, name should be auto-unregistered
  assert_eq!(engine.whereis("worker"), None);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_scheduler_register`
Expected: FAIL — `register_name`, `whereis`, `send_named` don't exist.

**Step 3: Add NameRegistry to SchedulerEngine**

In `zeptovm/src/kernel/scheduler.rs`:

Add import: `use crate::kernel::name_registry::NameRegistry;`

Add field to `SchedulerEngine` struct:
```rust
name_registry: NameRegistry,
```

Initialize in `new()`:
```rust
name_registry: NameRegistry::new(),
```

Add public methods:
```rust
/// Register a name for a process.
pub fn register_name(
  &mut self,
  name: String,
  pid: Pid,
) -> Result<(), String> {
  if !self.processes.contains_key(&pid) {
    return Err(format!(
      "process {} not found",
      pid.raw()
    ));
  }
  self.name_registry.register(name, pid)
}

/// Unregister a name.
pub fn unregister_name(
  &mut self,
  name: &str,
) -> Option<Pid> {
  self.name_registry.unregister(name)
}

/// Lookup a process by name.
pub fn whereis(&self, name: &str) -> Option<Pid> {
  self.name_registry.whereis(name)
}

/// Send a message to a named process.
pub fn send_named(
  &mut self,
  name: &str,
  mut env: Envelope,
) -> Result<(), String> {
  let pid = self
    .name_registry
    .whereis(name)
    .ok_or_else(|| {
      format!("name '{}' not registered", name)
    })?;
  env.to = pid;
  self.send(env);
  Ok(())
}
```

In `propagate_exit()`, add auto-unregister at the top:
```rust
fn propagate_exit(
  &mut self,
  pid: Pid,
  reason: &Reason,
) {
  // Auto-unregister name on exit
  self.name_registry.unregister_pid(pid);

  // ... existing link/monitor/parent propagation ...
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm scheduler`
Expected: All tests pass including 4 new registry tests.

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/scheduler.rs
git commit -m "feat(registry): integrate NameRegistry into SchedulerEngine (X1)"
```

---

### Task 3: TurnIntent integration for name registration

**Files:**
- Modify: `zeptovm/src/core/turn_context.rs`
- Modify: `zeptovm/src/kernel/scheduler.rs`

**Step 1: Write the failing tests**

Add to `zeptovm/src/core/turn_context.rs` tests:

```rust
#[test]
fn test_turn_context_register_name() {
  let pid = Pid::from_raw(1);
  let mut ctx = TurnContext::new(pid);
  ctx.register_name("my_name".into());
  assert_eq!(ctx.intent_count(), 1);
  let intents = ctx.take_intents();
  assert!(matches!(
    &intents[0],
    TurnIntent::RegisterName(name) if name == "my_name"
  ));
}

#[test]
fn test_turn_context_unregister_name() {
  let pid = Pid::from_raw(1);
  let mut ctx = TurnContext::new(pid);
  ctx.unregister_name("my_name".into());
  assert_eq!(ctx.intent_count(), 1);
  let intents = ctx.take_intents();
  assert!(matches!(
    &intents[0],
    TurnIntent::UnregisterName(name) if name == "my_name"
  ));
}

#[test]
fn test_turn_context_send_named() {
  let pid = Pid::from_raw(1);
  let mut ctx = TurnContext::new(pid);
  ctx.send_named("worker".into(), "hello".into());
  assert_eq!(ctx.intent_count(), 1);
}
```

Add to `zeptovm/src/kernel/scheduler.rs` tests:

```rust
#[test]
fn test_scheduler_register_name_via_intent() {
  let mut engine = SchedulerEngine::new();

  struct Registerer;
  impl StepBehavior for Registerer {
    fn init(
      &mut self,
      _: Option<Vec<u8>>,
    ) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _msg: Envelope,
      ctx: &mut TurnContext,
    ) -> StepResult {
      ctx.register_name("my_worker".into());
      StepResult::Continue
    }
    fn terminate(&mut self, _: &Reason) {}
  }

  let pid = engine.spawn(Box::new(Registerer));
  engine.send(Envelope::text(pid, "go"));
  engine.tick();

  assert_eq!(engine.whereis("my_worker"), Some(pid));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_turn_context_register`
Expected: FAIL — `RegisterName` variant and methods don't exist.

**Step 3: Add TurnIntent variants and TurnContext methods**

In `zeptovm/src/core/turn_context.rs`, add to `TurnIntent` enum:

```rust
RegisterName(String),
UnregisterName(String),
SendNamed {
  name: String,
  payload: String,
},
```

Add Debug formatting for the new variants in the existing `impl Debug for TurnIntent`:

```rust
TurnIntent::RegisterName(name) => {
  f.debug_tuple("RegisterName")
    .field(name)
    .finish()
}
TurnIntent::UnregisterName(name) => {
  f.debug_tuple("UnregisterName")
    .field(name)
    .finish()
}
TurnIntent::SendNamed { name, payload } => {
  f.debug_struct("SendNamed")
    .field("name", name)
    .field("payload", payload)
    .finish()
}
```

Add methods to `TurnContext`:

```rust
/// Register the current process under a name.
pub fn register_name(&mut self, name: String) {
  self.intents
    .push(TurnIntent::RegisterName(name));
}

/// Unregister a name.
pub fn unregister_name(&mut self, name: String) {
  self.intents
    .push(TurnIntent::UnregisterName(name));
}

/// Send a text message to a named process.
pub fn send_named(
  &mut self,
  name: String,
  text: String,
) {
  self.intents.push(TurnIntent::SendNamed {
    name,
    payload: text,
  });
}
```

**Step 4: Handle new intents in SchedulerEngine::step_process()**

In `zeptovm/src/kernel/scheduler.rs`, add to the intent processing `match`:

```rust
TurnIntent::RegisterName(name) => {
  let _ =
    self.name_registry.register(name, pid);
}
TurnIntent::UnregisterName(name) => {
  self.name_registry.unregister(&name);
}
TurnIntent::SendNamed { name, payload } => {
  if let Some(target) =
    self.name_registry.whereis(&name)
  {
    let mut env =
      Envelope::text(target, payload);
    env.from = Some(pid);
    self.outbound_messages.push(env);
  }
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm`
Expected: All tests pass.

**Step 6: Commit**

```bash
git add zeptovm/src/core/turn_context.rs zeptovm/src/kernel/scheduler.rs
git commit -m "feat(registry): add TurnIntent variants for name registration (X1)"
```

---

### Task 4: SupervisionStrategy enum and SupervisorSpec update

**Files:**
- Modify: `zeptovm/src/core/supervisor.rs`
- Modify: `zeptovm/src/kernel/supervisor_behavior.rs`

**Step 1: Write the failing tests**

Add to `zeptovm/src/core/supervisor.rs` tests:

```rust
#[test]
fn test_supervision_strategy_default_one_for_one() {
  let spec = SupervisorSpec::default();
  assert_eq!(
    spec.strategy,
    SupervisionStrategy::OneForOne
  );
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p zeptovm test_supervision_strategy`
Expected: FAIL — `SupervisionStrategy` and `strategy` field don't exist.

**Step 3: Add SupervisionStrategy enum**

In `zeptovm/src/core/supervisor.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisionStrategy {
  OneForOne,
  OneForAll,
  RestForOne,
}
```

Add `strategy: SupervisionStrategy` to `SupervisorSpec`:

```rust
pub struct SupervisorSpec {
  pub max_restarts: u32,
  pub restart_window_ms: u64,
  pub backoff: BackoffPolicy,
  pub strategy: SupervisionStrategy,
}
```

Update `Default`:

```rust
impl Default for SupervisorSpec {
  fn default() -> Self {
    Self {
      max_restarts: 3,
      restart_window_ms: 5000,
      backoff: BackoffPolicy::Immediate,
      strategy: SupervisionStrategy::OneForOne,
    }
  }
}
```

Fix all existing code that constructs `SupervisorSpec` without `strategy` — add `strategy: SupervisionStrategy::OneForOne` to those call sites (in `supervisor_behavior.rs` tests).

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm supervisor`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add zeptovm/src/core/supervisor.rs zeptovm/src/kernel/supervisor_behavior.rs
git commit -m "feat(supervisor): add SupervisionStrategy enum (G5)"
```

---

### Task 5: SupervisorBehavior — child ordering and OneForAll shutdown

**Files:**
- Modify: `zeptovm/src/kernel/supervisor_behavior.rs`

**Step 1: Write the failing tests**

Add to `zeptovm/src/kernel/supervisor_behavior.rs` tests:

```rust
#[test]
fn test_one_for_all_shutdown_siblings() {
  use crate::core::supervisor::SupervisionStrategy;

  let mut sup = SupervisorBehavior::new(SupervisorSpec {
    max_restarts: 5,
    restart_window_ms: 10000,
    backoff: BackoffPolicy::Immediate,
    strategy: SupervisionStrategy::OneForAll,
  });
  let sup_pid = Pid::from_raw(1);
  let c1 = Pid::from_raw(10);
  let c2 = Pid::from_raw(11);
  let c3 = Pid::from_raw(12);
  sup.register_child(
    "w1".into(), c1, RestartStrategy::Permanent,
  );
  sup.register_child(
    "w2".into(), c2, RestartStrategy::Permanent,
  );
  sup.register_child(
    "w3".into(), c3, RestartStrategy::Permanent,
  );
  sup.init(None);

  // c2 dies
  let mut ctx = TurnContext::new(sup_pid);
  let msg = Envelope::signal(
    sup_pid,
    Signal::ChildExited {
      child_pid: c2,
      reason: Reason::Custom("crash".into()),
    },
  );
  let result = sup.handle(msg, &mut ctx);
  assert!(matches!(result, StepResult::Continue));

  // Should have emitted Shutdown signals for c1 and c3
  let intents = ctx.take_intents();
  let shutdown_count = intents
    .iter()
    .filter(|i| matches!(
      i,
      TurnIntent::SendMessage(env) if matches!(
        &env.payload,
        EnvelopePayload::Signal(Signal::Shutdown)
      )
    ))
    .count();
  assert_eq!(
    shutdown_count, 2,
    "should shutdown 2 siblings"
  );

  assert!(sup.is_shutting_down());
}

#[test]
fn test_one_for_all_restart_after_all_exit() {
  use crate::core::supervisor::SupervisionStrategy;

  let mut sup = SupervisorBehavior::new(SupervisorSpec {
    max_restarts: 5,
    restart_window_ms: 10000,
    backoff: BackoffPolicy::Immediate,
    strategy: SupervisionStrategy::OneForAll,
  });
  let sup_pid = Pid::from_raw(1);
  let c1 = Pid::from_raw(10);
  let c2 = Pid::from_raw(11);
  sup.register_child(
    "w1".into(), c1, RestartStrategy::Permanent,
  );
  sup.register_child(
    "w2".into(), c2, RestartStrategy::Permanent,
  );
  sup.init(None);

  // c1 dies -> triggers OneForAll
  let mut ctx = TurnContext::new(sup_pid);
  let msg = Envelope::signal(
    sup_pid,
    Signal::ChildExited {
      child_pid: c1,
      reason: Reason::Custom("crash".into()),
    },
  );
  sup.handle(msg, &mut ctx);

  // c2 exits (from shutdown signal)
  let mut ctx = TurnContext::new(sup_pid);
  let msg = Envelope::signal(
    sup_pid,
    Signal::ChildExited {
      child_pid: c2,
      reason: Reason::Shutdown,
    },
  );
  sup.handle(msg, &mut ctx);

  // All children exited -> should schedule restarts
  assert!(!sup.is_shutting_down());
  // Check that restart timers were scheduled
  let intents = ctx.take_intents();
  let timer_count = intents
    .iter()
    .filter(|i| matches!(
      i,
      TurnIntent::ScheduleTimer(_)
    ))
    .count();
  assert!(
    timer_count >= 1,
    "should schedule restart timers"
  );
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_one_for_all`
Expected: FAIL — `is_shutting_down()` doesn't exist, OneForAll logic not implemented.

**Step 3: Implement OneForAll in SupervisorBehavior**

Add to `zeptovm/src/kernel/supervisor_behavior.rs`:

Add `use std::collections::HashSet;` to imports.

Add ordering tracking and phase state:

```rust
enum SupervisorPhase {
  Normal,
  ShuttingDown {
    pending_exits: HashSet<Pid>,
    children_to_restart: Vec<String>,
  },
}
```

Add fields to `SupervisorBehavior`:

```rust
pub struct SupervisorBehavior {
  spec: SupervisorSpec,
  children: HashMap<Pid, ChildState>,
  id_to_pid: HashMap<String, Pid>,
  child_order: Vec<String>,  // insertion order
  clock_ms: u64,
  phase: SupervisorPhase,
}
```

Update `new()` to initialize `child_order: Vec::new()` and `phase: SupervisorPhase::Normal`.

Update `register_child()` to push to `child_order`:
```rust
self.child_order.push(child_id.clone());
```

Add method:
```rust
pub fn is_shutting_down(&self) -> bool {
  matches!(self.phase, SupervisorPhase::ShuttingDown { .. })
}
```

Modify `handle()` to branch on `self.spec.strategy`:

For `OneForAll`: when a child exits, send `Signal::Shutdown` to all other living children, enter `ShuttingDown` phase with their pids in `pending_exits` and all child IDs in `children_to_restart`.

During `ShuttingDown` phase: on each `ChildExited`, remove from `pending_exits`. When `pending_exits` is empty, exit `ShuttingDown`, schedule restart timers for all `children_to_restart` in order.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm supervisor_behavior`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/supervisor_behavior.rs
git commit -m "feat(supervisor): implement OneForAll shutdown + restart (G5)"
```

---

### Task 6: RestForOne supervision strategy

**Files:**
- Modify: `zeptovm/src/kernel/supervisor_behavior.rs`

**Step 1: Write the failing tests**

```rust
#[test]
fn test_rest_for_one_only_affects_later_children() {
  use crate::core::supervisor::SupervisionStrategy;

  let mut sup = SupervisorBehavior::new(SupervisorSpec {
    max_restarts: 5,
    restart_window_ms: 10000,
    backoff: BackoffPolicy::Immediate,
    strategy: SupervisionStrategy::RestForOne,
  });
  let sup_pid = Pid::from_raw(1);
  let c1 = Pid::from_raw(10);
  let c2 = Pid::from_raw(11);
  let c3 = Pid::from_raw(12);
  sup.register_child(
    "w1".into(), c1, RestartStrategy::Permanent,
  );
  sup.register_child(
    "w2".into(), c2, RestartStrategy::Permanent,
  );
  sup.register_child(
    "w3".into(), c3, RestartStrategy::Permanent,
  );
  sup.init(None);

  // c2 dies -> should shutdown c3 only (after c2)
  let mut ctx = TurnContext::new(sup_pid);
  let msg = Envelope::signal(
    sup_pid,
    Signal::ChildExited {
      child_pid: c2,
      reason: Reason::Custom("crash".into()),
    },
  );
  sup.handle(msg, &mut ctx);

  let intents = ctx.take_intents();
  let shutdown_targets: Vec<Pid> = intents
    .iter()
    .filter_map(|i| match i {
      TurnIntent::SendMessage(env) => {
        if matches!(
          &env.payload,
          EnvelopePayload::Signal(Signal::Shutdown)
        ) {
          Some(env.to)
        } else {
          None
        }
      }
      _ => None,
    })
    .collect();

  // Only c3 should be shut down (not c1)
  assert_eq!(shutdown_targets.len(), 1);
  assert_eq!(shutdown_targets[0], c3);
}

#[test]
fn test_rest_for_one_first_child_shuts_all() {
  use crate::core::supervisor::SupervisionStrategy;

  let mut sup = SupervisorBehavior::new(SupervisorSpec {
    max_restarts: 5,
    restart_window_ms: 10000,
    backoff: BackoffPolicy::Immediate,
    strategy: SupervisionStrategy::RestForOne,
  });
  let sup_pid = Pid::from_raw(1);
  let c1 = Pid::from_raw(10);
  let c2 = Pid::from_raw(11);
  let c3 = Pid::from_raw(12);
  sup.register_child(
    "w1".into(), c1, RestartStrategy::Permanent,
  );
  sup.register_child(
    "w2".into(), c2, RestartStrategy::Permanent,
  );
  sup.register_child(
    "w3".into(), c3, RestartStrategy::Permanent,
  );
  sup.init(None);

  // c1 dies -> should shutdown c2 and c3
  let mut ctx = TurnContext::new(sup_pid);
  let msg = Envelope::signal(
    sup_pid,
    Signal::ChildExited {
      child_pid: c1,
      reason: Reason::Custom("crash".into()),
    },
  );
  sup.handle(msg, &mut ctx);

  let intents = ctx.take_intents();
  let shutdown_count = intents
    .iter()
    .filter(|i| matches!(
      i,
      TurnIntent::SendMessage(env) if matches!(
        &env.payload,
        EnvelopePayload::Signal(Signal::Shutdown)
      )
    ))
    .count();
  assert_eq!(
    shutdown_count, 2,
    "should shutdown c2 and c3"
  );
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_rest_for_one`
Expected: FAIL — RestForOne branch not implemented yet.

**Step 3: Implement RestForOne**

In the `handle()` method's strategy matching, add `RestForOne` case:

When a child exits under `RestForOne`:
1. Find the failed child's index in `self.child_order`
2. Collect all children whose index is > the failed child's index
3. Send `Signal::Shutdown` to those children
4. Enter `ShuttingDown` phase with those pids + the failed child's ID in `children_to_restart`
5. If no children are after the failed one, restart just the failed child immediately (like OneForOne)

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm supervisor_behavior`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/supervisor_behavior.rs
git commit -m "feat(supervisor): implement RestForOne strategy (G5)"
```

---

### Task 7: Full test suite + gap analysis update

**Step 1: Run all tests**

Run: `cargo test -p zeptovm`
Expected: All tests pass (331 existing + new tests).

**Step 2: Update gap analysis**

Edit `docs/plans/2026-03-06-spec-v03-gap-analysis.md`:
- Change X1 status to `DONE`
- Change G5 status to `DONE`

**Step 3: Commit**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark X1 (name registry) and G5 (supervision strategies) as DONE"
```
