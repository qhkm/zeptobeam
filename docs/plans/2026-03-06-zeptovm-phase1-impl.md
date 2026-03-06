# ZeptoVM Phase 1 — Single-Node Durable Runtime Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the single-node durable runtime kernel: process model, multi-lane mailbox, turn executor, async-aware scheduler, explicit effects with journaling, snapshots, supervision, and budget basics.

**Architecture:** Processes are state machines stepped by N scheduler threads. Side effects are explicit EffectRequests (not opaque futures) — journaled before dispatch, results recorded on completion. A shared Tokio reactor handles async I/O. Turn commits are atomic: state delta + journal entries + outbound messages + effect requests in one transaction.

**Tech Stack:** Rust (nightly), crossbeam-deque, crossbeam-channel, tokio (reactor only), rusqlite (journal/snapshots), dashmap, tracing, serde

**Design Doc:** `docs/plans/2026-03-06-zeptovm-canonical-design.md`

**Runtime Spec:** `docs/plans/2026-03-06-zeptovm-runtime-spec-v01.md`

**Supersedes:** `docs/plans/2026-03-06-zeptovm-async-scheduler-impl.md` (v1 plan, `Suspend(Future)` approach)

### Spec Refinements (from runtime-spec-v01)

Three changes from the spec that refine this plan:

1. **TurnContext** — `handle()` takes `(msg, &mut TurnContext)`, not just `(msg)`. Handlers emit intents (send, spawn, link, timer, effect, budget debit, state patch) through the context. The runtime collects intents and commits them atomically. `StepResult` only says what the process wants to do *next* (Continue/Wait/Suspend/Done/Fail).

2. **ExitReason** — Replaces our `Reason` enum with a richer set: Normal, Cancelled, Timeout, BudgetExceeded, PolicyDenied, DependencyFailed, Panic, Error, Killed, NodeLost. Used consistently across ProcessStatus, StepResult, supervision, and journal entries.

3. **TurnIntent + TurnCommit** — The turn executor collects `Vec<TurnIntent>` from TurnContext, builds a `TurnCommit` struct (turn_id, consumed_msg_id, state_patch, outbound_messages, effect_requests, timer_registrations, budget_changes, journal_entries, next_status), and persists it atomically. Only after commit: dispatch effects, deliver messages, fire timers.

**Implementation approach:** Tasks 1-7 build the core types and in-memory kernel. We use `serde_json::Value` for effect inputs (not the full `EffectInput` enum) to keep v0.1 simple. Full typed inputs are Phase 2. Similarly, Pid stays as `u64` internally (the rich Pid with tenant/namespace/incarnation is Phase 3 cluster work). ExitReason replaces Reason from Task 2 onward.

---

## Existing Codebase Reference

| File | Current Role | Phase 1 Impact |
|------|-------------|----------------|
| `src/behavior.rs` | `async fn handle() -> Action` | Kept for migration; new `StepBehavior` |
| `src/process.rs` | `tokio::spawn` per process | Kept; new `kernel/process_table.rs` |
| `src/mailbox.rs` | tokio mpsc 2-channel | Kept; new `kernel/mailbox.rs` 5-lane |
| `src/error.rs` | Reason, Action, Message | Extended with StepResult, EffectRequest, etc. |
| `src/pid.rs` | Atomic u64 | Stays as-is |
| `src/link.rs` | LinkTable | Stays as-is |
| `src/registry.rs` | DashMap + JoinHandle | Kept; new kernel manages processes |
| `src/supervisor.rs` | tokio-based supervision | Kept; new kernel supervisor later |
| `Cargo.toml` | tokio, async-trait, dashmap | Add crossbeam-deque, crossbeam-channel |

---

## Task 1: Add Dependencies and Create Module Skeleton

**Files:**
- Modify: `zeptovm/Cargo.toml`
- Create: `zeptovm/src/core/mod.rs`
- Create: `zeptovm/src/kernel/mod.rs`
- Create: `zeptovm/src/durability/mod.rs`
- Modify: `zeptovm/src/lib.rs`

**Step 1: Add crossbeam dependencies**

Add after the `futures` line in `Cargo.toml`:

```toml
crossbeam-deque = "0.8"
crossbeam-channel = "0.5"
```

**Step 2: Create module skeleton**

Create `zeptovm/src/core/mod.rs`:
```rust
pub mod effect;
pub mod message;
pub mod step_result;
```

Create `zeptovm/src/kernel/mod.rs`:
```rust
pub mod mailbox;
pub mod process_table;
pub mod run_queue;
pub mod scheduler;
pub mod reactor;
pub mod turn_executor;
```

Create `zeptovm/src/durability/mod.rs`:
```rust
pub mod journal;
pub mod snapshot;
```

**Step 3: Register modules in lib.rs**

Add to `zeptovm/src/lib.rs`:
```rust
pub mod core;
pub mod kernel;
pub mod durability;
```

**Step 4: Create empty files for all submodules**

Create each file with just `// TODO` placeholder so the skeleton compiles.

**Step 5: Build to verify skeleton compiles**

Run: `cd zeptovm && cargo build`
Expected: BUILD SUCCESS (with warnings about empty modules)

**Step 6: Run existing tests**

Run: `cd zeptovm && cargo test`
Expected: All existing tests still pass

**Step 7: Commit**

```bash
git add zeptovm/Cargo.toml zeptovm/src/core/ zeptovm/src/kernel/ zeptovm/src/durability/ zeptovm/src/lib.rs
git commit -m "chore(zeptovm): add crossbeam deps and core/kernel/durability module skeleton"
```

---

## Task 2: Define Core Types — EffectRequest, EffectResult, EffectKind

**Files:**
- Create: `zeptovm/src/core/effect.rs`
- Test: inline

These are the types that make effects explicit and journalable.

**Step 1: Write the failing test**

In `zeptovm/src/core/effect.rs`, write the full module with tests:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

static NEXT_EFFECT_ID: AtomicU64 = AtomicU64::new(1);

/// Unique identifier for an effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectId(u64);

impl EffectId {
    pub fn new() -> Self {
        Self(NEXT_EFFECT_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub fn raw(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for EffectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "eff-{}", self.0)
    }
}

/// What kind of side effect this is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    LlmCall,
    Http,
    DbQuery,
    DbWrite,
    CliExec,
    SandboxExec,
    BrowserAutomation,
    PublishEvent,
    HumanApproval,
    HumanInput,
    SleepUntil,
    Spawn,
    ObjectFetch,
    ObjectPut,
    VectorSearch,
    Custom(String),
}

/// Retry policy for effects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// A process emits this to request a side effect.
/// This is what gets journaled, policy-checked, and dispatched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectRequest {
    pub effect_id: EffectId,
    pub kind: EffectKind,
    pub input: serde_json::Value,
    pub timeout: Duration,
    pub retry: RetryPolicy,
    pub idempotency_key: Option<String>,
}

impl EffectRequest {
    pub fn new(kind: EffectKind, input: serde_json::Value) -> Self {
        Self {
            effect_id: EffectId::new(),
            kind,
            input,
            timeout: Duration::from_secs(60),
            retry: RetryPolicy::default(),
            idempotency_key: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
}

/// Status of an effect after execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

/// Result returned by the effect worker plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectResult {
    pub effect_id: EffectId,
    pub status: EffectStatus,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl EffectResult {
    pub fn success(effect_id: EffectId, output: serde_json::Value) -> Self {
        Self {
            effect_id,
            status: EffectStatus::Succeeded,
            output: Some(output),
            error: None,
        }
    }

    pub fn failure(effect_id: EffectId, error: impl Into<String>) -> Self {
        Self {
            effect_id,
            status: EffectStatus::Failed,
            output: None,
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effect_id_unique() {
        let a = EffectId::new();
        let b = EffectId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_effect_request_builder() {
        let req = EffectRequest::new(
            EffectKind::LlmCall,
            serde_json::json!({"prompt": "hello"}),
        )
        .with_timeout(Duration::from_secs(30))
        .with_idempotency_key("call-123");

        assert_eq!(req.kind, EffectKind::LlmCall);
        assert_eq!(req.timeout, Duration::from_secs(30));
        assert_eq!(req.idempotency_key.as_deref(), Some("call-123"));
        assert_eq!(req.retry.max_attempts, 3);
    }

    #[test]
    fn test_effect_result_success() {
        let id = EffectId::new();
        let result = EffectResult::success(id, serde_json::json!("ok"));
        assert_eq!(result.status, EffectStatus::Succeeded);
        assert!(result.output.is_some());
        assert!(result.error.is_none());
    }

    #[test]
    fn test_effect_result_failure() {
        let id = EffectId::new();
        let result = EffectResult::failure(id, "network error");
        assert_eq!(result.status, EffectStatus::Failed);
        assert!(result.output.is_none());
        assert_eq!(result.error.as_deref(), Some("network error"));
    }

    #[test]
    fn test_effect_request_serializable() {
        let req = EffectRequest::new(
            EffectKind::Http,
            serde_json::json!({"url": "https://api.example.com"}),
        );
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: EffectRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.kind, EffectKind::Http);
    }

    #[test]
    fn test_effect_kind_custom() {
        let kind = EffectKind::Custom("my-tool".into());
        assert_eq!(kind, EffectKind::Custom("my-tool".into()));
    }
}
```

**Step 2: Run tests**

Run: `cd zeptovm && cargo test --lib core::effect::tests`
Expected: 6 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/src/core/effect.rs
git commit -m "feat(zeptovm): add EffectRequest, EffectResult, EffectKind — explicit journalable effects"
```

---

## Task 3: Define Core Types — Message Envelope and MessageClass

**Files:**
- Create: `zeptovm/src/core/message.rs`
- Test: inline

Replace the simple `Message::User(payload)` with a proper envelope.

**Step 1: Write the module with tests**

```rust
use serde::{Deserialize, Serialize};

use crate::core::effect::{EffectId, EffectResult};
use crate::pid::Pid;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_MSG_ID: AtomicU64 = AtomicU64::new(1);

/// Unique message identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MsgId(u64);

impl MsgId {
    pub fn new() -> Self {
        Self(NEXT_MSG_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Which mailbox lane this message routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageClass {
    Control,
    Supervisor,
    EffectResult,
    User,
    Background,
}

/// Priority for scheduling and mailbox ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
}

impl Default for Priority {
    fn default() -> Self {
        Priority::Normal
    }
}

/// The payload carried inside a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

/// A message envelope with routing metadata.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub msg_id: MsgId,
    pub correlation_id: Option<String>,
    pub from: Option<Pid>,
    pub to: Pid,
    pub class: MessageClass,
    pub priority: Priority,
    pub dedup_key: Option<String>,
    pub payload: EnvelopePayload,
}

/// What's inside the envelope — either a user payload or an effect result.
#[derive(Debug, Clone)]
pub enum EnvelopePayload {
    User(Payload),
    Effect(EffectResult),
    Signal(Signal),
}

/// System/supervisor signals.
#[derive(Debug, Clone)]
pub enum Signal {
    Kill,
    Shutdown,
    ExitLinked(Pid, crate::error::Reason),
    MonitorDown(Pid, crate::error::Reason),
    Suspend,
    Resume,
}

impl Envelope {
    /// Create a user message.
    pub fn user(to: Pid, payload: Payload) -> Self {
        Self {
            msg_id: MsgId::new(),
            correlation_id: None,
            from: None,
            to,
            class: MessageClass::User,
            priority: Priority::Normal,
            dedup_key: None,
            payload: EnvelopePayload::User(payload),
        }
    }

    /// Create a user text message (convenience).
    pub fn text(to: Pid, s: impl Into<String>) -> Self {
        Self::user(to, Payload::Text(s.into()))
    }

    /// Create a user JSON message (convenience).
    pub fn json(to: Pid, v: serde_json::Value) -> Self {
        Self::user(to, Payload::Json(v))
    }

    /// Create an effect result message.
    pub fn effect_result(to: Pid, result: EffectResult) -> Self {
        Self {
            msg_id: MsgId::new(),
            correlation_id: None,
            from: None,
            to,
            class: MessageClass::EffectResult,
            priority: Priority::High,
            dedup_key: None,
            payload: EnvelopePayload::Effect(result),
        }
    }

    /// Create a control signal.
    pub fn signal(to: Pid, signal: Signal) -> Self {
        Self {
            msg_id: MsgId::new(),
            correlation_id: None,
            from: None,
            to,
            class: MessageClass::Control,
            priority: Priority::High,
            dedup_key: None,
            payload: EnvelopePayload::Signal(signal),
        }
    }

    /// Builder: set correlation ID.
    pub fn with_correlation(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Builder: set sender.
    pub fn with_from(mut self, from: Pid) -> Self {
        self.from = Some(from);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;

    #[test]
    fn test_envelope_text() {
        let to = Pid::from_raw(1);
        let env = Envelope::text(to, "hello");
        assert_eq!(env.to, to);
        assert_eq!(env.class, MessageClass::User);
        assert!(matches!(
            env.payload,
            EnvelopePayload::User(Payload::Text(_))
        ));
    }

    #[test]
    fn test_envelope_effect_result() {
        use crate::core::effect::{EffectId, EffectResult};
        let to = Pid::from_raw(2);
        let result = EffectResult::success(EffectId::new(), serde_json::json!("ok"));
        let env = Envelope::effect_result(to, result);
        assert_eq!(env.class, MessageClass::EffectResult);
        assert_eq!(env.priority, Priority::High);
    }

    #[test]
    fn test_envelope_signal() {
        let to = Pid::from_raw(3);
        let env = Envelope::signal(to, Signal::Kill);
        assert_eq!(env.class, MessageClass::Control);
    }

    #[test]
    fn test_msg_id_unique() {
        let a = MsgId::new();
        let b = MsgId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_envelope_builder() {
        let to = Pid::from_raw(4);
        let from = Pid::from_raw(5);
        let env = Envelope::text(to, "hi")
            .with_from(from)
            .with_correlation("corr-123");
        assert_eq!(env.from, Some(from));
        assert_eq!(env.correlation_id.as_deref(), Some("corr-123"));
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::High > Priority::Normal);
        assert!(Priority::Normal > Priority::Low);
    }
}
```

**Step 2: Run tests**

Run: `cd zeptovm && cargo test --lib core::message::tests`
Expected: 6 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/src/core/message.rs
git commit -m "feat(zeptovm): add Envelope, MessageClass, Signal — typed multi-lane message model"
```

---

## Task 4: Define StepResult

**Files:**
- Create: `zeptovm/src/core/step_result.rs`
- Test: inline

**Step 1: Write the module**

```rust
use crate::core::effect::EffectRequest;
use crate::error::Reason;

/// Result of a single step of process execution.
///
/// Key design: Suspend takes EffectRequest (intent), not Future (opaque).
/// This preserves journaling, replay, policy checks, and budget interception.
pub enum StepResult {
    /// Handled message successfully, may have more to process.
    Continue,
    /// No messages in mailbox, want to wait.
    Wait,
    /// Requesting a side effect — will be journaled, policy-checked, dispatched.
    Suspend(EffectRequest),
    /// Process is finished normally.
    Done(Reason),
    /// Process encountered an error.
    Fail(Reason),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectKind, EffectRequest};
    use crate::error::Reason;

    #[test]
    fn test_step_result_continue() {
        let r = StepResult::Continue;
        assert!(matches!(r, StepResult::Continue));
    }

    #[test]
    fn test_step_result_suspend_effect() {
        let req = EffectRequest::new(
            EffectKind::LlmCall,
            serde_json::json!({"prompt": "test"}),
        );
        let effect_id = req.effect_id;
        let r = StepResult::Suspend(req);
        match r {
            StepResult::Suspend(req) => {
                assert_eq!(req.effect_id, effect_id);
                assert_eq!(req.kind, EffectKind::LlmCall);
            }
            _ => panic!("expected Suspend"),
        }
    }

    #[test]
    fn test_step_result_done() {
        let r = StepResult::Done(Reason::Normal);
        assert!(matches!(r, StepResult::Done(Reason::Normal)));
    }

    #[test]
    fn test_step_result_fail() {
        let r = StepResult::Fail(Reason::Custom("budget exceeded".into()));
        assert!(matches!(r, StepResult::Fail(Reason::Custom(_))));
    }
}
```

**Step 2: Run tests**

Run: `cd zeptovm && cargo test --lib core::step_result::tests`
Expected: 4 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/src/core/step_result.rs
git commit -m "feat(zeptovm): add StepResult — Suspend(EffectRequest) not Suspend(Future)"
```

---

## Task 5: Create StepBehavior Trait + TurnContext

**Files:**
- Create: `zeptovm/src/core/behavior.rs`
- Create: `zeptovm/src/core/turn_context.rs`
- Modify: `zeptovm/src/core/mod.rs`

The spec requires `handle(msg, &mut TurnContext)` — handlers emit intents (send, spawn, link, effect, timer, state patch) through the context. StepResult only says what the process does *next*.

**Step 1: Write TurnContext and TurnIntent**

Create `zeptovm/src/core/turn_context.rs`:

```rust
use crate::core::effect::EffectRequest;
use crate::core::message::Envelope;
use crate::pid::Pid;

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

pub type TurnId = u64;

pub fn next_turn_id() -> TurnId {
    NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed)
}

/// An intent emitted by a behavior handler during a turn.
/// Collected by TurnContext, committed atomically by the turn executor.
#[derive(Debug)]
pub enum TurnIntent {
    SendMessage(Envelope),
    RequestEffect(EffectRequest),
    PatchState(Vec<u8>),
    // Phase 2: SpawnProcess, Link, Monitor, ScheduleTimer, DebitBudget
}

/// Context passed to behavior.handle(). Handlers emit intents through this.
pub struct TurnContext {
    pub pid: Pid,
    pub turn_id: TurnId,
    intents: Vec<TurnIntent>,
}

impl TurnContext {
    pub fn new(pid: Pid) -> Self {
        Self {
            pid,
            turn_id: next_turn_id(),
            intents: Vec::new(),
        }
    }

    /// Emit a message to another process.
    pub fn send(&mut self, msg: Envelope) {
        self.intents.push(TurnIntent::SendMessage(msg));
    }

    /// Convenience: send a text message to a pid.
    pub fn send_text(&mut self, to: Pid, text: impl Into<String>) {
        self.send(Envelope::text(to, text));
    }

    /// Request an effect (LLM call, HTTP, etc.).
    pub fn request_effect(&mut self, req: EffectRequest) {
        self.intents.push(TurnIntent::RequestEffect(req));
    }

    /// Patch process state (serialized blob).
    pub fn set_state(&mut self, state: Vec<u8>) {
        self.intents.push(TurnIntent::PatchState(state));
    }

    /// Take all collected intents (consumed by turn executor).
    pub fn take_intents(&mut self) -> Vec<TurnIntent> {
        std::mem::take(&mut self.intents)
    }

    /// How many intents have been emitted.
    pub fn intent_count(&self) -> usize {
        self.intents.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectKind, EffectRequest};
    use crate::pid::Pid;

    #[test]
    fn test_turn_context_send() {
        let pid = Pid::from_raw(1);
        let mut ctx = TurnContext::new(pid);
        ctx.send_text(Pid::from_raw(2), "hello");
        assert_eq!(ctx.intent_count(), 1);
    }

    #[test]
    fn test_turn_context_request_effect() {
        let pid = Pid::from_raw(1);
        let mut ctx = TurnContext::new(pid);
        let req = EffectRequest::new(EffectKind::LlmCall, serde_json::json!({}));
        ctx.request_effect(req);
        assert_eq!(ctx.intent_count(), 1);
    }

    #[test]
    fn test_turn_context_take_intents() {
        let pid = Pid::from_raw(1);
        let mut ctx = TurnContext::new(pid);
        ctx.send_text(Pid::from_raw(2), "a");
        ctx.send_text(Pid::from_raw(3), "b");
        let intents = ctx.take_intents();
        assert_eq!(intents.len(), 2);
        assert_eq!(ctx.intent_count(), 0);
    }

    #[test]
    fn test_turn_id_unique() {
        let a = next_turn_id();
        let b = next_turn_id();
        assert_ne!(a, b);
    }
}
```

**Step 2: Write the behavior trait**

Create `zeptovm/src/core/behavior.rs`:

```rust
use crate::core::message::Envelope;
use crate::core::step_result::StepResult;
use crate::core::turn_context::TurnContext;
use crate::error::Reason;

/// Step-based process behavior.
///
/// handle() is synchronous and takes a TurnContext for emitting intents.
/// Side effects go through ctx.request_effect() and return Suspend(EffectRequest).
/// Outbound messages go through ctx.send(). State patches through ctx.set_state().
/// The runtime collects all intents and commits them atomically.
pub trait StepBehavior: Send + 'static {
    /// Called once at spawn. checkpoint is Some if recovering.
    fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult;

    /// Called for each message. Emit intents via ctx; return what to do next.
    fn handle(&mut self, msg: Envelope, ctx: &mut TurnContext) -> StepResult;

    /// Called when the process is terminating.
    fn terminate(&mut self, reason: &Reason);

    /// Opt-in: serialize state for snapshot.
    fn snapshot(&self) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectKind, EffectRequest, EffectResult};
    use crate::core::message::{Envelope, EnvelopePayload, Payload};
    use crate::error::Reason;
    use crate::pid::Pid;

    struct EchoStep;
    impl StepBehavior for EchoStep {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
        fn handle(&mut self, _msg: Envelope, _ctx: &mut TurnContext) -> StepResult {
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    struct LlmAgent { awaiting_response: bool }
    impl StepBehavior for LlmAgent {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }

        fn handle(&mut self, msg: Envelope, ctx: &mut TurnContext) -> StepResult {
            match &msg.payload {
                EnvelopePayload::Effect(result) => {
                    // Got LLM response — forward to requester, then done
                    self.awaiting_response = false;
                    StepResult::Done(Reason::Normal)
                }
                EnvelopePayload::User(_) if !self.awaiting_response => {
                    // User trigger → request LLM call via context
                    self.awaiting_response = true;
                    let req = EffectRequest::new(
                        EffectKind::LlmCall,
                        serde_json::json!({"prompt": "hello"}),
                    );
                    StepResult::Suspend(req)
                }
                _ => StepResult::Continue,
            }
        }

        fn terminate(&mut self, _reason: &Reason) {}
    }

    /// Agent that sends outbound messages via TurnContext
    struct Forwarder { target: Pid }
    impl StepBehavior for Forwarder {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
        fn handle(&mut self, _msg: Envelope, ctx: &mut TurnContext) -> StepResult {
            ctx.send_text(self.target, "forwarded");
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    #[test]
    fn test_step_behavior_boxed() {
        let mut b: Box<dyn StepBehavior> = Box::new(EchoStep);
        let result = b.init(None);
        assert!(matches!(result, StepResult::Continue));
    }

    #[test]
    fn test_step_behavior_snapshot_default() {
        let b = EchoStep;
        assert!(b.snapshot().is_none());
    }

    #[test]
    fn test_llm_agent_suspend() {
        let mut agent = LlmAgent { awaiting_response: false };
        agent.init(None);
        let pid = Pid::from_raw(1);
        let mut ctx = TurnContext::new(pid);
        let msg = Envelope::text(pid, "call the LLM");
        let result = agent.handle(msg, &mut ctx);
        assert!(matches!(result, StepResult::Suspend(_)));
    }

    #[test]
    fn test_llm_agent_done_on_effect_result() {
        let mut agent = LlmAgent { awaiting_response: true };
        let pid = Pid::from_raw(1);
        let mut ctx = TurnContext::new(pid);
        use crate::core::effect::EffectId;
        let result = EffectResult::success(EffectId::new(), serde_json::json!("response"));
        let msg = Envelope::effect_result(pid, result);
        let step = agent.handle(msg, &mut ctx);
        assert!(matches!(step, StepResult::Done(Reason::Normal)));
    }

    #[test]
    fn test_forwarder_emits_intent() {
        let target = Pid::from_raw(99);
        let mut fwd = Forwarder { target };
        fwd.init(None);
        let pid = Pid::from_raw(1);
        let mut ctx = TurnContext::new(pid);
        let msg = Envelope::text(pid, "trigger");
        fwd.handle(msg, &mut ctx);
        assert_eq!(ctx.intent_count(), 1);
        let intents = ctx.take_intents();
        assert!(matches!(intents[0], crate::core::turn_context::TurnIntent::SendMessage(_)));
    }
}
```

**Step 2: Register in core/mod.rs**

Add:
```rust
pub mod behavior;
```

**Step 3: Run tests**

Run: `cd zeptovm && cargo test --lib core::behavior::tests`
Expected: 4 tests PASS

**Step 4: Commit**

```bash
git add zeptovm/src/core/behavior.rs zeptovm/src/core/mod.rs
git commit -m "feat(zeptovm): add StepBehavior trait — handle(Envelope) -> StepResult"
```

---

## Task 6: Create 5-Lane Mailbox

**Files:**
- Create: `zeptovm/src/kernel/mailbox.rs`

**Step 1: Write the mailbox**

```rust
use std::collections::VecDeque;

use crate::core::message::{Envelope, MessageClass};

/// Multi-lane mailbox with priority-ordered drain.
///
/// Lane order: control > supervisor > effect > user > background
/// Fairness window ensures low-priority lanes still progress.
pub struct MultiLaneMailbox {
    control: VecDeque<Envelope>,
    supervisor: VecDeque<Envelope>,
    effect: VecDeque<Envelope>,
    user: VecDeque<Envelope>,
    background: VecDeque<Envelope>,
    /// After this many high-priority drains, force one low-priority drain.
    fairness_window: u32,
    /// Counter for fairness tracking.
    high_priority_streak: u32,
}

impl MultiLaneMailbox {
    pub fn new() -> Self {
        Self {
            control: VecDeque::new(),
            supervisor: VecDeque::new(),
            effect: VecDeque::new(),
            user: VecDeque::new(),
            background: VecDeque::new(),
            fairness_window: 50,
            high_priority_streak: 0,
        }
    }

    /// Push an envelope into the correct lane based on its MessageClass.
    pub fn push(&mut self, env: Envelope) {
        match env.class {
            MessageClass::Control => self.control.push_back(env),
            MessageClass::Supervisor => self.supervisor.push_back(env),
            MessageClass::EffectResult => self.effect.push_back(env),
            MessageClass::User => self.user.push_back(env),
            MessageClass::Background => self.background.push_back(env),
        }
    }

    /// Pop the next envelope respecting lane priority with fairness.
    pub fn pop(&mut self) -> Option<Envelope> {
        // Control always first (kill signals, exit links)
        if let Some(env) = self.control.pop_front() {
            return Some(env);
        }

        // Fairness check: if high-priority lanes have been draining too long,
        // give low-priority a chance
        if self.high_priority_streak >= self.fairness_window {
            self.high_priority_streak = 0;
            if let Some(env) = self.background.pop_front() {
                return Some(env);
            }
        }

        // Supervisor
        if let Some(env) = self.supervisor.pop_front() {
            self.high_priority_streak += 1;
            return Some(env);
        }

        // Effect results (high priority — unblock waiting processes)
        if let Some(env) = self.effect.pop_front() {
            self.high_priority_streak += 1;
            return Some(env);
        }

        // User messages
        if let Some(env) = self.user.pop_front() {
            self.high_priority_streak = 0;
            return Some(env);
        }

        // Background
        if let Some(env) = self.background.pop_front() {
            self.high_priority_streak = 0;
            return Some(env);
        }

        None
    }

    /// Check if any lane has messages.
    pub fn has_messages(&self) -> bool {
        !self.control.is_empty()
            || !self.supervisor.is_empty()
            || !self.effect.is_empty()
            || !self.user.is_empty()
            || !self.background.is_empty()
    }

    /// Total messages across all lanes.
    pub fn total_len(&self) -> usize {
        self.control.len()
            + self.supervisor.len()
            + self.effect.len()
            + self.user.len()
            + self.background.len()
    }

    /// Messages in a specific lane.
    pub fn lane_len(&self, class: MessageClass) -> usize {
        match class {
            MessageClass::Control => self.control.len(),
            MessageClass::Supervisor => self.supervisor.len(),
            MessageClass::EffectResult => self.effect.len(),
            MessageClass::User => self.user.len(),
            MessageClass::Background => self.background.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectId, EffectResult};
    use crate::core::message::{Envelope, EnvelopePayload, MessageClass, Signal};
    use crate::pid::Pid;

    fn to() -> Pid {
        Pid::from_raw(1)
    }

    #[test]
    fn test_mailbox_empty() {
        let mut mb = MultiLaneMailbox::new();
        assert!(!mb.has_messages());
        assert!(mb.pop().is_none());
        assert_eq!(mb.total_len(), 0);
    }

    #[test]
    fn test_mailbox_push_pop_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "hello"));
        assert!(mb.has_messages());
        assert_eq!(mb.total_len(), 1);

        let env = mb.pop().unwrap();
        assert_eq!(env.class, MessageClass::User);
        assert!(!mb.has_messages());
    }

    #[test]
    fn test_mailbox_control_before_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "user msg"));
        mb.push(Envelope::signal(to(), Signal::Kill));

        let first = mb.pop().unwrap();
        assert_eq!(first.class, MessageClass::Control);

        let second = mb.pop().unwrap();
        assert_eq!(second.class, MessageClass::User);
    }

    #[test]
    fn test_mailbox_effect_before_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "user msg"));
        let result = EffectResult::success(
            EffectId::new(),
            serde_json::json!("ok"),
        );
        mb.push(Envelope::effect_result(to(), result));

        let first = mb.pop().unwrap();
        assert_eq!(first.class, MessageClass::EffectResult);

        let second = mb.pop().unwrap();
        assert_eq!(second.class, MessageClass::User);
    }

    #[test]
    fn test_mailbox_fifo_within_lane() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "first"));
        mb.push(Envelope::text(to(), "second"));
        mb.push(Envelope::text(to(), "third"));

        for expected in ["first", "second", "third"] {
            let env = mb.pop().unwrap();
            if let EnvelopePayload::User(crate::core::message::Payload::Text(s)) =
                &env.payload
            {
                assert_eq!(s, expected);
            } else {
                panic!("expected text payload");
            }
        }
    }

    #[test]
    fn test_mailbox_lane_len() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "a"));
        mb.push(Envelope::text(to(), "b"));
        mb.push(Envelope::signal(to(), Signal::Suspend));

        assert_eq!(mb.lane_len(MessageClass::User), 2);
        assert_eq!(mb.lane_len(MessageClass::Control), 1);
        assert_eq!(mb.lane_len(MessageClass::EffectResult), 0);
    }

    #[test]
    fn test_mailbox_fairness_window() {
        let mut mb = MultiLaneMailbox::new();
        mb.fairness_window = 3;

        // Add background message
        let mut bg = Envelope::text(to(), "background");
        bg.class = MessageClass::Background;
        mb.push(bg);

        // Add 4 effect results
        for _ in 0..4 {
            let result = EffectResult::success(
                EffectId::new(),
                serde_json::json!("ok"),
            );
            mb.push(Envelope::effect_result(to(), result));
        }

        // First 3 pops should be effect results (high-priority streak)
        for _ in 0..3 {
            let env = mb.pop().unwrap();
            assert_eq!(env.class, MessageClass::EffectResult);
        }

        // 4th pop: fairness kicks in, background gets a turn
        let env = mb.pop().unwrap();
        assert_eq!(env.class, MessageClass::Background);

        // 5th pop: back to effect
        let env = mb.pop().unwrap();
        assert_eq!(env.class, MessageClass::EffectResult);
    }
}
```

**Step 2: Run tests**

Run: `cd zeptovm && cargo test --lib kernel::mailbox::tests`
Expected: 7 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/src/kernel/mailbox.rs
git commit -m "feat(zeptovm): add 5-lane MultiLaneMailbox with priority drain and fairness"
```

---

## Task 7: Create Process Table (Process State Machine)

**Files:**
- Create: `zeptovm/src/kernel/process_table.rs`

Process entries with the full 9-state model, step logic using `StepBehavior + Envelope`, and `step_safe()` with `catch_unwind`.

**Step 1: Write the module with tests**

```rust
use std::panic::{self, AssertUnwindSafe};

use crate::{
    core::{
        effect::EffectId,
        message::{Envelope, EnvelopePayload, Signal},
        step_result::StepResult,
        turn_context::TurnContext,
    },
    error::Reason,
    kernel::mailbox::MultiLaneMailbox,
    pid::Pid,
};

use crate::core::behavior::StepBehavior;

/// Full runtime state of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRuntimeState {
    Ready,
    Running,
    WaitingMessage,
    WaitingEffect(u64), // EffectId raw value (Copy-friendly)
    WaitingTimer,
    Checkpointing,
    Suspended,
    Failed,
    Done,
}

/// A process entry managed by the scheduler.
pub struct ProcessEntry {
    pub pid: Pid,
    pub state: ProcessRuntimeState,
    pub mailbox: MultiLaneMailbox,
    behavior: Box<dyn StepBehavior>,
    reductions: u32,
    kill_requested: bool,
    exit_reason: Option<Reason>,
}

impl ProcessEntry {
    pub fn new(pid: Pid, behavior: Box<dyn StepBehavior>) -> Self {
        Self {
            pid,
            state: ProcessRuntimeState::Ready,
            mailbox: MultiLaneMailbox::new(),
            behavior,
            reductions: 0,
            kill_requested: false,
            exit_reason: None,
        }
    }

    /// Initialize the process.
    pub fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult {
        self.behavior.init(checkpoint)
    }

    /// Step the process once with panic isolation.
    /// Returns (StepResult, TurnContext with collected intents).
    pub fn step(&mut self) -> (StepResult, TurnContext) {
        let mut ctx = TurnContext::new(self.pid);

        // Kill check (highest priority, like BEAM)
        if self.kill_requested {
            self.state = ProcessRuntimeState::Done;
            return (StepResult::Done(Reason::Kill), ctx);
        }

        // Pop next message (mailbox handles lane priority)
        match self.mailbox.pop() {
            Some(env) => {
                // Check for control signals that the runtime handles
                if let EnvelopePayload::Signal(ref sig) = env.payload {
                    return (self.handle_signal(sig), ctx);
                }

                // User/effect messages go to behavior.handle() with catch_unwind
                self.reductions += 1;
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    self.behavior.handle(env, &mut ctx)
                }));

                match result {
                    Ok(step) => (step, ctx),
                    Err(panic_info) => {
                        let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                            format!("panic: {s}")
                        } else if let Some(s) = panic_info.downcast_ref::<String>() {
                            format!("panic: {s}")
                        } else {
                            "panic: unknown".to_string()
                        };
                        self.state = ProcessRuntimeState::Failed;
                        (StepResult::Fail(Reason::Custom(msg)), ctx)
                    }
                }
            }
            None => (StepResult::Wait, ctx),
        }
    }

    fn handle_signal(&mut self, signal: &Signal) -> StepResult {
        match signal {
            Signal::Kill => {
                self.state = ProcessRuntimeState::Done;
                StepResult::Done(Reason::Kill)
            }
            Signal::Shutdown => {
                self.state = ProcessRuntimeState::Done;
                StepResult::Done(Reason::Shutdown)
            }
            Signal::ExitLinked(_from, reason) => {
                if reason.is_abnormal() {
                    self.state = ProcessRuntimeState::Done;
                    StepResult::Done(reason.clone())
                } else {
                    // Normal exit from linked process — ignore
                    StepResult::Continue
                }
            }
            Signal::MonitorDown(_, _) => {
                // Could deliver to behavior, for now just continue
                StepResult::Continue
            }
            Signal::Suspend => {
                self.state = ProcessRuntimeState::Suspended;
                StepResult::Wait
            }
            Signal::Resume => StepResult::Continue,
        }
    }

    pub fn request_kill(&mut self) {
        self.kill_requested = true;
    }

    pub fn terminate(&mut self, reason: &Reason) {
        self.behavior.terminate(reason);
        self.exit_reason = Some(reason.clone());
        self.state = ProcessRuntimeState::Done;
    }

    pub fn reductions(&self) -> u32 {
        self.reductions
    }

    pub fn take_reductions(&mut self) -> u32 {
        let r = self.reductions;
        self.reductions = 0;
        r
    }

    pub fn exit_reason(&self) -> Option<&Reason> {
        self.exit_reason.as_ref()
    }

    pub fn is_killed(&self) -> bool {
        self.kill_requested
    }

    /// Get a snapshot of behavior state.
    pub fn snapshot(&self) -> Option<Vec<u8>> {
        self.behavior.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::behavior::StepBehavior;
    use crate::core::effect::{EffectKind, EffectRequest, EffectResult, EffectId};
    use crate::core::message::Envelope;
    use crate::error::Reason;

    struct Echo;
    impl StepBehavior for Echo {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
        fn handle(&mut self, _msg: Envelope, _ctx: &mut TurnContext) -> StepResult { StepResult::Continue }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    struct Counter { count: u32 }
    impl StepBehavior for Counter {
        fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
        fn handle(&mut self, _msg: Envelope, _ctx: &mut TurnContext) -> StepResult {
            self.count += 1;
            StepResult::Continue
        }
        fn terminate(&mut self, _reason: &Reason) {}
    }

    #[test]
    fn test_process_new() {
        let p = ProcessEntry::new(Pid::new(), Box::new(Echo));
        assert_eq!(p.state, ProcessRuntimeState::Ready);
        assert_eq!(p.reductions(), 0);
    }

    #[test]
    fn test_process_step_with_message() {
        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Echo));
        p.init(None);
        p.mailbox.push(Envelope::text(pid, "hello"));
        let (result, _ctx) = p.step();
        assert!(matches!(result, StepResult::Continue));
        assert_eq!(p.reductions(), 1);
    }

    #[test]
    fn test_process_step_empty_returns_wait() {
        let mut p = ProcessEntry::new(Pid::new(), Box::new(Echo));
        p.init(None);
        let (result, _) = p.step();
        assert!(matches!(result, StepResult::Wait));
    }

    #[test]
    fn test_process_kill() {
        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Echo));
        p.init(None);
        p.mailbox.push(Envelope::text(pid, "ignored"));
        p.request_kill();
        let (result, _) = p.step();
        assert!(matches!(result, StepResult::Done(Reason::Kill)));
    }

    #[test]
    fn test_process_signal_kill() {
        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Echo));
        p.init(None);
        p.mailbox.push(Envelope::signal(pid, Signal::Kill));
        let (result, _) = p.step();
        assert!(matches!(result, StepResult::Done(Reason::Kill)));
    }

    #[test]
    fn test_process_exit_linked_abnormal() {
        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Echo));
        p.init(None);
        let other = Pid::new();
        p.mailbox.push(Envelope::signal(
            pid,
            Signal::ExitLinked(other, Reason::Custom("crash".into())),
        ));
        let (result, _) = p.step();
        assert!(matches!(result, StepResult::Done(Reason::Custom(_))));
    }

    #[test]
    fn test_process_exit_linked_normal_ignored() {
        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Echo));
        p.init(None);
        let other = Pid::new();
        p.mailbox.push(Envelope::signal(
            pid,
            Signal::ExitLinked(other, Reason::Normal),
        ));
        p.mailbox.push(Envelope::text(pid, "still alive"));
        let (result, _) = p.step();
        assert!(matches!(result, StepResult::Continue));
    }

    #[test]
    fn test_process_panic_caught() {
        struct Panicker;
        impl StepBehavior for Panicker {
            fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
            fn handle(&mut self, _msg: Envelope, _ctx: &mut TurnContext) -> StepResult { panic!("boom") }
            fn terminate(&mut self, _reason: &Reason) {}
        }

        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Panicker));
        p.init(None);
        p.mailbox.push(Envelope::text(pid, "trigger"));
        let (result, _) = p.step();
        assert!(matches!(result, StepResult::Fail(Reason::Custom(_))));
        assert_eq!(p.state, ProcessRuntimeState::Failed);
    }

    #[test]
    fn test_process_suspend_effect() {
        struct Suspender;
        impl StepBehavior for Suspender {
            fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
            fn handle(&mut self, _msg: Envelope, _ctx: &mut TurnContext) -> StepResult {
                StepResult::Suspend(EffectRequest::new(
                    EffectKind::LlmCall,
                    serde_json::json!({"prompt": "hi"}),
                ))
            }
            fn terminate(&mut self, _reason: &Reason) {}
        }

        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Suspender));
        p.init(None);
        p.mailbox.push(Envelope::text(pid, "go"));
        let (result, _) = p.step();
        assert!(matches!(result, StepResult::Suspend(_)));
    }

    #[test]
    fn test_process_intents_collected() {
        struct Sender { target: Pid }
        impl StepBehavior for Sender {
            fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult { StepResult::Continue }
            fn handle(&mut self, _msg: Envelope, ctx: &mut TurnContext) -> StepResult {
                ctx.send_text(self.target, "forwarded");
                StepResult::Continue
            }
            fn terminate(&mut self, _reason: &Reason) {}
        }

        let pid = Pid::new();
        let target = Pid::from_raw(99);
        let mut p = ProcessEntry::new(pid, Box::new(Sender { target }));
        p.init(None);
        p.mailbox.push(Envelope::text(pid, "go"));
        let (result, ctx) = p.step();
        assert!(matches!(result, StepResult::Continue));
        assert_eq!(ctx.intent_count(), 1);
    }

    #[test]
    fn test_process_reductions_tracking() {
        let pid = Pid::new();
        let mut p = ProcessEntry::new(pid, Box::new(Counter { count: 0 }));
        p.init(None);
        for i in 0..5 {
            p.mailbox.push(Envelope::text(pid, format!("m-{i}")));
        }
        for _ in 0..5 {
            p.step();
        }
        assert_eq!(p.reductions(), 5);
        assert_eq!(p.take_reductions(), 5);
        assert_eq!(p.reductions(), 0);
    }
}
```

**Step 2: Run tests**

Run: `cd zeptovm && cargo test --lib kernel::process_table::tests`
Expected: 10 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/src/kernel/process_table.rs
git commit -m "feat(zeptovm): add ProcessEntry with 9-state model, catch_unwind, signal handling"
```

---

## Task 8: Create Run Queue

Same as previous plan Task 6 — crossbeam-deque based RunQueue + GlobalQueue. See previous plan for full code.

**Commit message:** `feat(zeptovm): add RunQueue and GlobalQueue with work-stealing deque`

---

## Task 9: Create Scheduler Engine (Single-Thread)

**Files:**
- Create: `zeptovm/src/kernel/scheduler.rs`

The scheduler steps processes, counts reductions, and handles state transitions. Key difference from v1 plan: when a process returns `Suspend(EffectRequest)`, the scheduler records the effect and routes it to the effect dispatcher.

The core loop:

```
pop ready pid from run queue
→ step process up to max_reductions times
→ on Continue: re-enqueue if more messages
→ on Wait: mark WaitingMessage
→ on Suspend(EffectRequest): mark WaitingEffect, emit request
→ on Done/Fail: terminate, remove, notify supervisor
```

Tests should cover: spawn, send, tick, idle, preemption, fairness, kill, suspend.

**Commit message:** `feat(zeptovm): add SchedulerEngine — turn-based stepping with effect dispatch`

---

## Task 10: Create Effect Dispatcher + Reactor Bridge

**Files:**
- Create: `zeptovm/src/kernel/reactor.rs`

The reactor receives `EffectRequest`s via crossbeam channel, converts them to async futures based on `EffectKind`, polls them on a shared Tokio runtime, and returns `EffectResult` via completion channel.

For v1, the dispatcher handles:
- `EffectKind::SleepUntil` → `tokio::time::sleep`
- `EffectKind::Http` → placeholder that returns success
- `EffectKind::LlmCall` → placeholder that returns success
- All others → placeholder

The scheduler drains the completion channel each tick and re-enqueues processes with the `EffectResult` delivered as an `Envelope`.

**Commit message:** `feat(zeptovm): add Reactor — EffectRequest dispatch via shared Tokio runtime`

---

## Task 11: Create Journal (SQLite-backed)

**Files:**
- Create: `zeptovm/src/durability/journal.rs`

Append-only log of turn events backed by SQLite. Schema:

```sql
CREATE TABLE journal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pid INTEGER NOT NULL,
    entry_type TEXT NOT NULL,
    payload BLOB,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_journal_pid ON journal(pid);
```

API:
- `append(pid, JournalEntry)` — write one entry
- `append_batch(entries)` — atomic batch write (for turn commit)
- `replay(pid, since_offset)` → `Vec<JournalEntry>`
- `truncate(pid, before_offset)` — compact after snapshot

Journal entries are serialized with serde + bincode or JSON.

**Commit message:** `feat(zeptovm): add SQLite-backed journal for durable turn events`

---

## Task 12: Create Snapshot Store

**Files:**
- Create: `zeptovm/src/durability/snapshot.rs`

SQLite-backed snapshot storage. Schema:

```sql
CREATE TABLE snapshots (
    pid INTEGER NOT NULL,
    version INTEGER NOT NULL,
    state_blob BLOB NOT NULL,
    mailbox_cursor INTEGER NOT NULL,
    pending_effects TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (pid, version)
);
```

API:
- `save(pid, version, state_blob, mailbox_cursor, pending_effects)`
- `load_latest(pid)` → `Option<Snapshot>`
- `delete_before(pid, version)` — compact old snapshots

**Commit message:** `feat(zeptovm): add SQLite-backed snapshot store for process recovery`

---

## Task 13: Create Turn Executor (Atomic Commit)

**Files:**
- Create: `zeptovm/src/kernel/turn_executor.rs`

The turn executor wraps the scheduler's step loop with atomic commit semantics. After a process completes a turn:

1. Collect: state delta, journal entries, outbound messages, effect requests
2. Write all to journal + snapshot store in one SQLite transaction
3. Only then: dispatch effects, deliver outbound messages, ack consumed message

If the commit fails, the turn does not count as progressed.

For v1, this can be a simple wrapper:
```rust
pub struct TurnCommit {
    pub pid: Pid,
    pub journal_entries: Vec<JournalEntry>,
    pub outbound_messages: Vec<Envelope>,
    pub effect_requests: Vec<EffectRequest>,
    pub state_snapshot: Option<Vec<u8>>,
}
```

The turn executor collects intents from the scheduler step, commits them atomically, then publishes.

**Commit message:** `feat(zeptovm): add TurnExecutor — atomic commit of state + journal + effects`

---

## Task 14: Multi-Threaded Runtime with Budget Gate

**Files:**
- Create: `zeptovm/src/kernel/runtime.rs`

Wraps everything together: N scheduler threads, shared process table (DashMap), global queue, reactor, journal, snapshot store. Adds budget checking before effect dispatch.

Budget gate (v1 — simple):
```rust
pub struct BudgetState {
    pub usd_remaining: f64,
    pub token_remaining: u64,
}
```

Before dispatching an `EffectRequest::LlmCall`, check budget. If exceeded, deliver `EffectResult::failure` with "budget exceeded" and notify supervisor.

**Commit message:** `feat(zeptovm): add SchedulerRuntime — multi-thread with budget gate`

---

## Task 15: Gate Tests — v0 through v3

**Files:**
- Create: `zeptovm/tests/phase1_gate.rs`

Four gate tests validating the Phase 1 invariants:

**v0 gate:** 100 processes, 10k messages, fair scheduling (single-thread engine)

**v1 gate:** Process emits `EffectRequest::LlmCall`, runtime journals it, dispatches to reactor, delivers `EffectResult` as `Envelope`, process handles it and exits normally.

**v2 gate:** Kill process, recover from snapshot + journal replay, process resumes correctly and processes next message.

**v3 gate:** Budget exceeded blocks LLM effect, supervisor receives notification.

**Commit message:** `test(zeptovm): add Phase 1 gate tests — scheduling, effects, recovery, budget`

---

## Summary of New Module Layout After Phase 1

```
zeptovm/src/
├── lib.rs                      # + pub mod core, kernel, durability
├── core/
│   ├── mod.rs
│   ├── effect.rs               # EffectRequest, EffectResult, EffectKind, EffectId
│   ├── message.rs              # Envelope, MessageClass, Signal, Payload
│   ├── step_result.rs          # StepResult (Suspend(EffectRequest))
│   └── behavior.rs             # StepBehavior trait
├── kernel/
│   ├── mod.rs
│   ├── mailbox.rs              # 5-lane MultiLaneMailbox
│   ├── process_table.rs        # ProcessEntry, ProcessRuntimeState (9 states)
│   ├── run_queue.rs            # RunQueue + GlobalQueue (crossbeam-deque)
│   ├── scheduler.rs            # SchedulerEngine (single-thread)
│   ├── reactor.rs              # Effect dispatcher + Tokio bridge
│   ├── turn_executor.rs        # Atomic turn commit
│   └── runtime.rs              # Multi-thread SchedulerRuntime + budget gate
├── durability/
│   ├── mod.rs
│   ├── journal.rs              # SQLite-backed append-only journal
│   └── snapshot.rs             # SQLite-backed snapshot store
│
│   # --- Legacy modules (kept for migration) ---
├── behavior.rs                 # Old async Behavior trait
├── process.rs                  # Old tokio::spawn process
├── mailbox.rs                  # Old 2-channel mailbox
├── error.rs                    # Reason, Action (old types still used)
├── pid.rs                      # Pid (shared)
├── link.rs                     # LinkTable (shared)
├── registry.rs                 # Old ProcessRegistry
└── supervisor.rs               # Old tokio supervisor

zeptovm/tests/
├── v0_gate.rs                  # Old gate test (tokio-based)
├── v1_gate.rs                  # Old supervision test
├── v1_exit_signals.rs          # Old exit signal test
└── phase1_gate.rs              # NEW: Phase 1 gate tests
```

## The 7 Invariants Validated by Gate Tests

| # | Invariant | Gate Test |
|---|-----------|-----------|
| 1 | Turn either fully commits or does not happen | v2: recovery replays correctly |
| 2 | No effect durable unless journaled | v1: effect request appears in journal |
| 3 | Process sees nondeterminism only through EffectResult | v1: LlmCall result arrives as Envelope |
| 4 | Every wait recoverable from snapshot + journal | v2: kill → recover → resume |
| 5 | Every process has a fault owner | v3: budget exceeded → supervisor notified |
| 6 | Large payloads via ObjectRef | Deferred to Phase 2 |
| 7 | Budget/policy before expensive effects | v3: budget gate blocks LLM call |
| 8 | Only shard owner schedules | Deferred to Phase 3 |

## Deferred Work

| Phase | What | When |
|-------|------|------|
| 2 | LLM-native effects, HTTP/browser workers, idempotency, retries, compensation, observability | After Phase 1 stabilizes |
| 3 | Cluster membership, shard leases, remote routing, failover | After Phase 2 |
| 4 | Object plane, restartable migration, locality-aware scheduling | After Phase 3 |
| 5 | Advanced policy, human approvals, behavior versioning, cost optimizer, replay UI | After Phase 4 |
| Migration | Rewrite supervisor/registry to use new kernel | After Phase 1 gate tests pass |
| Cleanup | Remove old tokio-based modules | After migration |
