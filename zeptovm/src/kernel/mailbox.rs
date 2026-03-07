use std::collections::HashSet;
use std::collections::VecDeque;

use crate::core::message::{Envelope, MessageClass};

/// Multi-lane mailbox with priority-ordered drain.
///
/// Lane order: control > supervisor > effect > user > background
/// Fairness window ensures low-priority lanes still progress.
pub struct MultiLaneMailbox {
    control: VecDeque<Envelope>,
    supervisor: VecDeque<Envelope>,
    effect: VecDeque<Envelope>,
    user: VecDeque<Envelope>,
    background: VecDeque<Envelope>,
    /// After this many high-priority drains, force one low-priority drain.
    fairness_window: u32,
    /// Counter for fairness tracking.
    high_priority_streak: u32,
    /// Tracks seen dedup keys to reject duplicate envelopes.
    seen_dedup_keys: HashSet<String>,
    /// Maximum number of dedup keys to track before eviction.
    dedup_capacity: usize,
    /// Accumulated count of expired messages discarded since last drain.
    expired_count: usize,
}

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

impl MultiLaneMailbox {
    pub fn new() -> Self {
        Self {
            control: VecDeque::new(),
            supervisor: VecDeque::new(),
            effect: VecDeque::new(),
            user: VecDeque::new(),
            background: VecDeque::new(),
            fairness_window: 50,
            high_priority_streak: 0,
            seen_dedup_keys: HashSet::new(),
            dedup_capacity: 1000,
            expired_count: 0,
        }
    }

    /// Push an envelope into the correct lane based on its MessageClass.
    ///
    /// Returns `false` if the envelope was rejected as a duplicate
    /// (matching `dedup_key` already seen). Returns `true` otherwise.
    pub fn push(&mut self, env: Envelope) -> bool {
        // Check dedup_key
        if let Some(ref key) = env.dedup_key {
            if self.seen_dedup_keys.contains(key) {
                return false; // Duplicate, skip
            }
            // Evict oldest if at capacity
            if self.seen_dedup_keys.len() >= self.dedup_capacity {
                // Simple eviction: clear the set when full
                self.seen_dedup_keys.clear();
            }
            self.seen_dedup_keys.insert(key.clone());
        }

        match env.class {
            MessageClass::Control => self.control.push_back(env),
            MessageClass::Supervisor => self.supervisor.push_back(env),
            MessageClass::EffectResult => self.effect.push_back(env),
            MessageClass::User => self.user.push_back(env),
            MessageClass::Background => self.background.push_back(env),
        }
        true
    }

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
        let (result, exp) =
            pop_lane_alive(&mut self.effect, now_ms);
        self.expired_count += exp;
        if let Some(env) = result {
            self.high_priority_streak += 1;
            return Some(env);
        }

        // User messages
        let (result, exp) =
            pop_lane_alive(&mut self.user, now_ms);
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

    /// Drain the expired-message counter. Returns the count since
    /// last drain. The runtime calls this to feed metrics.
    pub fn take_expired_count(&mut self) -> usize {
        let count = self.expired_count;
        self.expired_count = 0;
        count
    }

    /// Remove all expired messages from all lanes.
    /// Returns the number of messages removed. Also feeds
    /// expired_count.
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

    /// Check if any lane has messages.
    pub fn has_messages(&self) -> bool {
        !self.control.is_empty()
            || !self.supervisor.is_empty()
            || !self.effect.is_empty()
            || !self.user.is_empty()
            || !self.background.is_empty()
    }

    /// Total messages across all lanes.
    pub fn total_len(&self) -> usize {
        self.control.len()
            + self.supervisor.len()
            + self.effect.len()
            + self.user.len()
            + self.background.len()
    }

    /// Messages in a specific lane.
    pub fn lane_len(&self, class: MessageClass) -> usize {
        match class {
            MessageClass::Control => self.control.len(),
            MessageClass::Supervisor => self.supervisor.len(),
            MessageClass::EffectResult => self.effect.len(),
            MessageClass::User => self.user.len(),
            MessageClass::Background => self.background.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectId, EffectResult};
    use crate::core::message::{Envelope, EnvelopePayload, MessageClass, Signal};
    use crate::pid::Pid;

    fn to() -> Pid {
        Pid::from_raw(1)
    }

    #[test]
    fn test_mailbox_empty() {
        let mut mb = MultiLaneMailbox::new();
        assert!(!mb.has_messages());
        assert!(mb.pop(0).is_none());
        assert_eq!(mb.total_len(), 0);
    }

    #[test]
    fn test_mailbox_push_pop_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "hello"));
        assert!(mb.has_messages());
        assert_eq!(mb.total_len(), 1);

        let env = mb.pop(0).unwrap();
        assert_eq!(env.class, MessageClass::User);
        assert!(!mb.has_messages());
    }

    #[test]
    fn test_mailbox_control_before_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "user msg"));
        mb.push(Envelope::signal(to(), Signal::Kill));

        let first = mb.pop(0).unwrap();
        assert_eq!(first.class, MessageClass::Control);

        let second = mb.pop(0).unwrap();
        assert_eq!(second.class, MessageClass::User);
    }

    #[test]
    fn test_mailbox_effect_before_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "user msg"));
        let result = EffectResult::success(
            EffectId::new(),
            serde_json::json!("ok"),
        );
        mb.push(Envelope::effect_result(to(), result));

        let first = mb.pop(0).unwrap();
        assert_eq!(first.class, MessageClass::EffectResult);

        let second = mb.pop(0).unwrap();
        assert_eq!(second.class, MessageClass::User);
    }

    #[test]
    fn test_mailbox_fifo_within_lane() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "first"));
        mb.push(Envelope::text(to(), "second"));
        mb.push(Envelope::text(to(), "third"));

        for expected in ["first", "second", "third"] {
            let env = mb.pop(0).unwrap();
            if let EnvelopePayload::User(crate::core::message::Payload::Text(s)) =
                &env.payload
            {
                assert_eq!(s, expected);
            } else {
                panic!("expected text payload");
            }
        }
    }

    #[test]
    fn test_mailbox_lane_len() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "a"));
        mb.push(Envelope::text(to(), "b"));
        mb.push(Envelope::signal(to(), Signal::Suspend));

        assert_eq!(mb.lane_len(MessageClass::User), 2);
        assert_eq!(mb.lane_len(MessageClass::Control), 1);
        assert_eq!(mb.lane_len(MessageClass::EffectResult), 0);
    }

    #[test]
    fn test_mailbox_fairness_window() {
        let mut mb = MultiLaneMailbox::new();
        mb.fairness_window = 3;

        // Add background message
        let mut bg = Envelope::text(to(), "background");
        bg.class = MessageClass::Background;
        mb.push(bg);

        // Add 4 effect results
        for _ in 0..4 {
            let result = EffectResult::success(
                EffectId::new(),
                serde_json::json!("ok"),
            );
            mb.push(Envelope::effect_result(to(), result));
        }

        // First 3 pops should be effect results (high-priority streak)
        for _ in 0..3 {
            let env = mb.pop(0).unwrap();
            assert_eq!(env.class, MessageClass::EffectResult);
        }

        // 4th pop: fairness kicks in, background gets a turn
        let env = mb.pop(0).unwrap();
        assert_eq!(env.class, MessageClass::Background);

        // 5th pop: back to effect
        let env = mb.pop(0).unwrap();
        assert_eq!(env.class, MessageClass::EffectResult);
    }

    #[test]
    fn test_mailbox_dedup_rejects_duplicate() {
        let mut mb = MultiLaneMailbox::new();
        let mut env1 = Envelope::text(to(), "first");
        env1.dedup_key = Some("key-1".into());
        let mut env2 = Envelope::text(to(), "second");
        env2.dedup_key = Some("key-1".into());

        assert!(mb.push(env1));
        assert!(!mb.push(env2)); // Duplicate rejected
        assert_eq!(mb.total_len(), 1);
    }

    #[test]
    fn test_mailbox_dedup_allows_different_keys() {
        let mut mb = MultiLaneMailbox::new();
        let mut env1 = Envelope::text(to(), "first");
        env1.dedup_key = Some("key-1".into());
        let mut env2 = Envelope::text(to(), "second");
        env2.dedup_key = Some("key-2".into());

        assert!(mb.push(env1));
        assert!(mb.push(env2));
        assert_eq!(mb.total_len(), 2);
    }

    #[test]
    fn test_mailbox_dedup_no_key_always_accepted() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "a"));
        mb.push(Envelope::text(to(), "b"));
        assert_eq!(mb.total_len(), 2);
    }

    #[test]
    fn test_mailbox_dedup_capacity_eviction() {
        let mut mb = MultiLaneMailbox::new();
        mb.dedup_capacity = 3;

        for i in 0..3 {
            let mut env = Envelope::text(to(), format!("msg-{i}"));
            env.dedup_key = Some(format!("key-{i}"));
            assert!(mb.push(env));
        }

        // 4th message triggers capacity eviction (clear)
        let mut env4 = Envelope::text(to(), "msg-3");
        env4.dedup_key = Some("key-3".into());
        assert!(mb.push(env4));

        // Now key-0 should be accepted again (evicted)
        let mut env_dup = Envelope::text(to(), "msg-0-dup");
        env_dup.dedup_key = Some("key-0".into());
        assert!(mb.push(env_dup));
    }

    #[test]
    fn test_pop_no_ttl_never_expires() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "hello"));
        // Even at far-future time, no-TTL messages are fine
        let env = mb.pop(u64::MAX).unwrap();
        assert!(matches!(
            env.payload,
            EnvelopePayload::User(
                crate::core::message::Payload::Text(_)
            )
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
        if let EnvelopePayload::User(
            crate::core::message::Payload::Text(s),
        ) = &env.payload
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
}
