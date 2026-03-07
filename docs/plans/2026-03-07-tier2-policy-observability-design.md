# Tier 2 — Policy Engine + Structured Observability Design

## Goal

Add effect-kind policy gates (G3) and structured observability events (X4) — the remaining Tier 2 items for safety and visibility into the runtime.

## Architecture

Two complementary additions. The policy engine evaluates per-EffectKind rules at the existing budget-check point in `runtime.rs`, returning Allow/Deny decisions. The event bus provides a ring buffer + tracing dual-write for structured lifecycle events across processes, effects, supervision, and policy decisions.

## Tech Stack

Rust, existing ZeptoVM kernel (runtime, reactor, scheduler, metrics)

---

## G3: Effect-Kind Policy Gates

### PolicyEngine

```rust
pub enum PolicyDecision {
  Allow,
  Deny(String),  // reason
}

pub struct PolicyRule {
  pub kind: EffectKind,
  pub decision: PolicyDecision,
  pub max_cost_microdollars: Option<u64>,
}

pub struct PolicyEngine {
  rules: Vec<PolicyRule>,
  default: PolicyDecision,  // Allow if no rule matches
}
```

Programmatic configuration via `PolicyEngine::new(rules, default)`. Rules are `Vec<PolicyRule>` built in Rust code.

**Future**: TOML `[policy]` config section to declare rules declaratively (Tier 3+).

### Evaluation

`PolicyEngine::evaluate(kind, estimated_cost) -> PolicyDecision`

- First matching rule wins (by EffectKind)
- If rule has `max_cost_microdollars` and estimated cost exceeds it, deny
- If no rule matches, use `default` decision

### Integration Point

In `runtime.rs` step 7 ("Process outbound effects"), after idempotency check and before budget check. Denied effects get an `EffectResult::failed()` delivered back to the process immediately.

---

## X4: Structured Observability Events

### RuntimeEvent Enum

Flat enum — add/remove variants freely without breaking consumers.

```rust
pub enum RuntimeEvent {
  ProcessSpawned { pid: Pid, behavior_module: String },
  ProcessExited { pid: Pid, reason: Reason },
  EffectRequested { pid: Pid, effect_id: u64, kind: EffectKind },
  EffectDispatched { pid: Pid, effect_id: u64, kind: EffectKind },
  EffectCompleted { pid: Pid, effect_id: u64, kind: EffectKind, status: String },
  EffectBlocked { pid: Pid, effect_id: u64, kind: EffectKind, reason: String },
  PolicyEvaluated { pid: Pid, effect_id: u64, kind: EffectKind, decision: String },
  SupervisorRestart { supervisor_pid: Pid, child_id: String, strategy: String },
  BudgetExhausted { pid: Pid, tokens_used: u64, limit: u64 },
}
```

### EventBus

Ring buffer + tracing dual-write:

```rust
pub struct EventBus {
  buffer: VecDeque<(u64, RuntimeEvent)>,  // (timestamp_ms, event)
  capacity: usize,                         // default 10_000
}
```

- `emit(event)` — push to ring buffer (evict oldest if full) + `tracing::info!()` with structured fields
- `drain_events()` — take all buffered events (for tests, CLI polling)
- `recent(n)` — peek last N events without draining

### Emission Points

EventBus lives on `Runtime`. Events emitted at:

- `spawn()` -> `ProcessSpawned`
- `propagate_exit()` -> `ProcessExited`
- Effect gating in `tick()` -> `EffectRequested`, `PolicyEvaluated`, `EffectDispatched` or `EffectBlocked`
- Effect completion -> `EffectCompleted`
- Supervisor restart -> `SupervisorRestart`
- Budget block -> `BudgetExhausted`

---

## Error Handling

### Policy Engine
- No matching rule -> use `default` decision (Allow by default)
- Deny -> deliver `EffectResult::failed()` with reason back to process
- Cost estimation unavailable -> skip cost-based rules, evaluate kind-only

### EventBus
- Buffer full -> evict oldest (ring buffer, no error)
- Tracing subscriber not configured -> `tracing::info!` is a no-op

---

## Testing

### Policy Tests
- Default allow — no rules, effect passes
- Deny by kind — rule denies DbWrite, LlmCall still passes
- Cost threshold — allow LlmCall under $5, deny over
- First-match wins — conflicting rules, first one takes precedence
- Denied effect returns EffectResult::failed to process

### EventBus Tests
- Emit + drain returns events in order
- Ring buffer eviction when capacity exceeded
- `recent(n)` returns last N without draining
- Tracing emission (verify buffer side, tracing is a bonus)

### Integration Tests
- Full lifecycle: spawn -> effect -> policy allows -> dispatch -> complete -> drain shows full trace
- Policy deny: spawn -> effect -> policy denies -> EffectBlocked emitted -> process gets failed result
