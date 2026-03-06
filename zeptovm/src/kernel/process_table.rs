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
  max_turn_wall_clock: std::time::Duration,
  turn_overrun_flag: bool,
  last_turn_duration: Option<std::time::Duration>,
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
      max_turn_wall_clock: std::time::Duration::from_secs(5),
      turn_overrun_flag: false,
      last_turn_duration: None,
    }
  }

  /// Initialize the process.
  pub fn init(&mut self, checkpoint: Option<Vec<u8>>) -> StepResult {
    self.behavior.init(checkpoint)
  }

  pub fn set_trap_exit(&mut self, trap: bool) {
    self.trap_exit = trap;
  }

  pub fn set_max_turn_wall_clock(
    &mut self,
    d: std::time::Duration,
  ) {
    self.max_turn_wall_clock = d;
  }

  pub fn turn_overrun(&self) -> bool {
    self.turn_overrun_flag
  }

  pub fn last_turn_duration(
    &self,
  ) -> Option<std::time::Duration> {
    self.last_turn_duration
  }

  /// Step the process once with panic isolation.
  /// Returns (StepResult, TurnContext with collected intents).
  pub fn step(&mut self) -> (StepResult, TurnContext) {
    let mut ctx = TurnContext::new(self.pid);
    self.turn_overrun_flag = false;
    self.last_turn_duration = None;

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
        let start = std::time::Instant::now();
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
          self.behavior.handle(env, &mut ctx)
        }));
        let elapsed = start.elapsed();
        self.last_turn_duration = Some(elapsed);

        if elapsed > self.max_turn_wall_clock {
          self.turn_overrun_flag = true;
          tracing::warn!(
            pid = self.pid.raw(),
            elapsed_ms = elapsed.as_millis() as u64,
            limit_ms =
              self.max_turn_wall_clock.as_millis() as u64,
            "handler overrun: turn exceeded wall-clock limit"
          );
        }

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

  /// Get behavior metadata if the behavior provides it.
  pub fn behavior_meta(
    &self,
  ) -> Option<crate::core::behavior::BehaviorMeta> {
    self.behavior.meta()
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

  #[test]
  fn test_watchdog_slow_handler_sets_overrun() {
    struct SlowHandler;
    impl StepBehavior for SlowHandler {
      fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _msg: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        std::thread::sleep(
          std::time::Duration::from_millis(20),
        );
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::new();
    let mut p =
      ProcessEntry::new(pid, Box::new(SlowHandler));
    p.set_max_turn_wall_clock(
      std::time::Duration::from_millis(5),
    );
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Continue));
    assert!(
      p.turn_overrun(),
      "expected overrun flag to be set"
    );
  }

  #[test]
  fn test_watchdog_fast_handler_no_overrun() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.set_max_turn_wall_clock(
      std::time::Duration::from_secs(5),
    );
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Continue));
    assert!(
      !p.turn_overrun(),
      "expected no overrun for fast handler"
    );
  }

  #[test]
  fn test_watchdog_overrun_resets_each_step() {
    struct SlowHandler;
    impl StepBehavior for SlowHandler {
      fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn handle(
        &mut self,
        _msg: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        std::thread::sleep(
          std::time::Duration::from_millis(20),
        );
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::new();
    let mut p =
      ProcessEntry::new(pid, Box::new(SlowHandler));
    p.set_max_turn_wall_clock(
      std::time::Duration::from_millis(5),
    );
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "go"));
    p.step();
    assert!(p.turn_overrun());
    // Second step: no message, should return Wait and
    // reset overrun flag
    let (result, _) = p.step();
    assert!(matches!(result, StepResult::Wait));
    assert!(!p.turn_overrun());
  }

  #[test]
  fn test_process_entry_stores_behavior_meta() {
    use crate::core::behavior::BehaviorMeta;

    struct MetaBehavior;
    impl StepBehavior for MetaBehavior {
      fn init(
        &mut self,
        _cp: Option<Vec<u8>>,
      ) -> StepResult {
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
      fn meta(
        &self,
      ) -> Option<BehaviorMeta> {
        Some(BehaviorMeta {
          module: "test_agent".to_string(),
          version: "0.1.0".to_string(),
        })
      }
    }

    let pid = Pid::new();
    let p =
      ProcessEntry::new(pid, Box::new(MetaBehavior));
    let meta = p.behavior_meta();
    assert!(meta.is_some());
    let meta = meta.unwrap();
    assert_eq!(meta.module, "test_agent");
    assert_eq!(meta.version, "0.1.0");
  }

  #[test]
  fn test_process_entry_no_meta_default() {
    let pid = Pid::new();
    let p = ProcessEntry::new(pid, Box::new(Echo));
    assert!(p.behavior_meta().is_none());
  }
}
