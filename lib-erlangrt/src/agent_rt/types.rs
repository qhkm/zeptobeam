use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

static NEXT_AGENT_PID: AtomicU64 = AtomicU64::new(0x8000_0000);

/// Unique process identifier in the agent runtime.
/// PIDs start at 0x8000_0000 to avoid collision with
/// BEAM PIDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentPid(u64);

impl AgentPid {
  pub fn new() -> Self {
    Self(NEXT_AGENT_PID.fetch_add(1, Ordering::Relaxed))
  }

  pub fn from_raw(raw: u64) -> Self {
    Self(raw)
  }

  pub fn raw(&self) -> u64 {
    self.0
  }
}

impl Default for AgentPid {
  fn default() -> Self {
    Self::new()
  }
}

/// Messages passed between agent processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
  Text(String),
  Json(serde_json::Value),
  System(SystemMsg),
}

/// System-level messages (exit signals, I/O responses).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SystemMsg {
  Exit { from: u64, reason: Reason },
  IoResponse { correlation_id: u64, result: IoResult },
}

/// Reason for process termination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reason {
  Normal,
  Shutdown,
  Custom(String),
}

/// Result of an asynchronous I/O operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IoResult {
  Ok(serde_json::Value),
  Error(String),
  Timeout,
}

/// Action returned by a behavior's message handler
/// to direct the scheduler.
pub enum Action {
  Continue,
  Send { to: AgentPid, msg: Message },
  IoRequest(IoOp),
  Spawn {
    behavior: Arc<dyn AgentBehavior>,
    args: serde_json::Value,
  },
  Stop(Reason),
}

/// Asynchronous I/O operation submitted to the bridge.
#[derive(Debug)]
pub enum IoOp {
  HttpRequest {
    method: String,
    url: String,
    body: Option<Vec<u8>>,
  },
  Timer {
    duration: Duration,
  },
}

/// Defines the callbacks for an agent process.
/// Implementations must be Send + Sync for
/// cross-thread sharing via Arc.
pub trait AgentBehavior: Send + Sync {
  fn init(
    &self,
    args: serde_json::Value,
  ) -> Result<Box<dyn AgentState>, Reason>;
  fn handle_message(
    &self,
    msg: Message,
    state: &mut dyn AgentState,
  ) -> Action;
  fn handle_exit(
    &self,
    from: AgentPid,
    reason: Reason,
    state: &mut dyn AgentState,
  ) -> Action;
  fn terminate(
    &self,
    reason: Reason,
    state: &mut dyn AgentState,
  );
}

/// Mutable state owned by an agent process.
pub trait AgentState: Send {
  fn as_any(&self) -> &dyn std::any::Any;
  fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}
