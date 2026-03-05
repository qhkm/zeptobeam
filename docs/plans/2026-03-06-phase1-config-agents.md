# Phase 1: Config-Driven Agent Spawning — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `zeptobeam` spawn ZeptoClaw agent processes on startup from `[[agents]]` config in `zeptobeam.toml`.

**Architecture:** Add an `AgentConfig` struct and `[[agents]]` TOML array to `AppConfig`. Create an `AgentWorkerBehavior` that wraps ZeptoClaw agent interactions — on receiving a message, it submits an `AgentChat` IoOp through the bridge and forwards the response. On startup, `zeptobeam/src/main.rs` reads the config, spawns one process per `auto_start = true` agent entry, and registers each by name.

**Tech Stack:** Rust, serde/toml (config), existing `agent_rt` scheduler/bridge/registry

---

### Task 1: Add `AgentConfig` to config parsing

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/config.rs`

**Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `config.rs`:

```rust
#[test]
fn test_parse_agents_config() {
    let toml_str = r#"
[[agents]]
name = "researcher"
provider = "openrouter"
model = "anthropic/claude-sonnet-4"
system_prompt = "You are a research assistant."
tools = ["web_fetch", "longterm_memory"]
auto_start = true

[[agents]]
name = "coder"
provider = "openai"
system_prompt = "You are a coding assistant."
tools = ["shell", "filesystem"]
auto_start = false
"#;
    let config = load_config_from_str(toml_str).unwrap();
    assert_eq!(config.agents.len(), 2);

    let r = &config.agents[0];
    assert_eq!(r.name, "researcher");
    assert_eq!(r.provider, "openrouter");
    assert_eq!(r.model, Some("anthropic/claude-sonnet-4".into()));
    assert_eq!(r.system_prompt, Some("You are a research assistant.".into()));
    assert_eq!(r.tools, vec!["web_fetch", "longterm_memory"]);
    assert!(r.auto_start);

    let c = &config.agents[1];
    assert_eq!(c.name, "coder");
    assert!(!c.auto_start);
    assert!(c.model.is_none());
}

#[test]
fn test_empty_agents_defaults_to_empty_vec() {
    let config = load_config_from_str("").unwrap();
    assert!(config.agents.is_empty());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::config::tests::test_parse_agents_config`
Expected: FAIL — `agents` field doesn't exist on `AppConfig`

**Step 3: Write minimal implementation**

Add the struct and wire it into `AppConfig` in `config.rs`:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
```

Add to `AppConfig` struct: `pub agents: Vec<AgentConfig>,`
Add to `Default for AppConfig`: `agents: Vec::new(),`

**Step 4: Run test to verify it passes**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::config::tests::test_parse_agents`
Expected: PASS (both tests)

**Step 5: Commit**

```
git add lib-erlangrt/src/agent_rt/config.rs
git commit -m "feat(config): add [[agents]] config section for agent definitions"
```

---

### Task 2: Create `AgentWorkerBehavior`

**Files:**
- Create: `lib-erlangrt/src/agent_rt/agent_worker.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod agent_worker;`)

**Step 1: Write the failing test**

Create `agent_worker.rs` with the test at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::{scheduler::AgentScheduler, types::*};

    #[test]
    fn test_agent_worker_init_stores_config() {
        let behavior = AgentWorkerBehavior::new(
            "researcher".into(),
            "openrouter".into(),
            Some("anthropic/claude-sonnet-4".into()),
            Some("You are helpful.".into()),
            vec!["web_fetch".into()],
            None,
            None,
        );
        let state = behavior.init(serde_json::json!({})).unwrap();
        let s = state.as_any().downcast_ref::<AgentWorkerState>().unwrap();
        assert!(!s.busy);
        assert!(s.last_requester.is_none());
    }

    #[test]
    fn test_agent_worker_text_message_returns_io_request() {
        let behavior = AgentWorkerBehavior::new(
            "test".into(),
            "openrouter".into(),
            None,
            None,
            vec![],
            None,
            None,
        );
        let mut state = behavior.init(serde_json::json!({})).unwrap();
        let action = behavior.handle_message(
            Message::Text("hello".into()),
            state.as_mut(),
        );
        match action {
            Action::IoRequest(IoOp::AgentChat { prompt, provider, .. }) => {
                assert_eq!(prompt, "hello");
                assert_eq!(provider, "openrouter");
            }
            other => panic!("Expected IoRequest(AgentChat), got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn test_agent_worker_rejects_while_busy() {
        let behavior = AgentWorkerBehavior::new(
            "test".into(),
            "openrouter".into(),
            None,
            None,
            vec![],
            None,
            None,
        );
        let mut state = behavior.init(serde_json::json!({})).unwrap();
        // First message — should submit IoRequest
        let _ = behavior.handle_message(
            Message::Text("hello".into()),
            state.as_mut(),
        );
        // Second message while busy — should Continue (queue)
        let action = behavior.handle_message(
            Message::Text("world".into()),
            state.as_mut(),
        );
        assert!(matches!(action, Action::Continue));
    }

    #[test]
    fn test_agent_worker_io_response_clears_busy() {
        let behavior = AgentWorkerBehavior::new(
            "test".into(),
            "openrouter".into(),
            None,
            None,
            vec![],
            None,
            None,
        );
        let mut state = behavior.init(serde_json::json!({})).unwrap();
        // Send a message to make it busy
        let _ = behavior.handle_message(
            Message::Text("hello".into()),
            state.as_mut(),
        );
        // Simulate IoResponse
        let action = behavior.handle_message(
            Message::System(SystemMsg::IoResponse {
                correlation_id: 1,
                result: IoResult::Ok(serde_json::json!({"response": "hi there"})),
            }),
            state.as_mut(),
        );
        // Should be Continue (response processed)
        assert!(matches!(action, Action::Continue));
        // No longer busy
        let s = state.as_any().downcast_ref::<AgentWorkerState>().unwrap();
        assert!(!s.busy);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::agent_worker::tests`
Expected: FAIL — module doesn't exist

**Step 3: Write minimal implementation**

```rust
//! AgentWorkerBehavior wraps a ZeptoClaw agent as a runtime process.
//! On receiving a Text/Json message, it submits an AgentChat IoOp.
//! On receiving an IoResponse, it logs the result and becomes idle.

use std::sync::Arc;

use crate::agent_rt::types::*;

pub struct AgentWorkerState {
    pub busy: bool,
    pub last_requester: Option<AgentPid>,
    pub pending_messages: Vec<Message>,
}

impl AgentState for AgentWorkerState {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

pub struct AgentWorkerBehavior {
    pub name: String,
    pub provider: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub tools: Vec<String>,
    pub max_iterations: Option<usize>,
    pub timeout_ms: Option<u64>,
}

impl AgentWorkerBehavior {
    pub fn new(
        name: String,
        provider: String,
        model: Option<String>,
        system_prompt: Option<String>,
        tools: Vec<String>,
        max_iterations: Option<usize>,
        timeout_ms: Option<u64>,
    ) -> Self {
        Self {
            name,
            provider,
            model,
            system_prompt,
            tools,
            max_iterations,
            timeout_ms,
        }
    }

    fn make_chat_op(&self, prompt: &str) -> IoOp {
        IoOp::AgentChat {
            provider: self.provider.clone(),
            model: self.model.clone(),
            system_prompt: self.system_prompt.clone(),
            prompt: prompt.to_string(),
            tools: if self.tools.is_empty() {
                None
            } else {
                Some(self.tools.clone())
            },
            max_iterations: self.max_iterations,
            timeout_ms: self.timeout_ms,
        }
    }
}

impl AgentBehavior for AgentWorkerBehavior {
    fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
        Ok(Box::new(AgentWorkerState {
            busy: false,
            last_requester: None,
            pending_messages: Vec::new(),
        }))
    }

    fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
        let s = state
            .as_any_mut()
            .downcast_mut::<AgentWorkerState>()
            .expect("AgentWorkerState");

        match &msg {
            Message::System(SystemMsg::IoResponse { result, .. }) => {
                s.busy = false;
                if let IoResult::Ok(val) = result {
                    if let Some(response) = val.get("response").and_then(|v| v.as_str()) {
                        tracing::info!(agent = %self.name, response_len = response.len(), "agent response received");
                    }
                }
                // If there are pending messages, process the next one
                if let Some(next) = s.pending_messages.pop() {
                    return self.handle_message(next, state);
                }
                Action::Continue
            }
            Message::Text(prompt) => {
                if s.busy {
                    s.pending_messages.push(msg);
                    return Action::Continue;
                }
                s.busy = true;
                Action::IoRequest(self.make_chat_op(prompt))
            }
            Message::Json(val) => {
                let prompt = val
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if prompt.is_empty() {
                    return Action::Continue;
                }
                if s.busy {
                    s.pending_messages.push(msg);
                    return Action::Continue;
                }
                s.busy = true;
                Action::IoRequest(self.make_chat_op(prompt))
            }
            _ => Action::Continue,
        }
    }

    fn handle_exit(
        &self,
        _from: AgentPid,
        _reason: Reason,
        _state: &mut dyn AgentState,
    ) -> Action {
        Action::Continue
    }

    fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}
```

**Step 4: Run test to verify it passes**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::agent_worker::tests`
Expected: PASS (all 4 tests)

**Step 5: Commit**

```
git add lib-erlangrt/src/agent_rt/agent_worker.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(agent_worker): add AgentWorkerBehavior for wrapping ZeptoClaw agents"
```

---

### Task 3: Add `spawn_configured_agents` helper

**Files:**
- Create: `lib-erlangrt/src/agent_rt/startup.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add `pub mod startup;`)

**Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::{config::AgentConfig, scheduler::AgentScheduler};

    #[test]
    fn test_spawn_configured_agents_empty() {
        let mut scheduler = AgentScheduler::new();
        let result = spawn_configured_agents(&mut scheduler, &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn test_spawn_configured_agents_auto_start_true() {
        let mut scheduler = AgentScheduler::new();
        let agents = vec![AgentConfig {
            name: "test-agent".into(),
            provider: "openrouter".into(),
            model: None,
            system_prompt: None,
            tools: vec![],
            auto_start: true,
            max_iterations: None,
            timeout_ms: None,
        }];
        let result = spawn_configured_agents(&mut scheduler, &agents).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, "test-agent");
        // Verify process is registered by name
        let pid = scheduler.registry.lookup_name("test-agent");
        assert!(pid.is_some());
        assert_eq!(pid.unwrap(), result[0].0);
    }

    #[test]
    fn test_spawn_configured_agents_skips_auto_start_false() {
        let mut scheduler = AgentScheduler::new();
        let agents = vec![
            AgentConfig {
                name: "active".into(),
                provider: "openrouter".into(),
                model: None,
                system_prompt: None,
                tools: vec![],
                auto_start: true,
                max_iterations: None,
                timeout_ms: None,
            },
            AgentConfig {
                name: "inactive".into(),
                provider: "openai".into(),
                model: None,
                system_prompt: None,
                tools: vec![],
                auto_start: false,
                max_iterations: None,
                timeout_ms: None,
            },
        ];
        let result = spawn_configured_agents(&mut scheduler, &agents).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, "active");
        assert!(scheduler.registry.lookup_name("inactive").is_none());
    }

    #[test]
    fn test_spawn_configured_agents_enqueues_processes() {
        let mut scheduler = AgentScheduler::new();
        let agents = vec![AgentConfig {
            name: "worker".into(),
            provider: "openrouter".into(),
            model: None,
            system_prompt: None,
            tools: vec![],
            auto_start: true,
            max_iterations: None,
            timeout_ms: None,
        }];
        let spawned = spawn_configured_agents(&mut scheduler, &agents).unwrap();
        let pid = spawned[0].0;
        // Process should be in the registry and runnable
        let proc = scheduler.registry.lookup(&pid).unwrap();
        assert_eq!(proc.status, crate::agent_rt::process::ProcessStatus::Waiting);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::startup::tests`
Expected: FAIL — module doesn't exist

**Step 3: Write minimal implementation**

```rust
//! Startup helpers for spawning agents from config.

use std::sync::Arc;

use tracing::info;

use crate::agent_rt::{
    agent_worker::AgentWorkerBehavior,
    config::AgentConfig,
    scheduler::AgentScheduler,
    types::AgentPid,
};

/// Spawn agent processes for each `auto_start = true` entry in config.
/// Returns a vec of (pid, name) for successfully spawned agents.
pub fn spawn_configured_agents(
    scheduler: &mut AgentScheduler,
    agents: &[AgentConfig],
) -> Result<Vec<(AgentPid, String)>, String> {
    let mut spawned = Vec::new();

    for agent_cfg in agents {
        if !agent_cfg.auto_start {
            continue;
        }

        let behavior = Arc::new(AgentWorkerBehavior::new(
            agent_cfg.name.clone(),
            agent_cfg.provider.clone(),
            agent_cfg.model.clone(),
            agent_cfg.system_prompt.clone(),
            agent_cfg.tools.clone(),
            agent_cfg.max_iterations,
            agent_cfg.timeout_ms,
        ));

        let pid = scheduler
            .registry
            .spawn(behavior, serde_json::json!({}))
            .map_err(|r| format!("Failed to spawn agent '{}': {:?}", agent_cfg.name, r))?;

        scheduler
            .registry
            .register_name(&agent_cfg.name, pid)
            .map_err(|e| format!("Failed to register agent '{}': {}", agent_cfg.name, e))?;

        info!(
            name = %agent_cfg.name,
            pid = pid.raw(),
            provider = %agent_cfg.provider,
            "spawned agent from config"
        );

        spawned.push((pid, agent_cfg.name.clone()));
    }

    Ok(spawned)
}
```

**Step 4: Run test to verify it passes**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::startup::tests`
Expected: PASS (all 4 tests)

**Step 5: Commit**

```
git add lib-erlangrt/src/agent_rt/startup.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(startup): add spawn_configured_agents helper"
```

---

### Task 4: Wire agent spawning into `zeptobeam` daemon

**Files:**
- Modify: `zeptobeam/src/main.rs`

**Step 1: Read the current `main.rs`** (already read above)

**Step 2: Add agent spawning after scheduler creation**

In `spawn_mcp_runtime_worker`, after `let mut scheduler = AgentScheduler::new();`, add config-driven spawning. Since the function currently doesn't have access to config, we need to pass the agents config in.

Change `spawn_mcp_runtime_worker` signature to accept `agents: Vec<AgentConfig>`:

```rust
fn spawn_mcp_runtime_worker(
    process_count: Arc<AtomicUsize>,
    agents: Vec<erlangrt::agent_rt::config::AgentConfig>,
) -> (
    Arc<dyn McpRuntimeOps>,
    mpsc::Sender<McpRuntimeCommand>,
    JoinHandle<()>,
) {
```

Inside the thread, after `let mut scheduler = AgentScheduler::new();`:

```rust
    // Spawn configured agents
    match erlangrt::agent_rt::startup::spawn_configured_agents(&mut scheduler, &agents) {
        Ok(spawned) => {
            for (pid, name) in &spawned {
                info!(name = %name, pid = pid.raw(), "agent started");
                scheduler.enqueue(*pid);
            }
        }
        Err(e) => {
            warn!(error = %e, "failed to spawn configured agents");
        }
    }
```

Update the call site in `main()` to pass `config.agents.clone()`:

```rust
let (runtime_ops, shutdown_tx, worker_handle) =
    spawn_mcp_runtime_worker(process_count.clone(), config.agents.clone());
```

**Step 3: Build to verify it compiles**

Run: `cargo +nightly build`
Expected: Compiles without errors

**Step 4: Run existing tests to verify no regressions**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt`
Expected: All 342+ tests pass

**Step 5: Commit**

```
git add zeptobeam/src/main.rs
git commit -m "feat(daemon): spawn configured agents on startup"
```

---

### Task 5: Add integration test — full config-to-spawn flow

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/integration_tests.rs`

**Step 1: Write the integration test**

Add to integration_tests.rs:

```rust
#[test]
fn test_config_driven_agent_spawn_and_message() {
    use crate::agent_rt::{
        config::load_config_from_str,
        scheduler::AgentScheduler,
        startup::spawn_configured_agents,
        types::*,
    };

    let toml_str = r#"
[[agents]]
name = "helper"
provider = "openrouter"
model = "anthropic/claude-sonnet-4"
system_prompt = "You are helpful."
tools = ["web_fetch"]
auto_start = true

[[agents]]
name = "disabled"
provider = "openai"
auto_start = false
"#;
    let config = load_config_from_str(toml_str).unwrap();
    let mut scheduler = AgentScheduler::new();

    let spawned = spawn_configured_agents(&mut scheduler, &config.agents).unwrap();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].1, "helper");

    // Verify process exists and is registered
    let pid = scheduler.registry.lookup_name("helper").unwrap();
    assert!(scheduler.registry.lookup(&pid).is_some());

    // Send a message to the agent
    scheduler.enqueue(pid);
    let send_result = scheduler.send(pid, Message::Text("hello".into()));
    assert!(send_result.is_ok());

    // Tick the scheduler — agent should process message and emit IoRequest
    // (which will fail without a bridge, but the process should handle it)
    let did_work = scheduler.tick();
    assert!(did_work);

    // Agent should still be alive (IoRequest without bridge delivers error back)
    assert!(scheduler.registry.lookup(&pid).is_some());
}
```

**Step 2: Run test to verify it passes**

Run: `cargo +nightly test -p erlangrt --lib -- agent_rt::integration_tests::test_config_driven_agent_spawn_and_message`
Expected: PASS

**Step 3: Commit**

```
git add lib-erlangrt/src/agent_rt/integration_tests.rs
git commit -m "test: add integration test for config-driven agent spawning"
```

---

### Task 6: Final verification

**Step 1: Run all tests**

Run: `cargo +nightly test -p erlangrt --lib`
Expected: 346+ tests pass (342 existing + 4 new)

**Step 2: Run BEAM tests (regression check)**

Run: `make test-beam`
Expected: 3/3 pass (smoke, test2, test_bs_nostdlib)

**Step 3: Build zeptobeam binary**

Run: `cargo +nightly build --bin zeptobeam`
Expected: Compiles cleanly

**Step 4: Verify help output**

Run: `cargo +nightly run --bin zeptobeam -- --help`
Expected: Shows CLI help

---

## Summary

| Task | What | New Tests |
|------|------|-----------|
| 1 | `AgentConfig` struct + TOML parsing | 2 |
| 2 | `AgentWorkerBehavior` implementation | 4 |
| 3 | `spawn_configured_agents` helper | 4 |
| 4 | Wire into `zeptobeam` daemon | 0 (build check) |
| 5 | Integration test | 1 |
| 6 | Final verification | 0 (regression) |
| **Total** | | **11 new tests** |
