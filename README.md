<div align="center">

# ZeptoBeam

**Orchestration layer for ZeptoClaw. Fault-tolerant multi-agent runtime built in Rust.**

[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://rust-lang.github.io/rustup/concepts/channels.html)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</div>

---

## What

ZeptoBeam is the orchestration runtime for [ZeptoClaw](https://github.com/qhkm/zeptoclaw) — a multi-agent AI system where autonomous agents collaborate on complex tasks.

It handles the hard parts of running many agents at once:
- **Process isolation** — Each agent runs independently
- **Fault tolerance** — Crashed agents restart automatically
- **Message passing** — Agents communicate via async messages
- **Checkpoint/resume** — State persists to SQLite

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

**Supervision** — Supervisors monitor child agents and restart on crash. Supports OneForOne, OneForAll, and RestForOne restart strategies with exponential backoff.

**Checkpointing** — Agent state periodically saved to SQLite. Resume from checkpoint after restart.

---

## Influences

**[OpenAI Symphony](https://github.com/openai/symphony)** — The orchestrator-worker pattern and multi-agent coordination model. Symphony showed how to structure agent swarms; ZeptoBeam adds fault-tolerance.

**[Erlang/BEAM](https://www.erlang.org/)** — The "let it crash" philosophy. The BEAM VM runs WhatsApp (2B+ users) and Discord with nine nines of uptime. ZeptoBeam brings BEAM's process isolation and supervision trees to AI agents.

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

## Related

- [ZeptoClaw](https://github.com/qhkm/zeptoclaw) — The agent framework ZeptoBeam orchestrates
- [Symphony](https://github.com/openai/symphony) — OpenAI's orchestration system
- [The Zen of Erlang](https://ferd.ca/the-zen-of-erlang.html) — Fault-tolerance philosophy

---

## License

MIT
