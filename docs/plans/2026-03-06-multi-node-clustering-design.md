# Phase 8: Multi-Node Clustering — Design Document

**Status:** Design only (enterprise feature, implementation deferred)
**Date:** 2026-03-06
**Approach:** B — Router-Overlay Top-Down

---

## Requirements

- Full Erlang-style distribution: scalability, fault tolerance, location transparency
- Dual transport: gRPC (hot path) + HTTP/SSE (management)
- Static config + SWIM gossip for node discovery
- Hybrid consistency: CRDTs for registry/membership, Raft for checkpoints
- Target: small clusters (2-5 nodes), design for medium (up to 20)
- Spawn-balancing first, cold migration later
- Local supervisors + NodeWatcher, evolve to cross-node supervisors
- Feature-gated: all clustering code behind `clustering` Cargo feature flag

---

## 1. Feature Gate

All clustering code is optional and disabled by default.

In `lib-erlangrt/Cargo.toml`:

```toml
[features]
clustering = ["tonic", "prost", "openraft"]

[dependencies]
tonic = { version = "0.12", optional = true }
prost = { version = "0.13", optional = true }
openraft = { version = "0.10", optional = true }

[build-dependencies]
prost-build = { version = "0.13", optional = true }
```

In `zeptobeam/Cargo.toml`:

```toml
[features]
clustering = ["erlangrt/clustering"]
```

All cluster modules gated:

```rust
// agent_rt/mod.rs
#[cfg(feature = "clustering")]
pub mod cluster_router;
#[cfg(feature = "clustering")]
pub mod node_transport;
#[cfg(feature = "clustering")]
pub mod gossip;
#[cfg(feature = "clustering")]
pub mod raft_store;
#[cfg(feature = "clustering")]
pub mod node_watcher;
```

The existing `cluster.rs` (SchedulerCluster) stays ungated — it is local multi-partition scheduling. Only networking/distributed code is gated.

Build:

```bash
cargo build                      # no clustering
cargo build --features clustering  # with clustering
```

---

## 2. Node Identity & PID Routing

### Node Identity

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(u64);

pub struct NodeIdentity {
    pub id: NodeId,
    pub name: String,
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
}
```

### PID Routing

`AgentPid` stays as `u64` internally. A `ClusterRouter` maintains PID-to-node mapping:

```rust
pub struct ClusterRouter {
    local_node: NodeId,
    remote_pids: HashMap<AgentPid, NodeId>,
    connections: HashMap<NodeId, Arc<dyn NodeTransport>>,
    members: HashMap<NodeId, NodeMember>,
}
```

### Integration Point

`SchedulerCluster::route_outbound_messages()` forwards unknown PIDs to `ClusterRouter`:

```
SchedulerCluster.tick()
  -> route_outbound_messages()
    -> local PID? deliver to local scheduler
    -> unknown PID? -> ClusterRouter.route(pid, msg)
      -> remote_pids lookup -> NodeTransport.send_message()
      -> not found anywhere -> dead letter
```

---

## 3. Transport Layer

### NodeTransport Trait

```rust
#[async_trait]
pub trait NodeTransport: Send + Sync {
    async fn send_message(&self, to: AgentPid, msg: Message) -> Result<(), TransportError>;
    async fn remote_spawn(&self, behavior_name: &str, args: serde_json::Value) -> Result<AgentPid, TransportError>;
    async fn ping(&self) -> Result<NodeStatus, TransportError>;
    async fn send_exit(&self, to: AgentPid, from: AgentPid, reason: Reason) -> Result<(), TransportError>;
    async fn process_info(&self, pid: AgentPid) -> Result<Option<RemoteProcessInfo>, TransportError>;
    async fn send_batch(&self, messages: Vec<(AgentPid, Message)>) -> Result<usize, TransportError>;
    fn transport_type(&self) -> &'static str;
}
```

### gRPC Transport (tonic)

Primary hot-path transport for message passing and Raft RPCs.

```protobuf
service ClusterNode {
    rpc SendMessage(MessageEnvelope) returns (SendAck);
    rpc SendBatch(BatchEnvelope) returns (BatchAck);
    rpc RemoteSpawn(SpawnRequest) returns (SpawnResponse);
    rpc Ping(PingRequest) returns (PongResponse);
    rpc SendExit(ExitSignal) returns (SendAck);
    rpc ProcessInfo(ProcessInfoRequest) returns (ProcessInfoResponse);
    rpc AppendEntries(RaftAppendRequest) returns (RaftAppendResponse);
    rpc RequestVote(RaftVoteRequest) returns (RaftVoteResponse);
}
```

### HTTP/SSE Transport

Axum `/cluster/*` endpoints alongside existing `/health`, `/metrics`, `/mcp`:

```
POST   /cluster/send          -- send message to local process
POST   /cluster/send-batch    -- batched message delivery
POST   /cluster/spawn         -- remote spawn
GET    /cluster/ping          -- health/status
POST   /cluster/exit          -- send exit signal
GET    /cluster/process/:pid  -- query process info
GET    /cluster/events        -- SSE stream of node events
```

Auth reuses Phase 7's `mcp_auth_middleware`.

### Transport Selection

Default: gRPC for message passing, exit signals, Raft RPCs. HTTP/SSE for spawn, queries, health, events.

```toml
[cluster]
node_id = 1
node_name = "node-alpha"
grpc_addr = "0.0.0.0:9091"

[[cluster.peers]]
node_id = 2
name = "node-beta"
grpc_addr = "10.0.0.2:9091"
http_addr = "10.0.0.2:9090"
transport = "grpc"  # "grpc", "http", or "auto"
```

---

## 4. Gossip & Node Discovery

### Bootstrap

Static seed list in config:

```toml
[cluster]
enabled = true
node_id = 1
node_name = "node-alpha"
grpc_addr = "0.0.0.0:9091"

[[cluster.seeds]]
grpc_addr = "10.0.0.2:9091"

[[cluster.seeds]]
grpc_addr = "10.0.0.3:9091"
```

### SWIM Gossip Protocol

```rust
pub struct GossipState {
    local: NodeIdentity,
    members: HashMap<NodeId, GossipMember>,
    membership_crdt: MembershipCrdt,
    config: GossipConfig,
}

pub enum MemberStatus {
    Alive,
    Suspect,
    Down,
    Left,
}
```

### Failure Detection (3-phase SWIM)

1. **Direct ping** to target node
2. **Indirect ping** via K random peers (if direct fails)
3. **Suspect** if no response, then **Down** after timeout

```toml
[cluster.gossip]
interval_ms = 1000
suspect_timeout_ms = 5000
k_indirect = 3
fanout = 3
```

### Membership CRDT

Lamport-timestamp-versioned map. Merge rule: highest timestamp wins, ties broken by status priority (Down > Suspect > Left > Alive).

Deltas piggybacked on every ping/ack exchange.

### ClusterRouter Integration

- Node joins -> open transport, start monitoring
- Node suspected -> stop spawning there, keep routing
- Node Down -> remove node, orphan its PIDs, trigger NodeWatcher
- Node Left -> same as Down, no error logging

---

## 5. Distributed Registry (CRDTs)

### OR-Set CRDT for Global Name Registry

```rust
pub struct DistributedRegistry {
    local: AgentRegistry,
    global_names: ORSetCrdt<String, GlobalRegistration>,
    pid_locations: HashMap<AgentPid, NodeId>,
}

pub struct GlobalRegistration {
    pub pid: AgentPid,
    pub node: NodeId,
    pub registered_at: u64,
}
```

### OR-Set Implementation

```rust
pub struct ORSetCrdt<K: Hash + Eq, V> {
    entries: HashMap<K, Vec<(V, UniqueTag)>>,
    removed: HashSet<UniqueTag>,
}

pub struct UniqueTag(NodeId, u64);
```

Merge: union of all entries, subtract all tombstones.

### Name Conflict Resolution

Two nodes register same name concurrently: lowest NodeId wins (deterministic). Losing process gets `SystemMsg::NameConflict`.

### Sync

Deltas piggybacked on gossip, same as membership. Fallback broadcast query for convergence lag.

---

## 6. Distributed Checkpoints (Raft)

### openraft-based Consensus

```rust
pub struct RaftCheckpointStore {
    raft: openraft::Raft<CheckpointTypeConfig>,
    state_machine: Arc<RwLock<CheckpointStateMachine>>,
}

pub enum CheckpointCommand {
    Save { key: String, value: serde_json::Value },
    Delete { key: String },
    PruneBefore { epoch_secs: i64 },
}

pub struct CheckpointStateMachine {
    inner: SqliteCheckpointStore,
    last_applied: LogId,
}
```

### Implements Existing CheckpointStore Trait

- `save()` -> propose to Raft leader -> replicated -> applied to local SQLite
- `load()` -> read directly from local state machine (all replicas identical)
- Non-leader nodes forward writes to leader via gRPC

### Raft RPCs over gRPC

AppendEntries, RequestVote, InstallSnapshot.

### Cluster Size Behavior

- 1 node: SqliteCheckpointStore directly (no Raft)
- 2 nodes: primary/backup replication
- 3+ nodes: full Raft consensus

---

## 7. Remote Spawn & Load Balancing

### Placement Strategy

```rust
pub enum PlacementStrategy {
    Local,
    RoundRobin,
    LeastLoaded,
    Pinned(NodeId),
}
```

### Behavior Registry

Behaviors registered by name. Remote spawn sends name + args JSON; receiving node looks up behavior locally.

```rust
pub struct BehaviorRegistry {
    behaviors: HashMap<String, Arc<dyn AgentBehavior>>,
}
```

### Load Info

Piggybacked on gossip:

```rust
pub struct NodeLoad {
    pub process_count: usize,
    pub queue_depth: usize,
    pub bridge_pending: usize,
    pub cpu_estimate: f32,
}
```

### Config

```toml
[cluster.placement]
strategy = "least_loaded"
```

---

## 8. NodeWatcher & Failure Recovery

### NodeWatcher Process

High-priority agent process on every node. Reacts to gossip membership changes.

On node Down:
- Supervised processes -> respawn on surviving nodes
- Linked processes -> deliver exit signals
- Monitored processes -> deliver DOWN messages
- Named processes -> CRDT tombstone + re-register after respawn

### Orphan Policy

```rust
pub struct OrphanPolicy {
    pub respawn_supervised: bool,  // default: true
    pub respawn_named: bool,       // default: false
    pub drop_ephemeral: bool,      // default: true
}
```

### Guards Against Respawn Storms

1. Leader election: Raft leader's NodeWatcher is authoritative
2. Staggered respawn: 50ms delay between batches of 10
3. Circuit breaker: stop if >50% of cluster is Down

### Graceful Leave

Node broadcasts `Left`, drains IO, proactively migrates supervised processes, disconnects.

```toml
[cluster.recovery]
respawn_supervised = true
respawn_named = false
drop_ephemeral = true
respawn_batch_size = 10
respawn_delay_ms = 50
circuit_breaker_threshold = 0.5
```

---

## 9. Cross-Node Signals & Dead Letters

### Extended Dead-Letter Reasons

```rust
pub enum DeadLetterReason {
    MailboxFull,
    ProcessNotFound,
    RemoteNodeDown(NodeId),
    RemoteDeliveryFailed(NodeId, String),
}
```

### Cross-Node Exit/DOWN Signals

Forwarded via `ClusterRouter` -> `NodeTransport` (gRPC). Remote link/monitor tracking in `ClusterRouter`:

```rust
pub struct RemoteLink {
    local_pid: AgentPid,
    remote_pid: AgentPid,
    remote_node: NodeId,
}
```

This is the hook point for evolving to full cross-node supervisors later.

---

## Design Summary

| Component | Implementation | New Dependencies |
|-----------|---------------|-----------------|
| Node Identity | `NodeId`, `NodeIdentity`, cluster config | None |
| PID Routing | `ClusterRouter` extending `SchedulerCluster` outbound | None |
| gRPC Transport | `tonic` + protobuf | tonic, prost |
| HTTP/SSE Transport | Axum `/cluster/*`, reuses Phase 7 auth | None |
| Node Discovery | Static seeds + SWIM gossip | None |
| Failure Detection | 3-phase SWIM | None |
| Distributed Registry | OR-Set CRDT, piggybacked on gossip | None |
| Distributed Checkpoints | `openraft` + `RaftCheckpointStore` | openraft |
| Remote Spawn | `BehaviorRegistry` + `PlacementStrategy` | None |
| Load Balancing | NodeLoad piggybacked on gossip | None |
| NodeWatcher | Agent process, leader-coordinated respawns | None |
| Cross-Node Signals | Exit/DOWN via ClusterRouter, remote link tracking | None |
| Dead-Letter Routing | Extended DeadLetterReason | None |

**Unchanged existing code:** AgentPid format, AgentRegistry local API, CheckpointStore trait, SchedulerCluster (extended not rewritten), all existing tests.

---

## Architectural Decision: Why Approach B (Router-Overlay)

Three approaches were evaluated:

**A. Transport-First Bottom-Up** — Embed NodeId in AgentPid, build up from network layer. Rejected: invasive PID format change ripples through every module, long path to working demo.

**B. Router-Overlay Top-Down (chosen)** — Keep AgentPid as-is, add ClusterRouter mapping PID ranges to nodes. Extends existing `SchedulerCluster.outbound_messages` mechanism. Minimal disruption, fastest to first demo, incremental delivery.

**C. MCP-Native Federation** — Treat nodes as MCP servers. Rejected as primary architecture: MCP has too much overhead for high-frequency messaging. However, HTTP/SSE transport reuses Phase 7's MCP infrastructure as a secondary transport.
