# ZeptoBeam

> **AI agents that don't fall over.**

[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://rust-lang.github.io/rustup/concepts/channels.html)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

---

## The Problem

You're running 47 AI agents in production. One of them:
- Hallucinates a command and `rm -rf`s the wrong directory
- Gets stuck in an infinite tool-calling loop
- Hits a context window limit and starts spewing gibberish
- Encounters a malformed API response and panics

**What happens?**

In most frameworks: *Everything dies. Your orchestration graph collapses. Alerts fire at 3 AM. You wake up to a P0 incident.*

In ZeptoBeam: *That one agent restarts. The other 46 keep running. You check the logs in the morning.*

---

## The Insight

Erlang solved this in 1986.

The BEAM virtual machine runs WhatsApp (2+ billion users), Discord, and RabbitMQ with **nine nines of uptime** (99.9999999%). Their secret? 

> **Let it crash.**

Instead of defensive programming — try/catch blocks, retry loops, circuit breakers everywhere — Erlang embraces failure:
- Processes are isolated (no shared memory)
- If one crashes, a supervisor restarts it
- Message passing is the only communication
- Systems are designed to fail and recover continuously

**ZeptoBeam brings this philosophy to AI agents.**

---

## What Is It?

ZeptoBeam is a runtime for building **fault-tolerant systems of AI agents**. Think of it as:

- **Kubernetes meets BEAM for LLM agents** — Automatic restarts, supervision trees, resource isolation
- **Process-per-agent** — Each agent runs in its own lightweight "process" with private state and mailbox
- **Message-passing only** — No shared state between agents; all coordination via async messages
- **Checkpoint/resume** — Agent state persists to SQLite; resume after crashes or restarts
- **Tool sandboxing** — Each task specifies which tools the agent can access

---

## How It Works

```
┌─────────────────────────────────────────────────────────────────┐
│                        SUPERVISION TREE                          │
│                                                                  │
│  ┌─────────────┐     ┌─────────────┐     ┌─────────────┐       │
│  │ Orchestrator│────→│  Supervisor │────→│   Worker    │       │
│  │  (manages   │     │ (restarts   │     │  (runs LLM  │       │
│  │   workflow) │     │  on crash)  │     │   agent)    │       │
│  └─────────────┘     └──────┬──────┘     └──────┬──────┘       │
│                             │                   │              │
│                             ↓                   ↓              │
│                       ┌─────────────┐     ┌─────────────┐       │
│                       │   Worker    │     │   Worker    │       │
│                       │ (restarted) │     │ (restarted) │       │
│                       └─────────────┘     └─────────────┘       │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ↓
┌─────────────────────────────────────────────────────────────────┐
│                        BRIDGE (Tokio)                            │
│                                                                  │
│   ┌─────────────┐    ┌─────────────┐    ┌─────────────┐        │
│   │ZeptoAgent   │    │   Claude    │    │   OpenAI    │        │
│   │  Registry   │    │  Provider   │    │  Provider   │        │
│   └─────────────┘    └─────────────┘    └─────────────┘        │
└─────────────────────────────────────────────────────────────────┘
```

### A Day in the Life

1. **Goal arrives**: "Research competitors and draft a blog post"
2. **Orchestrator** (an agent process) decomposes this via LLM into subtasks
3. **Workers** spawn — each is a separate process with its own:
   - ZeptoAgent instance (conversation history, tool access)
   - Mailbox for messages
   - Supervision link to parent
4. **A worker crashes** mid-task (network timeout? bad tool output?)
   - Supervisor detects the crash via `DOWN` message
   - Restarts the worker with exponential backoff
   - Orchestrator retries the task or marks it failed
   - Other 12 workers keep running unaffected
5. **System stays up**. The blog post gets written.

---

## Core Concepts

| BEAM (Erlang) | ZeptoBeam (AI Agents) |
|---------------|----------------------|
| Process | Agent process with ZeptoAgent instance |
| Mailbox | Bounded queue (1024 messages) for task/follow-up messages |
| Supervisor | Restarts crashed agents (OneForOne/OneForAll/RestForOne) |
| Let-it-crash | Panic → terminate → restart clean |
| Links | Parent-child bidirectional death notification |
| Monitors | Watch agents without being killed if they die |
| Preemption | 200 "reductions" per timeslice (fair scheduling) |
| Hot code reload | Update agent behavior without stopping system |

---

## Project Status

🚧 **Production-Ready Core** — Phase 5 Complete

- [x] **Process runtime** — Scheduler, registry, mailboxes, links, monitors
- [x] **Supervision trees** — All 3 restart strategies, exponential backoff, escalation
- [x] **Bridge** — Tokio-based async I/O with cancellation tokens
- [x] **ZeptoAgent integration** — Real LLM agents with tools, multi-turn conversations
- [x] **Reliability** — Mailbox bounds, dead-letter queue, SQLite checkpoints, chaos testing
- [x] **Production** — Config (TOML), CLI, health server, logging, checkpoint pruning
- [ ] **Advanced Orchestration** — DAG dependencies, retry policies, resource budgets, human-in-the-loop
- [ ] **MCP Integration** — Model Context Protocol server/consumer
- [ ] **Distributed Clustering** — Multi-node agent swarms via gRPC

---

## Quick Start

```bash
# Clone and setup
git clone https://github.com/qhkm/zeptobeam.git
cd zeptobeam

# Build (requires Rust nightly)
cargo build --release -p zeptoclaw-rtd

# Run the daemon
./target/release/zeptoclaw-rtd --help

# Or run with custom config
cargo run -p zeptoclaw-rtd -- -c zeptoclaw-rt.toml
```

### Example: Spawn an Orchestration

```rust
use zeptobeam::agent_rt::{
    orchestration::{OrchestratorBehavior, WorkerBehavior},
    scheduler::AgentScheduler,
    types::*,
};

let mut sched = AgentScheduler::new();

// Start an orchestrator that manages workers
let behavior = Arc::new(OrchestratorBehavior {
    max_concurrency: 4,
    checkpoint_store: Some(sqlite_store),
});

let pid = sched.registry.spawn(behavior, json!({
    "goal": "Analyze these 100 support tickets and categorize them"
})).unwrap();

// Run the scheduler loop
while sched.tick() {
    // Agents are executing, supervised, and checkpointing
}
```

---

## Why Not Just Use [Other Framework]?

| Framework | Failure Model | Concurrency | Recovery |
|-----------|--------------|-------------|----------|
| LangChain | Exception-based | Shared state | Manual retry loops |
| CrewAI | Try/catch | Thread pool | None built-in |
| AutoGPT | Single process | Sequential | Restart everything |
| Temporal | External orchestration | Workflow engine | Replay from events |
| **ZeptoBeam** | **Let-it-crash** | **Process-per-agent** | **Supervisor restart** |

ZeptoBeam isn't higher-level — it's *lower-level* in the right ways. It gives you primitives (processes, mailboxes, supervisors) to build reliable systems, not opinionated workflows that break when reality gets messy.

---

## Architecture Principles

1. **Isolation > Sharing** — Each agent has its own memory space. No shared state means no shared corruption.

2. **Messages > Calls** — Async message passing is the only communication. No blocking RPC that can deadlock.

3. **Crash > Corrupt** — If an agent enters an invalid state, panic and restart. Don't try to recover from the impossible.

4. **Supervise > Defend** — Supervisors handle restarts with backoff. Application code doesn't clutter itself with retry logic.

5. **Checkpoint > Repeat** — State is periodically saved. Resume from crash rather than restarting from scratch.

---

## The Name

**Zepto** (10⁻²¹) — Extremely small, lightweight processes  
**Beam** — The Erlang/OTP virtual machine that inspired this design

---

## Inspiration

- [Erlang/OTP](https://www.erlang.org/) — The gold standard for fault-tolerant systems
- [Elixir](https://elixir-lang.org/) — Modern syntax, same BEAM superpowers
- [BEAM Wisdoms](http://beam-wisdoms.clau.se/) — Deep BEAM internals
- [The Zen of Erlang](https://ferd.ca/the-zen-of-erlang.html) — *"The only way to make a reliable system is to accept that things will fail."*

---

## Contributing

See [CONTRIBUTING.rst](CONTRIBUTING.rst). This is early-stage — style fixes, bug fixes, and design discussions welcome.

---

## License

Apache 2.0 — See [LICENSE](LICENSE).

---

> *"If the agent fails, restart it. If it keeps failing, escalate. If everything fails, log and continue."*
>
> — The Zen of ZeptoBeam
