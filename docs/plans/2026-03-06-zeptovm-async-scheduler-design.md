# ZeptoVM Async-Aware Scheduler Design

> Replaces the Tokio-based cooperative process model with a BEAM-inspired preemptive scheduler adapted for async I/O workloads.

**Motivation:** The original ZeptoVM design uses `tokio::spawn` per agent process with cooperative scheduling. This provides no preemption guarantees — a stuck or CPU-heavy agent can starve others, and the scheduler cannot forcibly interrupt a running task. BEAM's scheduler solves this with reduction-based preemption, per-core run queues, and work stealing. This design forks those concepts from lib-erlangrt and adapts them for async agent workloads where 98% of time is spent waiting on I/O (LLM API calls, HTTP, DB).

**Approach:** Fork and adapt. lib-erlangrt keeps its synchronous BEAM opcode scheduler. zeptovm gets a new async-aware scheduler inspired by the same concepts. Two codebases, same mental model.

---

## 1. Scheduler Architecture

N scheduler threads (default = number of CPU cores). Each owns a local run queue of ready processes.

```
┌──────────────────────────────────────────────────┐
│                    ZeptoVM                        │
│                                                   │
│  Scheduler Thread 1 ──→ Run Queue 1               │
│  Scheduler Thread 2 ──→ Run Queue 2               │
│  Scheduler Thread N ──→ Run Queue N               │
│         ↕ work stealing                           │
│                                                   │
│  ┌──────────────────────────────────┐              │
│  │      I/O Reactor (Tokio)         │              │
│  │  - LLM API calls                │              │
│  │  - HTTP requests                │              │
│  │  - DB queries                   │              │
│  │  - Timers / timeouts            │              │
│  └──────────────────────────────────┘              │
│         ↕ completion channel                      │
│                                                   │
│  Processes suspended on I/O get re-enqueued       │
│  when their Future completes                      │
└──────────────────────────────────────────────────┘
```

The scheduler only ever touches ready processes. A million agents waiting on LLM responses cost zero scheduler time.

---

## 2. Process Model — State Machine

Each agent is a state machine driven by the scheduler, not a tokio task.

```rust
enum ProcessState {
    Ready,              // In run queue, ready to be stepped
    Suspended,          // Waiting on async I/O
    Waiting,            // Waiting for a message (mailbox empty)
    Done(Reason),       // Process has finished
}

enum StepResult {
    Continue,                                        // Handled message, may have more
    Suspend(Pin<Box<dyn Future<Output = IoResult>>>), // Waiting on I/O
    Wait,                                            // No messages, want to wait
    Done(Reason),                                    // Process is finished
}
```

The Behavior trait becomes step-based:

```rust
pub trait Behavior: Send + 'static {
    fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult;
    fn handle(&mut self, msg: Message) -> StepResult;
    fn terminate(&mut self, reason: &Reason);
}
```

`handle()` is not async. When an agent needs to call an LLM, it returns `StepResult::Suspend(future)`. The scheduler parks the process and hands the future to the I/O reactor. When the future completes, the result is delivered as a message and the process is re-enqueued.

---

## 3. I/O Reactor Bridge

A single shared Tokio runtime handles all async I/O.

```
Scheduler Thread                    I/O Reactor (Tokio)
      │                                    │
      │  process.handle(msg)               │
      │  → Suspend(llm_call_future)        │
      │                                    │
      │──── send (pid, future) ───────────→│
      │     remove process from run queue  │
      │     run next process               │
      │                                    │ tokio::spawn(future)
      │                                    │ ... 2 seconds pass ...
      │                                    │ future completes
      │←── send (pid, result) ────────────│
      │                                    │
      │  re-enqueue process                │
      │  deliver result as message         │
      │  process.handle(IoComplete(result))│
```

Communication via two crossbeam channels per scheduler thread:
- `io_submit_tx` — scheduler → reactor (submit futures)
- `io_complete_rx` — reactor → scheduler (deliver results)

The scheduler checks `io_complete_rx` on every loop iteration (non-blocking `try_recv`).

Why a shared Tokio runtime: LLM API calls use connection pools, HTTP/2 multiplexing, TLS sessions. These benefit from sharing. One Tokio runtime handles thousands of concurrent API calls.

---

## 4. Reductions, Preemption, and Work Stealing

**Reductions:** Each `handle()` call = 1 reduction. After 200 reductions (configurable, matches BEAM default), the scheduler preempts the process and moves to the next in the queue.

In practice most agents `Suspend` on I/O long before hitting 200. The limit protects against:
- Agent with flooded mailbox processing messages in a tight loop
- Buggy agent returning `Continue` without doing I/O

**Work stealing:** When a scheduler thread's run queue is empty:
1. Check `io_complete_rx` for waking processes (free)
2. Steal from another scheduler's queue (lock-free deque, steal half)
3. If all queues empty, park the thread (scheduler collapse)

**Scheduler collapse:** If only 3 of 16 schedulers have work, the other 13 sleep. CPU usage proportional to actual load.

---

## 5. Supervision and Fault Isolation

**Crash handling:** If `handle()` panics, the scheduler thread catches it with `std::panic::catch_unwind`. The process is marked `Done(Crash)` and the supervisor is notified. The scheduler thread continues running other processes unaffected.

**Supervisor as a process:** Supervisors are processes in the run queue. They receive crash notifications as messages and apply restart strategies (OneForOne, OneForAll, RestForOne).

**Exit signal propagation (OTP-compatible):**

| Dying reason | Linked process traps exit? | Result |
|---|---|---|
| normal | no | ignore |
| normal | yes | deliver as message |
| abnormal | no | linked process dies too |
| abnormal | yes | deliver as message |
| kill | either | dies with `killed` |

**Kill:** Kill flag checked by scheduler before each `step()`. If set, process terminated immediately without calling `handle()`.

**Whole-runtime crash:** A lightweight guardian OS process monitors the daemon via heartbeat. If heartbeat stops, guardian restarts daemon, which recovers from checkpoints.

---

## 6. What Changes vs What Stays

### Stays from current ZeptoVM design:
- Pid type (atomic u64 counter)
- Two-channel mailbox concept (user + control)
- Supervision specs (RestartStrategy, ChildSpec, max_restarts/window)
- Link/Monitor semantics and exit signal truth table
- Durability (checkpoint, WAL, recovery sequence)
- Config contract (agent_ref, merge algorithm)
- Resource vector (budget gate, provider gate, admission control)

### Changes:

| Component | Old (Tokio) | New (Scheduler) |
|---|---|---|
| Process execution | `tokio::spawn` task | State machine in run queue |
| `Behavior::handle()` | `async fn` | sync fn returning `StepResult` |
| I/O (LLM calls) | `await` inside handle | Return `Suspend(future)`, reactor polls |
| Scheduling | Tokio work-stealing executor | Custom N-thread scheduler with reductions |
| Preemption | None (cooperative) | After 200 reductions |
| Message delivery | `mpsc::Sender::send()` | Enqueue in process mailbox, mark Ready |
| Fairness | None guaranteed | Guaranteed by reduction counting |
| Idle CPU | Tokio parks threads | Scheduler collapse |

### New components to build:
1. `scheduler.rs` — N threads, run queues, reduction counting, scheduler collapse
2. `reactor.rs` — Tokio runtime bridge for I/O futures
3. `run_queue.rs` — Lock-free work-stealing deque
4. Rework `process.rs` — State machine instead of task
5. Rework `mailbox.rs` — Scheduler-driven instead of channel-driven

### Crate dependencies:
- **Remove:** `tokio` as process executor (keep for reactor only)
- **Add:** `crossbeam-deque` (work-stealing queues), `crossbeam-channel` (scheduler↔reactor)
- **Keep:** `tokio` (reactor), `dashmap`, `rusqlite`, `tracing`

---

## 7. Migration Stages

| Stage | Scope | Gate |
|---|---|---|
| v0 | Scheduler threads, run queues, Process state machine, step-based Behavior | 100 processes, 10k messages, fair scheduling |
| v1 | I/O reactor bridge, Suspend/Resume lifecycle | Agent can call LLM via Suspend, result delivered as message |
| v2 | Work stealing, scheduler collapse | Load balances across cores, idle cores sleep |
| v3 | Supervision, links, monitors, crash isolation | OneForOne restart, panic caught, supervisor escalation |
| v4 | Checkpoint, WAL, recovery | Process recovers from checkpoint after kill |
| v5 | Budget gate, provider gate, admission | Budget precheck blocks exhausted agent |
