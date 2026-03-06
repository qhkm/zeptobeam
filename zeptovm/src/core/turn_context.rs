use crate::core::effect::EffectRequest;
use crate::core::message::Envelope;
use crate::pid::Pid;

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TURN_ID: AtomicU64 = AtomicU64::new(1);

pub type TurnId = u64;

pub fn next_turn_id() -> TurnId {
  NEXT_TURN_ID.fetch_add(1, Ordering::Relaxed)
}

/// An intent emitted by a behavior handler during a turn.
/// Collected by TurnContext, committed atomically by the turn
/// executor.
#[derive(Debug)]
pub enum TurnIntent {
  SendMessage(Envelope),
  RequestEffect(EffectRequest),
  PatchState(Vec<u8>),
  ScheduleTimer(crate::core::timer::TimerSpec),
  CancelTimer(crate::core::timer::TimerId),
  // Phase 2: SpawnProcess, Link, Monitor, DebitBudget
}

/// Context passed to behavior.handle(). Handlers emit intents
/// through this.
pub struct TurnContext {
  pub pid: Pid,
  pub turn_id: TurnId,
  intents: Vec<TurnIntent>,
}

impl TurnContext {
  pub fn new(pid: Pid) -> Self {
    Self {
      pid,
      turn_id: next_turn_id(),
      intents: Vec::new(),
    }
  }

  /// Emit a message to another process.
  pub fn send(&mut self, msg: Envelope) {
    self.intents.push(TurnIntent::SendMessage(msg));
  }

  /// Convenience: send a text message to a pid.
  pub fn send_text(&mut self, to: Pid, text: impl Into<String>) {
    self.send(Envelope::text(to, text));
  }

  /// Request an effect (LLM call, HTTP, etc.).
  pub fn request_effect(&mut self, req: EffectRequest) {
    self.intents.push(TurnIntent::RequestEffect(req));
  }

  /// Patch process state (serialized blob).
  pub fn set_state(&mut self, state: Vec<u8>) {
    self.intents.push(TurnIntent::PatchState(state));
  }

  /// Schedule a timer (fires as a signal later).
  pub fn schedule_timer(
    &mut self,
    spec: crate::core::timer::TimerSpec,
  ) {
    self.intents.push(TurnIntent::ScheduleTimer(spec));
  }

  /// Cancel a previously scheduled timer.
  pub fn cancel_timer(
    &mut self,
    id: crate::core::timer::TimerId,
  ) {
    self.intents.push(TurnIntent::CancelTimer(id));
  }

  /// Take all collected intents (consumed by turn executor).
  pub fn take_intents(&mut self) -> Vec<TurnIntent> {
    std::mem::take(&mut self.intents)
  }

  /// How many intents have been emitted.
  pub fn intent_count(&self) -> usize {
    self.intents.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::effect::{EffectKind, EffectRequest};
  use crate::pid::Pid;

  #[test]
  fn test_turn_context_send() {
    let pid = Pid::from_raw(1);
    let mut ctx = TurnContext::new(pid);
    ctx.send_text(Pid::from_raw(2), "hello");
    assert_eq!(ctx.intent_count(), 1);
  }

  #[test]
  fn test_turn_context_request_effect() {
    let pid = Pid::from_raw(1);
    let mut ctx = TurnContext::new(pid);
    let req =
      EffectRequest::new(EffectKind::LlmCall, serde_json::json!({}));
    ctx.request_effect(req);
    assert_eq!(ctx.intent_count(), 1);
  }

  #[test]
  fn test_turn_context_take_intents() {
    let pid = Pid::from_raw(1);
    let mut ctx = TurnContext::new(pid);
    ctx.send_text(Pid::from_raw(2), "a");
    ctx.send_text(Pid::from_raw(3), "b");
    let intents = ctx.take_intents();
    assert_eq!(intents.len(), 2);
    assert_eq!(ctx.intent_count(), 0);
  }

  #[test]
  fn test_turn_id_unique() {
    let a = next_turn_id();
    let b = next_turn_id();
    assert_ne!(a, b);
  }
}
