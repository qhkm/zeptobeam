# Tier 2 — Name Registry + Supervision Strategies Design

## Goal

Implement name-based process registry (X1) and OneForAll/RestForOne supervision strategies (G5) — the two most foundational Tier 2 items for building real agent trees.

## Architecture

Two independent but complementary changes. The registry adds a global flat name->Pid mapping with auto-cleanup on process exit. Supervision extends the existing step-based `SupervisorBehavior` with a `SupervisionStrategy` enum and a shutdown coordination state machine. Supervisors auto-register children by their `ChildSpec.id`.

## Tech Stack

Rust, existing ZeptoVM kernel (scheduler, process_table, supervisor_behavior, registry)

---

## X1: Name-Based Process Registry

### Data Structure

Add `NameRegistry` to `registry.rs`:

```rust
pub struct NameRegistry {
    names: HashMap<String, Pid>,
    pid_names: HashMap<Pid, String>,
}
```

Two maps for O(1) lookup in both directions. `pid_names` reverse index enables cleanup on process exit without scanning.

### API

On `NameRegistry`:
- `register(name: String, pid: Pid) -> Result<(), String>` — fails if name taken
- `unregister(name: &str) -> Option<Pid>` — removes registration, returns old pid
- `whereis(name: &str) -> Option<Pid>` — lookup by name
- `registered_name(pid: Pid) -> Option<&str>` — reverse lookup

On `SchedulerRuntime` (public API):
- `register_name(name, pid)`, `unregister_name(name)`, `whereis(name)`, `send_named(name, msg)`

### TurnContext Integration

New `TurnIntent` variants:
- `Register { name: String }`
- `Unregister { name: String }`
- `SendNamed { name: String, envelope: Envelope }`

Behaviors call `ctx.register(name)`, `ctx.whereis(name)`, `ctx.send_named(name, payload)`. The scheduler resolves these during intent dispatch.

Note: `whereis()` needs to return a value to the caller. Since `handle()` is synchronous, we add `whereis()` as a direct method on `TurnContext` that holds a reference to the registry (not an intent).

### Lifecycle

- Process exits → auto-unregister in scheduler's exit cleanup path
- Supervisor children auto-register using `ChildSpec.id` as the name
- Brief unregistered window between child exit and restart is correct (Erlang semantics)

---

## G5: OneForAll / RestForOne Supervision

### SupervisionStrategy Enum

Add to `kernel/supervisor_behavior.rs`:

```rust
pub enum SupervisionStrategy {
    OneForOne,   // restart only the failed child
    OneForAll,   // shutdown all, restart all
    RestForOne,  // shutdown children after failed one, restart those
}
```

Add `strategy: SupervisionStrategy` field to `SupervisorSpec`.

### Shutdown Coordination

Signal-based graceful shutdown:

1. On `ChildExited`, determine affected siblings based on strategy
2. Send `Signal::Shutdown` to affected siblings via mailbox
3. Track pending shutdowns in a `HashSet<Pid>`
4. Set timer for `shutdown_timeout_ms`
5. As `ChildExited` signals arrive, remove from pending set
6. When pending set empty (or timeout fires), restart all affected children in order
7. Timeout fallback: `request_kill()` for stragglers

### State Machine

```rust
enum SupervisorPhase {
    Normal,
    ShuttingDown {
        pending_exits: HashSet<Pid>,
        children_to_restart: Vec<String>, // child IDs in order
    },
}
```

During `ShuttingDown`, the supervisor does not process new spawn requests — only `ChildExited` signals and the shutdown timer.

### RestForOne Ordering

Children are tracked in insertion order (existing `Vec<ChildSpec>` in `children`). When child N dies, children N+1..end are shut down and restarted. Children 0..N-1 are untouched.

---

## Error Handling

### Registry
- Register duplicate name → `Err("name already registered")`
- Register dead pid → `Err("process not found")`
- Double unregister → no-op, returns `None`
- Send to unknown name → `Err("name not registered")`

### Supervision
- Child dies during ShuttingDown (already being shut down) → remove from pending, normal flow
- Different child dies during ShuttingDown → queue event, handle after restart
- Restart intensity exceeded → supervisor exits (same as OneForOne)
- Supervisor exits during ShuttingDown → children killed via link propagation

---

## Testing

### Registry Tests
- Register + whereis returns pid
- Duplicate register → error
- Process exit → auto-unregister
- send_named delivers correctly
- send_named unknown → error
- Unregister + re-register works

### Supervision Tests
- OneForAll: kill one → all siblings shutdown → all restart
- OneForAll: shutdown timeout → force-kill → restart all
- RestForOne: kill child 2/3 → child 3 shutdown → 2+3 restart, 1 untouched
- RestForOne: kill child 1/3 → 2+3 shutdown → all restart
- Restart intensity exceeded → supervisor exits
- Messages during ShuttingDown → buffered

### Integration Tests
- Supervisor registers children by name
- Child dies + restarts → name re-registered with new pid
- OneForAll restart → all names re-registered
