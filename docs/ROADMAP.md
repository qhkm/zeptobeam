# zeptoclaw-rt Roadmap

BEAM-inspired agent runtime for zeptoclaw. Built in layered phases — each phase is independently useful and tested before the next begins.

---

## Phase 1: Core Runtime [DONE]

Foundation: process model, scheduler, registry, bridge, supervision trees.

- AgentProcess with mailbox, reductions, priority, links, monitors
- AgentScheduler with 3-tier priority queues (High > Normal > Low), reduction-based preemption
- AgentRegistry with spawn, lookup, named registration
- Tokio bridge with bounded crossbeam channels for async I/O
- Supervision trees: OneForOne, OneForAll, RestForOne strategies
- Crash isolation via catch_unwind in message dispatch
- trap_exit, cascading death, exit signal handling
- Receive timeouts, monitors, demonitor

**Commits:** `de3a177` through `cd6067b`

---

## Phase 2: Orchestration Layer [DONE]

Swarm orchestration: orchestrator + worker pool, LLM task decomposition, checkpoint/resume.

- OrchestratorBehavior: receives goal, decomposes via LLM, spawns workers up to max_concurrency
- WorkerBehavior: receives task, calls LLM, sends result to parent, multi-turn follow-up
- Checkpoint/resume with pluggable CheckpointStore trait (InMemory, File)
- Runtime observability: RuntimeMetrics (messages, IO, latency, terminations)
- Graceful shutdown with bounded drain
- Cluster module: partitioned scheduler routing, shared bridge worker pool

**Commits:** `fb58638` through `6af545f`

---

## Phase 3: ZeptoAgent Integration [DONE]

Wire real zeptoclaw ZeptoAgent facade into the bridge for live LLM agent execution.

- IoOp::AgentChat and IoOp::AgentDestroy variants
- Bridge agent registry: HashMap<AgentPid, Arc<TokioMutex<ZeptoAgent>>>
- Provider registry: per-provider Arc<dyn LLMProvider> sharing across agents
- ToolFactory trait with DefaultToolFactory (filesystem, shell, git, web tools)
- Panic containment via tokio::task::spawn for agent chat
- BridgeMetrics: destroy_failures, busy_rejections, chat_panics
- Worker busy rejection, max_turns guard, idle timeout
- End-to-end orchestrator -> AgentChat -> ZeptoAgent roundtrip test

**16 tasks, 11 commits:** `c8ebd6e` through `e9910fa`

---

## Phase 4: Reliability Hardening [DONE]

BEAM-level reliability patterns across 7 areas.

- **Mailbox bounds**: 1024 default capacity, MailboxFull rejection, backpressure signaling
- **Supervisor backoff**: BackoffStrategy enum (None/Fixed/Exponential), per-child restart tracking, escalation on intensity exceeded
- **Timeouts/cancellation**: Default 120s timeout on all AgentChat, CancellationToken per-PID, orphan prevention on process termination
- **Dead-letter queue**: 256-entry ring buffer, routes MailboxFull + ProcessNotFound, total_count metric
- **Durable recovery**: SqliteCheckpointStore with WAL mode, upsert with created_at preservation
- **Fault injection**: FaultConfig with deterministic seeded RNG, FaultyCheckpointStore, ChaosConfig behind chaos_testing feature flag
- **Crash isolation verification**: Tests proving panics don't affect siblings
- **Observability**: Structured tracing spans on terminate, supervisor events

**18 tasks, 146 tests:** `106fe9c` through `bd957a4`

---

## Phase 5: Production Readiness [DONE]

Make the runtime deployable as a standalone service.

- **Configuration**: TOML config file loading with AppConfig, RuntimeConfig, CheckpointConfig, ServerConfig, LogConfig
- **CLI entry point**: `zeptoclaw-rtd` binary with clap arg parsing, config file override, log level, worker count, bind address
- **Structured errors**: AgentRtError enum with thiserror, replacing String errors in CheckpointStore and related modules
- **Health server**: Axum HTTP server with /health and /metrics endpoints, graceful shutdown
- **Logging setup**: tracing-subscriber with JSON/pretty/compact formats, env-filter, configurable levels
- **FileCheckpointStore tests**: 6 tests covering roundtrip, overwrite, delete, missing key, sanitization, atomic write
- **Checkpoint pruning**: list_keys and prune_before on CheckpointStore trait, background pruner task with configurable interval and TTL
- **Signal handling**: SIGTERM/SIGINT graceful shutdown, drain in-flight ops, stop server

**Commits:** `e9256a2` through `5869ea8`

---

## Phase 6: Advanced Orchestration [DONE]

Sophisticated multi-agent coordination patterns.

- **DAG task dependencies**: TaskGraph with cycle detection, dependency resolution, cascade failure
- **Retry policies**: RetryStrategy enum (None/Immediate/Backoff/Skip), per-task overrides via task JSON
- **Resource budgets**: Token count + cost tracking per orchestration, configurable per-model pricing
- **Approval gates**: ApprovalRegistry with HTTP endpoints (GET /approvals, POST /approve/:orch_id/:task_id), AwaitingApproval status
- **Streaming results**: worker_progress messages, partial_results storage, forwarding to parent orchestrators
- **Hierarchical orchestrators**: sub_orchestration task type, recursive OrchestratorBehavior spawning, depth limiting
- **Result aggregation**: ResultAggregator trait with ConcatAggregator, VoteAggregator, MergeAggregator

**12 tasks, 48 tests:** `3e91307` through `70fb607`

---

## Phase 7: MCP Integration [PLANNED]

Expose the runtime as an MCP server and consume MCP tools natively.

- **MCP server mode**: Expose runtime as Model Context Protocol server — external agents can spawn processes, send messages, read metrics
- **MCP tool consumer**: Agents natively discover and call MCP tool servers (filesystem, databases, APIs)
- **Tool discovery**: Runtime-level MCP tool registry — agents share discovered tools
- **Session management**: Map MCP sessions to agent process lifecycles
- **Transport**: stdio and SSE transports for MCP protocol
- **Authentication**: Token-based auth for MCP server endpoints

**Prerequisite:** Phase 5 (HTTP server for SSE transport), Phase 3 (ToolFactory already supports pluggable tools).

---

## Phase 8: Multi-Node Clustering [PLANNED]

Distribute agents across multiple runtime nodes.

- **Network transport**: gRPC (tonic) inter-node communication for message passing
- **Node discovery**: Static config node registry
- **Remote spawn**: Spawn processes on remote nodes, transparent PID routing
- **Partition rebalancing**: Migrate agent processes between nodes on load imbalance
- **Distributed supervision**: Supervisors that span nodes, restart on remote node failure
- **Cross-node dead-letter routing**: Forward undeliverable messages to origin node's DLQ
- **Split-brain resolution**: Leader-based consistency for shared state (agent registry, checkpoints)

**Prerequisite:** Phase 5 (production readiness), Phase 7 (MCP transport layer reusable for node communication).

---

## Phase Summary

| Phase | Name | Status | Tests |
|-------|------|--------|-------|
| 1 | Core Runtime | Done | ~60 |
| 2 | Orchestration Layer | Done | ~100 |
| 3 | ZeptoAgent Integration | Done | ~120 |
| 4 | Reliability Hardening | Done | 146 |
| 5 | Production Readiness | Done | ~20 |
| 6 | Advanced Orchestration | Done | 48 |
| 7 | MCP Integration | Planned | — |
| 8 | Multi-Node Clustering | Planned | — |
| 9 | BEAM Parity | Done | ~63 |

---

## Phase 9: BEAM Parity [DONE]

True Erlang/BEAM-inspired features that make the runtime uniquely powerful.

- **ETS/DETS Tables**: In-memory key-value with access control (Public/Protected/Private), prefix scan, predicate filter. Disk-backed DETS with append-log + compaction. Owner death cleanup with heir transfer
- **Persistent Message Queues**: Opt-in WAL-backed durable mailboxes with at-least-once delivery, strict fsync protocol, ack-based truncation, stable process identity
- **Behaviors Framework**: GenAgent (call/cast/info), StateMachine (state transitions), EventManager (pub/sub) — adapted for AI agents, wrapping AgentBehavior
- **Hot Code Upgrades**: Behavior version registry, per-process and per-type upgrades, rollback with version history, stale version detection
- **Release Handling**: JSON manifest-based releases with transactional apply, compensating rollback, crash recovery via persisted in-progress state

**12 tasks, ~63 tests:** `4a7580d` through `81d25cb`

---

# Enterprise Feature Track

The following features are designated as **Enterprise** — they extend the core runtime for production deployments at scale but are not required for the fundamental BEAM-inspired agent experience.

## Enterprise Phase E1: Cloud Native

- Kubernetes Operator with CRDs for AgentRuntime, AgentPool
- Horizontal Pod Autoscaler (HPA) integration
- GitOps workflows (ArgoCD/Flux)
- Helm charts and Terraform modules
- Multi-tenancy with namespace isolation
- Resource quotas and per-tenant rate limiting
- Cost optimization: token budgeting, model tier routing, spot instance support

## Enterprise Phase E2: Security & Compliance

- Zero-trust architecture with mTLS between nodes
- OIDC/OAuth2 authentication and RBAC
- Audit logging and tamper-proof log shipping
- PII detection and automatic redaction
- Data residency controls and GDPR deletion APIs
- Wasm-based agent sandboxing
- FIPS 140-2 compliance mode

## Enterprise Phase E3: Advanced AI Patterns

- Agent swarm intelligence (emergent coordination, pheromone trails)
- Federated learning across agent nodes
- Agent marketplace/registry (publish, discover, compose)
- Advanced human-in-the-loop workflows
- Multi-modal agents (vision, audio, code)
- Agent-to-agent negotiation protocols

## Enterprise Phase E4: Enterprise Observability

- OpenTelemetry distributed tracing
- Real-time WebSocket event streaming
- Live debugging: attach to running agents, inspect mailboxes
- Performance profiler with reduction analysis
- Cost attribution dashboards
- SLA/SLO monitoring with alerting

---

## Philosophy

**Core (Phases 1-9)**: Open source, BEAM-inspired, essential for any serious agent runtime. These features make zeptobeam unique and powerful on its own.

**Enterprise (E1-E4)**: Production-hardening for organizations running at scale. These are important but not differentiating — they make the runtime *deployable* at enterprise scale.
