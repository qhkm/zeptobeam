# Phase 6: Advanced Orchestration Design

**Goal:** Add sophisticated multi-agent coordination patterns to the existing orchestrator: DAG task dependencies, retry policies, resource budgets, approval gates, streaming results, hierarchical sub-orchestrators, and pluggable result aggregation.

**Working directory:** `~/ios/zeptoclaw-rt`

**Prerequisite:** Phase 5 (Production Readiness) — config system for budgets/retry/pricing, health server for approval endpoints. Phase 4 — timeouts/backoff already done.

**Core principle:** An orchestration with no DAG dependencies, no budget, no approval gates, and no retry policy behaves exactly like today. Each feature activates only when configured.

---

## Architecture

Extend the existing `OrchestratorBehavior` and `WorkerBehavior` with 7 composable modules, each opt-in:

```
OrchestratorBehavior (extended)
+-- TaskGraph (DAG engine)         -- resolves dependencies, emits ready tasks
+-- RetryPolicy (per-task)         -- retries failed workers with backoff
+-- ResourceBudget                 -- tracks tokens/cost, stops when exhausted
+-- ApprovalGate                   -- pauses tasks pending HTTP approval
+-- ResultAggregator (pluggable)   -- combines worker results (concat/vote/merge)
+-- SubOrchestrator (hierarchical) -- spawns child orchestrators for subtrees
+-- StreamingResults               -- partial result forwarding during execution
```

**New files:**

```
lib-erlangrt/src/agent_rt/
+-- orchestration.rs        (MODIFY -- extend OrchestratorBehavior/State)
+-- task_graph.rs            (NEW -- DAG dependency resolution)
+-- retry_policy.rs          (NEW -- per-task retry with backoff)
+-- resource_budget.rs       (NEW -- token/cost tracking and enforcement)
+-- approval_gate.rs         (NEW -- pause/resume with HTTP integration)
+-- result_aggregator.rs     (NEW -- pluggable result combination strategies)
+-- server.rs                (MODIFY -- add POST /approve/{task_id} endpoint)
```

---

## 1. DAG Task Dependencies (`task_graph.rs`)

**Current state:** Tasks are stored in a flat `VecDeque<Value>`, spawned in order up to `max_concurrency`. No dependency concept.

**Changes:**

New `TaskGraph` struct replaces the flat queue:

```rust
pub struct TaskGraph {
    tasks: HashMap<String, TaskNode>,
}

pub struct TaskNode {
    task: serde_json::Value,
    depends_on: Vec<String>,
    status: TaskStatus,
}

pub enum TaskStatus {
    Pending,           // waiting for dependencies
    Ready,             // all deps complete, can spawn
    AwaitingApproval,  // requires human approval before spawn
    Running,           // worker spawned
    Completed,         // worker returned result
    Failed,            // worker failed permanently
    Skipped,           // skipped due to failed dependency or skip retry
}
```

**Task JSON format** (from LLM decomposition response):
```json
{
  "task_id": "deploy",
  "prompt": "deploy to production",
  "depends_on": ["build", "test"]
}
```

Tasks without `depends_on` are immediately Ready. When a task completes, the graph promotes any Pending tasks whose dependencies are all Completed to Ready.

**Key methods:**
- `add_tasks(tasks: Vec<Value>)` -- parse and insert, detect cycles (reject with error)
- `ready_tasks() -> Vec<&Value>` -- return tasks with all deps completed
- `mark_completed(task_id)` / `mark_failed(task_id)` / `mark_running(task_id)`
- `is_complete() -> bool` -- all tasks in terminal state (Completed/Failed/Skipped)
- `fail_dependents(task_id)` -- cascade failure to downstream tasks

**Cycle detection:** Topological sort on `add_tasks`. If a cycle is detected, the orchestrator rejects the decomposition and returns an error result.

**Backward compatibility:** If no task has `depends_on`, the graph degenerates to a flat queue (same order as the array). Identical to current behavior.

---

## 2. Retry Policies (`retry_policy.rs`)

**Current state:** Worker failure (DOWN message) records an error and moves on. No retry.

**Changes:**

```rust
pub enum RetryStrategy {
    None,                                                    // fail permanently (current behavior)
    Immediate { max_attempts: u32 },                         // retry right away
    Backoff { max_attempts: u32, base_ms: u64, max_ms: u64 }, // exponential backoff
    Skip,                                                    // mark as skipped, continue graph
}

pub struct RetryTracker {
    attempts: HashMap<String, u32>,              // task_id -> attempt count
    default_strategy: RetryStrategy,             // from orchestrator config
    per_task: HashMap<String, RetryStrategy>,     // per-task overrides from task JSON
}
```

**Flow:**
1. Worker sends DOWN with non-Normal reason
2. Orchestrator checks `RetryTracker` for the task
3. If retries remain: increment attempt count, set task back to Ready in TaskGraph, wait backoff delay (if applicable)
4. If exhausted: mark task Failed in TaskGraph, cascade to dependents

**Per-task override** in task JSON:
```json
{
  "task_id": "flaky-api",
  "retry": { "strategy": "backoff", "max_attempts": 3, "base_ms": 1000, "max_ms": 30000 }
}
```

Default strategy from config (default: `None` -- current behavior preserved).

---

## 3. Resource Budgets (`resource_budget.rs`)

**Current state:** No token or cost tracking. Orchestrations run until all tasks complete.

**Changes:**

```rust
pub struct ResourceBudget {
    max_tokens: Option<u64>,
    max_cost_usd: Option<f64>,
    tokens_used: u64,
    cost_usd: f64,
    exhausted: bool,
}

pub struct TokenPricing {
    model_prices: HashMap<String, ModelPrice>,
}

pub struct ModelPrice {
    input_per_1k: f64,
    output_per_1k: f64,
}
```

**Token tracking flow:**
1. Worker receives `IoResult::Ok(value)` from AgentChat
2. Worker extracts `usage.input_tokens` + `usage.output_tokens` from response value
3. Worker includes token usage in `worker_result` message:
   ```json
   {
     "type": "worker_result",
     "usage": { "input_tokens": 500, "output_tokens": 200, "model": "gpt-4o" }
   }
   ```
4. Orchestrator updates `ResourceBudget` with cumulative usage
5. If budget exceeded, orchestrator stops spawning new tasks, waits for in-flight workers to finish, and completes with partial results + budget_exhausted flag

**Cost calculation:** Uses `TokenPricing` from config. If no pricing config, only token counting is available (cost stays 0.0).

**Budget exhaustion response:**
```json
{
  "type": "orchestration_complete",
  "status": "budget_exhausted",
  "budget": { "tokens_used": 95000, "max_tokens": 100000, "cost_usd": 4.75 },
  "results": [...]
}
```

---

## 4. Approval Gates (`approval_gate.rs`)

**Current state:** No human-in-the-loop. All tasks auto-execute.

**Changes:**

Tasks can require approval before execution:
```json
{
  "task_id": "deploy-prod",
  "requires_approval": true,
  "approval_message": "Deploy v2.1 to production?"
}
```

**Flow:**
1. TaskGraph marks task as Ready
2. Orchestrator checks `requires_approval` -- if true, task enters `AwaitingApproval` status
3. Orchestrator registers pending approval in shared `ApprovalRegistry`
4. External system calls `POST /approve/{orch_id}/{task_id}` with `{"approved": true}` or `{"approved": false, "reason": "..."}`
5. On approval: task moves to Ready for spawning
6. On rejection: task marked Failed, downstream dependents cascade-fail

**ApprovalRegistry:**
```rust
pub struct ApprovalRegistry {
    pending: HashMap<(String, String), ApprovalRequest>,  // (orch_id, task_id) -> request
    decisions: HashMap<(String, String), ApprovalDecision>, // (orch_id, task_id) -> decision
}

pub struct ApprovalRequest {
    message: String,
    task: serde_json::Value,
    requested_at: std::time::Instant,
}

pub enum ApprovalDecision {
    Approved,
    Rejected { reason: String },
}
```

Shared via `Arc<Mutex<ApprovalRegistry>>` between orchestrator processes and HTTP handlers.

**Server endpoints** (added to existing `server.rs`):
- `POST /approve/:orch_id/:task_id` -- approve or reject
- `GET /approvals` -- list all pending approval requests with details

**Orchestrator polling:** On each `tick` / message handling cycle, the orchestrator checks the registry for decisions on its pending approvals. If a decision exists, it processes it.

---

## 5. Streaming Results

**Current state:** Workers send a single `worker_result` message with final result. Orchestrator only sees results at completion.

**Changes:**

Workers can send partial progress during multi-turn execution:
```json
{
  "type": "worker_progress",
  "worker_pid": 123,
  "task_id": "task-1",
  "progress": 0.5,
  "partial_result": { "summary_so_far": "..." }
}
```

**Orchestrator handling:**
- Store in `OrchestratorState.partial_results: HashMap<String, Vec<Value>>`
- Forward to requester if present (so parent orchestrators see progress)
- Expose via `GET /status/:orch_id` on health server

**Worker changes:** After each AgentChat turn result, worker sends a `worker_progress` message to parent before the final `worker_result`. No change to final result format.

**Backward compat:** Orchestrators that don't check `worker_progress` messages simply ignore them (falls through to `_ => Action::Continue`).

---

## 6. Hierarchical Orchestrators

**Current state:** One orchestrator spawns flat worker pool. No nesting.

**Changes:**

Sub-orchestration task type:
```json
{
  "task_id": "research-phase",
  "type": "sub_orchestration",
  "goal": "Research competitor landscape",
  "max_concurrency": 3
}
```

When the orchestrator encounters `"type": "sub_orchestration"`:
- Instead of spawning `WorkerBehavior`, spawn a new `OrchestratorBehavior` as a child process
- Pass the sub-goal, a subset of the parent's remaining budget, and the parent's checkpoint store
- Child runs its own task decomposition, DAG, retry, etc.
- Child sends `orchestration_complete` result back to parent when done
- Parent treats it like any other worker result

**Budget propagation:**
- Parent allocates a fraction of remaining budget to child (configurable, default: remaining/pending_tasks_count)
- Child tracks its own budget independently
- When child completes, unused budget does NOT return to parent (simpler, avoids coordination complexity)

**Depth limit:** Configurable `max_orchestration_depth` (default: 3) to prevent infinite recursion.

---

## 7. Result Aggregation (`result_aggregator.rs`)

**Current state:** Results stored as `Vec<Value>`, returned as-is in `orchestration_complete`.

**Changes:**

Pluggable aggregation trait:
```rust
pub trait ResultAggregator: Send + Sync {
    fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value;
}
```

**Built-in strategies:**
- `ConcatAggregator` -- return results array as-is (default, current behavior)
- `VoteAggregator` -- majority vote on a specified field (e.g., `"decision"`)
- `MergeAggregator` -- deep-merge JSON objects, arrays concatenated
- `CustomAggregator` -- send all results to LLM with a prompt template for synthesis

Aggregator runs when `TaskGraph::is_complete()` returns true, before sending `orchestration_complete` to requester.

**Config:**
```toml
[orchestration.aggregator]
strategy = "concat"
```

---

## Config Integration

Extend Phase 5 TOML config with orchestration section:

```toml
[orchestration]
max_concurrency = 4
max_orchestration_depth = 3
default_retry = "none"              # none | immediate:3 | backoff:3:1000:30000 | skip

[orchestration.budget]
max_tokens = 0                      # 0 = unlimited
max_cost_usd = 0.0                  # 0.0 = unlimited

[orchestration.pricing]
# model_name = { input_per_1k, output_per_1k }

[orchestration.aggregator]
strategy = "concat"                 # concat | vote | merge
```

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/config.rs` (add OrchestrationConfig section)

---

## Implementation Order

Build bottom-up -- each layer is independently testable:

1. **TaskGraph** -- DAG engine with cycle detection (pure data structure, no runtime dependency)
2. **RetryPolicy** -- retry tracker (pure data structure)
3. **ResourceBudget** -- budget tracker (pure data structure)
4. **ResultAggregator** -- aggregation trait + built-in strategies (pure)
5. **ApprovalGate** -- approval registry + HTTP endpoints
6. **Integrate into OrchestratorBehavior** -- wire TaskGraph, RetryTracker, ResourceBudget, ApprovalRegistry into orchestrator state and message handling
7. **Streaming results** -- extend WorkerBehavior to send progress messages
8. **Hierarchical orchestrators** -- sub_orchestration task type handling
9. **Config integration** -- OrchestrationConfig in TOML
10. **Integration tests** -- end-to-end DAG + retry + budget + approval scenarios

---

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| DAG format | `depends_on` in task JSON | No new formats; LLM can produce this naturally |
| Retry location | Orchestrator, not worker | Orchestrator decides re-spawn; worker is stateless |
| Budget tracking | Token count + cost | Token-only misses cost differences between models |
| Budget return | No return from child to parent | Avoids coordination complexity; simpler accounting |
| Approval mechanism | HTTP endpoint on health server | Uses existing axum server; external tools can call it |
| Approval polling | Check registry each message cycle | No extra timer needed; orchestrator already ticks regularly |
| Streaming | Progress messages in existing mailbox | Stays within message-passing model, no new transport |
| Hierarchical | Sub-orchestrator as child process | Reuses all existing orchestrator logic recursively |
| Aggregation | Trait-based, default concat | Current behavior preserved; extensible without modification |
| Depth limit | Configurable max_orchestration_depth | Prevents infinite recursion from LLM-generated sub-goals |
| Backward compat | All features opt-in | No config = identical to current behavior |
| Cycle detection | Topological sort on add_tasks | Fail fast rather than deadlock on circular dependencies |
