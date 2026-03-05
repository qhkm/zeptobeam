/// Structured error type for the agent runtime.
#[derive(Debug, thiserror::Error)]
pub enum AgentRtError {
  #[error("config: {0}")]
  Config(String),

  #[error("checkpoint: {0}")]
  Checkpoint(String),

  #[error("checkpoint IO: {0}")]
  CheckpointIo(#[from] std::io::Error),

  #[error("serialization: {0}")]
  Serialization(#[from] serde_json::Error),

  #[error("bridge: {0}")]
  Bridge(String),

  #[error("server: {0}")]
  Server(String),

  #[error("shutdown: {0}")]
  Shutdown(String),

  #[error("ets: {0}")]
  Ets(String),

  #[error("durable mailbox: {0}")]
  DurableMailbox(String),

  #[error("hot code: {0}")]
  HotCode(String),

  #[error("release: {0}")]
  Release(String),
}

/// Convert rusqlite errors (separate impl to avoid orphan rule conflicts).
impl From<rusqlite::Error> for AgentRtError {
  fn from(e: rusqlite::Error) -> Self {
    AgentRtError::Checkpoint(format!("sqlite: {}", e))
  }
}
