use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::pid::Pid;

/// A scheduler-local run queue backed by a crossbeam work-stealing deque.
/// Each scheduler thread owns one RunQueue.
pub struct RunQueue {
    /// Local end — only the owning thread pushes/pops.
    local: Worker<Pid>,
}

impl RunQueue {
    pub fn new() -> Self {
        Self {
            local: Worker::new_fifo(),
        }
    }

    /// Push a ready process pid onto the local queue.
    pub fn push(&self, pid: Pid) {
        self.local.push(pid);
    }

    /// Pop the next ready pid from the local queue.
    pub fn pop(&self) -> Option<Pid> {
        self.local.pop()
    }

    /// Get a stealer handle (for other threads to steal from this queue).
    pub fn stealer(&self) -> Stealer<Pid> {
        self.local.stealer()
    }

    /// Check if the local queue is empty.
    pub fn is_empty(&self) -> bool {
        self.local.is_empty()
    }

    /// Try to steal a pid from another scheduler's queue.
    pub fn steal_from(stealer: &Stealer<Pid>) -> Option<Pid> {
        loop {
            match stealer.steal() {
                Steal::Success(pid) => return Some(pid),
                Steal::Empty => return None,
                Steal::Retry => continue,
            }
        }
    }
}

/// Global injector queue for distributing new processes to schedulers.
pub struct GlobalQueue {
    injector: Injector<Pid>,
}

impl GlobalQueue {
    pub fn new() -> Self {
        Self {
            injector: Injector::new(),
        }
    }

    /// Push a pid into the global queue (any thread can call this).
    pub fn push(&self, pid: Pid) {
        self.injector.push(pid);
    }

    /// Try to steal a batch from the global queue into a local worker.
    pub fn steal_into(&self, local: &RunQueue) -> Option<Pid> {
        loop {
            match self.injector.steal_batch_and_pop(&local.local) {
                Steal::Success(pid) => return Some(pid),
                Steal::Empty => return None,
                Steal::Retry => continue,
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.injector.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;

    #[test]
    fn test_run_queue_push_pop() {
        let rq = RunQueue::new();
        let pid = Pid::from_raw(1);
        rq.push(pid);
        assert_eq!(rq.pop(), Some(pid));
        assert_eq!(rq.pop(), None);
    }

    #[test]
    fn test_run_queue_fifo_order() {
        let rq = RunQueue::new();
        let p1 = Pid::from_raw(1);
        let p2 = Pid::from_raw(2);
        let p3 = Pid::from_raw(3);
        rq.push(p1);
        rq.push(p2);
        rq.push(p3);
        assert_eq!(rq.pop(), Some(p1));
        assert_eq!(rq.pop(), Some(p2));
        assert_eq!(rq.pop(), Some(p3));
    }

    #[test]
    fn test_run_queue_is_empty() {
        let rq = RunQueue::new();
        assert!(rq.is_empty());
        rq.push(Pid::from_raw(1));
        assert!(!rq.is_empty());
    }

    #[test]
    fn test_run_queue_steal() {
        let rq = RunQueue::new();
        let stealer = rq.stealer();

        rq.push(Pid::from_raw(10));
        rq.push(Pid::from_raw(20));

        let stolen = RunQueue::steal_from(&stealer);
        assert!(stolen.is_some());
    }

    #[test]
    fn test_global_queue_push_steal() {
        let gq = GlobalQueue::new();
        let local = RunQueue::new();

        gq.push(Pid::from_raw(100));
        gq.push(Pid::from_raw(200));

        let stolen = gq.steal_into(&local);
        assert!(stolen.is_some());
    }

    #[test]
    fn test_global_queue_empty() {
        let gq = GlobalQueue::new();
        let local = RunQueue::new();
        assert!(gq.is_empty());
        assert!(gq.steal_into(&local).is_none());
    }
}
