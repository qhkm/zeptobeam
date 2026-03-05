# Orchestration Layer Design

**Date:** 2026-03-05
**Status:** Approved
**Phase:** 4.5 (between supervision tree and BEAM strip)

## Goal

Add a swarm orchestration layer on top of the agent runtime. An orchestrator process decomposes goals into subtasks via LLM, spawns capped worker processes, collects results via direct messaging, and aggregates them.

## Key Decisions (from brainstorming)

1. **Pattern:** Orchestrator + Worker Pool (Symphony-style central controller, Option A)
2. **Task decomposition:** LLM-planned — orchestrator calls LLM to break goal into subtasks
3. **Orchestrator location:** Agent process (runs inside the scheduler, not external)
4. **Result delivery:** Workers send results directly to orchestrator via `Action::Send`
5. **Concurrency:** Capped — configurable `max_concurrency` limit on active workers
6. **Checkpoint/resume:** Deferred to future phase

## Architecture

```
[Orchestrator Process]
  |-- receives goal (Message::Json)
  |-- calls LLM to decompose (IoOp::Custom "decompose_goal")
  |-- receives subtasks (SystemMsg::IoResponse)
  |-- spawns workers up to max_concurrency
  |-- tracks children via SystemMsg::SpawnResult
  |-- receives worker results via Message::Json
  |-- spawns next worker if pending tasks remain
  |-- aggregates all results when done
  |-- sends final result to requester
  +-- stops when complete

[Worker Process]
  |-- receives task (Message::Json via init args or message)
  |-- calls LLM/tool (IoOp::Custom "llm_request")
  |-- receives response (SystemMsg::IoResponse)
  |-- sends result to parent (Action::Send)
  +-- stops (Action::Stop Normal)
```

## Runtime Prerequisites (Task 12, completed)

- `SystemMsg::SpawnResult { child_pid }` — parent learns child PID after spawn
- Auto-link on spawn — parent-child bidirectional links
- `IoOp::Custom { kind, payload }` — extensible I/O for LLM calls
- `handle_exit` callback — orchestrator detects worker crashes via trap_exit

## Data Flow

1. External caller sends goal to orchestrator: `Message::Json({ "goal": "..." })`
2. Orchestrator stores requester PID, issues `IoOp::Custom { kind: "decompose_goal" }`
3. Bridge returns subtask list as `IoResponse`
4. Orchestrator queues subtasks, spawns up to `max_concurrency` workers
5. Each `SpawnResult` registers the child in `active_workers`
6. Worker receives task, issues `IoOp::Custom { kind: "llm_request" }`
7. Worker receives `IoResponse`, sends result to orchestrator via `Send`, then `Stop(Normal)`
8. Orchestrator receives result, removes from active, spawns next pending if any
9. When all tasks complete, orchestrator sends aggregated result to requester

## Error Handling

- Worker crash (non-normal): orchestrator receives Exit via `handle_exit` (trap_exit=true), can retry or skip
- Bridge overflow: worker receives error in mailbox, can forward to orchestrator
- All workers failed: orchestrator sends error result to requester

## Types

```rust
pub struct OrchestratorBehavior {
    pub max_concurrency: usize,
}

pub struct OrchestratorState {
    pending_tasks: VecDeque<serde_json::Value>,
    active_workers: HashMap<u64, String>,  // child_pid_raw -> task_id
    results: Vec<serde_json::Value>,
    requester: Option<AgentPid>,
    goal: serde_json::Value,
}

pub struct WorkerBehavior;

pub struct WorkerState {
    parent: AgentPid,
    task_id: String,
}
```
