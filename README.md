# ZeptoBeam

> **AI agents that crash without killing your system.**

[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://rust-lang.github.io/rustup/concepts/channels.html)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

---

## One Bad Agent, 3 AM

You have 47 agents running. One hallucinates a bad command and panics.

In most systems: Everything dies. You wake up to a P0.

In ZeptoBeam: That agent restarts. The other 46 keep running.

---

## What Is It?

ZeptoBeam is a **runtime for fault-tolerant AI agents**.

Each agent runs as an isolated process with:
- Private mailbox for messages
- Private state (no shared memory)
- A supervisor that restarts it on crash
- Automatic checkpoint/resume to SQLite

Built in Rust. Inspired by Erlang/BEAM.

---

## The Model

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

**Processes** — Each agent is isolated. If one corrupts its state, it doesn't affect others.

**Messages** — Agents communicate only via async messages. No blocking calls, no deadlocks.

**Supervisors** — When an agent crashes, its supervisor restarts it with exponential backoff. If restarts exceed limits, the supervisor itself fails and escalates.

**Checkpoints** — Agent state is periodically saved to SQLite. Resume after crashes without losing progress.

---

## Why This Approach?

Erlang/BEAM runs WhatsApp (2 billion users), Discord, and RabbitMQ with **nine nines of uptime**.

Their philosophy: *Let it crash.*

Don't defensively program against every failure mode. Isolate failures. Restart clean. Design for failure from day one.

ZeptoBeam applies this to AI agents.

---

## Status

- ✅ **Core runtime** — Process scheduler, mailboxes, links, monitors
- ✅ **Supervision** — OneForOne, OneForAll, RestForOne strategies
- ✅ **Agent integration** — ZeptoAgent with tools, multi-turn conversations
- ✅ **Reliability** — Bounded mailboxes, dead-letter queue, chaos testing
- ✅ **Production** — TOML config, health server, tracing, checkpoint pruning
- 📝 **Planned** — DAG task dependencies, MCP protocol, distributed clustering

---

## Quick Start

```bash
git clone https://github.com/qhkm/zeptobeam.git
cd zeptobeam
cargo build --release -p zeptoclaw-rtd
./target/release/zeptoclaw-rtd --help
```

---

## Inspiration

- [Erlang/OTP](https://www.erlang.org/) — The gold standard for fault-tolerant systems
- [The Zen of Erlang](https://ferd.ca/the-zen-of-erlang.html) — *"The only way to make a reliable system is to accept that things will fail."*

---

## License

Apache 2.0

---

> *"If the agent fails, restart it. If it keeps failing, escalate. If everything fails, log and continue."*
