use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::core::effect::{EffectId, EffectKind};
use crate::pid::Pid;

/// A pending human approval or input request.
#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
  pub effect_id: EffectId,
  pub pid: Pid,
  pub kind: EffectKind,
  pub description: String,
  pub input: serde_json::Value,
  pub created_at_ms: u64,
  pub expires_at_ms: u64,
}

/// Thread-safe store for pending approval requests.
#[derive(Clone)]
pub struct ApprovalStore {
  inner: Arc<Mutex<HashMap<u64, PendingApproval>>>,
}

impl ApprovalStore {
  pub fn new() -> Self {
    Self {
      inner: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// Insert a pending approval. Keyed by effect_id.raw().
  pub fn insert(&self, approval: PendingApproval) {
    let key = approval.effect_id.raw();
    let mut map =
      self.inner.lock().expect("approval store lock");
    map.insert(key, approval);
  }

  /// Remove and return a pending approval (resolve it).
  pub fn resolve(
    &self,
    effect_id: u64,
  ) -> Option<PendingApproval> {
    let mut map =
      self.inner.lock().expect("approval store lock");
    map.remove(&effect_id)
  }

  /// List all pending approvals (snapshot).
  pub fn list(&self) -> Vec<PendingApproval> {
    let map =
      self.inner.lock().expect("approval store lock");
    map.values().cloned().collect()
  }

  /// Check if an approval is still pending.
  pub fn is_pending(&self, effect_id: u64) -> bool {
    let map =
      self.inner.lock().expect("approval store lock");
    map.contains_key(&effect_id)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_approval(
    effect_id: u64,
    kind: EffectKind,
  ) -> PendingApproval {
    PendingApproval {
      effect_id: EffectId::from_raw(effect_id),
      pid: Pid::from_raw(1),
      kind,
      description: "test approval".into(),
      input: serde_json::json!({}),
      created_at_ms: 1000,
      expires_at_ms: 2000,
    }
  }

  #[test]
  fn test_approval_store_insert_and_resolve() {
    let store = ApprovalStore::new();
    let approval =
      make_approval(42, EffectKind::HumanApproval);
    store.insert(approval);

    assert!(store.is_pending(42));
    assert_eq!(store.list().len(), 1);

    let resolved = store.resolve(42);
    assert!(resolved.is_some());
    assert_eq!(resolved.unwrap().effect_id.raw(), 42);

    assert!(!store.is_pending(42));
    assert_eq!(store.list().len(), 0);
  }

  #[test]
  fn test_approval_store_double_resolve() {
    let store = ApprovalStore::new();
    store.insert(
      make_approval(42, EffectKind::HumanApproval),
    );

    let first = store.resolve(42);
    assert!(first.is_some());

    let second = store.resolve(42);
    assert!(second.is_none());
  }

  #[test]
  fn test_approval_store_list_multiple() {
    let store = ApprovalStore::new();
    store.insert(
      make_approval(1, EffectKind::HumanApproval),
    );
    store.insert(
      make_approval(2, EffectKind::HumanInput),
    );
    store.insert(
      make_approval(3, EffectKind::HumanApproval),
    );

    assert_eq!(store.list().len(), 3);

    store.resolve(2);
    assert_eq!(store.list().len(), 2);
  }

  #[test]
  fn test_approval_store_resolve_nonexistent() {
    let store = ApprovalStore::new();
    assert!(store.resolve(999).is_none());
    assert!(!store.is_pending(999));
  }
}
