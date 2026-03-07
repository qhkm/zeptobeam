# Tier 3 — Mailbox Enhancements Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add per-message TTL and end-to-end tag-based selective receive to the ZeptoVM runtime.

**Architecture:** Two new optional fields on Envelope (`expires_at`, `tag`). TTL is enforced lazily in `pop()`/`pop_matching()` with an `expired_count` accumulator the runtime drains for metrics. Selective receive is wired end-to-end: `StepResult::WaitForTag(String)` → scheduler sets `proc.selective_tag = Some(tag)` + `proc.state = WaitingMessage` → `step()` reads `selective_tag` to choose `pop_matching()` vs `pop()`. Control-lane signals always bypass via `pop_control()`. No new state variants — `ProcessRuntimeState` keeps `Copy`. Envelope stays dumb (no wall-clock access) — only `with_expires_at(epoch_ms)` builder, caller computes absolute time.

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

### Task 5: Wire selective receive end-to-end (StepResult + selective_tag field + Scheduler)

**Design:** The tag filter lives in a `selective_tag: Option<String>` field on `ProcessEntry`, NOT as a state enum variant. This means:
- `ProcessRuntimeState` is unchanged (keeps `Copy`, no `WaitingTagged` variant)
- Wake-up sites are unchanged (they only check `WaitingMessage → Ready`)
- No queue duplication (state transitions are one-shot like today)
- `step()` reads `selective_tag` to choose `pop_matching()` vs `pop()`

Lifecycle:
```
behavior returns WaitForTag("reply")
  → scheduler sets proc.selective_tag = Some("reply")
  → scheduler sets proc.state = WaitingMessage

message arrives → WaitingMessage → Ready (one-shot, normal)

step():
  1. pop_control() — always, signals bypass tag filter
  2. if selective_tag.is_some() → pop_matching(tag)
     - match → selective_tag = None, dispatch to behavior
     - no match → return WaitForTag(tag)
  3. else → pop() as normal
```

**Files:**
- Modify: `zeptovm/src/core/step_result.rs:8-18` (add WaitForTag variant)
- Modify: `zeptovm/src/kernel/process_table.rs:31-44` (add selective_tag field)
- Modify: `zeptovm/src/kernel/process_table.rs:92-184` (control-lane bypass + selective pop)
- Modify: `zeptovm/src/kernel/scheduler.rs:283-328` (handle WaitForTag result)

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

**Step 2: Add `selective_tag` field to `ProcessEntry`**

In `process_table.rs`, add to the struct (NO changes to `ProcessRuntimeState`):

```rust
pub struct ProcessEntry {
    pub pid: Pid,
    pub state: ProcessRuntimeState,
    pub mailbox: MultiLaneMailbox,
    pub parent: Option<Pid>,
    pub trap_exit: bool,
    /// Tag filter for selective receive. When Some, step() uses
    /// pop_matching() instead of pop(). Cleared on match.
    pub selective_tag: Option<String>,
    behavior: Box<dyn StepBehavior>,
    // ... rest unchanged ...
}
```

Initialize in `new()`:

```rust
selective_tag: None,
```

**Step 3: Refactor `ProcessEntry::step()` with control-lane bypass and selective pop**

The refactored `step()` has two phases:
1. **Always drain control lane first** via `pop_control()` — handles Kill, Shutdown, ExitLinked etc. regardless of selective_tag.
2. **State-aware pop** — if `selective_tag` is set, use `pop_matching()`; otherwise use `pop()`.

```rust
pub fn step(&mut self, now_ms: u64) -> (StepResult, TurnContext) {
    let mut ctx = TurnContext::new(self.pid);
    self.turn_overrun_flag = false;
    self.last_turn_duration = None;

    // Kill check (highest priority, like BEAM)
    if self.kill_requested {
        self.state = ProcessRuntimeState::Done;
        self.selective_tag = None;
        return (StepResult::Done(Reason::Kill), ctx);
    }

    // Phase 1: ALWAYS drain control lane first.
    // Signals must never be blocked by tag filtering.
    if let Some(env) = self.mailbox.pop_control(now_ms) {
        self.state = ProcessRuntimeState::Running;
        self.selective_tag = None; // signal clears selective mode

        if let EnvelopePayload::Signal(ref sig) = env.payload
        {
            match sig {
                Signal::Kill => {
                    self.state = ProcessRuntimeState::Done;
                    return (
                        StepResult::Done(Reason::Kill),
                        ctx,
                    );
                }
                Signal::Shutdown => {
                    self.state = ProcessRuntimeState::Done;
                    return (
                        StepResult::Done(Reason::Shutdown),
                        ctx,
                    );
                }
                Signal::Suspend => {
                    self.state =
                        ProcessRuntimeState::Suspended;
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

        // Control message not handled by early return
        // (trappable signal or non-signal control) → behavior
        self.reductions += 1;
        let start = std::time::Instant::now();
        let result =
            panic::catch_unwind(AssertUnwindSafe(|| {
                self.behavior.handle(env, &mut ctx)
            }));
        let elapsed = start.elapsed();
        self.last_turn_duration = Some(elapsed);
        if elapsed > self.max_turn_wall_clock {
            self.turn_overrun_flag = true;
            tracing::warn!(
                pid = self.pid.raw(),
                elapsed_ms = elapsed.as_millis() as u64,
                limit_ms = self
                    .max_turn_wall_clock
                    .as_millis()
                    as u64,
                "handler overrun"
            );
        }
        return match result {
            Ok(step) => (step, ctx),
            Err(panic_info) => {
                let msg = if let Some(s) =
                    panic_info.downcast_ref::<&str>()
                {
                    format!("panic: {s}")
                } else if let Some(s) =
                    panic_info.downcast_ref::<String>()
                {
                    format!("panic: {s}")
                } else {
                    "panic: unknown".to_string()
                };
                self.state = ProcessRuntimeState::Failed;
                self.selective_tag = None;
                (StepResult::Fail(Reason::Custom(msg)), ctx)
            }
        };
    }

    // Phase 2: No control message. Pop based on selective_tag.
    let maybe_env = if let Some(ref tag) = self.selective_tag
    {
        self.mailbox.pop_matching(now_ms, tag)
    } else {
        self.mailbox.pop(now_ms)
    };

    match maybe_env {
        Some(env) => {
            self.state = ProcessRuntimeState::Running;
            self.selective_tag = None; // matched, clear filter

            // Check for signals that leaked to non-control lanes
            // (shouldn't happen, but defensive)
            if let EnvelopePayload::Signal(ref sig) =
                env.payload
            {
                match sig {
                    Signal::Kill => {
                        self.state =
                            ProcessRuntimeState::Done;
                        return (
                            StepResult::Done(Reason::Kill),
                            ctx,
                        );
                    }
                    Signal::Shutdown => {
                        self.state =
                            ProcessRuntimeState::Done;
                        return (
                            StepResult::Done(
                                Reason::Shutdown,
                            ),
                            ctx,
                        );
                    }
                    _ => {}
                }
            }

            // Dispatch to behavior
            self.reductions += 1;
            let start = std::time::Instant::now();
            let result =
                panic::catch_unwind(AssertUnwindSafe(|| {
                    self.behavior.handle(env, &mut ctx)
                }));
            let elapsed = start.elapsed();
            self.last_turn_duration = Some(elapsed);
            if elapsed > self.max_turn_wall_clock {
                self.turn_overrun_flag = true;
                tracing::warn!(
                    pid = self.pid.raw(),
                    elapsed_ms =
                        elapsed.as_millis() as u64,
                    limit_ms = self
                        .max_turn_wall_clock
                        .as_millis()
                        as u64,
                    "handler overrun"
                );
            }
            match result {
                Ok(step) => (step, ctx),
                Err(panic_info) => {
                    let msg = if let Some(s) =
                        panic_info.downcast_ref::<&str>()
                    {
                        format!("panic: {s}")
                    } else if let Some(s) =
                        panic_info.downcast_ref::<String>()
                    {
                        format!("panic: {s}")
                    } else {
                        "panic: unknown".to_string()
                    };
                    self.state = ProcessRuntimeState::Failed;
                    (
                        StepResult::Fail(Reason::Custom(msg)),
                        ctx,
                    )
                }
            }
        }
        None => {
            // No message. If selective receive was active,
            // return WaitForTag to preserve the filter.
            // selective_tag is still Some (not cleared above).
            if let Some(tag) = self.selective_tag.take() {
                (StepResult::WaitForTag(tag), ctx)
            } else {
                (StepResult::Wait, ctx)
            }
        }
    }
}
```

**Step 4: Handle `WaitForTag` in the scheduler result match**

In `scheduler.rs`, in the `match result` block (around line 283), add a new arm. The scheduler sets `selective_tag` and puts the process in `WaitingMessage` — the same state used for untagged waits. This means all existing wake-up sites work without modification.

```rust
StepResult::WaitForTag(tag) => {
    if let Some(proc) = self.processes.get_mut(&pid) {
        proc.selective_tag = Some(tag);
        proc.state = ProcessRuntimeState::WaitingMessage;
    }
    return;
}
```

**No changes to wake-up sites.** `WaitingMessage → Ready` is the existing one-shot wake pattern. When the process is stepped, `step()` reads `selective_tag` and uses `pop_matching()`. If no match, `WaitForTag` is returned, scheduler sets `selective_tag` + `WaitingMessage` again. No duplicate queueing.

**Step 5: Add integration tests**

Test 1 — selective receive filters by tag, non-matching messages preserved:

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

    // Process "start", get WaitForTag
    p.mailbox.push(Envelope::text(pid, "start"));
    let (result, _) = p.step(0);
    assert!(matches!(
        result,
        StepResult::WaitForTag(ref t) if t == "reply"
    ));

    // Simulate scheduler: set selective_tag + WaitingMessage
    p.selective_tag = Some("reply".into());
    p.mailbox.push(Envelope::text(pid, "noise"));
    p.mailbox.push(
        Envelope::text(pid, "wrong").with_tag("other"),
    );

    // No match → WaitForTag again
    let (result, _) = p.step(0);
    assert!(matches!(
        result,
        StepResult::WaitForTag(ref t) if t == "reply"
    ));
    assert_eq!(p.mailbox.total_len(), 2);

    // Send matching message
    p.selective_tag = Some("reply".into());
    p.mailbox.push(
        Envelope::text(pid, "the reply").with_tag("reply"),
    );
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Done(Reason::Normal)));
    assert!(p.selective_tag.is_none()); // cleared on match
    assert_eq!(p.mailbox.total_len(), 2); // noise + wrong
}
```

Test 2 — no match returns WaitForTag (not Wait):

```rust
#[test]
fn test_selective_receive_no_match_returns_wait_for_tag() {
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

    // Set selective_tag, step with empty mailbox
    p.selective_tag = Some("expected".into());
    let (result, _) = p.step(0);
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

Test 3 — control signals bypass tag filter:

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

    // Enter selective mode
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.selective_tag = Some("reply".into());

    // Send Kill (control lane, untagged)
    p.mailbox.push(Envelope::signal(pid, Signal::Kill));

    let (result, _) = p.step(0);
    assert!(
        matches!(result, StepResult::Done(Reason::Kill)),
        "Kill must bypass tag filter"
    );
    assert_eq!(p.state, ProcessRuntimeState::Done);
    assert!(p.selective_tag.is_none());
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
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.selective_tag = Some("reply".into());

    p.mailbox
        .push(Envelope::signal(pid, Signal::Shutdown));
    let (result, _) = p.step(0);
    assert!(matches!(
        result,
        StepResult::Done(Reason::Shutdown)
    ));
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
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.selective_tag = Some("reply".into());

    p.mailbox.push(Envelope::signal(
        pid,
        Signal::ExitLinked(
            Pid::from_raw(2),
            Reason::Custom("crashed".into()),
        ),
    ));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Done(_)));
}
```

Test 4 — **REGRESSION: no queue duplication from multiple non-matching arrivals:**

```rust
#[test]
fn test_selective_receive_no_queue_duplication() {
    // This test verifies the scheduler-level invariant:
    // a process in WaitingMessage (with selective_tag set)
    // only gets enqueued once per wake, and non-matching
    // messages don't cause duplicate stepping.

    struct CountingBehavior {
        handle_count: u32,
    }
    impl StepBehavior for CountingBehavior {
        fn handle(
            &mut self,
            _envelope: Envelope,
            _ctx: &mut TurnContext,
        ) -> StepResult {
            self.handle_count += 1;
            StepResult::WaitForTag("target".into())
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
        Box::new(CountingBehavior { handle_count: 0 }),
    );

    // Trigger selective mode
    p.mailbox.push(Envelope::text(pid, "go"));
    let _ = p.step(0);

    // Simulate scheduler setting selective_tag + WaitingMessage
    p.selective_tag = Some("target".into());
    p.state = ProcessRuntimeState::WaitingMessage;

    // Deliver 5 non-matching messages. In the old WaitingTagged
    // design, each would push pid to ready_queue. With
    // selective_tag + WaitingMessage, only the first wake
    // transitions to Ready (one-shot).
    for i in 0..5 {
        p.mailbox.push(Envelope::text(
            pid,
            format!("noise-{i}"),
        ));
    }

    // Simulate the wake: WaitingMessage → Ready (once)
    assert_eq!(p.state, ProcessRuntimeState::WaitingMessage);
    p.state = ProcessRuntimeState::Ready;
    // Second "wake" would see Ready, not WaitingMessage — no-op

    // Step once: pop_matching finds no match, returns WaitForTag
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(_)));

    // All 5 noise messages still in mailbox (not consumed)
    assert_eq!(p.mailbox.total_len(), 5);
}
```

**Step 6: Run full test suite**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 7: Commit**

```bash
git add zeptovm/src/core/step_result.rs zeptovm/src/kernel/process_table.rs zeptovm/src/kernel/scheduler.rs
git commit -m "feat(selective-receive): end-to-end WaitForTag via selective_tag field with control-lane bypass"
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
