use std::sync::Arc;

use crate::agent_rt::process::*;
use crate::agent_rt::registry::AgentRegistry;
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
