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

## Phase 7: MCP Integration [DONE]

Expose the runtime as an MCP server and consume MCP tools natively.

- **MCP server mode**: Axum-based HTTP MCP server with SSE transport, runtime tool handlers (spawn, send, kill, metrics, list processes)
- **MCP client**: Connect to external MCP tool servers via stdio transport, tool discovery and invocation
- **JSON-RPC layer**: Full MCP JSON-RPC 2.0 protocol with initialize/tools/call lifecycle
- **Tool factory integration**: McpToolFactory wires discovered MCP tools into agent ToolFactory
- **Session management**: Token-based auth, session expiry, map MCP sessions to runtime lifecycle
- **HTTP daemon wiring**: MCP runtime ops wired into health server endpoints

**Commits:** `0e6b33d` through `f4600e0`

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
| 7 | MCP Integration | Done | ~18 |
| 9 | BEAM Parity | Done | ~80 |
| **Total** | | **All core phases complete** | **343** |

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

## Enterprise Phase E1: Multi-Node Clustering

*Scale the runtime across machines — foundation for all enterprise features.*

**Networking & Discovery**
- gRPC (tonic) inter-node transport for message passing and control plane
- Node discovery: static config, DNS-based, and gossip protocol options
- Health-checked connection pools with automatic reconnection

**Distributed Process Model**
- Remote spawn: start processes on any node, transparent PID routing
- Location-transparent messaging: `send(pid, msg)` works across nodes
- Distributed supervision: supervisors that span nodes, restart on remote failure
- Cross-node dead-letter routing: forward undeliverable messages to origin node's DLQ

**Consistency & Reliability**
- Partition rebalancing: migrate agent processes between nodes on load imbalance
- Split-brain resolution: leader-based consistency for shared state (registry, checkpoints)
- Distributed ETS: replicated tables across nodes with eventual/strong consistency options
- Active-active geo-redundancy with configurable replication factor

**Design:** [`docs/plans/2026-03-06-multi-node-clustering-design.md`](plans/2026-03-06-multi-node-clustering-design.md) — Approach B (Router-Overlay), dual transport (gRPC + HTTP/SSE), SWIM gossip, CRDT registry, Raft checkpoints, feature-gated behind `clustering` Cargo feature.

**Prerequisite:** Core Phases 1–9 (complete).

---

## Enterprise Phase E2: Cloud Native Deployment

*Deploy and operate at scale on modern infrastructure.*

**Kubernetes Integration**
- Custom Operator with CRDs: AgentRuntime, AgentPool, AgentRelease
- Horizontal Pod Autoscaler (HPA) driven by agent queue depth and reduction load
- Rolling upgrades with hot-code reload coordination
- Helm charts and Terraform modules for one-command deployment

**Multi-Tenancy**
- Namespace isolation: separate agent pools, budgets, and config per tenant
- Resource quotas: CPU, memory, token budget, and agent count limits per tenant
- Per-tenant rate limiting with configurable burst allowances
- Tenant-aware routing: agent processes pinned to tenant-dedicated node pools

**Release & Deployment**
- Canary deployments: route % of traffic to new agent behavior versions
- Blue/green agent pool switching with instant rollback
- Shadow mode: run new behaviors alongside old, compare outputs without serving
- GitOps workflows (ArgoCD/Flux) for declarative agent configuration

**Prerequisite:** E1 (clustering for multi-node deployment).

---

## Enterprise Phase E3: Security, Compliance & Governance

*Enterprise trust: lock down, audit, and govern the runtime.*

**Authentication & Authorization**
- OIDC/OAuth2 authentication for API and MCP endpoints
- Role-based access control (RBAC): admin, operator, developer, read-only
- Per-agent permission scoping: restrict which tools, models, and data agents can access
- API key management with rotation and expiration

**Network Security**
- Zero-trust architecture with mTLS between all nodes
- Wasm-based agent sandboxing: isolate untrusted behaviors in WebAssembly
- Network policy enforcement: agents can only reach whitelisted endpoints
- Secret management integration (Vault, AWS Secrets Manager, SOPS)

**Compliance**
- Audit logging: tamper-proof log shipping for every agent action, tool call, and LLM request
- PII detection and automatic redaction in agent inputs/outputs
- Data residency controls: pin data and processing to specific regions
- GDPR deletion APIs: right-to-forget across checkpoints, ETS/DETS, and WAL
- FIPS 140-2 compliance mode for cryptographic operations
- SOC 2 Type II evidence collection automation

**Governance**
- Model governance: approved model whitelist, per-agent model assignment policies
- Prompt/config versioning: Git-backed version control for agent configurations
- Deployment approval workflows: require human sign-off for production behavior changes
- Agent decision audit trail: full lineage from input → LLM calls → tool use → output
- Cost governance: hard budget ceilings with automatic agent suspension

**Prerequisite:** E2 (multi-tenancy for tenant-scoped governance).

---

## Enterprise Phase E4: Observability & FinOps

*See everything, control costs, meet SLAs.*

**Distributed Tracing & Metrics**
- OpenTelemetry integration: traces span agent → LLM → tool → response
- Prometheus/Grafana metrics export: per-agent, per-tenant, per-model dashboards
- Structured log correlation: link logs to traces and agent PIDs
- Custom metric emission from agent behaviors

**Live Debugging**
- Attach to running agents: inspect mailbox contents, current state, behavior version
- Message flow visualization: trace a message through the supervision tree
- Performance profiler with reduction analysis and hot-path detection
- Replay mode: re-execute agent conversations from WAL for debugging

**Real-Time Streaming**
- WebSocket event streaming: subscribe to agent lifecycle events, messages, errors
- Event bus integration: publish agent events to Kafka, NATS, or Redis Streams
- Webhook notifications: configurable alerts on agent failures, budget exceeded, SLA breach

**FinOps & Cost Management**
- Token usage tracking per agent, per tenant, per model with real-time counters
- Cost attribution dashboards: who spent what, on which model, for which task
- Budget alerts: configurable thresholds with Slack/email/PagerDuty notifications
- Chargeback reports: per-team/per-project billing export (CSV, API)
- Model tier routing: auto-downgrade to cheaper models when budget is low
- Spot/preemptible instance awareness: graceful agent migration on eviction

**SLA Management**
- SLA/SLO definitions per agent pool: latency, throughput, error rate targets
- Automatic alerting when SLOs are breached
- Error budget tracking with burn-rate alerts
- Incident timeline generation from agent traces

**Prerequisite:** E1 (clustering for distributed traces), E3 (governance for cost controls).

---

## Enterprise Phase E5: Developer Experience & Integration

*Make it easy to build, test, and connect agents.*

**SDKs & Client Libraries**
- Python SDK: spawn agents, send messages, subscribe to events
- TypeScript/Node.js SDK: full API coverage with async/await
- REST and gRPC API with OpenAPI/protobuf specs
- CLI tool: manage agents, inspect state, deploy behaviors from terminal

**Agent Development**
- Interactive playground: web UI for testing agent behaviors with live state inspection
- Agent templates: starter behaviors for common patterns (chat, RAG, pipeline, router)
- Local development mode: single-binary with hot-reload for rapid iteration
- Integration test harness: mock LLM providers, assert on agent message sequences

**Testing & Quality**
- Replay testing: capture production conversations, replay against new behavior versions
- Simulation mode: run agents against synthetic workloads with configurable failure injection
- Behavior diff tool: compare outputs of two behavior versions side-by-side
- CI/CD integration: GitHub Actions / GitLab CI templates for agent test + deploy

**Enterprise Connectors**
- Database connectors: PostgreSQL, MySQL, MongoDB, Redis (as agent tools)
- Message bus: Kafka consumer/producer, NATS, RabbitMQ as IoOp backends
- Webhook management: inbound/outbound webhook registry with retry and dead-letter
- Enterprise APIs: Salesforce, Jira, Slack, GitHub integration as pluggable MCP tools
- Vector store integration: Pinecone, Weaviate, pgvector for RAG patterns

**Prerequisite:** Core Phase 7 (MCP for tool integration), E3 (auth for API security).

---

## Enterprise Phase E6: Advanced AI Patterns

*Differentiated capabilities for sophisticated AI deployments.*

**Swarm Intelligence**
- Emergent coordination: agents discover collaborators through shared ETS state
- Pheromone trail pattern: agents leave signals that influence routing of future tasks
- Collective decision-making: voting, consensus, and debate protocols between agents
- Dynamic team formation: agents self-organize into task-specific groups

**Agent Marketplace**
- Behavior registry: publish, version, discover, and compose agent behaviors
- Marketplace API: search by capability, rating, cost profile
- Composition engine: chain behaviors into pipelines with automatic type checking
- Dependency resolution: behaviors declare required tools, models, and permissions

**Advanced Orchestration**
- Long-running workflows: durable execution with saga pattern and compensation
- Human-in-the-loop: configurable approval points, escalation chains, feedback loops
- Multi-modal agents: vision, audio, and code generation with unified message types
- Agent-to-agent negotiation: bid/ask protocols for resource allocation and task delegation

**Learning & Adaptation**
- Federated learning across agent nodes: share model improvements without raw data
- Behavior A/B testing: statistical significance testing for behavior improvements
- Automatic prompt optimization: track which prompts produce best outcomes per task type
- Knowledge distillation: successful agent strategies condensed into reusable behaviors

**Prerequisite:** E1 (clustering for distributed agents), E4 (observability for A/B metrics).

---

## Enterprise Summary

| Phase | Focus | Dependency |
|-------|-------|------------|
| E1 | Multi-Node Clustering | Core complete |
| E2 | Cloud Native Deployment | E1 |
| E3 | Security, Compliance & Governance | E2 |
| E4 | Observability & FinOps | E1, E3 |
| E5 | Developer Experience & Integration | Core P7, E3 |
| E6 | Advanced AI Patterns | E1, E4 |

---

## Philosophy

**Core (Phases 1–9)**: Open source, BEAM-inspired, essential for any serious agent runtime. All core phases are complete (343 tests). These features make zeptobeam unique and powerful on its own.

**Enterprise (E1–E6)**: Production-hardening for organizations running at scale. Ordered by dependency — clustering enables cloud-native, which enables governance, which enables everything else. Each phase delivers standalone value while building toward the full platform.
