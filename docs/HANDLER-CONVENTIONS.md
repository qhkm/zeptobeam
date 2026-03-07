# Handler Conventions

## Replay Safety

ZeptoVM handlers (`StepBehavior::handle()`) are **intended to be replay-safe**: given the same message and state, they should produce the same intents. The runtime does not enforce this — it is the handler author's responsibility.

## What handlers SHOULD do

- Read from `Envelope` (the incoming message)
- Read from `&self` (process state)
- Emit intents via `TurnContext` (`ctx.send()`, `ctx.request_effect()`, `ctx.set_state()`)
- Return a `StepResult`

## What handlers SHOULD NOT do

- **Read wall clock** (`std::time::Instant`, `SystemTime`) — use `ctx.request_effect()` for time-sensitive operations
- **Generate randomness** (`rand::thread_rng()`) — use a seeded RNG stored in process state, or request randomness as an effect
- **Read environment variables** (`std::env::var()`) — pass configuration at spawn time via init checkpoint
- **Perform I/O** (file, network, database) — use `EffectRequest` instead
- **Access global mutable state** (`static mut`, `lazy_static` with mutation)

## Why this matters

Replay safety enables:
- **Recovery**: replay journal entries after crash to reconstruct state
- **Debugging**: reproduce exact execution by replaying the same messages
- **Testing**: deterministic test outcomes from fixed message sequences

## When replay safety doesn't matter

For one-shot processes that won't be replayed (e.g., CLI wrappers, test harnesses), these conventions can be relaxed. Mark such behaviors clearly in their documentation.
