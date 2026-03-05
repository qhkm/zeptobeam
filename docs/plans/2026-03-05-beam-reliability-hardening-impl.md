# BEAM-Level Reliability Hardening Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Harden the zeptoclaw-rt agent runtime with BEAM-inspired reliability across 7 areas: mailbox bounds, supervisor backoff, timeouts/cancellation, dead-letter queue, SQLite recovery, fault injection, and crash isolation verification.

**Architecture:** Layered bottom-up approach. Each layer is built and tested before the next. Mailbox bounds are the foundation (unblocks backpressure signaling), supervisor backoff uses it, timeouts/cancellation needs both, dead-letter queue captures rejects from all layers, SQLite recovery is independent, fault injection tests everything.

**Tech Stack:** Rust nightly, tokio, crossbeam-channel, rusqlite (bundled), tokio-util, tracing (already present)

**Working directory:** `~/ios/zeptoclaw-rt`

**Build command:** `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20`

**Important:** 4 pre-existing doctest failures exist in macros.rs files. Always use `--lib` flag to skip them.

---

### Task 1: Mailbox Bounds — MailboxFull Error Type and deliver_message Return Type

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/process.rs:7-76`
- Test: `lib-erlangrt/src/agent_rt/tests.rs` (add new tests at end)

**Step 1: Write the failing test**

Add at the end of `lib-erlangrt/src/agent_rt/tests.rs`, inside a new test module section:

```rust
#[test]
fn test_mailbox_rejects_when_full() {
  use crate::agent_rt::process::AgentProcess;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(EchoBehavior);
  let mut proc = AgentProcess::new_with_mailbox_capacity(
    behavior,
    serde_json::Value::Null,
    3,
  )
  .unwrap();

  // First 3 should succeed
  assert!(proc.deliver_message(Message::Text("a".into())).is_ok());
  assert!(proc.deliver_message(Message::Text("b".into())).is_ok());
  assert!(proc.deliver_message(Message::Text("c".into())).is_ok());

  // 4th should fail with MailboxFull
  let result = proc.deliver_message(Message::Text("d".into()));
  assert!(result.is_err());
}

#[test]
fn test_mailbox_default_capacity_is_1024() {
  use crate::agent_rt::process::AgentProcess;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(EchoBehavior);
  let proc = AgentProcess::new(behavior, serde_json::Value::Null).unwrap();
  assert_eq!(proc.mailbox_capacity, 1024);
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_mailbox_rejects_when_full 2>&1 | tail -20`

Expected: FAIL — `new_with_mailbox_capacity` doesn't exist, `deliver_message` returns `()` not `Result`.

**Step 3: Implement minimal code**

In `lib-erlangrt/src/agent_rt/process.rs`:

1. Add `MailboxFull` error type above `AgentProcess`:

```rust
/// Error returned when a process mailbox is at capacity.
#[derive(Debug, Clone)]
pub struct MailboxFull;
```

2. Add `mailbox_capacity: usize` field to `AgentProcess` struct (after `mailbox`):

```rust
pub mailbox_capacity: usize,
```

3. Add `pub const DEFAULT_MAILBOX_CAPACITY: usize = 1024;` above `AgentProcess`.

4. In `AgentProcess::new()`, add `mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,` to the struct init.

5. Add `new_with_mailbox_capacity` constructor:

```rust
pub fn new_with_mailbox_capacity(
  behavior: Arc<dyn AgentBehavior>,
  args: serde_json::Value,
  capacity: usize,
) -> Result<Self, Reason> {
  let pid = AgentPid::new();
  let state = behavior.init(args)?;
  Ok(Self {
    pid,
    behavior,
    state: Some(state),
    mailbox: VecDeque::new(),
    mailbox_capacity: capacity,
    reductions: 0,
    status: ProcessStatus::Runnable,
    priority: Priority::Normal,
    links: Vec::new(),
    monitors: Vec::new(),
    monitored_by: Vec::new(),
    supervisor: None,
    trap_exit: false,
    receive_timeout: None,
  })
}
```

6. Change `deliver_message` signature and body:

```rust
pub fn deliver_message(&mut self, msg: Message) -> Result<(), MailboxFull> {
  if self.mailbox.len() >= self.mailbox_capacity {
    return Err(MailboxFull);
  }
  self.mailbox.push_back(msg);
  self.receive_timeout = None;
  if self.status == ProcessStatus::Waiting {
    self.status = ProcessStatus::Runnable;
  }
  Ok(())
}
```

**Step 4: Fix all callers of deliver_message**

Every call to `deliver_message` now returns `Result`. Update these call sites to handle the new return type:

In `lib-erlangrt/src/agent_rt/scheduler.rs`:
- `scheduler.rs` line ~262: `proc.deliver_message(msg);` — change to `proc.deliver_message(msg).ok();` (the `send()` method will handle errors properly in Task 2)
- `scheduler.rs` line ~488-489: bridge error fallback — change to `proc.deliver_message(...).ok();`
- `scheduler.rs` line ~497-498: no bridge error — change to `proc.deliver_message(...).ok();`
- `scheduler.rs` line ~641-643: exit message delivery — change to use `.ok()`

**Step 5: Run test to verify it passes**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_mailbox 2>&1 | tail -20`

Expected: Both mailbox tests PASS.

**Step 6: Run full test suite**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20`

Expected: All tests PASS (no regressions from `.ok()` calls).

**Step 7: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/src/agent_rt/process.rs lib-erlangrt/src/agent_rt/scheduler.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat: add mailbox capacity bounds with MailboxFull rejection

deliver_message now returns Result<(), MailboxFull>. Default capacity
is 1024. new_with_mailbox_capacity constructor for custom limits."
```

---

### Task 2: Mailbox Bounds — Scheduler send() Returns MailboxFull Error

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs:258-286`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_scheduler_send_returns_error_on_full_mailbox() {
  let behavior: Arc<dyn AgentBehavior> = Arc::new(EchoBehavior);
  let mut sched = AgentScheduler::new();

  // Spawn process with capacity 2
  let pid = {
    let proc = AgentProcess::new_with_mailbox_capacity(
      behavior.clone(),
      serde_json::Value::Null,
      2,
    )
    .unwrap();
    let pid = proc.pid;
    sched.registry.processes_mut().insert(pid, proc);
    pid
  };
  sched.enqueue(pid);

  assert!(sched.send(pid, Message::Text("a".into())).is_ok());
  assert!(sched.send(pid, Message::Text("b".into())).is_ok());
  // 3rd send should fail
  let result = sched.send(pid, Message::Text("c".into()));
  assert!(result.is_err());
  assert!(result.unwrap_err().contains("mailbox full"));
}
```

Note: We need `processes_mut()` on the registry. If it doesn't exist, add a `pub fn processes_mut(&mut self) -> &mut HashMap<AgentPid, AgentProcess>` to `AgentRegistry` — OR spawn normally and manually set mailbox_capacity. Let's use the simpler approach: spawn normally then set the capacity:

```rust
#[test]
fn test_scheduler_send_returns_error_on_full_mailbox() {
  let behavior: Arc<dyn AgentBehavior> = Arc::new(EchoBehavior);
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.enqueue(pid);

  // Set tiny capacity
  sched.registry.lookup_mut(&pid).unwrap().mailbox_capacity = 2;

  assert!(sched.send(pid, Message::Text("a".into())).is_ok());
  assert!(sched.send(pid, Message::Text("b".into())).is_ok());
  // 3rd should fail
  let result = sched.send(pid, Message::Text("c".into()));
  assert!(result.is_err());
  assert!(result.unwrap_err().contains("mailbox full"));
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_scheduler_send_returns_error_on_full_mailbox 2>&1 | tail -20`

Expected: FAIL — `send()` currently uses `.ok()` and never returns the error.

**Step 3: Implement — update `send()` in scheduler.rs**

Replace the `send()` method body (around line 258-286) to propagate `MailboxFull`:

```rust
pub fn send(&mut self, to: AgentPid, msg: Message) -> Result<(), String> {
  let msg_kind = message_kind(&msg);
  let was_waiting = if let Some(proc) = self.registry.lookup_mut(&to) {
    let was = proc.status == ProcessStatus::Waiting;
    if let Err(_) = proc.deliver_message(msg) {
      return Err(format!("mailbox full for {:?}", to));
    }
    was
  } else if self.allow_external_routing {
    self.outbound_messages.push((to, msg));
    debug!(
      to = to.raw(),
      msg_kind = msg_kind,
      "scheduler queued outbound message for external routing"
    );
    return Ok(());
  } else {
    return Err(format!("Process {:?} not found", to));
  };
  self.metrics.record_message_sent();
  debug!(
    to = to.raw(),
    msg_kind = msg_kind,
    woke_waiting = was_waiting,
    "scheduler send"
  );
  if was_waiting {
    self.enqueue(to);
  }
  Ok(())
}
```

**Step 4: Run test to verify it passes**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_scheduler_send_returns_error_on_full_mailbox 2>&1 | tail -20`

Expected: PASS

**Step 5: Run full test suite**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20`

Expected: All PASS.

**Step 6: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/src/agent_rt/scheduler.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat: scheduler send() returns MailboxFull error

Propagates mailbox capacity rejection to callers instead of
silently dropping via .ok()."
```

---

### Task 3: Mailbox Bounds — Metrics Counter for Mailbox Rejections

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/observability.rs:46-60,80-82`
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs` (record metric on rejection)
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_mailbox_rejection_increments_metric() {
  use crate::agent_rt::observability::RuntimeMetrics;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(EchoBehavior);
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.enqueue(pid);
  sched.registry.lookup_mut(&pid).unwrap().mailbox_capacity = 1;

  let _ = sched.send(pid, Message::Text("a".into()));
  // This one should be rejected
  let _ = sched.send(pid, Message::Text("b".into()));

  let snap = sched.metrics_snapshot();
  assert_eq!(snap.mailbox_rejections_total, 1);
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_mailbox_rejection_increments_metric 2>&1 | tail -20`

Expected: FAIL — `mailbox_rejections_total` field doesn't exist.

**Step 3: Implement**

1. In `observability.rs`, add to `RuntimeMetrics` struct:
```rust
mailbox_rejections_total: AtomicU64,
```

2. In `RuntimeMetrics::new()`, add:
```rust
mailbox_rejections_total: AtomicU64::new(0),
```

3. Add method:
```rust
pub fn record_mailbox_rejection(&self) {
  self.mailbox_rejections_total.fetch_add(1, Ordering::Relaxed);
}
```

4. In `RuntimeMetricsSnapshot`, add:
```rust
pub mailbox_rejections_total: u64,
```

5. In `snapshot()`, add:
```rust
mailbox_rejections_total: self.mailbox_rejections_total.load(Ordering::Relaxed),
```

6. In `scheduler.rs` `send()` method, where the `MailboxFull` error is returned, add metric recording before the return:
```rust
if let Err(_) = proc.deliver_message(msg) {
  self.metrics.record_mailbox_rejection();
  return Err(format!("mailbox full for {:?}", to));
}
```

**Step 4: Run test to verify it passes**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_mailbox_rejection 2>&1 | tail -20`

Expected: PASS

**Step 5: Run full test suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/observability.rs lib-erlangrt/src/agent_rt/scheduler.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat: add mailbox_rejections_total metric counter

Tracks number of messages rejected due to full mailbox."
```

---

### Task 4: Supervisor Tests — OneForOne Restart

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/tests.rs`

The existing `test_full_supervisor_lifecycle` partially tests OneForOne, but calls `handle_child_exit` manually. We need a test that verifies only the crashed child restarts (not siblings).

**Step 1: Write the failing test**

```rust
#[test]
fn test_supervisor_one_for_one_only_restarts_crashed_child() {
  use crate::agent_rt::supervision::*;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![
      ChildSpec {
        id: "a".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      },
      ChildSpec {
        id: "b".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      },
    ],
  };

  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();

  let pid_a = sup.children[0].pid;
  let pid_b = sup.children[1].pid;

  // Crash child A
  sup.handle_child_exit(&mut sched, pid_a, Reason::Custom("crash".into()));

  // Child A should have new PID
  assert_ne!(sup.children[0].pid, pid_a);
  // Child B should be UNCHANGED
  assert_eq!(sup.children[1].pid, pid_b);
  // B should still be alive in registry
  assert!(sched.registry.lookup(&pid_b).is_some());
}
```

**Step 2: Run test to verify it passes (this validates existing behavior)**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_supervisor_one_for_one_only_restarts_crashed_child 2>&1 | tail -20`

Expected: PASS (this validates the existing OneForOne implementation works correctly).

**Step 3: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test: add OneForOne supervisor test — only crashed child restarts"
```

---

### Task 5: Supervisor Tests — RestForOne and Transient/Temporary Restart Policies

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write the tests**

```rust
#[test]
fn test_supervisor_rest_for_one_restarts_crashed_and_later() {
  use crate::agent_rt::supervision::*;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });
  let spec = SupervisorSpec {
    strategy: RestartStrategy::RestForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![
      ChildSpec {
        id: "a".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      },
      ChildSpec {
        id: "b".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      },
      ChildSpec {
        id: "c".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      },
    ],
  };

  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();

  let pid_a = sup.children[0].pid;
  let pid_b = sup.children[1].pid;
  let pid_c = sup.children[2].pid;

  // Crash child B — should restart B and C, leave A alone
  sup.handle_child_exit(&mut sched, pid_b, Reason::Custom("crash".into()));

  assert_eq!(sup.children[0].pid, pid_a, "A should be unchanged");
  assert_ne!(sup.children[1].pid, pid_b, "B should have new PID");
  assert_ne!(sup.children[2].pid, pid_c, "C should have new PID");
}

#[test]
fn test_supervisor_transient_does_not_restart_on_normal_exit() {
  use crate::agent_rt::supervision::*;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "trans".to_string(),
      behavior: behavior.clone(),
      args: serde_json::Value::Null,
      restart: ChildRestart::Transient,
      priority: Priority::Normal,
    }],
  };

  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();
  let pid = sup.children[0].pid;

  // Normal exit — transient should NOT restart
  sup.handle_child_exit(&mut sched, pid, Reason::Normal);
  assert!(sup.children.is_empty(), "Transient child removed on normal exit");

  // Abnormal exit — transient SHOULD restart
  // Need a fresh supervisor for this
  let behavior2: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });
  let spec2 = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "trans2".to_string(),
      behavior: behavior2.clone(),
      args: serde_json::Value::Null,
      restart: ChildRestart::Transient,
      priority: Priority::Normal,
    }],
  };
  let mut sched2 = AgentScheduler::new();
  let mut sup2 = Supervisor::start(&mut sched2, spec2).unwrap();
  let pid2 = sup2.children[0].pid;

  sup2.handle_child_exit(&mut sched2, pid2, Reason::Custom("crash".into()));
  assert_eq!(sup2.children.len(), 1, "Transient child restarted on crash");
  assert_ne!(sup2.children[0].pid, pid2);
}

#[test]
fn test_supervisor_temporary_never_restarts() {
  use crate::agent_rt::supervision::*;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "temp".to_string(),
      behavior: behavior.clone(),
      args: serde_json::Value::Null,
      restart: ChildRestart::Temporary,
      priority: Priority::Normal,
    }],
  };

  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();
  let pid = sup.children[0].pid;

  // Even abnormal exit — temporary NEVER restarts
  sup.handle_child_exit(&mut sched, pid, Reason::Custom("crash".into()));
  assert!(sup.children.is_empty(), "Temporary child never restarts");
}
```

**Step 2: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_supervisor_rest_for_one test_supervisor_transient test_supervisor_temporary 2>&1 | tail -20`

Expected: All PASS (validating existing correct implementation).

**Step 3: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test: add RestForOne, Transient, Temporary supervisor tests"
```

---

### Task 6: Supervisor BackoffStrategy — Add Enum and Wire into SupervisorSpec

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/supervision.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_supervisor_backoff_exponential_calculates_delay() {
  use crate::agent_rt::supervision::*;

  let backoff = BackoffStrategy::Exponential {
    base_ms: 100,
    max_ms: 5000,
    multiplier: 2.0,
  };

  assert_eq!(backoff.delay_for_attempt(0), 100);   // 100 * 2^0
  assert_eq!(backoff.delay_for_attempt(1), 200);   // 100 * 2^1
  assert_eq!(backoff.delay_for_attempt(2), 400);   // 100 * 2^2
  assert_eq!(backoff.delay_for_attempt(6), 5000);  // capped at max_ms
  assert_eq!(backoff.delay_for_attempt(100), 5000); // stays capped
}

#[test]
fn test_supervisor_backoff_fixed_delay() {
  use crate::agent_rt::supervision::*;

  let backoff = BackoffStrategy::Fixed { delay_ms: 500 };
  assert_eq!(backoff.delay_for_attempt(0), 500);
  assert_eq!(backoff.delay_for_attempt(10), 500);
}

#[test]
fn test_supervisor_backoff_none_is_zero() {
  use crate::agent_rt::supervision::*;

  let backoff = BackoffStrategy::None;
  assert_eq!(backoff.delay_for_attempt(0), 0);
  assert_eq!(backoff.delay_for_attempt(5), 0);
}
```

**Step 2: Run test to verify it fails**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_supervisor_backoff 2>&1 | tail -20`

Expected: FAIL — `BackoffStrategy` doesn't exist.

**Step 3: Implement**

In `lib-erlangrt/src/agent_rt/supervision.rs`, add after `ChildRestart` enum:

```rust
/// Strategy for delaying restarts to prevent restart storms.
#[derive(Debug, Clone)]
pub enum BackoffStrategy {
  /// No delay between restarts (current default behavior).
  None,
  /// Fixed delay between every restart.
  Fixed { delay_ms: u64 },
  /// Exponential backoff: base_ms * multiplier^attempt, capped at max_ms.
  Exponential {
    base_ms: u64,
    max_ms: u64,
    multiplier: f64,
  },
}

impl BackoffStrategy {
  /// Calculate the delay in milliseconds for a given restart attempt (0-indexed).
  pub fn delay_for_attempt(&self, attempt: u32) -> u64 {
    match self {
      BackoffStrategy::None => 0,
      BackoffStrategy::Fixed { delay_ms } => *delay_ms,
      BackoffStrategy::Exponential {
        base_ms,
        max_ms,
        multiplier,
      } => {
        let delay = (*base_ms as f64) * multiplier.powi(attempt as i32);
        let delay = delay.min(*max_ms as f64) as u64;
        delay.min(*max_ms)
      }
    }
  }
}

impl Default for BackoffStrategy {
  fn default() -> Self {
    BackoffStrategy::None
  }
}
```

Add `backoff` field to `SupervisorSpec`:

```rust
pub struct SupervisorSpec {
  pub strategy: RestartStrategy,
  pub max_restarts: u32,
  pub max_seconds: u32,
  pub children: Vec<ChildSpec>,
  pub backoff: BackoffStrategy,
}
```

Add `backoff` and `restart_counts` fields to `Supervisor`:

```rust
pub struct Supervisor {
  pub strategy: RestartStrategy,
  pub max_restarts: u32,
  pub max_seconds: u32,
  pub children: Vec<RunningChild>,
  restart_timestamps: Vec<Instant>,
  shutdown: bool,
  backoff: BackoffStrategy,
  restart_counts: HashMap<String, u32>,
}
```

(Add `use std::collections::HashMap;` at top if not already imported.)

Update `Supervisor::start()` to initialize the new fields:

```rust
backoff: spec.backoff,
restart_counts: HashMap::new(),
```

Update all existing `SupervisorSpec` construction sites in tests to add `backoff: BackoffStrategy::None` (or `backoff: BackoffStrategy::default()`).

**Step 4: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_supervisor_backoff 2>&1 | tail -20`

Expected: All 3 PASS.

**Step 5: Run full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/supervision.rs lib-erlangrt/src/agent_rt/tests.rs lib-erlangrt/src/agent_rt/integration_tests.rs
git commit -m "feat: add BackoffStrategy enum to supervisor

Supports None, Fixed, and Exponential backoff with delay_for_attempt().
Wired into SupervisorSpec and Supervisor state."
```

---

### Task 7: Supervisor Backoff — Track restart_count per child and compute delay

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/supervision.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_supervisor_backoff_tracks_restart_count_per_child() {
  use crate::agent_rt::supervision::*;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 10,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "crashy".to_string(),
      behavior: behavior.clone(),
      args: serde_json::Value::Null,
      restart: ChildRestart::Permanent,
      priority: Priority::Normal,
    }],
    backoff: BackoffStrategy::Exponential {
      base_ms: 100,
      max_ms: 5000,
      multiplier: 2.0,
    },
  };

  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();

  // First crash — attempt 0, delay should be 100ms
  let pid0 = sup.children[0].pid;
  let delay0 = sup.pending_restart_delay("crashy");
  assert_eq!(delay0, 100, "first restart delay should be base_ms");

  sup.handle_child_exit(&mut sched, pid0, Reason::Custom("crash".into()));

  // Second crash — attempt 1, delay should be 200ms
  let pid1 = sup.children[0].pid;
  let delay1 = sup.pending_restart_delay("crashy");
  assert_eq!(delay1, 200, "second restart delay should be 200");

  sup.handle_child_exit(&mut sched, pid1, Reason::Custom("crash".into()));

  // Third crash — attempt 2, delay should be 400ms
  let delay2 = sup.pending_restart_delay("crashy");
  assert_eq!(delay2, 400);
}
```

**Step 2: Run test to verify it fails**

Expected: FAIL — `pending_restart_delay` method doesn't exist.

**Step 3: Implement**

Add a public method to `Supervisor`:

```rust
/// Returns the backoff delay (in ms) for the next restart of a child.
/// This does NOT increment the counter — it's a read-only query.
pub fn pending_restart_delay(&self, child_id: &str) -> u64 {
  let attempt = self.restart_counts.get(child_id).copied().unwrap_or(0);
  self.backoff.delay_for_attempt(attempt)
}
```

In `restart_one()`, increment the restart count for the child:

```rust
fn restart_one(&mut self, sched: &mut AgentScheduler, idx: usize) {
  let dead = self.children.remove(idx);
  let count = self.restart_counts.entry(dead.id.clone()).or_insert(0);
  *count += 1;
  if let Some(new) = self.spawn_child(sched, &dead.spec) {
    self.children.insert(idx, new);
  }
}
```

Similarly update `restart_all()` and `restart_rest()` to increment counts for restarted children.

**Step 4: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_supervisor_backoff_tracks 2>&1 | tail -20`

Expected: PASS.

**Step 5: Full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/supervision.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat: track per-child restart count for backoff delay

Supervisor increments restart_counts on each restart.
pending_restart_delay() computes delay from BackoffStrategy."
```

---

### Task 8: Supervisor — Escalation on max_restarts Exceeded

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/tests.rs`

This test validates existing behavior — the supervisor already shuts down when `exceeds_max_restarts()` returns true. But we need a test that explicitly verifies the escalation (shutdown flag set to true).

**Step 1: Write the test**

```rust
#[test]
fn test_supervisor_escalates_on_intensity_exceeded() {
  use crate::agent_rt::supervision::*;

  let behavior: Arc<dyn AgentBehavior> = Arc::new(CounterBehavior { stop_at: 1000 });
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 2,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "crashy".to_string(),
      behavior: behavior.clone(),
      args: serde_json::Value::Null,
      restart: ChildRestart::Permanent,
      priority: Priority::Normal,
    }],
    backoff: BackoffStrategy::None,
  };

  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();

  // 3 rapid crashes should exceed max_restarts=2
  for i in 0..3 {
    if sup.is_shutdown() {
      break;
    }
    let pid = sup.children[0].pid;
    sup.handle_child_exit(&mut sched, pid, Reason::Custom(format!("crash_{}", i)));
  }

  assert!(sup.is_shutdown(), "Supervisor should escalate after exceeding intensity");
}
```

**Step 2: Run, verify passes, commit**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_supervisor_escalates 2>&1 | tail -20`

Expected: PASS.

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test: verify supervisor escalation on intensity limit exceeded"
```

---

### Task 9: Dead-Letter Queue — Core Module

**Files:**
- Create: `lib-erlangrt/src/agent_rt/dead_letter.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write the failing test**

In the new `dead_letter.rs` file, include an inline test module:

```rust
use std::collections::VecDeque;

use crate::agent_rt::types::{AgentPid, Message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadLetterReason {
  MailboxFull,
  ProcessNotFound,
  ProcessTerminated,
}

#[derive(Debug, Clone)]
pub struct DeadLetter {
  pub timestamp: std::time::Instant,
  pub sender_pid: Option<AgentPid>,
  pub target_pid: AgentPid,
  pub reason: DeadLetterReason,
}

pub struct DeadLetterQueue {
  buffer: VecDeque<DeadLetter>,
  capacity: usize,
  total_count: u64,
}

impl DeadLetterQueue {
  pub fn new(capacity: usize) -> Self {
    Self {
      buffer: VecDeque::with_capacity(capacity),
      capacity,
      total_count: 0,
    }
  }

  pub fn push(&mut self, letter: DeadLetter) {
    if self.buffer.len() >= self.capacity {
      self.buffer.pop_front();
    }
    self.buffer.push_back(letter);
    self.total_count += 1;
  }

  pub fn recent(&self) -> &VecDeque<DeadLetter> {
    &self.buffer
  }

  pub fn total_count(&self) -> u64 {
    self.total_count
  }

  pub fn len(&self) -> usize {
    self.buffer.len()
  }

  pub fn is_empty(&self) -> bool {
    self.buffer.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_dead_letter_queue_basic() {
    let mut dlq = DeadLetterQueue::new(3);
    assert!(dlq.is_empty());
    assert_eq!(dlq.total_count(), 0);

    let pid = AgentPid::from_raw(1);
    for i in 0..5 {
      dlq.push(DeadLetter {
        timestamp: std::time::Instant::now(),
        sender_pid: None,
        target_pid: AgentPid::from_raw(i),
        reason: DeadLetterReason::ProcessNotFound,
      });
    }

    // Ring buffer: only last 3 kept
    assert_eq!(dlq.len(), 3);
    // Total count tracks all pushes
    assert_eq!(dlq.total_count(), 5);
    // Oldest in buffer should be pid 2
    assert_eq!(dlq.recent()[0].target_pid.raw(), 2);
  }
}
```

**Step 2: Register the module**

In `lib-erlangrt/src/agent_rt/mod.rs`, add:
```rust
pub mod dead_letter;
```

**Step 3: Run test**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_dead_letter_queue_basic 2>&1 | tail -20`

Expected: PASS (we're writing code and test together since the module is new).

**Step 4: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/src/agent_rt/dead_letter.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat: add DeadLetterQueue bounded ring buffer module

256-entry ring buffer with DeadLetterReason enum. Tracks total
count and keeps most recent entries."
```

---

### Task 10: Dead-Letter Queue — Wire into Scheduler

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_scheduler_routes_to_dead_letter_on_full_mailbox() {
  let behavior: Arc<dyn AgentBehavior> = Arc::new(EchoBehavior);
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.enqueue(pid);
  sched.registry.lookup_mut(&pid).unwrap().mailbox_capacity = 1;

  let _ = sched.send(pid, Message::Text("a".into()));
  // This one should be rejected and go to DLQ
  let _ = sched.send(pid, Message::Text("b".into()));

  assert_eq!(sched.dead_letter_queue().total_count(), 1);
  assert_eq!(
    sched.dead_letter_queue().recent()[0].reason,
    crate::agent_rt::dead_letter::DeadLetterReason::MailboxFull
  );
}

#[test]
fn test_scheduler_routes_to_dead_letter_on_unknown_pid() {
  let mut sched = AgentScheduler::new();
  let fake_pid = AgentPid::from_raw(0xDEAD);

  let result = sched.send(fake_pid, Message::Text("hello".into()));
  assert!(result.is_err());
  assert_eq!(sched.dead_letter_queue().total_count(), 1);
  assert_eq!(
    sched.dead_letter_queue().recent()[0].reason,
    crate::agent_rt::dead_letter::DeadLetterReason::ProcessNotFound
  );
}
```

**Step 2: Run test to verify it fails**

Expected: FAIL — `dead_letter_queue()` method doesn't exist on scheduler.

**Step 3: Implement**

1. Add to `AgentScheduler` struct:
```rust
dead_letters: DeadLetterQueue,
```

2. In `AgentScheduler::new()`:
```rust
dead_letters: DeadLetterQueue::new(256),
```

3. Add accessor:
```rust
pub fn dead_letter_queue(&self) -> &DeadLetterQueue {
  &self.dead_letters
}
```

4. In `send()`, on `MailboxFull`:
```rust
if let Err(_) = proc.deliver_message(msg) {
  self.metrics.record_mailbox_rejection();
  self.dead_letters.push(DeadLetter {
    timestamp: std::time::Instant::now(),
    sender_pid: None,
    target_pid: to,
    reason: DeadLetterReason::MailboxFull,
  });
  return Err(format!("mailbox full for {:?}", to));
}
```

5. In `send()`, on process not found (the `else` branch that returns error):
```rust
} else {
  self.dead_letters.push(DeadLetter {
    timestamp: std::time::Instant::now(),
    sender_pid: None,
    target_pid: to,
    reason: DeadLetterReason::ProcessNotFound,
  });
  return Err(format!("Process {:?} not found", to));
}
```

6. Add necessary imports at top of scheduler.rs:
```rust
use crate::agent_rt::dead_letter::{DeadLetter, DeadLetterQueue, DeadLetterReason};
```

**Step 4: Run tests, full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_scheduler_routes_to_dead_letter 2>&1 | tail -20
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/scheduler.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat: wire DeadLetterQueue into scheduler

Routes MailboxFull and ProcessNotFound messages to DLQ ring buffer."
```

---

### Task 11: IO Timeouts — Add Default Timeout to Bridge AgentChat

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/bridge.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

The bridge already handles `timeout_ms` on `AgentChat` (line 481-491 of bridge.rs). But when `timeout_ms` is `None`, there's no default — it runs forever. We need to enforce a default.

**Step 1: Write the failing test**

```rust
#[tokio::test]
async fn test_bridge_agent_chat_applies_default_timeout() {
  use crate::agent_rt::bridge::{create_bridge, DEFAULT_AGENT_CHAT_TIMEOUT_MS};
  // Just verify the constant exists and is reasonable
  assert!(DEFAULT_AGENT_CHAT_TIMEOUT_MS > 0);
  assert!(DEFAULT_AGENT_CHAT_TIMEOUT_MS <= 300_000); // max 5 min
}
```

**Step 2: Run to verify failure**

Expected: FAIL — `DEFAULT_AGENT_CHAT_TIMEOUT_MS` doesn't exist.

**Step 3: Implement**

In `bridge.rs`, add constants:

```rust
pub const DEFAULT_AGENT_CHAT_TIMEOUT_MS: u64 = 120_000; // 2 minutes
pub const DEFAULT_HTTP_TIMEOUT_MS: u64 = 30_000; // 30 seconds
```

In `execute_agent_chat_standalone`, change the timeout logic (around line 481):

```rust
let effective_timeout = timeout_ms.unwrap_or(DEFAULT_AGENT_CHAT_TIMEOUT_MS);
let chat_result = match tokio::time::timeout(
  Duration::from_millis(effective_timeout),
  &mut chat_task,
).await {
  Ok(join_result) => join_result,
  Err(_) => {
    chat_task.abort();
    return IoResult::Timeout;
  }
};
```

This replaces the `if let Some(ms)` conditional — now ALL agent chats have a timeout.

**Step 4: Run tests, full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_bridge_agent_chat_applies_default_timeout 2>&1 | tail -20
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/bridge.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat: enforce default 120s timeout on all AgentChat operations

No agent chat can run indefinitely. DEFAULT_AGENT_CHAT_TIMEOUT_MS = 120s."
```

---

### Task 12: Cancellation Tokens — Add tokio-util Dependency

**Files:**
- Modify: `lib-erlangrt/Cargo.toml`

**Step 1: Add dependency**

Add to `[dependencies]` in `lib-erlangrt/Cargo.toml`:

```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

**Step 2: Verify build**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly check -p erlangrt 2>&1 | tail -10`

Expected: Compiles successfully.

**Step 3: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/Cargo.toml
git commit -m "deps: add tokio-util for CancellationToken support"
```

---

### Task 13: Cancellation Tokens — Wire into Bridge In-Flight Tracking

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/bridge.rs`
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write the failing test**

```rust
#[test]
fn test_bridge_handle_cancel_operation() {
  use crate::agent_rt::bridge::create_bridge;

  let (mut handle, _worker) = create_bridge();
  let pid = AgentPid::from_raw(0xCAFE);

  // Submit an IO op
  let cid = handle
    .submit(pid, IoOp::Timer { duration: std::time::Duration::from_secs(60) })
    .unwrap();

  // Cancel it
  let cancelled = handle.cancel(pid);
  assert!(cancelled, "should return true when pid had in-flight ops");

  // Cancel again — no ops
  let cancelled2 = handle.cancel(pid);
  assert!(!cancelled2, "should return false when no in-flight ops");
}
```

**Step 2: Verify failure**

Expected: FAIL — `cancel()` method doesn't exist on `BridgeHandle`.

**Step 3: Implement**

1. Add to `BridgeHandle`:
```rust
use tokio_util::sync::CancellationToken;
use std::collections::HashMap;

// Add field:
cancellation_tokens: Arc<std::sync::Mutex<HashMap<AgentPid, CancellationToken>>>,
```

2. In `create_bridge_pool()`, initialize:
```rust
let tokens = Arc::new(std::sync::Mutex::new(HashMap::new()));
```
Pass to handle and clone for each worker access.

3. In `BridgeHandle::submit()`, create a token per PID:
```rust
let token = {
  let mut tokens = self.cancellation_tokens.lock().unwrap();
  tokens.entry(pid).or_insert_with(CancellationToken::new).clone()
};
```

4. Add `cancel()` method:
```rust
pub fn cancel(&mut self, pid: AgentPid) -> bool {
  let mut tokens = self.cancellation_tokens.lock().unwrap();
  if let Some(token) = tokens.remove(&pid) {
    token.cancel();
    true
  } else {
    false
  }
}
```

5. In the bridge worker's IO task (the `in_flight.spawn(async move { ... })` block), wrap execution with `tokio::select!` on the token:
```rust
tokio::select! {
  result = execute_op => { /* send response */ }
  _ = token.cancelled() => {
    let resp = IoResponse {
      correlation_id: req.correlation_id,
      source_pid: req.source_pid,
      result: IoResult::Timeout,
    };
    let _ = tx.send(resp);
  }
}
```

6. In `scheduler.rs` `terminate_process()`, before removing from registry, cancel bridge operations:
```rust
if let Some(ref mut bridge) = self.bridge {
  bridge.cancel(pid);
}
```

**Step 4: Run tests, full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_bridge_handle_cancel 2>&1 | tail -20
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/bridge.rs lib-erlangrt/src/agent_rt/scheduler.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat: add CancellationToken to bridge for process termination

When a process is terminated, scheduler cancels its in-flight IO ops
via bridge.cancel(pid). Prevents orphaned agent chats."
```

---

### Task 14: SQLite Checkpoint Store — Add rusqlite Dependency

**Files:**
- Modify: `lib-erlangrt/Cargo.toml`

**Step 1: Add dependency**

```toml
rusqlite = { version = "0.31", features = ["bundled"] }
```

**Step 2: Verify build**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly check -p erlangrt 2>&1 | tail -10`

Expected: Compiles (bundled SQLite builds from source, may take a minute).

**Step 3: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/Cargo.toml
git commit -m "deps: add rusqlite with bundled SQLite for checkpoint store"
```

---

### Task 15: SQLite Checkpoint Store — Implementation

**Files:**
- Create: `lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write the failing test (in the new file)**

```rust
use std::sync::Mutex;

use rusqlite::Connection;

use crate::agent_rt::checkpoint::CheckpointStore;

pub struct SqliteCheckpointStore {
  conn: Mutex<Connection>,
}

impl SqliteCheckpointStore {
  pub fn open(path: &str) -> Result<Self, String> {
    let conn =
      Connection::open(path).map_err(|e| format!("sqlite open failed: {}", e))?;
    conn
      .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
      .map_err(|e| format!("sqlite pragma failed: {}", e))?;
    conn
      .execute(
        "CREATE TABLE IF NOT EXISTS checkpoints (
          key TEXT PRIMARY KEY,
          data BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL
        )",
        [],
      )
      .map_err(|e| format!("sqlite create table failed: {}", e))?;
    Ok(Self {
      conn: Mutex::new(conn),
    })
  }

  pub fn in_memory() -> Result<Self, String> {
    Self::open(":memory:")
  }
}

impl CheckpointStore for SqliteCheckpointStore {
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), String> {
    let data = serde_json::to_vec(checkpoint)
      .map_err(|e| format!("serialize failed: {}", e))?;
    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs() as i64;
    let conn = self
      .conn
      .lock()
      .map_err(|_| "sqlite mutex poisoned".to_string())?;
    conn
      .execute(
        "INSERT OR REPLACE INTO checkpoints (key, data, created_at, updated_at)
         VALUES (?1, ?2, COALESCE((SELECT created_at FROM checkpoints WHERE key = ?1), ?3), ?3)",
        rusqlite::params![key, data, now],
      )
      .map_err(|e| format!("sqlite insert failed: {}", e))?;
    Ok(())
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| "sqlite mutex poisoned".to_string())?;
    let mut stmt = conn
      .prepare("SELECT data FROM checkpoints WHERE key = ?1")
      .map_err(|e| format!("sqlite prepare failed: {}", e))?;
    let result = stmt
      .query_row(rusqlite::params![key], |row| {
        let data: Vec<u8> = row.get(0)?;
        Ok(data)
      })
      .optional()
      .map_err(|e| format!("sqlite query failed: {}", e))?;
    match result {
      Some(data) => {
        let value = serde_json::from_slice(&data)
          .map_err(|e| format!("deserialize failed: {}", e))?;
        Ok(Some(value))
      }
      None => Ok(None),
    }
  }

  fn delete(&self, key: &str) -> Result<(), String> {
    let conn = self
      .conn
      .lock()
      .map_err(|_| "sqlite mutex poisoned".to_string())?;
    conn
      .execute("DELETE FROM checkpoints WHERE key = ?1", rusqlite::params![key])
      .map_err(|e| format!("sqlite delete failed: {}", e))?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_sqlite_checkpoint_roundtrip() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    let key = "test-key";
    let payload = serde_json::json!({
      "goal": "test",
      "tasks": [1, 2, 3],
    });

    store.save(key, &payload).unwrap();
    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded, payload);

    store.delete(key).unwrap();
    assert!(store.load(key).unwrap().is_none());
  }

  #[test]
  fn test_sqlite_checkpoint_upsert_preserves_created_at() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    let key = "upsert-key";

    store
      .save(key, &serde_json::json!({"v": 1}))
      .unwrap();
    store
      .save(key, &serde_json::json!({"v": 2}))
      .unwrap();

    let loaded = store.load(key).unwrap().unwrap();
    assert_eq!(loaded["v"], 2);
  }

  #[test]
  fn test_sqlite_checkpoint_load_missing_key() {
    let store = SqliteCheckpointStore::in_memory().unwrap();
    assert!(store.load("nonexistent").unwrap().is_none());
  }
}
```

**Step 2: Register module in mod.rs**

Add to `lib-erlangrt/src/agent_rt/mod.rs`:
```rust
pub mod checkpoint_sqlite;
```

**Step 3: Add `use rusqlite::OptionalExtension;` at top of the file** (needed for `.optional()` on query_row).

**Step 4: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_sqlite_checkpoint 2>&1 | tail -20`

Expected: All 3 PASS.

**Step 5: Full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/checkpoint_sqlite.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat: add SqliteCheckpointStore with WAL mode

Implements CheckpointStore trait using rusqlite bundled SQLite.
Upsert preserves created_at. In-memory constructor for tests."
```

---

### Task 16: Fault Injection — FaultConfig and FaultyCheckpointStore

**Files:**
- Create: `lib-erlangrt/src/agent_rt/chaos.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write test + implementation (new module)**

```rust
use std::sync::atomic::{AtomicU64, Ordering};

/// Configuration for fault injection.
#[derive(Debug, Clone)]
pub struct FaultConfig {
  pub fail_rate: f64,
  pub seed: u64,
  pub modes: Vec<FaultMode>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FaultMode {
  Error,
  Timeout,
  CorruptData,
}

impl FaultConfig {
  /// Deterministic check: should this attempt fail?
  /// Uses a simple linear congruential generator seeded with
  /// `seed + attempt` for reproducibility.
  pub fn should_fail(&self, attempt: u64) -> bool {
    if self.fail_rate <= 0.0 {
      return false;
    }
    if self.fail_rate >= 1.0 {
      return true;
    }
    let hash = self.seed.wrapping_mul(6364136223846793005).wrapping_add(attempt);
    let normalized = (hash % 10000) as f64 / 10000.0;
    normalized < self.fail_rate
  }

  pub fn pick_mode(&self, attempt: u64) -> &FaultMode {
    if self.modes.is_empty() {
      return &FaultMode::Error;
    }
    &self.modes[(attempt as usize) % self.modes.len()]
  }
}

/// A CheckpointStore wrapper that randomly fails operations.
#[cfg(test)]
pub struct FaultyCheckpointStore<S: crate::agent_rt::checkpoint::CheckpointStore> {
  inner: S,
  config: FaultConfig,
  attempt: AtomicU64,
}

#[cfg(test)]
impl<S: crate::agent_rt::checkpoint::CheckpointStore> FaultyCheckpointStore<S> {
  pub fn new(inner: S, config: FaultConfig) -> Self {
    Self {
      inner,
      config,
      attempt: AtomicU64::new(0),
    }
  }
}

#[cfg(test)]
impl<S: crate::agent_rt::checkpoint::CheckpointStore> crate::agent_rt::checkpoint::CheckpointStore
  for FaultyCheckpointStore<S>
{
  fn save(&self, key: &str, checkpoint: &serde_json::Value) -> Result<(), String> {
    let attempt = self.attempt.fetch_add(1, Ordering::Relaxed);
    if self.config.should_fail(attempt) {
      return Err("chaos: injected save failure".into());
    }
    self.inner.save(key, checkpoint)
  }

  fn load(&self, key: &str) -> Result<Option<serde_json::Value>, String> {
    let attempt = self.attempt.fetch_add(1, Ordering::Relaxed);
    if self.config.should_fail(attempt) {
      let mode = self.config.pick_mode(attempt);
      if *mode == FaultMode::CorruptData {
        return Ok(Some(serde_json::json!({"corrupted": true})));
      }
      return Err("chaos: injected load failure".into());
    }
    self.inner.load(key)
  }

  fn delete(&self, key: &str) -> Result<(), String> {
    let attempt = self.attempt.fetch_add(1, Ordering::Relaxed);
    if self.config.should_fail(attempt) {
      return Err("chaos: injected delete failure".into());
    }
    self.inner.delete(key)
  }
}

/// Runtime chaos configuration, loaded from environment.
/// Only compiled when the `chaos_testing` feature is enabled.
#[cfg(feature = "chaos_testing")]
pub struct ChaosConfig {
  pub enabled: bool,
  pub fail_rate: f64,
  pub seed: u64,
  pub modes: Vec<FaultMode>,
}

#[cfg(feature = "chaos_testing")]
impl ChaosConfig {
  pub fn from_env() -> Self {
    let enabled = std::env::var("ZEPTOCLAW_CHAOS_ENABLED")
      .ok()
      .map(|v| v == "true" || v == "1")
      .unwrap_or(false);
    let fail_rate = std::env::var("ZEPTOCLAW_CHAOS_FAIL_RATE")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(0.05);
    let seed = std::env::var("ZEPTOCLAW_CHAOS_SEED")
      .ok()
      .and_then(|v| v.parse().ok())
      .unwrap_or(42);
    let modes = std::env::var("ZEPTOCLAW_CHAOS_MODES")
      .ok()
      .map(|v| {
        v.split(',')
          .filter_map(|s| match s.trim() {
            "error" => Some(FaultMode::Error),
            "timeout" => Some(FaultMode::Timeout),
            "corrupt" => Some(FaultMode::CorruptData),
            _ => None,
          })
          .collect()
      })
      .unwrap_or_else(|| vec![FaultMode::Error]);
    Self {
      enabled,
      fail_rate,
      seed,
      modes,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::checkpoint::{CheckpointStore, InMemoryCheckpointStore};

  #[test]
  fn test_fault_config_deterministic() {
    let config = FaultConfig {
      fail_rate: 0.5,
      seed: 42,
      modes: vec![FaultMode::Error],
    };
    // Same seed + attempt should give same result
    let r1 = config.should_fail(0);
    let r2 = config.should_fail(0);
    assert_eq!(r1, r2, "deterministic with same seed+attempt");
  }

  #[test]
  fn test_fault_config_zero_rate_never_fails() {
    let config = FaultConfig {
      fail_rate: 0.0,
      seed: 42,
      modes: vec![FaultMode::Error],
    };
    for i in 0..100 {
      assert!(!config.should_fail(i));
    }
  }

  #[test]
  fn test_fault_config_full_rate_always_fails() {
    let config = FaultConfig {
      fail_rate: 1.0,
      seed: 42,
      modes: vec![FaultMode::Error],
    };
    for i in 0..100 {
      assert!(config.should_fail(i));
    }
  }

  #[test]
  fn test_faulty_checkpoint_store_injects_failures() {
    let inner = InMemoryCheckpointStore::new();
    let store = FaultyCheckpointStore::new(
      inner,
      FaultConfig {
        fail_rate: 1.0, // always fail
        seed: 0,
        modes: vec![FaultMode::Error],
      },
    );
    let result = store.save("key", &serde_json::json!({"v": 1}));
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("chaos"));
  }

  #[test]
  fn test_faulty_checkpoint_store_passes_when_no_fault() {
    let inner = InMemoryCheckpointStore::new();
    let store = FaultyCheckpointStore::new(
      inner,
      FaultConfig {
        fail_rate: 0.0, // never fail
        seed: 0,
        modes: vec![FaultMode::Error],
      },
    );
    store
      .save("key", &serde_json::json!({"v": 1}))
      .unwrap();
    let loaded = store.load("key").unwrap().unwrap();
    assert_eq!(loaded["v"], 1);
  }
}
```

**Step 2: Register module and feature**

In `lib-erlangrt/src/agent_rt/mod.rs`:
```rust
pub mod chaos;
```

In `lib-erlangrt/Cargo.toml`, add feature:
```toml
chaos_testing = []
```

**Step 3: Run tests**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_fault_config test_faulty_checkpoint 2>&1 | tail -20`

Expected: All PASS.

**Step 4: Full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/chaos.rs lib-erlangrt/src/agent_rt/mod.rs lib-erlangrt/Cargo.toml
git commit -m "feat: add chaos/fault injection framework

FaultConfig with deterministic seeded failure rates.
FaultyCheckpointStore wrapper for test-time fault injection.
ChaosConfig from env vars behind chaos_testing feature flag."
```

---

### Task 17: Crash Isolation Verification Tests

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write verification tests**

```rust
#[test]
fn test_crash_isolation_panic_does_not_affect_siblings() {
  struct PanicBehavior;

  impl AgentBehavior for PanicBehavior {
    fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(EchoState { received: vec![] }))
    }
    fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
      panic!("intentional panic for crash isolation test");
    }
    fn handle_exit(&self, _: AgentPid, _: Reason, _: &mut dyn AgentState) -> Action {
      Action::Continue
    }
    fn terminate(&self, _: Reason, _: &mut dyn AgentState) {}
  }

  let mut sched = AgentScheduler::new();

  // Spawn panicking process and a healthy sibling
  let panic_pid = sched
    .registry
    .spawn(Arc::new(PanicBehavior), serde_json::Value::Null)
    .unwrap();
  let healthy_pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();

  // Send messages to both
  sched.send(panic_pid, Message::Text("boom".into())).unwrap();
  sched.send(healthy_pid, Message::Text("ok".into())).unwrap();
  sched.enqueue(panic_pid);
  sched.enqueue(healthy_pid);

  // Tick: panicking process should be terminated
  sched.tick();
  // Tick: healthy process should run fine
  sched.tick();

  // Panicking process terminated
  assert!(
    sched.registry.lookup(&panic_pid).is_none(),
    "panicking process should be removed"
  );
  // Healthy process still alive
  assert!(
    sched.registry.lookup(&healthy_pid).is_some(),
    "healthy sibling should still be alive"
  );
}
```

**Step 2: Run test**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt test_crash_isolation_panic 2>&1 | tail -20`

Expected: PASS.

**Step 3: Full suite, commit**

```bash
cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test: verify crash isolation — panic doesn't affect siblings

Spawns a panicking and healthy process side by side. Confirms
scheduler catch_unwind contains the panic, terminates only the
panicking process."
```

---

### Task 18: Observability — Add tracing Spans to Key Operations

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs`
- Modify: `lib-erlangrt/src/agent_rt/supervision.rs`

**Step 1: Add structured tracing events**

`tracing` is already imported in scheduler.rs and bridge.rs. Add targeted `tracing::info_span!` and `tracing::warn!` calls:

In `scheduler.rs` `terminate_process()`:
```rust
use tracing::info_span;
let _span = info_span!("process.terminate", pid = pid.raw(), ?reason).entered();
```

In `supervision.rs`, at the top of `handle_child_exit()`:
```rust
use tracing::{info, warn};
info!(
  child_pid = pid.raw(),
  ?reason,
  strategy = ?self.strategy,
  "supervisor.child_exit"
);
```

In `restart_one()`:
```rust
info!(
  child_id = dead.id,
  old_pid = dead.pid.raw(),
  strategy = "one_for_one",
  "supervisor.restart"
);
```

**Step 2: Verify compilation**

Run: `cd ~/ios/zeptoclaw-rt && cargo +nightly test --lib -p erlangrt 2>&1 | tail -20`

Expected: All PASS (tracing is no-op without a subscriber in tests).

**Step 3: Commit**

```bash
cd ~/ios/zeptoclaw-rt
git add lib-erlangrt/src/agent_rt/scheduler.rs lib-erlangrt/src/agent_rt/supervision.rs
git commit -m "feat: add structured tracing spans to scheduler and supervisor

Spans for process.terminate, supervisor.child_exit, supervisor.restart.
Compatible with any tracing subscriber."
```

---

## Summary

| Task | Area | What |
|------|------|------|
| 1-3 | Mailbox Bounds | MailboxFull error, capacity field, scheduler integration, metrics |
| 4-5 | Supervisor Tests | OneForOne, RestForOne, Transient, Temporary validation |
| 6-8 | Supervisor Backoff | BackoffStrategy enum, per-child tracking, escalation test |
| 9-10 | Dead-Letter Queue | Ring buffer module, scheduler wiring |
| 11 | IO Timeouts | Default 120s timeout on all AgentChat |
| 12-13 | Cancellation | tokio-util dep, CancellationToken in bridge |
| 14-15 | SQLite Recovery | rusqlite dep, SqliteCheckpointStore |
| 16 | Fault Injection | FaultConfig, FaultyCheckpointStore, chaos_testing feature |
| 17 | Crash Isolation | Verification tests for existing catch_unwind |
| 18 | Observability | Tracing spans on scheduler and supervisor |
