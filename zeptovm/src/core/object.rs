use serde::{Deserialize, Serialize};

/// Unique identifier for a stored artifact.
/// Values are assigned by the artifact store's database,
/// not by an in-process counter — this ensures durability
/// across restarts.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, Hash,
  Serialize, Deserialize,
)]
pub struct ObjectId(u64);

impl ObjectId {
  /// Wrap a raw database-assigned ID.
  pub fn from_raw(id: u64) -> Self {
    Self(id)
  }

  pub fn raw(&self) -> u64 {
    self.0
  }
}

/// Metadata handle for a stored artifact.
/// Lightweight — passes in messages without the payload.
#[derive(
  Debug, Clone, PartialEq, Serialize, Deserialize,
)]
pub struct ObjectRef {
  pub object_id: ObjectId,
  pub content_hash: String,
  pub media_type: String,
  pub size_bytes: u64,
  pub created_at_ms: u64,
  pub ttl_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_object_id_from_raw() {
    let id = ObjectId::from_raw(42);
    assert_eq!(id.raw(), 42);
  }

  #[test]
  fn test_object_id_equality() {
    let a = ObjectId::from_raw(1);
    let b = ObjectId::from_raw(1);
    let c = ObjectId::from_raw(2);
    assert_eq!(a, b);
    assert_ne!(a, c);
  }

  #[test]
  fn test_object_ref_serializable() {
    let r = ObjectRef {
      object_id: ObjectId::from_raw(1),
      content_hash: "abc123".into(),
      media_type: "text/plain".into(),
      size_bytes: 100,
      created_at_ms: 1000,
      ttl_ms: Some(5000),
    };
    let json = serde_json::to_string(&r).unwrap();
    let deser: ObjectRef =
      serde_json::from_str(&json).unwrap();
    assert_eq!(deser, r);
  }

  #[test]
  fn test_object_ref_no_ttl() {
    let r = ObjectRef {
      object_id: ObjectId::from_raw(1),
      content_hash: "abc".into(),
      media_type: "application/octet-stream".into(),
      size_bytes: 0,
      created_at_ms: 0,
      ttl_ms: None,
    };
    assert!(r.ttl_ms.is_none());
  }
}
