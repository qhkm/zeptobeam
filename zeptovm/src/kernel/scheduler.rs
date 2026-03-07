use std::collections::HashMap;

use tracing::info_span;

use crate::{
  core::{
    effect::{EffectRequest, EffectResult},
    message::{Envelope, Signal},
    step_result::StepResult,
    turn_context::TurnIntent,
  },
  error::Reason,
  kernel::{
    name_registry::NameRegistry,
    process_table::{ProcessEntry, ProcessRuntimeState},
    timer_wheel::TimerWheel,
  },
  link::LinkTable,
  pid::Pid,
};

/// Pending effects waiting for results.
struct PendingEffect {
  pid: Pid,
  #[allow(dead_code)] // Used by reactor in Phase 2
  request: EffectRequest,
}

/// Single-thread scheduler engine.
///
/// Owns the process table and local run queue.
/// Steps processes up to max_reductions per tick.
pub struct SchedulerEngine {
  processes: HashMap<Pid, ProcessEntry>,
  ready_queue: Vec<Pid>,
  pending_effects: HashMap<u64, PendingEffect>,
  outbound_effects: Vec<(Pid, EffectRequest)>,
  outbound_messages: Vec<Envelope>,
  state_patches: Vec<(Pid, Vec<u8>)>,
  rollback_pids: Vec<Pid>,
  spawn_requests:
    Vec<Box<dyn crate::core::behavior::StepBehavior>>,
  budget_debits: Vec<(Pid, u64, u64)>,
  max_reductions: u32,
  completed: Vec<(Pid, Reason)>,
  timer_wheel: TimerWheel,
  clock_ms: u64,
  link_table: LinkTable,
  name_registry: NameRegistry,
  tick_count: u64,
}

impl SchedulerEngine {
  pub fn new() -> Self {
    Self {
      processes: HashMap::new(),
      ready_queue: Vec::new(),
      pending_effects: HashMap::new(),
      outbound_effects: Vec::new(),
      outbound_messages: Vec::new(),
      state_patches: Vec::new(),
      rollback_pids: Vec::new(),
      spawn_requests: Vec::new(),
      budget_debits: Vec::new(),
      max_reductions: 200,
      completed: Vec::new(),
      timer_wheel: TimerWheel::new(),
      clock_ms: 0,
      link_table: LinkTable::new(),
      name_registry: NameRegistry::new(),
      tick_count: 0,
    }
  }

  pub fn with_max_reductions(mut self, max: u32) -> Self {
    self.max_reductions = max;
    self
  }

  /// Spawn a new process with the given behavior.
  pub fn spawn(
    &mut self,
    behavior: Box<dyn crate::core::behavior::StepBehavior>,
  ) -> Pid {
    let pid = Pid::new();
    let mut entry = ProcessEntry::new(pid, behavior);
    entry.init(None);
    entry.state = ProcessRuntimeState::Ready;
    self.processes.insert(pid, entry);
    self.ready_queue.push(pid);
    pid
  }

  /// Send a message to a process.
  pub fn send(&mut self, env: Envelope) {
    let to = env.to;
    if let Some(proc) = self.processes.get_mut(&to) {
      proc.mailbox.push(env);
      // If the process was waiting, wake it up
      if proc.state == ProcessRuntimeState::WaitingMessage {
        proc.state = ProcessRuntimeState::Ready;
        self.ready_queue.push(to);
      }
    }
  }

  /// Deliver an effect result to the waiting process.
  pub fn deliver_effect_result(
    &mut self,
    result: EffectResult,
  ) {
    let effect_raw = result.effect_id.raw();
    if let Some(pending) =
      self.pending_effects.remove(&effect_raw)
    {
      let pid = pending.pid;
      if let Some(proc) = self.processes.get_mut(&pid) {
        proc.mailbox.push(
          Envelope::effect_result(pid, result),
        );
        if matches!(
          proc.state,
          ProcessRuntimeState::WaitingEffect(_)
        ) {
          proc.state = ProcessRuntimeState::Ready;
          self.ready_queue.push(pid);
        }
      }
    }
  }

  /// Deliver a streaming chunk to a process's mailbox
  /// without removing from pending_effects.
  /// The effect is still in-flight; more chunks may follow.
  pub fn deliver_streaming_chunk(
    &mut self,
    result: EffectResult,
  ) {
    let effect_raw = result.effect_id.raw();
    if let Some(pending) =
      self.pending_effects.get(&effect_raw)
    {
      let pid = pending.pid;
      if let Some(proc) =
        self.processes.get_mut(&pid)
      {
        proc.mailbox.push(
          Envelope::effect_result(pid, result),
        );
      }
    }
  }

  /// Run one tick: step all ready processes up to max_reductions
  /// each. Returns: number of processes stepped.
  pub fn tick(&mut self) -> usize {
    self.tick_count += 1;
    let _span = info_span!(
      "scheduler_tick",
      tick = self.tick_count,
    )
    .entered();
    let ready: Vec<Pid> = self.ready_queue.drain(..).collect();
    let mut stepped = 0;

    for pid in ready {
      if !self.processes.contains_key(&pid) {
        continue;
      }

      stepped += 1;
      self.step_process(pid);
    }

    stepped
  }

  fn step_process(&mut self, pid: Pid) {
    let _span =
      info_span!("process_step", pid = pid.raw()).entered();
    // Set to Running (skip already-terminated processes)
    if let Some(proc) = self.processes.get_mut(&pid) {
      if proc.state == ProcessRuntimeState::Done
        || proc.state == ProcessRuntimeState::Failed
      {
        return;
      }
      proc.state = ProcessRuntimeState::Running;
    } else {
      return;
    }

    let mut reductions_left = self.max_reductions;

    loop {
      if reductions_left == 0 {
        // Preempted -- re-enqueue if there are still messages
        if let Some(proc) = self.processes.get_mut(&pid) {
          if proc.mailbox.has_messages() {
            proc.state = ProcessRuntimeState::Ready;
            self.ready_queue.push(pid);
          } else {
            proc.state = ProcessRuntimeState::WaitingMessage;
          }
        }
        return;
      }

      // Step the process -- borrow released after this block
      let (result, intents) = {
        let proc = match self.processes.get_mut(&pid) {
          Some(p) => p,
          None => return,
        };
        let (result, mut ctx) = proc.step(self.clock_ms);
        let intents = ctx.take_intents();
        (result, intents)
      };

      // Process intents (no borrow on self.processes here)
      for intent in intents {
        match intent {
          TurnIntent::SendMessage(msg) => {
            self.outbound_messages.push(msg);
          }
          TurnIntent::RequestEffect(req) => {
            self.outbound_effects.push((pid, req));
          }
          TurnIntent::PatchState(data) => {
            self.state_patches.push((pid, data));
          }
          TurnIntent::ScheduleTimer(spec) => {
            self.timer_wheel.schedule(spec);
          }
          TurnIntent::CancelTimer(id) => {
            self.timer_wheel.cancel(id);
          }
          TurnIntent::Rollback => {
            self.rollback_pids.push(pid);
          }
          TurnIntent::Link(target) => {
            self.link_table.link(pid, target);
          }
          TurnIntent::Unlink(target) => {
            self.link_table.unlink(pid, target);
          }
          TurnIntent::Monitor(target) => {
            self.link_table.monitor(pid, target);
          }
          TurnIntent::Demonitor(mref) => {
            self.link_table.demonitor(mref);
          }
          TurnIntent::SpawnProcess(behavior) => {
            self.spawn_requests.push(behavior);
          }
          TurnIntent::DebitBudget {
            tokens,
            cost_microdollars,
          } => {
            self
              .budget_debits
              .push((pid, tokens, cost_microdollars));
          }
          TurnIntent::RegisterName(name) => {
            let _ = self
              .name_registry
              .register(name, pid);
          }
          TurnIntent::UnregisterName(name) => {
            self.name_registry.unregister(&name);
          }
          TurnIntent::SendNamed { name, payload } => {
            if let Some(target) =
              self.name_registry.whereis(&name)
            {
              let env = Envelope::text(target, payload);
              self.outbound_messages.push(env);
            }
          }
        }
      }

      match result {
        StepResult::Continue => {
          reductions_left -= 1;
          let proc = match self.processes.get_mut(&pid) {
            Some(p) => p,
            None => return,
          };
          if !proc.mailbox.has_messages() {
            // No more messages -- wait
            proc.state = ProcessRuntimeState::WaitingMessage;
            return;
          }
          // Continue processing next message
        }
        StepResult::Wait => {
          if let Some(proc) = self.processes.get_mut(&pid) {
            proc.state = ProcessRuntimeState::WaitingMessage;
          }
          return;
        }
        StepResult::Suspend(req) => {
          let effect_raw = req.effect_id.raw();
          if let Some(proc) = self.processes.get_mut(&pid) {
            proc.state =
              ProcessRuntimeState::WaitingEffect(effect_raw);
          }
          self.pending_effects.insert(
            effect_raw,
            PendingEffect {
              pid,
              request: req.clone(),
            },
          );
          self.outbound_effects.push((pid, req));
          return;
        }
        StepResult::WaitForTag(tag) => {
          if let Some(proc) = self.processes.get_mut(&pid) {
            proc.selective_tag = Some(tag);
            proc.state = ProcessRuntimeState::WaitingMessage;
          }
          return;
        }
        StepResult::Done(reason)
        | StepResult::Fail(reason) => {
          self.propagate_exit(pid, &reason);
          if let Some(proc) = self.processes.get_mut(&pid) {
            proc.terminate(&reason);
          }
          self.completed.push((pid, reason));
          return;
        }
      }
    }
  }

  /// Propagate exit signals to linked processes, monitors,
  /// and parent.
  fn propagate_exit(&mut self, pid: Pid, reason: &Reason) {
    // Auto-unregister name on exit
    self.name_registry.unregister_pid(pid);

    // Links: notify all linked processes
    let linked = self.link_table.remove_all(&pid);
    for linked_pid in linked {
      if let Some(proc) =
        self.processes.get_mut(&linked_pid)
      {
        proc.mailbox.push(Envelope::signal(
          linked_pid,
          Signal::ExitLinked(pid, reason.clone()),
        ));
        if proc.state
          == ProcessRuntimeState::WaitingMessage
        {
          proc.state = ProcessRuntimeState::Ready;
          self.ready_queue.push(linked_pid);
        }
      }
    }

    // Monitors: notify all watchers
    let monitors =
      self.link_table.remove_monitors_of(&pid);
    for (_mref, watcher_pid) in monitors {
      if let Some(proc) =
        self.processes.get_mut(&watcher_pid)
      {
        proc.mailbox.push(Envelope::signal(
          watcher_pid,
          Signal::MonitorDown(pid, reason.clone()),
        ));
        if proc.state
          == ProcessRuntimeState::WaitingMessage
        {
          proc.state = ProcessRuntimeState::Ready;
          self.ready_queue.push(watcher_pid);
        }
      }
    }

    // Parent notification
    let parent_pid =
      self.processes.get(&pid).and_then(|p| p.parent);
    if let Some(parent) = parent_pid {
      if let Some(proc) =
        self.processes.get_mut(&parent)
      {
        proc.mailbox.push(Envelope::signal(
          parent,
          Signal::ChildExited {
            child_pid: pid,
            reason: reason.clone(),
          },
        ));
        if proc.state
          == ProcessRuntimeState::WaitingMessage
        {
          proc.state = ProcessRuntimeState::Ready;
          self.ready_queue.push(parent);
        }
      }
    }
  }

  /// Create a link between two processes (for testing).
  pub fn link(&mut self, a: Pid, b: Pid) {
    self.link_table.link(a, b);
  }

  /// Spawn a process with a parent (for supervisor
  /// wiring).
  pub fn spawn_with_parent(
    &mut self,
    behavior: Box<
      dyn crate::core::behavior::StepBehavior,
    >,
    parent: Pid,
  ) -> Pid {
    let pid = Pid::new();
    let mut entry = ProcessEntry::new(pid, behavior);
    entry.parent = Some(parent);
    entry.init(None);
    entry.state = ProcessRuntimeState::Ready;
    self.processes.insert(pid, entry);
    self.ready_queue.push(pid);
    pid
  }

  /// Take outbound effect requests
  /// (for the reactor/dispatcher).
  pub fn take_outbound_effects(
    &mut self,
  ) -> Vec<(Pid, EffectRequest)> {
    std::mem::take(&mut self.outbound_effects)
  }

  /// Take outbound messages (for runtime-level delivery
  /// and journaling).
  pub fn take_outbound_messages(
    &mut self,
  ) -> Vec<Envelope> {
    std::mem::take(&mut self.outbound_messages)
  }

  /// Take completed processes (for supervisor notification).
  pub fn take_completed(&mut self) -> Vec<(Pid, Reason)> {
    std::mem::take(&mut self.completed)
  }

  /// Take state patches emitted during this tick.
  pub fn take_state_patches(
    &mut self,
  ) -> Vec<(Pid, Vec<u8>)> {
    std::mem::take(&mut self.state_patches)
  }

  /// Take rollback requests from this tick.
  pub fn take_rollback_requests(
    &mut self,
  ) -> Vec<Pid> {
    std::mem::take(&mut self.rollback_pids)
  }

  /// Take spawn requests emitted by behaviors during
  /// this tick.
  pub fn take_spawn_requests(
    &mut self,
  ) -> Vec<Box<dyn crate::core::behavior::StepBehavior>>
  {
    std::mem::take(&mut self.spawn_requests)
  }

  /// Take budget debit requests from this tick.
  pub fn take_budget_debits(
    &mut self,
  ) -> Vec<(Pid, u64, u64)> {
    std::mem::take(&mut self.budget_debits)
  }

  /// Kill a process.
  pub fn kill(&mut self, pid: Pid) {
    if let Some(proc) = self.processes.get_mut(&pid) {
      proc.request_kill();
      // Make sure it gets stepped
      self.ready_queue.push(pid);
    }
  }

  /// Number of live processes.
  pub fn process_count(&self) -> usize {
    self.processes.len()
  }

  /// Check if a specific process exists.
  pub fn has_process(&self, pid: Pid) -> bool {
    self.processes.contains_key(&pid)
  }

  /// Get the state of a process.
  pub fn process_state(
    &self,
    pid: Pid,
  ) -> Option<ProcessRuntimeState> {
    self.processes.get(&pid).map(|p| p.state)
  }

  /// Remove completed processes from the table.
  pub fn reap_completed(&mut self) -> Vec<Pid> {
    let dead: Vec<Pid> = self
      .processes
      .iter()
      .filter(|(_, p)| {
        p.state == ProcessRuntimeState::Done
          || p.state == ProcessRuntimeState::Failed
      })
      .map(|(pid, _)| *pid)
      .collect();
    for pid in &dead {
      self.processes.remove(pid);
    }
    dead
  }

  /// Get a snapshot of a process's behavior state.
  pub fn snapshot_for(
    &self,
    pid: Pid,
  ) -> Option<Vec<u8>> {
    self.processes.get(&pid).and_then(|p| p.snapshot())
  }

  /// Advance the logical clock and deliver fired timers.
  ///
  /// Ticks the timer wheel at `now_ms`, delivering
  /// `Signal::TimerFired` to each owner whose timer expired.
  /// Wakes any owner that was `WaitingMessage`.
  pub fn advance_clock(&mut self, now_ms: u64) {
    self.clock_ms = now_ms;
    let fired = self.timer_wheel.tick(now_ms);
    for spec in fired {
      let env = Envelope::signal(
        spec.owner,
        Signal::TimerFired(spec.id),
      );
      let owner = spec.owner;
      if let Some(proc) = self.processes.get_mut(&owner) {
        proc.mailbox.push(env);
        if proc.state == ProcessRuntimeState::WaitingMessage {
          proc.state = ProcessRuntimeState::Ready;
          self.ready_queue.push(owner);
        }
      }
    }
  }

  /// Current logical clock value (milliseconds).
  pub fn clock_ms(&self) -> u64 {
    self.clock_ms
  }

  /// Insert a pre-built process entry (for recovery).
  pub fn insert_process(
    &mut self,
    mut entry: ProcessEntry,
  ) {
    let pid = entry.pid;
    entry.state = ProcessRuntimeState::Ready;
    self.processes.insert(pid, entry);
    self.ready_queue.push(pid);
  }

  /// Schedule a timer directly (for recovery).
  pub fn schedule_timer(
    &mut self,
    spec: crate::core::timer::TimerSpec,
  ) {
    self.timer_wheel.schedule(spec);
  }

  /// Register a name for a process.
  pub fn register_name(
    &mut self,
    name: String,
    pid: Pid,
  ) -> Result<(), String> {
    if !self.processes.contains_key(&pid) {
      return Err(format!(
        "process {} not found",
        pid.raw()
      ));
    }
    self.name_registry.register(name, pid)
  }

  /// Unregister a name.
  pub fn unregister_name(
    &mut self,
    name: &str,
  ) -> Option<Pid> {
    self.name_registry.unregister(name)
  }

  /// Lookup a process by name.
  pub fn whereis(&self, name: &str) -> Option<Pid> {
    self.name_registry.whereis(name)
  }

  /// Send a message to a named process.
  pub fn send_named(
    &mut self,
    name: &str,
    mut env: Envelope,
  ) -> Result<(), String> {
    let pid = self
      .name_registry
      .whereis(name)
      .ok_or_else(|| {
        format!("name '{}' not registered", name)
      })?;
    env.to = pid;
    self.send(env);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::behavior::StepBehavior;
  use crate::core::effect::{EffectKind, EffectRequest, EffectResult};
  use crate::core::message::{Envelope, EnvelopePayload};
  use crate::core::step_result::StepResult;
  use crate::core::turn_context::TurnContext;
  use crate::error::Reason;
  use crate::pid::Pid;

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

  struct Counter {
    count: u32,
  }
  impl StepBehavior for Counter {
    fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
      StepResult::Continue
    }
    fn handle(
      &mut self,
      _msg: Envelope,
      _ctx: &mut TurnContext,
    ) -> StepResult {
      self.count += 1;
      StepResult::Continue
    }
    fn terminate(&mut self, _reason: &Reason) {}
  }

  struct Suspender;
  impl StepBehavior for Suspender {
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
          serde_json::json!({"prompt": "hi"}),
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

  #[test]
  fn test_scheduler_spawn() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Echo));
    assert_eq!(engine.process_count(), 1);
    assert!(engine.has_process(pid));
  }

  #[test]
  fn test_scheduler_send_and_tick() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Echo));
    engine.send(Envelope::text(pid, "hello"));
    let stepped = engine.tick();
    assert_eq!(stepped, 1);
  }

  #[test]
  fn test_scheduler_idle_tick() {
    let mut engine = SchedulerEngine::new();
    engine.spawn(Box::new(Echo)); // No messages
    let stepped = engine.tick();
    assert_eq!(stepped, 1);
  }

  #[test]
  fn test_scheduler_process_done() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Stopper));
    engine.send(Envelope::text(pid, "stop"));
    engine.tick();
    let completed = engine.take_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].0, pid);
    assert!(matches!(completed[0].1, Reason::Normal));
  }

  #[test]
  fn test_scheduler_suspend_and_resume() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Suspender));
    engine.send(Envelope::text(pid, "go"));
    engine.tick();

    // Should have one outbound effect
    let effects = engine.take_outbound_effects();
    assert_eq!(effects.len(), 1);
    let (effect_pid, req) = &effects[0];
    assert_eq!(*effect_pid, pid);
    assert_eq!(req.kind, EffectKind::LlmCall);

    // Process should be WaitingEffect
    assert!(matches!(
      engine.process_state(pid),
      Some(ProcessRuntimeState::WaitingEffect(_))
    ));

    // Deliver effect result
    engine.deliver_effect_result(EffectResult::success(
      req.effect_id,
      serde_json::json!("response"),
    ));
    engine.tick();

    // Process should be done
    let completed = engine.take_completed();
    assert_eq!(completed.len(), 1);
  }

  #[test]
  fn test_scheduler_kill() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Echo));
    engine.kill(pid);
    engine.tick();
    let completed = engine.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(completed[0].1, Reason::Kill));
  }

  #[test]
  fn test_scheduler_message_forwarding() {
    let mut engine = SchedulerEngine::new();
    let receiver = engine.spawn(Box::new(Echo));
    let sender =
      engine.spawn(Box::new(Forwarder { target: receiver }));
    engine.send(Envelope::text(sender, "trigger"));
    engine.tick(); // sender processes, emits intent
    // After tick, outbound messages are delivered internally
    // receiver should now have the "forwarded" message
    // Tick again to let receiver process it
    engine.tick();
  }

  #[test]
  fn test_scheduler_preemption() {
    let mut engine =
      SchedulerEngine::new().with_max_reductions(3);
    let pid = engine.spawn(Box::new(Counter { count: 0 }));
    // Send 10 messages
    for i in 0..10 {
      engine.send(Envelope::text(pid, format!("m-{i}")));
    }
    // First tick: should process max 3 then preempt
    engine.tick();
    // Process should be re-enqueued
    assert!(engine.has_process(pid));
    // Tick again to process more
    engine.tick();
    engine.tick();
    engine.tick();
  }

  #[test]
  fn test_scheduler_reap_completed() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Stopper));
    engine.send(Envelope::text(pid, "stop"));
    engine.tick();
    let reaped = engine.reap_completed();
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0], pid);
    assert!(!engine.has_process(pid));
  }

  #[test]
  fn test_scheduler_many_processes() {
    let mut engine = SchedulerEngine::new();
    let mut pids = Vec::new();
    for _ in 0..100 {
      let pid = engine.spawn(Box::new(Echo));
      pids.push(pid);
    }
    assert_eq!(engine.process_count(), 100);

    // Send one message to each
    for pid in &pids {
      engine.send(Envelope::text(*pid, "hello"));
    }
    engine.tick();
    assert_eq!(engine.process_count(), 100);
  }

  #[test]
  fn test_scheduler_timer_schedule_and_fire() {
    use crate::core::message::Signal;

    let mut engine = SchedulerEngine::new();

    // On any user message, schedules a timer.
    // On TimerFired, exits normally.
    struct TimerScheduler;
    impl StepBehavior for TimerScheduler {
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
            use crate::core::timer::{
              TimerKind, TimerSpec,
            };
            let spec = TimerSpec::new(
              ctx.pid,
              TimerKind::Timeout,
              1000,
            );
            ctx.schedule_timer(spec);
            StepResult::Continue
          }
        }
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let pid = engine.spawn(Box::new(TimerScheduler));
    engine.send(Envelope::text(pid, "schedule"));
    engine.tick(); // Handles msg, emits ScheduleTimer

    // Timer not fired yet
    assert!(engine.take_completed().is_empty());

    // Advance clock past deadline
    engine.advance_clock(1001);
    engine.tick(); // Handles TimerFired signal

    let completed = engine.take_completed();
    assert_eq!(completed.len(), 1);
    assert!(matches!(completed[0].1, Reason::Normal));
  }

  #[test]
  fn test_scheduler_timer_cancel() {
    use crate::core::message::{Payload, Signal};

    let mut engine = SchedulerEngine::new();

    struct TimerCanceller {
      timer_id: Option<crate::core::timer::TimerId>,
    }
    impl StepBehavior for TimerCanceller {
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
          ) => {
            // Should NOT reach here if cancelled
            StepResult::Fail(Reason::Custom(
              "unexpected timer".into(),
            ))
          }
          EnvelopePayload::User(
            Payload::Text(text),
          ) if text == "cancel" => {
            if let Some(id) = self.timer_id {
              ctx.cancel_timer(id);
            }
            StepResult::Continue
          }
          _ => {
            use crate::core::timer::{
              TimerKind, TimerSpec,
            };
            let spec = TimerSpec::new(
              ctx.pid,
              TimerKind::Timeout,
              1000,
            );
            self.timer_id = Some(spec.id);
            ctx.schedule_timer(spec);
            StepResult::Continue
          }
        }
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let pid = engine
      .spawn(Box::new(TimerCanceller { timer_id: None }));
    engine.send(Envelope::text(pid, "schedule"));
    engine.tick(); // Schedules timer

    engine.send(Envelope::text(pid, "cancel"));
    engine.tick(); // Cancels timer

    engine.advance_clock(2000); // Past deadline
    engine.tick(); // No TimerFired should arrive

    // Process should NOT have failed
    let completed = engine.take_completed();
    assert!(completed.is_empty());
  }

  #[test]
  fn test_scheduler_link_exit_propagation() {
    let mut engine = SchedulerEngine::new();
    let a = engine.spawn(Box::new(Stopper));
    let b = engine.spawn(Box::new(Echo));
    engine.link(a, b);
    engine.send(Envelope::text(a, "stop"));
    engine.tick();

    // a completed
    let completed = engine.take_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].0, a);

    // b should have received ExitLinked signal and been
    // woken
    engine.tick();
    // b should still be alive (normal exit, non-trapping
    // => Continue)
    assert!(engine.has_process(b));
  }

  #[test]
  fn test_scheduler_parent_child_exit_notification() {
    let mut engine = SchedulerEngine::new();
    let parent = engine.spawn(Box::new(Echo));

    // Spawn child with parent
    let child = engine.spawn_with_parent(
      Box::new(Stopper),
      parent,
    );
    engine.send(Envelope::text(child, "stop"));
    engine.tick();

    // Child completed
    let completed = engine.take_completed();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].0, child);

    // Parent should have ChildExited in mailbox
    engine.tick();
    assert!(engine.has_process(parent));
  }

  #[test]
  fn test_scheduler_take_outbound_messages() {
    let mut engine = SchedulerEngine::new();
    let receiver = engine.spawn(Box::new(Echo));
    let sender =
      engine.spawn(Box::new(Forwarder { target: receiver }));
    engine.send(Envelope::text(sender, "trigger"));
    engine.tick();
    let messages = engine.take_outbound_messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].to, receiver);
  }

  #[test]
  fn test_scheduler_snapshot_for() {
    struct Snapshotted;
    impl StepBehavior for Snapshotted {
      fn init(
        &mut self,
        _: Option<Vec<u8>>,
      ) -> StepResult {
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
      fn snapshot(&self) -> Option<Vec<u8>> {
        Some(b"my-state".to_vec())
      }
    }

    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Snapshotted));
    let snap = engine.snapshot_for(pid);
    assert_eq!(snap, Some(b"my-state".to_vec()));

    // Non-existent pid returns None
    let missing = Pid::from_raw(99999);
    assert_eq!(engine.snapshot_for(missing), None);
  }

  #[test]
  fn test_scheduler_patch_state_captured() {
    struct StatePatcher;
    impl StepBehavior for StatePatcher {
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
        ctx.set_state(b"new-state".to_vec());
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(StatePatcher));
    engine.send(Envelope::text(pid, "patch"));
    engine.tick();
    let patches = engine.take_state_patches();
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, pid);
    assert_eq!(patches[0].1, b"new-state");
  }

  #[test]
  fn test_scheduler_spawn_process_intent() {
    let mut engine = SchedulerEngine::new();

    struct Spawner;
    impl StepBehavior for Spawner {
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
        ctx.spawn(Box::new(Echo));
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let parent =
      engine.spawn(Box::new(Spawner));
    engine
      .send(Envelope::text(parent, "spawn-child"));
    engine.tick();

    let spawns = engine.take_spawn_requests();
    assert_eq!(spawns.len(), 1);
  }

  #[test]
  fn test_deliver_streaming_chunk_keeps_pending() {
    use crate::core::effect::{
      EffectResult, EffectStatus,
    };

    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Suspender));
    engine.send(Envelope::text(pid, "go"));
    engine.tick(); // Process suspends

    // Take the outbound effect to get the effect_id
    let effects = engine.take_outbound_effects();
    assert_eq!(effects.len(), 1);
    let effect_id = effects[0].1.effect_id;

    // Process should be WaitingEffect
    assert!(matches!(
      engine.process_state(pid),
      Some(ProcessRuntimeState::WaitingEffect(_))
    ));

    // Deliver a streaming chunk
    let chunk = EffectResult {
      effect_id,
      status: EffectStatus::Streaming,
      output: Some(serde_json::json!({
        "delta": "hello"
      })),
      error: None,
    };
    engine.deliver_streaming_chunk(chunk);

    // Effect should still be pending (not removed).
    // Process should still be WaitingEffect (not woken).
    assert!(matches!(
      engine.process_state(pid),
      Some(ProcessRuntimeState::WaitingEffect(_))
    ));

    // Deliver final result -- this should remove from
    // pending and wake the process.
    let final_result = EffectResult::success(
      effect_id,
      serde_json::json!({"content": "hello world"}),
    );
    engine.deliver_effect_result(final_result);

    // Process should now be Ready (woken up)
    assert!(matches!(
      engine.process_state(pid),
      Some(ProcessRuntimeState::Ready)
    ));

    // Tick to let it handle the effect result
    engine.tick();

    // Suspender exits on effect result
    let completed = engine.take_completed();
    assert_eq!(completed.len(), 1);
  }

  #[test]
  fn test_scheduler_register_and_whereis() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Echo));
    engine
      .register_name("worker".into(), pid)
      .unwrap();
    assert_eq!(engine.whereis("worker"), Some(pid));
  }

  #[test]
  fn test_scheduler_send_named() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Echo));
    engine
      .register_name("worker".into(), pid)
      .unwrap();
    let result = engine.send_named(
      "worker",
      Envelope::text(pid, "hello"),
    );
    assert!(result.is_ok());
    engine.tick();
  }

  #[test]
  fn test_scheduler_send_named_unknown_fails() {
    let mut engine = SchedulerEngine::new();
    let pid = Pid::from_raw(1);
    let result = engine.send_named(
      "nope",
      Envelope::text(pid, "hello"),
    );
    assert!(result.is_err());
  }

  #[test]
  fn test_scheduler_auto_unregister_on_exit() {
    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Stopper));
    engine
      .register_name("worker".into(), pid)
      .unwrap();
    assert_eq!(
      engine.whereis("worker"),
      Some(pid)
    );

    engine.send(Envelope::text(pid, "stop"));
    engine.tick();

    // After process exits, name should be
    // auto-unregistered
    assert_eq!(engine.whereis("worker"), None);
  }

  #[test]
  fn test_scheduler_register_dead_process_fails() {
    let mut engine = SchedulerEngine::new();
    let pid = Pid::from_raw(99999);
    let result =
      engine.register_name("ghost".into(), pid);
    assert!(result.is_err());
  }

  #[test]
  fn test_scheduler_register_name_via_intent() {
    struct Registrar;
    impl StepBehavior for Registrar {
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
        ctx.register_name("my_worker".into());
        StepResult::Continue
      }
      fn terminate(&mut self, _: &Reason) {}
    }

    let mut engine = SchedulerEngine::new();
    let pid = engine.spawn(Box::new(Registrar));

    // Before handling, name should not exist
    assert_eq!(engine.whereis("my_worker"), None);

    engine.send(Envelope::text(pid, "register"));
    engine.tick();

    // After tick, the RegisterName intent should
    // have been processed
    assert_eq!(
      engine.whereis("my_worker"),
      Some(pid)
    );
  }
}
