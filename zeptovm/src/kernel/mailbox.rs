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
        }
    }

    /// Push an envelope into the correct lane based on its MessageClass.
    pub fn push(&mut self, env: Envelope) {
        match env.class {
            MessageClass::Control => self.control.push_back(env),
            MessageClass::Supervisor => self.supervisor.push_back(env),
            MessageClass::EffectResult => self.effect.push_back(env),
            MessageClass::User => self.user.push_back(env),
            MessageClass::Background => self.background.push_back(env),
        }
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
}
