use std::collections::{HashMap, HashSet};

use tracing::{debug, warn};

use crate::{
  control::budget::BudgetGate,
  control::policy::{PolicyDecision, PolicyEngine},
  core::{
    behavior::StepBehavior,
    effect::{
      CompensationSpec, EffectKind, EffectRequest,
      EffectResult,
    },
    message::Envelope,
  },
  durability::{
    idempotency::IdempotencyStore,
    journal::Journal,
    snapshot::SnapshotStore,
  },
  effects::config::ProviderConfig,
  error::Reason,
  kernel::{
    compensation::{CompensationEntry, CompensationLog},
    event_bus::{EventBus, RuntimeEvent},
    metrics::Metrics,
    process_table::ProcessRuntimeState,
    reactor::Reactor,
    scheduler::SchedulerEngine,
    turn_executor::{TurnCommit, TurnExecutor},
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
  turn_executor: Option<TurnExecutor>,
  compensation_log: CompensationLog,
  pending_compensations:
    HashMap<u64, (Pid, CompensationSpec)>,
  idempotency_store: Option<IdempotencyStore>,
  provider_config: Option<ProviderConfig>,
  budget: BudgetState,
  budget_gate: Option<BudgetGate>,
  budget_violations: Vec<BudgetViolation>,
  budget_enabled: bool,
  policy: Option<PolicyEngine>,
  event_bus: EventBus,
  metrics: Metrics,
}

impl SchedulerRuntime {
  /// Create a runtime with in-memory durability (for testing).
  pub fn new() -> Self {
    Self {
      engine: SchedulerEngine::new(),
      reactor: None,
      turn_executor: None,
      compensation_log: CompensationLog::new(),
      pending_compensations: HashMap::new(),
      idempotency_store: None,
      provider_config: None,
      budget: BudgetState::default(),
      budget_gate: None,
      budget_violations: Vec::new(),
      budget_enabled: false,
      policy: None,
      event_bus: EventBus::new(10_000),
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
      compensation_log: CompensationLog::new(),
      pending_compensations: HashMap::new(),
      idempotency_store: Some(
        IdempotencyStore::open_in_memory().unwrap(),
      ),
      provider_config: None,
      budget: BudgetState::default(),
      budget_gate: None,
      budget_violations: Vec::new(),
      budget_enabled: false,
      policy: None,
      event_bus: EventBus::new(10_000),
      metrics: Metrics::new(),
    }
  }

  /// Configure provider credentials for LLM/HTTP effects.
  ///
  /// If a reactor already exists it is restarted with the
  /// new provider config so that real API keys take effect.
  pub fn with_provider_config(
    mut self,
    config: ProviderConfig,
  ) -> Self {
    if self.reactor.is_some() {
      self.reactor = Some(
        Reactor::start_with_config(
          Some(config.clone()),
        ),
      );
    }
    self.provider_config = Some(config);
    self
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

  /// Enable atomic two-phase budget gate.
  pub fn with_budget_gate(
    mut self,
    token_limit: Option<u64>,
    cost_limit_microdollars: Option<u64>,
  ) -> Self {
    self.budget_gate = Some(BudgetGate::new(
      token_limit,
      cost_limit_microdollars,
    ));
    self.budget_enabled = true;
    self
  }

  /// Attach a policy engine for effect-level access control.
  pub fn with_policy(mut self, policy: PolicyEngine) -> Self {
    self.policy = Some(policy);
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
    self.event_bus.emit(RuntimeEvent::ProcessSpawned {
      pid,
      behavior_module: "unknown".into(),
    });
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
  /// 2. Advance logical clock (fires timers)
  /// 3. Tick the scheduler engine
  /// 4. Take outbound effects and messages
  /// 5. Journal before dispatch (if turn_executor present)
  /// 6. Deliver messages (after journaling)
  /// 7. Process outbound effects (budget gate -> reactor)
  pub fn tick(&mut self) -> usize {
    // 1. Drain reactor messages (state transitions +
    //    completions)
    if let Some(ref reactor) = self.reactor {
      use crate::core::effect::{EffectState, EffectStatus};
      use crate::kernel::reactor::ReactorMessage;

      let messages = reactor.drain_messages();
      for msg in messages {
        match msg {
          ReactorMessage::StateChanged {
            effect_id,
            pid,
            new_state,
          } => {
            self.engine.update_effect_state(
              effect_id.raw(),
              new_state.clone(),
            );
            self.event_bus.emit(
              RuntimeEvent::EffectStateChanged {
                pid,
                effect_id: effect_id.raw(),
                new_state: new_state.clone(),
              },
            );
            // Journal the state transition
            if let Some(ref executor) =
              self.turn_executor
            {
              let payload = serde_json::to_vec(
                &serde_json::json!({
                  "effect_id": effect_id.raw(),
                  "new_state": new_state,
                }),
              )
              .ok();
              let entry =
                crate::durability::journal::JournalEntry::new(
                  pid,
                  crate::durability::journal::JournalEntryType::EffectStateChanged,
                  payload,
                );
              let _ = executor.journal().append(&entry);
            }
          }
          ReactorMessage::Completion(completion) => {
            if completion.result.status
              == EffectStatus::Streaming
            {
              // Streaming chunk: deliver to process
              // mailbox but don't mark terminal
              self.engine.deliver_streaming_chunk(
                completion.result,
              );
              continue;
            }

            // Set terminal state for non-streaming
            self.engine.update_effect_state(
              completion.result.effect_id.raw(),
              EffectState::Completed(
                completion.result.status.clone(),
              ),
            );

            // Record compensation if effect succeeded
            if completion.result.status
              == EffectStatus::Succeeded
            {
              if let Some((pid, spec)) = self
                .pending_compensations
                .remove(
                  &completion.result.effect_id.raw(),
                )
              {
                self.compensation_log.record(
                  pid,
                  CompensationEntry {
                    effect_id: completion
                      .result
                      .effect_id,
                    spec,
                    completed_at_ms:
                      std::time::SystemTime::now()
                        .duration_since(
                          std::time::UNIX_EPOCH,
                        )
                        .unwrap_or_default()
                        .as_millis()
                        as u64,
                  },
                );
              }

              // Commit usage to BudgetGate
              if let Some(ref gate) = self.budget_gate {
                let tokens = completion
                  .result
                  .output
                  .as_ref()
                  .and_then(|v| v.get("tokens_used"))
                  .and_then(|v| v.as_u64())
                  .unwrap_or(100);
                let cost = completion
                  .result
                  .output
                  .as_ref()
                  .and_then(|v| {
                    v.get("cost_microdollars")
                  })
                  .and_then(|v| v.as_u64())
                  .unwrap_or(0);
                gate.commit(tokens, cost);
              }
            }
            // Record in idempotency store
            if let Some(ref store) =
              self.idempotency_store
            {
              let _ = store.record(
                &completion.result.effect_id,
                &completion.result,
              );
            }

            // Track effect completion metrics
            match completion.result.status {
              EffectStatus::Succeeded => {
                self.metrics.inc("effects.completed");
              }
              EffectStatus::Failed => {
                self.metrics.inc("effects.failed");
              }
              EffectStatus::TimedOut => {
                self
                  .metrics
                  .inc("effects.timed_out");
              }
              EffectStatus::Cancelled => {}
              EffectStatus::Streaming => {}
            }

            self.engine.deliver_effect_result(
              completion.result,
            );
          }
        }
      }
    }

    // 2. Advance logical clock (fires timers)
    let now_ms = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_millis() as u64;
    self.engine.advance_clock(now_ms);

    // 3. Tick scheduler
    let stepped = self.engine.tick();
    debug!(stepped, "runtime tick complete");
    self.metrics.inc("scheduler.ticks");
    let expired = self.engine.take_pending_expired_count();
    if expired > 0 {
      self.metrics.inc_by("messages.expired", expired as u64);
    }

    // 4. Take outbound effects, messages, and patches
    let effects = self.engine.take_outbound_effects();
    let messages = self.engine.take_outbound_messages();
    let patches = self.engine.take_state_patches();

    // 5. Journal before dispatch (if turn_executor present)
    if let Some(ref mut executor) = self.turn_executor {
      // Group effects by pid
      let mut effects_by_pid: HashMap<
        Pid,
        Vec<(Pid, EffectRequest)>,
      > = HashMap::new();
      for (pid, req) in &effects {
        effects_by_pid
          .entry(*pid)
          .or_default()
          .push((*pid, req.clone()));
      }

      // Group messages by sender
      let mut messages_by_sender: HashMap<
        Pid,
        Vec<Envelope>,
      > = HashMap::new();
      for msg in &messages {
        let sender = msg.from.unwrap_or(msg.to);
        messages_by_sender
          .entry(sender)
          .or_default()
          .push(msg.clone());
      }

      // Group patches by pid
      let mut patches_by_pid: HashMap<Pid, Vec<u8>> =
        HashMap::new();
      for (pid, data) in patches {
        patches_by_pid.insert(pid, data);
      }

      // All pids that produced output
      let mut pids: HashSet<Pid> = HashSet::new();
      pids.extend(effects_by_pid.keys());
      pids.extend(messages_by_sender.keys());
      pids.extend(patches_by_pid.keys().copied());

      // Build and commit TurnCommit per pid
      for pid in pids {
        let mut journal_entries = Vec::new();
        if let Some(state_data) =
          patches_by_pid.remove(&pid)
        {
          use crate::durability::journal::{
            JournalEntry, JournalEntryType,
          };
          journal_entries.push(JournalEntry::new(
            pid,
            JournalEntryType::StatePatched,
            Some(state_data),
          ));
        }

        let turn = TurnCommit {
          pid,
          turn_id:
            crate::core::turn_context::next_turn_id(),
          journal_entries,
          outbound_messages: messages_by_sender
            .remove(&pid)
            .unwrap_or_default(),
          effect_requests: effects_by_pid
            .remove(&pid)
            .unwrap_or_default(),
          state_snapshot: self
            .engine
            .snapshot_for(pid),
        };
        if let Err(e) = executor.commit(&turn) {
          warn!(
            pid = %pid,
            error = %e,
            "turn commit failed"
          );
        } else {
          self.metrics.inc("turns.committed");
        }
      }
    }

    // 6. Deliver messages (after journaling)
    for msg in messages {
      self.engine.send(msg);
    }

    // 7. Process outbound effects (budget gate -> reactor)
    for (pid, req) in effects {
      // Check idempotency store before dispatch
      if let Some(ref store) = self.idempotency_store {
        if req.idempotency_key.is_some() {
          if let Ok(Some(cached)) =
            store.check(&req.effect_id)
          {
            self
              .engine
              .deliver_effect_result(cached);
            continue;
          }
        }
      }

      self.event_bus.emit(RuntimeEvent::EffectRequested {
        pid,
        effect_id: req.effect_id.raw(),
        kind: req.kind.clone(),
      });

      // Policy check
      if let Some(ref policy) = self.policy {
        let decision = policy.evaluate(&req.kind, None);
        if let PolicyDecision::Deny(reason) = decision {
          self.event_bus.emit(
            RuntimeEvent::PolicyEvaluated {
              pid,
              effect_id: req.effect_id.raw(),
              kind: req.kind.clone(),
              decision: format!("Deny: {}", reason),
            },
          );
          self.event_bus.emit(
            RuntimeEvent::EffectBlocked {
              pid,
              effect_id: req.effect_id.raw(),
              kind: req.kind.clone(),
              reason: reason.clone(),
            },
          );
          warn!(
            pid = %pid,
            kind = ?req.kind,
            reason = %reason,
            "effect blocked by policy"
          );
          let result = EffectResult::failure(
            req.effect_id,
            format!("policy denied: {}", reason),
          );
          self.metrics.inc("policy.blocked");
          self.engine.deliver_effect_result(result);
          continue;
        }
      }

      if self.budget_enabled
        && self.should_block_effect(&req)
      {
        self.event_bus.emit(
          RuntimeEvent::EffectBlocked {
            pid,
            effect_id: req.effect_id.raw(),
            kind: req.kind.clone(),
            reason: "budget exceeded".into(),
          },
        );
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
        self.event_bus.emit(
          RuntimeEvent::EffectDispatched {
            pid,
            effect_id: req.effect_id.raw(),
            kind: req.kind.clone(),
          },
        );
        // Track compensatable effects
        if let Some(ref comp) = req.compensation {
          self.pending_compensations.insert(
            req.effect_id.raw(),
            (pid, comp.clone()),
          );
        }
        self.metrics.inc("effects.dispatched");
        if let Some(ref reactor) = self.reactor {
          reactor.dispatch(pid, req);
        }
      }
    }

    // 8. Process rollback requests
    let rollback_pids =
      self.engine.take_rollback_requests();
    for pid in rollback_pids {
      let undo_effects =
        self.compensation_log.rollback_all(pid);
      for req in undo_effects {
        self.metrics.inc("compensation.triggered");
        if let Some(ref reactor) = self.reactor {
          reactor.dispatch(pid, req);
        }
      }
    }

    // 9. Process spawn requests
    let spawn_requests =
      self.engine.take_spawn_requests();
    for behavior in spawn_requests {
      self.spawn(behavior);
    }

    // 10. Apply budget debits
    let debits = self.engine.take_budget_debits();
    for (_pid, tokens, cost_microdollars) in debits {
      if self.budget.token_remaining >= tokens {
        self.budget.token_remaining -= tokens;
      } else {
        self.budget.token_remaining = 0;
      }
      let cost_dollars =
        cost_microdollars as f64 / 1_000_000.0;
      self.budget.usd_remaining -= cost_dollars;
      if self.budget.usd_remaining < 0.0 {
        self.budget.usd_remaining = 0.0;
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
        // Use BudgetGate if available (atomic two-phase)
        if let Some(ref gate) = self.budget_gate {
          let estimated_tokens = 1000;
          return !gate.precheck(estimated_tokens);
        }
        // Fall back to simple BudgetState
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
    for (pid, reason) in &completed {
      self.metrics.inc("processes.exited");
      self.event_bus.emit(RuntimeEvent::ProcessExited {
        pid: *pid,
        reason: reason.clone(),
      });
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

  /// Get a reference to the BudgetGate (if configured).
  pub fn budget_gate(&self) -> Option<&BudgetGate> {
    self.budget_gate.as_ref()
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

  /// Deliver a streaming chunk directly (for testing
  /// without reactor). Does not remove from
  /// pending_effects.
  pub fn deliver_streaming_chunk(
    &mut self,
    result: EffectResult,
  ) {
    self.engine.deliver_streaming_chunk(result);
  }

  /// Drain all buffered runtime events.
  pub fn drain_events(
    &mut self,
  ) -> Vec<(u64, RuntimeEvent)> {
    self.event_bus.drain_events()
  }

  /// Peek recent events without draining.
  pub fn recent_events(
    &self,
    n: usize,
  ) -> Vec<(u64, RuntimeEvent)> {
    self.event_bus.recent(n)
  }

  /// Get a reference to the metrics.
  pub fn metrics(&self) -> &Metrics {
    &self.metrics
  }

  /// Recover a process from durable storage
  /// (journal + snapshot).
  pub fn recover_process(
    &mut self,
    pid: Pid,
    factory: &dyn Fn() -> Box<dyn StepBehavior>,
  ) -> Result<(), String> {
    let executor = self
      .turn_executor
      .as_ref()
      .ok_or("no turn executor configured")?;

    let coord =
      crate::kernel::recovery::RecoveryCoordinator::new(
        executor.journal(),
        executor.snapshot_store(),
      );

    let recovered = coord.recover_process(pid, factory)?;

    // Re-insert recovered process into scheduler
    self.engine.insert_process(recovered.entry);

    // Re-schedule recovered timers
    for timer in recovered.timers {
      self.engine.schedule_timer(timer);
    }

    self.metrics.inc("processes.spawned");
    self.metrics.gauge_set(
      "processes.active",
      self.engine.process_count() as i64,
    );

    Ok(())
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

  #[test]
  fn test_runtime_journals_before_dispatch() {
    let mut rt = SchedulerRuntime::with_durability();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    assert!(
      rt.metrics().counter("turns.committed") > 0,
      "turn should have been committed to journal"
    );
    assert!(
      rt.metrics().counter("effects.dispatched") > 0,
      "effect should have been dispatched"
    );
  }

  #[test]
  fn test_runtime_advances_clock() {
    use crate::core::message::Signal;
    use crate::core::timer::{TimerKind, TimerSpec};

    struct TimerBehavior;
    impl StepBehavior for TimerBehavior {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        msg: Envelope,
        ctx: &mut TurnContext,
      ) -> StepResult {
        match &msg.payload {
          EnvelopePayload::Signal(
            Signal::TimerFired(_),
          ) => StepResult::Done(Reason::Normal),
          _ => {
            let spec = TimerSpec::new(
              ctx.pid,
              TimerKind::Timeout,
              1, // 1ms — fires on next tick
            );
            ctx.schedule_timer(spec);
            StepResult::Continue
          }
        }
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(TimerBehavior));
    rt.send(Envelope::text(pid, "schedule"));
    rt.tick(); // Schedules 1ms timer

    // Wait for clock to advance past deadline
    std::thread::sleep(
      std::time::Duration::from_millis(10),
    );
    rt.tick(); // Clock advances, timer fires, process exits

    let completed = rt.take_completed();
    assert_eq!(
      completed.len(),
      1,
      "timer should have fired"
    );
    assert!(matches!(
      completed[0].1,
      Reason::Normal
    ));
  }

  #[test]
  fn test_runtime_idempotency_records() {
    // Verify that completed effects are recorded
    // in the idempotency store
    let mut rt = SchedulerRuntime::with_durability();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Suspends with LlmCall

    // Wait for reactor completion
    std::thread::sleep(
      std::time::Duration::from_millis(200),
    );
    rt.tick(); // Delivers result

    // Process should be done
    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
  }

  #[test]
  fn test_runtime_compensation_rollback() {
    use crate::core::effect::{
      CompensationSpec, EffectKind, EffectRequest,
    };

    struct CompensatingAgent {
      phase: u8,
    }
    impl StepBehavior for CompensatingAgent {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _msg: Envelope,
        ctx: &mut TurnContext,
      ) -> StepResult {
        match self.phase {
          0 => {
            self.phase = 1;
            StepResult::Suspend(
              EffectRequest::new(
                EffectKind::Http,
                serde_json::json!({
                  "action": "charge"
                }),
              )
              .with_compensation(CompensationSpec {
                undo_kind: EffectKind::Http,
                undo_input: serde_json::json!({
                  "action": "refund"
                }),
              }),
            )
          }
          1 => {
            // Effect result received; trigger rollback
            self.phase = 2;
            ctx.rollback();
            StepResult::Done(Reason::Normal)
          }
          _ => StepResult::Continue,
        }
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let mut rt = SchedulerRuntime::with_durability();
    let pid = rt.spawn(Box::new(
      CompensatingAgent { phase: 0 },
    ));
    rt.send(Envelope::text(pid, "start"));
    rt.tick(); // Suspends with compensatable effect

    // Wait for reactor
    std::thread::sleep(
      std::time::Duration::from_millis(200),
    );
    // Delivers result, process rollbacks + exits
    rt.tick();

    assert!(
      rt.metrics().counter("compensation.triggered")
        > 0,
      "compensation should have been triggered"
    );
  }

  #[test]
  fn test_runtime_metrics_effects_completed() {
    let mut rt = SchedulerRuntime::with_durability();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Suspends

    std::thread::sleep(
      std::time::Duration::from_millis(200),
    );
    rt.tick(); // Delivers result

    assert!(
      rt.metrics().counter("effects.completed") > 0,
      "effects.completed should be incremented"
    );
  }

  #[test]
  fn test_runtime_recover_process() {
    struct Recoverable {
      state: String,
    }
    impl StepBehavior for Recoverable {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        self.state = "fresh".into();
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        _: &mut TurnContext,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
      fn restore(
        &mut self,
        data: &[u8],
      ) -> Result<(), String> {
        self.state =
          String::from_utf8(data.to_vec())
            .map_err(|e| e.to_string())?;
        Ok(())
      }
      fn snapshot(&self) -> Option<Vec<u8>> {
        Some(self.state.as_bytes().to_vec())
      }
    }

    let mut rt = SchedulerRuntime::with_durability();
    let pid = Pid::from_raw(42);

    // Recover (no prior state -- creates fresh process)
    let result = rt.recover_process(pid, &|| {
      Box::new(Recoverable {
        state: String::new(),
      })
    });
    assert!(result.is_ok());
    assert_eq!(rt.process_count(), 1);

    // Process should be ready and responsive
    rt.send(Envelope::text(pid, "hello"));
    rt.tick();
    assert_eq!(rt.process_count(), 1);
  }

  #[test]
  fn test_runtime_recover_requires_durability() {
    let mut rt = SchedulerRuntime::new();
    let pid = Pid::from_raw(1);
    let result =
      rt.recover_process(pid, &|| Box::new(Echo));
    assert!(result.is_err());
    assert!(
      result
        .unwrap_err()
        .contains("no turn executor"),
    );
  }

  // ── Phase 2.5 gate tests ─────────────────────────────

  #[test]
  fn test_gate_journal_before_dispatch_invariant() {
    // Core invariant: effects are journaled BEFORE
    // reactor dispatch
    let mut rt = SchedulerRuntime::with_durability();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    let m = rt.metrics();
    assert!(
      m.counter("turns.committed") >= 1,
      "turn must be committed"
    );
    assert!(
      m.counter("effects.dispatched") >= 1,
      "effect must be dispatched"
    );
  }

  #[test]
  fn test_gate_message_journaled_before_delivery() {
    // Messages are journaled before being delivered
    let mut rt = SchedulerRuntime::with_durability();
    let receiver = rt.spawn(Box::new(Echo));
    let sender = rt.spawn(Box::new(Forwarder {
      target: receiver,
    }));
    rt.send(Envelope::text(sender, "trigger"));
    rt.tick();

    assert!(
      rt.metrics().counter("turns.committed") >= 1,
      "message turn must be committed"
    );
  }

  #[test]
  fn test_gate_full_lifecycle_metrics() {
    // Full lifecycle: spawn -> effect -> completion
    // -> exit. All metric categories should be populated.
    let mut rt = SchedulerRuntime::with_durability();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Suspends with effect

    std::thread::sleep(
      std::time::Duration::from_millis(200),
    );
    rt.tick(); // Delivers result, process exits

    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(
      completed[0].1,
      Reason::Normal
    ));

    let m = rt.metrics();
    assert!(m.counter("processes.spawned") >= 1);
    assert!(m.counter("effects.dispatched") >= 1);
    assert!(m.counter("effects.completed") >= 1);
    assert!(m.counter("turns.committed") >= 1);
    assert!(m.counter("processes.exited") >= 1);
    assert!(m.counter("scheduler.ticks") >= 2);
  }

  #[test]
  fn test_gate_runtime_without_durability_still_works()
  {
    // Runtime without durability should work fine
    // (no journaling, but no crashes either)
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    // No turns committed (no executor)
    assert_eq!(
      rt.metrics().counter("turns.committed"),
      0
    );
    // But effect was still dispatched (to nowhere)
    assert!(
      rt.metrics().counter("effects.dispatched") >= 1
    );
  }

  #[test]
  fn test_gate_multiple_processes_independent_journals()
  {
    // Multiple processes each get their own TurnCommit
    let mut rt = SchedulerRuntime::with_durability();
    let pid1 = rt.spawn(Box::new(LlmCaller));
    let pid2 = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid1, "go"));
    rt.send(Envelope::text(pid2, "go"));
    rt.tick();

    // Both should be journaled
    assert!(
      rt.metrics().counter("turns.committed") >= 2,
      "each process should have its turn committed"
    );
  }

  #[test]
  fn test_runtime_budget_gate_blocks_when_exhausted() {
    let mut rt = SchedulerRuntime::new()
      .with_budget_gate(Some(100), None); // 100 token limit

    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    // With 100 token limit and 1000 estimated, precheck
    // fails so the effect should be blocked
    let violations = rt.take_budget_violations();
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].pid, pid);
  }

  #[test]
  fn test_runtime_budget_gate_allows_when_sufficient() {
    let mut rt = SchedulerRuntime::new()
      .with_budget_gate(Some(1_000_000), None);

    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    let violations = rt.take_budget_violations();
    assert_eq!(violations.len(), 0);
  }

  #[test]
  fn test_runtime_budget_gate_accessor() {
    let rt = SchedulerRuntime::new()
      .with_budget_gate(Some(500), Some(10_000));

    let gate = rt.budget_gate().unwrap();
    assert_eq!(gate.tokens_used(), 0);
    assert_eq!(gate.cost_used(), 0);
  }

  #[test]
  fn test_runtime_no_budget_gate_by_default() {
    let rt = SchedulerRuntime::new();
    assert!(rt.budget_gate().is_none());
  }

  #[test]
  fn test_runtime_debit_budget_intent() {
    struct BudgetDebiter;
    impl StepBehavior for BudgetDebiter {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _: Envelope,
        ctx: &mut TurnContext,
      ) -> StepResult {
        ctx.debit_budget(1000, 500_000); // 1000 tokens, $0.50
        StepResult::Done(Reason::Normal)
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let budget = BudgetState {
      usd_remaining: 10.0,
      token_remaining: 100_000,
    };
    let mut rt =
      SchedulerRuntime::new().with_budget(budget);
    let pid = rt.spawn(Box::new(BudgetDebiter));
    rt.send(Envelope::text(pid, "debit"));
    rt.tick();

    assert_eq!(rt.budget().token_remaining, 99_000);
    // 10.0 - 0.50 = 9.50
    assert!(
      (rt.budget().usd_remaining - 9.5).abs() < 0.01
    );
  }

  #[test]
  fn test_runtime_with_provider_config() {
    use crate::effects::config::ProviderConfig;
    let config = ProviderConfig {
      openai_api_key: Some("sk-test".into()),
      ..Default::default()
    };
    let rt = SchedulerRuntime::with_durability()
      .with_provider_config(config);
    assert!(rt.provider_config.is_some());
    assert!(rt.has_reactor());
  }

  #[test]
  fn test_runtime_streaming_chunk_not_counted() {
    use crate::core::effect::{
      EffectId, EffectResult, EffectStatus,
    };

    // Use runtime without reactor so we can inject
    // completions manually via deliver_effect_result.
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Suspends with LlmCall

    // Deliver a streaming chunk directly via engine
    // (simulates what tick() does when reactor returns
    // a Streaming completion).
    let effect_id = {
      // We need the effect_id but it was consumed.
      // Retrieve it from the process state.
      match rt.process_state(pid) {
        Some(
          ProcessRuntimeState::WaitingEffect(raw),
        ) => EffectId::from_raw(raw),
        _ => panic!("expected WaitingEffect"),
      }
    };

    let chunk = EffectResult {
      effect_id,
      status: EffectStatus::Streaming,
      output: Some(
        serde_json::json!({"delta": "tok1"}),
      ),
      error: None,
    };
    rt.deliver_streaming_chunk(chunk);

    // Streaming chunk should NOT count as completed
    assert_eq!(
      rt.metrics().counter("effects.completed"),
      0,
      "streaming chunk must not increment \
       effects.completed"
    );

    // Process should still be waiting (not woken)
    assert!(matches!(
      rt.process_state(pid),
      Some(ProcessRuntimeState::WaitingEffect(_))
    ));

    // Now deliver the final result
    let final_result = EffectResult::success(
      effect_id,
      serde_json::json!({"content": "done"}),
    );
    rt.deliver_effect_result(final_result);
    rt.tick(); // Process handles result and exits

    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(
      completed[0].1,
      Reason::Normal
    ));
  }

  #[test]
  fn test_runtime_policy_denies_effect() {
    use crate::control::policy::{
      PolicyDecision, PolicyEngine, PolicyRule,
    };

    let policy = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::LlmCall,
        decision: PolicyDecision::Deny(
          "LLM calls blocked".into(),
        ),
        max_cost_microdollars: None,
      }],
      PolicyDecision::Allow,
    );
    let mut rt = SchedulerRuntime::new().with_policy(policy);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick(); // Process suspends with LlmCall; policy blocks it
    rt.tick(); // Process handles failed effect result and exits

    let completed = rt.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(completed[0].1, Reason::Normal));
  }

  #[test]
  fn test_runtime_policy_allows_effect() {
    use crate::control::policy::{
      PolicyDecision, PolicyEngine, PolicyRule,
    };

    let policy = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::DbWrite,
        decision: PolicyDecision::Deny(
          "writes blocked".into(),
        ),
        max_cost_microdollars: None,
      }],
      PolicyDecision::Allow,
    );
    let mut rt = SchedulerRuntime::new().with_policy(policy);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    let violations = rt.take_budget_violations();
    assert_eq!(violations.len(), 0);
  }

  #[test]
  fn test_runtime_no_policy_allows_all() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();
    let violations = rt.take_budget_violations();
    assert_eq!(violations.len(), 0);
  }

  // ── EventBus integration tests ────────────────────────

  #[test]
  fn test_runtime_event_bus_process_lifecycle() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(Stopper));
    rt.send(Envelope::text(pid, "stop"));
    rt.tick();
    let _ = rt.take_completed();

    let events = rt.drain_events();
    assert!(
      events.iter().any(|(_, e)| matches!(
        e,
        crate::kernel::event_bus::RuntimeEvent::ProcessSpawned { .. }
      )),
      "should have ProcessSpawned event"
    );
    assert!(
      events.iter().any(|(_, e)| matches!(
        e,
        crate::kernel::event_bus::RuntimeEvent::ProcessExited { .. }
      )),
      "should have ProcessExited event"
    );
  }

  #[test]
  fn test_runtime_event_bus_effect_lifecycle() {
    let mut rt = SchedulerRuntime::new();
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    let events = rt.drain_events();
    assert!(
      events.iter().any(|(_, e)| matches!(
        e,
        crate::kernel::event_bus::RuntimeEvent::EffectRequested { .. }
      )),
      "should have EffectRequested event"
    );
    assert!(
      events.iter().any(|(_, e)| matches!(
        e,
        crate::kernel::event_bus::RuntimeEvent::EffectDispatched { .. }
      )),
      "should have EffectDispatched event"
    );
  }

  #[test]
  fn test_runtime_event_bus_policy_blocked() {
    use crate::control::policy::{
      PolicyDecision, PolicyEngine, PolicyRule,
    };

    let policy = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::LlmCall,
        decision: PolicyDecision::Deny(
          "blocked".into(),
        ),
        max_cost_microdollars: None,
      }],
      PolicyDecision::Allow,
    );
    let mut rt =
      SchedulerRuntime::new().with_policy(policy);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();

    let events = rt.drain_events();
    assert!(
      events.iter().any(|(_, e)| matches!(
        e,
        crate::kernel::event_bus::RuntimeEvent::PolicyEvaluated { .. }
      )),
      "should have PolicyEvaluated event"
    );
    assert!(
      events.iter().any(|(_, e)| matches!(
        e,
        crate::kernel::event_bus::RuntimeEvent::EffectBlocked { .. }
      )),
      "should have EffectBlocked event"
    );
  }

  #[test]
  fn test_runtime_drain_events_empty_after_drain() {
    let mut rt = SchedulerRuntime::new();
    rt.spawn(Box::new(Echo));
    let events = rt.drain_events();
    assert!(!events.is_empty());
    let events2 = rt.drain_events();
    assert!(events2.is_empty());
  }

  #[test]
  fn test_runtime_metrics_policy_blocked() {
    use crate::control::policy::{
      PolicyDecision, PolicyEngine, PolicyRule,
    };

    let policy = PolicyEngine::new(
      vec![PolicyRule {
        kind: EffectKind::LlmCall,
        decision: PolicyDecision::Deny(
          "blocked".into(),
        ),
        max_cost_microdollars: None,
      }],
      PolicyDecision::Allow,
    );
    let mut rt = SchedulerRuntime::new()
      .with_policy(policy);
    let pid = rt.spawn(Box::new(LlmCaller));
    rt.send(Envelope::text(pid, "go"));
    rt.tick();
    assert_eq!(
      rt.metrics().counter("policy.blocked"),
      1,
    );
  }
}
