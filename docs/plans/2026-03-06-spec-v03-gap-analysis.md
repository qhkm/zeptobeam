# ZeptoVM SPEC-v03 Gap Analysis

**Date:** 2026-03-06
**Source:** `docs/internal/ZEPTOVM-SPEC-v03-review.md` (Codex design review)
**Compared against:** Current implementation (331 tests, 50+ files, ~13k LOC)

---

## Critiques — Things to Fix

| # | Critique | Status | Details |
|---|----------|--------|---------|
| C1 | Soften "deterministic shell" claim | DONE | Reworded to "replay-safe shell" + HANDLER-CONVENTIONS.md |
| C2 | Turn atomicity — explicit commit protocol | DONE | TurnExecutor::open_in_memory() uses shared SQLite connection for atomic journal+snapshot commits |
| C3 | Separate scheduling fairness from economic fairness | DONE | Reduction counter separate from BudgetGate |
| C4 | Single-threaded scheduler — handler discipline | DONE | Watchdog: max_turn_wall_clock + turn_overrun_flag + tracing::warn on overrun |
| C5 | Collapse two behavior traits to one | DONE | Only StepBehavior exists |

## Things to Add Now

| # | Addition | Status | Details |
|---|----------|--------|---------|
| A1 | ObjectRef type | NOT DONE | EffectKind variants exist, no ObjectRef struct or store |
| A2 | Behavior version in process metadata | DONE | BehaviorMeta struct + meta() trait method on StepBehavior |
| A3 | Effect state classification for recovery | PARTIAL | RecoveryCoordinator exists but no explicit state machine |
| A4 | Watchdog for handler overruns | DONE | Implemented with C4 — wall-clock timing in ProcessEntry::step() |

## Acknowledged Gaps (not yet needed per review)

| # | Gap | Status | Notes |
|---|-----|--------|-------|
| G1 | Multi-node clustering | NOT DONE | Design doc exists, no implementation |
| G2 | Object/artifact plane | NOT DONE | No ObjectRef, no artifact store |
| G3 | Full policy engine | DONE | PolicyEngine with effect-kind rules, integrated into runtime |
| G4 | Behavior versioning + migration | NOT DONE | No version metadata, no migration |
| G5 | OneForAll / RestForOne supervision | DONE | All three strategies implemented with shutdown coordination |
| G6 | Human approval UI/gateway | NOT DONE | EffectKind exists, no handler |

## Additional Gaps (from implementation audit)

| # | Gap | Status | Notes |
|---|-----|--------|-------|
| X1 | Name-based process registry | DONE | NameRegistry + TurnIntent integration + auto-cleanup |
| X2 | Selective receive | NOT DONE | No pattern matching in mailbox |
| X3 | Per-message TTL | NOT DONE | No expiry on messages |
| X4 | Structured observability events | DONE | RuntimeEvent enum + EventBus ring buffer with tracing dual-write |
| X5 | CliExec/SandboxExec/BrowserAutomation workers | NOT DONE | Enum variants only |

---

## Priority Tiers

### Tier 1 — Fix now (review explicitly flagged)

- **C1:** Soften deterministic claim (doc change)
- **C2:** True atomic turn commit (SQLite transaction)
- **C4/A4:** Watchdog for handler overruns
- **A2:** Behavior version metadata

### Tier 2 — High impact for real agent use

- **G3:** Policy engine (safety layer)
- **G5:** OneForAll/RestForOne supervision
- **X1:** Name-based registry
- **X4:** Structured observability

### Tier 3 — Mailbox enhancements

- **X3:** Per-message TTL (expiry field on messages, reap on receive)
- **X2:** Selective receive (pattern matching in MultiLaneMailbox)

### Tier 4 — Phase 3+ (deferred from Tier 3 + multi-node)

- **A1/G2:** ObjectRef + artifact plane (needs storage backend design: SQLite blobs / filesystem / S3-compatible)
- **A3:** Effect state machine for recovery (explicit Pending → Dispatched → Completed/Failed/TimedOut)
- **G6:** Human approval gateway (approval wait state + timeout + workflow)
- **G1:** Multi-node clustering
- **G4:** Behavior versioning + migration
