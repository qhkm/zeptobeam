# ZeptoBeam

> **Fault-tolerant AI agent systems inspired by the BEAM.**

[![Rust](https://img.shields.io/badge/rust-nightly-orange.svg)](https://rust-lang.github.io/rustup/concepts/channels.html)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

ZeptoBeam is an experimental runtime for building **fault-tolerant systems of AI agents**. It takes inspiration from Erlang/BEAM's legendary reliability patterns — let-it-crash supervision, lightweight processes, and message-passing concurrency — and applies them to the world of autonomous agents.

Rather than treating AI agents as monolithic black boxes, ZeptoBeam models them as lightweight, isolated, supervised processes that can fail, restart, and cooperate without bringing down the entire system.

---

## Why?

AI agents are inherently unpredictable. They:
- Hallucinate and make wrong decisions
- Get stuck in loops
- Fail on malformed tool outputs
- Exhaust context windows
- Crash on unexpected inputs

**Current approaches:** Wrap everything in try/catch and hope for the best.

**The ZeptoBeam approach:** Embrace failure as a first-class concept. Isolate agents. Supervise them. Let them crash and restart clean. Communicate via messages, not shared state.

> *"If the agent fails, restart it. If it keeps failing, escalate. If everything fails, log and continue."* — The Zen of Erlang, adapted.

---

## Core Concepts

| BEAM Concept | ZeptoBeam Adaptation |
|--------------|---------------------|
| **Processes** | Lightweight agent instances with isolated state |
| **Message Passing** | Async communication between agents (no shared memory) |
| **Supervision Trees** | Hierarchical fault tolerance — parent agents monitor children |
| **Let-it-Crash** | Agents that error are terminated and restarted clean |
| **Hot Code Upgrading** | Update agent behavior without stopping the system |
| **OTP Behaviors** | Reusable agent patterns (gen_agent, supervisor, etc.) |

---

## Project Status

🚧 **Early Proof-of-Concept**

- [x] Core VM with process isolation
- [x] BEAM bytecode loader (can load `.beam` files)
- [x] ~45% BEAM opcode coverage (74 of 168)
- [x] Lightweight process scheduler
- [x] Message-passing mailbox system
- [ ] Agent-specific OTP behaviors
- [ ] LLM tool-calling integration
- [ ] Distributed agent clusters
- [ ] Supervision tree implementation

---

## Quick Start

Requires **Rust nightly** (uses `#![feature(ptr_metadata)]`).

```bash
# Clone and setup
git clone https://github.com/qhkm/zeptobeam.git
cd zeptobeam

# Initialize OTP submodule (needed for BEAM files)
make otp

# Build the runtime
make build

# Run tests
make test

# Run the emulator
make run
```

---

## Architecture

```
┌─────────────────────────────────────────────┐
│           Supervisor Tree Layer             │
│    (monitors and restarts failed agents)    │
├─────────────────────────────────────────────┤
│           Agent Process Layer               │
│   (lightweight, isolated, message-passing)  │
├─────────────────────────────────────────────┤
│              VM Core (Rust)                 │
│  • Process scheduler (round-robin, 200 reds)│
│  • Heap-per-process with copying GC         │
│  • BEAM bytecode interpreter                │
│  • BIFs for agent lifecycle management      │
└─────────────────────────────────────────────┘
```

---

## What Makes This Different?

| Traditional AI Agents | ZeptoBeam Agents |
|----------------------|------------------|
| Monolithic, shared state | Isolated processes with private heaps |
| Crash = system down | Crash = supervisor intervenes |
| Synchronous, blocking calls | Async message passing |
| Restart whole system on failure | Granular per-agent restart |
| Hard to compose | Tree-structured supervision hierarchies |

---

## Inspiration

- **Erlang/OTP** — The gold standard for fault-tolerant systems
- **Elixir** — Modern syntax, same BEAM superpowers  
- **BEAM Wisdoms** — Deep BEAM internals ([beam-wisdoms.clau.se](http://beam-wisdoms.clau.se/))
- **The Zen of Erlang** — [ferd.ca](https://ferd.ca/the-zen-of-erlang.html)

---

## Contributing

See [CONTRIBUTING.rst](CONTRIBUTING.rst).

---

## License

Apache 2.0 — See [LICENSE](LICENSE).

---

> *"The only way to make a reliable system is to accept that things will fail."*
