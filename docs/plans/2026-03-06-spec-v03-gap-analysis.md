# ZeptoVM SPEC-v03 Gap Analysis

**Date:** 2026-03-06
**Source:** `docs/internal/ZEPTOVM-SPEC-v03-review.md` (Codex design review)
**Compared against:** Current implementation (297 tests, 48 files, ~12k LOC)

---

## Critiques — Things to Fix

| # | Critique | Status | Details |
|---|----------|--------|---------|
| C1 | Soften "deterministic shell" claim | NOT DONE | Spec says "handler is deterministic". Need reword + conventions doc |
| C2 | Turn atomicity — explicit commit protocol | PARTIAL | TurnExecutor::commit() exists but journal + snapshot + effects are sequential writes, not one SQLite transaction |
| C3 | Separate scheduling fairness from economic fairness | DONE | Reduction counter separate from BudgetGate |
| C4 | Single-threaded scheduler — handler discipline | NOT DONE | No watchdog, no max wall-clock per turn, no overrun detection |
| C5 | Collapse two behavior traits to one | DONE | Only StepBehavior exists |

## Things to Add Now

| # | Addition | Status | Details |
|---|----------|--------|---------|
| A1 | ObjectRef type | NOT DONE | EffectKind variants exist, no ObjectRef struct or store |
| A2 | Behavior version in process metadata | NOT DONE | No behavior_module, behavior_version, behavior_checksum |
| A3 | Effect state classification for recovery | PARTIAL | RecoveryCoordinator exists but no explicit state machine |
| A4 | Watchdog for handler overruns | NOT DONE | No max wall-clock, no burst rate limit, no overrun signal |

## Acknowledged Gaps (not yet needed per review)

| # | Gap | Status | Notes |
|---|-----|--------|-------|
| G1 | Multi-node clustering | NOT DONE | Design doc exists, no implementation |
| G2 | Object/artifact plane | NOT DONE | No ObjectRef, no artifact store |
| G3 | Full policy engine | NOT DONE | No PolicyDecision, no hook points |
| G4 | Behavior versioning + migration | NOT DONE | No version metadata, no migration |
| G5 | OneForAll / RestForOne supervision | PARTIAL | Enum declared, only OneForOne implemented |
| G6 | Human approval UI/gateway | NOT DONE | EffectKind exists, no handler |

## Additional Gaps (from implementation audit)

| # | Gap | Status | Notes |
|---|-----|--------|-------|
| X1 | Name-based process registry | NOT DONE | Only Pid-based lookup |
| X2 | Selective receive | NOT DONE | No pattern matching in mailbox |
| X3 | Per-message TTL | NOT DONE | No expiry on messages |
| X4 | Structured observability events | NOT DONE | Only tracing macros + basic Metrics |
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

### Tier 3 — Important but can wait

- **A1/G2:** ObjectRef + artifact plane
- **A3:** Effect state machine for recovery
- **G6:** Human approval gateway
- **X2/X3:** Selective receive, message TTL

### Tier 4 — Phase 3+ (multi-node)

- **G1:** Clustering
- **G4:** Behavior migration
