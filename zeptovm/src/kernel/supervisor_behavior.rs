use std::collections::{HashMap, HashSet};

use crate::core::behavior::StepBehavior;
use crate::core::message::{Envelope, EnvelopePayload, Signal};
use crate::core::step_result::StepResult;
use crate::core::supervisor::{
  RestartStrategy, SupervisionStrategy, SupervisorSpec,
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

enum SupervisorPhase {
  Normal,
  ShuttingDown {
    pending_exits: HashSet<Pid>,
    children_to_restart: Vec<String>,
  },
}

pub struct SupervisorBehavior {
  spec: SupervisorSpec,
  children: HashMap<Pid, ChildState>,
  id_to_pid: HashMap<String, Pid>,
  child_order: Vec<String>,
  clock_ms: u64,
  phase: SupervisorPhase,
}

impl SupervisorBehavior {
  pub fn new(spec: SupervisorSpec) -> Self {
    Self {
      spec,
      children: HashMap::new(),
      id_to_pid: HashMap::new(),
      child_order: Vec::new(),
      clock_ms: 0,
      phase: SupervisorPhase::Normal,
    }
  }

  pub fn register_child(
    &mut self,
    child_id: String,
    child_pid: Pid,
    restart: RestartStrategy,
  ) {
    if !self.child_order.contains(&child_id) {
      self.child_order.push(child_id.clone());
    }
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

  pub fn is_shutting_down(&self) -> bool {
    matches!(
      self.phase,
      SupervisorPhase::ShuttingDown { .. }
    )
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
    // Handle ShuttingDown phase first
    if let SupervisorPhase::ShuttingDown {
      ref mut pending_exits,
      ref children_to_restart,
    } = self.phase
    {
      if let EnvelopePayload::Signal(
        Signal::ChildExited { child_pid, .. },
      ) = &msg.payload
      {
        pending_exits.remove(child_pid);
        // Remove from children tracking
        if let Some(child) =
          self.children.remove(child_pid)
        {
          self.id_to_pid.remove(&child.id);
        }

        if pending_exits.is_empty() {
          // All siblings exited — schedule restarts
          let ids = children_to_restart.clone();
          self.phase = SupervisorPhase::Normal;
          for child_id in &ids {
            let delay =
              self.spec.backoff.delay_ms(0);
            let deadline = self.clock_ms + delay;
            let timer = TimerSpec::new(
              ctx.pid,
              TimerKind::RetryBackoff,
              deadline,
            )
            .with_payload(serde_json::json!({
              "action": "restart_child",
              "child_id": child_id,
            }));
            ctx.schedule_timer(timer);
          }
        }
      }
      return StepResult::Continue;
    }

    // Normal phase
    match &msg.payload {
      EnvelopePayload::Signal(Signal::ChildExited {
        child_pid,
        reason,
      }) => {
        let child_pid = *child_pid;
        let reason = reason.clone();

        let child =
          match self.children.get_mut(&child_pid) {
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

        match self.spec.strategy {
          SupervisionStrategy::OneForOne => {
            // Restart just the failed child
            let delay = self
              .spec
              .backoff
              .delay_ms(child.restart_count - 1);
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
          SupervisionStrategy::OneForAll => {
            let failed_id = child.id.clone();
            // Collect all OTHER living children
            let mut pending = HashSet::new();
            let siblings: Vec<Pid> = self
              .children
              .keys()
              .filter(|p| **p != child_pid)
              .copied()
              .collect();

            for sib_pid in &siblings {
              ctx.send(Envelope::signal(
                *sib_pid,
                Signal::Shutdown,
              ));
              pending.insert(*sib_pid);
            }

            // Remove the failed child
            self.children.remove(&child_pid);
            self.id_to_pid.remove(&failed_id);

            // All children to restart (in order)
            let children_to_restart: Vec<String> =
              self.child_order.clone();

            if pending.is_empty() {
              // No siblings — restart immediately
              for child_id in &children_to_restart {
                let delay =
                  self.spec.backoff.delay_ms(0);
                let deadline =
                  self.clock_ms + delay;
                let timer = TimerSpec::new(
                  ctx.pid,
                  TimerKind::RetryBackoff,
                  deadline,
                )
                .with_payload(serde_json::json!({
                  "action": "restart_child",
                  "child_id": child_id,
                }));
                ctx.schedule_timer(timer);
              }
            } else {
              self.phase =
                SupervisorPhase::ShuttingDown {
                  pending_exits: pending,
                  children_to_restart,
                };
            }

            StepResult::Continue
          }
          SupervisionStrategy::RestForOne => {
            // Placeholder: behave like OneForOne
            // until Task 6
            let delay = self
              .spec
              .backoff
              .delay_ms(child.restart_count - 1);
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
        }
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
    BackoffPolicy, SupervisionStrategy, SupervisorSpec,
  };
  use crate::core::turn_context::TurnIntent;

  fn make_supervisor(
    max_restarts: u32,
    window_ms: u64,
  ) -> SupervisorBehavior {
    SupervisorBehavior::new(SupervisorSpec {
      max_restarts,
      restart_window_ms: window_ms,
      backoff: BackoffPolicy::Immediate,
      strategy: SupervisionStrategy::OneForOne,
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
      strategy: SupervisionStrategy::OneForOne,
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

  #[test]
  fn test_one_for_all_shutdown_siblings() {
    let mut sup =
      SupervisorBehavior::new(SupervisorSpec {
        max_restarts: 5,
        restart_window_ms: 10000,
        backoff: BackoffPolicy::Immediate,
        strategy: SupervisionStrategy::OneForAll,
      });
    let sup_pid = Pid::from_raw(1);
    let c1 = Pid::from_raw(10);
    let c2 = Pid::from_raw(11);
    let c3 = Pid::from_raw(12);
    sup.register_child(
      "w1".into(),
      c1,
      RestartStrategy::Permanent,
    );
    sup.register_child(
      "w2".into(),
      c2,
      RestartStrategy::Permanent,
    );
    sup.register_child(
      "w3".into(),
      c3,
      RestartStrategy::Permanent,
    );
    sup.init(None);

    // c2 dies
    let mut ctx = TurnContext::new(sup_pid);
    let msg = Envelope::signal(
      sup_pid,
      Signal::ChildExited {
        child_pid: c2,
        reason: Reason::Custom("crash".into()),
      },
    );
    let result = sup.handle(msg, &mut ctx);
    assert!(matches!(result, StepResult::Continue));

    // Should emit Shutdown signals for c1 and c3
    let intents = ctx.take_intents();
    let shutdown_count = intents
      .iter()
      .filter(|i| {
        if let TurnIntent::SendMessage(env) = i {
          matches!(
            &env.payload,
            EnvelopePayload::Signal(
              Signal::Shutdown
            )
          )
        } else {
          false
        }
      })
      .count();
    assert_eq!(shutdown_count, 2);
    assert!(sup.is_shutting_down());
  }

  #[test]
  fn test_one_for_all_restart_after_all_exit() {
    let mut sup =
      SupervisorBehavior::new(SupervisorSpec {
        max_restarts: 5,
        restart_window_ms: 10000,
        backoff: BackoffPolicy::Immediate,
        strategy: SupervisionStrategy::OneForAll,
      });
    let sup_pid = Pid::from_raw(1);
    let c1 = Pid::from_raw(10);
    let c2 = Pid::from_raw(11);
    sup.register_child(
      "w1".into(),
      c1,
      RestartStrategy::Permanent,
    );
    sup.register_child(
      "w2".into(),
      c2,
      RestartStrategy::Permanent,
    );
    sup.init(None);

    // c1 dies -> triggers OneForAll
    let mut ctx = TurnContext::new(sup_pid);
    sup.handle(
      Envelope::signal(
        sup_pid,
        Signal::ChildExited {
          child_pid: c1,
          reason: Reason::Custom("crash".into()),
        },
      ),
      &mut ctx,
    );
    // Discard intents from first handle
    ctx.take_intents();
    assert!(sup.is_shutting_down());

    // c2 exits (from shutdown)
    let mut ctx2 = TurnContext::new(sup_pid);
    sup.handle(
      Envelope::signal(
        sup_pid,
        Signal::ChildExited {
          child_pid: c2,
          reason: Reason::Shutdown,
        },
      ),
      &mut ctx2,
    );

    // All exited -> should schedule restarts
    assert!(!sup.is_shutting_down());
    let intents = ctx2.take_intents();
    let timer_count = intents
      .iter()
      .filter(|i| {
        matches!(i, TurnIntent::ScheduleTimer(_))
      })
      .count();
    assert_eq!(
      timer_count, 2,
      "should restart both children"
    );
  }

  #[test]
  fn test_one_for_all_single_child_restarts_immediately()
  {
    let mut sup =
      SupervisorBehavior::new(SupervisorSpec {
        max_restarts: 5,
        restart_window_ms: 10000,
        backoff: BackoffPolicy::Immediate,
        strategy: SupervisionStrategy::OneForAll,
      });
    let sup_pid = Pid::from_raw(1);
    let c1 = Pid::from_raw(10);
    sup.register_child(
      "w1".into(),
      c1,
      RestartStrategy::Permanent,
    );
    sup.init(None);

    // Only child dies -> no siblings to shut down
    let mut ctx = TurnContext::new(sup_pid);
    sup.handle(
      Envelope::signal(
        sup_pid,
        Signal::ChildExited {
          child_pid: c1,
          reason: Reason::Custom("crash".into()),
        },
      ),
      &mut ctx,
    );

    // Should NOT be shutting down (no pending)
    assert!(!sup.is_shutting_down());
    let intents = ctx.take_intents();
    let timer_count = intents
      .iter()
      .filter(|i| {
        matches!(i, TurnIntent::ScheduleTimer(_))
      })
      .count();
    assert_eq!(timer_count, 1, "restart immediately");
  }
}
