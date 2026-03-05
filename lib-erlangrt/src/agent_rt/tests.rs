use std::{cell::RefCell, rc::Rc, sync::Arc};

use crate::agent_rt::{
  process::*, registry::AgentRegistry, scheduler::AgentScheduler, types::*,
};

struct EchoState {
  received: Vec<Message>,
}

impl AgentState for EchoState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

struct EchoBehavior;

impl AgentBehavior for EchoBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(EchoState {
      received: Vec::new(),
    }))
  }

  fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state.as_any_mut().downcast_mut::<EchoState>().unwrap();
    s.received.push(msg);
    Action::Continue
  }

  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) -> Action {
    Action::Continue
  }

  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

struct ExitAwareState {
  message_count: usize,
  exit_count: usize,
}

impl AgentState for ExitAwareState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

struct ExitAwareBehavior;

impl AgentBehavior for ExitAwareBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(ExitAwareState {
      message_count: 0,
      exit_count: 0,
    }))
  }

  fn handle_message(&self, _msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state.as_any_mut().downcast_mut::<ExitAwareState>().unwrap();
    s.message_count += 1;
    Action::Continue
  }

  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    state: &mut dyn AgentState,
  ) -> Action {
    let s = state.as_any_mut().downcast_mut::<ExitAwareState>().unwrap();
    s.exit_count += 1;
    Action::Continue
  }

  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_agent_pid_starts_at_high_range() {
  let pid = AgentPid::new();
  assert!(
    pid.raw() >= 0x8000_0000,
    "Agent PIDs should start at 0x8000_0000"
  );
}

#[test]
fn test_agent_pid_increments() {
  let p1 = AgentPid::new();
  let p2 = AgentPid::new();
  assert!(p2.raw() > p1.raw());
}

#[test]
fn test_agent_process_creation() {
  let behavior = Arc::new(EchoBehavior);
  let proc = AgentProcess::new(behavior, serde_json::Value::Null).unwrap();
  assert_eq!(proc.status, ProcessStatus::Runnable);
  assert!(!proc.has_messages());
  assert!(proc.links.is_empty());
  assert!(!proc.trap_exit);
}

#[test]
fn test_deliver_message() {
  let behavior = Arc::new(EchoBehavior);
  let mut proc = AgentProcess::new(behavior, serde_json::Value::Null).unwrap();
  assert!(!proc.has_messages());

  proc.deliver_message(Message::Text("hello".into()));
  assert!(proc.has_messages());

  let msg = proc.next_message().unwrap();
  match msg {
    Message::Text(s) => assert_eq!(s, "hello"),
    _ => panic!("Expected Text message"),
  }
  assert!(!proc.has_messages());
}

#[test]
fn test_deliver_message_wakes_waiting_process() {
  let behavior = Arc::new(EchoBehavior);
  let mut proc = AgentProcess::new(behavior, serde_json::Value::Null).unwrap();
  proc.status = ProcessStatus::Waiting;

  proc.deliver_message(Message::Text("wake up".into()));
  assert_eq!(proc.status, ProcessStatus::Runnable);
}

#[test]
fn test_handle_message_stores_in_state() {
  let behavior = Arc::new(EchoBehavior);
  let mut proc = AgentProcess::new(behavior.clone(), serde_json::Value::Null).unwrap();

  let msg = Message::Text("test".into());
  let state = proc.state.as_mut().unwrap();
  let _action = behavior.handle_message(msg, state.as_mut());

  let echo_state = state.as_any().downcast_ref::<EchoState>().unwrap();
  assert_eq!(echo_state.received.len(), 1);
}

#[test]
fn test_message_ordering_fifo() {
  let behavior = Arc::new(EchoBehavior);
  let mut proc = AgentProcess::new(behavior, serde_json::Value::Null).unwrap();

  proc.deliver_message(Message::Text("first".into()));
  proc.deliver_message(Message::Text("second".into()));
  proc.deliver_message(Message::Text("third".into()));

  match proc.next_message().unwrap() {
    Message::Text(s) => assert_eq!(s, "first"),
    _ => panic!("Expected Text"),
  }
  match proc.next_message().unwrap() {
    Message::Text(s) => assert_eq!(s, "second"),
    _ => panic!("Expected Text"),
  }
  match proc.next_message().unwrap() {
    Message::Text(s) => assert_eq!(s, "third"),
    _ => panic!("Expected Text"),
  }
}

// --- AgentRegistry tests ---

#[test]
fn test_registry_spawn_returns_pid() {
  let mut reg = AgentRegistry::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = reg.spawn(behavior, serde_json::Value::Null).unwrap();
  assert!(
    pid.raw() >= 0x8000_0000,
    "Spawned pid should be in the agent range"
  );
}

#[test]
fn test_registry_lookup_existing() {
  let mut reg = AgentRegistry::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = reg.spawn(behavior, serde_json::Value::Null).unwrap();
  let proc = reg.lookup(&pid);
  assert!(proc.is_some());
  assert_eq!(proc.unwrap().pid, pid);
}

#[test]
fn test_registry_lookup_missing() {
  let reg = AgentRegistry::new();
  let fake_pid = AgentPid::new();
  assert!(reg.lookup(&fake_pid).is_none());
}

#[test]
fn test_registry_remove() {
  let mut reg = AgentRegistry::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = reg.spawn(behavior, serde_json::Value::Null).unwrap();
  assert!(reg.lookup(&pid).is_some());

  let removed = reg.remove(&pid);
  assert!(removed.is_some());
  assert!(reg.lookup(&pid).is_none());
}

#[test]
fn test_registry_register_name() {
  let mut reg = AgentRegistry::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = reg.spawn(behavior, serde_json::Value::Null).unwrap();

  reg.register_name("echo_server", pid).unwrap();
  let found = reg.lookup_name("echo_server");
  assert_eq!(found, Some(pid));

  // Re-registering same pid is OK
  reg.register_name("echo_server", pid).unwrap();

  // Unregister and verify gone
  reg.unregister_name("echo_server");
  assert!(reg.lookup_name("echo_server").is_none());
}

#[test]
fn test_registry_count() {
  let mut reg = AgentRegistry::new();
  assert_eq!(reg.count(), 0);

  let b1 = Arc::new(EchoBehavior);
  let b2 = Arc::new(EchoBehavior);
  reg.spawn(b1, serde_json::Value::Null).unwrap();
  reg.spawn(b2, serde_json::Value::Null).unwrap();
  assert_eq!(reg.count(), 2);
}

// --- AgentScheduler tests ---

#[test]
fn test_scheduler_tick_returns_false_when_empty() {
  let mut sched = AgentScheduler::new();
  assert!(!sched.tick());
}

#[test]
fn test_scheduler_dispatches_message() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("hello".into())).unwrap();
  sched.enqueue(pid);
  assert!(sched.tick());
}

#[test]
fn test_scheduler_exit_message_calls_handle_exit() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(ExitAwareBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  sched
    .send(
      pid,
      Message::System(SystemMsg::Exit {
        from: AgentPid::new().raw(),
        reason: Reason::Custom("boom".into()),
      }),
    )
    .unwrap();
  sched.enqueue(pid);
  assert!(sched.tick());

  let proc = sched.registry.lookup(&pid).unwrap();
  let state = proc
    .state
    .as_ref()
    .unwrap()
    .as_any()
    .downcast_ref::<ExitAwareState>()
    .unwrap();
  assert_eq!(
    state.exit_count, 1,
    "System Exit should dispatch to handle_exit"
  );
  assert_eq!(
    state.message_count, 0,
    "System Exit should not go through handle_message"
  );
}

#[test]
fn test_scheduler_skips_stale_queue_entries() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let waiting_pid = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  if let Some(proc) = sched.registry.lookup_mut(&waiting_pid) {
    proc.status = ProcessStatus::Waiting;
  }
  sched.enqueue(waiting_pid);

  let runnable_pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched
    .send(runnable_pid, Message::Text("run".into()))
    .unwrap();
  sched.enqueue(runnable_pid);

  let did_work = sched.tick();
  assert!(
    did_work,
    "scheduler should skip stale waiting pid and run runnable pid"
  );
  let proc = sched.registry.lookup(&runnable_pid).unwrap();
  assert_eq!(proc.status, ProcessStatus::Waiting);
  assert!(!proc.has_messages());
}

#[test]
fn test_scheduler_exit_handler_is_invoked() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  let seen = Rc::new(RefCell::new(None::<(u64, String)>));
  let seen_closure = Rc::clone(&seen);
  sched.add_exit_handler(move |_sched, exited, reason| {
    let reason_label = match reason {
      Reason::Normal => "normal".to_string(),
      Reason::Shutdown => "shutdown".to_string(),
      Reason::Custom(s) => s,
    };
    *seen_closure.borrow_mut() = Some((exited.raw(), reason_label));
  });

  sched.terminate_process(pid, Reason::Custom("boom".into()));

  let seen = seen.borrow();
  let (seen_pid, seen_reason) = seen.as_ref().expect("exit handler should be called");
  assert_eq!(*seen_pid, pid.raw());
  assert_eq!(seen_reason, "boom");
}

#[test]
fn test_scheduler_process_yields_after_reductions() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  for i in 0..500 {
    sched
      .send(pid, Message::Text(format!("msg_{}", i)))
      .unwrap();
  }
  sched.enqueue(pid);
  sched.tick();
  let proc = sched.registry.lookup(&pid).unwrap();
  assert!(
    proc.has_messages(),
    "Should still have unprocessed messages \
     after reduction limit"
  );
}

#[test]
fn test_scheduler_waiting_process_wakes_on_message() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.enqueue(pid);
  // empty mailbox -> Waiting
  sched.tick();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting
  );

  sched.send(pid, Message::Text("wake".into())).unwrap();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Runnable
  );
}

#[test]
fn test_scheduler_high_priority_runs_first() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let low_pid = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  if let Some(p) = sched.registry.lookup_mut(&low_pid) {
    p.priority = Priority::Low;
  }

  let high_pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  if let Some(p) = sched.registry.lookup_mut(&high_pid) {
    p.priority = Priority::High;
  }

  // Send messages to both
  sched
    .send(low_pid, Message::Text("low_msg".into()))
    .unwrap();
  sched
    .send(high_pid, Message::Text("high_msg".into()))
    .unwrap();

  // Enqueue low first, then high
  sched.enqueue(low_pid);
  sched.enqueue(high_pid);

  // Tick should run high_pid first
  sched.tick();

  // high_pid should have consumed its message
  // (now Waiting with empty mailbox)
  let high_proc = sched.registry.lookup(&high_pid).unwrap();
  assert_eq!(
    high_proc.status,
    ProcessStatus::Waiting,
    "High priority should have run first"
  );
  assert!(!high_proc.has_messages());

  // low_pid should still have its message
  let low_proc = sched.registry.lookup(&low_pid).unwrap();
  assert!(
    low_proc.has_messages(),
    "Low priority should not have run yet"
  );
}

#[test]
fn test_scheduler_terminate_removes_process() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  assert!(sched.registry.lookup(&pid).is_some());

  sched.terminate_process(pid, Reason::Normal);
  assert!(sched.registry.lookup(&pid).is_none());
}

#[test]
fn test_scheduler_terminate_notifies_links() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let pid_a = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, set trap_exit so b receives
  // the Exit as a message instead of cascading
  if let Some(proc_a) = sched.registry.lookup_mut(&pid_a) {
    proc_a.links.push(pid_b);
  }
  if let Some(proc_b) = sched.registry.lookup_mut(&pid_b) {
    proc_b.trap_exit = true;
  }

  // Terminate a, b should get Exit message
  sched.terminate_process(pid_a, Reason::Normal);

  let proc_b = sched.registry.lookup(&pid_b).unwrap();
  assert!(
    proc_b.has_messages(),
    "Linked process should receive Exit message"
  );
}

#[test]
fn test_scheduler_send_to_missing_process_errors() {
  let mut sched = AgentScheduler::new();
  let fake_pid = AgentPid::new();
  let result = sched.send(fake_pid, Message::Text("oops".into()));
  assert!(result.is_err());
}

#[test]
fn test_scheduler_multiple_ticks_drain_mailbox() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Send exactly 5 messages
  for i in 0..5 {
    sched
      .send(pid, Message::Text(format!("msg_{}", i)))
      .unwrap();
  }
  sched.enqueue(pid);

  // One tick should process all 5 (well within
  // 200 reductions)
  sched.tick();

  let proc = sched.registry.lookup(&pid).unwrap();
  assert!(
    !proc.has_messages(),
    "All 5 messages should be consumed in one tick"
  );
  assert_eq!(proc.status, ProcessStatus::Waiting);
}

// --- Bridge tests ---

#[test]
fn test_bridge_creation() {
  let (_handle, _worker) = crate::agent_rt::bridge::create_bridge();
}

#[test]
fn test_bridge_submit_returns_correlation_id() {
  let (mut handle, _worker) = crate::agent_rt::bridge::create_bridge();
  let pid = AgentPid::new();
  let op = IoOp::Timer {
    duration: std::time::Duration::from_millis(100),
  };
  let id1 = handle.submit(pid, op).unwrap();
  let op2 = IoOp::Timer {
    duration: std::time::Duration::from_millis(50),
  };
  let id2 = handle.submit(pid, op2).unwrap();
  assert_eq!(id1, 0);
  assert_eq!(id2, 1);
}

#[test]
fn test_bridge_drain_empty() {
  let (handle, _worker) = crate::agent_rt::bridge::create_bridge();
  assert!(handle.drain_responses().is_empty());
}

#[tokio::test]
async fn test_bridge_roundtrip_timer() {
  let (mut handle, worker) = crate::agent_rt::bridge::create_bridge();
  let pid = AgentPid::new();

  let worker_handle = tokio::spawn(async move { worker.run().await });

  let _corr_id = handle
    .submit(
      pid,
      IoOp::Timer {
        duration: std::time::Duration::from_millis(10),
      },
    )
    .unwrap();

  // Wait for response
  tokio::time::sleep(std::time::Duration::from_millis(200)).await;

  let responses = handle.drain_responses();
  assert_eq!(responses.len(), 1);
  assert_eq!(responses[0].0, pid);

  drop(handle); // disconnect channels to stop worker
  let _ = worker_handle.await;
}

// --- Supervision tests ---

use crate::agent_rt::supervision::*;

#[test]
fn test_supervisor_spec_creation() {
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![],
  };
  assert_eq!(spec.max_restarts, 5);
}

#[test]
fn test_supervisor_starts_children() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![
      ChildSpec {
        id: "child1".into(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,

        priority: Priority::Normal,
      },
      ChildSpec {
        id: "child2".into(),
        behavior,
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,

        priority: Priority::Normal,
      },
    ],
  };
  let mut sched = AgentScheduler::new();
  let sup = Supervisor::start(&mut sched, spec).unwrap();
  assert_eq!(sup.children.len(), 2);
  assert_eq!(sched.registry.count(), 2);
}

#[test]
fn test_one_for_one_restart() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "child1".into(),
      behavior,
      args: serde_json::Value::Null,
      restart: ChildRestart::Permanent,

      priority: Priority::Normal,
    }],
  };
  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();
  let old_pid = sup.children[0].pid;

  // Simulate crash
  sup.handle_child_exit(&mut sched, old_pid, Reason::Custom("crash".into()));

  assert_eq!(sup.children.len(), 1);
  assert_ne!(sup.children[0].pid, old_pid);
  assert!(!sup.is_shutdown());
}

#[test]
fn test_transient_not_restarted_on_normal_exit() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "child1".into(),
      behavior,
      args: serde_json::Value::Null,
      restart: ChildRestart::Transient,

      priority: Priority::Normal,
    }],
  };
  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();
  let pid = sup.children[0].pid;

  sup.handle_child_exit(&mut sched, pid, Reason::Normal);
  assert!(sup.children.is_empty());
}

#[test]
fn test_temporary_never_restarted() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "child1".into(),
      behavior,
      args: serde_json::Value::Null,
      restart: ChildRestart::Temporary,

      priority: Priority::Normal,
    }],
  };
  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();
  let pid = sup.children[0].pid;

  sup.handle_child_exit(&mut sched, pid, Reason::Custom("crash".into()));
  assert!(sup.children.is_empty());
}

#[test]
fn test_max_restarts_exceeded() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 2,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "child1".into(),
      behavior,
      args: serde_json::Value::Null,
      restart: ChildRestart::Permanent,

      priority: Priority::Normal,
    }],
  };
  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();

  // Crash 3 times — exceeds max_restarts of 2
  for _ in 0..3 {
    if let Some(child) = sup.children.first() {
      let pid = child.pid;
      sup.handle_child_exit(&mut sched, pid, Reason::Custom("crash".into()));
    }
  }
  assert!(sup.is_shutdown());
}

#[test]
fn test_one_for_all_restarts_all() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForAll,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![
      ChildSpec {
        id: "a".into(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,

        priority: Priority::Normal,
      },
      ChildSpec {
        id: "b".into(),
        behavior,
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,

        priority: Priority::Normal,
      },
    ],
  };
  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();
  let old_pids: Vec<_> = sup.children.iter().map(|c| c.pid).collect();

  // Kill first child
  sup.handle_child_exit(&mut sched, old_pids[0], Reason::Custom("crash".into()));

  // Both should be restarted with new PIDs
  assert_eq!(sup.children.len(), 2);
  let new_pids: Vec<_> = sup.children.iter().map(|c| c.pid).collect();
  assert!(new_pids.iter().all(|p| !old_pids.contains(p)));
}

// --- Bug C1: trap_exit cascading death tests ---

#[test]
fn test_link_cascades_death_without_trap_exit() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let pid_a = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, both have trap_exit=false
  if let Some(proc_a) = sched.registry.lookup_mut(&pid_a) {
    proc_a.links.push(pid_b);
  }

  // Kill a with a non-normal reason
  sched.terminate_process(pid_a, Reason::Custom("crash".into()));

  // Both should be gone from registry
  assert!(
    sched.registry.lookup(&pid_a).is_none(),
    "pid_a should be removed"
  );
  assert!(
    sched.registry.lookup(&pid_b).is_none(),
    "pid_b should be cascade-terminated"
  );
}

#[test]
fn test_link_does_not_cascade_on_normal_exit() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let pid_a = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, both have trap_exit=false
  if let Some(proc_a) = sched.registry.lookup_mut(&pid_a) {
    proc_a.links.push(pid_b);
  }

  // Kill a with Normal reason
  sched.terminate_process(pid_a, Reason::Normal);

  // a is gone but b should still be alive
  assert!(
    sched.registry.lookup(&pid_a).is_none(),
    "pid_a should be removed"
  );
  assert!(
    sched.registry.lookup(&pid_b).is_some(),
    "pid_b should still be alive on normal exit"
  );
  // b should have no messages (normal + no trap)
  let proc_b = sched.registry.lookup(&pid_b).unwrap();
  assert!(
    !proc_b.has_messages(),
    "No message delivered for normal exit \
     without trap_exit"
  );
}

#[test]
fn test_trap_exit_delivers_message_instead_of_cascade() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let pid_a = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, set trap_exit on b
  if let Some(proc_a) = sched.registry.lookup_mut(&pid_a) {
    proc_a.links.push(pid_b);
  }
  if let Some(proc_b) = sched.registry.lookup_mut(&pid_b) {
    proc_b.trap_exit = true;
  }

  // Kill a with crash reason
  sched.terminate_process(pid_a, Reason::Custom("crash".into()));

  // a is gone
  assert!(sched.registry.lookup(&pid_a).is_none());

  // b should still be alive with an Exit message
  let proc_b = sched.registry.lookup(&pid_b).unwrap();
  assert!(
    proc_b.has_messages(),
    "trap_exit process should receive Exit msg"
  );
  // Verify it's an Exit system message
  let proc_b = sched.registry.lookup_mut(&pid_b).unwrap();
  let msg = proc_b.next_message().unwrap();
  match msg {
    Message::System(SystemMsg::Exit { from, reason }) => {
      assert_eq!(from, pid_a.raw());
      match reason {
        Reason::Custom(s) => {
          assert_eq!(s, "crash")
        }
        _ => panic!("Expected Custom reason"),
      }
    }
    _ => panic!("Expected System Exit message"),
  }
}

// --- Bug C2: reduction cost tests ---

/// A behavior that returns Action::Send on
/// every message, sending to a fixed target.
struct SendBehavior {
  target: AgentPid,
}

struct SendState;

impl AgentState for SendState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for SendBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SendState))
  }

  fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
    Action::Send {
      to: self.target,
      msg: Message::Text("reply".into()),
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

  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_send_action_costs_reductions() {
  // Create a target process to receive sends
  let mut sched = AgentScheduler::new();
  let echo = Arc::new(EchoBehavior);
  let target = sched.registry.spawn(echo, serde_json::Value::Null).unwrap();

  // Create a sender that returns Send on every msg
  let sender_behavior = Arc::new(SendBehavior { target });
  let sender = sched
    .registry
    .spawn(sender_behavior, serde_json::Value::Null)
    .unwrap();

  // Give sender 10 messages
  for i in 0..10 {
    sched
      .send(sender, Message::Text(format!("m{}", i)))
      .unwrap();
  }
  sched.enqueue(sender);
  sched.tick();

  // Each message costs 1 (dispatch) + 2 (send) = 3
  // reductions. With 200 budget, can process
  // floor(200/3) = 66 messages. All 10 should be
  // consumed. Check that reductions were actually
  // deducted: 200 - 10*3 = 170.
  let proc = sched.registry.lookup(&sender).unwrap();
  assert_eq!(
    proc.reductions, 170,
    "Each Send should cost 3 reductions total \
     (1 dispatch + 2 send)"
  );
}

// --- Bug C3: supervisor double-terminate tests ---

#[test]
fn test_transient_not_restarted_on_shutdown() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "child1".into(),
      behavior,
      args: serde_json::Value::Null,
      restart: ChildRestart::Transient,

      priority: Priority::Normal,
    }],
  };
  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();
  let pid = sup.children[0].pid;

  sup.handle_child_exit(&mut sched, pid, Reason::Shutdown);
  assert!(
    sup.children.is_empty(),
    "Transient should NOT restart on Shutdown"
  );
}

#[test]
fn test_rest_for_one_restarts_only_subsequent() {
  let behavior = Arc::new(EchoBehavior);
  let spec = SupervisorSpec {
    strategy: RestartStrategy::RestForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![
      ChildSpec {
        id: "a".into(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,

        priority: Priority::Normal,
      },
      ChildSpec {
        id: "b".into(),
        behavior: behavior.clone(),
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,

        priority: Priority::Normal,
      },
      ChildSpec {
        id: "c".into(),
        behavior,
        args: serde_json::Value::Null,
        restart: ChildRestart::Permanent,

        priority: Priority::Normal,
      },
    ],
  };
  let mut sched = AgentScheduler::new();
  let mut sup = Supervisor::start(&mut sched, spec).unwrap();

  let pid_a = sup.children[0].pid;
  let pid_b = sup.children[1].pid;
  let pid_c = sup.children[2].pid;

  // Kill child b — should restart b and c, leave a
  sup.handle_child_exit(&mut sched, pid_b, Reason::Custom("crash".into()));

  assert_eq!(sup.children.len(), 3);
  // a should keep same PID
  assert_eq!(sup.children[0].pid, pid_a, "Child a should be unchanged");
  // b and c should have new PIDs
  assert_ne!(sup.children[1].pid, pid_b, "Child b should be restarted");
  assert_ne!(sup.children[2].pid, pid_c, "Child c should be restarted");
}

// --- H6: Scheduler-bridge integration tests ---

/// A behavior that returns IoRequest on first message.
struct IoBehavior;

struct IoState;

impl AgentState for IoState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for IoBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(IoState))
  }
  fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
    Action::IoRequest(IoOp::Timer {
      duration: std::time::Duration::from_millis(10),
    })
  }
  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) -> Action {
    Action::Continue
  }
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

struct SingleTimerBehavior {
  duration: std::time::Duration,
}

struct SingleTimerState {
  requested: bool,
}

impl AgentState for SingleTimerState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for SingleTimerBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SingleTimerState { requested: false }))
  }

  fn handle_message(&self, _msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<SingleTimerState>()
      .unwrap();
    if s.requested {
      Action::Continue
    } else {
      s.requested = true;
      Action::IoRequest(IoOp::Timer {
        duration: self.duration,
      })
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

  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_scheduler_with_bridge_submits_io_request() {
  let (bridge_handle, _worker) = crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);

  let behavior = Arc::new(IoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("trigger".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  let proc = sched.registry.lookup(&pid).unwrap();
  assert_eq!(
    proc.status,
    ProcessStatus::Waiting,
    "Process should be Waiting after IoRequest"
  );
}

#[test]
fn test_scheduler_without_bridge_io_delivers_error() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(IoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("trigger".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  let proc = sched.registry.lookup(&pid).unwrap();
  assert_eq!(
    proc.status,
    ProcessStatus::Runnable,
    "Without bridge, IoRequest should deliver error and set Runnable"
  );
  assert!(
    proc.has_messages(),
    "Without bridge, process should receive error message"
  );
}

#[tokio::test]
async fn test_scheduler_bridge_roundtrip() {
  let (bridge_handle, worker) = crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);

  let worker_handle = tokio::spawn(async move { worker.run().await });

  // IoBehavior always returns IoRequest, so after
  // the response comes back and is dispatched, the
  // process will issue another IoRequest and go back
  // to Waiting. We verify the bridge delivered by
  // checking the bridge handle's correlation_id
  // advanced (submit was called).
  let behavior = Arc::new(IoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("trigger".into())).unwrap();
  sched.enqueue(pid);

  // First tick: dispatches message, gets IoRequest,
  // submits to bridge, process goes Waiting.
  sched.tick();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting,
  );

  // Wait for Tokio to process the timer
  tokio::time::sleep(std::time::Duration::from_millis(200)).await;

  // Second tick: drains IoResponse, delivers to
  // process (wakes it), picks it, dispatches the
  // IoResponse message, behavior returns IoRequest
  // again, process goes Waiting again. This proves
  // the full roundtrip worked.
  let ran = sched.tick();
  assert!(ran, "Second tick should have run work");

  // Process should be Waiting (issued another
  // IoRequest after receiving the response)
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting,
  );

  // Clean up
  drop(sched);
  let _ = worker_handle.await;
}

// --- S2: Action::Stop through scheduler ---

/// A behavior that returns Action::Stop on any message.
struct StopBehavior;

struct StopState;

impl AgentState for StopState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for StopBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(StopState))
  }
  fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
    Action::Stop(Reason::Normal)
  }
  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) -> Action {
    Action::Continue
  }
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_scheduler_stop_terminates_process() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(StopBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("die".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  assert!(
    sched.registry.lookup(&pid).is_none(),
    "Process should be removed after Action::Stop"
  );
}

// --- S3: Action::Spawn through scheduler ---

/// A behavior that spawns an EchoBehavior child
/// on first message.
struct SpawnBehavior;

struct SpawnState;

impl AgentState for SpawnState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for SpawnBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SpawnState))
  }
  fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
    Action::Spawn {
      behavior: Arc::new(EchoBehavior),
      args: serde_json::Value::Null,
      link: true,
      monitor: false,
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
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_scheduler_spawn_creates_child() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(SpawnBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  assert_eq!(sched.registry.count(), 1);

  sched.send(pid, Message::Text("spawn".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  assert_eq!(
    sched.registry.count(),
    2,
    "Spawn action should create a new child process"
  );
}

// --- S4: register_name rejects duplicates ---

// --- Runtime gaps: spawn notification + auto-link ---

/// A behavior that spawns a child and wants to know
/// the child PID via SpawnResult system message.
struct SpawnAndTrackBehavior;

struct SpawnAndTrackState {
  child_pid: Option<AgentPid>,
}

impl AgentState for SpawnAndTrackState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for SpawnAndTrackBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SpawnAndTrackState { child_pid: None }))
  }
  fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<SpawnAndTrackState>()
      .unwrap();
    match msg {
      Message::System(SystemMsg::SpawnResult {
        child_pid,
        monitor_ref: _,
      }) => {
        s.child_pid = Some(AgentPid::from_raw(child_pid));
        Action::Continue
      }
      Message::Text(_) => Action::Spawn {
        behavior: Arc::new(EchoBehavior),
        args: serde_json::Value::Null,
        link: true,
        monitor: false,
      },
      _ => Action::Continue,
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
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_spawn_notifies_parent_with_child_pid() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(SpawnAndTrackBehavior);
  let parent = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Send trigger to spawn a child
  sched.send(parent, Message::Text("spawn".into())).unwrap();
  sched.enqueue(parent);

  // First tick: processes Text, returns Spawn action,
  // scheduler spawns child and delivers SpawnResult
  sched.tick();

  // Second tick: parent processes SpawnResult message
  sched.enqueue(parent);
  sched.tick();

  // Verify parent received the child PID
  let proc = sched.registry.lookup(&parent).unwrap();
  let state = proc
    .state
    .as_ref()
    .unwrap()
    .as_any()
    .downcast_ref::<SpawnAndTrackState>()
    .unwrap();
  assert!(
    state.child_pid.is_some(),
    "Parent should receive SpawnResult with child PID"
  );

  // Verify the child actually exists
  let child_pid = state.child_pid.unwrap();
  assert!(
    sched.registry.lookup(&child_pid).is_some(),
    "Spawned child should exist in registry"
  );
}

#[test]
fn test_spawn_auto_links_parent_to_child() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(SpawnBehavior);
  let parent = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  sched.send(parent, Message::Text("spawn".into())).unwrap();
  sched.enqueue(parent);
  sched.tick();

  // Find the child (the other process in registry)
  let child_pid = sched
    .registry
    .pids()
    .into_iter()
    .find(|p| *p != parent)
    .expect("Child should exist");

  // Parent should be linked to child
  let parent_proc = sched.registry.lookup(&parent).unwrap();
  assert!(
    parent_proc.links.contains(&child_pid),
    "Parent should have link to child after spawn"
  );

  // Child should be linked back to parent
  let child_proc = sched.registry.lookup(&child_pid).unwrap();
  assert!(
    child_proc.links.contains(&parent),
    "Child should have link back to parent"
  );
}

#[test]
fn test_monitor_ref_is_unique_per_call() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let watcher = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let target = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  let r1 = sched.monitor(watcher, target).unwrap();
  let r2 = sched.monitor(watcher, target).unwrap();
  assert_ne!(r1.raw(), r2.raw());
}

#[test]
fn test_monitor_gets_down_on_monitored_exit() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let watcher = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let target = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  let mon_ref = sched.monitor(watcher, target).unwrap();
  sched.terminate_process(target, Reason::Custom("boom".into()));

  let watcher_proc = sched.registry.lookup_mut(&watcher).unwrap();
  let msg = watcher_proc.next_message().expect("expected DOWN message");
  match msg {
    Message::System(SystemMsg::Down {
      monitor_ref,
      pid,
      reason,
    }) => {
      assert_eq!(monitor_ref, mon_ref.raw());
      assert_eq!(pid, target.raw());
      assert!(matches!(reason, Reason::Custom(_)));
    }
    _ => panic!("expected DOWN system message"),
  }
}

#[test]
fn test_monitor_gets_down_on_normal_exit() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let watcher = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let target = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  let mon_ref = sched.monitor(watcher, target).unwrap();
  sched.terminate_process(target, Reason::Normal);

  let watcher_proc = sched.registry.lookup_mut(&watcher).unwrap();
  let msg = watcher_proc.next_message().expect("expected DOWN message");
  match msg {
    Message::System(SystemMsg::Down {
      monitor_ref,
      pid,
      reason,
    }) => {
      assert_eq!(monitor_ref, mon_ref.raw());
      assert_eq!(pid, target.raw());
      assert!(matches!(reason, Reason::Normal));
    }
    _ => panic!("expected DOWN system message"),
  }
}

#[test]
fn test_demonitor_prevents_down_delivery() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);
  let watcher = sched
    .registry
    .spawn(behavior.clone(), serde_json::Value::Null)
    .unwrap();
  let target = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  let mon_ref = sched.monitor(watcher, target).unwrap();
  assert!(sched.demonitor(watcher, mon_ref));
  sched.terminate_process(target, Reason::Custom("boom".into()));

  let watcher_proc = sched.registry.lookup(&watcher).unwrap();
  assert!(
    !watcher_proc.has_messages(),
    "DOWN should not be delivered after demonitor"
  );
}

struct SpawnNoLinkBehavior;
struct SpawnNoLinkState {
  child_pid: Option<AgentPid>,
}

impl AgentState for SpawnNoLinkState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for SpawnNoLinkBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SpawnNoLinkState { child_pid: None }))
  }
  fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<SpawnNoLinkState>()
      .unwrap();
    match msg {
      Message::System(SystemMsg::SpawnResult {
        child_pid,
        monitor_ref: _,
      }) => {
        s.child_pid = Some(AgentPid::from_raw(child_pid));
        Action::Continue
      }
      Message::Text(_) => Action::Spawn {
        behavior: Arc::new(EchoBehavior),
        args: serde_json::Value::Null,
        link: false,
        monitor: false,
      },
      _ => Action::Continue,
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
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_spawn_with_link_false_does_not_cascade() {
  let mut sched = AgentScheduler::new();
  let parent = sched
    .registry
    .spawn(Arc::new(SpawnNoLinkBehavior), serde_json::Value::Null)
    .unwrap();
  sched.send(parent, Message::Text("spawn".into())).unwrap();
  sched.enqueue(parent);
  sched.tick();
  sched.enqueue(parent);
  sched.tick();

  let parent_proc = sched.registry.lookup(&parent).unwrap();
  let parent_state = parent_proc
    .state
    .as_ref()
    .unwrap()
    .as_any()
    .downcast_ref::<SpawnNoLinkState>()
    .unwrap();
  let child = parent_state.child_pid.expect("child pid");

  sched.terminate_process(parent, Reason::Custom("boom".into()));
  assert!(
    sched.registry.lookup(&child).is_some(),
    "Child should survive when spawned without link"
  );
}

struct SpawnMonitorBehavior;
struct SpawnMonitorState {
  monitor_ref: Option<u64>,
}

impl AgentState for SpawnMonitorState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for SpawnMonitorBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SpawnMonitorState { monitor_ref: None }))
  }
  fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<SpawnMonitorState>()
      .unwrap();
    match msg {
      Message::System(SystemMsg::SpawnResult {
        child_pid: _,
        monitor_ref,
      }) => {
        s.monitor_ref = monitor_ref;
        Action::Continue
      }
      Message::Text(_) => Action::Spawn {
        behavior: Arc::new(EchoBehavior),
        args: serde_json::Value::Null,
        link: false,
        monitor: true,
      },
      _ => Action::Continue,
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
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_spawn_with_monitor_true_includes_monitor_ref() {
  let mut sched = AgentScheduler::new();
  let parent = sched
    .registry
    .spawn(Arc::new(SpawnMonitorBehavior), serde_json::Value::Null)
    .unwrap();
  sched.send(parent, Message::Text("spawn".into())).unwrap();
  sched.enqueue(parent);
  sched.tick();
  sched.enqueue(parent);
  sched.tick();

  let parent_proc = sched.registry.lookup(&parent).unwrap();
  let parent_state = parent_proc
    .state
    .as_ref()
    .unwrap()
    .as_any()
    .downcast_ref::<SpawnMonitorState>()
    .unwrap();
  assert!(
    parent_state.monitor_ref.is_some(),
    "SpawnResult should include monitor_ref when monitor=true"
  );
}

#[test]
fn test_multiple_exit_handlers_all_fire() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();
  let hits = Rc::new(RefCell::new(Vec::<u64>::new()));

  let hits_a = Rc::clone(&hits);
  sched.add_exit_handler(move |_sched, exited, _reason| {
    hits_a.borrow_mut().push(exited.raw());
  });
  let hits_b = Rc::clone(&hits);
  sched.add_exit_handler(move |_sched, exited, _reason| {
    hits_b.borrow_mut().push(exited.raw());
  });

  sched.terminate_process(pid, Reason::Custom("bye".into()));
  let hits = hits.borrow();
  assert_eq!(hits.len(), 2, "Both exit handlers should fire");
  assert_eq!(hits[0], pid.raw());
  assert_eq!(hits[1], pid.raw());
}

// --- Runtime gap: extensible IoOp ---

#[test]
fn test_custom_io_op_through_bridge() {
  let (bridge_handle, _worker) = crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);

  // A behavior that issues a Custom IoOp
  struct CustomIoBehavior;
  struct CustomIoState;

  impl AgentState for CustomIoState {
    fn as_any(&self) -> &dyn std::any::Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
      self
    }
  }

  impl AgentBehavior for CustomIoBehavior {
    fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(CustomIoState))
    }
    fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
      Action::IoRequest(IoOp::Custom {
        kind: "llm_request".to_string(),
        payload: serde_json::json!({
          "model": "claude-3",
          "prompt": "hello"
        }),
      })
    }
    fn handle_exit(
      &self,
      _from: AgentPid,
      _reason: Reason,
      _state: &mut dyn AgentState,
    ) -> Action {
      Action::Continue
    }
    fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
  }

  let behavior: Arc<dyn AgentBehavior> = Arc::new(CustomIoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("go".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  // Process should be Waiting after Custom IoRequest
  let proc = sched.registry.lookup(&pid).unwrap();
  assert_eq!(
    proc.status,
    ProcessStatus::Waiting,
    "Custom IoOp should set process to Waiting"
  );
}

#[test]
fn test_register_name_rejects_duplicate() {
  let mut reg = AgentRegistry::new();
  let b1 = Arc::new(EchoBehavior);
  let b2 = Arc::new(EchoBehavior);
  let pid1 = reg.spawn(b1, serde_json::Value::Null).unwrap();
  let pid2 = reg.spawn(b2, serde_json::Value::Null).unwrap();

  reg.register_name("server", pid1).unwrap();

  // Different pid with same name should fail
  let result = reg.register_name("server", pid2);
  assert!(result.is_err(), "Should reject duplicate name registration");

  // Original registration should be unchanged
  assert_eq!(reg.lookup_name("server"), Some(pid1));
}

// === Fix 1: catch_unwind around behavior callbacks ===

/// A behavior that panics in handle_message.
struct PanicOnMessageBehavior;

struct PanicState;

impl AgentState for PanicState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for PanicOnMessageBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(PanicState))
  }
  fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
    panic!("behavior bug in handle_message!")
  }
  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) -> Action {
    Action::Continue
  }
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_panic_in_handle_message_terminates_process() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(PanicOnMessageBehavior), serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("boom".into())).unwrap();
  sched.enqueue(pid);

  // Should NOT panic — scheduler catches it and terminates process
  let ran = sched.tick();
  assert!(ran);

  // Process should be removed (terminated due to panic)
  assert!(
    sched.registry.lookup(&pid).is_none(),
    "Panicking process should be terminated and removed"
  );
}

/// A behavior that panics in handle_exit.
struct PanicOnExitBehavior;

impl AgentBehavior for PanicOnExitBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(PanicState))
  }
  fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
    Action::Continue
  }
  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) -> Action {
    panic!("behavior bug in handle_exit!")
  }
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[test]
fn test_panic_in_handle_exit_terminates_process() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(PanicOnExitBehavior), serde_json::Value::Null)
    .unwrap();

  // Deliver an Exit system message to trigger handle_exit
  sched
    .send(
      pid,
      Message::System(SystemMsg::Exit {
        from: 0,
        reason: Reason::Custom("test".into()),
      }),
    )
    .unwrap();
  sched.enqueue(pid);

  // Should NOT panic
  let ran = sched.tick();
  assert!(ran);

  // Process should be terminated
  assert!(
    sched.registry.lookup(&pid).is_none(),
    "Process panicking in handle_exit should be terminated"
  );
}

/// A behavior that panics in terminate callback.
struct PanicOnTerminateBehavior;

impl AgentBehavior for PanicOnTerminateBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(PanicState))
  }
  fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
    Action::Stop(Reason::Normal)
  }
  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) -> Action {
    Action::Continue
  }
  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {
    panic!("behavior bug in terminate!")
  }
}

#[test]
fn test_panic_in_terminate_does_not_crash_scheduler() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(PanicOnTerminateBehavior), serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("stop".into())).unwrap();
  sched.enqueue(pid);

  // tick() triggers handle_message -> Stop -> terminate_process -> terminate() which panics
  // Should NOT crash the scheduler
  let ran = sched.tick();
  assert!(ran);

  // Process should still be cleaned up from registry
  assert!(
    sched.registry.lookup(&pid).is_none(),
    "Process should be removed even if terminate() panics"
  );
}

// === Fix 2: No-bridge IoRequest delivers error ===

#[test]
fn test_no_bridge_io_request_delivers_error() {
  let mut sched = AgentScheduler::new();
  // No bridge set
  let behavior = Arc::new(IoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("trigger".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  // Process should NOT be stuck in Waiting — should have error message
  let proc = sched.registry.lookup(&pid).unwrap();
  assert!(
    proc.has_messages(),
    "Should have error message in mailbox when no bridge"
  );
  assert_eq!(
    proc.status,
    ProcessStatus::Runnable,
    "Should be Runnable after error delivery, not stuck Waiting"
  );
}

// === Fix 3: Supervisor auto-wire via start_linked ===

#[test]
fn test_supervisor_start_linked_auto_restarts_children() {
  use crate::agent_rt::supervision::*;

  // A behavior that stops on first message (simulates crash)
  struct CrashOnMessageBehavior;
  struct CrashState;

  impl AgentState for CrashState {
    fn as_any(&self) -> &dyn std::any::Any {
      self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
      self
    }
  }

  impl AgentBehavior for CrashOnMessageBehavior {
    fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
      Ok(Box::new(CrashState))
    }
    fn handle_message(&self, _msg: Message, _state: &mut dyn AgentState) -> Action {
      Action::Stop(Reason::Custom("crash".into()))
    }
    fn handle_exit(
      &self,
      _from: AgentPid,
      _reason: Reason,
      _state: &mut dyn AgentState,
    ) -> Action {
      Action::Continue
    }
    fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
  }

  let mut sched = AgentScheduler::new();
  let spec = SupervisorSpec {
    strategy: RestartStrategy::OneForOne,
    max_restarts: 5,
    max_seconds: 60,
    children: vec![ChildSpec {
      id: "worker".to_string(),
      behavior: Arc::new(CrashOnMessageBehavior) as Arc<dyn AgentBehavior>,
      args: serde_json::Value::Null,
      restart: ChildRestart::Permanent,
      priority: Priority::Normal,
    }],
  };

  // start_linked should auto-wire the exit handler
  let sup = Supervisor::start_linked(&mut sched, spec).unwrap();
  let original_pid = sup.borrow().children[0].pid;

  // Send message to trigger crash
  sched
    .send(original_pid, Message::Text("crash".into()))
    .unwrap();
  sched.enqueue(original_pid);

  // tick() will: dispatch message -> Stop -> terminate_process -> exit_handler -> supervisor restart
  sched.tick();

  // Original child should be gone
  assert!(
    sched.registry.lookup(&original_pid).is_none(),
    "Original child should be terminated"
  );

  // Supervisor should have restarted the child automatically
  let sup_ref = sup.borrow();
  assert_eq!(
    sup_ref.children.len(),
    1,
    "Supervisor should still have 1 child after auto-restart"
  );
  assert_ne!(
    sup_ref.children[0].pid, original_pid,
    "Restarted child should have a new PID"
  );

  // New child should exist in registry
  let new_pid = sup_ref.children[0].pid;
  assert!(
    sched.registry.lookup(&new_pid).is_some(),
    "Auto-restarted child should exist in registry"
  );
}

// === Step 5: Receive timeout ===

#[test]
fn test_receive_timeout_wakes_waiting_process() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();

  // Process has no messages — goes to Waiting on first tick
  sched.enqueue(pid);
  sched.tick();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting
  );

  // Set an already-expired timeout (duration 0)
  sched
    .set_receive_timeout(pid, std::time::Duration::from_millis(0))
    .unwrap();

  // Next tick: detect expired timeout, deliver ReceiveTimeout,
  // wake process, dispatch the message
  let ran = sched.tick();
  assert!(ran, "tick should run work after timeout wakes process");

  // EchoBehavior stores received messages in state
  let proc = sched.registry.lookup(&pid).unwrap();
  let state = proc
    .state
    .as_ref()
    .unwrap()
    .as_any()
    .downcast_ref::<EchoState>()
    .unwrap();
  assert_eq!(state.received.len(), 1);
  assert!(
    matches!(
      &state.received[0],
      Message::System(SystemMsg::ReceiveTimeout)
    ),
    "Process should receive ReceiveTimeout message"
  );
}

#[test]
fn test_receive_timeout_canceled_by_real_message() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();

  // Process goes Waiting
  sched.enqueue(pid);
  sched.tick();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting
  );

  // Set timeout (won't expire yet — 10 seconds)
  sched
    .set_receive_timeout(pid, std::time::Duration::from_secs(10))
    .unwrap();

  // Deliver a real message — should cancel the timeout
  sched.send(pid, Message::Text("hello".into())).unwrap();

  // Process should have no receive_timeout after real message delivery
  let proc = sched.registry.lookup(&pid).unwrap();
  assert!(
    proc.receive_timeout.is_none(),
    "Real message delivery should cancel receive timeout"
  );
}

#[test]
fn test_receive_timeout_only_fires_when_waiting() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();

  // Give the process a message so it stays Runnable
  sched.send(pid, Message::Text("keep busy".into())).unwrap();

  // Set an expired timeout on a Runnable process
  sched
    .set_receive_timeout(pid, std::time::Duration::from_millis(0))
    .unwrap();

  // tick dispatches "keep busy", process goes Waiting (mailbox empty)
  sched.enqueue(pid);
  sched.tick();

  // The timeout should NOT have fired during this tick because the
  // process was Runnable when we checked timeouts (before dispatch)
  let proc = sched.registry.lookup(&pid).unwrap();
  let state = proc
    .state
    .as_ref()
    .unwrap()
    .as_any()
    .downcast_ref::<EchoState>()
    .unwrap();
  // Only "keep busy" was processed, no ReceiveTimeout
  assert_eq!(state.received.len(), 1);
  assert!(matches!(&state.received[0], Message::Text(_)));
}

#[test]
fn test_set_receive_timeout_errors_on_missing_pid() {
  let mut sched = AgentScheduler::new();
  let fake_pid = AgentPid::from_raw(0xDEAD);
  let result = sched.set_receive_timeout(fake_pid, std::time::Duration::from_secs(1));
  assert!(result.is_err(), "Should error for unknown PID");
}

#[test]
fn test_set_receive_timeout_errors_on_overflow() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();
  let result = sched.set_receive_timeout(pid, std::time::Duration::MAX);
  assert!(result.is_err(), "Should error on duration overflow");
}

// === Step 6: Real HTTP in bridge ===

#[derive(Debug)]
struct TestHttpRequest {
  method: String,
  path: String,
  headers: std::collections::HashMap<String, String>,
  body: Vec<u8>,
}

#[derive(Debug)]
struct TestHttpResponse {
  status: u16,
  body: String,
  headers: Vec<(String, String)>,
  delay: std::time::Duration,
}

impl TestHttpResponse {
  fn ok(body: impl Into<String>) -> Self {
    Self {
      status: 200,
      body: body.into(),
      headers: Vec::new(),
      delay: std::time::Duration::from_millis(0),
    }
  }
}

fn start_test_http_server<F>(
  expected_requests: usize,
  handler: F,
) -> (String, std::thread::JoinHandle<()>)
where
  F: Fn(TestHttpRequest) -> TestHttpResponse + Send + Sync + 'static,
{
  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  let addr = listener.local_addr().unwrap();
  let handler = Arc::new(handler);
  let join = std::thread::spawn(move || {
    for _ in 0..expected_requests {
      let (mut stream, _) = match listener.accept() {
        Ok(v) => v,
        Err(_) => break,
      };
      let req = read_http_request(&mut stream);
      let resp = handler(req);
      if !resp.delay.is_zero() {
        std::thread::sleep(resp.delay);
      }
      let _ = write_http_response(&mut stream, resp);
    }
  });
  (format!("http://{}", addr), join)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> TestHttpRequest {
  use std::io::Read;
  stream
    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
    .ok();

  let mut buf = Vec::new();
  let mut chunk = [0_u8; 1024];
  let mut header_end = None;
  let mut content_len = 0_usize;

  loop {
    match stream.read(&mut chunk) {
      Ok(0) => break,
      Ok(n) => {
        buf.extend_from_slice(&chunk[..n]);
        if header_end.is_none() {
          if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            let end = pos + 4;
            header_end = Some(end);
            content_len = parse_content_length(&buf[..end]);
          }
        }
        if let Some(end) = header_end {
          if buf.len() >= end + content_len {
            break;
          }
        }
      }
      Err(e)
        if e.kind() == std::io::ErrorKind::WouldBlock
          || e.kind() == std::io::ErrorKind::TimedOut =>
      {
        break
      }
      Err(_) => break,
    }
  }

  let end = header_end.unwrap_or(buf.len());
  let header_text = String::from_utf8_lossy(&buf[..end]);
  let mut lines = header_text.split("\r\n");
  let req_line = lines.next().unwrap_or("");
  let mut req_parts = req_line.split_whitespace();
  let method = req_parts.next().unwrap_or("").to_string();
  let path = req_parts.next().unwrap_or("/").to_string();

  let mut headers = std::collections::HashMap::new();
  for line in lines {
    if line.is_empty() {
      continue;
    }
    if let Some((k, v)) = line.split_once(':') {
      headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
  }

  let body_end = std::cmp::min(buf.len(), end + content_len);
  let body = if end < body_end {
    buf[end..body_end].to_vec()
  } else {
    Vec::new()
  };

  TestHttpRequest {
    method,
    path,
    headers,
    body,
  }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  if needle.is_empty() {
    return Some(0);
  }
  haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(headers: &[u8]) -> usize {
  let text = String::from_utf8_lossy(headers);
  for line in text.split("\r\n") {
    if let Some((k, v)) = line.split_once(':') {
      if k.trim().eq_ignore_ascii_case("content-length") {
        if let Ok(n) = v.trim().parse::<usize>() {
          return n;
        }
      }
    }
  }
  0
}

fn write_http_response(
  stream: &mut std::net::TcpStream,
  resp: TestHttpResponse,
) -> std::io::Result<()> {
  use std::io::Write;
  let status_text = match resp.status {
    200 => "OK",
    404 => "Not Found",
    _ => "OK",
  };
  let body_bytes = resp.body.as_bytes();
  let mut raw = format!(
    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
    resp.status,
    status_text,
    body_bytes.len()
  );
  for (k, v) in &resp.headers {
    raw.push_str(&format!("{}: {}\r\n", k, v));
  }
  raw.push_str("\r\n");

  stream.write_all(raw.as_bytes())?;
  stream.write_all(body_bytes)?;
  stream.flush()
}

/// A behavior that issues an HTTP GET via IoOp::HttpRequest
/// and stores the response.
struct HttpGetBehavior {
  url: String,
}

struct HttpGetState {
  response: Option<serde_json::Value>,
}

impl AgentState for HttpGetState {
  fn as_any(&self) -> &dyn std::any::Any {
    self
  }
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
    self
  }
}

impl AgentBehavior for HttpGetBehavior {
  fn init(&self, _args: serde_json::Value) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(HttpGetState { response: None }))
  }

  fn handle_message(&self, msg: Message, state: &mut dyn AgentState) -> Action {
    let s = state.as_any_mut().downcast_mut::<HttpGetState>().unwrap();
    match msg {
      Message::Text(_) => Action::IoRequest(IoOp::HttpRequest {
        method: "GET".into(),
        url: self.url.clone(),
        headers: std::collections::HashMap::new(),
        body: None,
        timeout_ms: Some(10_000),
      }),
      Message::System(SystemMsg::IoResponse { result, .. }) => {
        if let IoResult::Ok(val) = result {
          s.response = Some(val);
        }
        Action::Continue
      }
      _ => Action::Continue,
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

  fn terminate(&self, _reason: Reason, _state: &mut dyn AgentState) {}
}

#[tokio::test]
async fn test_bridge_http_get_success() {
  use crate::agent_rt::bridge::execute_io_op;
  use std::collections::HashMap;

  let (base_url, server) = start_test_http_server(1, |req| {
    assert_eq!(req.method, "GET");
    assert_eq!(req.path, "/get");
    TestHttpResponse::ok("{\"ok\":true}")
  });
  let op = IoOp::HttpRequest {
    method: "GET".into(),
    url: format!("{}/get", base_url),
    headers: HashMap::new(),
    body: None,
    timeout_ms: Some(2_000),
  };
  let result = execute_io_op(&op, None).await;
  server.join().unwrap();
  match &result {
    IoResult::Ok(val) => {
      assert_eq!(val["status"], 200);
      assert!(val["body"].is_string(), "body should be a string");
    }
    other => panic!("Expected IoResult::Ok, got {:?}", other),
  }
}

#[tokio::test]
async fn test_bridge_http_timeout() {
  use crate::agent_rt::bridge::execute_io_op;
  use std::collections::HashMap;

  let (base_url, server) = start_test_http_server(1, |_req| TestHttpResponse {
    status: 200,
    body: "late".into(),
    headers: Vec::new(),
    delay: std::time::Duration::from_millis(500),
  });
  let op = IoOp::HttpRequest {
    method: "GET".into(),
    url: format!("{}/slow", base_url),
    headers: HashMap::new(),
    body: None,
    timeout_ms: Some(100),
  };
  let result = execute_io_op(&op, None).await;
  server.join().unwrap();
  assert!(
    matches!(result, IoResult::Timeout),
    "Expected IoResult::Timeout for slow endpoint, got {:?}",
    result
  );
}

#[tokio::test]
async fn test_bridge_http_non_2xx() {
  use crate::agent_rt::bridge::execute_io_op;
  use std::collections::HashMap;

  let (base_url, server) = start_test_http_server(1, |_req| TestHttpResponse {
    status: 404,
    body: "missing".into(),
    headers: Vec::new(),
    delay: std::time::Duration::from_millis(0),
  });
  let op = IoOp::HttpRequest {
    method: "GET".into(),
    url: format!("{}/status/404", base_url),
    headers: HashMap::new(),
    body: None,
    timeout_ms: Some(2_000),
  };
  let result = execute_io_op(&op, None).await;
  server.join().unwrap();
  match &result {
    IoResult::Ok(val) => {
      assert_eq!(val["status"], 404);
    }
    other => panic!("Expected IoResult::Ok with status 404, got {:?}", other),
  }
}

#[tokio::test]
async fn test_bridge_http_post_with_body() {
  use crate::agent_rt::bridge::execute_io_op;
  use std::{collections::HashMap, sync::Mutex};

  let captured = Arc::new(Mutex::new(None::<TestHttpRequest>));
  let captured2 = Arc::clone(&captured);
  let (base_url, server) = start_test_http_server(1, move |req| {
    *captured2.lock().unwrap() = Some(req);
    TestHttpResponse::ok("{\"saved\":true}")
  });
  let mut headers = HashMap::new();
  headers.insert("Content-Type".into(), "application/json".into());

  let op = IoOp::HttpRequest {
    method: "POST".into(),
    url: format!("{}/post", base_url),
    headers,
    body: Some(b"{\"key\":\"value\"}".to_vec()),
    timeout_ms: Some(2_000),
  };
  let result = execute_io_op(&op, None).await;
  server.join().unwrap();

  let req = captured
    .lock()
    .unwrap()
    .take()
    .expect("server should capture request");
  assert_eq!(req.method, "POST");
  assert_eq!(req.path, "/post");
  assert_eq!(
    req.headers.get("content-type"),
    Some(&"application/json".to_string()),
  );
  assert_eq!(req.body, b"{\"key\":\"value\"}");
  match &result {
    IoResult::Ok(val) => {
      assert_eq!(val["status"], 200);
      assert!(val["body"].is_string(), "body should be present");
    }
    other => panic!("Expected IoResult::Ok, got {:?}", other),
  }
}

#[tokio::test]
async fn test_bridge_http_connection_error() {
  use crate::agent_rt::bridge::execute_io_op;
  use std::collections::HashMap;

  let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
  let addr = listener.local_addr().unwrap();
  drop(listener);
  let op = IoOp::HttpRequest {
    method: "GET".into(),
    url: format!("http://127.0.0.1:{}/unreachable", addr.port()),
    headers: HashMap::new(),
    body: None,
    timeout_ms: Some(500),
  };
  let result = execute_io_op(&op, None).await;
  assert!(
    matches!(result, IoResult::Error(_) | IoResult::Timeout),
    "Connection to unreachable host should error or timeout, got {:?}",
    result
  );
}

#[tokio::test]
async fn test_bridge_llm_openai_request_via_typed_op() {
  use crate::agent_rt::bridge::{execute_io_op_with_config, LlmConfig};

  let (base_url, server) = start_test_http_server(1, |req| {
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/chat/completions");
    assert_eq!(
      req.headers.get("authorization"),
      Some(&"Bearer test-openai-key".to_string()),
    );
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    TestHttpResponse::ok("{\"id\":\"cmpl-1\"}")
  });

  let cfg = LlmConfig {
    openai_api_key: Some("test-openai-key".to_string()),
    anthropic_api_key: None,
    openai_base_url: format!("{}/v1", base_url),
    anthropic_base_url: "http://127.0.0.1:1".to_string(),
  };
  let op = IoOp::LlmRequest {
    provider: "openai".to_string(),
    model: "gpt-4o-mini".to_string(),
    prompt: "summarize this".to_string(),
    system_prompt: Some("You are concise".to_string()),
    max_tokens: Some(128),
    temperature: Some(0.1),
    timeout_ms: Some(2_000),
  };
  let result = execute_io_op_with_config(&op, None, &cfg).await;
  server.join().unwrap();

  match result {
    IoResult::Ok(val) => {
      assert_eq!(val["status"], 200);
      assert!(val["body"].is_string());
    }
    other => panic!(
      "Expected IoResult::Ok for openai llm request, got {:?}",
      other
    ),
  }
}

#[tokio::test]
async fn test_bridge_llm_anthropic_request_via_typed_op() {
  use crate::agent_rt::bridge::{execute_io_op_with_config, LlmConfig};

  let (base_url, server) = start_test_http_server(1, |req| {
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(
      req.headers.get("x-api-key"),
      Some(&"test-anthropic-key".to_string()),
    );
    assert_eq!(
      req.headers.get("anthropic-version"),
      Some(&"2023-06-01".to_string()),
    );
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["model"], "claude-3-5-sonnet-latest");
    assert_eq!(body["messages"][0]["role"], "user");
    TestHttpResponse::ok("{\"id\":\"msg-1\"}")
  });

  let cfg = LlmConfig {
    openai_api_key: None,
    anthropic_api_key: Some("test-anthropic-key".to_string()),
    openai_base_url: "http://127.0.0.1:1".to_string(),
    anthropic_base_url: format!("{}/v1", base_url),
  };
  let op = IoOp::LlmRequest {
    provider: "anthropic".to_string(),
    model: "claude-3-5-sonnet-latest".to_string(),
    prompt: "draft plan".to_string(),
    system_prompt: None,
    max_tokens: Some(256),
    temperature: Some(0.2),
    timeout_ms: Some(2_000),
  };
  let result = execute_io_op_with_config(&op, None, &cfg).await;
  server.join().unwrap();

  match result {
    IoResult::Ok(val) => {
      assert_eq!(val["status"], 200);
      assert!(val["body"].is_string());
    }
    other => panic!(
      "Expected IoResult::Ok for anthropic llm request, got {:?}",
      other
    ),
  }
}

#[tokio::test]
async fn test_bridge_custom_llm_request_maps_to_http() {
  use crate::agent_rt::bridge::{execute_io_op_with_config, LlmConfig};

  let (base_url, server) = start_test_http_server(1, |req| {
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/chat/completions");
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["messages"][0]["content"], "legacy path prompt");
    TestHttpResponse::ok("{\"id\":\"legacy\"}")
  });

  let cfg = LlmConfig {
    openai_api_key: Some("test-openai-key".to_string()),
    anthropic_api_key: None,
    openai_base_url: format!("{}/v1", base_url),
    anthropic_base_url: "http://127.0.0.1:1".to_string(),
  };
  let op = IoOp::Custom {
    kind: "llm_request".to_string(),
    payload: serde_json::json!({
      "provider": "openai",
      "model": "gpt-4o-mini",
      "prompt": "legacy path prompt"
    }),
  };
  let result = execute_io_op_with_config(&op, None, &cfg).await;
  server.join().unwrap();

  match result {
    IoResult::Ok(val) => assert_eq!(val["status"], 200),
    other => panic!(
      "Expected IoResult::Ok for custom llm request mapping, got {:?}",
      other
    ),
  }
}

#[tokio::test]
async fn test_bridge_llm_missing_api_key_returns_error() {
  use crate::agent_rt::bridge::{execute_io_op_with_config, LlmConfig};

  let cfg = LlmConfig {
    openai_api_key: None,
    anthropic_api_key: None,
    openai_base_url: "http://127.0.0.1:1".to_string(),
    anthropic_base_url: "http://127.0.0.1:1".to_string(),
  };
  let op = IoOp::LlmRequest {
    provider: "openai".to_string(),
    model: "gpt-4o-mini".to_string(),
    prompt: "hello".to_string(),
    system_prompt: None,
    max_tokens: None,
    temperature: None,
    timeout_ms: Some(500),
  };
  let result = execute_io_op_with_config(&op, None, &cfg).await;
  match result {
    IoResult::Error(err) => {
      assert!(err.contains("OPENAI_API_KEY"), "unexpected error: {}", err);
    }
    other => panic!(
      "Expected IoResult::Error for missing api key, got {:?}",
      other
    ),
  }
}

#[tokio::test]
async fn test_scheduler_bridge_http_roundtrip() {
  let (base_url, server) = start_test_http_server(1, |req| {
    assert_eq!(req.method, "GET");
    assert_eq!(req.path, "/get");
    TestHttpResponse::ok("{\"from\":\"local\"}")
  });
  let (bridge_handle, worker) = crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);

  let worker_handle = tokio::spawn(async move { worker.run().await });

  let behavior = Arc::new(HttpGetBehavior {
    url: format!("{}/get", base_url),
  });
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("go".into())).unwrap();
  sched.enqueue(pid);

  // First tick: dispatches "go", behavior returns IoRequest(HttpRequest),
  // submitted to bridge, process goes Waiting.
  sched.tick();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting,
  );

  // Poll until IoResponse is delivered instead of using a fixed sleep.
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
  loop {
    sched.tick();
    let proc = sched.registry.lookup(&pid).unwrap();
    let state = proc
      .state
      .as_ref()
      .unwrap()
      .as_any()
      .downcast_ref::<HttpGetState>()
      .unwrap();
    if state.response.is_some() {
      break;
    }
    if std::time::Instant::now() >= deadline {
      panic!("Timed out waiting for scheduler bridge HTTP response");
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
  }

  let proc = sched.registry.lookup(&pid).unwrap();
  let state = proc
    .state
    .as_ref()
    .unwrap()
    .as_any()
    .downcast_ref::<HttpGetState>()
    .unwrap();
  assert!(
    state.response.is_some(),
    "Process should have received HTTP response"
  );
  let resp = state.response.as_ref().unwrap();
  assert_eq!(resp["status"], 200);
  assert!(resp["body"].is_string());

  server.join().unwrap();
  drop(sched);
  let _ = worker_handle.await;
}

#[test]
fn test_extract_provider_from_url() {
  use crate::agent_rt::bridge::extract_provider;

  assert_eq!(
    extract_provider("https://api.openai.com/v1/chat"),
    "api.openai.com"
  );
  assert_eq!(
    extract_provider("https://api.anthropic.com/v1/messages"),
    "api.anthropic.com"
  );
  assert_eq!(extract_provider("http://localhost:8080/api"), "localhost");
  assert_eq!(extract_provider("https://example.com"), "example.com");
}

#[tokio::test]
async fn test_bridge_rate_limiter_throttles_requests() {
  use crate::agent_rt::{bridge::execute_io_op, rate_limiter::RateLimiter};
  use std::collections::HashMap;

  let (base_url, server) =
    start_test_http_server(2, |_req| TestHttpResponse::ok("{\"ok\":true}"));
  let rl = RateLimiter::new();
  // Allow 1 request per 200ms to localhost provider
  rl.set_limit("127.0.0.1", 1, std::time::Duration::from_millis(200));

  let op = IoOp::HttpRequest {
    method: "GET".into(),
    url: format!("{}/get", base_url),
    headers: HashMap::new(),
    body: None,
    timeout_ms: Some(2_000),
  };

  // First request should complete quickly
  let start = std::time::Instant::now();
  let r1 = execute_io_op(&op, Some(&rl)).await;
  assert!(matches!(r1, IoResult::Ok(_)));

  // Second request should be delayed by ~200ms
  let r2 = execute_io_op(&op, Some(&rl)).await;
  assert!(matches!(r2, IoResult::Ok(_)));
  let elapsed = start.elapsed();
  assert!(
    elapsed >= std::time::Duration::from_millis(150),
    "Second request should have been rate-limited, total elapsed {:?}",
    elapsed
  );
  server.join().unwrap();
}

#[test]
fn test_scheduler_list_processes_reports_mailbox_depth() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();

  sched.send(pid, Message::Text("m1".into())).unwrap();
  sched.send(pid, Message::Text("m2".into())).unwrap();

  let processes = sched.list_processes();
  let snap = processes.iter().find(|p| p.pid == pid.raw()).unwrap();
  assert_eq!(snap.mailbox_depth, 2);
  assert_eq!(snap.link_count, 0);
  assert_eq!(snap.monitor_count, 0);
  assert_eq!(snap.monitored_by_count, 0);
  assert_eq!(snap.status, ProcessStatus::Runnable);
}

#[test]
fn test_scheduler_metrics_track_messages_and_termination() {
  let mut sched = AgentScheduler::new();
  let pid = sched
    .registry
    .spawn(Arc::new(EchoBehavior), serde_json::Value::Null)
    .unwrap();

  sched.send(pid, Message::Text("work".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();
  sched.terminate_process(pid, Reason::Shutdown);

  let metrics = sched.metrics_snapshot();
  assert_eq!(metrics.active_processes, 0);
  assert_eq!(metrics.messages_sent_total, 1);
  assert!(
    metrics.messages_processed_total >= 1,
    "expected processed messages to be tracked"
  );
  assert_eq!(metrics.process_terminations_total, 1);
}

#[tokio::test]
async fn test_bridge_records_io_metrics() {
  let mut sched = AgentScheduler::new();
  let metrics = sched.metrics();
  let (bridge_handle, worker) =
    crate::agent_rt::bridge::create_bridge_with_metrics(metrics);
  sched.set_bridge(bridge_handle);

  let worker_handle = tokio::spawn(async move { worker.run().await });

  let behavior = Arc::new(IoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched.send(pid, Message::Text("trigger".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
  loop {
    sched.tick();
    let snapshot = sched.metrics_snapshot();
    if snapshot.io_responses_total > 0 {
      assert!(snapshot.io_latency_samples > 0);
      assert!(snapshot.io_latency_total_ms > 0);
      break;
    }
    if std::time::Instant::now() >= deadline {
      panic!("timed out waiting for io metrics");
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
  }

  drop(sched);
  let _ = worker_handle.await;
}

#[tokio::test]
async fn test_graceful_shutdown_drains_inflight_io_and_terminates_processes() {
  let (bridge_handle, worker) = crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);
  let worker_handle = tokio::spawn(async move { worker.run().await });

  let pid = sched
    .registry
    .spawn(
      Arc::new(SingleTimerBehavior {
        duration: std::time::Duration::from_millis(80),
      }),
      serde_json::Value::Null,
    )
    .unwrap();
  sched.send(pid, Message::Text("go".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting
  );

  let start = std::time::Instant::now();
  sched.graceful_shutdown(std::time::Duration::from_secs(1));
  let elapsed = start.elapsed();
  assert!(
    elapsed >= std::time::Duration::from_millis(30),
    "shutdown should wait for in-flight io, elapsed {:?}",
    elapsed
  );
  assert_eq!(sched.registry.count(), 0);

  drop(sched);
  let _ = worker_handle.await;
}

#[tokio::test]
async fn test_graceful_shutdown_timeout_force_stops_bridge() {
  let (bridge_handle, worker) = crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);
  let worker_handle = tokio::spawn(async move { worker.run().await });

  let pid = sched
    .registry
    .spawn(
      Arc::new(SingleTimerBehavior {
        duration: std::time::Duration::from_millis(500),
      }),
      serde_json::Value::Null,
    )
    .unwrap();
  sched.send(pid, Message::Text("go".into())).unwrap();
  sched.enqueue(pid);
  sched.tick();

  let start = std::time::Instant::now();
  sched.graceful_shutdown(std::time::Duration::from_millis(20));
  let elapsed = start.elapsed();
  assert!(
    elapsed < std::time::Duration::from_millis(300),
    "shutdown should not block indefinitely, elapsed {:?}",
    elapsed
  );
  assert_eq!(sched.registry.count(), 0);

  drop(sched);
  let _ = worker_handle.await;
}

#[test]
fn test_agent_chat_io_op_fields() {
  let op = IoOp::AgentChat {
    provider: "anthropic".to_string(),
    model: Some("claude-3-5-sonnet-latest".to_string()),
    system_prompt: Some("You are a researcher.".to_string()),
    prompt: "Find recent papers on BEAM VM.".to_string(),
    tools: Some(vec!["web_search".to_string(), "web_fetch".to_string()]),
    max_iterations: Some(5),
    timeout_ms: Some(60_000),
  };
  match op {
    IoOp::AgentChat {
      provider,
      prompt,
      tools,
      ..
    } => {
      assert_eq!(provider, "anthropic");
      assert_eq!(prompt, "Find recent papers on BEAM VM.");
      assert_eq!(tools.unwrap().len(), 2);
    }
    _ => panic!("expected AgentChat"),
  }
}

#[test]
fn test_agent_destroy_io_op() {
  let pid = AgentPid::from_raw(0x8000_9999);
  let op = IoOp::AgentDestroy { target_pid: pid };
  match op {
    IoOp::AgentDestroy { target_pid } => {
      assert_eq!(target_pid.raw(), 0x8000_9999);
    }
    _ => panic!("expected AgentDestroy"),
  }
}
