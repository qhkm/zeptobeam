# ZeptoVM Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the `zeptovm` crate — an async-native process runtime with Erlang/OTP-style supervision, two-channel mailbox, out-of-band kill, and durable execution — then wire it into `zeptobeam` as the ZeptoClaw agent execution engine.

**Architecture:** Five staged deliveries following the strangler pattern. Each stage has a parity gate that must pass before the next stage begins. v0 = foundation (Pid, Process, Mailbox, Registry), v1 = supervision (links, monitors, restart strategies), v2 = durability (checkpoint, WAL, recovery), v3 = control plane (budget, provider gate, admission), v4 = ZeptoBeam integration (agent_adapter, config_loader, swarm).

**Tech Stack:** Rust nightly (2026-03-04), tokio 1.x, tokio-util (CancellationToken), async-trait, dashmap, rusqlite, serde, tracing

**Design Doc:** `docs/plans/2026-03-06-zeptovm-design.md`

**Existing Codebase Context:**
- Workspace root: `Cargo.toml` with members `[lib-erlangrt, erlexec, ct_run, zeptobeam]`
- Existing agent runtime: `lib-erlangrt/src/agent_rt/` (41 modules, sync scheduler, VecDeque mailbox)
- Existing zeptobeam binary: `zeptobeam/src/main.rs` (imports from agent_rt)
- Rust toolchain: `rust-toolchain.toml` pins `nightly-2026-03-04`
- All `cargo` commands use `+nightly` via Makefile, but `rust-toolchain.toml` handles it automatically in the workspace

**Conventions:**
- Run all tests with: `cargo test -p zeptovm`
- Run specific test: `cargo test -p zeptovm -- test_name`
- Build check: `cargo check -p zeptovm`
- Format: `cargo fmt -p zeptovm`
- Clippy: `cargo clippy -p zeptovm`

---

## Stage v0: Foundation

**Gate:** Can spawn 100 processes, deliver 10k messages, clean shutdown. All tests pass.

---

### Task 1: Create zeptovm crate scaffold

**Files:**
- Create: `zeptovm/Cargo.toml`
- Create: `zeptovm/src/lib.rs`
- Modify: `Cargo.toml` (workspace root, add `zeptovm` to members)

**Step 1: Create Cargo.toml for zeptovm**

```toml
[package]
name = "zeptovm"
version = "0.1.0"
edition = "2021"
description = "Async-native process runtime with OTP-style supervision"

[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "time", "sync", "macros", "signal"] }
tokio-util = { version = "0.7", features = ["rt"] }
async-trait = "0.1"
dashmap = "6"
tracing = "0.1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "time", "sync", "macros", "test-util"] }
```

**Step 2: Create lib.rs with module stubs**

```rust
pub mod pid;
pub mod error;
pub mod mailbox;
pub mod process;
pub mod registry;

pub use pid::Pid;
pub use error::{Reason, Action};
```

**Step 3: Add zeptovm to workspace members**

In root `Cargo.toml`, change:
```toml
members = ["lib-erlangrt", "erlexec", "ct_run", "zeptobeam", "zeptovm"]
```

**Step 4: Verify it compiles**

Run: `cargo check -p zeptovm`
Expected: Fails (module files don't exist yet). That's fine — we'll create them in subsequent tasks.

**Step 5: Create empty module files so it compiles**

Create these empty files:
- `zeptovm/src/pid.rs`
- `zeptovm/src/error.rs`
- `zeptovm/src/mailbox.rs`
- `zeptovm/src/process.rs`
- `zeptovm/src/registry.rs`

**Step 6: Verify it compiles**

Run: `cargo check -p zeptovm`
Expected: PASS (with warnings about unused imports in lib.rs)

**Step 7: Commit**

```bash
git add zeptovm/ Cargo.toml
git commit -m "feat(zeptovm): scaffold crate with module stubs"
```

---

### Task 2: Pid type

**Files:**
- Modify: `zeptovm/src/pid.rs`
- Test: inline `#[cfg(test)]` in `pid.rs`

**Step 1: Write the failing tests**

In `zeptovm/src/pid.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_unique() {
        let a = Pid::new();
        let b = Pid::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_pid_raw_roundtrip() {
        let pid = Pid::new();
        let raw = pid.raw();
        let restored = Pid::from_raw(raw);
        assert_eq!(pid, restored);
    }

    #[test]
    fn test_pid_display() {
        let pid = Pid::from_raw(42);
        assert_eq!(format!("{pid}"), "<0.42>");
    }

    #[test]
    fn test_pid_is_copy() {
        let a = Pid::new();
        let b = a; // copy
        assert_eq!(a, b);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_pid`
Expected: FAIL — `Pid` type doesn't exist

**Step 3: Implement Pid**

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::fmt;

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

/// Unique process identifier. Lightweight, Copy, monotonically increasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(u64);

impl Pid {
    /// Allocate a new globally unique Pid.
    pub fn new() -> Self {
        Self(NEXT_PID.fetch_add(1, Ordering::Relaxed))
    }

    /// Construct from a raw u64 (for deserialization/testing).
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Get the raw u64 value.
    pub fn raw(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<0.{}>", self.0)
    }
}

#[cfg(test)]
mod tests {
    // ... tests from step 1
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_pid`
Expected: 4 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/pid.rs
git commit -m "feat(zeptovm): Pid type with atomic allocation"
```

---

### Task 3: Error types (Reason, Action, Message)

**Files:**
- Modify: `zeptovm/src/error.rs`
- Test: inline `#[cfg(test)]` in `error.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reason_normal_is_not_abnormal() {
        assert!(!Reason::Normal.is_abnormal());
    }

    #[test]
    fn test_reason_shutdown_is_not_abnormal() {
        assert!(!Reason::Shutdown.is_abnormal());
    }

    #[test]
    fn test_reason_custom_is_abnormal() {
        assert!(Reason::Custom("oops".into()).is_abnormal());
    }

    #[test]
    fn test_reason_kill_is_abnormal() {
        assert!(Reason::Kill.is_abnormal());
    }

    #[test]
    fn test_action_variants() {
        let a = Action::Continue;
        assert!(matches!(a, Action::Continue));
        let b = Action::Stop(Reason::Normal);
        assert!(matches!(b, Action::Stop(Reason::Normal)));
        let c = Action::Checkpoint;
        assert!(matches!(c, Action::Checkpoint));
    }

    #[test]
    fn test_message_user_text() {
        let msg = Message::text("hello");
        assert!(matches!(msg, Message::User(UserPayload::Text(_))));
    }

    #[test]
    fn test_message_user_bytes() {
        let msg = Message::bytes(vec![1, 2, 3]);
        assert!(matches!(msg, Message::User(UserPayload::Bytes(_))));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_reason`
Expected: FAIL

**Step 3: Implement error types**

```rust
use crate::pid::Pid;

/// Reason for process termination.
#[derive(Debug, Clone)]
pub enum Reason {
    /// Normal exit — not propagated to linked processes (unless trap_exit).
    Normal,
    /// Controlled shutdown.
    Shutdown,
    /// Unrecoverable kill (via CancellationToken). Cannot be trapped.
    Kill,
    /// Application-specific abnormal exit.
    Custom(String),
}

impl Reason {
    /// Returns true if this reason should kill linked processes.
    pub fn is_abnormal(&self) -> bool {
        matches!(self, Reason::Kill | Reason::Custom(_))
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::Normal => write!(f, "normal"),
            Reason::Shutdown => write!(f, "shutdown"),
            Reason::Kill => write!(f, "kill"),
            Reason::Custom(s) => write!(f, "{s}"),
        }
    }
}

/// Action returned by Behavior::handle() to direct the process run loop.
pub enum Action {
    /// Continue processing messages.
    Continue,
    /// Stop the process with the given reason.
    Stop(Reason),
    /// Save a checkpoint, then continue.
    Checkpoint,
}

/// Payload carried by user-level messages.
#[derive(Debug, Clone)]
pub enum UserPayload {
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
}

/// Messages delivered to a process.
#[derive(Debug, Clone)]
pub enum Message {
    /// User-level message (from another process or external).
    User(UserPayload),
}

impl Message {
    pub fn text(s: impl Into<String>) -> Self {
        Message::User(UserPayload::Text(s.into()))
    }

    pub fn bytes(b: Vec<u8>) -> Self {
        Message::User(UserPayload::Bytes(b))
    }

    pub fn json(v: serde_json::Value) -> Self {
        Message::User(UserPayload::Json(v))
    }
}

/// System-level messages delivered on the control channel.
#[derive(Debug)]
pub enum SystemMsg {
    /// A linked process died.
    ExitLinked(Pid, Reason),
    /// A monitored process died.
    MonitorDown(Pid, Reason),
    /// Pause user message processing.
    Suspend,
    /// Resume user message processing.
    Resume,
    /// Query process info (response sent via oneshot).
    GetState(tokio::sync::oneshot::Sender<ProcessInfo>),
}

/// Snapshot of process info returned by GetState.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: Pid,
    pub status: ProcessStatus,
    pub mailbox_depth: usize,
}

/// Process lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Suspended,
    Exiting,
}

#[cfg(test)]
mod tests {
    // ... tests from step 1
}
```

**Step 4: Update lib.rs to add serde_json**

Add `serde_json = "1"` to `zeptovm/Cargo.toml` under `[dependencies]`.

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_reason && cargo test -p zeptovm -- test_action && cargo test -p zeptovm -- test_message`
Expected: 7 tests PASS

**Step 6: Commit**

```bash
git add zeptovm/src/error.rs zeptovm/Cargo.toml
git commit -m "feat(zeptovm): Reason, Action, Message, SystemMsg types"
```

---

### Task 4: Behavior trait

**Files:**
- Create: `zeptovm/src/behavior.rs`
- Modify: `zeptovm/src/lib.rs` (add module)
- Test: inline in `behavior.rs`

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Action, Message, Reason};

    struct EchoBehavior;

    #[async_trait]
    impl Behavior for EchoBehavior {
        async fn init(&mut self, _checkpoint: Option<Vec<u8>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        async fn handle(&mut self, msg: Message) -> Action {
            let _ = msg;
            Action::Continue
        }

        async fn terminate(&mut self, _reason: &Reason) {}
    }

    #[tokio::test]
    async fn test_behavior_can_be_boxed() {
        let mut b: Box<dyn Behavior> = Box::new(EchoBehavior);
        b.init(None).await.unwrap();
        let action = b.handle(Message::text("hi")).await;
        assert!(matches!(action, Action::Continue));
    }

    #[tokio::test]
    async fn test_behavior_checkpoint_defaults_to_none() {
        let b = EchoBehavior;
        assert!(!b.should_checkpoint());
        assert!(b.checkpoint().is_none());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_behavior`
Expected: FAIL

**Step 3: Implement Behavior trait**

```rust
use async_trait::async_trait;
use crate::error::{Action, Message, Reason};

/// The core process behavior trait. Each process owns one Behavior instance.
/// Implementations define how a process handles messages, initializes, and terminates.
///
/// Uses `async_trait` for object safety (`Box<dyn Behavior>`).
#[async_trait]
pub trait Behavior: Send + 'static {
    /// Called once at spawn. `checkpoint` is Some if recovering from a prior run.
    async fn init(
        &mut self,
        checkpoint: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Called for each user message. Returns an Action directing the run loop.
    async fn handle(&mut self, msg: Message) -> Action;

    /// Called when the process is terminating. Cleanup hook.
    async fn terminate(&mut self, reason: &Reason);

    /// Opt-in: return true if the runtime should call checkpoint() after handle().
    fn should_checkpoint(&self) -> bool {
        false
    }

    /// Opt-in: serialize process state for durable recovery.
    fn checkpoint(&self) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    // ... tests from step 1
}
```

**Step 4: Add module to lib.rs**

Add `pub mod behavior;` and `pub use behavior::Behavior;` to `lib.rs`.

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_behavior`
Expected: 2 tests PASS

**Step 6: Commit**

```bash
git add zeptovm/src/behavior.rs zeptovm/src/lib.rs
git commit -m "feat(zeptovm): async Behavior trait with checkpoint hooks"
```

---

### Task 5: Mailbox (two-channel + CancellationToken)

**Files:**
- Modify: `zeptovm/src/mailbox.rs`
- Test: inline `#[cfg(test)]`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Message;

    #[tokio::test]
    async fn test_mailbox_create_and_send_user_message() {
        let (handle, mut mailbox) = create_mailbox(16, 32);
        handle.send_user(Message::text("hello")).await.unwrap();
        let msg = mailbox.user_rx.recv().await.unwrap();
        assert!(matches!(msg, Message::User(_)));
    }

    #[tokio::test]
    async fn test_mailbox_kill_token() {
        let (handle, _mailbox) = create_mailbox(16, 32);
        assert!(!handle.kill_token.is_cancelled());
        handle.kill();
        assert!(handle.kill_token.is_cancelled());
    }

    #[tokio::test]
    async fn test_mailbox_user_channel_bounded() {
        let (handle, _mailbox) = create_mailbox(2, 32);
        handle.send_user(Message::text("1")).await.unwrap();
        handle.send_user(Message::text("2")).await.unwrap();
        // Third send should not complete immediately (channel full)
        let result = handle.user_tx.try_send(Message::text("3"));
        assert!(result.is_err());
    }

    #[test]
    fn test_process_handle_is_clone() {
        let (handle, _mailbox) = create_mailbox(16, 32);
        let _clone = handle.clone();
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_mailbox`
Expected: FAIL

**Step 3: Implement mailbox**

```rust
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{Message, SystemMsg};

/// The process-side of the mailbox (owned by the process task).
pub struct ProcessMailbox {
    pub user_rx: mpsc::Receiver<Message>,
    pub control_rx: mpsc::Receiver<SystemMsg>,
}

/// The external handle for sending messages to a process.
/// Clone-friendly — held by registry and other processes.
#[derive(Clone)]
pub struct ProcessHandle {
    pub kill_token: CancellationToken,
    pub user_tx: mpsc::Sender<Message>,
    pub control_tx: mpsc::Sender<SystemMsg>,
}

impl ProcessHandle {
    /// Send a user message. Returns Err if the channel is full or closed.
    pub async fn send_user(&self, msg: Message) -> Result<(), mpsc::error::SendError<Message>> {
        self.user_tx.send(msg).await
    }

    /// Try to send a user message without waiting.
    pub fn try_send_user(&self, msg: Message) -> Result<(), mpsc::error::TrySendError<Message>> {
        self.user_tx.try_send(msg)
    }

    /// Send a system/control message.
    pub fn try_send_control(&self, msg: SystemMsg) -> Result<(), mpsc::error::TrySendError<SystemMsg>> {
        self.control_tx.try_send(msg)
    }

    /// Kill the process via out-of-band CancellationToken.
    /// This is guaranteed to be delivered regardless of queue state.
    pub fn kill(&self) {
        self.kill_token.cancel();
    }
}

/// Create a matched pair of (ProcessHandle, ProcessMailbox).
pub fn create_mailbox(
    user_capacity: usize,
    control_capacity: usize,
) -> (ProcessHandle, ProcessMailbox) {
    let (user_tx, user_rx) = mpsc::channel(user_capacity);
    let (control_tx, control_rx) = mpsc::channel(control_capacity);
    let kill_token = CancellationToken::new();

    let handle = ProcessHandle {
        kill_token: kill_token.clone(),
        user_tx,
        control_tx,
    };

    let mailbox = ProcessMailbox { user_rx, control_rx };

    (handle, mailbox)
}

#[cfg(test)]
mod tests {
    // ... tests from step 1
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_mailbox && cargo test -p zeptovm -- test_process_handle`
Expected: 4 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/mailbox.rs
git commit -m "feat(zeptovm): two-channel mailbox with CancellationToken kill"
```

---

### Task 6: Process run loop

**Files:**
- Modify: `zeptovm/src/process.rs`
- Test: inline `#[cfg(test)]`

This is the core of ZeptoVM — the biased `select!` loop from the design doc.

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior::Behavior;
    use crate::error::{Action, Message, Reason};
    use async_trait::async_trait;
    use std::sync::{Arc, atomic::{AtomicU32, Ordering}};

    struct CountBehavior {
        count: Arc<AtomicU32>,
    }

    #[async_trait]
    impl Behavior for CountBehavior {
        async fn init(&mut self, _cp: Option<Vec<u8>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        async fn handle(&mut self, _msg: Message) -> Action {
            self.count.fetch_add(1, Ordering::Relaxed);
            Action::Continue
        }
        async fn terminate(&mut self, _reason: &Reason) {}
    }

    #[tokio::test]
    async fn test_process_handles_messages() {
        let count = Arc::new(AtomicU32::new(0));
        let behavior = CountBehavior { count: count.clone() };
        let (handle, join) = spawn_process(behavior, 16, None);

        handle.send_user(Message::text("a")).await.unwrap();
        handle.send_user(Message::text("b")).await.unwrap();
        handle.send_user(Message::text("c")).await.unwrap();

        // Give time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        handle.kill();
        let exit = join.await.unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 3);
        assert!(matches!(exit.reason, Reason::Kill));
    }

    #[tokio::test]
    async fn test_process_kill_is_immediate() {
        let count = Arc::new(AtomicU32::new(0));
        let behavior = CountBehavior { count: count.clone() };
        let (handle, join) = spawn_process(behavior, 16, None);

        handle.kill();
        let exit = join.await.unwrap();
        assert!(matches!(exit.reason, Reason::Kill));
    }

    struct StopAfterOneBehavior;

    #[async_trait]
    impl Behavior for StopAfterOneBehavior {
        async fn init(&mut self, _cp: Option<Vec<u8>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        async fn handle(&mut self, _msg: Message) -> Action {
            Action::Stop(Reason::Normal)
        }
        async fn terminate(&mut self, _reason: &Reason) {}
    }

    #[tokio::test]
    async fn test_process_stops_on_action_stop() {
        let behavior = StopAfterOneBehavior;
        let (handle, join) = spawn_process(behavior, 16, None);

        handle.send_user(Message::text("bye")).await.unwrap();
        let exit = join.await.unwrap();
        assert!(matches!(exit.reason, Reason::Normal));
    }

    #[tokio::test]
    async fn test_process_exits_when_senders_dropped() {
        let count = Arc::new(AtomicU32::new(0));
        let behavior = CountBehavior { count: count.clone() };
        let (handle, join) = spawn_process(behavior, 16, None);

        drop(handle);
        // Process should exit because all senders are dropped
        let _exit = join.await.unwrap();
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_process`
Expected: FAIL

**Step 3: Implement the process run loop**

```rust
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::behavior::Behavior;
use crate::error::{Action, Message, ProcessStatus, Reason, SystemMsg};
use crate::mailbox::{create_mailbox, ProcessHandle, ProcessMailbox};
use crate::pid::Pid;

/// Result returned when a process exits.
#[derive(Debug)]
pub struct ProcessExit {
    pub pid: Pid,
    pub reason: Reason,
}

/// Spawn a new process as a tokio task.
/// Returns the ProcessHandle (for sending messages) and a JoinHandle for the exit result.
pub fn spawn_process(
    mut behavior: impl Behavior,
    user_mailbox_capacity: usize,
    checkpoint: Option<Vec<u8>>,
) -> (ProcessHandle, JoinHandle<ProcessExit>) {
    let pid = Pid::new();
    let (handle, mailbox) = create_mailbox(user_mailbox_capacity, 32);

    let kill_token = handle.kill_token.clone();
    let join = tokio::spawn(async move {
        run_process(pid, &mut behavior, mailbox, kill_token, checkpoint).await
    });

    (handle, join)
}

async fn run_process(
    pid: Pid,
    behavior: &mut dyn Behavior,
    mut mailbox: ProcessMailbox,
    kill_token: tokio_util::sync::CancellationToken,
    checkpoint: Option<Vec<u8>>,
) -> ProcessExit {
    // Init phase
    if let Err(e) = behavior.init(checkpoint).await {
        warn!(pid = %pid, error = %e, "process init failed");
        return ProcessExit {
            pid,
            reason: Reason::Custom(format!("init failed: {e}")),
        };
    }

    debug!(pid = %pid, "process started");

    let mut suspended = false;
    let mut exit_reason = Reason::Normal;

    loop {
        tokio::select! {
            biased;

            // Out-of-band kill: always wins, never blocked
            _ = kill_token.cancelled() => {
                exit_reason = Reason::Kill;
                break;
            }

            // Control messages: exit signals, suspend, monitor-down
            Some(sys) = mailbox.control_rx.recv() => {
                match sys {
                    SystemMsg::ExitLinked(_from, reason) => {
                        // For now, non-trapping processes die on abnormal exits
                        if reason.is_abnormal() {
                            exit_reason = reason;
                            break;
                        }
                    }
                    SystemMsg::MonitorDown(_from, _reason) => {
                        // Will be delivered as user message when monitors are implemented
                    }
                    SystemMsg::Suspend => {
                        suspended = true;
                    }
                    SystemMsg::Resume => {
                        suspended = false;
                    }
                    SystemMsg::GetState(tx) => {
                        let info = crate::error::ProcessInfo {
                            pid,
                            status: if suspended {
                                ProcessStatus::Suspended
                            } else {
                                ProcessStatus::Running
                            },
                            mailbox_depth: 0, // TODO: expose from receiver
                        };
                        let _ = tx.send(info);
                    }
                }
            }

            // User messages: normal processing
            Some(msg) = mailbox.user_rx.recv(), if !suspended => {
                match behavior.handle(msg).await {
                    Action::Continue => {}
                    Action::Stop(reason) => {
                        exit_reason = reason;
                        break;
                    }
                    Action::Checkpoint => {
                        // Checkpoint support added in v2 (durability stage)
                    }
                }
            }

            // All senders dropped — no one can reach this process
            else => {
                break;
            }
        }
    }

    behavior.terminate(&exit_reason).await;
    debug!(pid = %pid, reason = %exit_reason, "process exited");

    ProcessExit {
        pid,
        reason: exit_reason,
    }
}

#[cfg(test)]
mod tests {
    // ... tests from step 1
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_process`
Expected: 4 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/process.rs
git commit -m "feat(zeptovm): process run loop with biased select, kill token"
```

---

### Task 7: ProcessRegistry

**Files:**
- Modify: `zeptovm/src/registry.rs`
- Test: inline `#[cfg(test)]`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Message;

    #[tokio::test]
    async fn test_registry_spawn_and_lookup() {
        let registry = ProcessRegistry::new();
        let (pid, _join) = registry.spawn(make_echo(), 16, None);
        assert!(registry.lookup(&pid).is_some());
    }

    #[tokio::test]
    async fn test_registry_send_message() {
        let registry = ProcessRegistry::new();
        let (pid, _join) = registry.spawn(make_echo(), 16, None);
        let result = registry.send(&pid, Message::text("hi")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_registry_kill_and_remove() {
        let registry = ProcessRegistry::new();
        let (pid, join) = registry.spawn(make_echo(), 16, None);
        registry.kill(&pid);
        let _ = join.await;
        registry.remove(&pid);
        assert!(registry.lookup(&pid).is_none());
    }

    #[tokio::test]
    async fn test_registry_count() {
        let registry = ProcessRegistry::new();
        assert_eq!(registry.count(), 0);
        let (pid1, _j1) = registry.spawn(make_echo(), 16, None);
        let (pid2, _j2) = registry.spawn(make_echo(), 16, None);
        assert_eq!(registry.count(), 2);
        registry.kill(&pid1);
        registry.kill(&pid2);
        // Note: count stays 2 until remove() is called
    }

    fn make_echo() -> impl crate::behavior::Behavior {
        struct Echo;
        #[async_trait::async_trait]
        impl crate::behavior::Behavior for Echo {
            async fn init(&mut self, _cp: Option<Vec<u8>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> { Ok(()) }
            async fn handle(&mut self, _msg: crate::error::Message) -> crate::error::Action { crate::error::Action::Continue }
            async fn terminate(&mut self, _reason: &crate::error::Reason) {}
        }
        Echo
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_registry`
Expected: FAIL

**Step 3: Implement ProcessRegistry**

```rust
use dashmap::DashMap;
use tokio::task::JoinHandle;

use crate::behavior::Behavior;
use crate::error::Message;
use crate::mailbox::ProcessHandle;
use crate::pid::Pid;
use crate::process::{ProcessExit, spawn_process};

/// Concurrent registry mapping Pid -> ProcessHandle.
/// Thread-safe via DashMap. Handles are Clone so multiple callers can send messages.
pub struct ProcessRegistry {
    handles: DashMap<Pid, ProcessHandle>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self {
            handles: DashMap::new(),
        }
    }

    /// Spawn a new process, register it, and return its Pid + JoinHandle.
    pub fn spawn(
        &self,
        behavior: impl Behavior,
        user_mailbox_capacity: usize,
        checkpoint: Option<Vec<u8>>,
    ) -> (Pid, JoinHandle<ProcessExit>) {
        let (handle, join) = spawn_process(behavior, user_mailbox_capacity, checkpoint);
        // We need to know the Pid before spawn returns — extract from the join handle
        // Actually, spawn_process doesn't return the Pid directly. Let's refactor.
        // For now, we need to change spawn_process to return the Pid too.
        // We'll use a different approach: allocate Pid here, pass it in.
        todo!("need to refactor spawn_process to accept Pid or return it")
    }

    /// Look up a process handle by Pid. Returns None if not registered.
    pub fn lookup(&self, pid: &Pid) -> Option<ProcessHandle> {
        self.handles.get(pid).map(|r| r.value().clone())
    }

    /// Send a user message to a process.
    pub async fn send(&self, pid: &Pid, msg: Message) -> Result<(), String> {
        match self.handles.get(pid) {
            Some(handle) => handle
                .send_user(msg)
                .await
                .map_err(|_| format!("process {pid} mailbox closed")),
            None => Err(format!("process {pid} not found")),
        }
    }

    /// Kill a process via out-of-band CancellationToken.
    pub fn kill(&self, pid: &Pid) {
        if let Some(handle) = self.handles.get(pid) {
            handle.kill();
        }
    }

    /// Remove a process from the registry (call after join completes).
    pub fn remove(&self, pid: &Pid) {
        self.handles.remove(pid);
    }

    /// Number of registered processes.
    pub fn count(&self) -> usize {
        self.handles.len()
    }
}
```

Note: The above has a `todo!()` because `spawn_process` allocates the Pid internally but the registry needs to know it. We need to refactor.

**Step 4: Refactor spawn_process to return Pid**

In `process.rs`, change `spawn_process` to return `(Pid, ProcessHandle, JoinHandle<ProcessExit>)`:

```rust
pub fn spawn_process(
    mut behavior: impl Behavior,
    user_mailbox_capacity: usize,
    checkpoint: Option<Vec<u8>>,
) -> (Pid, ProcessHandle, JoinHandle<ProcessExit>) {
    let pid = Pid::new();
    let (handle, mailbox) = create_mailbox(user_mailbox_capacity, 32);

    let kill_token = handle.kill_token.clone();
    let join = tokio::spawn(async move {
        run_process(pid, &mut behavior, mailbox, kill_token, checkpoint).await
    });

    (pid, handle, join)
}
```

Update `process.rs` tests to use 3-tuple return.

Then fix the registry:

```rust
pub fn spawn(
    &self,
    behavior: impl Behavior,
    user_mailbox_capacity: usize,
    checkpoint: Option<Vec<u8>>,
) -> (Pid, JoinHandle<ProcessExit>) {
    let (pid, handle, join) = spawn_process(behavior, user_mailbox_capacity, checkpoint);
    self.handles.insert(pid, handle);
    (pid, join)
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_registry && cargo test -p zeptovm -- test_process`
Expected: All PASS

**Step 6: Commit**

```bash
git add zeptovm/src/registry.rs zeptovm/src/process.rs
git commit -m "feat(zeptovm): ProcessRegistry with DashMap, refactored spawn"
```

---

### Task 8: Gate v0 — integration smoke test

**Files:**
- Create: `zeptovm/tests/v0_gate.rs` (integration test)

This test validates the v0 parity gate: spawn 100 processes, deliver 10k messages, clean shutdown.

**Step 1: Write the integration test**

```rust
//! v0 Gate Test: spawn 100 processes, deliver 10k messages, clean shutdown.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use zeptovm::behavior::Behavior;
use zeptovm::error::{Action, Message, Reason};
use zeptovm::registry::ProcessRegistry;

struct CounterBehavior {
    count: Arc<AtomicU32>,
}

#[async_trait]
impl Behavior for CounterBehavior {
    async fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn handle(&mut self, _msg: Message) -> Action {
        self.count.fetch_add(1, Ordering::Relaxed);
        Action::Continue
    }

    async fn terminate(&mut self, _reason: &Reason) {}
}

#[tokio::test]
async fn gate_v0_spawn_100_deliver_10k() {
    let registry = ProcessRegistry::new();
    let total_count = Arc::new(AtomicU32::new(0));

    // Spawn 100 processes
    let mut pids = Vec::new();
    let mut joins = Vec::new();
    for _ in 0..100 {
        let behavior = CounterBehavior {
            count: total_count.clone(),
        };
        let (pid, join) = registry.spawn(behavior, 256, None);
        pids.push(pid);
        joins.push(join);
    }

    assert_eq!(registry.count(), 100);

    // Deliver 10k messages (100 per process)
    for pid in &pids {
        for i in 0..100 {
            registry
                .send(pid, Message::text(format!("msg-{i}")))
                .await
                .unwrap();
        }
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill all processes
    for pid in &pids {
        registry.kill(pid);
    }

    // Wait for all to exit
    for join in joins {
        let exit = join.await.unwrap();
        assert!(matches!(exit.reason, Reason::Kill));
    }

    // Verify all 10k messages were processed
    let total = total_count.load(Ordering::Relaxed);
    assert_eq!(total, 10_000, "expected 10000 messages processed, got {total}");
}

#[tokio::test]
async fn gate_v0_clean_shutdown_on_drop() {
    let registry = ProcessRegistry::new();
    let count = Arc::new(AtomicU32::new(0));

    let behavior = CounterBehavior {
        count: count.clone(),
    };
    let (_pid, join) = registry.spawn(behavior, 16, None);

    // Drop registry (drops all handles) — process should exit
    drop(registry);

    let exit = join.await.unwrap();
    // Process exits with Normal when all senders drop
    assert!(matches!(exit.reason, Reason::Normal));
}
```

**Step 2: Run the gate test**

Run: `cargo test -p zeptovm --test v0_gate`
Expected: 2 tests PASS

**Step 3: Commit**

```bash
git add zeptovm/tests/v0_gate.rs
git commit -m "test(zeptovm): v0 gate — 100 processes, 10k messages, clean shutdown"
```

---

## Stage v1: Supervision

**Gate:** OneForOne restart works, max_restarts escalation, exit truth table passes.

---

### Task 9: Link table

**Files:**
- Create: `zeptovm/src/link.rs`
- Modify: `zeptovm/src/lib.rs`
- Test: inline `#[cfg(test)]`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;

    #[test]
    fn test_link_bidirectional() {
        let mut table = LinkTable::new();
        let a = Pid::from_raw(1);
        let b = Pid::from_raw(2);
        table.link(a, b);
        assert_eq!(table.get_links(&a), vec![b]);
        assert_eq!(table.get_links(&b), vec![a]);
    }

    #[test]
    fn test_unlink() {
        let mut table = LinkTable::new();
        let a = Pid::from_raw(1);
        let b = Pid::from_raw(2);
        table.link(a, b);
        table.unlink(a, b);
        assert!(table.get_links(&a).is_empty());
        assert!(table.get_links(&b).is_empty());
    }

    #[test]
    fn test_remove_all_links() {
        let mut table = LinkTable::new();
        let a = Pid::from_raw(1);
        let b = Pid::from_raw(2);
        let c = Pid::from_raw(3);
        table.link(a, b);
        table.link(a, c);
        let linked = table.remove_all(&a);
        assert_eq!(linked.len(), 2);
        assert!(table.get_links(&a).is_empty());
        assert!(table.get_links(&b).is_empty());
        assert!(table.get_links(&c).is_empty());
    }

    #[test]
    fn test_monitor_unidirectional() {
        let mut table = LinkTable::new();
        let watcher = Pid::from_raw(1);
        let target = Pid::from_raw(2);
        let mref = table.monitor(watcher, target);
        assert_eq!(table.get_monitors_of(&target), vec![(mref, watcher)]);
    }

    #[test]
    fn test_demonitor() {
        let mut table = LinkTable::new();
        let watcher = Pid::from_raw(1);
        let target = Pid::from_raw(2);
        let mref = table.monitor(watcher, target);
        table.demonitor(mref);
        assert!(table.get_monitors_of(&target).is_empty());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_link && cargo test -p zeptovm -- test_unlink && cargo test -p zeptovm -- test_monitor`
Expected: FAIL

**Step 3: Implement LinkTable**

```rust
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::pid::Pid;

static NEXT_MONITOR_REF: AtomicU64 = AtomicU64::new(1);

/// Opaque reference identifying a monitor relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(u64);

impl MonitorRef {
    pub fn new() -> Self {
        Self(NEXT_MONITOR_REF.fetch_add(1, Ordering::Relaxed))
    }
}

/// Tracks bidirectional links and unidirectional monitors between processes.
pub struct LinkTable {
    /// Bidirectional links: pid -> set of linked pids
    links: HashMap<Pid, HashSet<Pid>>,
    /// Monitors: target_pid -> vec of (MonitorRef, watcher_pid)
    monitors: HashMap<Pid, Vec<(MonitorRef, Pid)>>,
    /// Reverse index: MonitorRef -> (watcher, target) for demonitor
    monitor_index: HashMap<MonitorRef, (Pid, Pid)>,
}

impl LinkTable {
    pub fn new() -> Self {
        Self {
            links: HashMap::new(),
            monitors: HashMap::new(),
            monitor_index: HashMap::new(),
        }
    }

    /// Create a bidirectional link between two processes.
    pub fn link(&mut self, a: Pid, b: Pid) {
        self.links.entry(a).or_default().insert(b);
        self.links.entry(b).or_default().insert(a);
    }

    /// Remove a bidirectional link.
    pub fn unlink(&mut self, a: Pid, b: Pid) {
        if let Some(set) = self.links.get_mut(&a) {
            set.remove(&b);
            if set.is_empty() {
                self.links.remove(&a);
            }
        }
        if let Some(set) = self.links.get_mut(&b) {
            set.remove(&a);
            if set.is_empty() {
                self.links.remove(&b);
            }
        }
    }

    /// Get all pids linked to the given pid.
    pub fn get_links(&self, pid: &Pid) -> Vec<Pid> {
        self.links
            .get(pid)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Remove all links for a pid. Returns the list of previously-linked pids.
    pub fn remove_all(&mut self, pid: &Pid) -> Vec<Pid> {
        let linked = self.get_links(pid);
        self.links.remove(pid);
        for other in &linked {
            if let Some(set) = self.links.get_mut(other) {
                set.remove(pid);
                if set.is_empty() {
                    self.links.remove(other);
                }
            }
        }
        linked
    }

    /// Create a unidirectional monitor: watcher monitors target.
    pub fn monitor(&mut self, watcher: Pid, target: Pid) -> MonitorRef {
        let mref = MonitorRef::new();
        self.monitors.entry(target).or_default().push((mref, watcher));
        self.monitor_index.insert(mref, (watcher, target));
        mref
    }

    /// Remove a monitor.
    pub fn demonitor(&mut self, mref: MonitorRef) {
        if let Some((_, target)) = self.monitor_index.remove(&mref) {
            if let Some(monitors) = self.monitors.get_mut(&target) {
                monitors.retain(|(r, _)| *r != mref);
                if monitors.is_empty() {
                    self.monitors.remove(&target);
                }
            }
        }
    }

    /// Get all monitors watching the given target pid.
    pub fn get_monitors_of(&self, target: &Pid) -> Vec<(MonitorRef, Pid)> {
        self.monitors
            .get(target)
            .cloned()
            .unwrap_or_default()
    }

    /// Remove all monitors of a target. Returns the watchers to notify.
    pub fn remove_monitors_of(&mut self, target: &Pid) -> Vec<(MonitorRef, Pid)> {
        let monitors = self.monitors.remove(target).unwrap_or_default();
        for (mref, _) in &monitors {
            self.monitor_index.remove(mref);
        }
        monitors
    }
}

#[cfg(test)]
mod tests {
    // ... tests from step 1
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_link && cargo test -p zeptovm -- test_unlink && cargo test -p zeptovm -- test_monitor && cargo test -p zeptovm -- test_demonitor && cargo test -p zeptovm -- test_remove`
Expected: 5 tests PASS

**Step 5: Commit**

```bash
git add zeptovm/src/link.rs zeptovm/src/lib.rs
git commit -m "feat(zeptovm): LinkTable with bidirectional links and monitors"
```

---

### Task 10: Supervisor (OneForOne)

**Files:**
- Create: `zeptovm/src/supervisor.rs`
- Modify: `zeptovm/src/lib.rs`
- Test: inline `#[cfg(test)]`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior::Behavior;
    use crate::error::{Action, Message, Reason};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct CrashOnSecondMsg {
        init_count: Arc<AtomicU32>,
        msg_count: AtomicU32,
    }

    #[async_trait]
    impl Behavior for CrashOnSecondMsg {
        async fn init(&mut self, _cp: Option<Vec<u8>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.init_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        async fn handle(&mut self, _msg: Message) -> Action {
            let n = self.msg_count.fetch_add(1, Ordering::Relaxed);
            if n >= 1 {
                Action::Stop(Reason::Custom("crash".into()))
            } else {
                Action::Continue
            }
        }
        async fn terminate(&mut self, _reason: &Reason) {}
    }

    #[tokio::test]
    async fn test_supervisor_one_for_one_restarts_crashed_child() {
        let init_count = Arc::new(AtomicU32::new(0));
        let ic = init_count.clone();

        let spec = SupervisorSpec {
            strategy: RestartStrategy::OneForOne,
            max_restarts: 3,
            restart_window: Duration::from_secs(60),
            children: vec![ChildSpec {
                id: "crasher".into(),
                factory: Box::new(move || {
                    Box::new(CrashOnSecondMsg {
                        init_count: ic.clone(),
                        msg_count: AtomicU32::new(0),
                    })
                }),
                restart: RestartPolicy::OnFailure,
                shutdown: ShutdownPolicy::Timeout(Duration::from_secs(5)),
                user_mailbox_capacity: 16,
            }],
        };

        let (_sup_handle, sup_join) = start_supervisor(spec);

        // Give supervisor time to start child
        tokio::time::sleep(Duration::from_millis(100)).await;

        // init_count should be 1 (initial start)
        assert_eq!(init_count.load(Ordering::Relaxed), 1);

        // TODO: send message to the child to trigger crash, verify restart
        // This requires the supervisor to expose child handles — will implement
        // the full test after supervisor wiring is complete.
    }

    #[test]
    fn test_restart_policy_should_restart() {
        assert!(RestartPolicy::Always.should_restart(&Reason::Normal));
        assert!(RestartPolicy::Always.should_restart(&Reason::Custom("x".into())));
        assert!(!RestartPolicy::OnFailure.should_restart(&Reason::Normal));
        assert!(RestartPolicy::OnFailure.should_restart(&Reason::Custom("x".into())));
        assert!(!RestartPolicy::Never.should_restart(&Reason::Normal));
        assert!(!RestartPolicy::Never.should_restart(&Reason::Custom("x".into())));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm -- test_supervisor && cargo test -p zeptovm -- test_restart_policy`
Expected: FAIL

**Step 3: Implement supervisor types and start_supervisor**

```rust
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::behavior::Behavior;
use crate::error::Reason;
use crate::mailbox::ProcessHandle;
use crate::pid::Pid;
use crate::process::ProcessExit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    OneForOne,
    OneForAll,
    RestForOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

impl RestartPolicy {
    pub fn should_restart(&self, reason: &Reason) -> bool {
        match self {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => reason.is_abnormal(),
            RestartPolicy::Never => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownPolicy {
    Timeout(Duration),
    Brutal,
}

pub struct ChildSpec {
    pub id: String,
    pub factory: Box<dyn Fn() -> Box<dyn Behavior> + Send>,
    pub restart: RestartPolicy,
    pub shutdown: ShutdownPolicy,
    pub user_mailbox_capacity: usize,
}

pub struct SupervisorSpec {
    pub strategy: RestartStrategy,
    pub max_restarts: u32,
    pub restart_window: Duration,
    pub children: Vec<ChildSpec>,
}

struct ChildState {
    pid: Pid,
    handle: ProcessHandle,
    join: JoinHandle<ProcessExit>,
    spec_index: usize,
}

/// Start a supervisor as a tokio task.
/// Returns a handle to kill the supervisor and a JoinHandle.
pub fn start_supervisor(
    spec: SupervisorSpec,
) -> (ProcessHandle, JoinHandle<()>) {
    let (handle, mailbox) = crate::mailbox::create_mailbox(16, 32);
    let kill_token = handle.kill_token.clone();

    let join = tokio::spawn(async move {
        run_supervisor(spec, kill_token).await;
    });

    (handle, join)
}

async fn run_supervisor(
    spec: SupervisorSpec,
    kill_token: tokio_util::sync::CancellationToken,
) {
    let mut children: Vec<ChildState> = Vec::new();
    let mut restart_times: Vec<Instant> = Vec::new();

    // Start all children
    for (i, child_spec) in spec.children.iter().enumerate() {
        match spawn_child(child_spec) {
            Some(child) => {
                info!(child_id = %child_spec.id, pid = %child.pid, "supervisor started child");
                children.push(ChildState {
                    pid: child.pid,
                    handle: child.handle,
                    join: child.join,
                    spec_index: i,
                });
            }
            None => {
                warn!(child_id = %child_spec.id, "supervisor failed to start child");
            }
        }
    }

    // Supervision loop: watch for child exits
    loop {
        tokio::select! {
            biased;

            _ = kill_token.cancelled() => {
                info!("supervisor killed, shutting down children");
                for child in &children {
                    child.handle.kill();
                }
                for child in children {
                    let _ = child.join.await;
                }
                return;
            }

            // Poll all children for exits
            result = poll_any_child_exit(&mut children) => {
                if let Some((exit, spec_index)) = result {
                    let child_spec = &spec.children[spec_index];
                    info!(
                        child_id = %child_spec.id,
                        pid = %exit.pid,
                        reason = %exit.reason,
                        "child exited"
                    );

                    if child_spec.restart.should_restart(&exit.reason) {
                        // Check restart intensity
                        let now = Instant::now();
                        restart_times.retain(|t| now.duration_since(*t) < spec.restart_window);
                        restart_times.push(now);

                        if restart_times.len() as u32 > spec.max_restarts {
                            warn!(
                                max = spec.max_restarts,
                                window_secs = spec.restart_window.as_secs(),
                                "supervisor max restarts exceeded, escalating"
                            );
                            // Kill remaining children and exit
                            for child in &children {
                                child.handle.kill();
                            }
                            for child in children {
                                let _ = child.join.await;
                            }
                            return;
                        }

                        // Restart based on strategy
                        match spec.strategy {
                            RestartStrategy::OneForOne => {
                                if let Some(new_child) = spawn_child(child_spec) {
                                    info!(
                                        child_id = %child_spec.id,
                                        new_pid = %new_child.pid,
                                        "child restarted"
                                    );
                                    children.push(ChildState {
                                        pid: new_child.pid,
                                        handle: new_child.handle,
                                        join: new_child.join,
                                        spec_index,
                                    });
                                }
                            }
                            RestartStrategy::OneForAll | RestartStrategy::RestForOne => {
                                // TODO: implement in a later task
                                warn!("OneForAll/RestForOne not yet implemented, using OneForOne");
                                if let Some(new_child) = spawn_child(child_spec) {
                                    children.push(ChildState {
                                        pid: new_child.pid,
                                        handle: new_child.handle,
                                        join: new_child.join,
                                        spec_index,
                                    });
                                }
                            }
                        }
                    }
                } else {
                    // No children left
                    info!("supervisor has no children, exiting");
                    return;
                }
            }
        }
    }
}

struct SpawnedChild {
    pid: Pid,
    handle: ProcessHandle,
    join: JoinHandle<ProcessExit>,
}

fn spawn_child(spec: &ChildSpec) -> Option<SpawnedChild> {
    let behavior = (spec.factory)();
    let (pid, handle, join) = crate::process::spawn_process_boxed(
        behavior,
        spec.user_mailbox_capacity,
        None,
    );
    Some(SpawnedChild { pid, handle, join })
}

/// Wait for any child to exit. Returns the exit result and spec index.
async fn poll_any_child_exit(
    children: &mut Vec<ChildState>,
) -> Option<(ProcessExit, usize)> {
    if children.is_empty() {
        return None;
    }

    // Use select on all join handles
    let (exit, index) = {
        // Build a future that resolves when any child exits
        let mut futures: Vec<_> = children
            .iter_mut()
            .enumerate()
            .map(|(i, child)| {
                let join = &mut child.join;
                Box::pin(async move { (i, join.await) })
            })
            .collect();

        // Select the first one to complete
        let (result, _, _) = futures::future::select_all(futures).await;
        result
    };

    let spec_index = children[index].spec_index;
    children.swap_remove(index);

    match exit {
        Ok(process_exit) => Some((process_exit, spec_index)),
        Err(e) => {
            warn!(error = %e, "child task panicked");
            Some((
                ProcessExit {
                    pid: Pid::from_raw(0),
                    reason: Reason::Custom(format!("task panic: {e}")),
                },
                spec_index,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    // ... tests from step 1
}
```

**Step 4: Add spawn_process_boxed to process.rs**

We need a variant that accepts `Box<dyn Behavior>`:

```rust
/// Spawn a process from a boxed Behavior (used by supervisor).
pub fn spawn_process_boxed(
    mut behavior: Box<dyn Behavior>,
    user_mailbox_capacity: usize,
    checkpoint: Option<Vec<u8>>,
) -> (Pid, ProcessHandle, JoinHandle<ProcessExit>) {
    let pid = Pid::new();
    let (handle, mailbox) = create_mailbox(user_mailbox_capacity, 32);

    let kill_token = handle.kill_token.clone();
    let join = tokio::spawn(async move {
        run_process(pid, behavior.as_mut(), mailbox, kill_token, checkpoint).await
    });

    (pid, handle, join)
}
```

**Step 5: Add futures dependency**

Add to `zeptovm/Cargo.toml`:
```toml
futures = "0.3"
```

**Step 6: Run tests to verify they pass**

Run: `cargo test -p zeptovm -- test_restart_policy && cargo test -p zeptovm -- test_supervisor`
Expected: PASS

**Step 7: Commit**

```bash
git add zeptovm/src/supervisor.rs zeptovm/src/process.rs zeptovm/src/lib.rs zeptovm/Cargo.toml
git commit -m "feat(zeptovm): supervisor with OneForOne restart strategy"
```

---

### Task 11: Exit signal propagation

**Files:**
- Modify: `zeptovm/src/process.rs` (add exit propagation via link table)
- Modify: `zeptovm/src/registry.rs` (add link/monitor API)
- Create: `zeptovm/tests/v1_exit_signals.rs`

**Step 1: Write the integration test for exit signal truth table**

```rust
//! v1 Exit Signal Truth Table Tests
//!
//! | Dying reason | Receiver trap_exit? | Result |
//! |-------------|-------------------|--------|
//! | normal | false | ignore (link removed) |
//! | abnormal | false | receiver dies with same reason |
//! | kill | either | receiver dies with `killed` |

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use zeptovm::behavior::Behavior;
use zeptovm::error::{Action, Message, Reason};

struct WaitForever {
    terminated: Arc<AtomicBool>,
}

#[async_trait]
impl Behavior for WaitForever {
    async fn init(&mut self, _cp: Option<Vec<u8>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn handle(&mut self, _msg: Message) -> Action {
        Action::Continue
    }
    async fn terminate(&mut self, _reason: &Reason) {
        self.terminated.store(true, Ordering::Relaxed);
    }
}

struct DieImmediately {
    reason: Reason,
}

#[async_trait]
impl Behavior for DieImmediately {
    async fn init(&mut self, _cp: Option<Vec<u8>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
    async fn handle(&mut self, _msg: Message) -> Action {
        Action::Stop(self.reason.clone())
    }
    async fn terminate(&mut self, _reason: &Reason) {}
}

// Tests will call registry.link(a, b) then kill a and check b's fate.
// Full implementation requires wiring exit propagation into the registry/process
// loop which will be done in this task's implementation step.
```

**Step 2: Implement exit signal propagation**

This requires extending `ProcessRegistry` to hold a `LinkTable` (wrapped in `Arc<Mutex<LinkTable>>`) and notifying linked/monitored processes when a process exits.

Add to `registry.rs`:
- `link(&self, a: Pid, b: Pid)`
- `monitor(&self, watcher: Pid, target: Pid) -> MonitorRef`
- `notify_exit(&self, pid: Pid, reason: &Reason)` — sends `SystemMsg::ExitLinked` to all linked processes and `SystemMsg::MonitorDown` to all monitors

**Step 3: Run the truth table tests**

Run: `cargo test -p zeptovm --test v1_exit_signals`
Expected: PASS

**Step 4: Commit**

```bash
git add zeptovm/src/registry.rs zeptovm/src/process.rs zeptovm/tests/v1_exit_signals.rs
git commit -m "feat(zeptovm): exit signal propagation via links and monitors"
```

---

### Task 12: Gate v1 — supervision integration test

**Files:**
- Create: `zeptovm/tests/v1_gate.rs`

**Step 1: Write the gate test**

Test scenarios:
1. OneForOne: one child crashes, only that child restarts
2. max_restarts exceeded: supervisor itself exits
3. Normal exit with OnFailure policy: no restart
4. Exit signal propagation: linked process dies when partner crashes

```rust
//! v1 Gate Test: supervision tree with restart, escalation, and exit signals.

// ... comprehensive integration tests validating the supervisor behavior
```

**Step 2: Run the gate test**

Run: `cargo test -p zeptovm --test v1_gate`
Expected: PASS

**Step 3: Commit**

```bash
git add zeptovm/tests/v1_gate.rs
git commit -m "test(zeptovm): v1 gate — supervision restart, escalation, exit signals"
```

---

## Stage v2: Durability

**Gate:** Process recovers from checkpoint after kill. WAL replay is correct.

---

### Task 13: CheckpointStore trait + SqliteCheckpointStore

**Files:**
- Create: `zeptovm/src/durability/mod.rs`
- Create: `zeptovm/src/durability/checkpoint.rs`
- Modify: `zeptovm/src/lib.rs`
- Test: inline `#[cfg(test)]`

**Step 1: Write failing tests**

Test `SqliteCheckpointStore::save`, `load`, `delete` with temp database.

**Step 2: Implement CheckpointStore trait and Sqlite implementation**

```rust
use async_trait::async_trait;
use crate::pid::Pid;

#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn save(&self, pid: Pid, data: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn load(&self, pid: Pid) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>>;
    async fn delete(&self, pid: Pid) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
```

Add `rusqlite = { version = "0.31", features = ["bundled"] }` to Cargo.toml.

**Step 3: Run tests, commit**

```bash
git commit -m "feat(zeptovm): CheckpointStore trait + SqliteCheckpointStore"
```

---

### Task 14: WAL (Write-Ahead Log)

**Files:**
- Create: `zeptovm/src/durability/wal.rs`
- Test: inline `#[cfg(test)]`

**Step 1: Write failing tests**

Test `WalWriter::append`, `WalReader::replay_from`, compaction.

**Step 2: Implement WalEntry, WalWriter, WalReader**

Key invariant: `wal_seq` is the global monotonic cursor. Recovery replays from `last_acked_wal_seq + 1`.

**Step 3: Run tests, commit**

```bash
git commit -m "feat(zeptovm): WAL writer/reader with global wal_seq cursor"
```

---

### Task 15: Recovery sequence

**Files:**
- Create: `zeptovm/src/durability/recovery.rs`
- Test: inline + integration test

**Step 1: Write failing tests**

Test the 10-step recovery: checkpoint load -> WAL replay -> dedup by message_id -> correct ordering.

**Step 2: Implement recovery sequence**

**Step 3: Run tests, commit**

```bash
git commit -m "feat(zeptovm): 10-step recovery sequence with checkpoint + WAL replay"
```

---

### Task 16: Wire checkpoint into process run loop

**Files:**
- Modify: `zeptovm/src/process.rs`
- Test: integration test

**Step 1: When `Action::Checkpoint` is returned, call `behavior.checkpoint()` and persist via CheckpointStore.**

**Step 2: Test: spawn process with checkpoint behavior, send messages, kill, recover from checkpoint.**

**Step 3: Commit**

```bash
git commit -m "feat(zeptovm): checkpoint save/restore wired into process loop"
```

---

### Task 17: Gate v2 — durability integration test

**Files:**
- Create: `zeptovm/tests/v2_gate.rs`

Test: process saves checkpoint, gets killed, recovers with correct state. WAL replays messages missed between checkpoint and kill.

```bash
git commit -m "test(zeptovm): v2 gate — checkpoint recovery and WAL replay"
```

---

## Stage v3: Control Plane

**Gate:** Budget precheck blocks exhausted agent. Provider gate enforces RPM.

---

### Task 18: Budget gate (two-phase)

**Files:**
- Create: `zeptovm/src/control/mod.rs`
- Create: `zeptovm/src/control/budget.rs`
- Test: inline `#[cfg(test)]`

Implement `BudgetGate` with `precheck()` and `commit()` as specified in design doc.

```bash
git commit -m "feat(zeptovm): two-phase BudgetGate with token and cost limits"
```

---

### Task 19: Provider gate

**Files:**
- Create: `zeptovm/src/control/provider_gate.rs`
- Test: inline `#[cfg(test)]`

Implement `ProviderGate` with semaphore-based RPM/TPM enforcement.

```bash
git commit -m "feat(zeptovm): ProviderGate with RPM/TPM semaphore"
```

---

### Task 20: Admission control (weighted fair queuing)

**Files:**
- Create: `zeptovm/src/control/admission.rs`
- Test: inline `#[cfg(test)]`

Implement priority weights (High:4, Normal:2, Low:1) with starvation prevention (60s promotion).

```bash
git commit -m "feat(zeptovm): admission control with weighted fair queuing"
```

---

### Task 21: Gate v3 — control plane integration test

**Files:**
- Create: `zeptovm/tests/v3_gate.rs`

Test: exhausted budget blocks agent, provider gate enforces RPM, priority ordering.

```bash
git commit -m "test(zeptovm): v3 gate — budget, provider gate, admission control"
```

---

## Stage v4: ZeptoBeam Integration

**Gate:** ZeptoClaw agent runs via ZeptoBeam config. Swarm DAG executes.

---

### Task 22: Agent adapter (Behavior impl for ZeptoClaw agents)

**Files:**
- Create: `zeptobeam/src/agent_adapter.rs` (new, alongside existing main.rs)

Implements `zeptovm::Behavior` for ZeptoClaw agent configs. This is the bridge between the agent-agnostic `zeptovm` runtime and ZeptoClaw's LLM agent definitions.

**Note:** This is in the `zeptobeam` crate, not `zeptovm`. Add `zeptovm` as a dependency of `zeptobeam`.

```bash
git commit -m "feat(zeptobeam): AgentAdapter implements zeptovm::Behavior for ZeptoClaw agents"
```

---

### Task 23: Config loader (TOML parsing + merge algorithm)

**Files:**
- Create: `zeptobeam/src/config_loader.rs`

Implements the merge algorithm from the config contract:
1. Resolve `agent_ref` in ZeptoClaw config
2. Apply overrides (scalar, then tool delta)
3. Compute `effective_config_hash`
4. Run all 10 startup validation rules

```bash
git commit -m "feat(zeptobeam): config loader with merge algorithm and fail-fast validation"
```

---

### Task 24: Swarm orchestration (DAG execution)

**Files:**
- Create: `zeptobeam/src/swarm.rs`

Implements DAG, sequential, and parallel swarm strategies using `zeptovm` processes.

```bash
git commit -m "feat(zeptobeam): swarm orchestration with DAG/sequential/parallel strategies"
```

---

### Task 25: Wire new runtime into zeptobeam daemon

**Files:**
- Modify: `zeptobeam/src/main.rs`

Add a new daemon mode that uses `zeptovm` processes instead of the `agent_rt` scheduler. The old mode remains for backwards compatibility during the strangler migration.

```bash
git commit -m "feat(zeptobeam): daemon mode using zeptovm runtime (alongside legacy agent_rt)"
```

---

### Task 26: Gate v4 — end-to-end integration test

**Files:**
- Create: `zeptobeam/tests/v4_gate.rs`

Test: load ZeptoBeam TOML config, resolve ZeptoClaw agent refs, spawn agents via zeptovm, execute a two-step swarm DAG, verify output.

```bash
git commit -m "test(zeptobeam): v4 gate — end-to-end ZeptoClaw agent via zeptovm runtime"
```

---

## Summary

| Stage | Tasks | Key Deliverable |
|-------|-------|-----------------|
| v0 | 1-8 | Process runtime: Pid, Mailbox, Registry, Run Loop |
| v1 | 9-12 | Supervision: Links, Monitors, Restart Strategies |
| v2 | 13-17 | Durability: Checkpoint, WAL, Recovery |
| v3 | 18-21 | Control Plane: Budget, Provider Gate, Admission |
| v4 | 22-26 | Integration: Agent Adapter, Config, Swarm, Daemon |

Each gate must pass before proceeding to the next stage. Tasks within a stage are sequential (each builds on the previous).
