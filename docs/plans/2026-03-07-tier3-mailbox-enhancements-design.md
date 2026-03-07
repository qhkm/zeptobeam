# Tier 3 — Mailbox Enhancements Design (X3 + X2)

## Goal

Add per-message TTL (X3) and tag-based selective receive (X2) to the MultiLaneMailbox, completing the mailbox feature set for agent workflows.

## Architecture

Two complementary additions to the existing Envelope + MultiLaneMailbox. TTL adds an optional expiry timestamp checked lazily on dequeue. Selective receive adds an optional tag field with a `pop_matching` method that scans lanes in priority order.

## Tech Stack

Rust, existing ZeptoVM kernel (core/message.rs, kernel/mailbox.rs)

---

## X3: Per-Message TTL

### Envelope Changes

```rust
pub struct Envelope {
    // ... existing fields ...
    pub expires_at: Option<u64>,  // epoch millis, None = never expires
}
```

Builder method:
```rust
pub fn with_ttl_ms(mut self, millis: u64) -> Self {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    self.expires_at = Some(now + millis);
    self
}
```

### Mailbox Changes

`pop()` gains a `now_ms` parameter. Before returning a message, checks `expires_at`:
- If `Some(t)` and `now_ms >= t`, discard and continue scanning
- If `None` or `now_ms < t`, return normally

```rust
pub fn pop(&mut self, now_ms: u64) -> Option<Envelope> { ... }
```

Proactive sweep for memory pressure:
```rust
pub fn reap_expired(&mut self, now_ms: u64) -> usize { ... }
```

Removes all expired messages from all lanes, returns count removed.

---

## X2: Tag-Based Selective Receive

### Envelope Changes

```rust
pub struct Envelope {
    // ... existing fields ...
    pub tag: Option<String>,  // e.g. "effect_result", "user_input", "timer"
}
```

Builder method:
```rust
pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
    self.tag = Some(tag.into());
    self
}
```

### Mailbox Changes

New method scans lanes in priority order, returns first non-expired message matching tag:

```rust
pub fn pop_matching(&mut self, now_ms: u64, tag: &str) -> Option<Envelope> { ... }
```

- Scans each lane (control, supervisor, effect, user, background) in priority order
- Within each lane, iterates messages, skips expired, returns first with matching tag
- Uses `VecDeque::remove(index)` for positional removal — O(n) per lane, fine for typical sizes
- Non-matching messages stay in place

Existing `pop(now_ms)` unchanged — still drains in priority order regardless of tag.

### Tag vs Lane

Tags are orthogonal to MessageClass lanes. A message can be in the User lane with tag `"goal_update"`, or in the EffectResult lane with tag `"llm_response"`. Selective receive scans across lanes respecting priority order.

---

## Error Handling

- Expired messages silently discarded on dequeue (no error, no event)
- `pop_matching` with no match returns `None` (caller retries next tick)
- `with_ttl_ms(0)` is valid — message expires immediately ("try once" semantics)
- `reap_expired` is optional — lazy expiry in `pop` handles correctness, sweep is for memory pressure

---

## Testing

### X3 — TTL Tests
- Message without TTL never expires
- Message with TTL dequeued before expiry — delivered normally
- Message with TTL dequeued after expiry — skipped, next valid message returned
- `reap_expired` removes expired messages, returns count
- Expired messages don't count in `total_len` after reap

### X2 — Selective Receive Tests
- `pop_matching` returns first message with matching tag
- `pop_matching` skips non-matching messages (leaves them in mailbox)
- `pop_matching` respects lane priority (control tag match before user tag match)
- `pop_matching` skips expired messages even if tag matches
- No matching tag returns `None`, mailbox unchanged
- `pop` still works normally (ignores tags)
