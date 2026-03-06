use std::collections::HashMap;

use crate::{
  core::{
    effect::{EffectRequest, EffectResult},
    message::Envelope,
    step_result::StepResult,
    turn_context::TurnIntent,
  },
  error::Reason,
  kernel::process_table::{ProcessEntry, ProcessRuntimeState},
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
  max_reductions: u32,
  completed: Vec<(Pid, Reason)>,
}

impl SchedulerEngine {
  pub fn new() -> Self {
    Self {
      processes: HashMap::new(),
      ready_queue: Vec::new(),
      pending_effects: HashMap::new(),
      outbound_effects: Vec::new(),
      outbound_messages: Vec::new(),
      max_reductions: 200,
      completed: Vec::new(),
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
  pub fn deliver_effect_result(&mut self, result: EffectResult) {
    let effect_raw = result.effect_id.raw();
    if let Some(pending) = self.pending_effects.remove(&effect_raw)
    {
      let pid = pending.pid;
      if let Some(proc) = self.processes.get_mut(&pid) {
        proc.mailbox.push(Envelope::effect_result(pid, result));
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

  /// Run one tick: step all ready processes up to max_reductions
  /// each. Returns: number of processes stepped.
  pub fn tick(&mut self) -> usize {
    let ready: Vec<Pid> = self.ready_queue.drain(..).collect();
    let mut stepped = 0;

    for pid in ready {
      if !self.processes.contains_key(&pid) {
        continue;
      }

      stepped += 1;
      self.step_process(pid);
    }

    // Deliver outbound messages within the same engine
    let messages: Vec<Envelope> =
      self.outbound_messages.drain(..).collect();
    for msg in messages {
      self.send(msg);
    }

    stepped
  }

  fn step_process(&mut self, pid: Pid) {
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
        let (result, mut ctx) = proc.step();
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
          TurnIntent::PatchState(_data) => {
            // Phase 2: persist state patch
          }
          TurnIntent::ScheduleTimer(_spec) => {
            // Phase 2: wire into timer wheel
          }
          TurnIntent::CancelTimer(_id) => {
            // Phase 2: wire into timer wheel
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
        StepResult::Done(reason) => {
          if let Some(proc) = self.processes.get_mut(&pid) {
            proc.terminate(&reason);
          }
          self.completed.push((pid, reason));
          return;
        }
        StepResult::Fail(reason) => {
          if let Some(proc) = self.processes.get_mut(&pid) {
            proc.terminate(&reason);
          }
          self.completed.push((pid, reason));
          return;
        }
      }
    }
  }

  /// Take outbound effect requests (for the reactor/dispatcher).
  pub fn take_outbound_effects(
    &mut self,
  ) -> Vec<(Pid, EffectRequest)> {
    std::mem::take(&mut self.outbound_effects)
  }

  /// Take completed processes (for supervisor notification).
  pub fn take_completed(&mut self) -> Vec<(Pid, Reason)> {
    std::mem::take(&mut self.completed)
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
}
