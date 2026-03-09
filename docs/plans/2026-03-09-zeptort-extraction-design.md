# zeptort Extraction Design

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:writing-plans to create the implementation plan.

**Goal:** Extract `zeptovm/` from `zeptoclaw-rt` into a standalone Rust library crate `zeptort` with its own GitHub repo.

**Date:** 2026-03-09

---

## What Gets Copied

| Source | Destination | Notes |
|--------|-------------|-------|
| `zeptovm/src/` | `zeptort/src/` | All source files, rename crate |
| `zeptovm/tests/` | `zeptort/tests/` | All integration tests |
| `zeptovm/Cargo.toml` | `zeptort/Cargo.toml` | Rename package to `zeptort` |
| `docs/ZEPTOVM-SPEC.md` | `zeptort/docs/ZEPTOVM-SPEC.md` | Living spec |
| `docs/HANDLER-CONVENTIONS.md` | `zeptort/docs/HANDLER-CONVENTIONS.md` | Handler guidelines |
| `docs/plans/2026-03-06-spec-v03-gap-analysis.md` | `zeptort/docs/gap-analysis.md` | Current status |

## What Gets Created Fresh

- `zeptort/README.md` — standalone runtime library README
- `zeptort/CLAUDE.md` — project instructions for the new repo
- `zeptort/LICENSE` — MIT
- `zeptort/.gitignore` — standard Rust + `*.db`, `erl_crash.dump`

## What Does NOT Come

- `lib-erlangrt/` — dead BEAM emulator
- `zeptobeam/` — CLI shell, stays behind
- `otp/` submodule — not needed
- `priv/` — Erlang test files
- `docs/plans/*.md` (40+ impl/design history files) — stay in zeptoclaw-rt as archive
- `docs/internal/` — gitignored internal docs, stay behind
- `docs/ROADMAP.md` — will be rewritten for zeptort

## Renames

- Package name: `zeptovm` → `zeptort`
- All internal `use zeptovm::` → `use zeptort::`
- Doc references to "ZeptoVM" stay as-is (design name, not crate name)

## Structure

Single crate (no workspace). Module structure preserved as-is:

```
zeptort/
├── src/
│   ├── lib.rs
│   ├── core/          # StepBehavior, Effect, Message, TurnContext
│   ├── kernel/        # Scheduler, Runtime, Reactor, Recovery
│   ├── durability/    # Journal, Snapshot, Idempotency, TimerStore
│   ├── control/       # Budget, Admission, ProviderGate
│   ├── behavior.rs    # Legacy async Behavior trait
│   ├── error.rs
│   ├── link.rs
│   ├── mailbox.rs
│   ├── pid.rs
│   ├── process.rs
│   ├── registry.rs
│   └── supervisor.rs
├── tests/
│   ├── v0_gate.rs
│   ├── v1_gate.rs
│   ├── v1_exit_signals.rs
│   ├── v2_gate.rs
│   ├── phase1_gate.rs
│   └── phase2_gate.rs
├── docs/
│   ├── ZEPTOVM-SPEC.md
│   ├── HANDLER-CONVENTIONS.md
│   └── gap-analysis.md
├── Cargo.toml
├── CLAUDE.md
├── README.md
├── LICENSE
└── .gitignore
```

## Steps

1. Create `/Users/dr.noranizaahmad/ios/zeptort/`
2. Copy source, tests, selected docs
3. Update Cargo.toml (rename package, clean deps)
4. Rename crate references (`zeptovm` → `zeptort`)
5. `cargo test` — verify 471 tests pass
6. Write README.md, CLAUDE.md, LICENSE, .gitignore
7. `git init`, initial commit
8. `gh repo create qhkm/zeptort --public`, push
