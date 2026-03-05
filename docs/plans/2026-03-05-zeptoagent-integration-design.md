# ZeptoAgent Facade Integration Design

## Goal

Integrate the `ZeptoAgent` facade from `~/ios/zeptoclaw` into `zeptoclaw-rt` worker processes so each BEAM-style AgentProcess runs a full zeptoclaw agent with tool access, conversation history isolation, and shared LLM providers.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Dependency mode | Path dependency (`path = "../zeptoclaw"`) | Fast iteration, change later without code changes |
| Tool access | Configurable per-task, full by default | Matches zeptoclaw's DelegateTool role system, controls blast radius |
| Provider config | Per-task override via provider registry | Already half-built in orchestration layer (task has provider/model fields) |
| Conversation mode | Multi-turn, worker stays alive for follow-ups | ZeptoAgent manages own history via `Mutex<Vec<Message>>` |
| Architecture | Bridge-level with agent registry | ZeptoAgent is async-native, belongs in Tokio runtime |
| Reused agent config | Immutable, first call wins | Rebuild requires explicit destroy + recreate |

## Architecture

```
Scheduler Thread (sync)

  OrchestratorBehavior
    - decomposes goal into subtasks
    - spawns WorkerBehavior per subtask
    - sends follow-ups or shutdown

  WorkerBehavior
    - receives "run_task" -> emits IoOp::AgentChat
    - receives IoResponse -> sends result to parent
    - receives "follow_up" -> emits another AgentChat
    - receives "shutdown_worker" -> dies
    - rejects follow_up while awaiting_result (sends "worker_busy")
    - max_turns guard (default 10) + idle timeout (default 30s)

         | IoOp::AgentChat          ^ IoResult
         v (crossbeam)              | (crossbeam)

Bridge Worker (Tokio async)

  AgentRegistry: HashMap<AgentPid, Arc<tokio::sync::Mutex<ZeptoAgent>>>
    - first AgentChat for pid -> build ZeptoAgent
    - subsequent AgentChat -> reuse, call .chat()
    - AgentDestroy -> remove entry (tombstone approach)

  ProviderRegistry: HashMap<String, Arc<dyn LLMProvider + Send + Sync>>
    - "anthropic" -> ClaudeProvider
    - "openai" -> OpenAIProvider
    - configured at startup

  ToolFactory: Arc<dyn ToolFactory + Send + Sync>
    - builds Vec<Box<dyn Tool>> given optional whitelist

  RateLimiter (existing, applies per-provider at HTTP level)
```

## New Types

### IoOp::AgentChat

```rust
IoOp::AgentChat {
    provider: String,              // "anthropic", "openai"
    model: Option<String>,         // override model
    system_prompt: Option<String>,
    prompt: String,                // the user message
    tools: Option<Vec<String>>,    // tool whitelist, None = all
    max_iterations: Option<usize>,
    timeout_ms: Option<u64>,
}
```

### IoOp::AgentDestroy

```rust
IoOp::AgentDestroy {
    target_pid: AgentPid,          // which agent to evict
}
```

## Bridge Changes

### BridgeWorker new fields

```rust
pub struct BridgeWorker {
    request_rx: Receiver<IoRequest>,
    response_tx: Sender<IoResponse>,
    rate_limiter: Option<RateLimiter>,
    agent_registry: Arc<Mutex<HashMap<AgentPid, Arc<tokio::sync::Mutex<ZeptoAgent>>>>>,
    provider_registry: Arc<HashMap<String, Arc<dyn LLMProvider + Send + Sync>>>,
    tool_factory: Arc<dyn ToolFactory + Send + Sync>,
}
```

### AgentChat execution

Spawned as a `tokio::spawn` task (non-blocking bridge loop):

1. Lock `agent_registry`, look up or insert `Arc<tokio::sync::Mutex<ZeptoAgent>>` for pid
2. Release registry lock
3. Lock per-pid mutex (serializes concurrent chats for same pid)
4. Call `agent.chat(&prompt).await`
5. Return `IoResult::Ok(json!({ "response": text }))` or `IoResult::Error`

Wrapped in `catch_unwind(AssertUnwindSafe(...))` — panic returns `IoResult::Error("agent chat panicked: ...")`.

### AgentDestroy execution

1. Lock registry, remove entry for target_pid
2. If in-flight chat holds the Arc, it completes naturally then drops
3. Result sent to response channel but scheduler drops it (pid removed)
4. Return `{ "destroyed": true }` or `{ "destroyed": false }`
5. Idempotent — destroying non-existent pid is safe

### AgentDestroy delivery guarantee

Scheduler keeps `pending_destroys: Vec<AgentPid>`. On `terminate_process`, submit to bridge. If queue full, retry on next tick (3 attempts max). Failures increment `agent_destroy_failures` counter and log at warn level.

## Worker Behavior Changes

### Message types

```
{ "type": "run_task", "task": {...} }       // initial task
{ "type": "follow_up", "prompt": "..." }    // continue conversation
{ "type": "shutdown_worker" }               // terminate
```

### Lifespan guards

- `max_turns: usize` (default 10) — auto-stop after N chat rounds
- Idle timeout via `set_receive_timeout` (default 30s) — self-terminate on ReceiveTimeout
- Both configurable per-task in spawn args

### Busy rejection

Follow-up while `awaiting_result == true` returns `{ "type": "worker_busy" }` to parent. Orchestrator can queue and retry.

## Orchestration Changes

`build_llm_request_from_task` builds `IoOp::AgentChat` instead of `IoOp::LlmRequest`. Forwards provider, model, prompt, system_prompt, plus new tools whitelist.

`terminate_process` submits `IoOp::AgentDestroy` for the dying pid.

## Error Handling

| Failure | Behavior |
|---|---|
| Provider API error (4xx/5xx) | ZeptoAgent::chat() returns Err -> IoResult::Error -> worker sends error to orchestrator |
| Provider timeout | Same (zeptoclaw providers have 120s timeout) |
| Worker panic in handle_message | catch_unwind -> Stop -> terminate_process -> AgentDestroy |
| Bridge queue full on AgentChat | Worker gets error message, stays Runnable |
| ZeptoAgent tool panic | Caught inside chat() — tool errors return as text |
| Bridge-side agent.chat() panic | catch_unwind in spawned task -> IoResult::Error, bridge continues |
| Orchestrator dies | Links cascade -> workers terminate -> AgentDestroy for each |
| Bridge worker dies | Crossbeam disconnects -> workers get error |

## Metrics

Three `AtomicU64` counters on the bridge:

- `agent_destroy_failures` — AgentDestroy submit failures after 3 retries
- `worker_busy_rejections` — follow-ups rejected while chat pending
- `agent_chat_panics` — panics caught in bridge-side chat execution

All logged at warn level.

## Pre-Implementation Checks

1. Align dependency versions (tokio, serde, serde_json) between zeptoclaw and zeptoclaw-rt
2. Ensure spawned bridge tasks only capture Send + Sync + 'static objects
3. Add explicit metrics counters from day one
4. Document immutable-vs-rebuild: first AgentChat sets config, rebuild requires destroy + recreate

## BEAM Isolation Mapping

| BEAM Concept | zeptoclaw-rt Equivalent |
|---|---|
| Process heap | AgentProcess.state + ZeptoAgent.history (per-pid, no sharing) |
| Mailbox | VecDeque<Message> per process |
| Crash isolation | catch_unwind in scheduler + catch_unwind in bridge tasks |
| Death notification | Monitors (SystemMsg::Down), Links (cascade), AgentDestroy (bridge cleanup) |
| Preemption | Reduction counter (200/tick) |
| Shared I/O | Arc<dyn LLMProvider> (like BEAM port/NIF) |

## Testing Strategy

| Test | Validates |
|---|---|
| test_agent_chat_creates_agent_on_first_call | Bridge creates ZeptoAgent on first call, reuses on second |
| test_agent_chat_multi_turn_history | Second chat() sees context from first |
| test_agent_chat_tool_whitelist | Provider only sees whitelisted tools |
| test_agent_destroy_evicts_registry | After destroy, next chat creates fresh agent |
| test_agent_destroy_idempotent | Destroying non-existent pid returns destroyed: false |
| test_agent_destroy_during_inflight_chat | Chat completes, result dropped, no panic |
| test_worker_rejects_followup_while_busy | Follow-up during pending chat returns worker_busy |
| test_worker_max_turns_guard | Worker auto-stops after max_turns |
| test_worker_idle_timeout | Worker self-terminates on timeout |
| test_bridge_panic_containment | Panicking tool returns IoResult::Error, bridge continues |
| test_terminate_sends_agent_destroy | terminate_process submits AgentDestroy |
| test_orchestrator_agent_chat_roundtrip | Full flow: goal -> decompose -> AgentChat -> results |
