# Phase 6: Advanced Orchestration Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add 7 composable orchestration features: DAG dependencies, retry policies, resource budgets, approval gates, streaming results, hierarchical orchestrators, and pluggable result aggregation.

**Architecture:** Each feature is a standalone module with its own tests. They integrate into the existing `OrchestratorBehavior` via opt-in fields on `OrchestratorState`. All features are backward compatible -- an orchestrator with no new config behaves identically to today's flat-queue orchestrator.

**Tech Stack:** Rust, serde_json, axum (approval HTTP endpoints), tokio (async approval polling)

**Working directory:** `~/ios/zeptoclaw-rt`

**Build command:** `cargo build -p erlangrt`
**Test command:** `cargo test -p erlangrt`

**Design doc:** `docs/plans/2026-03-05-advanced-orchestration-design.md`

---

### Task 1: TaskGraph -- Data Structure and Tests

**Files:**
- Create: `lib-erlangrt/src/agent_rt/task_graph.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write the failing tests**

Create `lib-erlangrt/src/agent_rt/task_graph.rs` with only types and tests:

```rust
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Ready,
    AwaitingApproval,
    Running,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug)]
pub struct TaskNode {
    pub task: serde_json::Value,
    pub depends_on: Vec<String>,
    pub status: TaskStatus,
}

pub struct TaskGraph {
    tasks: HashMap<String, TaskNode>,
    insertion_order: Vec<String>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            insertion_order: Vec::new(),
        }
    }

    /// Parse tasks from JSON array and add to graph.
    /// Each task must have "task_id". Optional "depends_on": ["id1", "id2"].
    /// Returns Err if cycle detected.
    pub fn add_tasks(&mut self, tasks: Vec<serde_json::Value>) -> Result<(), String> {
        todo!()
    }

    /// Return task values whose dependencies are all Completed.
    pub fn ready_tasks(&self) -> Vec<&serde_json::Value> {
        todo!()
    }

    pub fn mark_running(&mut self, task_id: &str) {
        todo!()
    }

    pub fn mark_completed(&mut self, task_id: &str) {
        todo!()
    }

    pub fn mark_failed(&mut self, task_id: &str) {
        todo!()
    }

    /// Cascade failure to all tasks that transitively depend on task_id.
    pub fn fail_dependents(&mut self, task_id: &str) {
        todo!()
    }

    /// True if all tasks are in terminal state (Completed/Failed/Skipped).
    pub fn is_complete(&self) -> bool {
        todo!()
    }

    pub fn task_status(&self, task_id: &str) -> Option<&TaskStatus> {
        self.tasks.get(task_id).map(|n| &n.status)
    }

    pub fn pending_count(&self) -> usize {
        self.tasks.values().filter(|n| n.status == TaskStatus::Pending).count()
    }

    pub fn set_status(&mut self, task_id: &str, status: TaskStatus) {
        if let Some(node) = self.tasks.get_mut(task_id) {
            node.status = status;
        }
    }

    fn detect_cycle(&self) -> bool {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_no_deps_all_ready() {
        let mut graph = TaskGraph::new();
        graph.add_tasks(vec![
            json!({"task_id": "a", "prompt": "do a"}),
            json!({"task_id": "b", "prompt": "do b"}),
        ]).unwrap();
        let ready = graph.ready_tasks();
        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn test_linear_deps() {
        let mut graph = TaskGraph::new();
        graph.add_tasks(vec![
            json!({"task_id": "build", "prompt": "build it"}),
            json!({"task_id": "test", "prompt": "test it", "depends_on": ["build"]}),
            json!({"task_id": "deploy", "prompt": "deploy it", "depends_on": ["test"]}),
        ]).unwrap();

        // Only build is ready initially
        assert_eq!(graph.ready_tasks().len(), 1);
        assert_eq!(graph.ready_tasks()[0]["task_id"], "build");

        graph.mark_running("build");
        assert_eq!(graph.ready_tasks().len(), 0);

        graph.mark_completed("build");
        assert_eq!(graph.ready_tasks().len(), 1);
        assert_eq!(graph.ready_tasks()[0]["task_id"], "test");

        graph.mark_running("test");
        graph.mark_completed("test");
        assert_eq!(graph.ready_tasks().len(), 1);
        assert_eq!(graph.ready_tasks()[0]["task_id"], "deploy");
    }

    #[test]
    fn test_diamond_deps() {
        let mut graph = TaskGraph::new();
        graph.add_tasks(vec![
            json!({"task_id": "start"}),
            json!({"task_id": "left", "depends_on": ["start"]}),
            json!({"task_id": "right", "depends_on": ["start"]}),
            json!({"task_id": "merge", "depends_on": ["left", "right"]}),
        ]).unwrap();

        assert_eq!(graph.ready_tasks().len(), 1); // only "start"
        graph.mark_running("start");
        graph.mark_completed("start");
        assert_eq!(graph.ready_tasks().len(), 2); // "left" and "right"

        graph.mark_running("left");
        graph.mark_completed("left");
        assert_eq!(graph.ready_tasks().len(), 0); // "merge" still waiting on "right"

        graph.mark_running("right");
        graph.mark_completed("right");
        assert_eq!(graph.ready_tasks().len(), 1); // now "merge" is ready
    }

    #[test]
    fn test_cycle_detection() {
        let mut graph = TaskGraph::new();
        let result = graph.add_tasks(vec![
            json!({"task_id": "a", "depends_on": ["b"]}),
            json!({"task_id": "b", "depends_on": ["a"]}),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn test_fail_dependents_cascade() {
        let mut graph = TaskGraph::new();
        graph.add_tasks(vec![
            json!({"task_id": "a"}),
            json!({"task_id": "b", "depends_on": ["a"]}),
            json!({"task_id": "c", "depends_on": ["b"]}),
        ]).unwrap();

        graph.mark_running("a");
        graph.mark_failed("a");
        graph.fail_dependents("a");

        assert_eq!(graph.task_status("b"), Some(&TaskStatus::Skipped));
        assert_eq!(graph.task_status("c"), Some(&TaskStatus::Skipped));
    }

    #[test]
    fn test_is_complete() {
        let mut graph = TaskGraph::new();
        graph.add_tasks(vec![
            json!({"task_id": "x"}),
            json!({"task_id": "y"}),
        ]).unwrap();
        assert!(!graph.is_complete());

        graph.mark_running("x");
        graph.mark_completed("x");
        assert!(!graph.is_complete());

        graph.mark_running("y");
        graph.mark_completed("y");
        assert!(graph.is_complete());
    }

    #[test]
    fn test_is_complete_with_failures() {
        let mut graph = TaskGraph::new();
        graph.add_tasks(vec![
            json!({"task_id": "ok"}),
            json!({"task_id": "fail"}),
        ]).unwrap();

        graph.mark_running("ok");
        graph.mark_completed("ok");
        graph.mark_running("fail");
        graph.mark_failed("fail");
        assert!(graph.is_complete());
    }

    #[test]
    fn test_backward_compat_no_deps() {
        // Tasks without depends_on become ready in insertion order
        let mut graph = TaskGraph::new();
        graph.add_tasks(vec![
            json!({"task_id": "t1", "prompt": "first"}),
            json!({"task_id": "t2", "prompt": "second"}),
            json!({"task_id": "t3", "prompt": "third"}),
        ]).unwrap();
        // All should be ready at once (no dependencies)
        assert_eq!(graph.ready_tasks().len(), 3);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p erlangrt task_graph`
Expected: FAIL with `not yet implemented`

**Step 3: Implement TaskGraph methods**

Replace all `todo!()` calls with working implementations:

- `add_tasks`: Parse `task_id` and `depends_on` from each JSON value. Insert into `tasks` HashMap. Call `detect_cycle()` -- if true, remove all just-added tasks and return Err. Set initial status: if `depends_on` is empty, `Ready`; otherwise `Pending`.
- `ready_tasks`: Filter tasks where `status == Ready`, return refs in `insertion_order`.
- `mark_running/completed/failed`: Set status on the task. On `mark_completed`, scan all Pending tasks and promote to Ready if all deps are Completed.
- `fail_dependents`: BFS from failed task_id. For each downstream task that transitively depends on it, set status to Skipped.
- `is_complete`: All tasks have status in {Completed, Failed, Skipped}.
- `detect_cycle`: Kahn's algorithm (topological sort). If sorted count < total tasks, cycle exists.

**Step 4: Register module in mod.rs**

Add to `lib-erlangrt/src/agent_rt/mod.rs`:
```rust
pub mod task_graph;
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p erlangrt task_graph`
Expected: ALL 8 PASS

**Step 6: Commit**

```bash
git add lib-erlangrt/src/agent_rt/task_graph.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(phase6): add TaskGraph DAG engine with cycle detection"
```

---

### Task 2: RetryPolicy -- Data Structure and Tests

**Files:**
- Create: `lib-erlangrt/src/agent_rt/retry_policy.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write the failing tests**

Create `lib-erlangrt/src/agent_rt/retry_policy.rs`:

```rust
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum RetryStrategy {
    None,
    Immediate { max_attempts: u32 },
    Backoff { max_attempts: u32, base_ms: u64, max_ms: u64 },
    Skip,
}

impl Default for RetryStrategy {
    fn default() -> Self {
        RetryStrategy::None
    }
}

pub struct RetryTracker {
    attempts: HashMap<String, u32>,
    default_strategy: RetryStrategy,
    per_task: HashMap<String, RetryStrategy>,
}

impl RetryTracker {
    pub fn new(default_strategy: RetryStrategy) -> Self {
        Self {
            attempts: HashMap::new(),
            default_strategy,
            per_task: HashMap::new(),
        }
    }

    /// Register a per-task retry override (parsed from task JSON).
    pub fn set_task_strategy(&mut self, task_id: &str, strategy: RetryStrategy) {
        self.per_task.insert(task_id.to_string(), strategy);
    }

    /// Check if a failed task should be retried.
    /// Returns Some(delay_ms) if retry, None if exhausted.
    pub fn should_retry(&mut self, task_id: &str) -> Option<u64> {
        todo!()
    }

    /// Parse retry strategy from task JSON "retry" field.
    pub fn parse_task_retry(task: &serde_json::Value) -> Option<RetryStrategy> {
        todo!()
    }

    pub fn attempt_count(&self, task_id: &str) -> u32 {
        self.attempts.get(task_id).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_retry_strategy() {
        let mut tracker = RetryTracker::new(RetryStrategy::None);
        assert!(tracker.should_retry("task-1").is_none());
    }

    #[test]
    fn test_immediate_retry() {
        let mut tracker = RetryTracker::new(RetryStrategy::Immediate { max_attempts: 3 });
        // First 3 calls should return Some(0) (no delay)
        assert_eq!(tracker.should_retry("t1"), Some(0));
        assert_eq!(tracker.should_retry("t1"), Some(0));
        assert_eq!(tracker.should_retry("t1"), Some(0));
        // 4th call: exhausted
        assert_eq!(tracker.should_retry("t1"), None);
    }

    #[test]
    fn test_backoff_retry() {
        let mut tracker = RetryTracker::new(RetryStrategy::Backoff {
            max_attempts: 3,
            base_ms: 100,
            max_ms: 1000,
        });
        // attempt 1: 100ms
        assert_eq!(tracker.should_retry("t1"), Some(100));
        // attempt 2: 200ms
        assert_eq!(tracker.should_retry("t1"), Some(200));
        // attempt 3: 400ms
        assert_eq!(tracker.should_retry("t1"), Some(400));
        // exhausted
        assert_eq!(tracker.should_retry("t1"), None);
    }

    #[test]
    fn test_backoff_capped_at_max() {
        let mut tracker = RetryTracker::new(RetryStrategy::Backoff {
            max_attempts: 5,
            base_ms: 500,
            max_ms: 1000,
        });
        assert_eq!(tracker.should_retry("t1"), Some(500));   // 500
        assert_eq!(tracker.should_retry("t1"), Some(1000));  // 1000 (capped)
        assert_eq!(tracker.should_retry("t1"), Some(1000));  // 1000 (capped)
    }

    #[test]
    fn test_skip_strategy() {
        let mut tracker = RetryTracker::new(RetryStrategy::Skip);
        // Skip always returns None (never retry, task goes to Skipped)
        assert!(tracker.should_retry("t1").is_none());
    }

    #[test]
    fn test_per_task_override() {
        let mut tracker = RetryTracker::new(RetryStrategy::None);
        tracker.set_task_strategy("special", RetryStrategy::Immediate { max_attempts: 2 });

        // Default strategy: no retry
        assert!(tracker.should_retry("normal").is_none());
        // Override: 2 retries
        assert_eq!(tracker.should_retry("special"), Some(0));
        assert_eq!(tracker.should_retry("special"), Some(0));
        assert_eq!(tracker.should_retry("special"), None);
    }

    #[test]
    fn test_parse_task_retry_backoff() {
        let task = serde_json::json!({
            "task_id": "t1",
            "retry": { "strategy": "backoff", "max_attempts": 3, "base_ms": 1000, "max_ms": 30000 }
        });
        let strategy = RetryTracker::parse_task_retry(&task).unwrap();
        match strategy {
            RetryStrategy::Backoff { max_attempts, base_ms, max_ms } => {
                assert_eq!(max_attempts, 3);
                assert_eq!(base_ms, 1000);
                assert_eq!(max_ms, 30000);
            }
            _ => panic!("expected Backoff"),
        }
    }

    #[test]
    fn test_parse_task_retry_none() {
        let task = serde_json::json!({"task_id": "t1"});
        assert!(RetryTracker::parse_task_retry(&task).is_none());
    }

    #[test]
    fn test_independent_task_tracking() {
        let mut tracker = RetryTracker::new(RetryStrategy::Immediate { max_attempts: 2 });
        assert_eq!(tracker.should_retry("a"), Some(0));
        assert_eq!(tracker.should_retry("b"), Some(0));
        // Each task has independent attempt count
        assert_eq!(tracker.attempt_count("a"), 1);
        assert_eq!(tracker.attempt_count("b"), 1);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p erlangrt retry_policy`
Expected: FAIL with `not yet implemented`

**Step 3: Implement RetryTracker methods**

- `should_retry`: Look up strategy (per_task overrides default). Match on strategy variant. Increment `attempts[task_id]`. If attempts <= max_attempts, return delay. For Backoff: `delay = min(base_ms * 2^(attempt-1), max_ms)`. For None/Skip: return None.
- `parse_task_retry`: Check for `task["retry"]["strategy"]`. Parse "none", "immediate", "backoff", "skip". Extract numeric fields with defaults.

**Step 4: Register module**

Add `pub mod retry_policy;` to `mod.rs`.

**Step 5: Run tests**

Run: `cargo test -p erlangrt retry_policy`
Expected: ALL 9 PASS

**Step 6: Commit**

```bash
git add lib-erlangrt/src/agent_rt/retry_policy.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(phase6): add RetryTracker with per-task strategy overrides"
```

---

### Task 3: ResourceBudget -- Data Structure and Tests

**Files:**
- Create: `lib-erlangrt/src/agent_rt/resource_budget.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write the failing tests**

Create `lib-erlangrt/src/agent_rt/resource_budget.rs`:

```rust
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ModelPrice {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
}

#[derive(Debug, Clone, Default)]
pub struct TokenPricing {
    pub model_prices: HashMap<String, ModelPrice>,
}

#[derive(Debug)]
pub struct ResourceBudget {
    max_tokens: Option<u64>,
    max_cost_usd: Option<f64>,
    tokens_used: u64,
    cost_usd: f64,
    pricing: TokenPricing,
}

#[derive(Debug, Clone)]
pub struct UsageReport {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: Option<String>,
}

impl ResourceBudget {
    pub fn new(
        max_tokens: Option<u64>,
        max_cost_usd: Option<f64>,
        pricing: TokenPricing,
    ) -> Self {
        Self {
            max_tokens,
            max_cost_usd,
            tokens_used: 0,
            cost_usd: 0.0,
            pricing,
        }
    }

    pub fn unlimited() -> Self {
        Self::new(None, None, TokenPricing::default())
    }

    /// Record token usage from a worker result. Returns true if budget is now exhausted.
    pub fn record_usage(&mut self, usage: &UsageReport) -> bool {
        todo!()
    }

    pub fn is_exhausted(&self) -> bool {
        todo!()
    }

    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    pub fn cost_usd(&self) -> f64 {
        self.cost_usd
    }

    pub fn snapshot(&self) -> serde_json::Value {
        todo!()
    }

    /// Parse usage from a worker_result JSON payload.
    pub fn parse_usage(payload: &serde_json::Value) -> Option<UsageReport> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unlimited_budget_never_exhausted() {
        let mut budget = ResourceBudget::unlimited();
        let exhausted = budget.record_usage(&UsageReport {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            model: None,
        });
        assert!(!exhausted);
        assert!(!budget.is_exhausted());
        assert_eq!(budget.tokens_used(), 1_500_000);
    }

    #[test]
    fn test_token_budget_exhaustion() {
        let mut budget = ResourceBudget::new(Some(1000), None, TokenPricing::default());
        budget.record_usage(&UsageReport { input_tokens: 400, output_tokens: 300, model: None });
        assert!(!budget.is_exhausted());
        assert_eq!(budget.tokens_used(), 700);

        budget.record_usage(&UsageReport { input_tokens: 200, output_tokens: 200, model: None });
        assert!(budget.is_exhausted());
        assert_eq!(budget.tokens_used(), 1100);
    }

    #[test]
    fn test_cost_budget_exhaustion() {
        let mut pricing = TokenPricing::default();
        pricing.model_prices.insert("gpt-4o".into(), ModelPrice {
            input_per_1k: 0.0025,
            output_per_1k: 0.01,
        });
        let mut budget = ResourceBudget::new(None, Some(1.0), pricing);

        // 10k input ($0.025) + 5k output ($0.05) = $0.075
        budget.record_usage(&UsageReport {
            input_tokens: 10_000, output_tokens: 5_000, model: Some("gpt-4o".into()),
        });
        assert!(!budget.is_exhausted());
        assert!(budget.cost_usd() > 0.07);

        // 100k input ($0.25) + 100k output ($1.00) = $1.25 total > $1.00
        budget.record_usage(&UsageReport {
            input_tokens: 100_000, output_tokens: 100_000, model: Some("gpt-4o".into()),
        });
        assert!(budget.is_exhausted());
    }

    #[test]
    fn test_unknown_model_no_cost() {
        let mut budget = ResourceBudget::new(None, Some(1.0), TokenPricing::default());
        budget.record_usage(&UsageReport {
            input_tokens: 1_000_000, output_tokens: 1_000_000, model: Some("unknown".into()),
        });
        // No pricing for "unknown" model, cost stays 0
        assert_eq!(budget.cost_usd(), 0.0);
        assert!(!budget.is_exhausted());
    }

    #[test]
    fn test_snapshot() {
        let mut budget = ResourceBudget::new(Some(10000), Some(5.0), TokenPricing::default());
        budget.record_usage(&UsageReport { input_tokens: 100, output_tokens: 50, model: None });
        let snap = budget.snapshot();
        assert_eq!(snap["tokens_used"], 150);
        assert_eq!(snap["max_tokens"], 10000);
    }

    #[test]
    fn test_parse_usage_from_worker_result() {
        let payload = serde_json::json!({
            "type": "worker_result",
            "usage": { "input_tokens": 500, "output_tokens": 200, "model": "gpt-4o" }
        });
        let usage = ResourceBudget::parse_usage(&payload).unwrap();
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(usage.output_tokens, 200);
        assert_eq!(usage.model, Some("gpt-4o".into()));
    }

    #[test]
    fn test_parse_usage_missing() {
        let payload = serde_json::json!({"type": "worker_result"});
        assert!(ResourceBudget::parse_usage(&payload).is_none());
    }

    #[test]
    fn test_record_returns_true_on_exhaustion() {
        let mut budget = ResourceBudget::new(Some(100), None, TokenPricing::default());
        let exhausted = budget.record_usage(&UsageReport {
            input_tokens: 200, output_tokens: 0, model: None,
        });
        assert!(exhausted);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p erlangrt resource_budget`
Expected: FAIL with `not yet implemented`

**Step 3: Implement ResourceBudget methods**

- `record_usage`: Add input+output to `tokens_used`. If model is in `pricing.model_prices`, calculate cost and add to `cost_usd`. Check exhaustion. Return `is_exhausted()`.
- `is_exhausted`: True if (`max_tokens.is_some()` AND `tokens_used >= max_tokens`) OR (`max_cost_usd.is_some()` AND `cost_usd >= max_cost_usd`).
- `snapshot`: Return JSON with `tokens_used`, `max_tokens`, `cost_usd`, `max_cost_usd`.
- `parse_usage`: Extract `payload["usage"]["input_tokens"]`, `output_tokens`, `model`.

**Step 4: Register module**

Add `pub mod resource_budget;` to `mod.rs`.

**Step 5: Run tests**

Run: `cargo test -p erlangrt resource_budget`
Expected: ALL 8 PASS

**Step 6: Commit**

```bash
git add lib-erlangrt/src/agent_rt/resource_budget.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(phase6): add ResourceBudget with token/cost tracking"
```

---

### Task 4: ResultAggregator -- Trait and Built-in Strategies

**Files:**
- Create: `lib-erlangrt/src/agent_rt/result_aggregator.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write the failing tests**

Create `lib-erlangrt/src/agent_rt/result_aggregator.rs`:

```rust
pub trait ResultAggregator: Send + Sync {
    fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value;
}

/// Returns results array as-is (default, backward-compatible).
pub struct ConcatAggregator;

/// Majority vote on a specified field.
pub struct VoteAggregator {
    pub field: String,
}

/// Deep-merge JSON objects; arrays concatenated.
pub struct MergeAggregator;

impl ResultAggregator for ConcatAggregator {
    fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value {
        todo!()
    }
}

impl ResultAggregator for VoteAggregator {
    fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value {
        todo!()
    }
}

impl ResultAggregator for MergeAggregator {
    fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value {
        todo!()
    }
}

/// Build an aggregator from a strategy name.
pub fn build_aggregator(strategy: &str) -> Box<dyn ResultAggregator> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_concat_aggregator() {
        let agg = ConcatAggregator;
        let results = vec![json!({"a": 1}), json!({"b": 2})];
        let out = agg.aggregate(&results);
        assert_eq!(out, json!([{"a": 1}, {"b": 2}]));
    }

    #[test]
    fn test_concat_empty() {
        let agg = ConcatAggregator;
        assert_eq!(agg.aggregate(&[]), json!([]));
    }

    #[test]
    fn test_vote_aggregator() {
        let agg = VoteAggregator { field: "decision".into() };
        let results = vec![
            json!({"decision": "approve"}),
            json!({"decision": "reject"}),
            json!({"decision": "approve"}),
        ];
        let out = agg.aggregate(&results);
        assert_eq!(out["winner"], "approve");
        assert_eq!(out["votes"]["approve"], 2);
        assert_eq!(out["votes"]["reject"], 1);
    }

    #[test]
    fn test_merge_aggregator() {
        let agg = MergeAggregator;
        let results = vec![
            json!({"name": "alice", "tags": ["fast"]}),
            json!({"age": 30, "tags": ["smart"]}),
        ];
        let out = agg.aggregate(&results);
        assert_eq!(out["name"], "alice");
        assert_eq!(out["age"], 30);
        assert_eq!(out["tags"], json!(["fast", "smart"]));
    }

    #[test]
    fn test_build_aggregator_concat() {
        let agg = build_aggregator("concat");
        let out = agg.aggregate(&[json!(1), json!(2)]);
        assert_eq!(out, json!([1, 2]));
    }

    #[test]
    fn test_build_aggregator_unknown_defaults_concat() {
        let agg = build_aggregator("nonexistent");
        let out = agg.aggregate(&[json!(1)]);
        assert_eq!(out, json!([1]));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p erlangrt result_aggregator`
Expected: FAIL with `not yet implemented`

**Step 3: Implement aggregators**

- `ConcatAggregator::aggregate`: Return `serde_json::Value::Array(results.to_vec())`.
- `VoteAggregator::aggregate`: Count occurrences of each value at `result[field]`. Return `{"winner": most_common, "votes": {value: count, ...}}`.
- `MergeAggregator::aggregate`: Start with `{}`. For each result, merge keys. If both have same key and both are arrays, concatenate. Otherwise later value wins.
- `build_aggregator`: Match on "concat", "vote", "merge". Default to ConcatAggregator. VoteAggregator uses `field: "decision"` as default.

**Step 4: Register module**

Add `pub mod result_aggregator;` to `mod.rs`.

**Step 5: Run tests**

Run: `cargo test -p erlangrt result_aggregator`
Expected: ALL 6 PASS

**Step 6: Commit**

```bash
git add lib-erlangrt/src/agent_rt/result_aggregator.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(phase6): add ResultAggregator trait with concat/vote/merge strategies"
```

---

### Task 5: ApprovalGate -- Registry and Tests

**Files:**
- Create: `lib-erlangrt/src/agent_rt/approval_gate.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`

**Step 1: Write the failing tests**

Create `lib-erlangrt/src/agent_rt/approval_gate.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub message: String,
    pub task: serde_json::Value,
    pub requested_at: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    Approved,
    Rejected { reason: String },
}

pub struct ApprovalRegistry {
    pending: HashMap<(String, String), ApprovalRequest>,
    decisions: HashMap<(String, String), ApprovalDecision>,
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            decisions: HashMap::new(),
        }
    }

    /// Register a task that needs approval.
    pub fn request_approval(
        &mut self,
        orch_id: &str,
        task_id: &str,
        message: &str,
        task: serde_json::Value,
    ) {
        let key = (orch_id.to_string(), task_id.to_string());
        self.pending.insert(key, ApprovalRequest {
            message: message.to_string(),
            task,
            requested_at: std::time::Instant::now(),
        });
    }

    /// Submit an approval or rejection decision.
    pub fn submit_decision(
        &mut self,
        orch_id: &str,
        task_id: &str,
        decision: ApprovalDecision,
    ) -> bool {
        let key = (orch_id.to_string(), task_id.to_string());
        if self.pending.remove(&key).is_some() {
            self.decisions.insert(key, decision);
            true
        } else {
            false
        }
    }

    /// Check if a decision has been made for a task.
    pub fn take_decision(
        &mut self,
        orch_id: &str,
        task_id: &str,
    ) -> Option<ApprovalDecision> {
        let key = (orch_id.to_string(), task_id.to_string());
        self.decisions.remove(&key)
    }

    /// List all pending approval requests.
    pub fn pending_requests(&self) -> Vec<(&str, &str, &ApprovalRequest)> {
        self.pending
            .iter()
            .map(|((oid, tid), req)| (oid.as_str(), tid.as_str(), req))
            .collect()
    }

    pub fn has_pending(&self, orch_id: &str, task_id: &str) -> bool {
        self.pending.contains_key(&(orch_id.to_string(), task_id.to_string()))
    }
}

pub type SharedApprovalRegistry = Arc<Mutex<ApprovalRegistry>>;

pub fn new_shared_registry() -> SharedApprovalRegistry {
    Arc::new(Mutex::new(ApprovalRegistry::new()))
}

/// Check if a task requires approval (has "requires_approval": true).
pub fn task_requires_approval(task: &serde_json::Value) -> bool {
    task.get("requires_approval")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Extract approval message from task, with fallback.
pub fn approval_message(task: &serde_json::Value) -> String {
    task.get("approval_message")
        .and_then(|v| v.as_str())
        .unwrap_or("Approval required")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_request_and_approve() {
        let mut registry = ApprovalRegistry::new();
        registry.request_approval("orch-1", "task-a", "Deploy?", json!({"task_id": "task-a"}));
        assert!(registry.has_pending("orch-1", "task-a"));
        assert_eq!(registry.pending_requests().len(), 1);

        registry.submit_decision("orch-1", "task-a", ApprovalDecision::Approved);
        assert!(!registry.has_pending("orch-1", "task-a"));

        let decision = registry.take_decision("orch-1", "task-a");
        assert_eq!(decision, Some(ApprovalDecision::Approved));

        // Decision consumed
        assert!(registry.take_decision("orch-1", "task-a").is_none());
    }

    #[test]
    fn test_reject() {
        let mut registry = ApprovalRegistry::new();
        registry.request_approval("o1", "t1", "OK?", json!({}));
        registry.submit_decision("o1", "t1", ApprovalDecision::Rejected {
            reason: "not ready".into(),
        });
        let decision = registry.take_decision("o1", "t1").unwrap();
        assert_eq!(decision, ApprovalDecision::Rejected { reason: "not ready".into() });
    }

    #[test]
    fn test_submit_unknown_task_returns_false() {
        let mut registry = ApprovalRegistry::new();
        let ok = registry.submit_decision("o1", "unknown", ApprovalDecision::Approved);
        assert!(!ok);
    }

    #[test]
    fn test_task_requires_approval() {
        assert!(task_requires_approval(&json!({"requires_approval": true})));
        assert!(!task_requires_approval(&json!({"requires_approval": false})));
        assert!(!task_requires_approval(&json!({"task_id": "t1"})));
    }

    #[test]
    fn test_approval_message_fallback() {
        assert_eq!(
            approval_message(&json!({"approval_message": "Ship it?"})),
            "Ship it?"
        );
        assert_eq!(approval_message(&json!({})), "Approval required");
    }

    #[test]
    fn test_shared_registry_thread_safety() {
        let registry = new_shared_registry();
        let r2 = registry.clone();

        // Simulate two threads accessing
        {
            let mut reg = registry.lock().unwrap();
            reg.request_approval("o", "t", "msg", json!({}));
        }
        {
            let reg = r2.lock().unwrap();
            assert!(reg.has_pending("o", "t"));
        }
    }
}
```

**Step 2: Run tests to verify they pass** (this module has no `todo!()` -- fully implemented inline)

Run: `cargo test -p erlangrt approval_gate`
Expected: ALL 6 PASS

**Step 3: Register module**

Add `pub mod approval_gate;` to `mod.rs`.

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/approval_gate.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(phase6): add ApprovalRegistry for human-in-the-loop gates"
```

---

### Task 6: Approval HTTP Endpoints

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/server.rs`

**Step 1: Read existing server.rs**

Read `lib-erlangrt/src/agent_rt/server.rs` to understand current structure (already shown above).

**Step 2: Add approval endpoints**

Add to `server.rs`:

```rust
use axum::{extract::Path, routing::post};
use crate::agent_rt::approval_gate::{ApprovalDecision, SharedApprovalRegistry};

// Update ServerState to include optional approval registry:
#[derive(Clone)]
pub struct ServerState {
    pub metrics: Arc<RuntimeMetrics>,
    pub process_count: Arc<std::sync::atomic::AtomicUsize>,
    pub approval_registry: Option<SharedApprovalRegistry>,
}

// Update Router in HealthServer::start to add routes:
let app = Router::new()
    .route("/health", get(health_handler))
    .route("/metrics", get(metrics_handler))
    .route("/approvals", get(list_approvals_handler))
    .route("/approve/:orch_id/:task_id", post(approve_handler))
    .with_state(state);

// New handlers:
async fn list_approvals_handler(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let registry = match &state.approval_registry {
        Some(r) => r.lock().unwrap(),
        None => return Json(serde_json::json!({"pending": []})),
    };
    let pending: Vec<serde_json::Value> = registry
        .pending_requests()
        .iter()
        .map(|(orch_id, task_id, req)| {
            serde_json::json!({
                "orch_id": orch_id,
                "task_id": task_id,
                "message": req.message,
                "task": req.task,
            })
        })
        .collect();
    Json(serde_json::json!({"pending": pending}))
}

async fn approve_handler(
    State(state): State<ServerState>,
    Path((orch_id, task_id)): Path<(String, String)>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let registry = match &state.approval_registry {
        Some(r) => r,
        None => return Json(serde_json::json!({"error": "approval registry not configured"})),
    };
    let approved = body.get("approved").and_then(|v| v.as_bool()).unwrap_or(false);
    let decision = if approved {
        ApprovalDecision::Approved
    } else {
        let reason = body.get("reason").and_then(|v| v.as_str()).unwrap_or("rejected").to_string();
        ApprovalDecision::Rejected { reason }
    };
    let mut reg = registry.lock().unwrap();
    let ok = reg.submit_decision(&orch_id, &task_id, decision);
    Json(serde_json::json!({"ok": ok}))
}
```

**Step 3: Update existing test to pass new field**

Update `test_health_endpoint` to include `approval_registry: None` in `ServerState`.

**Step 4: Run tests**

Run: `cargo test -p erlangrt server`
Expected: PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/server.rs
git commit -m "feat(phase6): add approval HTTP endpoints to health server"
```

---

### Task 7: Orchestration Config Section

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/config.rs`

**Step 1: Write failing test**

Add to `config.rs` tests:
```rust
#[test]
fn test_orchestration_config_defaults() {
    let config = AppConfig::default();
    assert_eq!(config.orchestration.max_concurrency, 4);
    assert_eq!(config.orchestration.max_orchestration_depth, 3);
    assert_eq!(config.orchestration.default_retry, "none");
    assert_eq!(config.orchestration.budget.max_tokens, 0);
    assert_eq!(config.orchestration.aggregator.strategy, "concat");
}

#[test]
fn test_orchestration_config_from_toml() {
    let toml_str = r#"
[orchestration]
max_concurrency = 8
default_retry = "backoff:3:1000:30000"

[orchestration.budget]
max_tokens = 100000
max_cost_usd = 5.0

[orchestration.aggregator]
strategy = "merge"
"#;
    let config = load_config_from_str(toml_str).unwrap();
    assert_eq!(config.orchestration.max_concurrency, 8);
    assert_eq!(config.orchestration.budget.max_tokens, 100000);
    assert_eq!(config.orchestration.budget.max_cost_usd, 5.0);
    assert_eq!(config.orchestration.aggregator.strategy, "merge");
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p erlangrt test_orchestration_config`
Expected: FAIL (field `orchestration` not found on `AppConfig`)

**Step 3: Add OrchestrationConfig structs**

Add to `config.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    pub max_concurrency: usize,
    pub max_orchestration_depth: usize,
    pub default_retry: String,
    pub budget: BudgetConfig,
    pub aggregator: AggregatorConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub max_tokens: u64,
    pub max_cost_usd: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AggregatorConfig {
    pub strategy: String,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_orchestration_depth: 3,
            default_retry: "none".into(),
            budget: BudgetConfig::default(),
            aggregator: AggregatorConfig::default(),
        }
    }
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self { max_tokens: 0, max_cost_usd: 0.0 }
    }
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        Self { strategy: "concat".into() }
    }
}
```

Add `pub orchestration: OrchestrationConfig` to `AppConfig` struct and its `Default` impl.

**Step 4: Run tests**

Run: `cargo test -p erlangrt config`
Expected: ALL PASS (including new + existing)

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/config.rs
git commit -m "feat(phase6): add OrchestrationConfig to TOML config"
```

---

### Task 8: Wire TaskGraph into OrchestratorBehavior

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs`

This is the largest task -- it replaces the flat `VecDeque<Value>` task queue with `TaskGraph`, and wires in `RetryTracker`, `ResourceBudget`, and `ResultAggregator`.

**Step 1: Update OrchestratorBehavior struct**

Add new optional fields:
```rust
pub struct OrchestratorBehavior {
    pub max_concurrency: usize,
    pub checkpoint_store: Option<Arc<dyn CheckpointStore>>,
    pub retry_strategy: RetryStrategy,           // NEW
    pub budget: Option<ResourceBudget>,           // NEW (moved to state on init)
    pub aggregator: Option<Arc<dyn ResultAggregator>>,  // NEW
    pub approval_registry: Option<SharedApprovalRegistry>, // NEW
    pub max_depth: usize,                         // NEW
    pub orch_id: String,                          // NEW (for approval key)
    pub current_depth: usize,                     // NEW (for hierarchical)
}
```

**Step 2: Update OrchestratorState**

Replace `pending_tasks: VecDeque<Value>` with `task_graph: TaskGraph`. Add:
```rust
pub struct OrchestratorState {
    task_graph: TaskGraph,                              // REPLACE VecDeque
    pending_spawn_tasks: VecDeque<serde_json::Value>,   // keep
    active_workers: HashMap<u64, String>,               // keep
    worker_monitors: HashMap<u64, u64>,                 // keep
    inflight_tasks: HashMap<String, serde_json::Value>,  // keep
    results: Vec<serde_json::Value>,                    // keep
    partial_results: HashMap<String, Vec<serde_json::Value>>, // NEW
    requester: Option<AgentPid>,
    goal: serde_json::Value,
    self_pid: Option<AgentPid>,
    checkpoint_key: String,
    resumed_from_checkpoint: bool,
    awaiting_decomposition: bool,
    decomposition_done: bool,
    retry_tracker: RetryTracker,                        // NEW
    budget: ResourceBudget,                             // NEW
}
```

**Step 3: Update handle_message for TaskGraph**

Key changes to `handle_message`:
- After `extract_tasks()`, call `state.task_graph.add_tasks(tasks)` instead of pushing to VecDeque
- In `next_or_finalize()`: call `task_graph.ready_tasks()` instead of `pending_tasks.pop_front()`
- On worker DOWN/result: call `task_graph.mark_completed(task_id)` or `mark_failed(task_id)` + `fail_dependents(task_id)`
- Check `task_graph.is_complete()` instead of the old `is_complete()` logic
- On `worker_result`: parse usage with `ResourceBudget::parse_usage()`, record, check exhaustion

**Step 4: Update next_or_finalize for approval gates**

Before spawning a ready task, check `task_requires_approval()`. If true and `approval_registry` is set:
- Set task to `AwaitingApproval` in graph
- Register in approval registry
- Return `Action::Continue` (don't spawn yet)

Each message cycle, check registry for decisions on awaiting tasks.

**Step 5: Update finalization for aggregation**

When `task_graph.is_complete()`, run `aggregator.aggregate(&state.results)` instead of returning raw results array.

**Step 6: Run existing orchestration tests**

Run: `cargo test -p erlangrt orchestration`
Expected: ALL existing tests PASS (backward compatibility -- no DAG deps, no budget, no approval)

**Step 7: Add new orchestration tests for TaskGraph integration**

Add tests to `orchestration.rs` `#[cfg(test)]` block:

```rust
#[test]
fn test_orchestrator_dag_respects_dependencies() {
    // Decompose returns tasks with depends_on
    // Verify that only tasks with met deps get spawned
}

#[test]
fn test_orchestrator_retry_respawns_failed_worker() {
    // Configure retry strategy, simulate worker DOWN
    // Verify task goes back to Ready and gets re-spawned
}

#[test]
fn test_orchestrator_budget_stops_spawning_when_exhausted() {
    // Set max_tokens budget, simulate worker results with usage
    // Verify orchestrator stops spawning after budget exceeded
}
```

**Step 8: Run all tests**

Run: `cargo test -p erlangrt`
Expected: ALL PASS

**Step 9: Commit**

```bash
git add lib-erlangrt/src/agent_rt/orchestration.rs
git commit -m "feat(phase6): wire TaskGraph, retry, budget, approval into orchestrator"
```

---

### Task 9: Streaming Results -- Worker Progress Messages

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs`

**Step 1: Write failing test**

Add to orchestration tests:
```rust
#[test]
fn test_worker_sends_progress_and_orchestrator_stores_it() {
    // Create orchestrator with task graph
    // Simulate worker sending worker_progress message
    // Verify partial_results stored in state
}
```

**Step 2: Add worker_progress handling to OrchestratorBehavior::handle_message**

In the `Message::Json(payload)` match arm, add a check for `"type": "worker_progress"`:
```rust
if payload.get("type").and_then(|v| v.as_str()) == Some("worker_progress") {
    if let Some(task_id) = payload.get("task_id").and_then(|v| v.as_str()) {
        s.partial_results
            .entry(task_id.to_string())
            .or_default()
            .push(payload.clone());
    }
    // Forward to requester if present
    if let Some(requester) = s.requester {
        return Action::Send {
            to: requester,
            msg: Message::Json(payload),
        };
    }
    return Action::Continue;
}
```

**Step 3: Update WorkerBehavior to send progress on multi-turn**

In `WorkerBehavior::handle_message`, when handling `IoResponse` and `turn_count < max_turns`, send progress before the result:
```rust
// After incrementing turn_count, before sending worker_result:
// If this isn't the final turn, send progress first
// (worker_result already goes to parent, this adds a progress message)
```

Actually, the simplest approach: the worker already sends `worker_result` per turn. The orchestrator just needs to distinguish intermediate results. Add a `"final": false` field to non-final results, or handle `worker_progress` as a separate message type that the worker sends explicitly. For minimal changes, just have the orchestrator store all incoming `worker_result` messages in `partial_results` before processing them as final. No worker changes needed.

**Step 4: Run tests**

Run: `cargo test -p erlangrt orchestration`
Expected: ALL PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/orchestration.rs
git commit -m "feat(phase6): add worker_progress handling for streaming results"
```

---

### Task 10: Hierarchical Orchestrators -- Sub-Orchestration Spawning

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs`

**Step 1: Write failing test**

```rust
#[test]
fn test_sub_orchestration_task_spawns_orchestrator_not_worker() {
    // Create orchestrator with max_depth > 0
    // Give it a task with "type": "sub_orchestration"
    // Verify it spawns OrchestratorBehavior (check behavior type)
}
```

**Step 2: Modify next_or_finalize to check task type**

In `next_or_finalize`, after popping a ready task, check if `task["type"] == "sub_orchestration"`:
```rust
if task.get("type").and_then(|v| v.as_str()) == Some("sub_orchestration") {
    if self.current_depth >= self.max_depth {
        // Depth limit reached, skip this task
        state.task_graph.mark_failed(&task_id);
        state.results.push(json!({
            "type": "sub_orchestration_depth_exceeded",
            "task_id": task_id,
        }));
        return self.next_or_finalize(state);
    }

    let sub_goal = task.get("goal").cloned().unwrap_or(json!(null));
    let sub_concurrency = task.get("max_concurrency")
        .and_then(|v| v.as_u64())
        .unwrap_or(self.max_concurrency as u64) as usize;

    let sub_orch = OrchestratorBehavior {
        max_concurrency: sub_concurrency,
        checkpoint_store: self.checkpoint_store.clone(),
        current_depth: self.current_depth + 1,
        max_depth: self.max_depth,
        // ... other fields
    };

    return Action::Spawn {
        behavior: Arc::new(sub_orch),
        args: json!({
            "goal": sub_goal,
            "parent_pid": state.self_pid.map(|p| p.raw()),
        }),
        link: false,
        monitor: true,
    };
}
```

**Step 3: Run tests**

Run: `cargo test -p erlangrt orchestration`
Expected: ALL PASS

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/orchestration.rs
git commit -m "feat(phase6): add hierarchical sub-orchestration spawning"
```

---

### Task 11: Integration Tests -- End-to-End Scenarios

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/integration_tests.rs`

**Step 1: Write DAG integration test**

```rust
#[test]
fn test_dag_orchestration_end_to_end() {
    // 1. Create orchestrator with TaskGraph support
    // 2. Feed it decomposed tasks with depends_on
    // 3. Tick through scheduler
    // 4. Verify tasks execute in correct dependency order
    // 5. Verify orchestration completes
}
```

**Step 2: Write retry integration test**

```rust
#[test]
fn test_retry_policy_end_to_end() {
    // 1. Create orchestrator with Immediate { max_attempts: 2 }
    // 2. Worker crashes (DOWN with Custom reason)
    // 3. Verify worker is re-spawned
    // 4. Worker crashes again
    // 5. Verify task is marked Failed (exhausted)
}
```

**Step 3: Write budget integration test**

```rust
#[test]
fn test_budget_stops_orchestration() {
    // 1. Create orchestrator with max_tokens: 100
    // 2. Worker result includes usage: 200 tokens
    // 3. Verify orchestrator stops spawning new tasks
    // 4. Verify completion includes budget_exhausted status
}
```

**Step 4: Run integration tests**

Run: `cargo test -p erlangrt integration_tests`
Expected: ALL PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/integration_tests.rs
git commit -m "test(phase6): end-to-end integration tests for DAG, retry, budget"
```

---

### Task 12: Update ROADMAP.md

**Files:**
- Modify: `docs/ROADMAP.md`

**Step 1: Update Phase 6 status**

Change Phase 6 from `[PLANNED]` to `[DONE]` and add implementation details matching the Phase 4/5 format:

```markdown
## Phase 6: Advanced Orchestration [DONE]

Sophisticated multi-agent coordination patterns.

- **DAG task dependencies**: TaskGraph with cycle detection, dependency resolution, cascade failure
- **Retry policies**: RetryStrategy enum (None/Immediate/Backoff/Skip), per-task overrides via task JSON
- **Resource budgets**: Token count + cost tracking per orchestration, configurable per-model pricing
- **Approval gates**: ApprovalRegistry with HTTP endpoints (POST /approve, GET /approvals), AwaitingApproval status
- **Streaming results**: worker_progress messages, partial_results storage, forwarding to parent orchestrators
- **Hierarchical orchestrators**: sub_orchestration task type, recursive OrchestratorBehavior spawning, depth limit
- **Result aggregation**: ResultAggregator trait with ConcatAggregator, VoteAggregator, MergeAggregator

**12 tasks, ~XX tests:** `<first>` through `<last>`
```

Update the Phase Summary table.

**Step 2: Commit**

```bash
git add docs/ROADMAP.md
git commit -m "docs: update ROADMAP.md -- Phase 6 complete"
```

---

## Task Summary

| Task | Component | New Tests |
|------|-----------|-----------|
| 1 | TaskGraph DAG engine | 8 |
| 2 | RetryTracker | 9 |
| 3 | ResourceBudget | 8 |
| 4 | ResultAggregator trait + strategies | 6 |
| 5 | ApprovalGate registry | 6 |
| 6 | Approval HTTP endpoints | 1 (update existing) |
| 7 | OrchestrationConfig | 2 |
| 8 | Wire into OrchestratorBehavior | 3+ (backward compat) |
| 9 | Streaming results | 1 |
| 10 | Hierarchical orchestrators | 1 |
| 11 | Integration tests | 3 |
| 12 | Update ROADMAP.md | -- |

**Total: 12 tasks, ~48 new tests**
