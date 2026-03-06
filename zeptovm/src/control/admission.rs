use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Priority level for agent scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Priority {
    High,
    Normal,
    Low,
}

impl Priority {
    /// Scheduling weight for weighted fair queuing.
    pub fn weight(&self) -> u32 {
        match self {
            Priority::High => 4,
            Priority::Normal => 2,
            Priority::Low => 1,
        }
    }
}

/// A request waiting for admission.
#[derive(Debug)]
pub struct AdmissionRequest<T> {
    pub data: T,
    pub priority: Priority,
    pub enqueued_at: Instant,
}

/// Weighted fair queuing admission controller with starvation prevention.
///
/// Dequeues from High:Normal:Low in 4:2:1 ratio.
/// Low-priority requests are promoted to Normal after `promotion_threshold`
/// (default 60s).
pub struct AdmissionController<T> {
    high: VecDeque<AdmissionRequest<T>>,
    normal: VecDeque<AdmissionRequest<T>>,
    low: VecDeque<AdmissionRequest<T>>,
    /// Counter for weighted round-robin
    round_counter: u32,
    /// Duration after which Low priority is promoted to Normal
    promotion_threshold: Duration,
}

impl<T> AdmissionController<T> {
    pub fn new() -> Self {
        Self {
            high: VecDeque::new(),
            normal: VecDeque::new(),
            low: VecDeque::new(),
            round_counter: 0,
            promotion_threshold: Duration::from_secs(60),
        }
    }

    pub fn with_promotion_threshold(mut self, threshold: Duration) -> Self {
        self.promotion_threshold = threshold;
        self
    }

    /// Enqueue a request with the given priority.
    pub fn enqueue(&mut self, data: T, priority: Priority) {
        let request = AdmissionRequest {
            data,
            priority,
            enqueued_at: Instant::now(),
        };
        match priority {
            Priority::High => self.high.push_back(request),
            Priority::Normal => self.normal.push_back(request),
            Priority::Low => self.low.push_back(request),
        }
    }

    /// Dequeue the next request using weighted fair queuing.
    ///
    /// The algorithm: in a cycle of 7 slots (4+2+1):
    /// - Slots 0-3: try High, fallback Normal, fallback Low
    /// - Slots 4-5: try Normal, fallback High, fallback Low
    /// - Slot 6: try Low, fallback Normal, fallback High
    ///
    /// Before dequeuing, promotes Low->Normal if waiting > promotion_threshold.
    pub fn dequeue(&mut self) -> Option<AdmissionRequest<T>> {
        self.promote_starved();

        let cycle = self.round_counter % 7;
        self.round_counter = self.round_counter.wrapping_add(1);

        match cycle {
            0..=3 => self.try_dequeue_order(&[
                Priority::High,
                Priority::Normal,
                Priority::Low,
            ]),
            4..=5 => self.try_dequeue_order(&[
                Priority::Normal,
                Priority::High,
                Priority::Low,
            ]),
            6 => self.try_dequeue_order(&[
                Priority::Low,
                Priority::Normal,
                Priority::High,
            ]),
            _ => unreachable!(),
        }
    }

    fn try_dequeue_order(
        &mut self,
        order: &[Priority],
    ) -> Option<AdmissionRequest<T>> {
        for &p in order {
            let queue = match p {
                Priority::High => &mut self.high,
                Priority::Normal => &mut self.normal,
                Priority::Low => &mut self.low,
            };
            if let Some(req) = queue.pop_front() {
                return Some(req);
            }
        }
        None
    }

    /// Promote Low-priority requests to Normal if they've waited too long.
    fn promote_starved(&mut self) {
        let threshold = self.promotion_threshold;
        let now = Instant::now();
        let mut i = 0;
        while i < self.low.len() {
            if now.duration_since(self.low[i].enqueued_at) >= threshold {
                let mut req = self.low.remove(i).unwrap();
                req.priority = Priority::Normal;
                self.normal.push_back(req);
            } else {
                i += 1;
            }
        }
    }

    /// Number of pending requests.
    pub fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.low.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_weighted_fair_queuing() {
        let mut ac = AdmissionController::new();

        // Enqueue many of each priority
        for i in 0..10 {
            ac.enqueue(format!("high-{i}"), Priority::High);
            ac.enqueue(format!("normal-{i}"), Priority::Normal);
            ac.enqueue(format!("low-{i}"), Priority::Low);
        }

        // Dequeue 7 items (one full cycle: 4 high, 2 normal, 1 low)
        let mut priorities = Vec::new();
        for _ in 0..7 {
            let req = ac.dequeue().unwrap();
            priorities.push(req.priority);
        }

        let high_count =
            priorities.iter().filter(|&&p| p == Priority::High).count();
        let normal_count =
            priorities.iter().filter(|&&p| p == Priority::Normal).count();
        let low_count =
            priorities.iter().filter(|&&p| p == Priority::Low).count();

        assert_eq!(high_count, 4);
        assert_eq!(normal_count, 2);
        assert_eq!(low_count, 1);
    }

    #[test]
    fn test_fallback_when_queue_empty() {
        let mut ac = AdmissionController::new();
        // Only normal priority
        ac.enqueue("normal-1", Priority::Normal);
        ac.enqueue("normal-2", Priority::Normal);

        // Even though cycle 0-3 prefers High, it should fallback to Normal
        let req = ac.dequeue().unwrap();
        assert_eq!(req.priority, Priority::Normal);
    }

    #[test]
    fn test_empty_dequeue() {
        let mut ac: AdmissionController<String> = AdmissionController::new();
        assert!(ac.dequeue().is_none());
        assert!(ac.is_empty());
    }

    #[test]
    fn test_starvation_prevention() {
        let mut ac = AdmissionController::new()
            .with_promotion_threshold(Duration::from_millis(0));

        ac.enqueue("low-1", Priority::Low);

        // After promotion, should be dequeued as Normal
        let req = ac.dequeue().unwrap();
        assert_eq!(req.priority, Priority::Normal);
        assert_eq!(req.data, "low-1");
    }

    #[test]
    fn test_priority_weights() {
        assert_eq!(Priority::High.weight(), 4);
        assert_eq!(Priority::Normal.weight(), 2);
        assert_eq!(Priority::Low.weight(), 1);
    }

    #[test]
    fn test_len_and_is_empty() {
        let mut ac = AdmissionController::new();
        assert_eq!(ac.len(), 0);
        assert!(ac.is_empty());
        ac.enqueue("x", Priority::Normal);
        assert_eq!(ac.len(), 1);
        assert!(!ac.is_empty());
        ac.dequeue();
        assert_eq!(ac.len(), 0);
    }
}
