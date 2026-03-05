#[cfg(test)]
mod tests {
  use crate::agent_rt::process::*;
  use crate::agent_rt::scheduler::AgentScheduler;
  use crate::agent_rt::supervision::*;
  use crate::agent_rt::types::*;
  use std::any::Any;
  use std::sync::Arc;

  // A behavior that counts messages and stops after N
  struct CounterState {
    count: usize,
    stop_at: usize,
  }

  impl AgentState for CounterState {
    fn as_any(&self) -> &dyn Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
      self
    }
  }

  struct CounterBehavior {
    stop_at: usize,
  }

  impl AgentBehavior for CounterBehavior {
    fn init(
      &self,
      _args: serde_json::Value,
    ) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(CounterState {
        count: 0,
        stop_at: self.stop_at,
      }))
    }

    fn handle_message(
      &self,
      _msg: Message,
      state: &mut dyn AgentState,
    ) -> Action {
      let s = state
        .as_any_mut()
        .downcast_mut::<CounterState>()
        .unwrap();
      s.count += 1;
      if s.count >= s.stop_at {
        Action::Stop(Reason::Normal)
      } else {
        Action::Continue
      }
    }

    fn handle_exit(
      &self,
      _from: AgentPid,
      _reason: Reason,
      _state: &mut dyn AgentState,
    ) -> Action {
      Action::Continue
    }

    fn terminate(
      &self,
      _reason: Reason,
      _state: &mut dyn AgentState,
    ) {
    }
  }

  #[test]
  fn test_full_supervisor_lifecycle() {
    let behavior: Arc<dyn AgentBehavior> =
      Arc::new(CounterBehavior { stop_at: 3 });

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForOne,
      max_restarts: 10,
      max_seconds: 60,
      children: vec![ChildSpec {
        id: "counter".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      }],
    };

    let mut sched = AgentScheduler::new();
    let mut sup =
      Supervisor::start(&mut sched, spec).unwrap();

    let child_pid = sup.children[0].pid;

    // Send 3 messages — child will count to 3 and stop
    for _ in 0..3 {
      sched
        .send(
          child_pid,
          Message::Json(serde_json::json!("tick")),
        )
        .unwrap();
    }
    sched.enqueue(child_pid);

    // Run scheduler ticks until child processes all
    // messages and returns Action::Stop
    for _ in 0..10 {
      sched.tick();
    }

    // Child should have been terminated by Action::Stop
    assert!(
      sched.registry.lookup(&child_pid).is_none(),
      "Child should be terminated after stop_at=3"
    );

    // Supervisor should restart permanent child
    sup.handle_child_exit(
      &mut sched,
      child_pid,
      Reason::Normal,
    );

    assert_eq!(sup.children.len(), 1);
    assert_ne!(
      sup.children[0].pid, child_pid,
      "Permanent child should get a new PID"
    );

    // New child should exist in registry
    let new_pid = sup.children[0].pid;
    assert!(
      sched.registry.lookup(&new_pid).is_some(),
      "Restarted child should exist in registry"
    );
  }

  #[test]
  fn test_multiple_processes_fair_scheduling() {
    let mut sched = AgentScheduler::new();
    let behavior: Arc<dyn AgentBehavior> =
      Arc::new(CounterBehavior { stop_at: 1000 });

    // Spawn 3 processes, each with 100 messages
    let mut pids = Vec::new();
    for _ in 0..3 {
      let pid = sched
        .registry
        .spawn(
          behavior.clone(),
          serde_json::Value::Null,
        )
        .unwrap();
      for i in 0..100 {
        sched
          .send(
            pid,
            Message::Json(serde_json::json!(i)),
          )
          .unwrap();
      }
      sched.enqueue(pid);
      pids.push(pid);
    }

    // Run 10 ticks — each process should get some time
    for _ in 0..10 {
      sched.tick();
    }

    // All processes should have made progress.
    // With 200 reductions and dispatch cost of 1 per
    // message, each tick processes up to 200 messages.
    // After 10 ticks with round-robin on 3 normal
    // priority processes, all should be fully drained.
    for pid in &pids {
      if let Some(proc) = sched.registry.lookup(pid) {
        assert!(
          !proc.has_messages(),
          "Process {:?} should have drained mailbox",
          pid
        );
      }
    }
  }

  #[test]
  fn test_supervisor_restart_cycle() {
    let behavior: Arc<dyn AgentBehavior> =
      Arc::new(CounterBehavior { stop_at: 1 });

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForOne,
      max_restarts: 3,
      max_seconds: 60,
      children: vec![ChildSpec {
        id: "fast_crash".to_string(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,
        priority: Priority::Normal,
      }],
    };

    let mut sched = AgentScheduler::new();
    let mut sup =
      Supervisor::start(&mut sched, spec).unwrap();

    // Crash the child 4 times — exceeds max_restarts=3
    for round in 0..4 {
      if sup.is_shutdown() {
        break;
      }
      let pid = sup.children[0].pid;

      // Send message, tick to trigger Action::Stop
      sched
        .send(
          pid,
          Message::Text("crash".into()),
        )
        .unwrap();
      sched.enqueue(pid);
      sched.tick();

      // Notify supervisor of the exit
      sup.handle_child_exit(
        &mut sched,
        pid,
        Reason::Custom(format!("crash_{}", round)),
      );
    }

    assert!(
      sup.is_shutdown(),
      "Supervisor should shut down after exceeding \
       max_restarts"
    );
  }

  #[test]
  fn test_linked_processes_cascade_through_supervisor()
  {
    let behavior: Arc<dyn AgentBehavior> =
      Arc::new(CounterBehavior { stop_at: 1000 });

    let spec = SupervisorSpec {
      strategy: RestartStrategy::OneForAll,
      max_restarts: 5,
      max_seconds: 60,
      children: vec![
        ChildSpec {
          id: "worker_a".to_string(),
          behavior: behavior.clone(),
          args: serde_json::Value::Null,
          restart: ChildRestart::Permanent,
          priority: Priority::Normal,
        },
        ChildSpec {
          id: "worker_b".to_string(),
          behavior: behavior.clone(),
          args: serde_json::Value::Null,
          restart: ChildRestart::Permanent,
          priority: Priority::Normal,
        },
      ],
    };

    let mut sched = AgentScheduler::new();
    let mut sup =
      Supervisor::start(&mut sched, spec).unwrap();

    let pid_a = sup.children[0].pid;
    let pid_b = sup.children[1].pid;

    // Kill worker_a — OneForAll should restart both
    sup.handle_child_exit(
      &mut sched,
      pid_a,
      Reason::Custom("crash".into()),
    );

    assert_eq!(sup.children.len(), 2);
    let new_a = sup.children[0].pid;
    let new_b = sup.children[1].pid;
    assert_ne!(new_a, pid_a);
    assert_ne!(new_b, pid_b);

    // New processes should be runnable
    assert_eq!(
      sched
        .registry
        .lookup(&new_a)
        .unwrap()
        .status,
      ProcessStatus::Runnable
    );
    assert_eq!(
      sched
        .registry
        .lookup(&new_b)
        .unwrap()
        .status,
      ProcessStatus::Runnable
    );
  }
}
