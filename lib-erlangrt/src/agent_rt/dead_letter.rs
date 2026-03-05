use std::collections::VecDeque;

use crate::agent_rt::types::AgentPid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadLetterReason {
  MailboxFull,
  ProcessNotFound,
  ProcessTerminated,
}

#[derive(Debug, Clone)]
pub struct DeadLetter {
  pub timestamp: std::time::Instant,
  pub sender_pid: Option<AgentPid>,
  pub target_pid: AgentPid,
  pub reason: DeadLetterReason,
}

/// Bounded ring buffer of dropped deliveries.
pub struct DeadLetterQueue {
  buffer: VecDeque<DeadLetter>,
  capacity: usize,
  total_count: u64,
}

impl DeadLetterQueue {
  pub fn new(capacity: usize) -> Self {
    Self {
      buffer: VecDeque::with_capacity(capacity),
      capacity,
      total_count: 0,
    }
  }

  pub fn push(&mut self, letter: DeadLetter) {
    if self.buffer.len() >= self.capacity {
      self.buffer.pop_front();
    }
    self.buffer.push_back(letter);
    self.total_count += 1;
  }

  pub fn recent(&self) -> &VecDeque<DeadLetter> {
    &self.buffer
  }

  pub fn total_count(&self) -> u64 {
    self.total_count
  }

  pub fn len(&self) -> usize {
    self.buffer.len()
  }

  pub fn is_empty(&self) -> bool {
    self.buffer.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_dead_letter_queue_basic() {
    let mut dlq = DeadLetterQueue::new(3);
    assert!(dlq.is_empty());
    assert_eq!(dlq.total_count(), 0);

    for i in 0..5 {
      dlq.push(DeadLetter {
        timestamp: std::time::Instant::now(),
        sender_pid: None,
        target_pid: AgentPid::from_raw(i),
        reason: DeadLetterReason::ProcessNotFound,
      });
    }

    assert_eq!(dlq.len(), 3);
    assert_eq!(dlq.total_count(), 5);
    assert_eq!(dlq.recent()[0].target_pid.raw(), 2);
  }
}
