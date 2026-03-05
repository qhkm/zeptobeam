# ZeptoAgent Facade Integration — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Integrate zeptoclaw's `ZeptoAgent` facade into zeptoclaw-rt so each worker process runs a full AI agent with tool access, multi-turn conversation, and shared LLM providers.

**Architecture:** Bridge-level integration. Workers emit `IoOp::AgentChat`, the bridge's Tokio runtime holds a `HashMap<AgentPid, Arc<Mutex<ZeptoAgent>>>` agent registry. First call creates the agent, subsequent calls reuse it. `IoOp::AgentDestroy` evicts on worker death. Providers shared via `Arc<dyn LLMProvider>`.

**Tech Stack:** Rust, zeptoclaw (path dep), tokio, crossbeam-channel, serde_json

**Design Doc:** `docs/plans/2026-03-05-zeptoagent-integration-design.md`

---

## Task 0: Update Toolchain and Remove Unused Feature Flag

The pinned `nightly-2024-11-01` toolchain cannot compile `edition2024` crates (reqwest 0.12 -> getrandom 0.4.2). The `#![feature(ptr_metadata)]` flag is declared but never used in any source file. Remove it and update the toolchain.

**Files:**
- Modify: `rust-toolchain.toml`
- Modify: `lib-erlangrt/src/lib.rs:6`
- Modify: `CLAUDE.md:11` (remove nightly requirement note)

**Step 1: Remove unused feature flag**

In `lib-erlangrt/src/lib.rs`, delete line 6:
```rust
// DELETE: #![feature(ptr_metadata)]
```

**Step 2: Update toolchain**

In `rust-toolchain.toml`, change to latest nightly:
```toml
[toolchain]
channel = "nightly-2026-03-04"
components = ["rustfmt", "clippy"]
```

**Step 3: Update CLAUDE.md**

In `CLAUDE.md` line 11, change:
```
Requires Rust **nightly** toolchain (uses `#![feature(ptr_metadata)]`).
```
to:
```
Requires Rust **nightly** toolchain (pinned in `rust-toolchain.toml`).
```

**Step 4: Verify existing code still builds**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo build 2>&1 | tail -5`
Expected: Compiles successfully

**Step 5: Run existing tests**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt -- agent_rt 2>&1 | tail -10`
Expected: All ~95 existing agent_rt tests pass

**Step 6: Commit**

```bash
git add rust-toolchain.toml lib-erlangrt/src/lib.rs CLAUDE.md
git commit -m "chore: update toolchain to nightly-2026-03-04, remove unused ptr_metadata feature"
```

---

## Task 1: Add zeptoclaw Path Dependency and ToolFactory Trait

**Files:**
- Modify: `lib-erlangrt/Cargo.toml:36-50` (add zeptoclaw dep, async-trait)
- Create: `lib-erlangrt/src/agent_rt/tool_factory.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs` (add module)

**Step 1: Add dependencies to Cargo.toml**

Add to `[dependencies]` section of `lib-erlangrt/Cargo.toml`:
```toml
zeptoclaw = { path = "../../zeptoclaw", default-features = false }
async-trait = "0.1"
```

Note: `default-features = false` avoids pulling in optional features (screenshot, pdf, hardware, etc.) that aren't needed.

**Step 2: Verify it compiles**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo check -p erlangrt 2>&1 | tail -10`
Expected: Compiles (may take a while on first build to compile zeptoclaw)

**Step 3: Write ToolFactory trait**

Create `lib-erlangrt/src/agent_rt/tool_factory.rs`:
```rust
use zeptoclaw::tools::Tool;

/// Builds tool sets for ZeptoAgent instances.
/// Implementations know about available tools and filter
/// by an optional whitelist.
pub trait ToolFactory: Send + Sync {
    fn build_tools(&self, whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>>;
}
```

**Step 4: Add module to mod.rs**

In `lib-erlangrt/src/agent_rt/mod.rs`, add:
```rust
pub mod tool_factory;
```

**Step 5: Verify it compiles**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo check -p erlangrt 2>&1 | tail -5`
Expected: Compiles

**Step 6: Commit**

```bash
git add lib-erlangrt/Cargo.toml lib-erlangrt/src/agent_rt/tool_factory.rs lib-erlangrt/src/agent_rt/mod.rs
git commit -m "feat(agent_rt): add zeptoclaw path dependency and ToolFactory trait"
```

---

## Task 2: Add IoOp::AgentChat and IoOp::AgentDestroy Variants

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/types.rs:125-151`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

Add to `tests.rs`:
```rust
#[test]
fn test_agent_chat_io_op_fields() {
    let op = IoOp::AgentChat {
        provider: "anthropic".to_string(),
        model: Some("claude-3-5-sonnet-latest".to_string()),
        system_prompt: Some("You are a researcher.".to_string()),
        prompt: "Find recent papers on BEAM VM.".to_string(),
        tools: Some(vec!["web_search".to_string(), "web_fetch".to_string()]),
        max_iterations: Some(5),
        timeout_ms: Some(60_000),
    };
    match op {
        IoOp::AgentChat { provider, prompt, tools, .. } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(prompt, "Find recent papers on BEAM VM.");
            assert_eq!(tools.unwrap().len(), 2);
        }
        _ => panic!("expected AgentChat"),
    }
}

#[test]
fn test_agent_destroy_io_op() {
    let pid = AgentPid::from_raw(0x8000_9999);
    let op = IoOp::AgentDestroy { target_pid: pid };
    match op {
        IoOp::AgentDestroy { target_pid } => {
            assert_eq!(target_pid.raw(), 0x8000_9999);
        }
        _ => panic!("expected AgentDestroy"),
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_agent_chat_io_op_fields 2>&1 | tail -5`
Expected: FAIL — `AgentChat` variant doesn't exist

**Step 3: Add variants to IoOp**

In `lib-erlangrt/src/agent_rt/types.rs`, add to the `IoOp` enum (after `Custom`):
```rust
AgentChat {
    provider: String,
    model: Option<String>,
    system_prompt: Option<String>,
    prompt: String,
    tools: Option<Vec<String>>,
    max_iterations: Option<usize>,
    timeout_ms: Option<u64>,
},
AgentDestroy {
    target_pid: AgentPid,
},
```

**Step 4: Run tests to verify they pass**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_agent_chat_io_op_fields test_agent_destroy_io_op 2>&1 | tail -5`
Expected: PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/types.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): add IoOp::AgentChat and IoOp::AgentDestroy variants"
```

---

## Task 3: Bridge Metrics Counters

**Files:**
- Create: `lib-erlangrt/src/agent_rt/bridge_metrics.rs`
- Modify: `lib-erlangrt/src/agent_rt/mod.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

Add to `tests.rs`:
```rust
use crate::agent_rt::bridge_metrics::BridgeMetrics;

#[test]
fn test_bridge_metrics_increment_and_read() {
    let m = BridgeMetrics::new();
    assert_eq!(m.agent_destroy_failures(), 0);
    assert_eq!(m.worker_busy_rejections(), 0);
    assert_eq!(m.agent_chat_panics(), 0);

    m.inc_destroy_failures();
    m.inc_destroy_failures();
    m.inc_busy_rejections();
    m.inc_chat_panics();

    assert_eq!(m.agent_destroy_failures(), 2);
    assert_eq!(m.worker_busy_rejections(), 1);
    assert_eq!(m.agent_chat_panics(), 1);
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_bridge_metrics_increment_and_read 2>&1 | tail -5`
Expected: FAIL — module doesn't exist

**Step 3: Implement BridgeMetrics**

Create `lib-erlangrt/src/agent_rt/bridge_metrics.rs`:
```rust
use std::sync::atomic::{AtomicU64, Ordering};

/// Counters for bridge-level operational metrics.
#[derive(Default)]
pub struct BridgeMetrics {
    destroy_failures: AtomicU64,
    busy_rejections: AtomicU64,
    chat_panics: AtomicU64,
}

impl BridgeMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc_destroy_failures(&self) {
        self.destroy_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_busy_rejections(&self) {
        self.busy_rejections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_chat_panics(&self) {
        self.chat_panics.fetch_add(1, Ordering::Relaxed);
    }

    pub fn agent_destroy_failures(&self) -> u64 {
        self.destroy_failures.load(Ordering::Relaxed)
    }

    pub fn worker_busy_rejections(&self) -> u64 {
        self.busy_rejections.load(Ordering::Relaxed)
    }

    pub fn agent_chat_panics(&self) -> u64 {
        self.chat_panics.load(Ordering::Relaxed)
    }
}
```

**Step 4: Add module to mod.rs**

In `lib-erlangrt/src/agent_rt/mod.rs`, add:
```rust
pub mod bridge_metrics;
```

**Step 5: Run test to verify it passes**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_bridge_metrics 2>&1 | tail -5`
Expected: PASS

**Step 6: Commit**

```bash
git add lib-erlangrt/src/agent_rt/bridge_metrics.rs lib-erlangrt/src/agent_rt/mod.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): add BridgeMetrics counters for observability"
```

---

## Task 4: Bridge Agent Registry and Provider Registry

This is the core integration. Add the agent registry, provider registry, and tool factory to `BridgeWorker`. Handle `IoOp::AgentChat` and `IoOp::AgentDestroy` in `execute_io_op`.

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/bridge.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test — AgentChat creates agent**

Add to `tests.rs`:
```rust
#[tokio::test]
async fn test_agent_chat_creates_agent_on_first_call() {
    use crate::agent_rt::bridge::{create_bridge, AgentChatConfig};
    use crate::agent_rt::bridge_metrics::BridgeMetrics;
    use std::sync::Arc;

    let (_handle, mut worker) = create_bridge();
    let metrics = Arc::new(BridgeMetrics::new());
    let provider_registry = Arc::new(create_mock_provider_registry());
    let tool_factory = Arc::new(MockToolFactory);

    worker.set_provider_registry(provider_registry);
    worker.set_tool_factory(tool_factory);
    worker.set_metrics(metrics);

    let pid = AgentPid::from_raw(0x8000_A001);
    let op = IoOp::AgentChat {
        provider: "mock".to_string(),
        model: None,
        system_prompt: None,
        prompt: "Hello".to_string(),
        tools: None,
        max_iterations: None,
        timeout_ms: None,
    };

    let result = worker.execute_agent_chat(pid, &op).await;
    match result {
        IoResult::Ok(v) => {
            assert!(v.get("response").is_some());
        }
        IoResult::Error(e) => panic!("expected Ok, got Error: {}", e),
        _ => panic!("expected Ok"),
    }
}
```

Note: This test requires `MockLLMProvider`, `MockToolFactory`, and `create_mock_provider_registry` helpers. These will be defined in the test module using zeptoclaw's traits:

```rust
use zeptoclaw::providers::{LLMProvider, LLMResponse, ChatOptions, ToolDefinition, StreamEvent};
use zeptoclaw::session::Message as ZeptoMessage;
use zeptoclaw::tools::Tool;
use zeptoclaw::error::Result as ZeptoResult;
use async_trait::async_trait;

struct MockLLMProvider;

#[async_trait]
impl LLMProvider for MockLLMProvider {
    async fn chat(
        &self,
        _messages: Vec<ZeptoMessage>,
        _tools: Vec<ToolDefinition>,
        _model: Option<&str>,
        _options: ChatOptions,
    ) -> ZeptoResult<LLMResponse> {
        Ok(LLMResponse::text("Mock response"))
    }
    fn default_model(&self) -> &str { "mock-model" }
    fn name(&self) -> &str { "mock" }
    async fn chat_stream(
        &self, _m: Vec<ZeptoMessage>, _t: Vec<ToolDefinition>,
        _model: Option<&str>, _o: ChatOptions,
    ) -> ZeptoResult<tokio::sync::mpsc::Receiver<StreamEvent>> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(rx)
    }
    async fn embed(&self, _texts: &[String]) -> ZeptoResult<Vec<Vec<f32>>> {
        Ok(vec![])
    }
}

struct MockToolFactory;

impl crate::agent_rt::tool_factory::ToolFactory for MockToolFactory {
    fn build_tools(&self, _whitelist: Option<&[String]>) -> Vec<Box<dyn Tool>> {
        vec![] // No tools for basic test
    }
}

fn create_mock_provider_registry() -> std::collections::HashMap<String, Arc<dyn LLMProvider + Send + Sync>> {
    let mut m = std::collections::HashMap::new();
    m.insert("mock".to_string(), Arc::new(MockLLMProvider) as Arc<dyn LLMProvider + Send + Sync>);
    m
}
```

**Step 2: Run test to verify it fails**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_agent_chat_creates_agent 2>&1 | tail -10`
Expected: FAIL — `set_provider_registry`, `execute_agent_chat` don't exist

**Step 3: Implement bridge changes**

In `lib-erlangrt/src/agent_rt/bridge.rs`, add imports:
```rust
use crate::agent_rt::{
    bridge_metrics::BridgeMetrics,
    rate_limiter::RateLimiter,
    tool_factory::ToolFactory,
    types::*,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use zeptoclaw::agent::ZeptoAgent;
use zeptoclaw::providers::LLMProvider;
```

Add new fields to `BridgeWorker`:
```rust
pub struct BridgeWorker {
    request_rx: Receiver<IoRequest>,
    response_tx: Sender<IoResponse>,
    rate_limiter: Option<RateLimiter>,
    agent_registry: Arc<std::sync::Mutex<HashMap<AgentPid, Arc<TokioMutex<ZeptoAgent>>>>>,
    provider_registry: Arc<HashMap<String, Arc<dyn LLMProvider + Send + Sync>>>,
    tool_factory: Arc<dyn ToolFactory>,
    metrics: Arc<BridgeMetrics>,
}
```

Update `create_bridge()` to initialize new fields with defaults:
```rust
BridgeWorker {
    request_rx: req_rx,
    response_tx: resp_tx,
    rate_limiter: None,
    agent_registry: Arc::new(std::sync::Mutex::new(HashMap::new())),
    provider_registry: Arc::new(HashMap::new()),
    tool_factory: Arc::new(NoopToolFactory),
    metrics: Arc::new(BridgeMetrics::new()),
}
```

Where `NoopToolFactory` is a private default:
```rust
struct NoopToolFactory;
impl ToolFactory for NoopToolFactory {
    fn build_tools(&self, _whitelist: Option<&[String]>) -> Vec<Box<dyn zeptoclaw::tools::Tool>> {
        vec![]
    }
}
```

Add setter methods:
```rust
impl BridgeWorker {
    pub fn set_provider_registry(
        &mut self,
        registry: Arc<HashMap<String, Arc<dyn LLMProvider + Send + Sync>>>,
    ) {
        self.provider_registry = registry;
    }

    pub fn set_tool_factory(&mut self, factory: Arc<dyn ToolFactory>) {
        self.tool_factory = factory;
    }

    pub fn set_metrics(&mut self, metrics: Arc<BridgeMetrics>) {
        self.metrics = metrics;
    }
}
```

Add `execute_agent_chat` method (public for testing):
```rust
impl BridgeWorker {
    pub async fn execute_agent_chat(&self, pid: AgentPid, op: &IoOp) -> IoResult {
        let (provider_name, model, system_prompt, prompt, tools_whitelist, max_iterations) =
            match op {
                IoOp::AgentChat {
                    provider, model, system_prompt, prompt, tools, max_iterations, ..
                } => (provider, model, system_prompt, prompt, tools, max_iterations),
                _ => return IoResult::Error("not an AgentChat op".into()),
            };

        // Get or create agent
        let agent_arc = {
            let mut registry = self.agent_registry.lock().unwrap();
            registry.entry(pid).or_insert_with(|| {
                let llm_provider = self.provider_registry
                    .get(provider_name)
                    .cloned()
                    .unwrap_or_else(|| {
                        self.provider_registry.values().next().unwrap().clone()
                    });

                let tools = self.tool_factory.build_tools(
                    tools_whitelist.as_ref().map(|v| v.as_slice()),
                );

                let mut builder = ZeptoAgent::builder()
                    .provider_arc(llm_provider)
                    .tools(tools);

                if let Some(sp) = system_prompt {
                    builder = builder.system_prompt(sp);
                }
                if let Some(m) = model {
                    builder = builder.model(m);
                }
                if let Some(mi) = max_iterations {
                    builder = builder.max_iterations(*mi);
                }

                Arc::new(TokioMutex::new(builder.build().expect("agent build")))
            }).clone()
        };

        // Lock per-pid and chat — wrapped in panic containment
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // We need to be inside an async context for .await
            // This closure is sync, so we return a future
        }));

        // Actually: use tokio spawn + catch panic on join
        let metrics = self.metrics.clone();
        let agent = agent_arc.lock().await;
        match agent.chat(prompt).await {
            Ok(response) => IoResult::Ok(serde_json::json!({ "response": response })),
            Err(e) => IoResult::Error(format!("agent chat error: {}", e)),
        }
    }

    pub fn execute_agent_destroy(&self, target_pid: AgentPid) -> IoResult {
        let mut registry = self.agent_registry.lock().unwrap();
        let removed = registry.remove(&target_pid).is_some();
        IoResult::Ok(serde_json::json!({ "destroyed": removed }))
    }
}
```

Update the `run()` loop to handle AgentChat and AgentDestroy by spawning tasks:
```rust
pub async fn run(self) {
    let req_rx = self.request_rx;
    let resp_tx = self.response_tx;
    let rate_limiter = self.rate_limiter;
    let agent_registry = self.agent_registry;
    let provider_registry = self.provider_registry;
    let tool_factory = self.tool_factory;
    let metrics = self.metrics;

    loop {
        let rx = req_rx.clone();
        let recv_result = tokio::task::spawn_blocking(move || rx.recv()).await;
        let req = match recv_result {
            Ok(Ok(r)) => r,
            _ => break,
        };

        let tx = resp_tx.clone();
        let rl = rate_limiter.clone();
        let ar = agent_registry.clone();
        let pr = provider_registry.clone();
        let tf = tool_factory.clone();
        let met = metrics.clone();

        tokio::spawn(async move {
            let result = match &req.op {
                IoOp::AgentChat { .. } => {
                    execute_agent_chat_task(
                        req.source_pid, &req.op, ar, pr, tf, met,
                    ).await
                }
                IoOp::AgentDestroy { target_pid } => {
                    let mut reg = ar.lock().unwrap();
                    let removed = reg.remove(target_pid).is_some();
                    IoResult::Ok(serde_json::json!({ "destroyed": removed }))
                }
                _ => execute_io_op(&req.op, rl.as_ref()).await,
            };
            let resp = IoResponse {
                correlation_id: req.correlation_id,
                source_pid: req.source_pid,
                result,
            };
            let _ = tx.send(resp);
        });
    }
}
```

Where `execute_agent_chat_task` is a standalone async fn with panic containment:
```rust
async fn execute_agent_chat_task(
    pid: AgentPid,
    op: &IoOp,
    agent_registry: Arc<std::sync::Mutex<HashMap<AgentPid, Arc<TokioMutex<ZeptoAgent>>>>>,
    provider_registry: Arc<HashMap<String, Arc<dyn LLMProvider + Send + Sync>>>,
    tool_factory: Arc<dyn ToolFactory>,
    metrics: Arc<BridgeMetrics>,
) -> IoResult {
    // Extract fields, get-or-create agent, lock per-pid, call chat
    // Panic containment via tokio::spawn + JoinHandle error check
    // On panic: metrics.inc_chat_panics(), return IoResult::Error
    // (Full implementation as described above)
}
```

Note: `ZeptoAgentBuilder` has `.provider(impl LLMProvider)` but we need `.provider_arc(Arc<dyn LLMProvider>)` to avoid cloning. Check if this method exists in zeptoclaw — if not, we'll need to add it or use the existing `.provider()` with a wrapper.

**Step 4: Run test to verify it passes**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_agent_chat_creates_agent 2>&1 | tail -10`
Expected: PASS

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/bridge.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): bridge agent registry with AgentChat and AgentDestroy execution"
```

---

## Task 5: AgentDestroy Tests — Eviction and Idempotency

**Files:**
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing tests**

```rust
#[tokio::test]
async fn test_agent_destroy_evicts_registry() {
    let (_handle, mut worker) = create_bridge();
    // ... setup with mock provider registry ...
    let pid = AgentPid::from_raw(0x8000_B001);

    // Create agent via chat
    let op = IoOp::AgentChat {
        provider: "mock".to_string(),
        model: None,
        system_prompt: None,
        prompt: "Hello".to_string(),
        tools: None,
        max_iterations: None,
        timeout_ms: None,
    };
    let _ = worker.execute_agent_chat(pid, &op).await;

    // Destroy
    let result = worker.execute_agent_destroy(pid);
    match result {
        IoResult::Ok(v) => assert_eq!(v["destroyed"], true),
        _ => panic!("expected Ok"),
    }

    // Next chat should create a fresh agent (no error)
    let result2 = worker.execute_agent_chat(pid, &op).await;
    assert!(matches!(result2, IoResult::Ok(_)));
}

#[tokio::test]
async fn test_agent_destroy_idempotent() {
    let (_handle, worker) = create_bridge();
    let pid = AgentPid::from_raw(0x8000_B002);

    let result = worker.execute_agent_destroy(pid);
    match result {
        IoResult::Ok(v) => assert_eq!(v["destroyed"], false),
        _ => panic!("expected Ok with destroyed: false"),
    }
}
```

**Step 2: Run tests, verify they pass**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_agent_destroy 2>&1 | tail -10`
Expected: PASS (implementation already in Task 4)

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test(agent_rt): AgentDestroy eviction and idempotency tests"
```

---

## Task 6: Multi-Turn History Test

**Files:**
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

Uses a `CountingMockProvider` that tracks how many messages it receives on each call. Second chat should have more messages (history from first).

```rust
#[tokio::test]
async fn test_agent_chat_multi_turn_history() {
    // MockProvider that records message count per call
    let call_log = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log_clone = call_log.clone();

    // ... setup with counting mock provider ...
    let pid = AgentPid::from_raw(0x8000_C001);

    // First chat
    let op1 = IoOp::AgentChat {
        provider: "counting".to_string(),
        prompt: "First message".to_string(),
        // ... other fields None ...
    };
    let _ = worker.execute_agent_chat(pid, &op1).await;

    // Second chat — same pid, history should include first exchange
    let op2 = IoOp::AgentChat {
        provider: "counting".to_string(),
        prompt: "Second message".to_string(),
        // ... other fields None ...
    };
    let _ = worker.execute_agent_chat(pid, &op2).await;

    let log = call_log.lock().await;
    // First call: system + user = 2 messages
    // Second call: system + user + assistant + user = 4 messages
    assert!(log[1] > log[0], "second call should see more history");
}
```

**Step 2: Run test, verify pass**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt test_agent_chat_multi_turn 2>&1 | tail -10`
Expected: PASS

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test(agent_rt): multi-turn conversation history accumulation"
```

---

## Task 7: Tool Whitelist Test

**Files:**
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

```rust
#[tokio::test]
async fn test_agent_chat_tool_whitelist() {
    // ToolFactory that records which whitelist was requested
    let requested = Arc::new(std::sync::Mutex::new(None));
    // ... setup with recording tool factory ...

    let op = IoOp::AgentChat {
        provider: "mock".to_string(),
        tools: Some(vec!["web_search".to_string(), "shell".to_string()]),
        prompt: "Search for something".to_string(),
        // ... other fields ...
    };

    let _ = worker.execute_agent_chat(pid, &op).await;

    let req = requested.lock().unwrap();
    assert_eq!(req.as_ref().unwrap(), &vec!["web_search".to_string(), "shell".to_string()]);
}
```

**Step 2: Run test, verify pass**

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test(agent_rt): tool whitelist filtering"
```

---

## Task 8: Bridge Panic Containment Test

**Files:**
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

```rust
#[tokio::test]
async fn test_bridge_panic_containment_on_agent_chat() {
    // Provider that panics on chat()
    struct PanickingProvider;
    // impl LLMProvider ... { fn chat() { panic!("provider exploded") } }

    let metrics = Arc::new(BridgeMetrics::new());
    // ... setup with panicking provider ...

    let op = IoOp::AgentChat {
        provider: "panicking".to_string(),
        prompt: "Trigger panic".to_string(),
        // ...
    };

    let result = worker.execute_agent_chat(pid, &op).await;
    assert!(matches!(result, IoResult::Error(_)));
    assert_eq!(metrics.agent_chat_panics(), 1);
}
```

**Step 2: Run test, verify pass**

**Step 3: Commit**

```bash
git add lib-erlangrt/src/agent_rt/tests.rs
git commit -m "test(agent_rt): bridge panic containment for agent chat"
```

---

## Task 9: Worker Busy Rejection

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

```rust
#[test]
fn test_worker_rejects_followup_while_busy() {
    let behavior = WorkerBehavior;
    let mut state = behavior
        .init(serde_json::json!({ "parent_pid": 0x8000_D001u64, "task_id": "t1" }))
        .unwrap();

    // Send run_task — puts worker in awaiting state
    let action = behavior.handle_message(
        Message::Json(serde_json::json!({
            "type": "run_task",
            "worker_pid": 0x8000_D002u64,
            "task_id": "t1",
            "task": { "prompt": "do work" },
        })),
        state.as_mut(),
    );
    assert!(matches!(action, Action::IoRequest(_)));

    // Send follow_up while still awaiting — should reject
    let action = behavior.handle_message(
        Message::Json(serde_json::json!({
            "type": "follow_up",
            "prompt": "do more",
        })),
        state.as_mut(),
    );
    match action {
        Action::Send { to, msg } => {
            assert_eq!(to.raw(), 0x8000_D001u64); // sent back to parent
            match msg {
                Message::Json(v) => assert_eq!(v["type"], "worker_busy"),
                _ => panic!("expected json"),
            }
        }
        _ => panic!("expected Send with worker_busy"),
    }
}
```

**Step 2: Run test to verify it fails**

**Step 3: Implement follow_up and busy rejection in WorkerBehavior**

In `orchestration.rs`, update `WorkerBehavior::handle_message`:
- Add match for `"follow_up"` message type
- If `awaiting_result`, return `Action::Send { to: parent, msg: worker_busy }`
- If not awaiting, emit `IoOp::AgentChat` with the follow-up prompt

**Step 4: Run test to verify it passes**

**Step 5: Commit**

```bash
git add lib-erlangrt/src/agent_rt/orchestration.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): worker busy rejection on follow_up while awaiting result"
```

---

## Task 10: Worker Max Turns Guard

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

```rust
#[test]
fn test_worker_max_turns_guard() {
    let behavior = WorkerBehavior;
    let mut state = behavior
        .init(serde_json::json!({
            "parent_pid": 0x8000_E001u64,
            "task_id": "t1",
            "max_turns": 2,
        }))
        .unwrap();

    // Turn 1: run_task
    let _ = behavior.handle_message(
        Message::Json(serde_json::json!({
            "type": "run_task",
            "worker_pid": 0x8000_E002u64,
            "task_id": "t1",
            "task": { "prompt": "turn 1" },
        })),
        state.as_mut(),
    );
    // Simulate IoResponse
    let _ = behavior.handle_message(
        Message::System(SystemMsg::IoResponse {
            correlation_id: 0,
            result: IoResult::Ok(serde_json::json!({"response": "result 1"})),
        }),
        state.as_mut(),
    );

    // Turn 2: follow_up
    let _ = behavior.handle_message(
        Message::Json(serde_json::json!({
            "type": "follow_up",
            "prompt": "turn 2",
        })),
        state.as_mut(),
    );
    // Simulate IoResponse — should trigger auto-stop (max_turns reached)
    let action = behavior.handle_message(
        Message::System(SystemMsg::IoResponse {
            correlation_id: 1,
            result: IoResult::Ok(serde_json::json!({"response": "result 2"})),
        }),
        state.as_mut(),
    );

    // After sending result, worker should stop
    // First it sends the result, then on next tick it should stop
    // (exact behavior depends on implementation)
    match action {
        Action::Send { to, msg } => {
            assert_eq!(to.raw(), 0x8000_E001u64);
            // After this send, the worker should auto-stop on next message
        }
        Action::Stop(_) => { /* also acceptable */ }
        _ => panic!("expected send or stop after max_turns"),
    }
}
```

**Step 2: Implement max_turns in WorkerState**

Add `max_turns: usize` and `turn_count: usize` to `WorkerState`. Decrement/check after each IoResponse. When `turn_count >= max_turns`, return `Action::Stop(Reason::Normal)` after sending the final result.

**Step 3: Run test, verify pass**

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/orchestration.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): worker max_turns lifespan guard"
```

---

## Task 11: Worker Idle Timeout

**Files:**
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

```rust
#[test]
fn test_worker_idle_timeout_self_terminates() {
    let behavior = WorkerBehavior;
    let mut state = behavior
        .init(serde_json::json!({
            "parent_pid": 0x8000_F001u64,
            "task_id": "t1",
        }))
        .unwrap();

    // Worker receives ReceiveTimeout — should self-terminate
    let action = behavior.handle_message(
        Message::System(SystemMsg::ReceiveTimeout),
        state.as_mut(),
    );
    assert!(matches!(action, Action::Stop(Reason::Shutdown)));
}
```

**Step 2: Implement ReceiveTimeout handling in WorkerBehavior**

Add a match arm in `handle_message` for `SystemMsg::ReceiveTimeout` that returns `Action::Stop(Reason::Shutdown)`.

**Step 3: Run test, verify pass**

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/orchestration.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): worker idle timeout self-termination"
```

---

## Task 12: Scheduler AgentDestroy on terminate_process

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/scheduler.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Write failing test**

```rust
#[test]
fn test_terminate_process_submits_agent_destroy() {
    let (mut handle, _worker) = create_bridge();
    let mut sched = AgentScheduler::new();
    sched.set_bridge(handle);

    // Spawn a dummy process
    let pid = sched.registry.spawn(
        Arc::new(WorkerBehavior),
        serde_json::json!({"parent_pid": 0x8000_0001u64, "task_id": "t"}),
    ).unwrap();
    sched.enqueue(pid);

    // Terminate it
    sched.terminate_process(pid, Reason::Normal);

    // The bridge request channel should contain an AgentDestroy
    // (drain via the worker's request_rx)
    // ... verify AgentDestroy was submitted
}
```

**Step 2: Implement pending_destroys in scheduler**

Add `pending_destroys: Vec<AgentPid>` to `AgentScheduler`. In `terminate_process`, attempt `bridge.submit(pid, IoOp::AgentDestroy { target_pid: pid })`. If submit fails, push to `pending_destroys`. In `tick()`, drain `pending_destroys` with retry (max 3 attempts).

**Step 3: Run test, verify pass**

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/scheduler.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): scheduler submits AgentDestroy on process termination"
```

---

## Task 13: Update Orchestration to Use IoOp::AgentChat

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/orchestration.rs`
- Test: `lib-erlangrt/src/agent_rt/tests.rs`

**Step 1: Update build_llm_request_from_task to return AgentChat**

Change `build_llm_request_from_task` to return `IoOp::AgentChat` instead of `IoOp::LlmRequest`. Add `tools` field extraction from task JSON.

**Step 2: Update existing orchestration tests**

Update `test_worker_run_task_and_send_result` to expect `IoOp::AgentChat` instead of `IoOp::LlmRequest`.

**Step 3: Run all orchestration tests**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt orchestrat 2>&1 | tail -10`
Expected: All pass

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/orchestration.rs lib-erlangrt/src/agent_rt/tests.rs
git commit -m "feat(agent_rt): orchestration uses IoOp::AgentChat instead of LlmRequest"
```

---

## Task 14: Full End-to-End Roundtrip Test

**Files:**
- Test: `lib-erlangrt/src/agent_rt/integration_tests.rs`

**Step 1: Write integration test**

```rust
#[tokio::test]
async fn test_orchestrator_agent_chat_roundtrip() {
    // 1. Create bridge with mock provider registry
    // 2. Create scheduler with bridge
    // 3. Spawn orchestrator process
    // 4. Send goal message
    // 5. Tick scheduler through decomposition -> spawn -> AgentChat -> result
    // 6. Run bridge worker in background
    // 7. Verify orchestrator receives orchestration_complete with results
}
```

This test validates the complete flow: goal → decompose → spawn workers → IoOp::AgentChat → ZeptoAgent::chat() → IoResult → worker_result → orchestration_complete.

**Step 2: Run test, verify pass**

**Step 3: Run ALL tests**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt 2>&1 | tail -15`
Expected: All tests pass (existing + new)

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/integration_tests.rs
git commit -m "test(agent_rt): end-to-end orchestrator -> AgentChat -> ZeptoAgent roundtrip"
```

---

## Task 15: Remove IoOp::LlmRequest (Cleanup)

**Files:**
- Modify: `lib-erlangrt/src/agent_rt/types.rs`
- Modify: `lib-erlangrt/src/agent_rt/bridge.rs` (remove match arm if any)
- Test: verify all tests still pass

`IoOp::LlmRequest` is now fully replaced by `IoOp::AgentChat`. Remove the variant and any dead code referencing it.

**Step 1: Remove LlmRequest variant**

**Step 2: Fix any compiler errors**

**Step 3: Run all tests**

Run: `cd /Users/dr.noranizaahmad/ios/zeptoclaw-rt && cargo test -p erlangrt 2>&1 | tail -15`
Expected: All pass

**Step 4: Commit**

```bash
git add lib-erlangrt/src/agent_rt/types.rs lib-erlangrt/src/agent_rt/bridge.rs
git commit -m "refactor(agent_rt): remove IoOp::LlmRequest, replaced by AgentChat"
```
