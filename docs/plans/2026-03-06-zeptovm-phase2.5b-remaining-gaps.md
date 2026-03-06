# Phase 2.5b — Remaining Medium Gaps

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the 6 remaining medium-priority gaps from the Phase 2.5 audit: SpawnProcess intent, DebitBudget intent, BudgetGate integration, journal compaction, Envelope dedup_key checking, and Envelope.from propagation.

**Architecture:** Each gap is a small, focused change. SpawnProcess and DebitBudget add new TurnIntent variants and wire them through the scheduler into the runtime. BudgetGate replaces the simple BudgetState. Journal compaction runs automatically after snapshots. Dedup checking prevents duplicate message delivery.

**Tech Stack:** Rust, SQLite (rusqlite)

---

### Task 1: Add SpawnProcess intent

Behaviors should be able to spawn child processes via `ctx.spawn()`.

**Files:**
- Modify: `zeptovm/src/core/turn_context.rs`
- Modify: `zeptovm/src/kernel/scheduler.rs`
- Modify: `zeptovm/src/kernel/runtime.rs`

**Steps:**
1. Add `SpawnProcess(Box<dyn StepBehavior>)` variant to TurnIntent
2. Add `spawn(&mut self, behavior: Box<dyn StepBehavior>)` to TurnContext
3. Add `spawn_requests` vec to SchedulerEngine, route SpawnProcess intent there
4. Add `take_spawn_requests()` accessor
5. In runtime tick, process spawn requests via engine.spawn_with_parent()
6. Test: behavior spawns child, verify child exists and parent is set
7. Commit

---

### Task 2: Add DebitBudget intent

Behaviors should be able to debit budget explicitly via `ctx.debit_budget()`.

**Files:**
- Modify: `zeptovm/src/core/turn_context.rs`
- Modify: `zeptovm/src/kernel/scheduler.rs`
- Modify: `zeptovm/src/kernel/runtime.rs`

**Steps:**
1. Add `DebitBudget { tokens: u64, cost_microdollars: u64 }` variant to TurnIntent
2. Add `debit_budget(&mut self, tokens: u64, cost_microdollars: u64)` to TurnContext
3. Add `budget_debits` vec to SchedulerEngine, route DebitBudget there
4. Add `take_budget_debits()` accessor
5. In runtime tick, apply debits to budget state
6. Test: behavior debits budget, verify budget decreases
7. Commit

---

### Task 3: Integrate BudgetGate into runtime

Replace simple `BudgetState` with the atomic `BudgetGate` from `control/budget.rs`.

**Files:**
- Modify: `zeptovm/src/kernel/runtime.rs`

**Steps:**
1. Add `budget_gate: Option<BudgetGate>` field to SchedulerRuntime
2. Add `with_budget_gate(token_limit, cost_limit)` builder
3. In tick() budget check, use `budget_gate.precheck(estimated_tokens)` instead of simple comparison
4. After effect completion, call `budget_gate.commit(actual_tokens, cost)` to track usage
5. Keep BudgetState for backward compat, but BudgetGate is the real gate
6. Test: BudgetGate blocks when token limit exceeded
7. Commit

---

### Task 4: Auto journal compaction after snapshot

Journal entries should be truncated after a snapshot is saved to prevent unbounded growth.

**Files:**
- Modify: `zeptovm/src/kernel/turn_executor.rs`

**Steps:**
1. In TurnExecutor::commit(), after saving a snapshot, call journal.truncate_before(pid, snapshot_seq) to remove old entries
2. Track the journal entry ID range so we know what to truncate
3. Test: after enough turns to trigger a snapshot, verify old journal entries are pruned
4. Commit

---

### Task 5: Envelope dedup_key checking

The `dedup_key` field on Envelope exists but is never checked. The mailbox should reject duplicate messages.

**Files:**
- Modify: `zeptovm/src/kernel/mailbox.rs`

**Steps:**
1. Add a `seen_dedup_keys: HashSet<String>` to MultiLaneMailbox
2. In `push()`, if `env.dedup_key` is Some and already in the set, skip the message
3. Add a bounded capacity to the dedup set (e.g., retain last 1000 keys)
4. Test: push two messages with same dedup_key, verify only one delivered
5. Commit

---

### Task 6: Propagate Envelope.from in TurnContext.send

When a process sends a message via `ctx.send()`, the `from` field should automatically be set to `ctx.pid`.

**Files:**
- Modify: `zeptovm/src/core/turn_context.rs`

**Steps:**
1. In `send()` and `send_text()`, set `msg.from = Some(self.pid)` before pushing
2. Test: verify sent messages have correct from field
3. Commit
