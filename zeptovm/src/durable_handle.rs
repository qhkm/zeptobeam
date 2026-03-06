use std::sync::{
  atomic::{AtomicU64, Ordering},
  Arc,
};

use tracing::warn;

use crate::{
  durability::wal::WalWriter,
  error::{Message, UserPayload},
  mailbox::ProcessHandle,
  pid::Pid,
};

/// A durable process handle that persists messages to WAL before delivery.
pub struct DurableHandle {
  inner: ProcessHandle,
  wal_writer: Arc<WalWriter>,
  recipient: Pid,
  sender: Pid,
  sender_seq: AtomicU64,
}

impl DurableHandle {
  pub fn new(
    inner: ProcessHandle,
    wal_writer: Arc<WalWriter>,
    recipient: Pid,
    sender: Pid,
  ) -> Self {
    Self {
      inner,
      wal_writer,
      recipient,
      sender,
      sender_seq: AtomicU64::new(0),
    }
  }

  /// Send a message durably: write to WAL first, then deliver to mailbox.
  /// Returns the assigned wal_seq.
  pub async fn send_durable(
    &self,
    msg: Message,
  ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    // Serialize the message to bytes for WAL
    let payload = serde_json::to_vec(&serialize_message(&msg))?;

    // Generate a unique message_id (sender pid + sender_seq)
    let seq = self.sender_seq.fetch_add(1, Ordering::SeqCst);
    let message_id = generate_message_id(self.sender.raw(), seq);

    // Write to WAL first (durable)
    let wal_seq = self
      .wal_writer
      .append(message_id, self.sender, seq, self.recipient, payload)
      .await?;

    // Then deliver to mailbox (best-effort — if this fails, recovery
    // will replay from WAL)
    if let Err(e) = self.inner.send_user(msg).await {
      warn!(
        wal_seq = wal_seq,
        recipient = %self.recipient,
        "mailbox delivery failed after WAL write — will be replayed on recovery: {e}"
      );
    }

    Ok(wal_seq)
  }

  /// Non-durable send (bypass WAL). Delegates to inner handle.
  pub async fn send_user(
    &self,
    msg: Message,
  ) -> Result<(), tokio::sync::mpsc::error::SendError<Message>> {
    self.inner.send_user(msg).await
  }

  /// Kill the process (not persisted — kills are immediate).
  pub fn kill(&self) {
    self.inner.kill();
  }

  /// Get the sender_seq counter (number of durable sends so far).
  pub fn sender_seq(&self) -> u64 {
    self.sender_seq.load(Ordering::SeqCst)
  }
}

/// Generate a u128 message ID from sender pid + sequence.
fn generate_message_id(sender_raw: u64, seq: u64) -> u128 {
  ((sender_raw as u128) << 64) | (seq as u128)
}

/// Serialize a Message to a JSON value for WAL storage.
fn serialize_message(msg: &Message) -> serde_json::Value {
  match msg {
    Message::User(UserPayload::Text(t)) => {
      serde_json::json!({"type": "text", "data": t})
    }
    Message::User(UserPayload::Bytes(b)) => {
      serde_json::json!({"type": "bytes", "data": hex_encode(b)})
    }
    Message::User(UserPayload::Json(v)) => {
      serde_json::json!({"type": "json", "data": v})
    }
  }
}

/// Deserialize a Message from WAL payload bytes.
pub fn deserialize_message(
  payload: &[u8],
) -> Result<Message, serde_json::Error> {
  let value: serde_json::Value = serde_json::from_slice(payload)?;
  match value.get("type").and_then(|t| t.as_str()) {
    Some("text") => {
      let data = value["data"].as_str().unwrap_or("").to_string();
      Ok(Message::text(data))
    }
    Some("json") => {
      let data = value["data"].clone();
      Ok(Message::json(data))
    }
    Some("bytes") => {
      let data = value["data"].as_str().unwrap_or("");
      let bytes = hex_decode(data);
      Ok(Message::bytes(bytes))
    }
    _ => Ok(Message::text("")),
  }
}

fn hex_encode(data: &[u8]) -> String {
  use std::fmt::Write;
  let mut s = String::with_capacity(data.len() * 2);
  for byte in data {
    write!(s, "{:02x}", byte).unwrap();
  }
  s
}

fn hex_decode(hex: &str) -> Vec<u8> {
  (0..hex.len())
    .step_by(2)
    .filter_map(|i| hex.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok()))
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::durability::wal::{SqliteWalStore, WalWriter};
  use crate::process::spawn_process;

  // Helper behavior that just continues
  struct NoopBehavior;

  #[async_trait::async_trait]
  impl crate::Behavior for NoopBehavior {
    async fn init(
      &mut self,
      _: Option<Vec<u8>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
      Ok(())
    }
    async fn handle(&mut self, _: Message) -> crate::error::Action {
      crate::error::Action::Continue
    }
    async fn terminate(&mut self, _: &crate::error::Reason) {}
  }

  #[tokio::test]
  async fn test_durable_send_writes_to_wal() {
    let wal_store = Arc::new(SqliteWalStore::new(":memory:").unwrap());
    let wal_writer = Arc::new(WalWriter::new(wal_store.clone()).await.unwrap());

    let (pid, handle, join) =
      spawn_process(NoopBehavior, 64, None, None, None);
    let sender = Pid::from_raw(999);
    let durable = DurableHandle::new(handle, wal_writer, pid, sender);

    let wal_seq = durable.send_durable(Message::text("hello")).await.unwrap();
    assert_eq!(wal_seq, 1);

    // Verify WAL entry exists
    let entries = wal_store.replay_from(0, pid).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].wal_seq, 1);

    // Cleanup
    durable.kill();
    let _ = join.await;
  }

  #[tokio::test]
  async fn test_durable_send_multiple() {
    let wal_store = Arc::new(SqliteWalStore::new(":memory:").unwrap());
    let wal_writer = Arc::new(WalWriter::new(wal_store.clone()).await.unwrap());

    let (pid, handle, join) =
      spawn_process(NoopBehavior, 64, None, None, None);
    let sender = Pid::from_raw(999);
    let durable = DurableHandle::new(handle, wal_writer, pid, sender);

    let s1 = durable.send_durable(Message::text("a")).await.unwrap();
    let s2 = durable.send_durable(Message::text("b")).await.unwrap();
    let s3 = durable.send_durable(Message::text("c")).await.unwrap();

    assert_eq!(s1, 1);
    assert_eq!(s2, 2);
    assert_eq!(s3, 3);

    let entries = wal_store.replay_from(0, pid).await.unwrap();
    assert_eq!(entries.len(), 3);

    durable.kill();
    let _ = join.await;
  }

  #[tokio::test]
  async fn test_serialize_deserialize_text() {
    let msg = Message::text("hello");
    let serialized = serialize_message(&msg);
    let payload = serde_json::to_vec(&serialized).unwrap();
    let deserialized = deserialize_message(&payload).unwrap();
    match deserialized {
      Message::User(UserPayload::Text(t)) => assert_eq!(t, "hello"),
      _ => panic!("expected text message"),
    }
  }

  #[tokio::test]
  async fn test_serialize_deserialize_json() {
    let msg = Message::json(serde_json::json!({"key": "value"}));
    let serialized = serialize_message(&msg);
    let payload = serde_json::to_vec(&serialized).unwrap();
    let deserialized = deserialize_message(&payload).unwrap();
    match deserialized {
      Message::User(UserPayload::Json(v)) => {
        assert_eq!(v["key"], "value");
      }
      _ => panic!("expected json message"),
    }
  }

  #[tokio::test]
  async fn test_serialize_deserialize_bytes() {
    let msg = Message::bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]);
    let serialized = serialize_message(&msg);
    let payload = serde_json::to_vec(&serialized).unwrap();
    let deserialized = deserialize_message(&payload).unwrap();
    match deserialized {
      Message::User(UserPayload::Bytes(b)) => {
        assert_eq!(b, vec![0xDE, 0xAD, 0xBE, 0xEF]);
      }
      _ => panic!("expected bytes message"),
    }
  }

  #[tokio::test]
  async fn test_generate_message_id_unique() {
    let id1 = generate_message_id(1, 0);
    let id2 = generate_message_id(1, 1);
    let id3 = generate_message_id(2, 0);
    assert_ne!(id1, id2);
    assert_ne!(id1, id3);
  }
}
