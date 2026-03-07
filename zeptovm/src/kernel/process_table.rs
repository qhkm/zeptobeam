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
  /// Tag filter for selective receive. When Some, step() uses
  /// pop_matching() instead of pop(). Cleared on match.
  pub selective_tag: Option<String>,
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
      selective_tag: None,
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
  pub fn step(&mut self, now_ms: u64) -> (StepResult, TurnContext) {
    let mut ctx = TurnContext::new(self.pid);
    self.turn_overrun_flag = false;
    self.last_turn_duration = None;

    // Kill check (highest priority, like BEAM)
    if self.kill_requested {
      self.state = ProcessRuntimeState::Done;
      self.selective_tag = None;
      return (StepResult::Done(Reason::Kill), ctx);
    }

    // Phase 1: ALWAYS drain control lane first.
    // Signals must never be blocked by tag filtering.
    //
    // IMPORTANT: Do NOT clear selective_tag here. Only terminal
    // paths (Kill, Shutdown, abnormal ExitLinked, panic) clear it.
    // Non-terminal signals (TimerFired, MonitorDown, ChildExited,
    // trapping ExitLinked, Resume) are dispatched to the behavior,
    // which may return Continue or WaitForTag — the tag filter
    // must survive across these.
    if let Some(env) = self.mailbox.pop_control(now_ms) {
      if let EnvelopePayload::Signal(ref sig) = env.payload {
        match sig {
          Signal::Kill => {
            self.state = ProcessRuntimeState::Done;
            self.selective_tag = None; // terminal
            return (StepResult::Done(Reason::Kill), ctx);
          }
          Signal::Shutdown => {
            self.state = ProcessRuntimeState::Done;
            self.selective_tag = None; // terminal
            return (
              StepResult::Done(Reason::Shutdown),
              ctx,
            );
          }
          Signal::Suspend => {
            // Preserve selective_tag — resume will
            // continue the wait
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
            self.selective_tag = None; // terminal
            return (
              StepResult::Done(reason.clone()),
              ctx,
            );
          }
          Signal::ExitLinked(_, _) if !self.trap_exit => {
            // Normal exit from linked, not trapping.
            // Preserve selective_tag.
            return (StepResult::Continue, ctx);
          }
          // Trappable signals (ChildExited, TimerFired,
          // MonitorDown, trapping ExitLinked) fall through
          // to behavior. selective_tag preserved.
          _ => {}
        }
      }

      // Control message not handled by early return
      // (trappable signal or non-signal control) → behavior.
      // selective_tag is preserved — behavior decides whether
      // to continue waiting (WaitForTag) or do something else.
      self.reductions += 1;
      let start = std::time::Instant::now();
      let result =
        panic::catch_unwind(AssertUnwindSafe(|| {
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
          "handler overrun"
        );
      }
      return match result {
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
          self.selective_tag = None; // terminal
          (StepResult::Fail(Reason::Custom(msg)), ctx)
        }
      };
    }

    // Phase 2: No control message. Pop based on selective_tag.
    let maybe_env = if let Some(ref tag) = self.selective_tag {
      self.mailbox.pop_matching(now_ms, tag)
    } else {
      self.mailbox.pop(now_ms)
    };

    match maybe_env {
      Some(env) => {
        self.state = ProcessRuntimeState::Running;
        self.selective_tag = None; // matched, clear filter

        // Check for signals that leaked to non-control lanes
        // (shouldn't happen, but defensive)
        if let EnvelopePayload::Signal(ref sig) = env.payload
        {
          match sig {
            Signal::Kill => {
              self.state = ProcessRuntimeState::Done;
              return (
                StepResult::Done(Reason::Kill),
                ctx,
              );
            }
            Signal::Shutdown => {
              self.state = ProcessRuntimeState::Done;
              return (
                StepResult::Done(Reason::Shutdown),
                ctx,
              );
            }
            _ => {}
          }
        }

        // Dispatch to behavior
        self.reductions += 1;
        let start = std::time::Instant::now();
        let result =
          panic::catch_unwind(AssertUnwindSafe(|| {
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
            "handler overrun"
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
      None => {
        // No message. If selective receive was active,
        // return WaitForTag to preserve the filter.
        // selective_tag is still Some (not cleared above).
        if let Some(tag) = self.selective_tag.take() {
          (StepResult::WaitForTag(tag), ctx)
        } else {
          (StepResult::Wait, ctx)
        }
      }
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
    let (result, _ctx) = p.step(0);
    assert!(matches!(result, StepResult::Continue));
    assert_eq!(p.reductions(), 1);
  }

  #[test]
  fn test_process_step_empty_returns_wait() {
    let mut p = ProcessEntry::new(Pid::new(), Box::new(Echo));
    p.init(None);
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Wait));
  }

  #[test]
  fn test_process_kill() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.init(None);
    p.mailbox.push(Envelope::text(pid, "ignored"));
    p.request_kill();
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Done(Reason::Kill)));
  }

  #[test]
  fn test_process_signal_kill() {
    let pid = Pid::new();
    let mut p = ProcessEntry::new(pid, Box::new(Echo));
    p.init(None);
    p.mailbox.push(Envelope::signal(pid, Signal::Kill));
    let (result, _) = p.step(0);
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
    let (result, _) = p.step(0);
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
    let (result, _) = p.step(0);
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
    let (result, _) = p.step(0);
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
    let (result, _) = p.step(0);
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
    let (result, ctx) = p.step(0);
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
      p.step(0);
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
    let (result, _) = p.step(0);
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
    let (result, _) = p.step(0);
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
    let (result, _) = p.step(0);
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
    p.step(0);
    assert!(p.turn_overrun());
    // Second step: no message, should return Wait and
    // reset overrun flag
    let (result, _) = p.step(0);
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

  // ── Selective receive tests ──────────────────────────

  use crate::core::message::{Payload, EnvelopePayload};

  #[test]
  fn test_selective_receive_filters_by_tag() {
    struct SelectiveBehavior;
    impl StepBehavior for SelectiveBehavior {
      fn handle(
        &mut self,
        envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        match &envelope.payload {
          EnvelopePayload::User(Payload::Text(s))
            if s == "start" =>
          {
            StepResult::WaitForTag("reply".into())
          }
          EnvelopePayload::User(Payload::Text(s))
            if s == "the reply" =>
          {
            StepResult::Done(Reason::Normal)
          }
          _ => StepResult::Continue,
        }
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(
      pid,
      Box::new(SelectiveBehavior),
    );

    // Process "start", get WaitForTag
    p.mailbox.push(Envelope::text(pid, "start"));
    let (result, _) = p.step(0);
    assert!(matches!(
      result,
      StepResult::WaitForTag(ref t) if t == "reply"
    ));

    // Simulate scheduler: set selective_tag + WaitingMessage
    p.selective_tag = Some("reply".into());
    p.mailbox.push(Envelope::text(pid, "noise"));
    p.mailbox.push(
      Envelope::text(pid, "wrong").with_tag("other"),
    );

    // No match → WaitForTag again
    let (result, _) = p.step(0);
    assert!(matches!(
      result,
      StepResult::WaitForTag(ref t) if t == "reply"
    ));
    assert_eq!(p.mailbox.total_len(), 2);

    // Send matching message
    p.selective_tag = Some("reply".into());
    p.mailbox.push(
      Envelope::text(pid, "the reply").with_tag("reply"),
    );
    let (result, _) = p.step(0);
    assert!(matches!(
      result,
      StepResult::Done(Reason::Normal)
    ));
    assert!(p.selective_tag.is_none()); // cleared on match
    assert_eq!(p.mailbox.total_len(), 2); // noise + wrong
  }

  #[test]
  fn test_selective_receive_no_match_returns_wait_for_tag() {
    struct TagWaiter;
    impl StepBehavior for TagWaiter {
      fn handle(
        &mut self,
        _envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        StepResult::WaitForTag("expected".into())
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p =
      ProcessEntry::new(pid, Box::new(TagWaiter));

    // Trigger WaitForTag
    p.mailbox.push(Envelope::text(pid, "go"));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(_)));

    // Set selective_tag, step with empty mailbox
    p.selective_tag = Some("expected".into());
    let (result, _) = p.step(0);
    match result {
      StepResult::WaitForTag(tag) => {
        assert_eq!(tag, "expected")
      }
      StepResult::Wait => {
        panic!("BUG: downgraded WaitForTag to Wait")
      }
      other => panic!("unexpected: {:?}", other),
    }
  }

  #[test]
  fn test_selective_receive_kill_bypasses_tag_filter() {
    struct NeverDone;
    impl StepBehavior for NeverDone {
      fn handle(
        &mut self,
        _envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        StepResult::WaitForTag("reply".into())
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p =
      ProcessEntry::new(pid, Box::new(NeverDone));

    // Enter selective mode
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.selective_tag = Some("reply".into());

    // Send Kill (control lane, untagged)
    p.mailbox
      .push(Envelope::signal(pid, Signal::Kill));

    let (result, _) = p.step(0);
    assert!(
      matches!(result, StepResult::Done(Reason::Kill)),
      "Kill must bypass tag filter"
    );
    assert_eq!(p.state, ProcessRuntimeState::Done);
    assert!(p.selective_tag.is_none());
  }

  #[test]
  fn test_selective_receive_shutdown_bypasses_tag_filter() {
    struct TagWaiter;
    impl StepBehavior for TagWaiter {
      fn handle(
        &mut self,
        _envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        StepResult::WaitForTag("reply".into())
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p =
      ProcessEntry::new(pid, Box::new(TagWaiter));
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.selective_tag = Some("reply".into());

    p.mailbox
      .push(Envelope::signal(pid, Signal::Shutdown));
    let (result, _) = p.step(0);
    assert!(matches!(
      result,
      StepResult::Done(Reason::Shutdown)
    ));
  }

  #[test]
  fn test_selective_receive_exit_linked_bypasses_tag_filter()
  {
    struct TagWaiter;
    impl StepBehavior for TagWaiter {
      fn handle(
        &mut self,
        _envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        StepResult::WaitForTag("reply".into())
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p =
      ProcessEntry::new(pid, Box::new(TagWaiter));
    p.mailbox.push(Envelope::text(pid, "trigger"));
    let _ = p.step(0);
    p.selective_tag = Some("reply".into());

    p.mailbox.push(Envelope::signal(
      pid,
      Signal::ExitLinked(
        Pid::from_raw(2),
        Reason::Custom("crashed".into()),
      ),
    ));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::Done(_)));
  }

  #[test]
  fn test_selective_receive_no_queue_duplication() {
    struct CountingBehavior {
      handle_count: u32,
    }
    impl StepBehavior for CountingBehavior {
      fn handle(
        &mut self,
        _envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        self.handle_count += 1;
        StepResult::WaitForTag("target".into())
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(
      pid,
      Box::new(CountingBehavior { handle_count: 0 }),
    );

    // Trigger selective mode
    p.mailbox.push(Envelope::text(pid, "go"));
    let _ = p.step(0);

    // Simulate scheduler setting selective_tag +
    // WaitingMessage
    p.selective_tag = Some("target".into());
    p.state = ProcessRuntimeState::WaitingMessage;

    // Deliver 5 non-matching messages
    for i in 0..5 {
      p.mailbox.push(Envelope::text(
        pid,
        format!("noise-{i}"),
      ));
    }

    // Simulate the wake: WaitingMessage → Ready (once)
    assert_eq!(
      p.state,
      ProcessRuntimeState::WaitingMessage
    );
    p.state = ProcessRuntimeState::Ready;

    // Step once: pop_matching finds no match, returns
    // WaitForTag
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(_)));

    // All 5 noise messages still in mailbox (not consumed)
    assert_eq!(p.mailbox.total_len(), 5);
  }

  #[test]
  fn test_selective_receive_timer_fired_preserves_tag() {
    use crate::core::timer::TimerId;

    struct TimerAwareBehavior {
      timer_seen: bool,
    }
    impl StepBehavior for TimerAwareBehavior {
      fn handle(
        &mut self,
        envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        match &envelope.payload {
          EnvelopePayload::User(Payload::Text(s))
            if s == "start" =>
          {
            StepResult::WaitForTag("reply".into())
          }
          EnvelopePayload::Signal(
            Signal::TimerFired(_),
          ) => {
            self.timer_seen = true;
            StepResult::WaitForTag("reply".into())
          }
          EnvelopePayload::User(Payload::Text(s))
            if s == "the reply" =>
          {
            StepResult::Done(Reason::Normal)
          }
          _ => StepResult::Continue,
        }
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(
      pid,
      Box::new(TimerAwareBehavior {
        timer_seen: false,
      }),
    );

    // Enter selective mode
    p.mailbox.push(Envelope::text(pid, "start"));
    let (result, _) = p.step(0);
    assert!(matches!(result, StepResult::WaitForTag(_)));
    p.selective_tag = Some("reply".into());

    // Deliver TimerFired (control lane, non-terminal)
    p.mailbox.push(Envelope::signal(
      pid,
      Signal::TimerFired(TimerId::new()),
    ));

    let (result, _) = p.step(0);
    assert!(
      matches!(
        result,
        StepResult::WaitForTag(ref t) if t == "reply"
      ),
      "TimerFired must not cancel selective receive"
    );
  }

  #[test]
  fn test_selective_receive_monitor_down_preserves_tag() {
    struct MonitorAwareBehavior;
    impl StepBehavior for MonitorAwareBehavior {
      fn handle(
        &mut self,
        envelope: Envelope,
        _ctx: &mut TurnContext,
      ) -> StepResult {
        match &envelope.payload {
          EnvelopePayload::User(Payload::Text(s))
            if s == "start" =>
          {
            StepResult::WaitForTag("reply".into())
          }
          EnvelopePayload::Signal(
            Signal::MonitorDown(_, _),
          ) => {
            StepResult::WaitForTag("reply".into())
          }
          _ => StepResult::Continue,
        }
      }
      fn init(
        &mut self,
        _checkpoint: Option<Vec<u8>>,
      ) -> StepResult {
        StepResult::Continue
      }
      fn terminate(&mut self, _reason: &Reason) {}
    }

    let pid = Pid::from_raw(1);
    let mut p = ProcessEntry::new(
      pid,
      Box::new(MonitorAwareBehavior),
    );

    // Enter selective mode
    p.mailbox.push(Envelope::text(pid, "start"));
    let _ = p.step(0);
    p.selective_tag = Some("reply".into());

    // Deliver MonitorDown (control lane, non-terminal)
    p.mailbox.push(Envelope::signal(
      pid,
      Signal::MonitorDown(
        Pid::from_raw(2),
        Reason::Normal,
      ),
    ));

    let (result, _) = p.step(0);
    assert!(
      matches!(
        result,
        StepResult::WaitForTag(ref t) if t == "reply"
      ),
      "MonitorDown must not cancel selective receive"
    );
  }
}
