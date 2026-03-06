use std::panic::{self, AssertUnwindSafe};

use crate::{
  core::{
    message::{EnvelopePayload, Signal},
    step_result::StepResult,
    turn_context::TurnContext,
  },
  error::Reason,
  kernel::mailbox::MultiLaneMailbox,
  pid::Pid,
};

use crate::core::behavior::StepBehavior;

/// Full runtime state of a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRuntimeState {
  Ready,
  Running,
  WaitingMessage,
  WaitingEffect(u64), // EffectId raw value (Copy-friendly)
  WaitingTimer,
  Checkpointing,
  Suspended,
  Failed,
  Done,
}

/// A process entry managed by the scheduler.
pub struct ProcessEntry {
  pub pid: Pid,
  pub state: ProcessRuntimeState,
  pub mailbox: MultiLaneMailbox,
  pub parent: Option<Pid>,
  pub trap_exit: bool,
  behavior: Box<dyn StepBehavior>,
  reductions: u32,
  kill_requested: bool,
  exit_reason: Option<Reason>,
}

impl ProcessEntry {
  pub fn new(pid: Pid, behavior: Box<dyn StepBehavior>) -> Self {
    Self {
      pid,
      state: ProcessRuntimeState::Ready,
      mailbox: MultiLaneMailbox::new(),
      parent: None,
      trap_exit: false,
      behavior,
      reductions: 0,
      kill_requested: false,
      exit_reason: None,
    }
  }

  /// Initialize the process.
  pub fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult {
    self.behavior.init(checkpoint)
  }

  pub fn set_trap_exit(&mut self, trap: bool) {
    self.trap_exit = trap;
  }

  /// Step the process once with panic isolation.
  /// Returns (StepResult, TurnContext with collected intents).
  pub fn step(&mut self) -> (StepResult, TurnContext) {
    let mut ctx = TurnContext::new(self.pid);

    // Kill check (highest priority, like BEAM)
    if self.kill_requested {
      self.state = ProcessRuntimeState::Done;
      return (StepResult::Done(Reason::Kill), ctx);
    }

    // Pop next message (mailbox handles lane priority)
    match self.mailbox.pop() {
      Some(env) => {
        // Check for control signals that the runtime handles
        // directly. Signals not matched here fall through to
        // behavior.handle() (e.g. ChildExited, TimerFired,
        // MonitorDown, trapping ExitLinked).
        if let EnvelopePayload::Signal(ref sig) = env.payload {
          match sig {
            Signal::Kill => {
              self.state = ProcessRuntimeState::Done;
              return (StepResult::Done(Reason::Kill), ctx);
            }
            Signal::Shutdown => {
              self.state = ProcessRuntimeState::Done;
              return (StepResult::Done(Reason::Shutdown), ctx);
            }
            Signal::Suspend => {
              self.state = ProcessRuntimeState::Suspended;
              return (StepResult::Wait, ctx);
            }
            Signal::Resume => {
              return (StepResult::Continue, ctx);
            }
            Signal::ExitLinked(_, reason)
              if !self.trap_exit && reason.is_abnormal() =>
            {
              self.state = ProcessRuntimeState::Done;
              return (StepResult::Done(reason.clone()), ctx);
            }
            Signal::ExitLinked(_, _) if !self.trap_exit => {
              // Normal exit from linked process, not trapping
              return (StepResult::Continue, ctx);
            }
            // All other signals (ChildExited, TimerFired,
            // MonitorDown, trapping ExitLinked) fall through
            // to behavior.handle()
            _ => {}
          }
        }

        // User/effect messages and fall-through signals go to
        // behavior.handle() with catch_unwind
        self.reductions += 1;
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
          self.behavior.handle(env, &mut ctx)
        }));

        match result {
          Ok(step) => (step, ctx),
          Err(panic_info) => {
            let msg = if let Some(s) =
              panic_info.downcast_ref::<&str>()
            {
              format!("panic: {s}")
            } else if let Some(s) =
              panic_info.downcast_ref::<String>()
            {
              format!("panic: {s}")
            } else {
              "panic: unknown".to_string()
            };
            self.state = ProcessRuntimeState::Failed;
            (StepResult::Fail(Reason::Custom(msg)), ctx)
          }
        }
      }
      None => (StepResult::Wait, ctx),
    }
  }

  pub fn request_kill(&mut self) {
    self.kill_requested = true;
  }

  pub fn terminate(&mut self, reason: &Reason) {
    self.behavior.terminate(reason);
    self.exit_reason = Some(reason.clone());
    self.state = ProcessRuntimeState::Done;
  }

  pub fn reductions(&self) -> u32 {
    self.reductions
  }

  pub fn take_reductions(&mut self) -> u32 {
    let r = self.reductions;
    self.reductions = 0;
    r
  }

  pub fn exit_reason(&self) -> Option<&Reason> {
    self.exit_reason.as_ref()
  }

  pub fn is_killed(&self) -> bool {
    self.kill_requested
  }

  /// Get a snapshot of behavior state.
  pub fn snapshot(&self) -> Option<Vec<u8>> {
    self.behavior.snapshot()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::behavior::StepBehavior;
  use crate::core::effect::{EffectKind, EffectRequest};
  use crate::core::message::Envelope;
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

  #[test]
  fn test_process_new() {
    let p = ProcessEntry::new(Pid::new(), Box::new(Echo));
    assert_eq!(p.state, ProcessRuntimeState::Ready);
    assert_eq!(p.reductions(), 0);
  }

  #[test]
  fn test_process_step_with_message() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "hello"));
    let (result, _ctx) = p.step();
    assert!(matches!(result, StepResult::Continue));
    assert_eq!(p.reductions(), 1);
  }

  #[test]
  fn test_process_step_empty_returns_wait() {
    let mut p = ProcessEntry::new(Pid::new(), Box::new(Echo));
    p.init(None);
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Wait));
  }

  #[test]
  fn test_process_kill() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "ignored"));
    p.request_kill();
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Done(Reason::Kill)));
  }

  #[test]
  fn test_process_signal_kill() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.init(None);
    p.mailbox.push(Envelope::signal(pid, Signal::Kill));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Done(Reason::Kill)));
  }

  #[test]
  fn test_process_exit_linked_abnormal() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.init(None);
    let other = Pid::new();
    p.mailbox.push(Envelope::signal(
      pid,
      Signal::ExitLinked(other, Reason::Custom("crash".into())),
    ));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Done(Reason::Custom(_))));
  }

  #[test]
  fn test_process_exit_linked_normal_ignored() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.init(None);
    let other = Pid::new();
    p.mailbox.push(Envelope::signal(
      pid,
      Signal::ExitLinked(other, Reason::Normal),
    ));
    p.mailbox.push(Envelope::text(pid, "still alive"));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Continue));
  }

  #[test]
  fn test_process_panic_caught() {
    struct Panicker;
    impl StepBehavior for Panicker {
      fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _msg: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        panic!("boom")
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Panicker));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Fail(Reason::Custom(_))));
    assert_eq!(p.state, ProcessRuntimeState::Failed);
  }

  #[test]
  fn test_process_suspend_effect() {
    struct Suspender;
    impl StepBehavior for Suspender {
      fn init(&mut self, _cp: Option<Vec<u8>>) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _msg: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        StepResult::Suspend(EffectRequest::new(
          EffectKind::LlmCall,
          serde_json::json!({"prompt": "hi"}),
        ))
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Suspender));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Suspend(_)));
  }

  #[test]
  fn test_process_intents_collected() {
    struct Sender {
      target: Pid,
    }
    impl StepBehavior for Sender {
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

    let pid = Pid::new();
    let target = Pid::from_raw(99);
    let mut p =
      ProcessEntry::new(pid, Box::new(Sender { target }));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, ctx) = p.step();
    assert!(matches!(result, StepResult::Continue));
    assert_eq!(ctx.intent_count(), 1);
  }

  #[test]
  fn test_process_reductions_tracking() {
    let pid = Pid::new();
    let mut p =
      ProcessEntry::new(pid, Box::new(Counter { count: 0 }));
    p.init(None);
    for i in 0..5 {
      p.mailbox.push(Envelope::text(pid, format!("m-{i}")));
    }
    for _ in 0..5 {
      p.step();
    }
    assert_eq!(p.reductions(), 5);
    assert_eq!(p.take_reductions(), 5);
    assert_eq!(p.reductions(), 0);
  }

  #[test]
  fn test_process_with_parent() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.parent = Some(Pid::from_raw(100));
    assert_eq!(p.parent, Some(Pid::from_raw(100)));
  }

  #[test]
  fn test_process_trap_exit_converts_signal() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.set_trap_exit(true);
    p.init(None);
    let other = Pid::new();
    p.mailbox.push(Envelope::signal(
      pid,
      Signal::ExitLinked(other, Reason::Custom("crash".into())),
    ));
    let (result, _) = p.step();
    // trap_exit: signal goes to behavior.handle(), which returns
    // Continue
    assert!(matches!(result, StepResult::Continue));
    assert_ne!(p.state, ProcessRuntimeState::Done);
  }
}
