use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::agent_rt::process::*;
use crate::agent_rt::registry::AgentRegistry;
use crate::agent_rt::scheduler::AgentScheduler;
use crate::agent_rt::types::*;

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
  fn init(
    &self,
    _args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(EchoState {
      received: Vec::new(),
    }))
  }

  fn handle_message(
    &self,
    msg: Message,
    state: &mut dyn AgentState,
  ) -> Action {
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

  fn terminate(
    &self,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) {
  }
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
  fn init(
    &self,
    _args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(ExitAwareState {
      message_count: 0,
      exit_count: 0,
    }))
  }

  fn handle_message(
    &self,
    _msg: Message,
    state: &mut dyn AgentState,
  ) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<ExitAwareState>()
      .unwrap();
    s.message_count += 1;
    Action::Continue
  }

  fn handle_exit(
    &self,
    _from: AgentPid,
    _reason: Reason,
    state: &mut dyn AgentState,
  ) -> Action {
    let s = state
      .as_any_mut()
      .downcast_mut::<ExitAwareState>()
      .unwrap();
    s.exit_count += 1;
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
  let proc =
    AgentProcess::new(behavior, serde_json::Value::Null).unwrap();
  assert_eq!(proc.status, ProcessStatus::Runnable);
  assert!(!proc.has_messages());
  assert!(proc.links.is_empty());
  assert!(!proc.trap_exit);
}

#[test]
fn test_deliver_message() {
  let behavior = Arc::new(EchoBehavior);
  let mut proc =
    AgentProcess::new(behavior, serde_json::Value::Null).unwrap();
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
  let mut proc =
    AgentProcess::new(behavior, serde_json::Value::Null).unwrap();
  proc.status = ProcessStatus::Waiting;

  proc.deliver_message(Message::Text("wake up".into()));
  assert_eq!(proc.status, ProcessStatus::Runnable);
}

#[test]
fn test_handle_message_stores_in_state() {
  let behavior = Arc::new(EchoBehavior);
  let mut proc =
    AgentProcess::new(behavior.clone(), serde_json::Value::Null)
      .unwrap();

  let msg = Message::Text("test".into());
  let state = proc.state.as_mut().unwrap();
  let _action = behavior.handle_message(msg, state.as_mut());

  let echo_state =
    state.as_any().downcast_ref::<EchoState>().unwrap();
  assert_eq!(echo_state.received.len(), 1);
}

#[test]
fn test_message_ordering_fifo() {
  let behavior = Arc::new(EchoBehavior);
  let mut proc =
    AgentProcess::new(behavior, serde_json::Value::Null).unwrap();

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
  let pid =
    reg.spawn(behavior, serde_json::Value::Null).unwrap();
  assert!(
    pid.raw() >= 0x8000_0000,
    "Spawned pid should be in the agent range"
  );
}

#[test]
fn test_registry_lookup_existing() {
  let mut reg = AgentRegistry::new();
  let behavior = Arc::new(EchoBehavior);
  let pid =
    reg.spawn(behavior, serde_json::Value::Null).unwrap();
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
  let pid =
    reg.spawn(behavior, serde_json::Value::Null).unwrap();
  assert!(reg.lookup(&pid).is_some());

  let removed = reg.remove(&pid);
  assert!(removed.is_some());
  assert!(reg.lookup(&pid).is_none());
}

#[test]
fn test_registry_register_name() {
  let mut reg = AgentRegistry::new();
  let behavior = Arc::new(EchoBehavior);
  let pid =
    reg.spawn(behavior, serde_json::Value::Null).unwrap();

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
  sched
    .send(pid, Message::Text("hello".into()))
    .unwrap();
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
    .spawn(
      behavior.clone(),
      serde_json::Value::Null,
    )
    .unwrap();
  if let Some(proc) =
    sched.registry.lookup_mut(&waiting_pid)
  {
    proc.status = ProcessStatus::Waiting;
  }
  sched.enqueue(waiting_pid);

  let runnable_pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched
    .send(
      runnable_pid,
      Message::Text("run".into()),
    )
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

  let seen = Rc::new(RefCell::new(None::<(
    u64,
    String,
  )>));
  let seen_closure = Rc::clone(&seen);
  sched.set_exit_handler(move |_sched, exited, reason| {
    let reason_label = match reason {
      Reason::Normal => "normal".to_string(),
      Reason::Shutdown => "shutdown".to_string(),
      Reason::Custom(s) => s,
    };
    *seen_closure.borrow_mut() =
      Some((exited.raw(), reason_label));
  });

  sched.terminate_process(
    pid,
    Reason::Custom("boom".into()),
  );

  let seen = seen.borrow();
  let (seen_pid, seen_reason) =
    seen.as_ref().expect("exit handler should be called");
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
      .send(
        pid,
        Message::Text(format!("msg_{}", i)),
      )
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

  sched
    .send(pid, Message::Text("wake".into()))
    .unwrap();
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
    .spawn(
      behavior.clone(),
      serde_json::Value::Null,
    )
    .unwrap();
  if let Some(p) =
    sched.registry.lookup_mut(&low_pid)
  {
    p.priority = Priority::Low;
  }

  let high_pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  if let Some(p) =
    sched.registry.lookup_mut(&high_pid)
  {
    p.priority = Priority::High;
  }

  // Send messages to both
  sched
    .send(
      low_pid,
      Message::Text("low_msg".into()),
    )
    .unwrap();
  sched
    .send(
      high_pid,
      Message::Text("high_msg".into()),
    )
    .unwrap();

  // Enqueue low first, then high
  sched.enqueue(low_pid);
  sched.enqueue(high_pid);

  // Tick should run high_pid first
  sched.tick();

  // high_pid should have consumed its message
  // (now Waiting with empty mailbox)
  let high_proc =
    sched.registry.lookup(&high_pid).unwrap();
  assert_eq!(
    high_proc.status,
    ProcessStatus::Waiting,
    "High priority should have run first"
  );
  assert!(!high_proc.has_messages());

  // low_pid should still have its message
  let low_proc =
    sched.registry.lookup(&low_pid).unwrap();
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
    .spawn(
      behavior.clone(),
      serde_json::Value::Null,
    )
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, set trap_exit so b receives
  // the Exit as a message instead of cascading
  if let Some(proc_a) =
    sched.registry.lookup_mut(&pid_a)
  {
    proc_a.links.push(pid_b);
  }
  if let Some(proc_b) =
    sched.registry.lookup_mut(&pid_b)
  {
    proc_b.trap_exit = true;
  }

  // Terminate a, b should get Exit message
  sched.terminate_process(pid_a, Reason::Normal);

  let proc_b =
    sched.registry.lookup(&pid_b).unwrap();
  assert!(
    proc_b.has_messages(),
    "Linked process should receive Exit message"
  );
}

#[test]
fn test_scheduler_send_to_missing_process_errors() {
  let mut sched = AgentScheduler::new();
  let fake_pid = AgentPid::new();
  let result = sched
    .send(fake_pid, Message::Text("oops".into()));
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
      .send(
        pid,
        Message::Text(format!("msg_{}", i)),
      )
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
  let (_handle, _worker) =
    crate::agent_rt::bridge::create_bridge();
}

#[test]
fn test_bridge_submit_returns_correlation_id() {
  let (mut handle, _worker) =
    crate::agent_rt::bridge::create_bridge();
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
  let (handle, _worker) =
    crate::agent_rt::bridge::create_bridge();
  assert!(handle.drain_responses().is_empty());
}

#[tokio::test]
async fn test_bridge_roundtrip_timer() {
  let (mut handle, worker) =
    crate::agent_rt::bridge::create_bridge();
  let pid = AgentPid::new();

  let worker_handle =
    tokio::spawn(async move { worker.run().await });

  let _corr_id = handle
    .submit(
      pid,
      IoOp::Timer {
        duration: std::time::Duration::from_millis(10),
      },
    )
    .unwrap();

  // Wait for response
  tokio::time::sleep(
    std::time::Duration::from_millis(200),
  )
  .await;

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
  let sup = Supervisor::start(&mut sched, spec)
    .unwrap();
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
  let mut sup = Supervisor::start(&mut sched, spec)
    .unwrap();
  let old_pid = sup.children[0].pid;

  // Simulate crash
  sup.handle_child_exit(
    &mut sched,
    old_pid,
    Reason::Custom("crash".into()),
  );

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
  let mut sup = Supervisor::start(&mut sched, spec)
    .unwrap();
  let pid = sup.children[0].pid;

  sup.handle_child_exit(
    &mut sched,
    pid,
    Reason::Normal,
  );
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
  let mut sup = Supervisor::start(&mut sched, spec)
    .unwrap();
  let pid = sup.children[0].pid;

  sup.handle_child_exit(
    &mut sched,
    pid,
    Reason::Custom("crash".into()),
  );
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
  let mut sup = Supervisor::start(&mut sched, spec)
    .unwrap();

  // Crash 3 times — exceeds max_restarts of 2
  for _ in 0..3 {
    if let Some(child) = sup.children.first() {
      let pid = child.pid;
      sup.handle_child_exit(
        &mut sched,
        pid,
        Reason::Custom("crash".into()),
      );
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
  let mut sup = Supervisor::start(&mut sched, spec)
    .unwrap();
  let old_pids: Vec<_> =
    sup.children.iter().map(|c| c.pid).collect();

  // Kill first child
  sup.handle_child_exit(
    &mut sched,
    old_pids[0],
    Reason::Custom("crash".into()),
  );

  // Both should be restarted with new PIDs
  assert_eq!(sup.children.len(), 2);
  let new_pids: Vec<_> =
    sup.children.iter().map(|c| c.pid).collect();
  assert!(
    new_pids.iter().all(|p| !old_pids.contains(p))
  );
}

// --- Bug C1: trap_exit cascading death tests ---

#[test]
fn test_link_cascades_death_without_trap_exit() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let pid_a = sched
    .registry
    .spawn(
      behavior.clone(),
      serde_json::Value::Null,
    )
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, both have trap_exit=false
  if let Some(proc_a) =
    sched.registry.lookup_mut(&pid_a)
  {
    proc_a.links.push(pid_b);
  }

  // Kill a with a non-normal reason
  sched.terminate_process(
    pid_a,
    Reason::Custom("crash".into()),
  );

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
    .spawn(
      behavior.clone(),
      serde_json::Value::Null,
    )
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, both have trap_exit=false
  if let Some(proc_a) =
    sched.registry.lookup_mut(&pid_a)
  {
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
  let proc_b =
    sched.registry.lookup(&pid_b).unwrap();
  assert!(
    !proc_b.has_messages(),
    "No message delivered for normal exit \
     without trap_exit"
  );
}

#[test]
fn test_trap_exit_delivers_message_instead_of_cascade()
{
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(EchoBehavior);

  let pid_a = sched
    .registry
    .spawn(
      behavior.clone(),
      serde_json::Value::Null,
    )
    .unwrap();
  let pid_b = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();

  // Link a -> b, set trap_exit on b
  if let Some(proc_a) =
    sched.registry.lookup_mut(&pid_a)
  {
    proc_a.links.push(pid_b);
  }
  if let Some(proc_b) =
    sched.registry.lookup_mut(&pid_b)
  {
    proc_b.trap_exit = true;
  }

  // Kill a with crash reason
  sched.terminate_process(
    pid_a,
    Reason::Custom("crash".into()),
  );

  // a is gone
  assert!(sched.registry.lookup(&pid_a).is_none());

  // b should still be alive with an Exit message
  let proc_b =
    sched.registry.lookup(&pid_b).unwrap();
  assert!(
    proc_b.has_messages(),
    "trap_exit process should receive Exit msg"
  );
  // Verify it's an Exit system message
  let proc_b =
    sched.registry.lookup_mut(&pid_b).unwrap();
  let msg = proc_b.next_message().unwrap();
  match msg {
    Message::System(SystemMsg::Exit {
      from,
      reason,
    }) => {
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
  fn init(
    &self,
    _args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SendState))
  }

  fn handle_message(
    &self,
    _msg: Message,
    _state: &mut dyn AgentState,
  ) -> Action {
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

  fn terminate(
    &self,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) {
  }
}

#[test]
fn test_send_action_costs_reductions() {
  // Create a target process to receive sends
  let mut sched = AgentScheduler::new();
  let echo = Arc::new(EchoBehavior);
  let target = sched
    .registry
    .spawn(echo, serde_json::Value::Null)
    .unwrap();

  // Create a sender that returns Send on every msg
  let sender_behavior =
    Arc::new(SendBehavior { target });
  let sender = sched
    .registry
    .spawn(sender_behavior, serde_json::Value::Null)
    .unwrap();

  // Give sender 10 messages
  for i in 0..10 {
    sched
      .send(
        sender,
        Message::Text(format!("m{}", i)),
      )
      .unwrap();
  }
  sched.enqueue(sender);
  sched.tick();

  // Each message costs 1 (dispatch) + 2 (send) = 3
  // reductions. With 200 budget, can process
  // floor(200/3) = 66 messages. All 10 should be
  // consumed. Check that reductions were actually
  // deducted: 200 - 10*3 = 170.
  let proc =
    sched.registry.lookup(&sender).unwrap();
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
  let mut sup =
    Supervisor::start(&mut sched, spec).unwrap();
  let pid = sup.children[0].pid;

  sup.handle_child_exit(
    &mut sched,
    pid,
    Reason::Shutdown,
  );
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
  let mut sup =
    Supervisor::start(&mut sched, spec).unwrap();

  let pid_a = sup.children[0].pid;
  let pid_b = sup.children[1].pid;
  let pid_c = sup.children[2].pid;

  // Kill child b — should restart b and c, leave a
  sup.handle_child_exit(
    &mut sched,
    pid_b,
    Reason::Custom("crash".into()),
  );

  assert_eq!(sup.children.len(), 3);
  // a should keep same PID
  assert_eq!(
    sup.children[0].pid, pid_a,
    "Child a should be unchanged"
  );
  // b and c should have new PIDs
  assert_ne!(
    sup.children[1].pid, pid_b,
    "Child b should be restarted"
  );
  assert_ne!(
    sup.children[2].pid, pid_c,
    "Child c should be restarted"
  );
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
  fn init(
    &self,
    _args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(IoState))
  }
  fn handle_message(
    &self,
    _msg: Message,
    _state: &mut dyn AgentState,
  ) -> Action {
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
  fn terminate(
    &self,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) {
  }
}

#[test]
fn test_scheduler_with_bridge_submits_io_request() {
  let (bridge_handle, _worker) =
    crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);

  let behavior = Arc::new(IoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched
    .send(pid, Message::Text("trigger".into()))
    .unwrap();
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
fn test_scheduler_without_bridge_io_still_waits() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(IoBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched
    .send(pid, Message::Text("trigger".into()))
    .unwrap();
  sched.enqueue(pid);
  sched.tick();

  let proc = sched.registry.lookup(&pid).unwrap();
  assert_eq!(
    proc.status,
    ProcessStatus::Waiting,
    "Without bridge, IoRequest still sets Waiting"
  );
}

#[tokio::test]
async fn test_scheduler_bridge_roundtrip() {
  let (bridge_handle, worker) =
    crate::agent_rt::bridge::create_bridge();
  let mut sched = AgentScheduler::new();
  sched.set_bridge(bridge_handle);

  let worker_handle =
    tokio::spawn(async move { worker.run().await });

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
  sched
    .send(pid, Message::Text("trigger".into()))
    .unwrap();
  sched.enqueue(pid);

  // First tick: dispatches message, gets IoRequest,
  // submits to bridge, process goes Waiting.
  sched.tick();
  assert_eq!(
    sched.registry.lookup(&pid).unwrap().status,
    ProcessStatus::Waiting,
  );

  // Wait for Tokio to process the timer
  tokio::time::sleep(
    std::time::Duration::from_millis(200),
  )
  .await;

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
  fn init(
    &self,
    _args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(StopState))
  }
  fn handle_message(
    &self,
    _msg: Message,
    _state: &mut dyn AgentState,
  ) -> Action {
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
  fn terminate(
    &self,
    _reason: Reason,
    _state: &mut dyn AgentState,
  ) {
  }
}

#[test]
fn test_scheduler_stop_terminates_process() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(StopBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  sched
    .send(pid, Message::Text("die".into()))
    .unwrap();
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
  fn init(
    &self,
    _args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason> {
    Ok(Box::new(SpawnState))
  }
  fn handle_message(
    &self,
    _msg: Message,
    _state: &mut dyn AgentState,
  ) -> Action {
    Action::Spawn {
      behavior: Arc::new(EchoBehavior),
      args: serde_json::Value::Null,
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
fn test_scheduler_spawn_creates_child() {
  let mut sched = AgentScheduler::new();
  let behavior = Arc::new(SpawnBehavior);
  let pid = sched
    .registry
    .spawn(behavior, serde_json::Value::Null)
    .unwrap();
  assert_eq!(sched.registry.count(), 1);

  sched
    .send(pid, Message::Text("spawn".into()))
    .unwrap();
  sched.enqueue(pid);
  sched.tick();

  assert_eq!(
    sched.registry.count(),
    2,
    "Spawn action should create a new child process"
  );
}

// --- S4: register_name rejects duplicates ---

#[test]
fn test_register_name_rejects_duplicate() {
  let mut reg = AgentRegistry::new();
  let b1 = Arc::new(EchoBehavior);
  let b2 = Arc::new(EchoBehavior);
  let pid1 =
    reg.spawn(b1, serde_json::Value::Null).unwrap();
  let pid2 =
    reg.spawn(b2, serde_json::Value::Null).unwrap();

  reg.register_name("server", pid1).unwrap();

  // Different pid with same name should fail
  let result = reg.register_name("server", pid2);
  assert!(
    result.is_err(),
    "Should reject duplicate name registration"
  );

  // Original registration should be unchanged
  assert_eq!(reg.lookup_name("server"), Some(pid1));
}
