# Tier 3 — Mailbox Enhancements Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add per-message TTL and end-to-end tag-based selective receive to the ZeptoVM runtime.

**Architecture:** Two new optional fields on Envelope (`expires_at`, `tag`). TTL is enforced lazily in `pop()`/`pop_matching()` with an `expired_count` accumulator the runtime drains for metrics. Selective receive is wired end-to-end: `StepResult::WaitForTag(String)` → `ProcessRuntimeState::WaitingTagged(String)` → scheduler uses `pop_matching()` on next step. Envelope stays dumb (no wall-clock access) — only `with_expires_at(epoch_ms)` builder, caller computes absolute time.

**Tech Stack:** Rust, ZeptoVM kernel (core/message.rs, core/step_result.rs, kernel/mailbox.rs, kernel/process_table.rs, kernel/scheduler.rs, kernel/metrics.rs)

---

### Task 1: Add `expires_at` and `tag` fields to Envelope

**Files:**
- Modify: `zeptovm/src/core/message.rs:56-65` (Envelope struct)
- Modify: `zeptovm/src/core/message.rs:92-159` (Envelope constructors)

**Step 1: Add fields to Envelope struct**

Add `expires_at: Option<u64>` and `tag: Option<String>` to the Envelope struct:

```rust
pub struct Envelope {
  pub msg_id: MsgId,
  pub correlation_id: Option<String>,
  pub from: Option<Pid>,
  pub to: Pid,
  pub class: MessageClass,
  pub priority: Priority,
  pub dedup_key: Option<String>,
  pub payload: EnvelopePayload,
  pub expires_at: Option<u64>,
  pub tag: Option<String>,
}
```

**Step 2: Update all constructors to initialize new fields**

In `Envelope::user()`, `Envelope::effect_result()`, and `Envelope::signal()`, add:

```rust
expires_at: None,
tag: None,
```

**Step 3: Add builder methods**

After the existing `with_from` builder. Note: NO `with_ttl_ms()` — Envelope has no clock access. Callers compute absolute expiry from runtime's `clock_ms`.

```rust
/// Builder: set explicit expiry timestamp (epoch millis).
/// The caller is responsible for computing this from clock_ms + ttl.
pub fn with_expires_at(mut self, epoch_ms: u64) -> Self {
    self.expires_at = Some(epoch_ms);
    self
}

/// Builder: set message tag for selective receive.
pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
    self.tag = Some(tag.into());
    self
}
```

**Step 4: Add tests for new builders**

```rust
#[test]
fn test_envelope_with_expires_at() {
    let to = Pid::from_raw(1);
    let env = Envelope::text(to, "hi").with_expires_at(99999);
    assert_eq!(env.expires_at, Some(99999));
}

#[test]
fn test_envelope_with_tag() {
    let to = Pid::from_raw(1);
    let env = Envelope::text(to, "hi").with_tag("effect_result");
    assert_eq!(env.tag.as_deref(), Some("effect_result"));
}

#[test]
fn test_envelope_defaults_no_expiry_no_tag() {
    let to = Pid::from_raw(1);
    let env = Envelope::text(to, "hi");
    assert!(env.expires_at.is_none());
    assert!(env.tag.is_none());
}

#[test]
fn test_envelope_builder_chain() {
    let to = Pid::from_raw(1);
    let from = Pid::from_raw(2);
    let env = Envelope::text(to, "hi")
        .with_from(from)
        .with_expires_at(5000)
        .with_tag("important")
        .with_correlation("c-1");
    assert_eq!(env.from, Some(from));
    assert_eq!(env.expires_at, Some(5000));
    assert_eq!(env.tag.as_deref(), Some("important"));
    assert_eq!(env.correlation_id.as_deref(), Some("c-1"));
}
```

**Step 5: Run tests to verify**

Run: `cargo test -p zeptovm --lib -- message::tests`
Expected: All message tests pass (existing + 4 new)

**Step 6: Commit**

```bash
git add zeptovm/src/core/message.rs
git commit -m "feat(message): add expires_at and tag fields to Envelope"
```

---

### Task 2: Update `pop()` with TTL checking + `expired_count` + `reap_expired()`

**Files:**
- Modify: `zeptovm/src/kernel/mailbox.rs` (struct fields, pop method, new methods, tests)

**Step 1: Write failing TTL tests**

Add these tests to the existing `mod tests` block in `mailbox.rs`:

```rust
#[test]
fn test_pop_no_ttl_never_expires() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "hello"));
    // Even at far-future time, no-TTL messages are fine
    let env = mb.pop(u64::MAX).unwrap();
    assert!(matches!(
        env.payload,
        EnvelopePayload::User(crate::core::message::Payload::Text(_))
    ));
}

#[test]
fn test_pop_ttl_before_expiry() {
    let mut mb = MultiLaneMailbox::new();
    let mut env = Envelope::text(to(), "fresh");
    env.expires_at = Some(1000);
    mb.push(env);
    let result = mb.pop(500);
    assert!(result.is_some());
}

#[test]
fn test_pop_ttl_after_expiry_skipped() {
    let mut mb = MultiLaneMailbox::new();
    let mut expired = Envelope::text(to(), "old");
    expired.expires_at = Some(100);
    mb.push(expired);
    mb.push(Envelope::text(to(), "fresh"));
    let env = mb.pop(200).unwrap();
    if let EnvelopePayload::User(crate::core::message::Payload::Text(s)) =
        &env.payload
    {
        assert_eq!(s, "fresh");
    } else {
        panic!("expected text");
    }
    assert_eq!(mb.total_len(), 0);
}

#[test]
fn test_pop_all_expired_returns_none() {
    let mut mb = MultiLaneMailbox::new();
    let mut env = Envelope::text(to(), "old");
    env.expires_at = Some(100);
    mb.push(env);
    assert!(mb.pop(200).is_none());
    assert_eq!(mb.total_len(), 0);
}

#[test]
fn test_expired_count_tracks_discards() {
    let mut mb = MultiLaneMailbox::new();
    let mut e1 = Envelope::text(to(), "old1");
    e1.expires_at = Some(100);
    let mut e2 = Envelope::text(to(), "old2");
    e2.expires_at = Some(200);
    mb.push(e1);
    mb.push(e2);
    mb.push(Envelope::text(to(), "fresh"));
    // Pop at t=300 should skip 2 expired, return fresh
    let _ = mb.pop(300).unwrap();
    assert_eq!(mb.take_expired_count(), 2);
    // Second call returns 0 (counter was drained)
    assert_eq!(mb.take_expired_count(), 0);
}

#[test]
fn test_reap_expired_removes_and_returns_count() {
    let mut mb = MultiLaneMailbox::new();
    let mut e1 = Envelope::text(to(), "old1");
    e1.expires_at = Some(100);
    let mut e2 = Envelope::text(to(), "old2");
    e2.expires_at = Some(200);
    mb.push(e1);
    mb.push(e2);
    mb.push(Envelope::text(to(), "fresh"));

    let reaped = mb.reap_expired(300);
    assert_eq!(reaped, 2);
    assert_eq!(mb.total_len(), 1);
    // reap also feeds expired_count
    assert_eq!(mb.take_expired_count(), 2);
}

#[test]
fn test_reap_expired_nothing_to_reap() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "no ttl"));
    assert_eq!(mb.reap_expired(1000), 0);
    assert_eq!(mb.total_len(), 1);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm --lib -- mailbox::tests`
Expected: FAIL — `pop()` doesn't accept `now_ms`, `take_expired_count` doesn't exist

**Step 3: Add `expired_count` field to struct and `take_expired_count()` method**

Add to `MultiLaneMailbox` struct:

```rust
/// Accumulated count of expired messages discarded since last drain.
expired_count: usize,
```

Initialize in `new()`:

```rust
expired_count: 0,
```

Add method:

```rust
/// Drain the expired-message counter. Returns the count since last drain.
/// The runtime calls this to feed metrics.
pub fn take_expired_count(&mut self) -> usize {
    let count = self.expired_count;
    self.expired_count = 0;
    count
}
```

**Step 4: Add module-level helper functions**

Add these as free functions in the `mailbox` module, above `impl MultiLaneMailbox`:

```rust
/// Check if an envelope has expired.
fn is_expired(env: &Envelope, now_ms: u64) -> bool {
    matches!(env.expires_at, Some(t) if now_ms >= t)
}

/// Pop first non-expired message from a lane.
/// Returns (envelope_or_none, number_of_expired_discarded).
fn pop_lane_alive(
    lane: &mut VecDeque<Envelope>,
    now_ms: u64,
) -> (Option<Envelope>, usize) {
    let mut expired = 0;
    loop {
        match lane.front() {
            None => return (None, expired),
            Some(env) if is_expired(env, now_ms) => {
                lane.pop_front();
                expired += 1;
            }
            Some(_) => return (lane.pop_front(), expired),
        }
    }
}
```

**Step 5: Replace the `pop` method**

```rust
/// Pop the next envelope respecting lane priority with fairness.
/// Expired messages (expires_at <= now_ms) are silently discarded.
/// Discarded count accumulates in expired_count for metrics.
pub fn pop(&mut self, now_ms: u64) -> Option<Envelope> {
    // Control always first
    let (result, exp) = pop_lane_alive(&mut self.control, now_ms);
    self.expired_count += exp;
    if result.is_some() {
        return result;
    }

    // Fairness check
    if self.high_priority_streak >= self.fairness_window {
        self.high_priority_streak = 0;
        let (result, exp) =
            pop_lane_alive(&mut self.background, now_ms);
        self.expired_count += exp;
        if result.is_some() {
            return result;
        }
    }

    // Supervisor
    let (result, exp) =
        pop_lane_alive(&mut self.supervisor, now_ms);
    self.expired_count += exp;
    if let Some(env) = result {
        self.high_priority_streak += 1;
        return Some(env);
    }

    // Effect results
    let (result, exp) = pop_lane_alive(&mut self.effect, now_ms);
    self.expired_count += exp;
    if let Some(env) = result {
        self.high_priority_streak += 1;
        return Some(env);
    }

    // User messages
    let (result, exp) = pop_lane_alive(&mut self.user, now_ms);
    self.expired_count += exp;
    if let Some(env) = result {
        self.high_priority_streak = 0;
        return Some(env);
    }

    // Background
    let (result, exp) =
        pop_lane_alive(&mut self.background, now_ms);
    self.expired_count += exp;
    if let Some(env) = result {
        self.high_priority_streak = 0;
        return Some(env);
    }

    None
}
```

**Step 6: Add `reap_expired` method**

```rust
/// Remove all expired messages from all lanes.
/// Returns the number of messages removed. Also feeds expired_count.
pub fn reap_expired(&mut self, now_ms: u64) -> usize {
    fn reap_lane(
        lane: &mut VecDeque<Envelope>,
        now_ms: u64,
    ) -> usize {
        let before = lane.len();
        lane.retain(|env| !is_expired(env, now_ms));
        before - lane.len()
    }
    let count = reap_lane(&mut self.control, now_ms)
        + reap_lane(&mut self.supervisor, now_ms)
        + reap_lane(&mut self.effect, now_ms)
        + reap_lane(&mut self.user, now_ms)
        + reap_lane(&mut self.background, now_ms);
    self.expired_count += count;
    count
}
```

**Step 7: Update existing tests**

All existing tests call `mb.pop()` without arguments. Update them to `mb.pop(0)` — at time 0, no messages will have expired (all test messages have `expires_at: None`).

Replace all occurrences of `mb.pop()` in the tests section with `mb.pop(0)`.

**Step 8: Run tests to verify they pass**

Run: `cargo test -p zeptovm --lib -- mailbox::tests`
Expected: All tests pass (existing updated + 8 new TTL tests)

**Step 9: Commit**

```bash
git add zeptovm/src/kernel/mailbox.rs
git commit -m "feat(mailbox): TTL checking in pop() with expired_count + reap_expired()"
```

---

### Task 3: Update `ProcessEntry::step()` to pass `now_ms` to `pop()`

**Files:**
- Modify: `zeptovm/src/kernel/process_table.rs:92` (step signature)
- Modify: `zeptovm/src/kernel/process_table.rs:104` (pop call)
- Modify: `zeptovm/src/kernel/scheduler.rs:215` (caller)

**Step 1: Change `step()` to accept `now_ms`**

Change the signature from:

```rust
pub fn step(&mut self) -> (StepResult, TurnContext) {
```

to:

```rust
pub fn step(&mut self, now_ms: u64) -> (StepResult, TurnContext) {
```

**Step 2: Update the `pop()` call**

Change line 104 from:

```rust
match self.mailbox.pop() {
```

to:

```rust
match self.mailbox.pop(now_ms) {
```

**Step 3: Update the scheduler caller**

In `scheduler.rs:215`, change:

```rust
let (result, mut ctx) = proc.step();
```

to:

```rust
let (result, mut ctx) = proc.step(self.clock_ms);
```

**Step 4: Update all test callers in `process_table.rs`**

All test calls like `p.step()` become `p.step(0)`. There are ~15 call sites in the tests section of `process_table.rs` — update all of them.

**Step 5: Run full test suite**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 6: Commit**

```bash
git add zeptovm/src/kernel/process_table.rs zeptovm/src/kernel/scheduler.rs
git commit -m "feat(process): pass now_ms through step() to mailbox pop()"
```

---

### Task 4: Add `pop_matching()` for tag-based selective receive (mailbox primitive)

**Files:**
- Modify: `zeptovm/src/kernel/mailbox.rs` (add pop_matching method + tests)

**Step 1: Write failing selective receive tests**

Add these tests to the `mod tests` block:

```rust
#[test]
fn test_pop_matching_returns_tagged_message() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "untagged"));
    mb.push(Envelope::text(to(), "tagged").with_tag("important"));
    mb.push(Envelope::text(to(), "other tag").with_tag("other"));

    let env = mb.pop_matching(0, "important").unwrap();
    assert_eq!(env.tag.as_deref(), Some("important"));
    assert_eq!(mb.total_len(), 2);
}

#[test]
fn test_pop_matching_skips_non_matching() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "a").with_tag("x"));
    mb.push(Envelope::text(to(), "b").with_tag("y"));

    let result = mb.pop_matching(0, "z");
    assert!(result.is_none());
    assert_eq!(mb.total_len(), 2);
}

#[test]
fn test_pop_matching_respects_lane_priority() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "user").with_tag("find_me"));
    let mut ctrl = Envelope::signal(to(), Signal::Suspend);
    ctrl.tag = Some("find_me".into());
    mb.push(ctrl);

    let env = mb.pop_matching(0, "find_me").unwrap();
    assert_eq!(env.class, MessageClass::Control);
    assert_eq!(mb.total_len(), 1);
}

#[test]
fn test_pop_matching_skips_expired_even_if_tag_matches() {
    let mut mb = MultiLaneMailbox::new();
    let mut expired = Envelope::text(to(), "old").with_tag("target");
    expired.expires_at = Some(100);
    mb.push(expired);
    mb.push(Envelope::text(to(), "fresh").with_tag("target"));

    let env = mb.pop_matching(200, "target").unwrap();
    if let EnvelopePayload::User(crate::core::message::Payload::Text(s)) =
        &env.payload
    {
        assert_eq!(s, "fresh");
    } else {
        panic!("expected text");
    }
}

#[test]
fn test_pop_matching_expired_feeds_expired_count() {
    let mut mb = MultiLaneMailbox::new();
    let mut expired = Envelope::text(to(), "old").with_tag("target");
    expired.expires_at = Some(100);
    mb.push(expired);
    mb.push(Envelope::text(to(), "fresh").with_tag("target"));

    let _ = mb.pop_matching(200, "target").unwrap();
    assert_eq!(mb.take_expired_count(), 1);
}

#[test]
fn test_pop_matching_no_match_returns_none_mailbox_unchanged() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "a"));
    mb.push(Envelope::text(to(), "b").with_tag("other"));

    assert!(mb.pop_matching(0, "missing").is_none());
    assert_eq!(mb.total_len(), 2);
}

#[test]
fn test_pop_still_works_ignores_tags() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "a").with_tag("x"));
    mb.push(Envelope::text(to(), "b").with_tag("y"));

    let env = mb.pop(0).unwrap();
    assert_eq!(env.tag.as_deref(), Some("x"));
    assert_eq!(mb.total_len(), 1);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p zeptovm --lib -- mailbox::tests`
Expected: FAIL — `pop_matching` doesn't exist yet

**Step 3: Implement `pop_matching`**

**IMPORTANT:** Do NOT create an array of `&mut` lane references — that causes overlapping mutable borrows and won't compile. Instead, use a helper function called sequentially per lane:

```rust
/// Helper: scan a lane for first non-expired message with matching tag.
/// Removes expired messages encountered during scan.
/// Returns (matched_envelope_or_none, expired_count).
fn scan_lane_for_tag(
    lane: &mut VecDeque<Envelope>,
    now_ms: u64,
    tag: &str,
) -> (Option<Envelope>, usize) {
    // First, remove expired entries and count them
    let before = lane.len();
    lane.retain(|env| !is_expired(env, now_ms));
    let expired = before - lane.len();

    // Find first matching tag
    if let Some(idx) =
        lane.iter().position(|env| env.tag.as_deref() == Some(tag))
    {
        (lane.remove(idx), expired)
    } else {
        (None, expired)
    }
}
```

Then the method:

```rust
/// Selective receive: pop first non-expired message matching `tag`.
/// Scans lanes in priority order (control > supervisor > effect > user > background).
/// Non-matching and untagged messages stay in place.
/// Expired messages encountered during scan are discarded and counted.
pub fn pop_matching(
    &mut self,
    now_ms: u64,
    tag: &str,
) -> Option<Envelope> {
    // Scan each lane sequentially to avoid overlapping &mut borrows
    let (result, exp) =
        scan_lane_for_tag(&mut self.control, now_ms, tag);
    self.expired_count += exp;
    if result.is_some() {
        return result;
    }

    let (result, exp) =
        scan_lane_for_tag(&mut self.supervisor, now_ms, tag);
    self.expired_count += exp;
    if result.is_some() {
        return result;
    }

    let (result, exp) =
        scan_lane_for_tag(&mut self.effect, now_ms, tag);
    self.expired_count += exp;
    if result.is_some() {
        return result;
    }

    let (result, exp) =
        scan_lane_for_tag(&mut self.user, now_ms, tag);
    self.expired_count += exp;
    if result.is_some() {
        return result;
    }

    let (result, exp) =
        scan_lane_for_tag(&mut self.background, now_ms, tag);
    self.expired_count += exp;
    result
}
```

**Step 4: Add `pop_control()` method**

Selective receive must not block control-lane signals (Kill, Shutdown, ExitLinked, etc.). `step()` needs to drain the control lane before calling `pop_matching()`. Add a method that pops from the control lane only:

```rust
/// Pop the next non-expired message from the control lane only.
/// Used by step() to ensure signals bypass tag filtering.
pub fn pop_control(&mut self, now_ms: u64) -> Option<Envelope> {
    let (result, exp) =
        pop_lane_alive(&mut self.control, now_ms);
    self.expired_count += exp;
    result
}
```

Add a test:

```rust
#[test]
fn test_pop_control_only_drains_control_lane() {
    let mut mb = MultiLaneMailbox::new();
    mb.push(Envelope::text(to(), "user msg"));
    mb.push(Envelope::signal(to(), Signal::Kill));

    // pop_control only returns control lane messages
    let env = mb.pop_control(0).unwrap();
    assert_eq!(env.class, MessageClass::Control);

    // User message still there
    assert!(mb.pop_control(0).is_none());
    assert_eq!(mb.total_len(), 1);
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p zeptovm --lib -- mailbox::tests`
Expected: All tests pass (TTL tests + 7 selective receive tests + 1 pop_control test)

**Step 6: Commit**

```bash
git add zeptovm/src/kernel/mailbox.rs
git commit -m "feat(mailbox): add pop_matching() and pop_control() for selective receive"
```

---

### Task 5: Wire selective receive end-to-end (StepResult + ProcessState + Scheduler)

This task has four specific pitfalls that MUST be handled:

1. **Scheduler line 188 overwrites state to Running** — must preserve `WaitingTagged`
2. **None from pop_matching must not downgrade to WaitingMessage** — must re-enter `WaitForTag`
3. **ProcessRuntimeState derives Copy** — adding `WaitingTagged(String)` breaks Copy; must remove it
4. **6 wake-up sites check only WaitingMessage** — all must also wake `WaitingTagged`

**Files:**
- Modify: `zeptovm/src/core/step_result.rs:8-18` (add WaitForTag variant)
- Modify: `zeptovm/src/kernel/process_table.rs:17-28` (add WaitingTagged, remove Copy derive)
- Modify: `zeptovm/src/kernel/process_table.rs:92-184` (state-aware pop + None handling)
- Modify: `zeptovm/src/kernel/scheduler.rs:95-105` (send wake-up)
- Modify: `zeptovm/src/kernel/scheduler.rs:107-130` (deliver_effect_result wake-up)
- Modify: `zeptovm/src/kernel/scheduler.rs:178-207` (step_process: preserve WaitingTagged, handle WaitForTag result)
- Modify: `zeptovm/src/kernel/scheduler.rs:283-328` (result match: add WaitForTag arm)
- Modify: `zeptovm/src/kernel/scheduler.rs:334-398` (propagate_exit: 3 wake-up sites)
- Modify: `zeptovm/src/kernel/scheduler.rs:530-550` (advance_clock: timer wake-up)
- Modify: All sites using `== ProcessRuntimeState::*` (must use `matches!()` after Copy removal)

**Step 1: Add `WaitForTag` to `StepResult`**

In `step_result.rs`, add variant:

```rust
pub enum StepResult {
    Continue,
    Wait,
    /// Wait for a message with a specific tag (selective receive).
    WaitForTag(String),
    Suspend(EffectRequest),
    Done(Reason),
    Fail(Reason),
}
```

Add a test:

```rust
#[test]
fn test_step_result_wait_for_tag() {
    let r = StepResult::WaitForTag("effect_result".into());
    match r {
        StepResult::WaitForTag(tag) => {
            assert_eq!(tag, "effect_result")
        }
        _ => panic!("expected WaitForTag"),
    }
}
```

**Step 2: Add `WaitingTagged` to `ProcessRuntimeState` and remove `Copy`**

In `process_table.rs:17`, change the derive from:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRuntimeState {
```

to:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessRuntimeState {
```

Then add the new variant:

```rust
pub enum ProcessRuntimeState {
    Ready,
    Running,
    WaitingMessage,
    WaitingTagged(String),
    WaitingEffect(u64),
    WaitingTimer,
    Checkpointing,
    Suspended,
    Failed,
    Done,
}
```

**Step 3: Fix all `==` comparisons broken by Copy removal**

After removing `Copy`, all `proc.state == ProcessRuntimeState::X` comparisons will still compile for variants without data (like `Ready`, `Done`, `Failed`, `WaitingMessage`) because they derive `PartialEq`. However, `matches!()` is more idiomatic for pattern-matching enums with data. Change the `WaitingEffect(_)` matches that currently use `matches!()` — those are fine. The `==` comparisons for simple variants are fine too since `PartialEq` is still derived.

The key places that won't compile are any that try to **copy** the state. Search for assignments like `let x = proc.state` that relied on Copy — these need `.clone()`. Also check for function returns like `Option<ProcessRuntimeState>` in `process_state()` which returns a copied value.

Specifically, in `scheduler.rs:499`:

```rust
pub fn process_state(&self, pid: Pid) -> Option<ProcessRuntimeState> {
    self.processes.get(&pid).map(|p| p.state)
}
```

This needs `.clone()`:

```rust
pub fn process_state(&self, pid: Pid) -> Option<ProcessRuntimeState> {
    self.processes.get(&pid).map(|p| p.state.clone())
}
```

Search for similar patterns in `runtime.rs:608` and `recovery.rs:191`. Fix all.

**Step 4: Update `ProcessEntry::step()` with state-aware pop, control-lane bypass, explicit Running on match, correct None handling**

**Critical invariants:**
1. Control-lane signals (Kill, Shutdown, ExitLinked, etc.) must ALWAYS be delivered, even during selective receive. `pop_control()` is checked first regardless of state.
2. `step()` must set `self.state = Running` when a message is matched, to exit selective mode.
3. When `pop_matching()` returns `None`, return `WaitForTag(tag)` (not `Wait`) to preserve the filter.

Replace the mailbox pop section in `step()`:

```rust
// ALWAYS check control lane first — signals must not be blocked
// by tag filtering. Kill, Shutdown, ExitLinked, etc. take priority.
if let Some(env) = self.mailbox.pop_control(now_ms) {
    self.state = ProcessRuntimeState::Running;
    // Handle the control signal (existing signal handling code)
    // ... (the existing if let EnvelopePayload::Signal check) ...
    // Falls through to behavior.handle() for trappable signals
}

// Pop next message based on process state.
// WaitingTagged uses selective receive for non-control lanes.
let (maybe_env, waiting_tag) = match &self.state {
    ProcessRuntimeState::WaitingTagged(tag) => {
        let t = tag.clone();
        (self.mailbox.pop_matching(now_ms, &t), Some(t))
    }
    _ => (self.mailbox.pop(now_ms), None),
};

match maybe_env {
    Some(env) => {
        // IMPORTANT: Clear selective mode. The process now has
        // a message and enters normal handling. Without this,
        // WaitingTagged leaks into subsequent turns.
        self.state = ProcessRuntimeState::Running;

        // ... rest of existing Some(env) arm unchanged:
        // signal checks, behavior.handle(), etc.
    }
    None => {
        // No message available. If we were doing selective receive,
        // return WaitForTag to preserve the tag filter.
        // Otherwise return Wait as before.
        match waiting_tag {
            Some(tag) => (StepResult::WaitForTag(tag), ctx),
            None => (StepResult::Wait, ctx),
        }
    }
}
```

**Implementation note:** The control-lane check must be structured so that if a signal is found, it is handled immediately (Kill → Done, Shutdown → Done, Suspend → Wait, etc.) and the function returns. Only if no control message exists do we proceed to the state-aware pop. The existing signal-handling code in `step()` (lines 106-141) already does this via early returns. The refactor is:

```rust
// 1. Always drain control lane first (bypasses tag filter)
if let Some(env) = self.mailbox.pop_control(now_ms) {
    self.state = ProcessRuntimeState::Running;
    if let EnvelopePayload::Signal(ref sig) = env.payload {
        match sig {
            Signal::Kill => {
                self.state = ProcessRuntimeState::Done;
                return (StepResult::Done(Reason::Kill), ctx);
            }
            Signal::Shutdown => {
                self.state = ProcessRuntimeState::Done;
                return (
                    StepResult::Done(Reason::Shutdown),
                    ctx,
                );
            }
            Signal::Suspend => {
                self.state = ProcessRuntimeState::Suspended;
                return (StepResult::Wait, ctx);
            }
            Signal::Resume => {
                return (StepResult::Continue, ctx);
            }
            Signal::ExitLinked(_, reason)
                if !self.trap_exit
                    && reason.is_abnormal() =>
            {
                self.state = ProcessRuntimeState::Done;
                return (
                    StepResult::Done(reason.clone()),
                    ctx,
                );
            }
            Signal::ExitLinked(_, _)
                if !self.trap_exit =>
            {
                return (StepResult::Continue, ctx);
            }
            // Trappable signals fall through to behavior
            _ => {}
        }
    }
    // Control message that's not an early-return signal
    // (or trappable signal) → dispatch to behavior
    self.reductions += 1;
    let start = std::time::Instant::now();
    let result =
        panic::catch_unwind(AssertUnwindSafe(|| {
            self.behavior.handle(env, &mut ctx)
        }));
    // ... (existing elapsed/overrun/panic handling) ...
    return match result {
        Ok(step) => (step, ctx),
        Err(panic_info) => {
            // ... existing panic handling ...
        }
    };
}

// 2. No control message. State-aware pop for remaining lanes.
let (maybe_env, waiting_tag) = match &self.state {
    ProcessRuntimeState::WaitingTagged(tag) => {
        let t = tag.clone();
        (self.mailbox.pop_matching(now_ms, &t), Some(t))
    }
    _ => (self.mailbox.pop(now_ms), None),
};

match maybe_env {
    Some(env) => {
        self.state = ProcessRuntimeState::Running;
        // ... existing behavior dispatch ...
    }
    None => match waiting_tag {
        Some(tag) => (StepResult::WaitForTag(tag), ctx),
        None => (StepResult::Wait, ctx),
    },
}
```

**Step 5: Update scheduler `step_process()` to preserve WaitingTagged state**

In `scheduler.rs:178-191`, the scheduler currently unconditionally sets `proc.state = ProcessRuntimeState::Running` before calling `step()`. This would destroy the `WaitingTagged` state that `step()` needs to read. Fix:

```rust
fn step_process(&mut self, pid: Pid) {
    let _span =
        info_span!("process_step", pid = pid.raw()).entered();
    // Skip terminated processes. Do NOT overwrite WaitingTagged —
    // step() reads it to choose pop vs pop_matching.
    if let Some(proc) = self.processes.get_mut(&pid) {
        match &proc.state {
            ProcessRuntimeState::Done
            | ProcessRuntimeState::Failed => return,
            ProcessRuntimeState::WaitingTagged(_) => {
                // Keep WaitingTagged — step() will read it
            }
            _ => {
                proc.state = ProcessRuntimeState::Running;
            }
        }
    } else {
        return;
    }
    // ... rest of step_process unchanged ...
```

**Step 6: Handle `WaitForTag` in the result match block**

In the `match result` block (around line 283), add a new arm:

```rust
StepResult::WaitForTag(tag) => {
    if let Some(proc) = self.processes.get_mut(&pid) {
        proc.state =
            ProcessRuntimeState::WaitingTagged(tag);
    }
    return;
}
```

**Step 7: Update all 7 wake-up sites — enqueue without erasing tag**

**Critical invariant: waking a tagged waiter must not destroy the tag.**

For `WaitingMessage`: set state to `Ready` and enqueue (existing behavior).
For `WaitingTagged`: keep state as-is, just enqueue. `step_process()` preserves `WaitingTagged` (Step 5), `step()` reads it for `pop_matching()` (Step 4), and on no match returns `WaitForTag` which re-enters the same state (Step 6).

Helper method:

```rust
/// Wake a process if it's waiting for a message.
/// WaitingMessage → Ready (normal wake).
/// WaitingTagged → stays WaitingTagged, just enqueued
///   (step() will use pop_matching, which preserves the tag filter).
fn wake_if_waiting(&mut self, pid: Pid) {
    if let Some(proc) = self.processes.get_mut(&pid) {
        match &proc.state {
            ProcessRuntimeState::WaitingMessage => {
                proc.state = ProcessRuntimeState::Ready;
                self.ready_queue.push(pid);
            }
            ProcessRuntimeState::WaitingTagged(_) => {
                // DO NOT clear the tag. Just enqueue for stepping.
                // step_process() preserves WaitingTagged state.
                self.ready_queue.push(pid);
            }
            _ => {}
        }
    }
}
```

Apply at all **7** sites (not 6 — includes deliver_streaming_chunk):

**Site 1 — `send()` (line 100):**

```rust
pub fn send(&mut self, env: Envelope) {
    let to = env.to;
    if let Some(proc) = self.processes.get_mut(&to) {
        proc.mailbox.push(env);
    }
    self.wake_if_waiting(to);
}
```

**Site 2 — `deliver_effect_result()` (line 121):** Keep the existing `WaitingEffect` handling, add message-waiting wake:

```rust
pub fn deliver_effect_result(
    &mut self,
    result: EffectResult,
) {
    let effect_raw = result.effect_id.raw();
    if let Some(pending) =
        self.pending_effects.remove(&effect_raw)
    {
        let pid = pending.pid;
        if let Some(proc) = self.processes.get_mut(&pid) {
            proc.mailbox.push(
                Envelope::effect_result(pid, result),
            );
            match &proc.state {
                ProcessRuntimeState::WaitingEffect(_) => {
                    proc.state = ProcessRuntimeState::Ready;
                    self.ready_queue.push(pid);
                }
                ProcessRuntimeState::WaitingMessage => {
                    proc.state = ProcessRuntimeState::Ready;
                    self.ready_queue.push(pid);
                }
                ProcessRuntimeState::WaitingTagged(_) => {
                    // Preserve tag, just enqueue
                    self.ready_queue.push(pid);
                }
                _ => {}
            }
        }
    }
}
```

**Site 3 — `deliver_streaming_chunk()` (line 135):** Currently pushes to mailbox but doesn't wake. Add wake logic:

```rust
pub fn deliver_streaming_chunk(
    &mut self,
    result: EffectResult,
) {
    let effect_raw = result.effect_id.raw();
    if let Some(pending) =
        self.pending_effects.get(&effect_raw)
    {
        let pid = pending.pid;
        if let Some(proc) =
            self.processes.get_mut(&pid)
        {
            proc.mailbox.push(
                Envelope::effect_result(pid, result),
            );
            // Wake if waiting for messages (effect still in-flight)
            match &proc.state {
                ProcessRuntimeState::WaitingMessage => {
                    proc.state = ProcessRuntimeState::Ready;
                    self.ready_queue.push(pid);
                }
                ProcessRuntimeState::WaitingTagged(_) => {
                    self.ready_queue.push(pid);
                }
                _ => {}
            }
        }
    }
}
```

**Sites 4-6 — `propagate_exit()` (lines 348, 368, 391):** Each site pushes a signal envelope then wakes. Signals are control-lane messages that should always wake (even tagged waiters — the signal is important):

```rust
match &proc.state {
    ProcessRuntimeState::WaitingMessage => {
        proc.state = ProcessRuntimeState::Ready;
        self.ready_queue.push(target_pid);
    }
    ProcessRuntimeState::WaitingTagged(_) => {
        // Preserve tag, just enqueue — signals go to
        // control lane which pop() always checks first,
        // and step() handles signals before behavior
        self.ready_queue.push(target_pid);
    }
    _ => {}
}
```

Apply this pattern to all three sites (links, monitors, parent notification).

**Site 7 — `advance_clock()` (line 544):**

```rust
match &proc.state {
    ProcessRuntimeState::WaitingMessage => {
        proc.state = ProcessRuntimeState::Ready;
        self.ready_queue.push(owner);
    }
    ProcessRuntimeState::WaitingTagged(_) => {
        self.ready_queue.push(owner);
    }
    _ => {}
}
```

**Step 8: Handle preemption with WaitingTagged**

In `step_process()` around line 196-207, when reductions run out:

```rust
if reductions_left == 0 {
    if let Some(proc) = self.processes.get_mut(&pid) {
        if proc.mailbox.has_messages() {
            // Re-enqueue. If WaitingTagged, keep tag.
            if !matches!(
                proc.state,
                ProcessRuntimeState::WaitingTagged(_)
            ) {
                proc.state = ProcessRuntimeState::Ready;
            }
            self.ready_queue.push(pid);
        } else {
            // No messages. Preserve WaitingTagged if active.
            if !matches!(
                proc.state,
                ProcessRuntimeState::WaitingTagged(_)
            ) {
                proc.state =
                    ProcessRuntimeState::WaitingMessage;
            }
        }
    }
    return;
}
```

Similarly, in the `StepResult::Continue` arm, when mailbox is empty:

```rust
StepResult::Continue => {
    reductions_left -= 1;
    let proc = match self.processes.get_mut(&pid) {
        Some(p) => p,
        None => return,
    };
    if !proc.mailbox.has_messages() {
        if !matches!(
            proc.state,
            ProcessRuntimeState::WaitingTagged(_)
        ) {
            proc.state =
                ProcessRuntimeState::WaitingMessage;
        }
        return;
    }
    // Continue processing next message
}
```

**Step 9: Add integration tests**

Test 1 — process_table level (selective receive filters correctly):

```rust
#[test]
fn test_selective_receive_filters_by_tag() {
    struct SelectiveBehavior;
    impl StepBehavior for SelectiveBehavior {
        fn handle(
            &mut self,
            envelope: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            match &envelope.payload {
                EnvelopePayload::User(Payload::Text(s))
                    if s == "start" =>
                {
                    StepResult::WaitForTag("reply".into())
                }
                EnvelopePayload::User(Payload::Text(s))
                    if s == "the reply" =>
                {
                    StepResult::Done(Reason::Normal)
                }
                _ => StepResult::Continue,
            }
        }
        fn init(
            &mut self,
            _checkpoint: Option<Vec<u8>>,
        ) -> StepResult {
            StepResult::Continue
        }
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(
        pid,
        Box::new(SelectiveBehavior),
    );

    // Step 1: process "start", get WaitForTag
    p.mailbox.push(Envelope::text(pid, "start"));
    let (result, _) = p.step(0);
    assert!(matches!(
        result,
        StepResult::WaitForTag(ref t) if t == "reply"
    ));

    // Step 2: set WaitingTagged (as scheduler would), send non-matching messages
    p.state =
        ProcessRuntimeState::WaitingTagged("reply".into());
    p.mailbox.push(Envelope::text(pid, "noise"));
    p.mailbox.push(
        Envelope::text(pid, "wrong").with_tag("other"),
    );

    // Step with no matching message — should return WaitForTag again
    let (result, _) = p.step(0);
    assert!(matches!(
        result,
        StepResult::WaitForTag(ref t) if t == "reply"
    ));
    // Non-matching messages still in mailbox
    assert_eq!(p.mailbox.total_len(), 2);

    // Step 3: send matching message
    p.state =
        ProcessRuntimeState::WaitingTagged("reply".into());
    p.mailbox.push(
        Envelope::text(pid, "the reply").with_tag("reply"),
    );
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Done(Reason::Normal)));

    // noise and "wrong" still in mailbox
    assert_eq!(p.mailbox.total_len(), 2);
}
```

Test 2 — verify WaitForTag with no match preserves tag (doesn't downgrade):

```rust
#[test]
fn test_selective_receive_no_match_preserves_wait_tag() {
    struct TagWaiter;
    impl StepBehavior for TagWaiter {
        fn handle(
            &mut self,
            _envelope: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            StepResult::WaitForTag("expected".into())
        }
        fn init(
            &mut self,
            _checkpoint: Option<Vec<u8>>,
        ) -> StepResult {
            StepResult::Continue
        }
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(pid, Box::new(TagWaiter));

    // Trigger WaitForTag
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(_)));

    // Set tagged state, step with empty mailbox
    p.state =
        ProcessRuntimeState::WaitingTagged("expected".into());
    let (result, _) = p.step(0);
    // Must return WaitForTag, NOT Wait
    match result {
        StepResult::WaitForTag(tag) => {
            assert_eq!(tag, "expected")
        }
        StepResult::Wait => {
            panic!("BUG: downgraded WaitForTag to Wait")
        }
        other => panic!("unexpected: {:?}", other),
    }
}
```

Test 3 — verify tag survives wake→step cycle (the core invariant):

```rust
#[test]
fn test_selective_receive_tag_survives_wake_cycle() {
    struct TaggedReceiver {
        phase: u8,
    }
    impl StepBehavior for TaggedReceiver {
        fn handle(
            &mut self,
            envelope: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            match self.phase {
                0 => {
                    // First message triggers selective wait
                    self.phase = 1;
                    StepResult::WaitForTag("target".into())
                }
                1 => {
                    // Should only get here with tagged message
                    assert_eq!(
                        envelope.tag.as_deref(),
                        Some("target")
                    );
                    self.phase = 2;
                    StepResult::Done(Reason::Normal)
                }
                _ => StepResult::Continue,
            }
        }
        fn init(
            &mut self,
            _checkpoint: Option<Vec<u8>>,
        ) -> StepResult {
            StepResult::Continue
        }
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(
        pid,
        Box::new(TaggedReceiver { phase: 0 }),
    );

    // Phase 0: trigger WaitForTag
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(_)));

    // Simulate wake_if_waiting: scheduler enqueues pid
    // but does NOT change state from WaitingTagged
    p.state =
        ProcessRuntimeState::WaitingTagged("target".into());
    // Send non-matching message (simulates the wake trigger)
    p.mailbox.push(Envelope::text(pid, "noise"));

    // step_process would NOT overwrite WaitingTagged to Running.
    // step() should use pop_matching, find no match, return WaitForTag.
    let (result, _) = p.step(0);
    assert!(matches!(
        result,
        StepResult::WaitForTag(ref t) if t == "target"
    ));
    // State should be set back by scheduler to WaitingTagged
    // (we simulate that here)
    p.state =
        ProcessRuntimeState::WaitingTagged("target".into());

    // Now send matching message
    p.mailbox.push(
        Envelope::text(pid, "match").with_tag("target"),
    );
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Done(Reason::Normal)));

    // After match, step() set state to Running (clearing tag)
    assert_eq!(p.state, ProcessRuntimeState::Running);

    // noise still in mailbox (was skipped by pop_matching)
    assert_eq!(p.mailbox.total_len(), 1);
}
```

Test 4 — **REGRESSION: control signals bypass tag filter during selective receive:**

```rust
#[test]
fn test_selective_receive_kill_bypasses_tag_filter() {
    struct NeverDone;
    impl StepBehavior for NeverDone {
        fn handle(
            &mut self,
            _envelope: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            // Always ask for a tagged message
            StepResult::WaitForTag("reply".into())
        }
        fn init(
            &mut self,
            _checkpoint: Option<Vec<u8>>,
        ) -> StepResult {
            StepResult::Continue
        }
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(pid, Box::new(NeverDone));

    // Enter WaitingTagged state
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(_)));
    p.state =
        ProcessRuntimeState::WaitingTagged("reply".into());

    // Send a Kill signal (untagged, control lane)
    p.mailbox.push(Envelope::signal(pid, Signal::Kill));

    // step() must handle Kill immediately despite WaitingTagged
    let (result, _) = p.step(0);
    assert!(
        matches!(result, StepResult::Done(Reason::Kill)),
        "Kill signal must bypass tag filter, got: {:?}",
        result
    );
    assert_eq!(p.state, ProcessRuntimeState::Done);
}

#[test]
fn test_selective_receive_shutdown_bypasses_tag_filter() {
    struct TagWaiter;
    impl StepBehavior for TagWaiter {
        fn handle(
            &mut self,
            _envelope: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            StepResult::WaitForTag("reply".into())
        }
        fn init(
            &mut self,
            _checkpoint: Option<Vec<u8>>,
        ) -> StepResult {
            StepResult::Continue
        }
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(pid, Box::new(TagWaiter));

    // Enter WaitingTagged
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.state =
        ProcessRuntimeState::WaitingTagged("reply".into());

    // Send Shutdown (untagged, control lane)
    p.mailbox
        .push(Envelope::signal(pid, Signal::Shutdown));

    let (result, _) = p.step(0);
    assert!(
        matches!(result, StepResult::Done(Reason::Shutdown)),
        "Shutdown must bypass tag filter"
    );
}

#[test]
fn test_selective_receive_exit_linked_bypasses_tag_filter() {
    struct TagWaiter;
    impl StepBehavior for TagWaiter {
        fn handle(
            &mut self,
            _envelope: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            StepResult::WaitForTag("reply".into())
        }
        fn init(
            &mut self,
            _checkpoint: Option<Vec<u8>>,
        ) -> StepResult {
            StepResult::Continue
        }
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(pid, Box::new(TagWaiter));

    // Enter WaitingTagged
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.state =
        ProcessRuntimeState::WaitingTagged("reply".into());

    // Send ExitLinked with abnormal reason (control lane)
    p.mailbox.push(Envelope::signal(
        pid,
        Signal::ExitLinked(
            Pid::from_raw(2),
            Reason::Custom("crashed".into()),
        ),
    ));

    let (result, _) = p.step(0);
    assert!(
        matches!(result, StepResult::Done(_)),
        "ExitLinked(abnormal) must bypass tag filter"
    );
}
```

**Step 10: Run full test suite**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 11: Commit**

```bash
git add zeptovm/src/core/step_result.rs zeptovm/src/kernel/process_table.rs zeptovm/src/kernel/scheduler.rs zeptovm/src/kernel/runtime.rs zeptovm/src/kernel/recovery.rs
git commit -m "feat(selective-receive): end-to-end WaitForTag with control-lane bypass and scheduler state preservation"
```

---

### Task 6: Add `messages.expired` metric with data path

**Files:**
- Modify: `zeptovm/src/kernel/metrics.rs:17-35` (register counter)
- Modify: `zeptovm/src/kernel/scheduler.rs` (drain expired_count after step)

**Step 1: Register `"messages.expired"` counter in metrics**

In `metrics.rs`, add `"messages.expired"` to the counters array:

```rust
for name in [
    "processes.spawned",
    "processes.exited",
    "effects.dispatched",
    "effects.completed",
    "effects.failed",
    "effects.retries",
    "effects.timed_out",
    "turns.committed",
    "timers.scheduled",
    "timers.fired",
    "supervisor.restarts",
    "compensation.triggered",
    "budget.blocked",
    "policy.blocked",
    "scheduler.ticks",
    "messages.expired",
] {
```

**Step 2: Drain expired_count in scheduler after stepping a process**

In `scheduler.rs`, after the `proc.step(self.clock_ms)` call and before processing intents, drain the expired counter. Find the block around line 210-218:

```rust
let (result, intents) = {
    let proc = match self.processes.get_mut(&pid) {
        Some(p) => p,
        None => return,
    };
    let (result, mut ctx) = proc.step(self.clock_ms);
    let intents = ctx.take_intents();
    (result, intents)
};
```

This is tricky because metrics is on Runtime, not Scheduler. Two options:
- (a) Return the expired count from `tick_process` and let Runtime handle it
- (b) Add a `pending_expired_count` field to SchedulerEngine that Runtime drains

Option (b) is simpler:

Add to `SchedulerEngine`:
```rust
/// Accumulated expired message count for metrics emission.
pending_expired_count: usize,
```

Initialize in `new()`:
```rust
pending_expired_count: 0,
```

After the step call, drain the mailbox's counter:

```rust
let (result, intents, expired) = {
    let proc = match self.processes.get_mut(&pid) {
        Some(p) => p,
        None => return,
    };
    let (result, mut ctx) = proc.step(self.clock_ms);
    let expired = proc.mailbox.take_expired_count();
    let intents = ctx.take_intents();
    (result, intents, expired)
};
self.pending_expired_count += expired;
```

Add public accessor:
```rust
pub fn take_pending_expired_count(&mut self) -> usize {
    let c = self.pending_expired_count;
    self.pending_expired_count = 0;
    c
}
```

In `runtime.rs` `tick()`, after calling `self.engine.tick_process(pid)`, drain and emit:
```rust
let expired = self.engine.take_pending_expired_count();
if expired > 0 {
    self.metrics.inc_by("messages.expired", expired as u64);
}
```

**Step 3: Add test in metrics**

```rust
#[test]
fn test_metrics_messages_expired() {
    let m = Metrics::new();
    m.inc_by("messages.expired", 5);
    assert_eq!(m.counter("messages.expired"), 5);
}
```

**Step 4: Run tests**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/metrics.rs zeptovm/src/kernel/scheduler.rs zeptovm/src/kernel/runtime.rs
git commit -m "feat(metrics): wire messages.expired counter through scheduler to runtime"
```

---

### Task 7: Update gap analysis

**Files:**
- Modify: `docs/plans/2026-03-06-spec-v03-gap-analysis.md`

**Step 1: Update X2 and X3 status**

In the "Additional Gaps" table, update:
- X2 row: Status → `DONE`, Notes → `Tag-based selective receive: pop_matching() + WaitForTag StepResult + WaitingTagged process state + scheduler wiring`
- X3 row: Status → `DONE`, Notes → `expires_at field + lazy TTL in pop() + reap_expired() + expired_count metrics path`

In the "Tier 3" section, update both items:
- **X3:** DONE — Per-message TTL
- **X2:** DONE — Selective receive (end-to-end)

**Step 2: Commit**

```bash
git add docs/plans/2026-03-06-spec-v03-gap-analysis.md
git commit -m "docs: mark X2 and X3 as DONE in gap analysis"
```
