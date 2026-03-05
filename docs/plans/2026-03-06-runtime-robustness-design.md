# ZeptoBeam Runtime Robustness Design

Date: 2026-03-06

## Context

ZeptoBeam is an orchestration runtime for [ZeptoClaw](https://github.com/qhkm/zeptoclaw) — an ultra-lightweight AI agent written in Rust (~6MB binary, 29 tools, 9 providers, 9 channels). ZeptoBeam wraps ZeptoClaw agents in Erlang/BEAM-inspired processes with supervision, fault tolerance, and message passing.

## Architecture Overview

The project has two distinct subsystems:

### 1. BEAM VM (legacy foundation)

The original ErlangRT codebase — loads and executes `.beam` files compiled from Erlang source. Located in `beam/`, `emulator/`, `term/`.

**Status**: Proof-of-concept. ~86/168 opcodes implemented. 3 BEAM integration tests pass (smoke, test2, test_bs_nostdlib). Recent OTP 28 compatibility work fixed binary matching, compact term endianness, put_tuple2, tuple comparison, and byte_size/bit_size BIF match state handling.

**Relevance to AI agents**: None. The agent runtime does not execute BEAM files. The BEAM VM validates the "BEAM-inspired" claim but is not on the critical path for agent orchestration. No further BEAM VM work is planned unless a specific use case demands it.

### 2. Agent Runtime (`agent_rt/`)

Pure Rust implementation of Erlang's process model for AI agents. This is the production system.

**Core components**:

| Component | File | Lines | Status |
|-----------|------|-------|--------|
| Scheduler | `scheduler.rs` | 800 | Complete — reduction-based preemption, 3-tier priority queues (High/Normal/Low) |
| Process/Types | `types.rs`, `process.rs` | 280 | Complete — AgentPid, Message, Action, AgentBehavior trait |
| Supervision | `supervision.rs` | 350 | Complete — OneForOne/OneForAll/RestForOne, backoff (None/Fixed/Exponential), restart limits |
| Bridge (Tokio I/O) | `bridge.rs` | 820 | Complete — async HTTP, Timer, AgentChat (ZeptoClaw), AgentDestroy, rate limiting |
| Orchestration | `orchestration.rs` | 1190 | Partial — DAG task graph, retry, budgets, approval gates work; goal decomposition is stub |
| MCP Server | `mcp_server.rs` | 820 | Complete — JSON-RPC tools: spawn_orchestration, list_processes, send_message, cancel, metrics |
| HTTP Server | `server.rs` | 1180 | Complete — Axum-based health + MCP HTTP endpoints, SSE, session management |
| GenAgent | `behaviors.rs` | 280 | Complete — gen_server-equivalent with request/notify/info/code_change/terminate |
| Checkpoint | `checkpoint.rs`, `checkpoint_sqlite.rs` | 420 | Complete — InMemory, File, SQLite stores with pruner |
| ETS/DETS | `ets.rs`, `dets.rs` | 1100 | Complete — in-memory and persistent key-value stores |
| Config | `config.rs` | 580 | Complete — TOML config with runtime, server, checkpoint, MCP, logging sections |
| Observability | `observability.rs` | 300 | Complete — RuntimeMetrics, ProcessSnapshot, dead letter queue |
| Hot Code | `hot_code.rs` | 380 | Complete — swap behaviors at runtime |
| Cluster | `cluster.rs` | 470 | Complete — multi-node external routing |
| Tool Factory | `tool_factory.rs` | 220 | Complete — builds ZeptoClaw tool sets with whitelist filtering + MCP remote tools |

**Test coverage**: 342 unit tests + 19 integration tests, all passing.

## Key Decision: BEAM VM vs Agent Runtime

Discussion concluded that fixing BEAM VM stubs (45 `unimplemented!()` calls in `compare.rs`, `arithmetic.rs`, `gc_copying.rs`, etc.) is **not relevant** for the AI agent use case. The agent runtime is a standalone Rust implementation that does not depend on the BEAM VM.

The demand-driven approach applies: only implement BEAM features if a specific test module requires them. Current BEAM tests (smoke, test2, test_bs_nostdlib) all pass.

## What's Working

- Scheduler dispatches messages to processes with reduction counting
- Supervision trees restart crashed agents with backoff
- Bridge submits AgentChat IoOps that build real `ZeptoAgent` instances via `zeptoclaw` crate
- MCP server exposes runtime operations over HTTP JSON-RPC
- Orchestrator decomposes goals into task DAGs, spawns workers, aggregates results
- Checkpoints persist state to SQLite for crash recovery
- Panic containment — `catch_unwind` isolates behavior panics from crashing the scheduler

## What's Missing for Production

### Gap 1: No way to start agents from config

`zeptobeam` binary starts an HTTP/MCP server and waits for shutdown. There's no mechanism to say "on startup, spawn these agents with these providers/tools/prompts." The `zeptobeam.toml` config has `[runtime]`, `[server]`, `[checkpoint]`, `[mcp]`, and `[logging]` sections but no `[[agents]]`.

### Gap 2: Orchestrator decomposition is a stub

When the orchestrator receives a goal, it submits `IoOp::Custom { kind: "decompose_goal" }`. The bridge returns a placeholder response. No actual LLM call decomposes the goal into sub-tasks. This is the critical missing piece for multi-agent workflows.

### Gap 3: No end-to-end test with real ZeptoClaw

The `AgentChat` bridge path (`execute_agent_chat_standalone`) builds a `ZeptoAgent::builder()` and calls `.chat()`, but this has only been tested with mocks. No test sends a real prompt to a real LLM through the full runtime stack: scheduler -> process -> bridge -> ZeptoAgent -> LLM -> response -> process.

### Gap 4: No CLI for one-shot agent execution

Users can't do `zeptobeam agent -m "do something"` to run a single agent through the runtime. The only interface is the HTTP MCP server.

## Design: 3-Phase Robustness Plan

### Phase 1: Config-driven agent spawning

**Goal**: `zeptobeam` spawns agents on startup from config.

Add `[[agents]]` section to `zeptobeam.toml`:

```toml
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
model = "gpt-4o"
system_prompt = "You are a coding assistant."
tools = ["shell", "filesystem", "web_fetch"]
auto_start = false
```

On startup, for each `auto_start = true` agent:
1. Spawn a process with a new `AgentWorkerBehavior`
2. Register it by name in the registry
3. The process waits for messages, sends them as `AgentChat` IoOps

**Deliverables**:
- Config parsing for `[[agents]]`
- `AgentWorkerBehavior` implementation
- Startup wiring in `zeptobeam/src/main.rs`
- Test: config with 2 agents, verify both spawn and are in process list

### Phase 2: Real orchestration decomposition

**Goal**: Orchestrator calls LLM to decompose goals into executable sub-tasks.

Wire `decompose_goal` custom IoOp to:
1. Build a prompt: "Given this goal, decompose into independent sub-tasks. Return JSON array."
2. Submit as `AgentChat` IoOp to the bridge
3. Parse LLM response into task list
4. Feed tasks into the TaskGraph

This requires:
- A decomposition prompt template in config
- Bridge handler for `decompose_goal` that wraps it as `AgentChat`
- Response parser that extracts task definitions from LLM output
- Fallback: if LLM returns unparseable output, treat the entire goal as a single task

**Deliverables**:
- Decomposition bridge handler
- Prompt template system (simple string interpolation)
- Integration test with mock provider
- Error handling for malformed LLM responses

### Phase 3: CLI one-shot and orchestration commands

**Goal**: Run agents and orchestrations from the command line.

```bash
# One-shot agent execution (no server, no config needed)
zeptobeam agent -m "Explain async Rust" --provider openrouter --model anthropic/claude-sonnet-4

# Orchestrated multi-agent execution
zeptobeam orchestrate -m "Build a REST API for todo items" --max-concurrency 3

# Start daemon with agents from config
zeptobeam daemon
```

This requires:
- Clap subcommands: `agent`, `orchestrate`, `daemon` (current behavior)
- `agent` subcommand: creates scheduler + bridge + single process, runs until completion, prints result
- `orchestrate` subcommand: creates scheduler + bridge + orchestrator, runs until all tasks complete

**Deliverables**:
- CLI subcommands
- One-shot execution mode (scheduler runs until process exits)
- Result output formatting (plain text, JSON)
- Integration test for CLI smoke path

## Testing Strategy

Each phase adds tests at two levels:

1. **Unit tests** (in `agent_rt/tests.rs`) — test individual components with mock behaviors
2. **Integration tests** (in `agent_rt/integration_tests.rs`) — test multi-component flows

End-to-end tests with real LLM calls are manual / CI-gated behind API key availability.

## Non-Goals

- Further BEAM VM opcode implementation (not relevant for agents)
- Multi-node clustering (already implemented, not the bottleneck)
- Additional MCP tools (the MCP server already exposes the right primitives)
- Performance optimization (premature — need correctness first)

## Success Criteria

1. `zeptobeam daemon` starts and spawns agents defined in config
2. Sending a message to a spawned agent returns an LLM response
3. `zeptobeam agent -m "hello"` runs a one-shot agent and prints the response
4. `zeptobeam orchestrate -m "research topic X"` decomposes, executes, and aggregates
5. All existing 342+ tests continue to pass
