use std::collections::HashMap;

use crate::core::behavior::StepBehavior;
use crate::core::message::{Envelope, EnvelopePayload, Signal};
use crate::core::step_result::StepResult;
use crate::core::supervisor::{
  RestartStrategy, SupervisorSpec,
};
use crate::core::timer::{TimerKind, TimerSpec};
use crate::core::turn_context::TurnContext;
use crate::error::Reason;
use crate::pid::Pid;

struct ChildState {
  id: String,
  pid: Pid,
  restart: RestartStrategy,
  restart_timestamps: Vec<u64>,
  restart_count: u32,
}

pub struct SupervisorBehavior {
  spec: SupervisorSpec,
  children: HashMap<Pid, ChildState>,
  id_to_pid: HashMap<String, Pid>,
  clock_ms: u64,
}

impl SupervisorBehavior {
  pub fn new(spec: SupervisorSpec) -> Self {
    Self {
      spec,
      children: HashMap::new(),
      id_to_pid: HashMap::new(),
      clock_ms: 0,
    }
  }

  pub fn register_child(
    &mut self,
    child_id: String,
    child_pid: Pid,
    restart: RestartStrategy,
  ) {
    self.id_to_pid.insert(child_id.clone(), child_pid);
    self.children.insert(
      child_pid,
      ChildState {
        id: child_id,
        pid: child_pid,
        restart,
        restart_timestamps: Vec::new(),
        restart_count: 0,
      },
    );
  }

  fn intensity_exceeded(
    child: &ChildState,
    max_restarts: u32,
    clock_ms: u64,
    restart_window_ms: u64,
  ) -> bool {
    let cutoff = clock_ms.saturating_sub(restart_window_ms);
    let recent = child
      .restart_timestamps
      .iter()
      .filter(|&&ts| ts >= cutoff)
      .count();
    recent as u32 >= max_restarts
  }

  fn should_restart(
    child: &ChildState,
    reason: &Reason,
  ) -> bool {
    match child.restart {
      RestartStrategy::Permanent => true,
      RestartStrategy::Transient => reason.is_abnormal(),
      RestartStrategy::Temporary => false,
    }
  }

  pub fn set_clock(&mut self, ms: u64) {
    self.clock_ms = ms;
  }

  pub fn child_count(&self) -> usize {
    self.children.len()
  }
}

impl StepBehavior for SupervisorBehavior {
  fn init(
    &mut self,
    _checkpoint: Option<Vec<u8>>,
  ) -> StepResult {
    StepResult::Continue
  }

  fn handle(
    &mut self,
    msg: Envelope,
    ctx: &mut TurnContext,
  ) -> StepResult {
    match &msg.payload {
      EnvelopePayload::Signal(Signal::ChildExited {
        child_pid,
        reason,
      }) => {
        let child_pid = *child_pid;
        let reason = reason.clone();

        let child = match self.children.get_mut(&child_pid)
        {
          Some(c) => c,
          None => return StepResult::Continue,
        };

        if !Self::should_restart(child, &reason) {
          let id = child.id.clone();
          self.children.remove(&child_pid);
          self.id_to_pid.remove(&id);
          return StepResult::Continue;
        }

        child.restart_timestamps.push(self.clock_ms);
        child.restart_count += 1;

        if Self::intensity_exceeded(
          child,
          self.spec.max_restarts,
          self.clock_ms,
          self.spec.restart_window_ms,
        ) {
          return StepResult::Fail(Reason::Shutdown);
        }

        let delay =
          self.spec.backoff.delay_ms(child.restart_count - 1);
        let deadline = self.clock_ms + delay;
        let timer = TimerSpec::new(
          ctx.pid,
          TimerKind::RetryBackoff,
          deadline,
        )
        .with_payload(serde_json::json!({
            "action": "restart_child",
            "child_id": child.id,
            "child_pid": child_pid.raw(),
        }));
        ctx.schedule_timer(timer);

        StepResult::Continue
      }
      _ => StepResult::Continue,
    }
  }

  fn terminate(&mut self, _reason: &Reason) {}

  fn snapshot(&self) -> Option<Vec<u8>> {
    let data: Vec<(String, u64)> = self
      .children
      .values()
      .map(|c| (c.id.clone(), c.pid.raw()))
      .collect();
    serde_json::to_vec(&data).ok()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::supervisor::{
    BackoffPolicy, SupervisorSpec,
  };

  fn make_supervisor(
    max_restarts: u32,
    window_ms: u64,
  ) -> SupervisorBehavior {
    SupervisorBehavior::new(SupervisorSpec {
      max_restarts,
      restart_window_ms: window_ms,
      backoff: BackoffPolicy::Immediate,
    })
  }

  #[test]
  fn test_supervisor_register_child() {
    let mut sup = make_supervisor(3, 5000);
    let child_pid = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      child_pid,
      RestartStrategy::Permanent,
    );
    assert_eq!(sup.child_count(), 1);
  }

  #[test]
  fn test_supervisor_child_exit_triggers_restart() {
    let mut sup = make_supervisor(3, 5000);
    let sup_pid = Pid::from_raw(1);
    let child_pid = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      child_pid,
      RestartStrategy::Permanent,
    );
    sup.init(None);

    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
      sup_pid,
      Signal::ChildExited {
        child_pid,
        reason: Reason::Custom("crash".into()),
      },
    );
    let result = sup.handle(msg, &mut ctx);
    assert!(matches!(result, StepResult::Continue));
    assert_eq!(ctx.intent_count(), 1);
  }

  #[test]
  fn test_supervisor_temporary_not_restarted() {
    let mut sup = make_supervisor(3, 5000);
    let sup_pid = Pid::from_raw(1);
    let child_pid = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      child_pid,
      RestartStrategy::Temporary,
    );
    sup.init(None);

    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
      sup_pid,
      Signal::ChildExited {
        child_pid,
        reason: Reason::Custom("crash".into()),
      },
    );
    sup.handle(msg, &mut ctx);
    assert_eq!(ctx.intent_count(), 0);
    assert_eq!(sup.child_count(), 0);
  }

  #[test]
  fn test_supervisor_transient_normal_exit_no_restart() {
    let mut sup = make_supervisor(3, 5000);
    let sup_pid = Pid::from_raw(1);
    let child_pid = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      child_pid,
      RestartStrategy::Transient,
    );
    sup.init(None);

    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
      sup_pid,
      Signal::ChildExited {
        child_pid,
        reason: Reason::Normal,
      },
    );
    sup.handle(msg, &mut ctx);
    assert_eq!(ctx.intent_count(), 0);
  }

  #[test]
  fn test_supervisor_max_restarts_exceeded() {
    let mut sup = make_supervisor(2, 5000);
    let sup_pid = Pid::from_raw(1);
    let child_pid = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      child_pid,
      RestartStrategy::Permanent,
    );
    sup.init(None);

    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
      sup_pid,
      Signal::ChildExited {
        child_pid,
        reason: Reason::Custom("crash".into()),
      },
    );
    let r = sup.handle(msg, &mut ctx);
    assert!(matches!(r, StepResult::Continue));

    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
      sup_pid,
      Signal::ChildExited {
        child_pid,
        reason: Reason::Custom("crash".into()),
      },
    );
    let r = sup.handle(msg, &mut ctx);
    assert!(matches!(r, StepResult::Fail(Reason::Shutdown)));
  }

  #[test]
  fn test_supervisor_backoff_delay() {
    let mut sup = SupervisorBehavior::new(SupervisorSpec {
      max_restarts: 5,
      restart_window_ms: 10000,
      backoff: BackoffPolicy::Fixed(1000),
    });
    let sup_pid = Pid::from_raw(1);
    let child_pid = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      child_pid,
      RestartStrategy::Permanent,
    );
    sup.set_clock(5000);
    sup.init(None);

    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
      sup_pid,
      Signal::ChildExited {
        child_pid,
        reason: Reason::Custom("crash".into()),
      },
    );
    sup.handle(msg, &mut ctx);
    let intents = ctx.take_intents();
    match &intents[0] {
      crate::core::turn_context::TurnIntent::ScheduleTimer(
        spec,
      ) => {
        assert_eq!(spec.deadline_ms, 6000);
      }
      _ => panic!("expected ScheduleTimer"),
    }
  }

  #[test]
  fn test_supervisor_snapshot() {
    let mut sup = make_supervisor(3, 5000);
    let child_pid = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      child_pid,
      RestartStrategy::Permanent,
    );
    let snap = sup.snapshot();
    assert!(snap.is_some());
  }
}
