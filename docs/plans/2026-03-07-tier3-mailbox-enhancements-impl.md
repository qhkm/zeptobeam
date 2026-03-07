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

**Step 4: Run tests to verify they pass**

Run: `cargo test -p zeptovm --lib -- mailbox::tests`
Expected: All tests pass (TTL tests + 7 new selective receive tests)

**Step 5: Commit**

```bash
git add zeptovm/src/kernel/mailbox.rs
git commit -m "feat(mailbox): add tag-based selective receive via pop_matching()"
```

---

### Task 5: Wire selective receive end-to-end (StepResult + ProcessState + Scheduler)

**Files:**
- Modify: `zeptovm/src/core/step_result.rs:8-18` (add WaitForTag variant)
- Modify: `zeptovm/src/kernel/process_table.rs:17-28` (add WaitingTagged state)
- Modify: `zeptovm/src/kernel/process_table.rs:92-119` (step uses state to choose pop vs pop_matching)
- Modify: `zeptovm/src/kernel/scheduler.rs:283-328` (handle WaitForTag result)
- Modify: `zeptovm/src/kernel/scheduler.rs:188-207` (wake WaitingTagged when messages arrive)

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
        StepResult::WaitForTag(tag) => assert_eq!(tag, "effect_result"),
        _ => panic!("expected WaitForTag"),
    }
}
```

**Step 2: Add `WaitingTagged` to `ProcessRuntimeState`**

In `process_table.rs`, add:

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

**Step 3: Update `ProcessEntry::step()` to use state for pop strategy**

Replace the `mailbox.pop(now_ms)` call with state-aware logic:

```rust
// Pop next message based on process state
let maybe_env = match &self.state {
    ProcessRuntimeState::WaitingTagged(tag) => {
        self.mailbox.pop_matching(now_ms, tag)
    }
    _ => self.mailbox.pop(now_ms),
};

match maybe_env {
    Some(env) => {
        // Reset state to Running now that we have a message
        self.state = ProcessRuntimeState::Running;
        // ... rest of existing match arm ...
```

**Step 4: Update scheduler to handle `WaitForTag` result**

In `scheduler.rs`, in the `match result` block (around line 283), add handling for `WaitForTag`:

```rust
StepResult::WaitForTag(tag) => {
    if let Some(proc) = self.processes.get_mut(&pid) {
        // Check if there's already a matching message
        if proc.mailbox.pop_matching(self.clock_ms, &tag).is_some() {
            // Message already available — re-queue immediately
            // (put it back, we just peeked)
            // Actually: simpler to just re-enqueue process as Ready
            // and let next step() pick it up with WaitingTagged state
            proc.state = ProcessRuntimeState::WaitingTagged(tag);
            proc.state = ProcessRuntimeState::Ready;
            self.ready_queue.push(pid);
        } else {
            proc.state = ProcessRuntimeState::WaitingTagged(tag);
        }
    }
    return;
}
```

Wait — that's too complex. Simpler approach: just set the state and let the existing wake-up logic handle it.

```rust
StepResult::WaitForTag(tag) => {
    if let Some(proc) = self.processes.get_mut(&pid) {
        proc.state =
            ProcessRuntimeState::WaitingTagged(tag);
    }
    return;
}
```

**Step 5: Update the wake-up logic for message delivery**

In `scheduler.rs`, where messages are delivered to mailboxes and processes are woken up (the section around lines 188-207 that checks `WaitingMessage`), also wake `WaitingTagged` processes — but only if the delivered message's tag matches:

Find the code that delivers messages and wakes waiting processes. After delivering a message to a process's mailbox, add:

```rust
ProcessRuntimeState::WaitingTagged(ref tag)
    if env.tag.as_deref() == Some(tag.as_str()) =>
{
    proc.state = ProcessRuntimeState::Ready;
    self.ready_queue.push(pid);
}
```

For `WaitingTagged` where the tag doesn't match, the process stays waiting — the message sits in the mailbox until the process is stepped again or a matching message arrives.

**Step 6: Add integration test**

Add a test in `scheduler.rs` tests (or `process_table.rs` tests) that verifies the end-to-end flow:

```rust
#[test]
fn test_selective_receive_waits_for_tagged_message() {
    // Create a behavior that returns WaitForTag("reply")
    // after receiving a "start" message
    struct SelectiveBehavior {
        started: bool,
        got_reply: bool,
    }
    impl StepBehavior for SelectiveBehavior {
        fn handle(
            &mut self,
            envelope: Envelope,
            ctx: &mut TurnContext,
        ) -> StepResult {
            match &envelope.payload {
                EnvelopePayload::User(Payload::Text(s))
                    if s == "start" =>
                {
                    self.started = true;
                    StepResult::WaitForTag("reply".into())
                }
                EnvelopePayload::User(Payload::Text(s))
                    if s == "the reply" =>
                {
                    self.got_reply = true;
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

    let mut p = ProcessEntry::new(
        Pid::from_raw(1),
        Box::new(SelectiveBehavior {
            started: false,
            got_reply: false,
        }),
    );

    // Send start message
    p.mailbox.push(Envelope::text(Pid::from_raw(1), "start"));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(ref t) if t == "reply"));

    // Send untagged message — should be skipped by selective receive
    p.mailbox.push(Envelope::text(Pid::from_raw(1), "noise"));
    // Send tagged message with wrong tag — also skipped
    p.mailbox.push(
        Envelope::text(Pid::from_raw(1), "wrong")
            .with_tag("other"),
    );
    // Send correctly tagged message
    p.mailbox.push(
        Envelope::text(Pid::from_raw(1), "the reply")
            .with_tag("reply"),
    );

    // Set state as scheduler would
    p.state = ProcessRuntimeState::WaitingTagged("reply".into());

    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Done(Reason::Normal)));

    // noise and "wrong" messages should still be in mailbox
    assert_eq!(p.mailbox.total_len(), 2);
}
```

**Step 7: Run full test suite**

Run: `cargo test -p zeptovm --lib`
Expected: All tests pass

**Step 8: Commit**

```bash
git add zeptovm/src/core/step_result.rs zeptovm/src/kernel/process_table.rs zeptovm/src/kernel/scheduler.rs
git commit -m "feat(selective-receive): wire WaitForTag through process state and scheduler"
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
