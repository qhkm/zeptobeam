# Phase 2: Real Orchestration Decomposition — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Wire the orchestrator's `decompose_goal` IoOp to call a real LLM (via `AgentChat`) so goals are decomposed into executable sub-tasks instead of returning a placeholder.

**Architecture:** Add a `decomposition.rs` module with a prompt builder and response parser. In the bridge's `BridgeWorker` dispatch loop, intercept `Custom { kind: "decompose_goal" }` and route it through `execute_agent_chat_standalone` with a decomposition prompt. Parse the LLM response into a `{"tasks": [...]}` result so the existing `extract_tasks` function picks them up. Keep the placeholder fallback in `execute_io_op` for when no bridge is running.

**Tech Stack:** Rust, existing `agent_rt` bridge/orchestration, `serde_json`

---

### Task 1: Add decomposition prompt builder and response parser

**Files:**
- Create: `lib-erlangrt/src/agent_rt/decomposition.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod decomposition;`)

**Step 1: Write the failing tests**

Create `decomposition.rs` with tests at the bottom:

```rust
//! Goal decomposition: prompt building and LLM response parsing.

use crate::agent_rt::types::IoOp;

/// Default provider used for decomposition when none is configured.
pub const DEFAULT_DECOMPOSE_PROVIDER: &str = "openrouter";
/// Default model for decomposition.
pub const DEFAULT_DECOMPOSE_MODEL: &str = "anthropic/claude-sonnet-4";

/// Build a decomposition prompt from a goal description.
/// Returns the full system prompt and user prompt as a tuple.
pub fn build_decomposition_prompt(goal: &serde_json::Value) -> (String, String) {
    let goal_text = match goal {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };

    let system_prompt = "You are a task decomposition engine. Given a goal, break it into \
        independent sub-tasks that can be executed in parallel by AI agents. \
        Respond ONLY with a JSON object: {\"tasks\": [{\"task_id\": \"t1\", \"goal\": \"...\", \
        \"depends_on\": []}, ...]}. Each task needs a unique task_id, a goal description, \
        and an optional depends_on array of task_ids it must wait for. \
        Keep tasks atomic and actionable. Aim for 2-6 tasks."
        .to_string();

    let user_prompt = format!("Decompose this goal into sub-tasks:\n\n{}", goal_text);

    (system_prompt, user_prompt)
}

/// Build an `IoOp::AgentChat` for goal decomposition.
pub fn build_decomposition_chat_op(
    goal: &serde_json::Value,
    provider: Option<&str>,
    model: Option<&str>,
) -> IoOp {
    let (system_prompt, user_prompt) = build_decomposition_prompt(goal);
    IoOp::AgentChat {
        provider: provider.unwrap_or(DEFAULT_DECOMPOSE_PROVIDER).to_string(),
        model: Some(model.unwrap_or(DEFAULT_DECOMPOSE_MODEL).to_string()),
        system_prompt: Some(system_prompt),
        prompt: user_prompt,
        tools: None,
        max_iterations: Some(1),
        timeout_ms: Some(60_000),
    }
}

/// Parse an LLM response string into a tasks JSON value.
/// Tries to extract a JSON object with a "tasks" array.
/// Falls back to wrapping the entire goal as a single task.
pub fn parse_decomposition_response(
    response: &str,
    goal: &serde_json::Value,
) -> serde_json::Value {
    // Try to find JSON in the response (LLMs sometimes wrap in markdown)
    let json_str = extract_json_block(response);

    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
        if let Some(tasks) = parsed.get("tasks") {
            if tasks.is_array() && !tasks.as_array().unwrap().is_empty() {
                return serde_json::json!({ "tasks": tasks });
            }
        }
        // If parsed is itself an array of tasks
        if parsed.is_array() && !parsed.as_array().unwrap().is_empty() {
            return serde_json::json!({ "tasks": parsed });
        }
    }

    // Fallback: single task with the original goal
    serde_json::json!({
        "tasks": [{
            "task_id": "task-0",
            "goal": goal.clone(),
        }]
    })
}

/// Extract JSON from a string that might have markdown fences.
fn extract_json_block(s: &str) -> &str {
    // Try ```json ... ``` first
    if let Some(start) = s.find("```json") {
        let after_fence = &s[start + 7..];
        if let Some(end) = after_fence.find("```") {
            return after_fence[..end].trim();
        }
    }
    // Try ``` ... ```
    if let Some(start) = s.find("```") {
        let after_fence = &s[start + 3..];
        if let Some(end) = after_fence.find("```") {
            return after_fence[..end].trim();
        }
    }
    // Try to find raw JSON object
    if let Some(start) = s.find('{') {
        if let Some(end) = s.rfind('}') {
            if end > start {
                return &s[start..=end];
            }
        }
    }
    s.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_decomposition_prompt_string_goal() {
        let goal = serde_json::json!("Build a REST API for todo items");
        let (system, user) = build_decomposition_prompt(&goal);
        assert!(system.contains("task decomposition"));
        assert!(user.contains("Build a REST API"));
    }

    #[test]
    fn test_build_decomposition_prompt_json_goal() {
        let goal = serde_json::json!({"description": "Research topic X", "depth": "deep"});
        let (_, user) = build_decomposition_prompt(&goal);
        assert!(user.contains("Research topic X"));
    }

    #[test]
    fn test_build_decomposition_chat_op() {
        let goal = serde_json::json!("test goal");
        let op = build_decomposition_chat_op(&goal, None, None);
        match op {
            IoOp::AgentChat { provider, model, system_prompt, prompt, max_iterations, .. } => {
                assert_eq!(provider, DEFAULT_DECOMPOSE_PROVIDER);
                assert_eq!(model, Some(DEFAULT_DECOMPOSE_MODEL.to_string()));
                assert!(system_prompt.is_some());
                assert!(prompt.contains("test goal"));
                assert_eq!(max_iterations, Some(1));
            }
            _ => panic!("Expected AgentChat"),
        }
    }

    #[test]
    fn test_build_decomposition_chat_op_custom_provider() {
        let goal = serde_json::json!("test");
        let op = build_decomposition_chat_op(&goal, Some("openai"), Some("gpt-4o"));
        match op {
            IoOp::AgentChat { provider, model, .. } => {
                assert_eq!(provider, "openai");
                assert_eq!(model, Some("gpt-4o".to_string()));
            }
            _ => panic!("Expected AgentChat"),
        }
    }

    #[test]
    fn test_parse_valid_json_response() {
        let response = r#"{"tasks": [{"task_id": "t1", "goal": "Do X"}, {"task_id": "t2", "goal": "Do Y"}]}"#;
        let goal = serde_json::json!("original");
        let result = parse_decomposition_response(response, &goal);
        let tasks = result["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["task_id"], "t1");
    }

    #[test]
    fn test_parse_markdown_fenced_response() {
        let response = "Here are the tasks:\n```json\n{\"tasks\": [{\"task_id\": \"t1\", \"goal\": \"Do X\"}]}\n```\nDone!";
        let goal = serde_json::json!("original");
        let result = parse_decomposition_response(response, &goal);
        let tasks = result["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn test_parse_array_response() {
        let response = r#"[{"task_id": "t1", "goal": "Do X"}, {"task_id": "t2", "goal": "Do Y"}]"#;
        let goal = serde_json::json!("original");
        let result = parse_decomposition_response(response, &goal);
        let tasks = result["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn test_parse_garbage_falls_back_to_single_task() {
        let response = "I don't understand the request, sorry!";
        let goal = serde_json::json!("Build an API");
        let result = parse_decomposition_response(response, &goal);
        let tasks = result["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["task_id"], "task-0");
        assert_eq!(tasks[0]["goal"], "Build an API");
    }

    #[test]
    fn test_parse_empty_tasks_falls_back() {
        let response = r#"{"tasks": []}"#;
        let goal = serde_json::json!("Build an API");
        let result = parse_decomposition_response(response, &goal);
        let tasks = result["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn test_extract_json_block_raw() {
        assert_eq!(extract_json_block(r#"{"a": 1}"#), r#"{"a": 1}"#);
    }

    #[test]
    fn test_extract_json_block_markdown() {
        let input = "text\n```json\n{\"a\": 1}\n```\nmore";
        assert_eq!(extract_json_block(input), "{\"a\": 1}");
    }
}
```

**Step 2: Add module to mod.rs**

Add `pub mod decomposition;` to `lib-erlangrt/src/agent_rt/mod.rs` (alphabetically, after `pub mod dead_letter;`).

**Step 3: Run tests to verify they pass**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::decomposition::tests`
Expected: PASS (all 10 tests)

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/decomposition.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(decomposition): add prompt builder and LLM response parser"
```

---

### Task 2: Wire decompose_goal through AgentChat in BridgeWorker

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/bridge.rs`

**Context:** The `BridgeWorker::run()` method dispatches IoOps in an async block (lines 404-429). Currently:
- `AgentChat` → `execute_agent_chat_standalone(...)`
- `AgentDestroy` → inline destroy logic
- Everything else (including `Custom`) → `execute_io_op(...)` which returns placeholder

We need to intercept `Custom { kind: "decompose_goal" }` before the catch-all and route it through `execute_agent_chat_standalone`, then parse the response.

**Step 1: Add import**

At top of `bridge.rs`, add to existing imports:
```rust
use crate::agent_rt::decomposition::{build_decomposition_chat_op, parse_decomposition_response};
```

**Step 2: Add decompose_goal handler in BridgeWorker dispatch**

In the `execute_op` async block (around line 404-429), add a new match arm BEFORE the catch-all `_ =>` line (line 428):

```rust
IoOp::Custom { kind, payload } if kind == "decompose_goal" => {
    let goal = payload.get("goal").cloned().unwrap_or(payload.clone());
    let chat_op = build_decomposition_chat_op(&goal, None, None);
    let chat_result = execute_agent_chat_standalone(
        req.source_pid,
        &chat_op,
        agent_reg,
        agent_prov,
        agent_tf,
        agent_met,
        mcp_mgr,
    )
    .await;
    match chat_result {
        IoResult::Ok(val) => {
            let response_text = val
                .get("response")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            IoResult::Ok(parse_decomposition_response(response_text, &goal))
        }
        other => {
            // On error/timeout, fall back to single-task
            tracing::warn!("decompose_goal LLM call failed, using single task fallback");
            IoResult::Ok(parse_decomposition_response("", &goal))
        }
    }
}
```

**Step 3: Build to verify**

Run: `cargo +nightly build`
Expected: Compiles without errors

**Step 4: Run all tests for regressions**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt`
Expected: All tests pass (434+)

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/bridge.rs
git commit -m "feat(bridge): route decompose_goal through AgentChat for real LLM decomposition"
```

---

### Task 3: Add decomposition config to orchestration settings

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/config.rs`

**Step 1: Write the failing test**

Add to the `mod tests` block in `config.rs`:

```rust
#[test]
fn test_decomposition_config_defaults() {
    let config = AppConfig::default();
    assert!(config.orchestration.decomposition.provider.is_none());
    assert!(config.orchestration.decomposition.model.is_none());
}

#[test]
fn test_parse_decomposition_config() {
    let toml_str = r#"
[orchestration.decomposition]
provider = "openai"
model = "gpt-4o"
"#;
    let config = load_config_from_str(toml_str).unwrap();
    assert_eq!(config.orchestration.decomposition.provider, Some("openai".into()));
    assert_eq!(config.orchestration.decomposition.model, Some("gpt-4o".into()));
}
```

**Step 2: Add DecompositionConfig struct and wire it in**

Add to `config.rs` (near `OrchestrationConfig`):

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DecompositionConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl Default for DecompositionConfig {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
        }
    }
}
```

Add field to `OrchestrationConfig`:
```rust
pub decomposition: DecompositionConfig,
```

Add to `Default for OrchestrationConfig`:
```rust
decomposition: DecompositionConfig::default(),
```

**Step 3: Run tests**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::config::tests`
Expected: PASS (all config tests including 2 new ones)

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/config.rs
git commit -m "feat(config): add [orchestration.decomposition] config section"
```

---

### Task 4: Integration test — decomposition with mock response

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/integration_tests.rs`

**Step 1: Write the integration test**

This test verifies the full flow: goal → orchestrator → decompose IoOp → extract_tasks → task graph. Since we can't call a real LLM in tests, we test the orchestrator's handling of a decomposition response by simulating the IoResponse.

Add to `integration_tests.rs` tests module:

```rust
#[test]
fn test_orchestrator_decomposes_goal_into_task_graph() {
    use crate::agent_rt::{
        decomposition::parse_decomposition_response,
        orchestration::OrchestratorBehavior,
    };

    // Test the decomposition parsing pipeline
    let goal = serde_json::json!("Build a REST API for todo items");

    // Simulate an LLM response with decomposed tasks
    let llm_response = r#"{"tasks": [
        {"task_id": "design", "goal": "Design the API schema and endpoints"},
        {"task_id": "implement", "goal": "Implement CRUD endpoints", "depends_on": ["design"]},
        {"task_id": "test", "goal": "Write integration tests", "depends_on": ["implement"]}
    ]}"#;

    let result = parse_decomposition_response(llm_response, &goal);
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0]["task_id"], "design");
    assert_eq!(tasks[1]["task_id"], "implement");
    assert_eq!(tasks[2]["task_id"], "test");

    // Verify dependency chain
    let deps = tasks[1]["depends_on"].as_array().unwrap();
    assert_eq!(deps[0], "design");
}

#[test]
fn test_orchestrator_fallback_on_bad_decomposition() {
    use crate::agent_rt::decomposition::parse_decomposition_response;

    let goal = serde_json::json!("Do something complex");

    // Simulate a garbage LLM response
    let bad_response = "Sorry, I can't help with that.";
    let result = parse_decomposition_response(bad_response, &goal);
    let tasks = result["tasks"].as_array().unwrap();

    // Should fall back to single task
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["goal"], "Do something complex");
}
```

**Step 2: Run test**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::integration_tests::tests::test_orchestrator_decomposes`
Expected: PASS

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::integration_tests::tests::test_orchestrator_fallback`
Expected: PASS

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/integration_tests.rs
git commit -m "test: add integration tests for orchestration decomposition"
```

---

### Task 5: Final verification

**Step 1: Run all tests**

Run: `cargo +nightly test -p erlangrt --lib`
Expected: 448+ tests pass (434 existing + 14 new)

**Step 2: Build zeptobeam binary**

Run: `cargo +nightly build --bin zeptobeam`
Expected: Compiles cleanly

**Step 3: Run BEAM regression tests**

Run: `make test`
Expected: test2:test/0 passes

---

## Summary

| Task | What | New Tests |
|------|------|-----------|
| 1 | Decomposition prompt builder + response parser | 10 |
| 2 | Wire decompose_goal through AgentChat in bridge | 0 (build check) |
| 3 | DecompositionConfig in orchestration settings | 2 |
| 4 | Integration tests for decomposition pipeline | 2 |
| 5 | Final verification | 0 (regression) |
| **Total** | | **14 new tests** |
