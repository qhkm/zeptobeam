> **This repo is an experiment.** We forked [ErlangRT](https://github.com/kvakvs/ErlangRT) and extended it toward AI agent orchestration — sync scheduler, reduction counting, supervision trees, ETS/DETS, hot code upgrades. It got to 343 tests and then hit a wall: the sync scheduler can't `await`, so every LLM call required two thread hops and correlation IDs. Reduction counting models the wrong resource — AI agents don't consume CPU, they consume tokens and dollars. The learnings shaped [ZeptoRT](https://github.com/qhkm/zeptort), which fixes both problems. Active development is there.

---

<div align="center">

# ZeptoBeam

**BEAM-inspired multi-agent runtime in Rust. Fault-tolerant, message-driven, production-ready.**

[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://rust-lang.github.io/rustup/concepts/channels.html)
[![Tests](https://img.shields.io/badge/tests-343%20passing-brightgreen.svg)](#)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</div>

---

## What

ZeptoBeam is the orchestration runtime for [ZeptoClaw](https://github.com/qhkm/zeptoclaw) — a multi-agent AI system where autonomous agents collaborate on complex tasks.

Built on Erlang/BEAM principles, it handles the hard parts of running many agents at once:

- **Process isolation** — Each agent has its own mailbox, state, and lifecycle
- **Fault tolerance** — Crashed agents restart automatically via supervision trees
- **Message passing** — Agents communicate via async messages, no shared state
- **Preemptive scheduling** — Reduction-based preemption with 3-tier priority queues
- **Durable state** — Checkpoints to SQLite, WAL-backed durable mailboxes
- **Hot code upgrades** — Swap agent behaviors at runtime without restart
- **MCP integration** — Expose runtime as MCP server, consume external MCP tools
- **Advanced orchestration** — DAG dependencies, retry policies, resource budgets, approval gates

---

## The Problem

AI agents fail. They hallucinate commands, loop infinitely, exhaust context windows, panic on unexpected inputs. When one agent in a swarm fails, it often takes down the entire workflow.

## The Solution

ZeptoBeam treats failure as normal. When an agent crashes:

1. The supervisor detects it
2. The agent restarts with exponential backoff
3. The other agents keep running
4. The system continues

No 3 AM pages. No cascade failures.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                      SUPERVISION TREE                           │
│                                                                 │
│  ┌─────────────┐     ┌─────────────┐     ┌─────────────┐      │
│  │ Orchestrator │────→│  Supervisor │────→│   Worker    │      │
│  │  (DAG tasks, │     │ (OneForOne/ │     │  (runs LLM  │      │
│  │  budgets)    │     │  backoff)   │     │   agent)    │      │
│  └─────────────┘     └──────┬──────┘     └──────┬──────┘      │
│                              │                   │              │
│                              ↓                   ↓              │
│                        ┌─────────────┐     ┌─────────────┐     │
│                        │   Worker    │     │   Worker    │     │
│                        │ (restarted) │     │ (restarted) │     │
│                        └─────────────┘     └─────────────┘     │
└─────────────────────────────────────────────────────────────────┘
           │                    │                    │
           ↓                    ↓                    ↓
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────────┐
│  BRIDGE (Tokio)  │  │   ETS / DETS    │  │  DURABLE MAILBOX    │
│                  │  │                 │  │                     │
│  ZeptoAgent      │  │  In-memory KV   │  │  WAL-backed queues  │
│  LLM Providers   │  │  Disk-backed    │  │  At-least-once      │
│  MCP Client      │  │  Access control │  │  Ack + truncation   │
└─────────────────┘  └─────────────────┘  └─────────────────────┘
           │
           ↓
┌─────────────────────────────────────────────────────────────────┐
│                      MCP SERVER (HTTP + SSE)                     │
│  spawn / send / kill / metrics / list — JSON-RPC 2.0            │
└─────────────────────────────────────────────────────────────────┘
```

---

## Status

All core phases complete. 343 tests passing.

| Phase | Feature | Status |
|-------|---------|--------|
| 1 | Process runtime (scheduler, mailboxes, links, monitors) | ✅ Done |
| 2 | Orchestration (orchestrator-worker, checkpoint/resume) | ✅ Done |
| 3 | ZeptoAgent integration (multi-turn LLM, tools) | ✅ Done |
| 4 | Reliability (bounded mailboxes, DLQ, chaos testing, backoff) | ✅ Done |
| 5 | Production (config, health server, tracing, signal handling) | ✅ Done |
| 6 | Advanced orchestration (DAG, retry, budgets, approval gates) | ✅ Done |
| 7 | MCP integration (server + client, JSON-RPC, tool discovery) | ✅ Done |
| 9 | BEAM parity (ETS/DETS, durable mailbox, behaviors, hot code, releases) | ✅ Done |

See [ROADMAP.md](docs/ROADMAP.md) for detailed phase descriptions.

---

## Features

### Core Runtime
- **AgentProcess**: Mailbox, reductions, priority (High/Normal/Low), links, monitors
- **AgentScheduler**: 3-tier priority queues, reduction-based preemption, 200 reductions/slice
- **Supervision**: OneForOne, OneForAll, RestForOne with None/Fixed/Exponential backoff
- **Crash isolation**: `catch_unwind` per message dispatch — panics terminate the process, not the runtime

### Orchestration
- **Orchestrator-Worker pattern**: Decompose goals via LLM, spawn worker pool
- **DAG task dependencies**: Cycle detection, dependency resolution, cascade failure
- **Retry policies**: None/Immediate/Backoff/Skip, per-task overrides
- **Resource budgets**: Token count + cost tracking per orchestration
- **Approval gates**: HTTP endpoints for human-in-the-loop sign-off
- **Result aggregation**: Concat, Vote, and Merge strategies

### BEAM Parity
- **ETS tables**: In-memory key-value with Public/Protected/Private access, prefix scan, predicate filter, owner death + heir transfer
- **DETS tables**: Disk-backed with append-log + compaction, crash recovery from `.dat` + `.log`
- **Durable mailboxes**: WAL-backed with fsync protocol, monotonic ack, truncation, stable process identity
- **Behaviors**: GenAgent (call/cast/info), StateMachine (state transitions), EventManager (pub/sub)
- **Hot code upgrades**: Behavior version registry, per-process upgrades, rollback with version history
- **Release handling**: JSON manifest, transactional apply with compensating rollback, crash recovery

### MCP Integration
- **MCP server**: HTTP + SSE transport, runtime tool handlers (spawn, send, kill, metrics)
- **MCP client**: Connect to external tool servers via stdio, tool discovery and invocation
- **JSON-RPC 2.0**: Full MCP protocol lifecycle (initialize, tools/list, tools/call)

### Reliability
- **Bounded mailboxes**: 1024 default capacity, reject-sender on overflow
- **Dead-letter queue**: 256-entry ring buffer for undeliverable messages
- **Fault injection**: Deterministic chaos testing behind feature flag
- **SQLite checkpoints**: WAL mode, upsert, TTL-based pruning

---

## Quick Start

### Prerequisites

- Rust (nightly toolchain)
- (Optional) [ZeptoClaw](https://github.com/qhkm/zeptoclaw) — for running actual LLM agents with tools

```bash
# Install Rust nightly
rustup toolchain install nightly

# Clone and build
git clone https://github.com/qhkm/zeptobeam.git
cd zeptobeam
cargo +nightly build --release -p zeptobeam
```

### Run the Daemon

```bash
# Start with defaults
./target/release/zeptobeam

# Or with custom config
./target/release/zeptobeam -c zeptobeam.toml
```

### Configuration

Create `zeptobeam.toml`:

```toml
[runtime]
worker_count = 4
mailbox_capacity = 1024

[checkpoint]
store = "sqlite"
path = "./zeptobeam.db"
ttl_hours = 24

[server]
enabled = true
bind = "127.0.0.1:9090"

[logging]
level = "info"
format = "pretty"   # pretty | json | compact

[mcp.server]
enabled = true
session_timeout_secs = 3600

[ets]
max_tables = 256
max_entries_per_table = 100000

[durable_mailbox]
enabled = false
wal_dir = "./data/wal"

[hot_code]
quiesce_timeout_ms = 30000

[release]
manifest_dir = "./releases"
max_history = 10
```

### Health & Metrics

```bash
curl http://127.0.0.1:9090/health
curl http://127.0.0.1:9090/metrics
```

### Programmatic Usage

```rust
use zeptobeam::agent_rt::{
    orchestration::OrchestratorBehavior,
    scheduler::AgentScheduler,
    types::*,
};

let mut sched = AgentScheduler::new();

// Spawn an orchestrator with a goal
let behavior = Arc::new(OrchestratorBehavior::default());
let pid = sched.registry.spawn(behavior, json!({
    "goal": "Analyze support tickets and categorize by urgency"
})).unwrap();

// Run the scheduler loop
while sched.tick() {
    // Agents execute, supervised, and checkpoint automatically
}
```

### Run Tests

```bash
cargo +nightly test --lib -p erlangrt   # 343 tests
```

---

## Principles

1. **Isolation > Sharing** — Private state prevents cascade failures
2. **Messages > Calls** — Async communication eliminates deadlocks
3. **Crash > Corrupt** — Restart clean rather than recover from undefined state
4. **Supervise > Defend** — Let supervisors handle restarts, not application code
5. **Checkpoint > Repeat** — Resume progress, don't restart from scratch

---

## Influences

**[Erlang/BEAM](https://www.erlang.org/)** — The "let it crash" philosophy. The BEAM VM runs WhatsApp (2B+ users) and Discord with nine nines of uptime. ZeptoBeam brings BEAM's process isolation, supervision trees, ETS/DETS, hot code upgrades, and release handling to AI agents.

**[OpenAI Symphony](https://github.com/openai/symphony)** — The orchestrator-worker pattern and multi-agent coordination model. Symphony showed how to structure agent swarms; ZeptoBeam adds fault-tolerance.

**[ErlangRT](https://github.com/kvakvs/ErlangRT)** — Originally forked from ErlangRT, an experimental BEAM runtime in Rust by Dmytro Lytovchenko. ZeptoBeam has diverged significantly to focus on AI agent orchestration.

---

## Related

- [ZeptoClaw](https://github.com/qhkm/zeptoclaw) — The agent framework ZeptoBeam orchestrates
- [ROADMAP.md](docs/ROADMAP.md) — Detailed phase descriptions
- [The Zen of Erlang](https://ferd.ca/the-zen-of-erlang.html) — Fault-tolerance philosophy

---

## License

MIT
