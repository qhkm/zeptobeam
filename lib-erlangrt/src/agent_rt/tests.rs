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

  reg.register_name("echo_server", pid);
  let found = reg.lookup_name("echo_server");
  assert_eq!(found, Some(pid));

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

  // Link a -> b
  if let Some(proc_a) =
    sched.registry.lookup_mut(&pid_a)
  {
    proc_a.links.push(pid_b);
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
