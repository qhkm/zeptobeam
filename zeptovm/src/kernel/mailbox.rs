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
    pub fn pop(&mut self) -> Option<Envelope> {
        // Control always first (kill signals, exit links)
        if let Some(env) = self.control.pop_front() {
            return Some(env);
        }

        // Fairness check: if high-priority lanes have been draining too long,
        // give low-priority a chance
        if self.high_priority_streak >= self.fairness_window {
            self.high_priority_streak = 0;
            if let Some(env) = self.background.pop_front() {
                return Some(env);
            }
        }

        // Supervisor
        if let Some(env) = self.supervisor.pop_front() {
            self.high_priority_streak += 1;
            return Some(env);
        }

        // Effect results (high priority — unblock waiting processes)
        if let Some(env) = self.effect.pop_front() {
            self.high_priority_streak += 1;
            return Some(env);
        }

        // User messages
        if let Some(env) = self.user.pop_front() {
            self.high_priority_streak = 0;
            return Some(env);
        }

        // Background
        if let Some(env) = self.background.pop_front() {
            self.high_priority_streak = 0;
            return Some(env);
        }

        None
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
        assert!(mb.pop().is_none());
        assert_eq!(mb.total_len(), 0);
    }

    #[test]
    fn test_mailbox_push_pop_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "hello"));
        assert!(mb.has_messages());
        assert_eq!(mb.total_len(), 1);

        let env = mb.pop().unwrap();
        assert_eq!(env.class, MessageClass::User);
        assert!(!mb.has_messages());
    }

    #[test]
    fn test_mailbox_control_before_user() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "user msg"));
        mb.push(Envelope::signal(to(), Signal::Kill));

        let first = mb.pop().unwrap();
        assert_eq!(first.class, MessageClass::Control);

        let second = mb.pop().unwrap();
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

        let first = mb.pop().unwrap();
        assert_eq!(first.class, MessageClass::EffectResult);

        let second = mb.pop().unwrap();
        assert_eq!(second.class, MessageClass::User);
    }

    #[test]
    fn test_mailbox_fifo_within_lane() {
        let mut mb = MultiLaneMailbox::new();
        mb.push(Envelope::text(to(), "first"));
        mb.push(Envelope::text(to(), "second"));
        mb.push(Envelope::text(to(), "third"));

        for expected in ["first", "second", "third"] {
            let env = mb.pop().unwrap();
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
            let env = mb.pop().unwrap();
            assert_eq!(env.class, MessageClass::EffectResult);
        }

        // 4th pop: fairness kicks in, background gets a turn
        let env = mb.pop().unwrap();
        assert_eq!(env.class, MessageClass::Background);

        // 5th pop: back to effect
        let env = mb.pop().unwrap();
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
}
