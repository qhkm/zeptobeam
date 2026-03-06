use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
};

use erlangrt::agent_rt::{
  mcp_types::ProcessInfo,
  server::McpRuntimeOps,
  types::{Message as LegacyMessage, Reason as LegacyReason},
};
use zeptovm::{
  error::Message as ZeptoMessage,
  mailbox::ProcessHandle,
  pid::Pid,
};

/// Runtime operations for zeptovm processes, implementing the
/// McpRuntimeOps trait so the HTTP/MCP server can query and
/// control DaemonV2 agent processes.
pub struct ZeptovmRuntimeOps {
  /// Map of agent name -> (Pid, ProcessHandle).
  processes: Arc<Mutex<HashMap<String, (Pid, ProcessHandle)>>>,
}

impl ZeptovmRuntimeOps {
  pub fn new() -> Self {
    Self {
      processes: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// Register a spawned agent process.
  pub fn register(
    &self,
    name: String,
    pid: Pid,
    handle: ProcessHandle,
  ) {
    let mut procs = self.processes.lock().unwrap();
    procs.insert(name, (pid, handle));
  }

  /// Remove a process by name (e.g. after it exits).
  pub fn remove(&self, name: &str) {
    let mut procs = self.processes.lock().unwrap();
    procs.remove(name);
  }
}

impl McpRuntimeOps for ZeptovmRuntimeOps {
  fn list_processes(&self) -> Result<Vec<ProcessInfo>, String> {
    let procs =
      self.processes.lock().map_err(|e| format!("lock: {e}"))?;
    let mut infos = Vec::new();
    for (name, (pid, _handle)) in procs.iter() {
      infos.push(ProcessInfo {
        pid: pid.raw(),
        status: "running".into(),
        priority: format!("normal ({})", name),
        mailbox_depth: 0,
      });
    }
    Ok(infos)
  }

  fn send_message(
    &self,
    pid_raw: u64,
    message: LegacyMessage,
  ) -> Result<(), String> {
    let procs =
      self.processes.lock().map_err(|e| format!("lock: {e}"))?;
    let handle = procs
      .values()
      .find(|(pid, _)| pid.raw() == pid_raw)
      .map(|(_, h)| h.clone())
      .ok_or_else(|| format!("process {} not found", pid_raw))?;

    let zepto_msg = convert_legacy_message(message)?;
    handle
      .try_send_user(zepto_msg)
      .map_err(|e| format!("send failed: {e}"))
  }

  fn cancel_process(
    &self,
    pid_raw: u64,
    _reason: LegacyReason,
  ) -> Result<(), String> {
    let procs =
      self.processes.lock().map_err(|e| format!("lock: {e}"))?;
    let handle = procs
      .values()
      .find(|(pid, _)| pid.raw() == pid_raw)
      .map(|(_, h)| h.clone())
      .ok_or_else(|| format!("process {} not found", pid_raw))?;

    handle.kill();
    Ok(())
  }
}

/// Convert a legacy agent_rt Message to a zeptovm Message.
fn convert_legacy_message(
  msg: LegacyMessage,
) -> Result<ZeptoMessage, String> {
  match msg {
    LegacyMessage::Text(t) => Ok(ZeptoMessage::text(t)),
    LegacyMessage::Json(v) => Ok(ZeptoMessage::json(v)),
    LegacyMessage::System(_) => {
      Err("system messages cannot be sent via MCP".into())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use zeptovm::mailbox::create_mailbox;

  #[test]
  fn test_list_empty() {
    let ops = ZeptovmRuntimeOps::new();
    let result = ops.list_processes().unwrap();
    assert!(result.is_empty());
  }

  #[test]
  fn test_register_and_list() {
    let ops = ZeptovmRuntimeOps::new();
    let (handle, _mailbox) = create_mailbox(16, 32);
    let pid = Pid::from_raw(100);

    ops.register("test-agent".into(), pid, handle);
    let result = ops.list_processes().unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].pid, 100);
    assert_eq!(result[0].status, "running");
  }

  #[test]
  fn test_send_message_to_process() {
    let ops = ZeptovmRuntimeOps::new();
    let (handle, mut mailbox) = create_mailbox(16, 32);
    let pid = Pid::from_raw(200);

    ops.register("msg-agent".into(), pid, handle);

    let legacy_msg = LegacyMessage::Text("hello from MCP".into());
    ops.send_message(200, legacy_msg).unwrap();

    // Verify the message arrived in the mailbox
    let received = mailbox.user_rx.try_recv().unwrap();
    assert!(matches!(
      received,
      ZeptoMessage::User(zeptovm::error::UserPayload::Text(ref t))
        if t == "hello from MCP"
    ));
  }

  #[test]
  fn test_send_message_not_found() {
    let ops = ZeptovmRuntimeOps::new();
    let result = ops.send_message(
      999,
      LegacyMessage::Text("lost".into()),
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
  }

  #[test]
  fn test_cancel_kills_process() {
    let ops = ZeptovmRuntimeOps::new();
    let (handle, _mailbox) = create_mailbox(16, 32);
    let pid = Pid::from_raw(300);
    let kill_token = handle.kill_token.clone();

    ops.register("cancel-agent".into(), pid, handle);
    assert!(!kill_token.is_cancelled());

    ops
      .cancel_process(300, LegacyReason::Shutdown)
      .unwrap();
    assert!(kill_token.is_cancelled());
  }

  #[test]
  fn test_cancel_not_found() {
    let ops = ZeptovmRuntimeOps::new();
    let result = ops.cancel_process(999, LegacyReason::Shutdown);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
  }

  #[test]
  fn test_remove_then_list() {
    let ops = ZeptovmRuntimeOps::new();
    let (handle, _mailbox) = create_mailbox(16, 32);
    let pid = Pid::from_raw(400);

    ops.register("ephemeral".into(), pid, handle);
    assert_eq!(ops.list_processes().unwrap().len(), 1);

    ops.remove("ephemeral");
    assert!(ops.list_processes().unwrap().is_empty());
  }

  #[test]
  fn test_send_json_message() {
    let ops = ZeptovmRuntimeOps::new();
    let (handle, mut mailbox) = create_mailbox(16, 32);
    let pid = Pid::from_raw(500);

    ops.register("json-agent".into(), pid, handle);

    let json_msg = LegacyMessage::Json(
      serde_json::json!({"action": "ping"}),
    );
    ops.send_message(500, json_msg).unwrap();

    let received = mailbox.user_rx.try_recv().unwrap();
    assert!(matches!(
      received,
      ZeptoMessage::User(zeptovm::error::UserPayload::Json(ref v))
        if v["action"] == "ping"
    ));
  }
}
