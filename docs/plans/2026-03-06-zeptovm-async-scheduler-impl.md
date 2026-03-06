# ZeptoVM Async-Aware Scheduler Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the Tokio-based cooperative process model with a BEAM-inspired preemptive scheduler adapted for async I/O workloads.

**Architecture:** N scheduler threads each own a local run queue of ready processes. Processes are state machines (not tokio tasks) stepped by the scheduler. A shared Tokio runtime handles all async I/O — when a process needs I/O, it returns `Suspend(future)` and the scheduler hands the future to the reactor. Crossbeam channels connect scheduler threads to the reactor. Work stealing balances load across cores.

**Tech Stack:** Rust (nightly), crossbeam-deque (work-stealing queues), crossbeam-channel (scheduler-reactor communication), tokio (reactor only), dashmap, tracing

**Design Doc:** `docs/plans/2026-03-06-zeptovm-async-scheduler-design.md`

---

## Existing Codebase Reference

| File | Current Role | Scheduler Impact |
|------|-------------|-----------------|
| `src/behavior.rs` | `async fn handle() -> Action` | New step-based trait: `fn handle() -> StepResult` |
| `src/process.rs` | `tokio::spawn` per process | State machine in run queue |
| `src/mailbox.rs` | tokio mpsc channels | VecDeque, scheduler pushes directly |
| `src/error.rs` | Reason, Action, Message, SystemMsg | Add StepResult, IoResult |
| `src/pid.rs` | Atomic u64 counter | Stays as-is |
| `src/link.rs` | LinkTable | Stays as-is |
| `src/registry.rs` | DashMap + tokio JoinHandle | Adapt to scheduler-based spawn |
| `src/supervisor.rs` | tokio::spawn supervision loop | Later: supervisor as process |
| `src/durability.rs` | Not yet created | Deferred to v4 |
| `Cargo.toml` | tokio, async-trait, dashmap | Add crossbeam-deque, crossbeam-channel |

---

## Task 1: Add Crossbeam Dependencies

**Files:**
- Modify: `zeptovm/Cargo.toml`

**Step 1: Add crossbeam-deque and crossbeam-channel to dependencies**

```toml
# Add after the futures line in [dependencies]:
crossbeam-deque = "0.8"
crossbeam-channel = "0.5"
```

**Step 2: Verify build**

Run: `cd zeptovm && cargo build`
Expected: BUILD SUCCESS

**Step 3: Run existing tests to confirm no regression**

Run: `cd zeptovm && cargo test`
Expected: All existing tests pass

**Step 4: Commit**

```bash
git add zeptovm/Cargo.toml
git commit -m "chore(zeptovm): add crossbeam-deque and crossbeam-channel dependencies"
```

---

## Task 2: Add StepResult and IoResult Types

**Files:**
- Modify: `zeptovm/src/error.rs`
- Test: `zeptovm/src/error.rs` (inline tests)

**Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `error.rs`:

```rust
#[test]
fn test_step_result_variants() {
    let cont = StepResult::Continue;
    assert!(matches!(cont, StepResult::Continue));

    let wait = StepResult::Wait;
    assert!(matches!(wait, StepResult::Wait));

    let done = StepResult::Done(Reason::Normal);
    assert!(matches!(done, StepResult::Done(Reason::Normal)));
}

#[test]
fn test_io_result_send() {
    fn assert_send<T: Send>() {}
    assert_send::<IoResult>();
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib error::tests::test_step_result_variants`
Expected: FAIL — `StepResult` not found

**Step 3: Write minimal implementation**

Add to `error.rs` before the `#[cfg(test)]` block:

```rust
use std::future::Future;
use std::pin::Pin;

/// Result type for async I/O operations submitted to the reactor.
pub type IoResult = Result<Message, String>;

/// Future that the reactor will poll on behalf of a suspended process.
pub type IoFuture = Pin<Box<dyn Future<Output = IoResult> + Send>>;

/// Result of a single step of process execution.
pub enum StepResult {
    /// Handled message successfully, may have more to process.
    Continue,
    /// Waiting on async I/O — hand this future to the reactor.
    Suspend(IoFuture),
    /// No messages in mailbox, want to wait.
    Wait,
    /// Process is finished.
    Done(Reason),
}
```

**Step 4: Run test to verify it passes**

Run: `cd zeptovm && cargo test --lib error::tests`
Expected: All error tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/error.rs
git commit -m "feat(zeptovm): add StepResult, IoResult, IoFuture types for scheduler"
```

---

## Task 3: Create Step-Based Behavior Trait

**Files:**
- Create: `zeptovm/src/step_behavior.rs`
- Modify: `zeptovm/src/lib.rs`
- Test: `zeptovm/src/step_behavior.rs` (inline tests)

**Step 1: Write the failing test**

Create `zeptovm/src/step_behavior.rs`:

```rust
use crate::error::{Message, Reason, StepResult};

/// Step-based process behavior. Each process owns one StepBehavior.
/// Unlike the async Behavior trait, handle() is synchronous.
/// When I/O is needed, return StepResult::Suspend(future).
pub trait StepBehavior: Send + 'static {
    /// Called once at spawn. checkpoint is Some if recovering.
    fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult;

    /// Called for each user message. NOT async — returns StepResult.
    fn handle(&mut self, msg: Message) -> StepResult;

    /// Called when the process is terminating.
    fn terminate(&mut self, reason: &Reason);

    /// Opt-in: return true if runtime should checkpoint after handle().
    fn should_checkpoint(&self) -> bool {
        false
    }

    /// Opt-in: serialize state for durable recovery.
    fn checkpoint(&self) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Message, Reason, StepResult};

    struct EchoStep;

    impl StepBehavior for EchoStep {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    #[test]
    fn test_step_behavior_can_be_boxed() {
        let mut b: Box<dyn StepBehavior> = Box::new(EchoStep);
        let result = b.init(None);
        assert!(matches!(result, StepResult::Continue));
        let result = b.handle(Message::text("hi"));
        assert!(matches!(result, StepResult::Continue));
    }

    #[test]
    fn test_step_behavior_checkpoint_defaults() {
        let b = EchoStep;
        assert!(!b.should_checkpoint());
        assert!(b.checkpoint().is_none());
    }

    #[test]
    fn test_step_behavior_done_variant() {
        struct DieOnFirst;
        impl StepBehavior for DieOnFirst {
            fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
                StepResult::Continue
            }
            fn handle(&mut self, _msg: Message) -> StepResult {
                StepResult::Done(Reason::Normal)
            }
            fn terminate(&mut self, _reason: &Reason) {}
        }

        let mut b = DieOnFirst;
        let result = b.handle(Message::text("bye"));
        assert!(matches!(result, StepResult::Done(Reason::Normal)));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib step_behavior::tests`
Expected: FAIL — module not registered

**Step 3: Register module in lib.rs**

Add to `zeptovm/src/lib.rs`:

```rust
pub mod step_behavior;
```

**Step 4: Run test to verify it passes**

Run: `cd zeptovm && cargo test --lib step_behavior::tests`
Expected: 3 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/step_behavior.rs zeptovm/src/lib.rs
git commit -m "feat(zeptovm): add StepBehavior trait — sync, step-based process behavior"
```

---

## Task 4: Create Scheduler-Driven Mailbox

**Files:**
- Create: `zeptovm/src/scheduler/mailbox.rs`
- Create: `zeptovm/src/scheduler/mod.rs`
- Modify: `zeptovm/src/lib.rs`

**Step 1: Write the failing test**

Create `zeptovm/src/scheduler/mailbox.rs`:

```rust
use std::collections::VecDeque;

use crate::error::{Message, SystemMsg};

/// Per-process mailbox owned by the scheduler (not channel-based).
/// The scheduler pushes messages directly; the process pops when stepped.
pub struct SchedMailbox {
    user: VecDeque<Message>,
    control: VecDeque<SystemMsg>,
}

impl SchedMailbox {
    pub fn new() -> Self {
        Self {
            user: VecDeque::new(),
            control: VecDeque::new(),
        }
    }

    pub fn push_user(&mut self, msg: Message) {
        self.user.push_back(msg);
    }

    pub fn push_control(&mut self, msg: SystemMsg) {
        self.control.push_back(msg);
    }

    pub fn pop_control(&mut self) -> Option<SystemMsg> {
        self.control.pop_front()
    }

    pub fn pop_user(&mut self) -> Option<Message> {
        self.user.pop_front()
    }

    pub fn has_messages(&self) -> bool {
        !self.user.is_empty() || !self.control.is_empty()
    }

    pub fn user_len(&self) -> usize {
        self.user.len()
    }

    pub fn control_len(&self) -> usize {
        self.control.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Message;

    #[test]
    fn test_mailbox_push_pop_user() {
        let mut mb = SchedMailbox::new();
        assert!(!mb.has_messages());

        mb.push_user(Message::text("hello"));
        assert!(mb.has_messages());
        assert_eq!(mb.user_len(), 1);

        let msg = mb.pop_user().unwrap();
        assert!(matches!(msg, Message::User(_)));
        assert!(!mb.has_messages());
    }

    #[test]
    fn test_mailbox_fifo_order() {
        let mut mb = SchedMailbox::new();
        mb.push_user(Message::text("first"));
        mb.push_user(Message::text("second"));

        if let Some(Message::User(crate::error::UserPayload::Text(s))) = mb.pop_user() {
            assert_eq!(s, "first");
        } else {
            panic!("expected text message");
        }
        if let Some(Message::User(crate::error::UserPayload::Text(s))) = mb.pop_user() {
            assert_eq!(s, "second");
        } else {
            panic!("expected text message");
        }
    }

    #[test]
    fn test_mailbox_control_priority() {
        let mut mb = SchedMailbox::new();
        mb.push_user(Message::text("user"));
        mb.push_control(SystemMsg::Suspend);

        // Control should be pop-able independently of user
        assert!(mb.pop_control().is_some());
        assert!(mb.pop_user().is_some());
    }

    #[test]
    fn test_mailbox_empty_returns_none() {
        let mut mb = SchedMailbox::new();
        assert!(mb.pop_user().is_none());
        assert!(mb.pop_control().is_none());
    }
}
```

**Step 2: Create scheduler module**

Create `zeptovm/src/scheduler/mod.rs`:

```rust
pub mod mailbox;
```

**Step 3: Register scheduler module in lib.rs**

Add to `zeptovm/src/lib.rs`:

```rust
pub mod scheduler;
```

**Step 4: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::mailbox::tests`
Expected: 4 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/scheduler/
git add zeptovm/src/lib.rs
git commit -m "feat(zeptovm): add scheduler-driven VecDeque mailbox"
```

---

## Task 5: Create Process State Machine

**Files:**
- Create: `zeptovm/src/scheduler/process.rs`
- Modify: `zeptovm/src/scheduler/mod.rs`

**Step 1: Write the failing test**

Create `zeptovm/src/scheduler/process.rs`:

```rust
use crate::{
    error::{IoFuture, Message, Reason, StepResult, SystemMsg},
    pid::Pid,
    scheduler::mailbox::SchedMailbox,
    step_behavior::StepBehavior,
};

/// Lifecycle state of a scheduler-managed process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// In run queue, ready to be stepped.
    Ready,
    /// Waiting on async I/O (future submitted to reactor).
    Suspended,
    /// Waiting for a message (mailbox empty).
    Waiting,
    /// Process has finished.
    Done,
}

/// A process entry managed by the scheduler.
pub struct ProcessEntry {
    pub pid: Pid,
    pub state: ProcessState,
    pub mailbox: SchedMailbox,
    behavior: Box<dyn StepBehavior>,
    reductions: u32,
    kill_requested: bool,
    exit_reason: Option<Reason>,
}

impl ProcessEntry {
    pub fn new(pid: Pid, behavior: Box<dyn StepBehavior>) -> Self {
        Self {
            pid,
            state: ProcessState::Ready,
            mailbox: SchedMailbox::new(),
            behavior,
            reductions: 0,
            kill_requested: false,
            exit_reason: None,
        }
    }

    /// Initialize the process. Returns the StepResult from init().
    pub fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult {
        self.behavior.init(checkpoint)
    }

    /// Step the process once: drain control queue, then handle one user message.
    /// Returns (StepResult, whether a reduction was consumed).
    pub fn step(&mut self) -> StepResult {
        // Check kill flag first (highest priority, like BEAM)
        if self.kill_requested {
            self.state = ProcessState::Done;
            return StepResult::Done(Reason::Kill);
        }

        // Drain control messages first
        while let Some(ctrl) = self.mailbox.pop_control() {
            match ctrl {
                SystemMsg::ExitLinked(_from, ref reason) => {
                    if reason.is_abnormal() {
                        self.state = ProcessState::Done;
                        return StepResult::Done(reason.clone());
                    }
                }
                SystemMsg::MonitorDown(_, _) => {
                    // Deliver as user-visible event in future
                }
                SystemMsg::Suspend => {
                    self.state = ProcessState::Suspended;
                    return StepResult::Wait;
                }
                SystemMsg::Resume => {
                    // No-op if not suspended
                }
                SystemMsg::GetState(_tx) => {
                    // TODO: respond with process info
                }
            }
        }

        // Handle one user message
        match self.mailbox.pop_user() {
            Some(msg) => {
                self.reductions += 1;
                self.behavior.handle(msg)
            }
            None => StepResult::Wait,
        }
    }

    /// Request kill (checked at next step).
    pub fn request_kill(&mut self) {
        self.kill_requested = true;
    }

    /// Call terminate callback.
    pub fn terminate(&mut self, reason: &Reason) {
        self.behavior.terminate(reason);
        self.exit_reason = Some(reason.clone());
        self.state = ProcessState::Done;
    }

    /// Get current reduction count and reset it.
    pub fn take_reductions(&mut self) -> u32 {
        let r = self.reductions;
        self.reductions = 0;
        r
    }

    pub fn reductions(&self) -> u32 {
        self.reductions
    }

    pub fn exit_reason(&self) -> Option<&Reason> {
        self.exit_reason.as_ref()
    }

    pub fn is_killed(&self) -> bool {
        self.kill_requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Message, Reason, StepResult};
    use crate::step_behavior::StepBehavior;

    struct Echo;
    impl StepBehavior for Echo {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    struct Counter {
        count: u32,
    }
    impl StepBehavior for Counter {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            self.count += 1;
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    #[test]
    fn test_process_entry_new() {
        let pid = Pid::new();
        let proc = ProcessEntry::new(pid, Box::new(Echo));
        assert_eq!(proc.state, ProcessState::Ready);
        assert_eq!(proc.reductions(), 0);
        assert!(!proc.is_killed());
    }

    #[test]
    fn test_process_init() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        let result = proc.init(None);
        assert!(matches!(result, StepResult::Continue));
    }

    #[test]
    fn test_process_step_with_message() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        proc.init(None);
        proc.mailbox.push_user(Message::text("hello"));

        let result = proc.step();
        assert!(matches!(result, StepResult::Continue));
        assert_eq!(proc.reductions(), 1);
    }

    #[test]
    fn test_process_step_empty_mailbox_returns_wait() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        proc.init(None);

        let result = proc.step();
        assert!(matches!(result, StepResult::Wait));
    }

    #[test]
    fn test_process_kill_flag() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        proc.init(None);
        proc.mailbox.push_user(Message::text("ignored"));

        proc.request_kill();
        let result = proc.step();
        assert!(matches!(result, StepResult::Done(Reason::Kill)));
        assert_eq!(proc.state, ProcessState::Done);
    }

    #[test]
    fn test_process_reductions_increment() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Counter { count: 0 }));
        proc.init(None);

        for i in 0..5 {
            proc.mailbox.push_user(Message::text(format!("msg-{i}")));
        }

        for _ in 0..5 {
            proc.step();
        }
        assert_eq!(proc.reductions(), 5);
    }

    #[test]
    fn test_process_take_reductions_resets() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        proc.init(None);
        proc.mailbox.push_user(Message::text("a"));
        proc.step();

        assert_eq!(proc.take_reductions(), 1);
        assert_eq!(proc.reductions(), 0);
    }

    #[test]
    fn test_process_terminate() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        proc.init(None);
        proc.terminate(&Reason::Normal);

        assert_eq!(proc.state, ProcessState::Done);
        assert!(matches!(proc.exit_reason(), Some(Reason::Normal)));
    }

    #[test]
    fn test_process_exit_linked_abnormal_kills() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        proc.init(None);

        let other = Pid::new();
        proc.mailbox
            .push_control(SystemMsg::ExitLinked(other, Reason::Custom("crash".into())));

        let result = proc.step();
        assert!(matches!(result, StepResult::Done(Reason::Custom(_))));
    }

    #[test]
    fn test_process_exit_linked_normal_ignored() {
        let pid = Pid::new();
        let mut proc = ProcessEntry::new(pid, Box::new(Echo));
        proc.init(None);
        proc.mailbox.push_user(Message::text("still alive"));

        let other = Pid::new();
        proc.mailbox
            .push_control(SystemMsg::ExitLinked(other, Reason::Normal));

        let result = proc.step();
        // Normal exit link is ignored, should process user message
        assert!(matches!(result, StepResult::Continue));
    }
}
```

**Step 2: Register in scheduler/mod.rs**

Add to `zeptovm/src/scheduler/mod.rs`:

```rust
pub mod process;
```

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::process::tests`
Expected: All 10 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/scheduler/process.rs zeptovm/src/scheduler/mod.rs
git commit -m "feat(zeptovm): add ProcessEntry state machine for scheduler"
```

---

## Task 6: Create Run Queue

**Files:**
- Create: `zeptovm/src/scheduler/run_queue.rs`
- Modify: `zeptovm/src/scheduler/mod.rs`

**Step 1: Write the run queue with tests**

Create `zeptovm/src/scheduler/run_queue.rs`:

```rust
use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::pid::Pid;

/// A scheduler-local run queue backed by a crossbeam work-stealing deque.
/// Each scheduler thread owns one RunQueue.
pub struct RunQueue {
    /// Local end — only the owning thread pushes/pops.
    local: Worker<Pid>,
}

impl RunQueue {
    pub fn new() -> Self {
        Self {
            local: Worker::new_fifo(),
        }
    }

    /// Push a ready process pid onto the local queue.
    pub fn push(&self, pid: Pid) {
        self.local.push(pid);
    }

    /// Pop the next ready pid from the local queue.
    pub fn pop(&self) -> Option<Pid> {
        self.local.pop()
    }

    /// Get a stealer handle (for other threads to steal from this queue).
    pub fn stealer(&self) -> Stealer<Pid> {
        self.local.stealer()
    }

    /// Check if the local queue is empty.
    pub fn is_empty(&self) -> bool {
        self.local.is_empty()
    }

    /// Try to steal a pid from another scheduler's queue.
    pub fn steal_from(stealer: &Stealer<Pid>) -> Option<Pid> {
        loop {
            match stealer.steal() {
                Steal::Success(pid) => return Some(pid),
                Steal::Empty => return None,
                Steal::Retry => continue,
            }
        }
    }
}

/// Global injector queue for distributing new processes to schedulers.
pub struct GlobalQueue {
    injector: Injector<Pid>,
}

impl GlobalQueue {
    pub fn new() -> Self {
        Self {
            injector: Injector::new(),
        }
    }

    /// Push a pid into the global queue (any thread can call this).
    pub fn push(&self, pid: Pid) {
        self.injector.push(pid);
    }

    /// Try to steal a batch from the global queue into a local worker.
    pub fn steal_into(&self, local: &RunQueue) -> Option<Pid> {
        loop {
            match self.injector.steal_batch_and_pop(&local.local) {
                Steal::Success(pid) => return Some(pid),
                Steal::Empty => return None,
                Steal::Retry => continue,
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.injector.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;

    #[test]
    fn test_run_queue_push_pop() {
        let rq = RunQueue::new();
        let pid = Pid::from_raw(1);
        rq.push(pid);
        assert_eq!(rq.pop(), Some(pid));
        assert_eq!(rq.pop(), None);
    }

    #[test]
    fn test_run_queue_fifo_order() {
        let rq = RunQueue::new();
        let p1 = Pid::from_raw(1);
        let p2 = Pid::from_raw(2);
        let p3 = Pid::from_raw(3);
        rq.push(p1);
        rq.push(p2);
        rq.push(p3);
        assert_eq!(rq.pop(), Some(p1));
        assert_eq!(rq.pop(), Some(p2));
        assert_eq!(rq.pop(), Some(p3));
    }

    #[test]
    fn test_run_queue_is_empty() {
        let rq = RunQueue::new();
        assert!(rq.is_empty());
        rq.push(Pid::from_raw(1));
        assert!(!rq.is_empty());
    }

    #[test]
    fn test_run_queue_steal() {
        let rq = RunQueue::new();
        let stealer = rq.stealer();

        rq.push(Pid::from_raw(10));
        rq.push(Pid::from_raw(20));

        let stolen = RunQueue::steal_from(&stealer);
        assert!(stolen.is_some());
    }

    #[test]
    fn test_global_queue_push_steal() {
        let gq = GlobalQueue::new();
        let local = RunQueue::new();

        gq.push(Pid::from_raw(100));
        gq.push(Pid::from_raw(200));

        let stolen = gq.steal_into(&local);
        assert!(stolen.is_some());
    }

    #[test]
    fn test_global_queue_empty() {
        let gq = GlobalQueue::new();
        let local = RunQueue::new();
        assert!(gq.is_empty());
        assert!(gq.steal_into(&local).is_none());
    }
}
```

**Step 2: Register in scheduler/mod.rs**

Add to `zeptovm/src/scheduler/mod.rs`:

```rust
pub mod run_queue;
```

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::run_queue::tests`
Expected: 6 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/scheduler/run_queue.rs zeptovm/src/scheduler/mod.rs
git commit -m "feat(zeptovm): add RunQueue and GlobalQueue with work-stealing deque"
```

---

## Task 7: Create Scheduler Engine (Single-Threaded Core)

**Files:**
- Create: `zeptovm/src/scheduler/engine.rs`
- Modify: `zeptovm/src/scheduler/mod.rs`

This is the heart of the system. The engine steps processes from the run queue, counts reductions, handles state transitions, and manages the process table.

**Step 1: Write the engine with tests**

Create `zeptovm/src/scheduler/engine.rs`:

```rust
use std::collections::HashMap;

use crate::{
    error::{Message, Reason, StepResult, SystemMsg},
    pid::Pid,
    scheduler::{
        mailbox::SchedMailbox,
        process::{ProcessEntry, ProcessState},
        run_queue::RunQueue,
    },
    step_behavior::StepBehavior,
};

/// Default reductions per time slice (matches BEAM).
pub const DEFAULT_REDUCTIONS: u32 = 200;

/// Outcome of running one scheduler tick.
#[derive(Debug, PartialEq, Eq)]
pub enum TickResult {
    /// Stepped a process.
    Stepped(Pid),
    /// No ready processes in queue.
    Idle,
    /// A process exited.
    Exited(Pid, Reason),
}

/// The scheduler engine. Owns a process table and a run queue.
pub struct SchedulerEngine {
    /// All processes managed by this scheduler.
    processes: HashMap<Pid, ProcessEntry>,
    /// Local run queue of ready process pids.
    pub run_queue: RunQueue,
    /// Max reductions before preempting a process.
    max_reductions: u32,
}

impl SchedulerEngine {
    pub fn new() -> Self {
        Self {
            processes: HashMap::new(),
            run_queue: RunQueue::new(),
            max_reductions: DEFAULT_REDUCTIONS,
        }
    }

    pub fn with_max_reductions(mut self, max: u32) -> Self {
        self.max_reductions = max;
        self
    }

    /// Spawn a new process into this scheduler.
    pub fn spawn(
        &mut self,
        behavior: Box<dyn StepBehavior>,
        checkpoint: Option<Vec<u8>>,
    ) -> Pid {
        let pid = Pid::new();
        let mut entry = ProcessEntry::new(pid, behavior);

        // Run init
        match entry.init(checkpoint) {
            StepResult::Continue => {
                entry.state = ProcessState::Ready;
                self.processes.insert(pid, entry);
                self.run_queue.push(pid);
            }
            StepResult::Wait => {
                entry.state = ProcessState::Waiting;
                self.processes.insert(pid, entry);
                // Don't enqueue — will be enqueued when message arrives
            }
            StepResult::Suspend(_future) => {
                entry.state = ProcessState::Suspended;
                self.processes.insert(pid, entry);
                // TODO: submit future to reactor (Task 10)
            }
            StepResult::Done(reason) => {
                entry.terminate(&reason);
                // Process never enters the table — init failed
            }
        }

        pid
    }

    /// Send a user message to a process.
    pub fn send(&mut self, pid: &Pid, msg: Message) -> Result<(), String> {
        let entry = self
            .processes
            .get_mut(pid)
            .ok_or_else(|| format!("process {pid} not found"))?;

        entry.mailbox.push_user(msg);

        // If process was Waiting, make it Ready and enqueue
        if entry.state == ProcessState::Waiting {
            entry.state = ProcessState::Ready;
            self.run_queue.push(*pid);
        }

        Ok(())
    }

    /// Send a control message to a process.
    pub fn send_control(&mut self, pid: &Pid, msg: SystemMsg) -> Result<(), String> {
        let entry = self
            .processes
            .get_mut(pid)
            .ok_or_else(|| format!("process {pid} not found"))?;

        entry.mailbox.push_control(msg);

        if entry.state == ProcessState::Waiting {
            entry.state = ProcessState::Ready;
            self.run_queue.push(*pid);
        }

        Ok(())
    }

    /// Kill a process.
    pub fn kill(&mut self, pid: &Pid) {
        if let Some(entry) = self.processes.get_mut(pid) {
            entry.request_kill();
            // Ensure it gets scheduled to process the kill
            if entry.state == ProcessState::Waiting {
                entry.state = ProcessState::Ready;
                self.run_queue.push(*pid);
            }
        }
    }

    /// Run one scheduler tick: pick next ready process, step it up to
    /// max_reductions times, handle the result.
    pub fn tick(&mut self) -> TickResult {
        let pid = match self.run_queue.pop() {
            Some(pid) => pid,
            None => return TickResult::Idle,
        };

        // Process might have been removed between enqueue and dequeue
        let entry = match self.processes.get_mut(&pid) {
            Some(e) => e,
            None => return TickResult::Idle,
        };

        // Step the process up to max_reductions times
        let mut last_result = StepResult::Wait;
        for _ in 0..self.max_reductions {
            last_result = entry.step();
            match &last_result {
                StepResult::Continue => {
                    // Process handled a message, keep going if more available
                    if !entry.mailbox.has_messages() {
                        break;
                    }
                }
                StepResult::Wait | StepResult::Suspend(_) | StepResult::Done(_) => {
                    break;
                }
            }
        }

        // Handle the result
        match last_result {
            StepResult::Continue => {
                // Hit reduction limit with messages remaining — preempted
                if entry.mailbox.has_messages() {
                    entry.state = ProcessState::Ready;
                    self.run_queue.push(pid);
                } else {
                    entry.state = ProcessState::Waiting;
                }
                TickResult::Stepped(pid)
            }
            StepResult::Wait => {
                entry.state = ProcessState::Waiting;
                TickResult::Stepped(pid)
            }
            StepResult::Suspend(_future) => {
                entry.state = ProcessState::Suspended;
                // TODO: submit future to reactor (Task 10)
                TickResult::Stepped(pid)
            }
            StepResult::Done(reason) => {
                entry.terminate(&reason);
                let reason_clone = reason.clone();
                self.processes.remove(&pid);
                TickResult::Exited(pid, reason_clone)
            }
        }
    }

    /// Number of live processes.
    pub fn process_count(&self) -> usize {
        self.processes.len()
    }

    /// Check if a process exists.
    pub fn has_process(&self, pid: &Pid) -> bool {
        self.processes.contains_key(pid)
    }

    /// Get process state.
    pub fn process_state(&self, pid: &Pid) -> Option<ProcessState> {
        self.processes.get(pid).map(|e| e.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Message, Reason, StepResult};
    use crate::step_behavior::StepBehavior;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    struct Echo;
    impl StepBehavior for Echo {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    struct CountStep {
        count: Arc<AtomicU32>,
    }
    impl StepBehavior for CountStep {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            self.count.fetch_add(1, Ordering::Relaxed);
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    struct DieOnFirst;
    impl StepBehavior for DieOnFirst {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            StepResult::Done(Reason::Normal)
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    struct FailInit;
    impl StepBehavior for FailInit {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Done(Reason::Custom("init failed".into()))
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    #[test]
    fn test_engine_spawn_and_count() {
        let mut engine = SchedulerEngine::new();
        let pid = engine.spawn(Box::new(Echo), None);
        assert_eq!(engine.process_count(), 1);
        assert!(engine.has_process(&pid));
    }

    #[test]
    fn test_engine_spawn_fail_init() {
        let mut engine = SchedulerEngine::new();
        let _pid = engine.spawn(Box::new(FailInit), None);
        // Process should not be in the table (init failed)
        assert_eq!(engine.process_count(), 0);
    }

    #[test]
    fn test_engine_send_and_tick() {
        let count = Arc::new(AtomicU32::new(0));
        let mut engine = SchedulerEngine::new();
        let pid = engine.spawn(
            Box::new(CountStep {
                count: count.clone(),
            }),
            None,
        );

        engine.send(&pid, Message::text("hello")).unwrap();
        let result = engine.tick();
        assert!(matches!(result, TickResult::Stepped(_)));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_engine_tick_idle_when_empty() {
        let mut engine = SchedulerEngine::new();
        let result = engine.tick();
        assert_eq!(result, TickResult::Idle);
    }

    #[test]
    fn test_engine_process_exits() {
        let mut engine = SchedulerEngine::new();
        let pid = engine.spawn(Box::new(DieOnFirst), None);

        engine.send(&pid, Message::text("bye")).unwrap();
        let result = engine.tick();
        assert!(matches!(result, TickResult::Exited(_, Reason::Normal)));
        assert_eq!(engine.process_count(), 0);
    }

    #[test]
    fn test_engine_kill() {
        let mut engine = SchedulerEngine::new();
        let pid = engine.spawn(Box::new(Echo), None);

        engine.kill(&pid);
        let result = engine.tick();
        assert!(matches!(result, TickResult::Exited(_, Reason::Kill)));
        assert_eq!(engine.process_count(), 0);
    }

    #[test]
    fn test_engine_waiting_process_wakes_on_send() {
        let count = Arc::new(AtomicU32::new(0));
        let mut engine = SchedulerEngine::new();
        let pid = engine.spawn(
            Box::new(CountStep {
                count: count.clone(),
            }),
            None,
        );

        // First tick: no messages → process goes to Waiting
        let result = engine.tick();
        assert!(matches!(result, TickResult::Stepped(_)));
        assert_eq!(
            engine.process_state(&pid),
            Some(ProcessState::Waiting)
        );

        // Send wakes the process
        engine.send(&pid, Message::text("wake")).unwrap();
        assert_eq!(
            engine.process_state(&pid),
            Some(ProcessState::Ready)
        );

        // Second tick processes the message
        let result = engine.tick();
        assert!(matches!(result, TickResult::Stepped(_)));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_engine_preemption_at_reduction_limit() {
        let count = Arc::new(AtomicU32::new(0));
        let mut engine = SchedulerEngine::new().with_max_reductions(3);
        let pid = engine.spawn(
            Box::new(CountStep {
                count: count.clone(),
            }),
            None,
        );

        // Send 10 messages
        for i in 0..10 {
            engine.send(&pid, Message::text(format!("msg-{i}"))).unwrap();
        }

        // First tick: processes only 3 (reduction limit)
        engine.tick();
        assert_eq!(count.load(Ordering::Relaxed), 3);
        // Process should be re-enqueued (still has messages)
        assert_eq!(
            engine.process_state(&pid),
            Some(ProcessState::Ready)
        );

        // Second tick: 3 more
        engine.tick();
        assert_eq!(count.load(Ordering::Relaxed), 6);

        // Third tick: 3 more
        engine.tick();
        assert_eq!(count.load(Ordering::Relaxed), 9);

        // Fourth tick: 1 remaining
        engine.tick();
        assert_eq!(count.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_engine_fairness_two_processes() {
        let count_a = Arc::new(AtomicU32::new(0));
        let count_b = Arc::new(AtomicU32::new(0));
        let mut engine = SchedulerEngine::new().with_max_reductions(5);

        let pid_a = engine.spawn(
            Box::new(CountStep {
                count: count_a.clone(),
            }),
            None,
        );
        let pid_b = engine.spawn(
            Box::new(CountStep {
                count: count_b.clone(),
            }),
            None,
        );

        // Flood both with messages
        for i in 0..20 {
            engine
                .send(&pid_a, Message::text(format!("a-{i}")))
                .unwrap();
            engine
                .send(&pid_b, Message::text(format!("b-{i}")))
                .unwrap();
        }

        // Run 4 ticks — each process should get ~2 ticks
        for _ in 0..4 {
            engine.tick();
        }

        let a = count_a.load(Ordering::Relaxed);
        let b = count_b.load(Ordering::Relaxed);

        // Both should have been serviced (fairness)
        assert!(a > 0, "process A should have been stepped, got {a}");
        assert!(b > 0, "process B should have been stepped, got {b}");
        // Neither should have consumed all — they share the scheduler
        assert!(
            a <= 10 && b <= 10,
            "both processes should be roughly fair: a={a}, b={b}"
        );
    }
}
```

**Step 2: Register in scheduler/mod.rs**

Add to `zeptovm/src/scheduler/mod.rs`:

```rust
pub mod engine;
```

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::engine::tests`
Expected: All 9 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/scheduler/engine.rs zeptovm/src/scheduler/mod.rs
git commit -m "feat(zeptovm): add SchedulerEngine — single-thread process stepping with reductions"
```

---

## Task 8: v0 Gate Test — 100 Processes, 10k Messages, Fair Scheduling

**Files:**
- Create: `zeptovm/tests/sched_v0_gate.rs`

**Step 1: Write the gate test**

Create `zeptovm/tests/sched_v0_gate.rs`:

```rust
//! Scheduler v0 Gate Test: 100 processes, 10k messages, fair scheduling.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use zeptovm::{
    error::{Message, Reason, StepResult},
    scheduler::engine::{SchedulerEngine, TickResult},
    step_behavior::StepBehavior,
};

struct CounterStep {
    count: Arc<AtomicU32>,
}

impl StepBehavior for CounterStep {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
        StepResult::Continue
    }
    fn handle(&mut self, _msg: Message) -> StepResult {
        self.count.fetch_add(1, Ordering::Relaxed);
        StepResult::Continue
    }
    fn terminate(&mut self, _reason: &Reason) {}
}

#[test]
fn sched_gate_v0_100_processes_10k_messages() {
    let mut engine = SchedulerEngine::new();
    let total_count = Arc::new(AtomicU32::new(0));

    // Spawn 100 processes
    let mut pids = Vec::new();
    for _ in 0..100 {
        let pid = engine.spawn(
            Box::new(CounterStep {
                count: total_count.clone(),
            }),
            None,
        );
        pids.push(pid);
    }

    assert_eq!(engine.process_count(), 100);

    // Deliver 10k messages (100 per process)
    for pid in &pids {
        for i in 0..100 {
            engine
                .send(pid, Message::text(format!("msg-{i}")))
                .unwrap();
        }
    }

    // Run scheduler ticks until all messages are processed
    let mut tick_count = 0u32;
    let max_ticks = 100_000;
    loop {
        let result = engine.tick();
        tick_count += 1;

        if total_count.load(Ordering::Relaxed) >= 10_000 {
            break;
        }

        if tick_count >= max_ticks {
            let processed = total_count.load(Ordering::Relaxed);
            panic!(
                "timed out after {max_ticks} ticks, only {processed}/10000 processed"
            );
        }

        if matches!(result, TickResult::Idle) {
            let processed = total_count.load(Ordering::Relaxed);
            if processed < 10_000 {
                panic!(
                    "scheduler idle but only {processed}/10000 processed"
                );
            }
        }
    }

    let total = total_count.load(Ordering::Relaxed);
    assert_eq!(total, 10_000, "expected 10000 messages, got {total}");
}

#[test]
fn sched_gate_v0_fair_scheduling() {
    let mut engine = SchedulerEngine::new().with_max_reductions(10);
    let counts: Vec<Arc<AtomicU32>> = (0..10).map(|_| Arc::new(AtomicU32::new(0))).collect();

    // Spawn 10 processes
    let mut pids = Vec::new();
    for count in &counts {
        let pid = engine.spawn(
            Box::new(CounterStep {
                count: count.clone(),
            }),
            None,
        );
        pids.push(pid);
    }

    // Flood first process with 1000 messages, others with 10 each
    for i in 0..1000 {
        engine
            .send(&pids[0], Message::text(format!("flood-{i}")))
            .unwrap();
    }
    for pid in &pids[1..] {
        for i in 0..10 {
            engine
                .send(pid, Message::text(format!("msg-{i}")))
                .unwrap();
        }
    }

    // Run 50 ticks
    for _ in 0..50 {
        engine.tick();
    }

    // All non-flooded processes should have completed their 10 messages
    for (i, count) in counts.iter().enumerate().skip(1) {
        let c = count.load(Ordering::Relaxed);
        assert!(
            c >= 10,
            "process {i} should have processed all 10 messages, got {c}"
        );
    }

    // Flooded process should NOT have processed all 1000 yet (fair scheduling
    // means it didn't starve others)
    let flood_count = counts[0].load(Ordering::Relaxed);
    assert!(
        flood_count < 1000,
        "flooded process should not have consumed all messages yet, got {flood_count}"
    );
    assert!(
        flood_count > 0,
        "flooded process should have processed some messages"
    );
}

#[test]
fn sched_gate_v0_clean_exit() {
    let mut engine = SchedulerEngine::new();

    struct ExitOnMessage;
    impl StepBehavior for ExitOnMessage {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            StepResult::Done(Reason::Normal)
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = engine.spawn(Box::new(ExitOnMessage), None);
    engine.send(&pid, Message::text("bye")).unwrap();

    let result = engine.tick();
    assert!(matches!(result, TickResult::Exited(_, Reason::Normal)));
    assert_eq!(engine.process_count(), 0);
}

#[test]
fn sched_gate_v0_kill() {
    let mut engine = SchedulerEngine::new();

    struct Forever;
    impl StepBehavior for Forever {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = engine.spawn(Box::new(Forever), None);
    engine.kill(&pid);

    let result = engine.tick();
    assert!(matches!(result, TickResult::Exited(_, Reason::Kill)));
    assert_eq!(engine.process_count(), 0);
}
```

**Step 2: Run the gate tests**

Run: `cd zeptovm && cargo test --test sched_v0_gate`
Expected: 4 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/tests/sched_v0_gate.rs
git commit -m "test(zeptovm): add scheduler v0 gate tests — 100 processes, 10k messages, fairness"
```

---

## Task 9: Multi-Threaded Scheduler

**Files:**
- Create: `zeptovm/src/scheduler/runtime.rs`
- Modify: `zeptovm/src/scheduler/mod.rs`

The runtime wraps N SchedulerEngines running on N OS threads, with a shared process table protected by DashMap, and a global injector queue.

**Step 1: Write the runtime with tests**

Create `zeptovm/src/scheduler/runtime.rs`:

```rust
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};

use crossbeam_channel::{self, Receiver, Sender};
use crossbeam_deque::Stealer;
use dashmap::DashMap;

use crate::{
    error::{Message, Reason, StepResult, SystemMsg},
    pid::Pid,
    scheduler::{
        process::{ProcessEntry, ProcessState},
        run_queue::{GlobalQueue, RunQueue},
    },
    step_behavior::StepBehavior,
};

/// Default reductions per time slice.
pub const DEFAULT_REDUCTIONS: u32 = 200;

/// Commands sent from the public API to scheduler threads.
enum Command {
    /// A message to deliver to a process.
    Send(Pid, Message),
    /// A control message to deliver.
    SendControl(Pid, SystemMsg),
    /// Kill a process.
    Kill(Pid),
    /// Shutdown the scheduler.
    Shutdown,
}

/// Notification from scheduler threads back to runtime.
pub enum Notification {
    /// A process exited.
    Exited(Pid, Reason),
}

/// Multi-threaded scheduler runtime.
pub struct SchedulerRuntime {
    /// Shared process table (all scheduler threads can access).
    processes: Arc<DashMap<Pid, ProcessEntry>>,
    /// Global injector queue for new processes.
    global_queue: Arc<GlobalQueue>,
    /// Command channel (runtime API → scheduler threads).
    cmd_tx: Sender<Command>,
    /// Notification channel (scheduler threads → runtime API).
    notify_rx: Receiver<Notification>,
    /// Shutdown flag.
    shutdown: Arc<AtomicBool>,
    /// Scheduler thread join handles.
    threads: Vec<thread::JoinHandle<()>>,
    /// Stealers for each scheduler thread (for work stealing).
    stealers: Vec<Stealer<Pid>>,
}

impl SchedulerRuntime {
    /// Create and start a scheduler runtime with `n` threads.
    pub fn new(n: usize) -> Self {
        assert!(n > 0, "need at least 1 scheduler thread");

        let processes: Arc<DashMap<Pid, ProcessEntry>> = Arc::new(DashMap::new());
        let global_queue = Arc::new(GlobalQueue::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Command>();
        let (notify_tx, notify_rx) = crossbeam_channel::unbounded::<Notification>();

        let mut threads = Vec::with_capacity(n);
        let mut stealers = Vec::with_capacity(n);
        let mut run_queues_stealers = Vec::with_capacity(n);

        // Create run queues first to collect stealers
        let run_queues: Vec<RunQueue> = (0..n).map(|_| RunQueue::new()).collect();
        for rq in &run_queues {
            run_queues_stealers.push(rq.stealer());
        }
        stealers = run_queues_stealers.clone();

        // Spawn scheduler threads
        for (i, rq) in run_queues.into_iter().enumerate() {
            let procs = processes.clone();
            let gq = global_queue.clone();
            let sd = shutdown.clone();
            let rx = cmd_rx.clone();
            let tx = notify_tx.clone();
            let peer_stealers: Vec<Stealer<Pid>> = run_queues_stealers
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, s)| s.clone())
                .collect();

            let handle = thread::Builder::new()
                .name(format!("sched-{i}"))
                .spawn(move || {
                    scheduler_thread_loop(
                        rq,
                        procs,
                        gq,
                        peer_stealers,
                        sd,
                        rx,
                        tx,
                    );
                })
                .expect("failed to spawn scheduler thread");

            threads.push(handle);
        }

        Self {
            processes,
            global_queue,
            cmd_tx,
            notify_rx,
            shutdown,
            threads,
            stealers,
        }
    }

    /// Spawn a process into the scheduler.
    pub fn spawn(
        &self,
        behavior: Box<dyn StepBehavior>,
        checkpoint: Option<Vec<u8>>,
    ) -> Pid {
        let pid = Pid::new();
        let mut entry = ProcessEntry::new(pid, behavior);

        match entry.init(checkpoint) {
            StepResult::Continue => {
                entry.state = ProcessState::Ready;
                self.processes.insert(pid, entry);
                self.global_queue.push(pid);
            }
            StepResult::Wait => {
                entry.state = ProcessState::Waiting;
                self.processes.insert(pid, entry);
            }
            StepResult::Suspend(_) => {
                entry.state = ProcessState::Suspended;
                self.processes.insert(pid, entry);
            }
            StepResult::Done(reason) => {
                entry.terminate(&reason);
                // Not inserted — init failed
            }
        }

        pid
    }

    /// Send a message to a process.
    pub fn send(&self, pid: &Pid, msg: Message) -> Result<(), String> {
        if !self.processes.contains_key(pid) {
            return Err(format!("process {pid} not found"));
        }
        self.cmd_tx
            .send(Command::Send(*pid, msg))
            .map_err(|e| format!("send failed: {e}"))
    }

    /// Kill a process.
    pub fn kill(&self, pid: &Pid) {
        let _ = self.cmd_tx.send(Command::Kill(*pid));
    }

    /// Drain exit notifications.
    pub fn drain_notifications(&self) -> Vec<Notification> {
        let mut notes = Vec::new();
        while let Ok(n) = self.notify_rx.try_recv() {
            notes.push(n);
        }
        notes
    }

    /// Number of live processes.
    pub fn process_count(&self) -> usize {
        self.processes.len()
    }

    /// Shutdown all scheduler threads.
    pub fn shutdown(self) {
        self.shutdown.store(true, Ordering::Release);
        // Send shutdown commands to wake blocked threads
        for _ in &self.threads {
            let _ = self.cmd_tx.send(Command::Shutdown);
        }
        for handle in self.threads {
            let _ = handle.join();
        }
    }
}

fn scheduler_thread_loop(
    run_queue: RunQueue,
    processes: Arc<DashMap<Pid, ProcessEntry>>,
    global_queue: Arc<GlobalQueue>,
    peer_stealers: Vec<Stealer<Pid>>,
    shutdown: Arc<AtomicBool>,
    cmd_rx: Receiver<Command>,
    notify_tx: Sender<Notification>,
) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // 1. Process commands (non-blocking)
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                Command::Send(pid, msg) => {
                    if let Some(mut entry) = processes.get_mut(&pid) {
                        entry.mailbox.push_user(msg);
                        if entry.state == ProcessState::Waiting {
                            entry.state = ProcessState::Ready;
                            run_queue.push(pid);
                        }
                    }
                }
                Command::SendControl(pid, msg) => {
                    if let Some(mut entry) = processes.get_mut(&pid) {
                        entry.mailbox.push_control(msg);
                        if entry.state == ProcessState::Waiting {
                            entry.state = ProcessState::Ready;
                            run_queue.push(pid);
                        }
                    }
                }
                Command::Kill(pid) => {
                    if let Some(mut entry) = processes.get_mut(&pid) {
                        entry.request_kill();
                        if entry.state == ProcessState::Waiting {
                            entry.state = ProcessState::Ready;
                            run_queue.push(pid);
                        }
                    }
                }
                Command::Shutdown => {
                    return;
                }
            }
        }

        // 2. Try to get work: local queue → global queue → steal from peers
        let pid = run_queue
            .pop()
            .or_else(|| global_queue.steal_into(&run_queue))
            .or_else(|| {
                for stealer in &peer_stealers {
                    if let Some(pid) = RunQueue::steal_from(stealer) {
                        return Some(pid);
                    }
                }
                None
            });

        let pid = match pid {
            Some(p) => p,
            None => {
                // No work — brief sleep to avoid busy-wait
                // (scheduler collapse in Task 14)
                thread::sleep(std::time::Duration::from_micros(100));
                continue;
            }
        };

        // 3. Step the process
        let mut entry = match processes.get_mut(&pid) {
            Some(e) => e,
            None => continue,
        };

        let mut last_result = StepResult::Wait;
        for _ in 0..DEFAULT_REDUCTIONS {
            last_result = entry.step();
            match &last_result {
                StepResult::Continue => {
                    if !entry.mailbox.has_messages() {
                        break;
                    }
                }
                StepResult::Wait | StepResult::Suspend(_) | StepResult::Done(_) => {
                    break;
                }
            }
        }

        match last_result {
            StepResult::Continue => {
                if entry.mailbox.has_messages() {
                    entry.state = ProcessState::Ready;
                    drop(entry); // release DashMap guard before pushing
                    run_queue.push(pid);
                } else {
                    entry.state = ProcessState::Waiting;
                }
            }
            StepResult::Wait => {
                entry.state = ProcessState::Waiting;
            }
            StepResult::Suspend(_future) => {
                entry.state = ProcessState::Suspended;
                // TODO: submit to reactor
            }
            StepResult::Done(reason) => {
                entry.terminate(&reason);
                drop(entry);
                processes.remove(&pid);
                let _ = notify_tx.send(Notification::Exited(pid, reason));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Message, Reason, StepResult};
    use crate::step_behavior::StepBehavior;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };
    use std::time::Duration;

    struct CountStep {
        count: Arc<AtomicU32>,
    }
    impl StepBehavior for CountStep {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            self.count.fetch_add(1, Ordering::Relaxed);
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    #[test]
    fn test_runtime_spawn_and_count() {
        let rt = SchedulerRuntime::new(2);
        let pid = rt.spawn(
            Box::new(CountStep {
                count: Arc::new(AtomicU32::new(0)),
            }),
            None,
        );
        assert_eq!(rt.process_count(), 1);
        rt.shutdown();
    }

    #[test]
    fn test_runtime_send_and_process() {
        let count = Arc::new(AtomicU32::new(0));
        let rt = SchedulerRuntime::new(2);
        let pid = rt.spawn(
            Box::new(CountStep {
                count: count.clone(),
            }),
            None,
        );

        for i in 0..100 {
            rt.send(&pid, Message::text(format!("msg-{i}"))).unwrap();
        }

        // Wait for processing
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if count.load(Ordering::Relaxed) >= 100 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let c = count.load(Ordering::Relaxed);
                rt.shutdown();
                panic!("timed out: only {c}/100 processed");
            }
            thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(count.load(Ordering::Relaxed), 100);
        rt.shutdown();
    }

    #[test]
    fn test_runtime_kill() {
        let rt = SchedulerRuntime::new(1);
        let count = Arc::new(AtomicU32::new(0));
        let pid = rt.spawn(
            Box::new(CountStep {
                count: count.clone(),
            }),
            None,
        );

        rt.kill(&pid);

        // Wait for process to exit
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if rt.process_count() == 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                rt.shutdown();
                panic!("kill did not remove process");
            }
            thread::sleep(Duration::from_millis(1));
        }

        rt.shutdown();
    }

    #[test]
    fn test_runtime_100_processes_10k_messages() {
        let total = Arc::new(AtomicU32::new(0));
        let rt = SchedulerRuntime::new(4);

        let mut pids = Vec::new();
        for _ in 0..100 {
            let pid = rt.spawn(
                Box::new(CountStep {
                    count: total.clone(),
                }),
                None,
            );
            pids.push(pid);
        }

        for pid in &pids {
            for i in 0..100 {
                rt.send(pid, Message::text(format!("m-{i}"))).unwrap();
            }
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if total.load(Ordering::Relaxed) >= 10_000 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let c = total.load(Ordering::Relaxed);
                rt.shutdown();
                panic!("timed out: {c}/10000");
            }
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(total.load(Ordering::Relaxed), 10_000);
        rt.shutdown();
    }
}
```

**Step 2: Register in scheduler/mod.rs**

Add to `zeptovm/src/scheduler/mod.rs`:

```rust
pub mod runtime;
```

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::runtime::tests -- --test-threads=1`
Expected: 4 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/scheduler/runtime.rs zeptovm/src/scheduler/mod.rs
git commit -m "feat(zeptovm): add multi-threaded SchedulerRuntime with work stealing"
```

---

## Task 10: Create I/O Reactor Bridge

**Files:**
- Create: `zeptovm/src/scheduler/reactor.rs`
- Modify: `zeptovm/src/scheduler/mod.rs`

**Step 1: Write the reactor bridge**

Create `zeptovm/src/scheduler/reactor.rs`:

```rust
use std::sync::Arc;

use crossbeam_channel::{self, Receiver, Sender};
use tokio::runtime::Runtime as TokioRuntime;

use crate::{
    error::{IoFuture, IoResult, Message},
    pid::Pid,
};

/// Request to submit a future to the reactor.
pub struct IoSubmit {
    pub pid: Pid,
    pub future: IoFuture,
}

/// Completion notification from the reactor.
pub struct IoComplete {
    pub pid: Pid,
    pub result: IoResult,
}

/// The I/O reactor: a shared Tokio runtime that polls futures on behalf of
/// suspended processes. Communication is via crossbeam channels.
pub struct Reactor {
    /// Channel for submitting futures (scheduler → reactor).
    submit_tx: Sender<IoSubmit>,
    /// Channel for receiving completions (reactor → scheduler).
    complete_rx: Receiver<IoComplete>,
    /// Handle to the Tokio runtime (kept alive).
    _tokio_rt: Arc<TokioRuntime>,
}

impl Reactor {
    /// Create a new reactor with a shared Tokio runtime.
    pub fn new() -> Self {
        let tokio_rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("io-reactor")
                .build()
                .expect("failed to build tokio runtime for reactor"),
        );

        let (submit_tx, submit_rx) = crossbeam_channel::unbounded::<IoSubmit>();
        let (complete_tx, complete_rx) = crossbeam_channel::unbounded::<IoComplete>();

        // Spawn a dispatcher task that reads submit_rx and spawns tokio tasks
        let rt_clone = tokio_rt.clone();
        std::thread::Builder::new()
            .name("io-reactor-dispatch".into())
            .spawn(move || {
                for req in submit_rx {
                    let tx = complete_tx.clone();
                    rt_clone.spawn(async move {
                        let result = req.future.await;
                        let _ = tx.send(IoComplete {
                            pid: req.pid,
                            result,
                        });
                    });
                }
            })
            .expect("failed to spawn reactor dispatch thread");

        Self {
            submit_tx,
            complete_rx,
            _tokio_rt: tokio_rt,
        }
    }

    /// Submit a future for async execution. The process is suspended until
    /// the reactor returns a completion.
    pub fn submit(&self, pid: Pid, future: IoFuture) {
        let _ = self.submit_tx.send(IoSubmit { pid, future });
    }

    /// Non-blocking: try to receive a completed I/O result.
    pub fn try_recv(&self) -> Option<IoComplete> {
        self.complete_rx.try_recv().ok()
    }

    /// Get a clone of the submit sender (for use by scheduler threads).
    pub fn submit_sender(&self) -> Sender<IoSubmit> {
        self.submit_tx.clone()
    }

    /// Get a clone of the complete receiver (for use by scheduler threads).
    pub fn complete_receiver(&self) -> Receiver<IoComplete> {
        self.complete_rx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_reactor_submit_and_complete() {
        let reactor = Reactor::new();
        let pid = Pid::from_raw(1);

        // Submit a future that completes immediately
        let future: IoFuture = Box::pin(async {
            Ok(Message::text("done"))
        });
        reactor.submit(pid, future);

        // Wait for completion
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(complete) = reactor.try_recv() {
                assert_eq!(complete.pid, pid);
                assert!(complete.result.is_ok());
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("reactor did not complete future");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn test_reactor_async_delay() {
        let reactor = Reactor::new();
        let pid = Pid::from_raw(2);

        // Submit a future that takes some time
        let future: IoFuture = Box::pin(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Message::text("delayed"))
        });
        reactor.submit(pid, future);

        // Should not be available immediately
        assert!(reactor.try_recv().is_none());

        // But should complete eventually
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(complete) = reactor.try_recv() {
                assert_eq!(complete.pid, pid);
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("delayed future did not complete");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn test_reactor_error_result() {
        let reactor = Reactor::new();
        let pid = Pid::from_raw(3);

        let future: IoFuture = Box::pin(async {
            Err("network error".to_string())
        });
        reactor.submit(pid, future);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(complete) = reactor.try_recv() {
                assert_eq!(complete.pid, pid);
                assert!(complete.result.is_err());
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("error future did not complete");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn test_reactor_multiple_concurrent() {
        let reactor = Reactor::new();
        let pids: Vec<Pid> = (0..10).map(|i| Pid::from_raw(i + 100)).collect();

        for pid in &pids {
            let p = *pid;
            let future: IoFuture = Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(Message::text(format!("result-{}", p.raw())))
            });
            reactor.submit(*pid, future);
        }

        let mut completed = 0u32;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while completed < 10 {
            if let Some(_) = reactor.try_recv() {
                completed += 1;
            }
            if std::time::Instant::now() >= deadline {
                panic!("only {completed}/10 futures completed");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(completed, 10);
    }
}
```

**Step 2: Register in scheduler/mod.rs**

Add to `zeptovm/src/scheduler/mod.rs`:

```rust
pub mod reactor;
```

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::reactor::tests -- --test-threads=1`
Expected: 4 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/scheduler/reactor.rs zeptovm/src/scheduler/mod.rs
git commit -m "feat(zeptovm): add I/O Reactor bridge — Tokio runtime for async futures"
```

---

## Task 11: Wire Suspend/Resume into Scheduler Runtime

**Files:**
- Modify: `zeptovm/src/scheduler/runtime.rs`

This task integrates the reactor into the multi-threaded scheduler. When a process returns `StepResult::Suspend(future)`, the scheduler submits it to the reactor. Each tick, the scheduler checks for I/O completions and re-enqueues the process with the result delivered as a message.

**Step 1: Write the failing integration test**

Add to `zeptovm/src/scheduler/runtime.rs` tests module:

```rust
#[test]
fn test_runtime_suspend_resume() {
    use crate::error::IoFuture;

    struct SuspendOnce {
        suspended: bool,
        completed: Arc<AtomicU32>,
    }
    impl StepBehavior for SuspendOnce {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            if !self.suspended {
                self.suspended = true;
                let future: IoFuture = Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(Message::text("io-result"))
                });
                StepResult::Suspend(future)
            } else {
                // Second message should be the I/O result
                self.completed.fetch_add(1, Ordering::Relaxed);
                StepResult::Done(Reason::Normal)
            }
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    let completed = Arc::new(AtomicU32::new(0));
    let rt = SchedulerRuntime::new(2);
    let pid = rt.spawn(
        Box::new(SuspendOnce {
            suspended: false,
            completed: completed.clone(),
        }),
        None,
    );

    // Send first message → triggers Suspend
    rt.send(&pid, Message::text("trigger-io")).unwrap();

    // Wait for I/O completion and second handle call
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if completed.load(Ordering::Relaxed) >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            rt.shutdown();
            panic!("suspend/resume did not complete");
        }
        thread::sleep(Duration::from_millis(10));
    }

    rt.shutdown();
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib scheduler::runtime::tests::test_runtime_suspend_resume -- --test-threads=1`
Expected: FAIL — Suspend future is dropped, I/O never submitted

**Step 3: Integrate reactor into runtime**

Modify `SchedulerRuntime::new()` to create a `Reactor` and pass its channels to scheduler threads. Modify `scheduler_thread_loop` to:

1. On `StepResult::Suspend(future)`: call `reactor_submit_tx.send(IoSubmit { pid, future })`
2. Each loop iteration: `while let Ok(complete) = io_complete_rx.try_recv()` → push result as message to process, mark Ready, enqueue

The key changes to `runtime.rs`:

In `SchedulerRuntime::new()`:
```rust
use crate::scheduler::reactor::Reactor;

// After creating channels, before spawning threads:
let reactor = Reactor::new();
let reactor_submit_tx = reactor.submit_sender();
let io_complete_rx = reactor.complete_receiver();
```

Pass `reactor_submit_tx.clone()` and `io_complete_rx.clone()` to each scheduler thread.

In `scheduler_thread_loop`, add parameters:
```rust
fn scheduler_thread_loop(
    run_queue: RunQueue,
    processes: Arc<DashMap<Pid, ProcessEntry>>,
    global_queue: Arc<GlobalQueue>,
    peer_stealers: Vec<Stealer<Pid>>,
    shutdown: Arc<AtomicBool>,
    cmd_rx: Receiver<Command>,
    notify_tx: Sender<Notification>,
    reactor_submit: Sender<crate::scheduler::reactor::IoSubmit>,
    io_complete: Receiver<crate::scheduler::reactor::IoComplete>,
)
```

At top of loop, drain I/O completions:
```rust
// 0. Check I/O completions
while let Ok(complete) = io_complete.try_recv() {
    if let Some(mut entry) = processes.get_mut(&complete.pid) {
        let msg = match complete.result {
            Ok(msg) => msg,
            Err(err) => Message::text(format!("io_error: {err}")),
        };
        entry.mailbox.push_user(msg);
        if entry.state == ProcessState::Suspended || entry.state == ProcessState::Waiting {
            entry.state = ProcessState::Ready;
            drop(entry);
            run_queue.push(complete.pid);
        }
    }
}
```

On `StepResult::Suspend(future)`:
```rust
StepResult::Suspend(future) => {
    entry.state = ProcessState::Suspended;
    let pid_copy = pid;
    drop(entry);
    let _ = reactor_submit.send(crate::scheduler::reactor::IoSubmit {
        pid: pid_copy,
        future,
    });
}
```

Store reactor in SchedulerRuntime struct to keep it alive:
```rust
pub struct SchedulerRuntime {
    // ... existing fields ...
    _reactor: Reactor,
}
```

**Step 4: Run test to verify it passes**

Run: `cd zeptovm && cargo test --lib scheduler::runtime::tests -- --test-threads=1`
Expected: All runtime tests PASS including `test_runtime_suspend_resume`

**Step 5: Commit**

```bash
git add zeptovm/src/scheduler/runtime.rs
git commit -m "feat(zeptovm): wire I/O reactor into scheduler — Suspend/Resume lifecycle"
```

---

## Task 12: v1 Gate Test — Suspend on I/O, Result as Message

**Files:**
- Create: `zeptovm/tests/sched_v1_gate.rs`

**Step 1: Write the gate test**

Create `zeptovm/tests/sched_v1_gate.rs`:

```rust
//! Scheduler v1 Gate Test: processes can suspend on I/O and resume when complete.

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use zeptovm::{
    error::{IoFuture, Message, Reason, StepResult},
    scheduler::runtime::SchedulerRuntime,
    step_behavior::StepBehavior,
};

/// Process that suspends on first message (simulating LLM API call),
/// then processes the I/O result.
struct LlmAgent {
    io_results: Arc<AtomicU32>,
}

impl StepBehavior for LlmAgent {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
        StepResult::Continue
    }

    fn handle(&mut self, msg: Message) -> StepResult {
        // Check if this is an I/O result (from reactor) or a user trigger
        match &msg {
            Message::User(payload) => {
                if let crate::error::UserPayload::Text(s) = payload {
                    if s.starts_with("io_result:") || s.starts_with("io_error:") {
                        // This is the I/O completion — count it
                        self.io_results.fetch_add(1, Ordering::Relaxed);
                        return StepResult::Done(Reason::Normal);
                    }
                }
            }
        }

        // User trigger → simulate LLM API call
        let future: IoFuture = Box::pin(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(Message::text("io_result: LLM response"))
        });
        StepResult::Suspend(future)
    }

    fn terminate(&mut self, _reason: &Reason) {}
}

#[test]
fn sched_gate_v1_suspend_resume_single() {
    let io_results = Arc::new(AtomicU32::new(0));
    let rt = SchedulerRuntime::new(2);

    let pid = rt.spawn(
        Box::new(LlmAgent {
            io_results: io_results.clone(),
        }),
        None,
    );

    // Trigger the LLM call
    rt.send(&pid, Message::text("call LLM")).unwrap();

    // Wait for the I/O result to be processed
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if io_results.load(Ordering::Relaxed) >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            rt.shutdown();
            panic!("v1 gate: suspend/resume did not complete");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    rt.shutdown();
}

#[test]
fn sched_gate_v1_concurrent_io() {
    let io_results = Arc::new(AtomicU32::new(0));
    let rt = SchedulerRuntime::new(4);

    // Spawn 50 agents, each doing one I/O call
    let mut pids = Vec::new();
    for _ in 0..50 {
        let pid = rt.spawn(
            Box::new(LlmAgent {
                io_results: io_results.clone(),
            }),
            None,
        );
        pids.push(pid);
    }

    // Trigger all at once
    for pid in &pids {
        rt.send(pid, Message::text("call LLM")).unwrap();
    }

    // Wait for all 50 to complete
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if io_results.load(Ordering::Relaxed) >= 50 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let c = io_results.load(Ordering::Relaxed);
            rt.shutdown();
            panic!("v1 gate: only {c}/50 I/O results processed");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(io_results.load(Ordering::Relaxed), 50);
    rt.shutdown();
}

#[test]
fn sched_gate_v1_io_does_not_block_others() {
    let io_results = Arc::new(AtomicU32::new(0));
    let cpu_count = Arc::new(AtomicU32::new(0));
    let rt = SchedulerRuntime::new(2);

    // Spawn one agent that does slow I/O
    let slow_pid = rt.spawn(
        Box::new(LlmAgent {
            io_results: io_results.clone(),
        }),
        None,
    );

    // Spawn one CPU-bound agent
    struct CpuWorker {
        count: Arc<AtomicU32>,
    }
    impl StepBehavior for CpuWorker {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            self.count.fetch_add(1, Ordering::Relaxed);
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    let cpu_pid = rt.spawn(
        Box::new(CpuWorker {
            count: cpu_count.clone(),
        }),
        None,
    );

    // Trigger slow I/O
    let slow_future: IoFuture = Box::pin(async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(Message::text("io_result: slow"))
    });
    rt.send(&slow_pid, Message::text("call LLM")).unwrap();

    // Send CPU messages immediately
    for i in 0..100 {
        rt.send(&cpu_pid, Message::text(format!("work-{i}")))
            .unwrap();
    }

    // CPU worker should process messages while I/O is pending
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if cpu_count.load(Ordering::Relaxed) >= 100 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let c = cpu_count.load(Ordering::Relaxed);
            rt.shutdown();
            panic!("CPU worker blocked by I/O: only {c}/100 processed");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    rt.shutdown();
}
```

**Step 2: Run gate tests**

Run: `cd zeptovm && cargo test --test sched_v1_gate -- --test-threads=1`
Expected: 3 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/tests/sched_v1_gate.rs
git commit -m "test(zeptovm): add scheduler v1 gate tests — Suspend/Resume I/O lifecycle"
```

---

## Task 13: Crash Isolation with catch_unwind

**Files:**
- Modify: `zeptovm/src/scheduler/process.rs`
- Modify: `zeptovm/src/scheduler/runtime.rs`

**Step 1: Write the failing test**

Add to `zeptovm/src/scheduler/process.rs` tests module:

```rust
#[test]
fn test_process_panic_returns_done() {
    struct Panicker;
    impl StepBehavior for Panicker {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
            StepResult::Continue
        }
        fn handle(&mut self, _msg: Message) -> StepResult {
            panic!("intentional panic");
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::new();
    let mut proc = ProcessEntry::new(pid, Box::new(Panicker));
    proc.init(None);
    proc.mailbox.push_user(Message::text("boom"));

    let result = proc.step_safe();
    assert!(matches!(result, StepResult::Done(Reason::Custom(_))));
}
```

**Step 2: Run test to verify it fails**

Run: `cd zeptovm && cargo test --lib scheduler::process::tests::test_process_panic_returns_done`
Expected: FAIL — `step_safe` doesn't exist

**Step 3: Add step_safe() to ProcessEntry**

In `process.rs`, add:

```rust
use std::panic::{self, AssertUnwindSafe};

impl ProcessEntry {
    /// Step with panic isolation. If handle() panics, returns Done(Crash).
    pub fn step_safe(&mut self) -> StepResult {
        // Kill check doesn't need catch_unwind
        if self.kill_requested {
            self.state = ProcessState::Done;
            return StepResult::Done(Reason::Kill);
        }

        // Control messages don't need catch_unwind (our code, not user code)
        while let Some(ctrl) = self.mailbox.pop_control() {
            match ctrl {
                SystemMsg::ExitLinked(_from, ref reason) => {
                    if reason.is_abnormal() {
                        self.state = ProcessState::Done;
                        return StepResult::Done(reason.clone());
                    }
                }
                SystemMsg::MonitorDown(_, _) => {}
                SystemMsg::Suspend => {
                    self.state = ProcessState::Suspended;
                    return StepResult::Wait;
                }
                SystemMsg::Resume => {}
                SystemMsg::GetState(_tx) => {}
            }
        }

        match self.mailbox.pop_user() {
            Some(msg) => {
                self.reductions += 1;
                // Wrap behavior.handle() in catch_unwind
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    self.behavior.handle(msg)
                }));
                match result {
                    Ok(step) => step,
                    Err(panic_info) => {
                        let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                            format!("panic: {s}")
                        } else if let Some(s) = panic_info.downcast_ref::<String>() {
                            format!("panic: {s}")
                        } else {
                            "panic: unknown".to_string()
                        };
                        self.state = ProcessState::Done;
                        StepResult::Done(Reason::Custom(msg))
                    }
                }
            }
            None => StepResult::Wait,
        }
    }
}
```

**Step 4: Update scheduler_thread_loop to use step_safe()**

In `runtime.rs`, change `entry.step()` to `entry.step_safe()`.

**Step 5: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::process::tests -- --test-threads=1`
Expected: All tests PASS including panic test

**Step 6: Commit**

```bash
git add zeptovm/src/scheduler/process.rs zeptovm/src/scheduler/runtime.rs
git commit -m "feat(zeptovm): add crash isolation — catch_unwind around handle()"
```

---

## Task 14: Scheduler Collapse

**Files:**
- Modify: `zeptovm/src/scheduler/runtime.rs`

When all queues are empty, idle threads should park (sleep) instead of busy-waiting. They wake when new work appears.

**Step 1: Write the test**

Add to `runtime.rs` tests:

```rust
#[test]
fn test_runtime_idle_does_not_burn_cpu() {
    // Start runtime with 4 threads but no processes
    let rt = SchedulerRuntime::new(4);

    // Sleep 200ms — if threads are busy-waiting, CPU usage would be high
    // (we can't measure CPU usage easily in a test, but we verify the
    // runtime doesn't hang or panic with no work)
    std::thread::sleep(Duration::from_millis(200));

    // Should still be responsive
    let count = Arc::new(AtomicU32::new(0));
    let pid = rt.spawn(
        Box::new(CountStep {
            count: count.clone(),
        }),
        None,
    );
    rt.send(&pid, Message::text("hello")).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if count.load(Ordering::Relaxed) >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            rt.shutdown();
            panic!("runtime not responsive after idle");
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    rt.shutdown();
}
```

**Step 2: Implement scheduler collapse**

Replace the busy-wait `thread::sleep(Duration::from_micros(100))` in `scheduler_thread_loop` with progressive backoff:

```rust
// No work — progressive backoff (scheduler collapse)
if idle_rounds < 10 {
    // Spin briefly (hot path)
    std::hint::spin_loop();
    idle_rounds += 1;
} else if idle_rounds < 100 {
    // Short sleep
    thread::sleep(Duration::from_micros(50));
    idle_rounds += 1;
} else {
    // Deep sleep (collapsed)
    thread::sleep(Duration::from_millis(1));
}
continue;
```

Reset `idle_rounds = 0` when work is found.

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --lib scheduler::runtime::tests -- --test-threads=1`
Expected: All PASS

**Step 4: Commit**

```bash
git add zeptovm/src/scheduler/runtime.rs
git commit -m "feat(zeptovm): add scheduler collapse — progressive backoff when idle"
```

---

## Task 15: Exit Signal Propagation via LinkTable

**Files:**
- Modify: `zeptovm/src/scheduler/runtime.rs`
- Create: `zeptovm/tests/sched_v3_links.rs`

**Step 1: Add link/monitor support to SchedulerRuntime**

Add a `LinkTable` field to `SchedulerRuntime` (wrapped in `Arc<RwLock<LinkTable>>`). Add methods:

```rust
pub fn link(&self, a: Pid, b: Pid) {
    self.links.write().unwrap().link(a, b);
}

pub fn monitor(&self, watcher: Pid, target: Pid) -> MonitorRef {
    self.links.write().unwrap().monitor(watcher, target)
}
```

When a process exits (`Notification::Exited`), propagate exit signals to linked and monitoring processes via the command channel.

**Step 2: Write the link test**

Create `zeptovm/tests/sched_v3_links.rs`:

```rust
//! Scheduler v3: exit signal propagation via links.

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use zeptovm::{
    error::{Message, Reason, StepResult},
    scheduler::runtime::SchedulerRuntime,
    step_behavior::StepBehavior,
};

struct DieOnMessage;
impl StepBehavior for DieOnMessage {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
        StepResult::Continue
    }
    fn handle(&mut self, _msg: Message) -> StepResult {
        StepResult::Done(Reason::Custom("crash".into()))
    }
    fn terminate(&mut self, _reason: &Reason) {}
}

struct WaitForever {
    terminated: Arc<AtomicU32>,
}
impl StepBehavior for WaitForever {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
        StepResult::Continue
    }
    fn handle(&mut self, _msg: Message) -> StepResult {
        StepResult::Continue
    }
    fn terminate(&mut self, _reason: &Reason) {
        self.terminated.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn sched_v3_linked_process_dies_on_abnormal() {
    let terminated = Arc::new(AtomicU32::new(0));
    let rt = SchedulerRuntime::new(2);

    let pid_a = rt.spawn(Box::new(DieOnMessage), None);
    let pid_b = rt.spawn(
        Box::new(WaitForever {
            terminated: terminated.clone(),
        }),
        None,
    );

    rt.link(pid_a, pid_b);

    // Kill A with abnormal exit
    rt.send(&pid_a, Message::text("die")).unwrap();

    // B should also die
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if terminated.load(Ordering::Relaxed) >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            rt.shutdown();
            panic!("linked process B did not terminate");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    rt.shutdown();
}
```

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --test sched_v3_links -- --test-threads=1`
Expected: PASS

**Step 4: Commit**

```bash
git add zeptovm/src/scheduler/runtime.rs zeptovm/tests/sched_v3_links.rs
git commit -m "feat(zeptovm): add link/monitor support and exit signal propagation to scheduler"
```

---

## Summary of Modules After All Tasks

```
zeptovm/src/
├── lib.rs                    # + pub mod scheduler, pub mod step_behavior
├── behavior.rs               # Original async trait (kept for migration)
├── step_behavior.rs          # NEW: sync step-based Behavior trait
├── error.rs                  # + StepResult, IoResult, IoFuture
├── pid.rs                    # Unchanged
├── link.rs                   # Unchanged
├── mailbox.rs                # Original channel-based (kept for migration)
├── process.rs                # Original tokio-based (kept for migration)
├── registry.rs               # Original (kept for migration)
├── supervisor.rs             # Original (kept for migration)
├── scheduler/
│   ├── mod.rs                # Module declarations
│   ├── mailbox.rs            # VecDeque-based scheduler mailbox
│   ├── process.rs            # ProcessEntry state machine
│   ├── run_queue.rs          # RunQueue + GlobalQueue (crossbeam-deque)
│   ├── engine.rs             # Single-thread SchedulerEngine
│   ├── reactor.rs            # I/O Reactor (Tokio bridge)
│   └── runtime.rs            # Multi-thread SchedulerRuntime
└── durability.rs             # Deferred to v4

zeptovm/tests/
├── v0_gate.rs                # Original gate test (tokio-based)
├── v1_gate.rs                # Original supervision test
├── v1_exit_signals.rs        # Original exit signal test
├── sched_v0_gate.rs          # NEW: scheduler v0 gate
├── sched_v1_gate.rs          # NEW: scheduler v1 gate (I/O)
└── sched_v3_links.rs         # NEW: scheduler v3 links
```

## Migration Strategy

The old tokio-based modules (`behavior.rs`, `process.rs`, `mailbox.rs`, `registry.rs`, `supervisor.rs`) are kept intact. The new scheduler modules live alongside them in `scheduler/`. This allows:

1. **Incremental adoption**: New code uses `StepBehavior` + `SchedulerRuntime`
2. **Parallel testing**: Old and new tests both run in CI
3. **Future cleanup**: Once all consumers migrate, old modules can be removed

## Deferred Work (Future Plans)

| Stage | Scope | When |
|-------|-------|------|
| v4 | Checkpoint, WAL, recovery | After v3 stabilizes |
| v5 | Budget gate, provider gate, admission control | After v4 |
| Migration | Rewrite supervisor to use SchedulerRuntime | After v3 |
| Cleanup | Remove old tokio-based process/mailbox/behavior | After migration |
