use crate::core::behavior::StepBehavior;

#[derive(Debug, Clone)]
pub enum BackoffPolicy {
    Immediate,
    Fixed(u64),
    Exponential { base_ms: u64, max_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartStrategy {
    Permanent,
    Transient,
    Temporary,
}

#[derive(Debug, Clone)]
pub struct SupervisorSpec {
    pub max_restarts: u32,
    pub restart_window_ms: u64,
    pub backoff: BackoffPolicy,
}

impl Default for SupervisorSpec {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            restart_window_ms: 5000,
            backoff: BackoffPolicy::Immediate,
        }
    }
}

pub struct ChildSpec {
    pub id: String,
    pub behavior_factory: Box<dyn Fn() -> Box<dyn StepBehavior> + Send>,
    pub restart: RestartStrategy,
    pub shutdown_timeout_ms: u64,
}

impl ChildSpec {
    pub fn new(
        id: impl Into<String>,
        factory: impl Fn() -> Box<dyn StepBehavior> + Send + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            behavior_factory: Box::new(factory),
            restart: RestartStrategy::Permanent,
            shutdown_timeout_ms: 5000,
        }
    }

    pub fn with_restart(mut self, strategy: RestartStrategy) -> Self {
        self.restart = strategy;
        self
    }

    pub fn with_shutdown_timeout(mut self, ms: u64) -> Self {
        self.shutdown_timeout_ms = ms;
        self
    }
}

impl BackoffPolicy {
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        match self {
            BackoffPolicy::Immediate => 0,
            BackoffPolicy::Fixed(ms) => *ms,
            BackoffPolicy::Exponential { base_ms, max_ms } => {
                let delay = base_ms.saturating_mul(1u64 << attempt.min(30));
                delay.min(*max_ms)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supervisor_spec_default() {
        let spec = SupervisorSpec::default();
        assert_eq!(spec.max_restarts, 3);
        assert_eq!(spec.restart_window_ms, 5000);
    }

    #[test]
    fn test_backoff_immediate() {
        let b = BackoffPolicy::Immediate;
        assert_eq!(b.delay_ms(0), 0);
        assert_eq!(b.delay_ms(5), 0);
    }

    #[test]
    fn test_backoff_fixed() {
        let b = BackoffPolicy::Fixed(1000);
        assert_eq!(b.delay_ms(0), 1000);
        assert_eq!(b.delay_ms(5), 1000);
    }

    #[test]
    fn test_backoff_exponential() {
        let b = BackoffPolicy::Exponential {
            base_ms: 100,
            max_ms: 5000,
        };
        assert_eq!(b.delay_ms(0), 100);
        assert_eq!(b.delay_ms(1), 200);
        assert_eq!(b.delay_ms(2), 400);
        assert_eq!(b.delay_ms(10), 5000);
    }

    #[test]
    fn test_restart_strategy() {
        assert_eq!(RestartStrategy::Permanent, RestartStrategy::Permanent);
        assert_ne!(RestartStrategy::Permanent, RestartStrategy::Transient);
    }

    #[test]
    fn test_child_spec_builder() {
        use crate::core::behavior::StepBehavior;
        use crate::core::message::Envelope;
        use crate::core::step_result::StepResult;
        use crate::core::turn_context::TurnContext;
        use crate::error::Reason;

        struct Dummy;
        impl StepBehavior for Dummy {
            fn init(&mut self, _: Option<Vec<u8>>) -> StepResult {
                StepResult::Continue
            }
            fn handle(&mut self, _: Envelope, _: &mut TurnContext) -> StepResult {
                StepResult::Continue
            }
            fn terminate(&mut self, _: &Reason) {}
        }

        let spec = ChildSpec::new("worker-1", || Box::new(Dummy))
            .with_restart(RestartStrategy::Transient)
            .with_shutdown_timeout(3000);
        assert_eq!(spec.id, "worker-1");
        assert_eq!(spec.restart, RestartStrategy::Transient);
        assert_eq!(spec.shutdown_timeout_ms, 3000);
        let _b = (spec.behavior_factory)();
    }
}
