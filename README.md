# ZeptoBeam

> **Fault-tolerant AI agent runtime. Built in Rust. Inspired by Erlang.**

[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://rust-lang.github.io/rustup/concepts/channels.html)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

---

## The Problem

AI agents fail. They hallucinate, loop infinitely, hit context limits, panic on bad tool output. In most systems, one failing agent takes down the entire workflow.

## The Solution

ZeptoBeam treats failure as a first-class concept. Each agent runs in an isolated process with:

- **Private state** — No shared memory between agents
- **Message passing** — Async communication only
- **Supervision trees** — Automatic restart on crash with backoff
- **Checkpoint/resume** — State persists to SQLite

When an agent fails, it restarts. The rest of the system keeps running.

---

## Architecture

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

**Process Isolation** — Each agent has its own mailbox, state, and lifecycle. Crash one, the others survive.

**Message Passing** — Agents communicate only through async messages. No blocking RPC, no shared-state corruption.

**Supervision** — Supervisors monitor child agents. On crash: restart (with exponential backoff), escalate if restarts exceed limits, or propagate failure depending on strategy (OneForOne, OneForAll, RestForOne).

**Checkpointing** — Agent state periodically saved to SQLite. Resume from checkpoint after restart instead of starting fresh.

---

## Why Erlang/BEAM?

The BEAM virtual machine runs WhatsApp (2 billion users), Discord, and RabbitMQ with **nine nines of uptime** (99.9999999%).

Their approach: *Let it crash.*

Don't defensively code against every failure. Isolate processes. Restart on failure. Design the system to fail and recover continuously.

ZeptoBeam brings this philosophy to AI agents.

---

## Status

| Component | Status |
|-----------|--------|
| Process runtime (scheduler, mailboxes, links, monitors) | ✅ Complete |
| Supervision trees (all restart strategies, backoff) | ✅ Complete |
| ZeptoAgent integration (multi-turn, tools) | ✅ Complete |
| Reliability (bounded mailboxes, DLQ, chaos testing) | ✅ Complete |
| Production (config, health server, tracing, pruning) | ✅ Complete |
| DAG task dependencies, retry policies, budgets | 📝 Planned |
| MCP (Model Context Protocol) | 📝 Planned |
| Distributed clustering | 📝 Planned |

---

## Quick Start

```bash
git clone https://github.com/qhkm/zeptobeam.git
cd zeptobeam
cargo build --release -p zeptoclaw-rtd
./target/release/zeptoclaw-rtd --help
```

---

## Principles

1. **Isolation > Sharing** — Private state prevents cascade failures
2. **Messages > Calls** — Async communication eliminates deadlocks
3. **Crash > Corrupt** — Restart clean rather than recover from undefined state
4. **Supervise > Defend** — Let supervisors handle restarts, not application code
5. **Checkpoint > Repeat** — Resume progress, don't restart from scratch

---

## Inspiration

- [Erlang/OTP](https://www.erlang.org/) — Fault-tolerant systems
- [The Zen of Erlang](https://ferd.ca/the-zen-of-erlang.html) — *"The only way to make a reliable system is to accept that things will fail."*

---

## License

Apache 2.0
