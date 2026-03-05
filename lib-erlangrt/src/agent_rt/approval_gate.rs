use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
  pub message: String,
  pub task: serde_json::Value,
  pub requested_at: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
  Approved,
  Rejected { reason: String },
}

pub struct ApprovalRegistry {
  pending: HashMap<(String, String), ApprovalRequest>,
  decisions: HashMap<(String, String), ApprovalDecision>,
}

impl ApprovalRegistry {
  pub fn new() -> Self {
    Self {
      pending: HashMap::new(),
      decisions: HashMap::new(),
    }
  }

  pub fn request_approval(
    &mut self,
    orch_id: &str,
    task_id: &str,
    message: &str,
    task: serde_json::Value,
  ) {
    let key = (orch_id.to_string(), task_id.to_string());
    self.pending.insert(
      key,
      ApprovalRequest {
        message: message.to_string(),
        task,
        requested_at: std::time::Instant::now(),
      },
    );
  }

  pub fn submit_decision(
    &mut self,
    orch_id: &str,
    task_id: &str,
    decision: ApprovalDecision,
  ) -> bool {
    let key = (orch_id.to_string(), task_id.to_string());
    if self.pending.remove(&key).is_some() {
      self.decisions.insert(key, decision);
      true
    } else {
      false
    }
  }

  pub fn take_decision(
    &mut self,
    orch_id: &str,
    task_id: &str,
  ) -> Option<ApprovalDecision> {
    let key = (orch_id.to_string(), task_id.to_string());
    self.decisions.remove(&key)
  }

  pub fn pending_requests(&self) -> Vec<(&str, &str, &ApprovalRequest)> {
    self
      .pending
      .iter()
      .map(|((oid, tid), req)| (oid.as_str(), tid.as_str(), req))
      .collect()
  }

  pub fn has_pending(&self, orch_id: &str, task_id: &str) -> bool {
    self
      .pending
      .contains_key(&(orch_id.to_string(), task_id.to_string()))
  }
}

pub type SharedApprovalRegistry = Arc<Mutex<ApprovalRegistry>>;

pub fn new_shared_registry() -> SharedApprovalRegistry {
  Arc::new(Mutex::new(ApprovalRegistry::new()))
}

pub fn task_requires_approval(task: &serde_json::Value) -> bool {
  task
    .get("requires_approval")
    .and_then(|v| v.as_bool())
    .unwrap_or(false)
}

pub fn approval_message(task: &serde_json::Value) -> String {
  task
    .get("approval_message")
    .and_then(|v| v.as_str())
    .unwrap_or("Approval required")
    .to_string()
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn test_request_and_approve() {
    let mut registry = ApprovalRegistry::new();
    registry.request_approval(
      "orch-1",
      "task-a",
      "Deploy?",
      json!({"task_id": "task-a"}),
    );
    assert!(registry.has_pending("orch-1", "task-a"));
    assert_eq!(registry.pending_requests().len(), 1);

    registry.submit_decision("orch-1", "task-a", ApprovalDecision::Approved);
    assert!(!registry.has_pending("orch-1", "task-a"));

    let decision = registry.take_decision("orch-1", "task-a");
    assert_eq!(decision, Some(ApprovalDecision::Approved));
    assert!(registry.take_decision("orch-1", "task-a").is_none());
  }

  #[test]
  fn test_reject() {
    let mut registry = ApprovalRegistry::new();
    registry.request_approval("o1", "t1", "OK?", json!({}));
    registry.submit_decision(
      "o1",
      "t1",
      ApprovalDecision::Rejected {
        reason: "not ready".into(),
      },
    );
    let decision = registry.take_decision("o1", "t1").unwrap();
    assert_eq!(
      decision,
      ApprovalDecision::Rejected {
        reason: "not ready".into()
      }
    );
  }

  #[test]
  fn test_submit_unknown_task_returns_false() {
    let mut registry = ApprovalRegistry::new();
    let ok = registry.submit_decision("o1", "unknown", ApprovalDecision::Approved);
    assert!(!ok);
  }

  #[test]
  fn test_task_requires_approval() {
    assert!(task_requires_approval(&json!({"requires_approval": true})));
    assert!(!task_requires_approval(
      &json!({"requires_approval": false})
    ));
    assert!(!task_requires_approval(&json!({"task_id": "t1"})));
  }

  #[test]
  fn test_approval_message_fallback() {
    assert_eq!(
      approval_message(&json!({"approval_message": "Ship it?"})),
      "Ship it?"
    );
    assert_eq!(approval_message(&json!({})), "Approval required");
  }

  #[test]
  fn test_shared_registry_thread_safety() {
    let registry = new_shared_registry();
    let r2 = registry.clone();

    {
      let mut reg = registry.lock().unwrap();
      reg.request_approval("o", "t", "msg", json!({}));
    }
    {
      let reg = r2.lock().unwrap();
      assert!(reg.has_pending("o", "t"));
    }
  }
}
