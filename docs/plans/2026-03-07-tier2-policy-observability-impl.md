# Tier 2 — Policy Engine + Structured Observability Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add effect-kind policy gates (G3) and structured observability event bus (X4) to ZeptoVM.

**Architecture:** PolicyEngine evaluates per-EffectKind rules at the budget-check point in runtime.rs, returning Allow/Deny. EventBus is a ring buffer + tracing dual-write collecting lifecycle events across the runtime. Both integrate into SchedulerRuntime.

**Tech Stack:** Rust, existing ZeptoVM kernel (runtime, reactor, scheduler, metrics)

---

### Task 1: PolicyEngine struct with evaluation logic

**Files:**
- Create: `zeptovm/src/control/policy.rs`
- Modify: `zeptovm/src/control/mod.rs`

**Step 1: Write the failing tests**

Create `zeptovm/src/control/policy.rs` with tests first:

```rust
use crate::core::effect::EffectKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
  Allow,
  Deny(String),
}

pub struct PolicyRule {
  pub kind: EffectKind,
  pub decision: PolicyDecision,
  pub max_cost_microdollars: Option<u64>,
}

pub struct PolicyEngine {
  rules: Vec<PolicyRule>,
  default: PolicyDecision,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_allow_no_rules() {
    let engine = PolicyEngine::new(
      vec![],
      PolicyDecision::Allow,
    );
    let decision = engine.evaluate(
      &EffectKind::LlmCall, None,
    );
    assert_eq!(decision, PolicyDecision::Allow);
  }

  #[test]
  fn test_deny_by_kind() {
    let engine = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::DbWrite,
        decision: PolicyDecision::Deny(
          "writes not allowed".into(),
        ),
        max_cost_microdollars: None,
      }],
      PolicyDecision::Allow,
    );
    assert_eq!(
      engine.evaluate(&EffectKind::DbWrite, None),
      PolicyDecision::Deny("writes not allowed".into()),
    );
    assert_eq!(
      engine.evaluate(&EffectKind::LlmCall, None),
      PolicyDecision::Allow,
    );
  }

  #[test]
  fn test_cost_threshold_deny() {
    let engine = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::LlmCall,
        decision: PolicyDecision::Allow,
        max_cost_microdollars: Some(5_000_000),
      }],
      PolicyDecision::Allow,
    );
    // Under threshold: allow
    assert_eq!(
      engine.evaluate(
        &EffectKind::LlmCall, Some(1_000_000),
      ),
      PolicyDecision::Allow,
    );
    // Over threshold: deny
    let decision = engine.evaluate(
      &EffectKind::LlmCall, Some(10_000_000),
    );
    assert!(matches!(decision, PolicyDecision::Deny(_)));
  }

  #[test]
  fn test_first_match_wins() {
    let engine = PolicyEngine::new(
      vec![
        PolicyRule {
          kind: EffectKind::LlmCall,
          decision: PolicyDecision::Deny(
            "rule 1".into(),
          ),
          max_cost_microdollars: None,
        },
        PolicyRule {
          kind: EffectKind::LlmCall,
          decision: PolicyDecision::Allow,
          max_cost_microdollars: None,
        },
      ],
      PolicyDecision::Allow,
    );
    assert_eq!(
      engine.evaluate(&EffectKind::LlmCall, None),
      PolicyDecision::Deny("rule 1".into()),
    );
  }

  #[test]
  fn test_default_deny() {
    let engine = PolicyEngine::new(
      vec![],
      PolicyDecision::Deny("default deny".into()),
    );
    assert_eq!(
      engine.evaluate(&EffectKind::Http, None),
      PolicyDecision::Deny("default deny".into()),
    );
  }

  #[test]
  fn test_cost_unavailable_skips_cost_check() {
    let engine = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::LlmCall,
        decision: PolicyDecision::Allow,
        max_cost_microdollars: Some(5_000_000),
      }],
      PolicyDecision::Allow,
    );
    // No cost estimate: rule matches by kind, cost
    // check skipped, allow
    assert_eq!(
      engine.evaluate(&EffectKind::LlmCall, None),
      PolicyDecision::Allow,
    );
  }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm policy`
Expected: FAIL — `PolicyEngine::new()` and `evaluate()` don't exist yet.

**Step 3: Implement PolicyEngine**

Add above the `#[cfg(test)]` block:

```rust
impl PolicyEngine {
  pub fn new(
    rules: Vec<PolicyRule>,
    default: PolicyDecision,
  ) -> Self {
    Self { rules, default }
  }

  /// Evaluate policy for an effect kind with optional
  /// estimated cost.
  pub fn evaluate(
    &self,
    kind: &EffectKind,
    estimated_cost: Option<u64>,
  ) -> PolicyDecision {
    for rule in &self.rules {
      if &rule.kind == kind {
        // Check cost threshold if both rule and
        // estimate have one
        if let (Some(max), Some(est)) =
          (rule.max_cost_microdollars, estimated_cost)
        {
          if est > max {
            return PolicyDecision::Deny(format!(
              "cost {} exceeds limit {}",
              est, max,
            ));
          }
        }
        return rule.decision.clone();
      }
    }
    self.default.clone()
  }
}
```

**Step 4: Register module**

Add `pub mod policy;` to `zeptovm/src/control/mod.rs`.

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm policy`
Expected: All 6 tests pass.

**Step 6: Commit**

```bash
git add zeptovm/src/control/policy.rs zeptovm/src/control/mod.rs
git commit -m "feat(policy): add PolicyEngine with effect-kind rules (G3)"
```

---

### Task 2: RuntimeEvent enum and EventBus

**Files:**
- Create: `zeptovm/src/kernel/event_bus.rs`
- Modify: `zeptovm/src/kernel/mod.rs`

**Step 1: Write the failing tests**

Create `zeptovm/src/kernel/event_bus.rs`:

```rust
use std::collections::VecDeque;

use crate::core::effect::EffectKind;
use crate::error::Reason;
use crate::pid::Pid;

/// Structured lifecycle event emitted by the runtime.
/// Flat enum — add/remove variants freely.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
  ProcessSpawned {
    pid: Pid,
    behavior_module: String,
  },
  ProcessExited {
    pid: Pid,
    reason: Reason,
  },
  EffectRequested {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
  },
  EffectDispatched {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
  },
  EffectCompleted {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
    status: String,
  },
  EffectBlocked {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
    reason: String,
  },
  PolicyEvaluated {
    pid: Pid,
    effect_id: u64,
    kind: EffectKind,
    decision: String,
  },
  SupervisorRestart {
    supervisor_pid: Pid,
    child_id: String,
    strategy: String,
  },
  BudgetExhausted {
    pid: Pid,
    tokens_used: u64,
    limit: u64,
  },
}

/// Ring buffer event bus with tracing dual-write.
pub struct EventBus {
  buffer: VecDeque<(u64, RuntimeEvent)>,
  capacity: usize,
  clock_ms: u64,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_emit_and_drain() {
    let mut bus = EventBus::new(100);
    bus.emit(RuntimeEvent::ProcessSpawned {
      pid: Pid::from_raw(1),
      behavior_module: "Echo".into(),
    });
    bus.emit(RuntimeEvent::ProcessExited {
      pid: Pid::from_raw(1),
      reason: Reason::Normal,
    });
    let events = bus.drain_events();
    assert_eq!(events.len(), 2);
    assert!(matches!(
      &events[0].1,
      RuntimeEvent::ProcessSpawned { .. }
    ));
    assert!(matches!(
      &events[1].1,
      RuntimeEvent::ProcessExited { .. }
    ));
  }

  #[test]
  fn test_drain_clears_buffer() {
    let mut bus = EventBus::new(100);
    bus.emit(RuntimeEvent::ProcessSpawned {
      pid: Pid::from_raw(1),
      behavior_module: "Echo".into(),
    });
    let events = bus.drain_events();
    assert_eq!(events.len(), 1);
    let events2 = bus.drain_events();
    assert_eq!(events2.len(), 0);
  }

  #[test]
  fn test_ring_buffer_eviction() {
    let mut bus = EventBus::new(3);
    for i in 0..5 {
      bus.emit(RuntimeEvent::ProcessSpawned {
        pid: Pid::from_raw(i),
        behavior_module: "Echo".into(),
      });
    }
    // Only last 3 should remain
    let events = bus.drain_events();
    assert_eq!(events.len(), 3);
    if let RuntimeEvent::ProcessSpawned { pid, .. } =
      &events[0].1
    {
      assert_eq!(pid.raw(), 2);
    }
  }

  #[test]
  fn test_recent_without_drain() {
    let mut bus = EventBus::new(100);
    for i in 0..5 {
      bus.emit(RuntimeEvent::ProcessSpawned {
        pid: Pid::from_raw(i),
        behavior_module: "Echo".into(),
      });
    }
    let recent = bus.recent(2);
    assert_eq!(recent.len(), 2);
    // Should be the last 2 events
    if let RuntimeEvent::ProcessSpawned { pid, .. } =
      &recent[0].1
    {
      assert_eq!(pid.raw(), 3);
    }
    // Buffer should still have all 5
    assert_eq!(bus.drain_events().len(), 5);
  }

  #[test]
  fn test_recent_more_than_buffer() {
    let mut bus = EventBus::new(100);
    bus.emit(RuntimeEvent::ProcessSpawned {
      pid: Pid::from_raw(1),
      behavior_module: "Echo".into(),
    });
    let recent = bus.recent(10);
    assert_eq!(recent.len(), 1);
  }

  #[test]
  fn test_event_count() {
    let mut bus = EventBus::new(100);
    assert_eq!(bus.event_count(), 0);
    bus.emit(RuntimeEvent::ProcessSpawned {
      pid: Pid::from_raw(1),
      behavior_module: "Echo".into(),
    });
    assert_eq!(bus.event_count(), 1);
  }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm event_bus`
Expected: FAIL — `EventBus::new()`, `emit()`, etc. don't exist.

**Step 3: Implement EventBus**

Add above `#[cfg(test)]`:

```rust
impl EventBus {
  pub fn new(capacity: usize) -> Self {
    Self {
      buffer: VecDeque::with_capacity(capacity),
      capacity,
      clock_ms: 0,
    }
  }

  /// Set the current clock (called by runtime).
  pub fn set_clock(&mut self, ms: u64) {
    self.clock_ms = ms;
  }

  /// Emit an event: push to ring buffer + tracing.
  pub fn emit(&mut self, event: RuntimeEvent) {
    // Evict oldest if at capacity
    if self.buffer.len() >= self.capacity {
      self.buffer.pop_front();
    }
    // Tracing dual-write
    tracing::info!(
      event = ?event,
      "runtime_event"
    );
    self.buffer.push_back((self.clock_ms, event));
  }

  /// Drain all buffered events.
  pub fn drain_events(
    &mut self,
  ) -> Vec<(u64, RuntimeEvent)> {
    self.buffer.drain(..).collect()
  }

  /// Peek the last N events without draining.
  pub fn recent(
    &self,
    n: usize,
  ) -> Vec<(u64, RuntimeEvent)> {
    let len = self.buffer.len();
    let start = len.saturating_sub(n);
    self.buffer
      .iter()
      .skip(start)
      .cloned()
      .collect()
  }

  /// Number of buffered events.
  pub fn event_count(&self) -> usize {
    self.buffer.len()
  }
}
```

**Step 4: Register module**

Add `pub mod event_bus;` to `zeptovm/src/kernel/mod.rs`.

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm event_bus`
Expected: All 6 tests pass.

**Step 6: Commit**

```bash
git add zeptovm/src/kernel/event_bus.rs zeptovm/src/kernel/mod.rs
git commit -m "feat(observability): add RuntimeEvent enum and EventBus (X4)"
```

---

### Task 3: Integrate PolicyEngine into SchedulerRuntime

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing tests**

Add to `zeptovm/src/kernel/runtime.rs` tests:

```rust
#[test]
fn test_runtime_policy_denies_effect() {
  use crate::control::policy::{
    PolicyDecision, PolicyEngine, PolicyRule,
  };

  let policy = PolicyEngine::new(
    vec![PolicyRule {
      kind: EffectKind::LlmCall,
      decision: PolicyDecision::Deny(
        "LLM calls blocked".into(),
      ),
      max_cost_microdollars: None,
    }],
    PolicyDecision::Allow,
  );
  let mut rt = SchedulerRuntime::new()
    .with_policy(policy);
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick(); // Process suspends with LlmCall
  rt.tick(); // Process handles failed effect result

  let completed = rt.take_completed();
  assert_eq!(completed.len(), 1);
  assert!(matches!(completed[0].1, Reason::Normal));
}

#[test]
fn test_runtime_policy_allows_effect() {
  use crate::control::policy::{
    PolicyDecision, PolicyEngine, PolicyRule,
  };

  let policy = PolicyEngine::new(
    vec![PolicyRule {
      kind: EffectKind::DbWrite,
      decision: PolicyDecision::Deny(
        "writes blocked".into(),
      ),
      max_cost_microdollars: None,
    }],
    PolicyDecision::Allow,
  );
  // LlmCall not blocked — only DbWrite is
  let mut rt = SchedulerRuntime::new()
    .with_policy(policy);
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();

  // No violations — LlmCall was allowed
  let violations = rt.take_budget_violations();
  assert_eq!(violations.len(), 0);
}

#[test]
fn test_runtime_no_policy_allows_all() {
  // Without a policy engine, all effects pass
  let mut rt = SchedulerRuntime::new();
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();
  let violations = rt.take_budget_violations();
  assert_eq!(violations.len(), 0);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_runtime_policy`
Expected: FAIL — `with_policy()` doesn't exist.

**Step 3: Integrate PolicyEngine**

In `zeptovm/src/kernel/runtime.rs`:

Add import:
```rust
use crate::control::policy::{
  PolicyDecision, PolicyEngine,
};
```

Add field to `SchedulerRuntime`:
```rust
policy: Option<PolicyEngine>,
```

Initialize to `None` in both `new()` and `with_durability()`.

Add builder method:
```rust
pub fn with_policy(
  mut self,
  policy: PolicyEngine,
) -> Self {
  self.policy = Some(policy);
  self
}
```

In `tick()` step 7, insert policy check BEFORE the budget check (after idempotency check at line 409, before line 411). Add:

```rust
// Policy check
if let Some(ref policy) = self.policy {
  let decision = policy.evaluate(
    &req.kind, None,
  );
  if let PolicyDecision::Deny(reason) = decision {
    warn!(
      pid = %pid,
      kind = ?req.kind,
      reason = %reason,
      "effect blocked by policy"
    );
    let result = EffectResult::failure(
      req.effect_id,
      format!("policy denied: {}", reason),
    );
    self.engine.deliver_effect_result(result);
    continue;
  }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(policy): integrate PolicyEngine into SchedulerRuntime (G3)"
```

---

### Task 4: Integrate EventBus into SchedulerRuntime

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing tests**

Add to `zeptovm/src/kernel/runtime.rs` tests:

```rust
#[test]
fn test_runtime_event_bus_process_lifecycle() {
  let mut rt = SchedulerRuntime::new();
  let pid = rt.spawn(Box::new(Stopper));
  rt.send(Envelope::text(pid, "stop"));
  rt.tick();
  let _ = rt.take_completed();

  let events = rt.drain_events();
  assert!(
    events.iter().any(|(_, e)| matches!(
      e,
      crate::kernel::event_bus::RuntimeEvent::ProcessSpawned { .. }
    )),
    "should have ProcessSpawned event"
  );
  assert!(
    events.iter().any(|(_, e)| matches!(
      e,
      crate::kernel::event_bus::RuntimeEvent::ProcessExited { .. }
    )),
    "should have ProcessExited event"
  );
}

#[test]
fn test_runtime_event_bus_effect_lifecycle() {
  let mut rt = SchedulerRuntime::new();
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();

  let events = rt.drain_events();
  assert!(
    events.iter().any(|(_, e)| matches!(
      e,
      crate::kernel::event_bus::RuntimeEvent::EffectRequested { .. }
    )),
    "should have EffectRequested event"
  );
  assert!(
    events.iter().any(|(_, e)| matches!(
      e,
      crate::kernel::event_bus::RuntimeEvent::EffectDispatched { .. }
    )),
    "should have EffectDispatched event"
  );
}

#[test]
fn test_runtime_event_bus_policy_blocked() {
  use crate::control::policy::{
    PolicyDecision, PolicyEngine, PolicyRule,
  };

  let policy = PolicyEngine::new(
    vec![PolicyRule {
      kind: EffectKind::LlmCall,
      decision: PolicyDecision::Deny(
        "blocked".into(),
      ),
      max_cost_microdollars: None,
    }],
    PolicyDecision::Allow,
  );
  let mut rt = SchedulerRuntime::new()
    .with_policy(policy);
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();

  let events = rt.drain_events();
  assert!(
    events.iter().any(|(_, e)| matches!(
      e,
      crate::kernel::event_bus::RuntimeEvent::PolicyEvaluated { .. }
    )),
    "should have PolicyEvaluated event"
  );
  assert!(
    events.iter().any(|(_, e)| matches!(
      e,
      crate::kernel::event_bus::RuntimeEvent::EffectBlocked { .. }
    )),
    "should have EffectBlocked event"
  );
}

#[test]
fn test_runtime_drain_events_empty_after_drain() {
  let mut rt = SchedulerRuntime::new();
  rt.spawn(Box::new(Echo));
  let events = rt.drain_events();
  assert!(!events.is_empty());
  let events2 = rt.drain_events();
  assert!(events2.is_empty());
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm test_runtime_event_bus`
Expected: FAIL — `drain_events()` doesn't exist on runtime.

**Step 3: Add EventBus to SchedulerRuntime and emit events**

In `zeptovm/src/kernel/runtime.rs`:

Add import:
```rust
use crate::kernel::event_bus::{EventBus, RuntimeEvent};
```

Add field to `SchedulerRuntime`:
```rust
event_bus: EventBus,
```

Initialize in both `new()` and `with_durability()`:
```rust
event_bus: EventBus::new(10_000),
```

Add public methods:
```rust
/// Drain all buffered runtime events.
pub fn drain_events(
  &mut self,
) -> Vec<(u64, RuntimeEvent)> {
  self.event_bus.drain_events()
}

/// Peek recent events without draining.
pub fn recent_events(
  &self,
  n: usize,
) -> Vec<(u64, RuntimeEvent)> {
  self.event_bus.recent(n)
}
```

Add event emissions at these points:

**In `spawn()`** (after `self.engine.spawn(behavior)`):
```rust
self.event_bus.emit(RuntimeEvent::ProcessSpawned {
  pid,
  behavior_module: "unknown".into(),
});
```

**In `take_completed()`** (inside the `for` loop):
```rust
for (pid, reason) in &completed {
  self.metrics.inc("processes.exited");
  self.event_bus.emit(RuntimeEvent::ProcessExited {
    pid: *pid,
    reason: reason.clone(),
  });
}
```

**In `tick()` step 7**, for each effect `(pid, req)`:

After the idempotency check, emit EffectRequested:
```rust
self.event_bus.emit(
  RuntimeEvent::EffectRequested {
    pid,
    effect_id: req.effect_id.raw(),
    kind: req.kind.clone(),
  },
);
```

In the policy deny branch, emit PolicyEvaluated + EffectBlocked:
```rust
self.event_bus.emit(
  RuntimeEvent::PolicyEvaluated {
    pid,
    effect_id: req.effect_id.raw(),
    kind: req.kind.clone(),
    decision: format!("Deny: {}", reason),
  },
);
self.event_bus.emit(
  RuntimeEvent::EffectBlocked {
    pid,
    effect_id: req.effect_id.raw(),
    kind: req.kind.clone(),
    reason: reason.clone(),
  },
);
```

In the budget blocked branch, emit EffectBlocked:
```rust
self.event_bus.emit(
  RuntimeEvent::EffectBlocked {
    pid,
    effect_id: req.effect_id.raw(),
    kind: req.kind.clone(),
    reason: "budget exceeded".into(),
  },
);
```

In the dispatch branch (before `reactor.dispatch()`), emit EffectDispatched:
```rust
self.event_bus.emit(
  RuntimeEvent::EffectDispatched {
    pid,
    effect_id: req.effect_id.raw(),
    kind: req.kind.clone(),
  },
);
```

In step 1 (drain reactor completions), for non-streaming completions, emit EffectCompleted:
```rust
self.event_bus.emit(
  RuntimeEvent::EffectCompleted {
    pid: completion_pid, // need to capture pid
    effect_id: completion.result.effect_id.raw(),
    kind: EffectKind::Custom("unknown".into()),
    status: format!("{:?}", completion.result.status),
  },
);
```

Note: The reactor completion doesn't carry the EffectKind, so use `"unknown"` for now. This can be improved later by storing kind in pending_effects.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/runtime.rs
git commit -m "feat(observability): integrate EventBus into SchedulerRuntime (X4)"
```

---

### Task 5: Add metrics counter for policy blocks

**Files:**
- Modify: `zeptovm/src/kernel/metrics.rs`
- Modify: `zeptovm/src/kernel/runtime.rs`

**Step 1: Write the failing test**

Add to `zeptovm/src/kernel/runtime.rs` tests:

```rust
#[test]
fn test_runtime_metrics_policy_blocked() {
  use crate::control::policy::{
    PolicyDecision, PolicyEngine, PolicyRule,
  };

  let policy = PolicyEngine::new(
    vec![PolicyRule {
      kind: EffectKind::LlmCall,
      decision: PolicyDecision::Deny(
        "blocked".into(),
      ),
      max_cost_microdollars: None,
    }],
    PolicyDecision::Allow,
  );
  let mut rt = SchedulerRuntime::new()
    .with_policy(policy);
  let pid = rt.spawn(Box::new(LlmCaller));
  rt.send(Envelope::text(pid, "go"));
  rt.tick();
  assert_eq!(
    rt.metrics().counter("policy.blocked"),
    1,
  );
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p zeptovm test_runtime_metrics_policy`
Expected: FAIL — `"policy.blocked"` counter not pre-registered.

**Step 3: Add policy.blocked counter**

In `zeptovm/src/kernel/metrics.rs`, add `"policy.blocked"` to the pre-registered counter list (after `"budget.blocked"`).

In `zeptovm/src/kernel/runtime.rs`, in the policy deny branch, add:
```rust
self.metrics.inc("policy.blocked");
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/metrics.rs zeptovm/src/kernel/runtime.rs
git commit -m "feat(metrics): add policy.blocked counter (G3+X4)"
```

---

### Task 6: Full test suite + gap analysis update

**Step 1: Run all tests**

Run: `cargo test -p zeptovm`
Expected: All tests pass (354 existing + new tests).

**Step 2: Update gap analysis**

Edit `docs/plans/2026-03-06-spec-v03-gap-analysis.md`:
- Change G3 status from `NOT DONE` to `DONE`
- Change X4 status from `NOT DONE` to `DONE`

Update G3 notes to: `PolicyEngine with effect-kind rules, integrated into runtime`
Update X4 notes to: `RuntimeEvent enum + EventBus ring buffer with tracing dual-write`

**Step 3: Commit**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark G3 (policy engine) and X4 (structured observability) as DONE"
```
