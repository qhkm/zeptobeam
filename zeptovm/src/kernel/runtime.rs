use tracing::{debug, warn};

use crate::{
  core::{
    behavior::StepBehavior,
    effect::{EffectKind, EffectRequest, EffectResult},
    message::Envelope,
  },
  durability::{journal::Journal, snapshot::SnapshotStore},
  error::Reason,
  kernel::{
    metrics::Metrics,
    process_table::ProcessRuntimeState,
    reactor::Reactor,
    scheduler::SchedulerEngine,
    turn_executor::TurnExecutor,
  },
  pid::Pid,
};

/// Simple budget state for v1.
#[derive(Debug, Clone)]
pub struct BudgetState {
  pub usd_remaining: f64,
  pub token_remaining: u64,
}

impl Default for BudgetState {
  fn default() -> Self {
    Self {
      usd_remaining: 100.0,
      token_remaining: 1_000_000,
    }
  }
}

/// Notification when budget blocks an effect.
#[derive(Debug)]
pub struct BudgetViolation {
  pub pid: Pid,
  pub effect_kind: EffectKind,
  pub reason: String,
}

/// The top-level runtime combining scheduler, reactor,
/// durability, and budget.
pub struct SchedulerRuntime {
  engine: SchedulerEngine,
  reactor: Option<Reactor>,
  #[allow(dead_code)]
  turn_executor: Option<TurnExecutor>,
  budget: BudgetState,
  budget_violations: Vec<BudgetViolation>,
  budget_enabled: bool,
  metrics: Metrics,
}

impl SchedulerRuntime {
  /// Create a runtime with in-memory durability (for testing).
  pub fn new() -> Self {
    Self {
      engine: SchedulerEngine::new(),
      reactor: None,
      turn_executor: None,
      budget: BudgetState::default(),
      budget_violations: Vec::new(),
      budget_enabled: false,
      metrics: Metrics::new(),
    }
  }

  /// Create a runtime with full durability and reactor.
  pub fn with_durability() -> Self {
    let journal = Journal::open_in_memory().unwrap();
    let snapshot_store =
      SnapshotStore::open_in_memory().unwrap();
    let turn_executor =
      TurnExecutor::new(journal, snapshot_store);
    let reactor = Reactor::start();

    Self {
      engine: SchedulerEngine::new(),
      reactor: Some(reactor),
      turn_executor: Some(turn_executor),
      budget: BudgetState::default(),
      budget_violations: Vec::new(),
      budget_enabled: false,
      metrics: Metrics::new(),
    }
  }

  /// Enable budget checking with custom limits.
  pub fn with_budget(mut self, budget: BudgetState) -> Self {
    self.budget = budget;
    self.budget_enabled = true;
    self
  }

  /// Enable budget checking with default limits.
  pub fn enable_budget(mut self) -> Self {
    self.budget_enabled = true;
    self
  }

  /// Set max reductions per process per tick.
  pub fn with_max_reductions(mut self, max: u32) -> Self {
    self.engine = self.engine.with_max_reductions(max);
    self
  }

  /// Spawn a new process.
  pub fn spawn(
    &mut self,
    behavior: Box<dyn StepBehavior>,
  ) -> Pid {
    let pid = self.engine.spawn(behavior);
    self.metrics.inc("processes.spawned");
    self.metrics.gauge_set(
      "processes.active",
      self.engine.process_count() as i64,
    );
    pid
  }

  /// Send a message to a process.
  pub fn send(&mut self, env: Envelope) {
    self.engine.send(env);
  }

  /// Run one tick of the runtime:
  /// 1. Drain reactor completions -> deliver to scheduler
  /// 2. Tick the scheduler engine
  /// 3. Process outbound effects (budget gate -> reactor
  ///    dispatch)
  /// 4. Process completed processes
  pub fn tick(&mut self) -> usize {
    // 1. Drain reactor completions
    if let Some(ref reactor) = self.reactor {
      let completions = reactor.drain_completions();
      for completion in completions {
        self.engine.deliver_effect_result(completion.result);
      }
    }

    // 2. Tick scheduler
    let stepped = self.engine.tick();
    debug!(stepped, "runtime tick complete");
    self.metrics.inc("scheduler.ticks");

    // 3. Process outbound effects
    let effects = self.engine.take_outbound_effects();
    for (pid, req) in effects {
      if self.budget_enabled && self.should_block_effect(&req)
      {
        // Budget exceeded -- deliver failure result
        warn!(
          pid = %pid,
          kind = ?req.kind,
          "effect blocked by budget"
        );
        self.metrics.inc("budget.blocked");
        let violation = BudgetViolation {
          pid,
          effect_kind: req.kind.clone(),
          reason: "budget exceeded".to_string(),
        };
        self.budget_violations.push(violation);

        let result = EffectResult::failure(
          req.effect_id,
          "budget exceeded",
        );
        self.engine.deliver_effect_result(result);
      } else {
        // Dispatch to reactor
        self.metrics.inc("effects.dispatched");
        if let Some(ref reactor) = self.reactor {
          reactor.dispatch(pid, req);
        }
      }
    }

    stepped
  }

  /// Run multiple ticks until idle or max_ticks reached.
  pub fn run(&mut self, max_ticks: usize) -> usize {
    let mut total_stepped = 0;
    for _ in 0..max_ticks {
      let stepped = self.tick();
      total_stepped += stepped;
      if stepped == 0 {
        break;
      }
    }
    total_stepped
  }

  /// Check if an effect should be blocked by budget.
  fn should_block_effect(
    &self,
    req: &EffectRequest,
  ) -> bool {
    match &req.kind {
      EffectKind::LlmCall => {
        self.budget.usd_remaining <= 0.0
          || self.budget.token_remaining == 0
      }
      _ => false, // Only gate LLM calls for v1
    }
  }

  /// Kill a process.
  pub fn kill(&mut self, pid: Pid) {
    self.engine.kill(pid);
  }

  /// Number of live processes.
  pub fn process_count(&self) -> usize {
    self.engine.process_count()
  }

  /// Get process state.
  pub fn process_state(
    &self,
    pid: Pid,
  ) -> Option<ProcessRuntimeState> {
    self.engine.process_state(pid)
  }

  /// Take completed processes.
  pub fn take_completed(&mut self) -> Vec<(Pid, Reason)> {
    let completed = self.engine.take_completed();
    for _ in &completed {
      self.metrics.inc("processes.exited");
    }
    if !completed.is_empty() {
      self.metrics.gauge_set(
        "processes.active",
        self.engine.process_count() as i64,
      );
    }
    completed
  }

  /// Take budget violations.
  pub fn take_budget_violations(
    &mut self,
  ) -> Vec<BudgetViolation> {
    std::mem::take(&mut self.budget_violations)
  }

  /// Reap completed processes.
  pub fn reap_completed(&mut self) -> Vec<Pid> {
    let reaped = self.engine.reap_completed();
    self.metrics.gauge_set(
      "processes.active",
      self.engine.process_count() as i64,
    );
    reaped
  }

  /// Get a reference to the budget state.
  pub fn budget(&self) -> &BudgetState {
    &self.budget
  }

  /// Mutably access the budget.
  pub fn budget_mut(&mut self) -> &mut BudgetState {
    &mut self.budget
  }

  /// Check if the runtime has a reactor.
  pub fn has_reactor(&self) -> bool {
    self.reactor.is_some()
  }

  /// Deliver an effect result directly (for testing without
  /// reactor).
  pub fn deliver_effect_result(
    &mut self,
    result: EffectResult,
  ) {
    self.engine.deliver_effect_result(result);
  }

  /// Get a reference to the metrics.
  pub fn metrics(&self) -> &Metrics {
    &self.metrics
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::behavior::StepBehavior;
  use crate::core::effect::{EffectKind, EffectRequest};
  use crate::core::message::{Envelope, EnvelopePayload};
  use crate::core::step_result::StepResult;
  use crate::core::turn_context::TurnContext;
  use crate::error::Reason;

  struct Echo;
  impl StepBehavior for Echo {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _msg: Envelope,
      _ctx: &mut TurnContext,
    ) -> StepResult {
      StepResult::Continue
    }
    fn terminate(&mut self, _reason: &Reason) {}
  }

  struct Stopper;
  impl StepBehavior for Stopper {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _msg: Envelope,
      _ctx: &mut TurnContext,
    ) -> StepResult {
      StepResult::Done(Reason::Normal)
    }
    fn terminate(&mut self, _reason: &Reason) {}
  }

  struct LlmCaller;
  impl StepBehavior for LlmCaller {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      msg: Envelope,
      _ctx: &mut TurnContext,
    ) -> StepResult {
      match &msg.payload {
        EnvelopePayload::Effect(_) => {
          StepResult::Done(Reason::Normal)
        }
        _ => StepResult::Suspend(EffectRequest::new(
          EffectKind::LlmCall,
          serde_json::json!({"prompt": "hello"}),
        )),
      }
    }
    fn terminate(&mut self, _reason: &Reason) {}
  }

  struct Forwarder {
    target: Pid,
  }
  impl StepBehavior for Forwarder {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _msg: Envelope,
      ctx: &mut TurnContext,
    ) -> StepResult {
      ctx.send_text(self.target, "forwarded");
      StepResult::Continue
    }
    fn terminate(&mut self, _reason: &Reason) {}
  }

  #[test]
  fn test_runtime_spawn_and_send() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(Echo));
    rt.send(Envelope::text(pid, "hello"));
    let stepped = rt.tick();
    assert!(stepped > 0);
  }

  #[test]
  fn test_runtime_process_done() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(Stopper));
    rt.send(Envelope::text(pid, "stop"));
    rt.tick();
    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(completed[0].1, Reason::Normal));
  }

  #[test]
  fn test_runtime_budget_blocks_llm() {
    let budget = BudgetState {
      usd_remaining: 0.0,
      token_remaining: 0,
    };
    let mut rt =
      SchedulerRuntime::new().with_budget(budget);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Process suspends with LlmCall; budget blocks
    // Budget should block -- effect result failure delivered
    rt.tick(); // Process handles failure effect result

    let violations = rt.take_budget_violations();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].pid, pid);
    assert!(matches!(
      violations[0].effect_kind,
      EffectKind::LlmCall
    ));
  }

  #[test]
  fn test_runtime_budget_allows_when_sufficient() {
    let budget = BudgetState {
      usd_remaining: 100.0,
      token_remaining: 1_000_000,
    };
    // Use runtime without reactor -- effects just go nowhere
    let mut rt =
      SchedulerRuntime::new().with_budget(budget);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    let violations = rt.take_budget_violations();
    assert_eq!(violations.len(), 0);
  }

  #[test]
  fn test_runtime_with_reactor() {
    let mut rt = SchedulerRuntime::with_durability();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Process suspends

    // Wait for reactor to complete
    std::thread::sleep(
      std::time::Duration::from_millis(200),
    );
    rt.tick(); // Deliver result

    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(completed[0].1, Reason::Normal));
  }

  #[test]
  fn test_runtime_kill() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(Echo));
    rt.kill(pid);
    rt.tick();
    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(completed[0].1, Reason::Kill));
  }

  #[test]
  fn test_runtime_many_processes() {
    let mut rt = SchedulerRuntime::new();
    for _ in 0..100 {
      let pid = rt.spawn(Box::new(Echo));
      rt.send(Envelope::text(pid, "hello"));
    }
    assert_eq!(rt.process_count(), 100);
    rt.tick();
    assert_eq!(rt.process_count(), 100);
  }

  #[test]
  fn test_runtime_message_routing() {
    let mut rt = SchedulerRuntime::new();
    let receiver = rt.spawn(Box::new(Echo));
    let sender =
      rt.spawn(Box::new(Forwarder { target: receiver }));
    rt.send(Envelope::text(sender, "trigger"));
    rt.tick(); // Sender processes and emits outbound
    rt.tick(); // Receiver processes forwarded message
  }

  #[test]
  fn test_runtime_run_until_idle() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(Stopper));
    rt.send(Envelope::text(pid, "stop"));
    let total = rt.run(100);
    assert!(total > 0);
  }

  #[test]
  fn test_runtime_reap() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(Stopper));
    rt.send(Envelope::text(pid, "stop"));
    rt.tick();
    let reaped = rt.reap_completed();
    assert_eq!(reaped.len(), 1);
    assert_eq!(rt.process_count(), 0);
  }

  #[test]
  fn test_runtime_metrics_spawn() {
    let mut rt = SchedulerRuntime::new();
    rt.spawn(Box::new(Echo));
    rt.spawn(Box::new(Echo));
    assert_eq!(
      rt.metrics().counter("processes.spawned"),
      2
    );
    assert_eq!(rt.metrics().gauge("processes.active"), 2);
  }

  #[test]
  fn test_runtime_metrics_ticks() {
    let mut rt = SchedulerRuntime::new();
    rt.spawn(Box::new(Echo));
    rt.tick();
    assert_eq!(
      rt.metrics().counter("scheduler.ticks"),
      1
    );
  }

  #[test]
  fn test_runtime_metrics_budget_blocked() {
    let budget = BudgetState {
      usd_remaining: 0.0,
      token_remaining: 0,
    };
    let mut rt =
      SchedulerRuntime::new().with_budget(budget);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();
    assert_eq!(
      rt.metrics().counter("budget.blocked"),
      1
    );
  }

  #[test]
  fn test_runtime_metrics_process_exit() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(Stopper));
    rt.send(Envelope::text(pid, "stop"));
    rt.tick();
    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(
      rt.metrics().counter("processes.exited"),
      1
    );
  }
}
