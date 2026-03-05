# ZeptoBeam

> **AI agents that crash without killing your system.**

[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://rust-lang.github.io/rustup/concepts/channels.html)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

---

## One Bad Agent, 3 AM

You have 47 agents running. One hallucinates a bad command and panics.

**Most frameworks:** Everything dies. P0 incident. You wake up.

**ZeptoBeam:** That agent restarts. The other 46 keep running.

---

## What Is It?

ZeptoBeam is a **fault-tolerant runtime for AI agents** built on Erlang/BEAM principles:

- **Process-per-agent** — Each agent is isolated with private state and mailbox
- **Supervision trees** — Crashed agents auto-restart with backoff
- **Message passing** — No shared state, no shared corruption
- **Let-it-crash** — Panics are contained, not caught and logged

Think Kubernetes meets BEAM for LLM agents.

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

1. **Orchestrator** decomposes goals into tasks via LLM
2. **Workers** spawn as isolated processes, each with a ZeptoAgent
3. **A worker crashes** → Supervisor detects it → Restarts with backoff
4. **Other workers keep running**. The job completes.

---

## Why?

Erlang/BEAM runs WhatsApp (2B+ users), Discord, and RabbitMQ with **nine nines uptime**. Their philosophy:

> **Let it crash.**

Don't wrap everything in try/catch. Isolate processes. Restart on failure. Design for failure from day one.

**ZeptoBeam brings this to AI agents.**

---

## Core Concepts

| BEAM Concept | ZeptoBeam |
|--------------|-----------|
| Process | Agent process with private mailbox (1024 msg) |
| Supervisor | Restarts crashed agents (OneForOne/OneForAll/RestForOne) |
| Links | Parent-child death notifications |
| Monitors | Watch agents without dying if they crash |
| Preemption | 200 reductions/timeslice (fair scheduling) |
| Let-it-crash | Panic → terminate → restart clean |

---

## Status

🟢 **Production-Ready Core** — Phase 5 Complete

- ✅ Process runtime, scheduler, supervision trees
- ✅ ZeptoAgent integration (multi-turn, tools)
- ✅ Reliability: mailbox bounds, DLQ, SQLite checkpoints, chaos testing
- ✅ Production: TOML config, CLI, health server, tracing
- 📝 Planned: DAG orchestration, MCP, distributed clustering

---

## Quick Start

```bash
git clone https://github.com/qhkm/zeptobeam.git
cd zeptobeam
cargo build --release -p zeptoclaw-rtd
./target/release/zeptoclaw-rtd --help
```

---

## vs Other Frameworks

| | Failure Model | Concurrency | Recovery |
|---|---------------|-------------|----------|
| LangChain | Exceptions | Shared state | Manual retry |
| CrewAI | Try/catch | Thread pool | None built-in |
| Temporal | External | Workflow engine | Replay |
| **ZeptoBeam** | **Let-it-crash** | **Process-per-agent** | **Supervisor restart** |

---

## Principles

1. **Isolation > Sharing** — No shared state, no shared corruption
2. **Messages > Calls** — Async only, no blocking RPC
3. **Crash > Corrupt** — Restart clean, don't recover from impossible
4. **Supervise > Defend** — Let supervisors handle restarts
5. **Checkpoint > Repeat** — Resume from crash, don't restart from scratch

---

## Inspiration

- [Erlang/OTP](https://www.erlang.org/) — Fault-tolerance gold standard
- [The Zen of Erlang](https://ferd.ca/the-zen-of-erlang.html) — *"The only way to make a reliable system is to accept that things will fail."*

---

## License

Apache 2.0

---

> *"If the agent fails, restart it. If it keeps failing, escalate. If everything fails, log and continue."*
