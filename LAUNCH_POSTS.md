# ZeptoBeam Launch Posts

**Launch Date:** 2024
**GitHub:** https://github.com/qhkm/zeptobeam
**Tagline:** Fault-tolerant AI agent runtime. Built in Rust. Inspired by Erlang.

---

## Hacker News (Show HN)

**Title:** Show HN: ZeptoBeam – Fault-tolerant runtime for AI agent swarms

---

Hey HN,

I built ZeptoBeam, a runtime for running AI agent swarms without them falling over.

**The Problem:**

AI agents fail. They hallucinate commands, loop infinitely, hit context limits. When one agent in a swarm fails, most frameworks take down the entire workflow.

**The Solution:**

ZeptoBeam treats failure as normal. Each agent runs in an isolated process with:
- Private mailbox (bounded, 1024 messages)
- Private state (no shared memory)
- A supervisor that auto-restarts on crash
- Checkpoint/resume to SQLite

When an agent crashes, it restarts. The other 46 agents keep running.

**Influences:**

**OpenAI Symphony** — The orchestrator-worker pattern for multi-agent coordination. Symphony showed how to structure agent swarms. ZeptoBeam adds fault-tolerance.

**Erlang/BEAM** — The BEAM VM runs WhatsApp (2B+ users) and Discord with nine nines of uptime. Their philosophy: "Let it crash." Don't defensively code against every failure. Isolate processes. Restart on failure.

ZeptoBeam brings both together: Symphony's orchestration model + BEAM's fault-tolerance.

**Architecture:**

- Process-per-agent with reduction-counting scheduler
- Supervision trees: OneForOne, OneForAll, RestForOne
- Tokio-based bridge for async I/O
- MCP (Model Context Protocol) support for external tools

**Tech Stack:**

- Rust (nightly)
- Tokio for async runtime
- SQLite for checkpoints
- Axum for health/metrics server

**Current Status:**

- ✅ Core runtime, scheduler, supervision
- ✅ ZeptoAgent integration (multi-turn, tools)
- ✅ Production ready (config, health server, tracing)
- ✅ MCP server/client (bidirectional)
- 📝 Planned: DAG orchestration, distributed clustering

**GitHub:** https://github.com/qhkm/zeptobeam

Happy to discuss the implementation. The supervision tree implementation was particularly interesting to get right.

---

## Twitter/X Thread

**Tweet 1 (Hook):**

You have 47 AI agents running in production.

One hallucinates a bad command and panics.

Most frameworks: Everything dies. You wake up to a P0.

ZeptoBeam: That agent restarts. The other 46 keep running.

https://github.com/qhkm/zeptobeam

🧵 Here's how:

---

**Tweet 2 (The Philosophy):**

Erlang/BEAM solved this in 1986.

WhatsApp runs 2 billion users on it. Nine nines of uptime.

Their secret? "Let it crash."

Don't wrap everything in try/catch. Isolate processes. Restart on failure.

ZeptoBeam brings this philosophy to AI agents.

---

**Tweet 3 (The Model):**

ZeptoBeam is a runtime, not a framework.

→ Process-per-agent (isolated state)
→ Message passing only (no shared memory)
→ Supervision trees (auto-restart on crash)
→ Checkpoint/resume (SQLite persistence)

Build your agents however you want. ZeptoBeam handles the hard parts.

---

**Tweet 4 (Architecture):**

Built in Rust:

• Reduction-counting scheduler (fair preemption)
• 3-tier priority queues (High/Normal/Low)
• Bounded mailboxes (backpressure)
• Tokio bridge for async I/O
• MCP support for external tools

Open source. MIT license.

---

**Tweet 5 (Use Case):**

Multi-agent orchestration:

1. Orchestrator decomposes goal via LLM
2. Spawns worker agents (each isolated)
3. One worker crashes mid-task
4. Supervisor restarts it
5. Other workers continue
6. Job completes

No cascade failures. No 3 AM pages.

---

**Tweet 6 (Status):**

Production-ready:

✅ Core runtime & scheduler
✅ Supervision trees (all strategies)
✅ ZeptoAgent integration
✅ MCP server/client
✅ Config, health server, tracing

📝 Planned: DAG orchestration, distributed clustering

---

**Tweet 7 (CTA):**

ZeptoBeam is open source.

If you're building multi-agent systems and tired of them falling over, check it out.

https://github.com/qhkm/zeptobeam

What's your experience with agent reliability? 👇

---

## Reddit r/SideProject

**Title:** I was tired of AI agent swarms falling over, so I built a fault-tolerant runtime inspired by WhatsApp's architecture

---

Hey r/SideProject,

Wanted to share a project I just launched: **ZeptoBeam** — a runtime for fault-tolerant AI agent swarms.

### The Problem

I was building multi-agent systems for automation workflows. One agent would:
- Hallucinate a command and crash
- Get stuck in an infinite loop
- Hit a context window limit

And the entire workflow would die. Existing frameworks (LangChain, CrewAI) use shared-state threads with try/catch. When something breaks, everything breaks.

### The Insight

Erlang/BEAM solved this in 1986. WhatsApp runs 2B+ users on it. Discord. RabbitMQ. Nine nines of uptime.

Their philosophy: *"Let it crash."*

Don't defensively code against every failure. Isolate processes. Design for failure from day one.

### The Solution

ZeptoBeam brings BEAM's architecture to AI agents:

1. **Process-per-agent** — Each agent has private state and mailbox
2. **Message passing** — No shared memory, no shared corruption
3. **Supervision trees** — Auto-restart on crash with backoff
4. **Checkpoint/resume** — State persists to SQLite

When an agent fails, it restarts. The others keep running.

### Tech Stack

- **Language:** Rust (nightly)
- **Async runtime:** Tokio
- **Process model:** BEAM-inspired (reduction counting, mailboxes)
- **Persistence:** SQLite (WAL mode)
- **Health server:** Axum

### What I Learned

1. **Supervision is harder than it looks** — Getting the restart intensity limits and backoff strategies right took multiple iterations
2. **Message passing changes everything** — No blocking RPC means no deadlocks, but it requires rethinking how agents coordinate
3. **Checkpoints are underrated** — Being able to resume a crashed agent from where it left off is a game-changer

### Current Status

- ✅ Core runtime, scheduler, supervision trees
- ✅ ZeptoAgent integration (multi-turn LLM agents)
- ✅ MCP support (Model Context Protocol)
- ✅ Production ready (config, health server, tracing)

Open source, MIT license.

**GitHub:** https://github.com/qhkm/zeptobeam

Happy to answer questions about the tech, BEAM internals, or agent orchestration!

What's your experience building reliable agent systems?

---

## LinkedIn

**Post:**

I just open-sourced ZeptoBeam — a fault-tolerant runtime for AI agent swarms.

**The business problem:**

Enterprises are deploying AI agent swarms for automation, but reliability is a major blocker. When one agent fails, it often cascades and takes down the entire workflow. This means:
- 3 AM pages for engineering teams
- Lost work and failed automations
- Hesitation to deploy agents in production

**The technical insight:**

Two sources:

1. **OpenAI Symphony** — Demonstrated the orchestrator-worker pattern for multi-agent systems
2. **Erlang/BEAM** — Solved distributed systems reliability decades ago (WhatsApp: 2B+ users, Discord, RabbitMQ — all nine nines uptime)

The insight: "Let it crash." Don't try to handle every failure case. Isolate processes. Restart on failure. Design for failure as the normal case.

**ZeptoBeam brings this to AI agents:**

→ Process-per-agent isolation
→ Supervision trees with automatic restart
→ Checkpoint/resume for state persistence
→ Message-passing architecture (no shared state)

The result: When an agent crashes, it restarts. The other agents keep running. The business process continues.

**Built in Rust** with production-readiness in mind:
- TOML configuration
- Health and metrics endpoints
- Structured logging with tracing
- MCP (Model Context Protocol) for external tool integration

**GitHub:** https://github.com/qhkm/zeptobeam

If you're building or deploying AI agents at scale, I'd love to hear about your reliability challenges. What's your current approach to handling agent failures?

---

## Post Schedule

| Platform | Day | Time (EST) | Status |
|----------|-----|------------|--------|
| Hacker News | Tuesday | 8:00 AM | Draft ready |
| Twitter/X | Tuesday | 9:00 AM | Draft ready |
| r/SideProject | Wednesday | 9:00 AM | Draft ready |
| LinkedIn | Thursday | 9:00 AM | Draft ready |

**Note:** Space posts 24-48 hours apart to avoid looking spammy.

---

## FAQ Prep

**Q: How is this different from LangChain/CrewAI?**
A: ZeptoBeam is a runtime, not a framework. It provides process isolation and supervision trees, not high-level agent APIs. Inspired by OpenAI Symphony's orchestration model + Erlang/BEAM's fault-tolerance.

**Q: What's the relationship to OpenAI Symphony?**
A: Symphony inspired the orchestrator-worker pattern we use. ZeptoBeam adds fault-tolerance (supervision trees, process isolation) that Symphony doesn't have.

**Q: Why Rust instead of Elixir?**
A: Rust gives us fine-grained control over memory and performance while still being able to implement BEAM's process model. Also, the AI ecosystem is heavily Rust/Python.

**Q: Is this production-ready?**
A: The core runtime is. We have config loading, health servers, tracing, and checkpointing. Distributed clustering is still planned.

**Q: What is MCP?**
A: Model Context Protocol — an open standard for connecting AI agents to external tools and data sources. ZeptoBeam can act as both an MCP server and client.

**Q: How do I get started?**
A: Clone the repo, build with `cargo build --release -p zeptobeam`, and run the daemon. Check the README for config examples.

---

## Short Copy Versions

**One-liner:**
ZeptoBeam — AI agents that crash without killing your system. Built in Rust, inspired by Erlang.

**Twitter bio:**
Fault-tolerant runtime for AI agent swarms. Process isolation + supervision trees. Open source Rust.

**Tagline options:**
1. AI agents that crash without killing your system
2. Fault-tolerant runtime for AI agent swarms
3. Symphony-style orchestration + BEAM-style reliability
