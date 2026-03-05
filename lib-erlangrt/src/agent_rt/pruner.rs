use std::sync::Arc;

use tokio::time::{self, Duration};
use tracing::{debug, info, warn};

use crate::agent_rt::checkpoint::CheckpointStore;

/// Spawns a background task that periodically prunes stale checkpoints.
/// Returns a JoinHandle that can be aborted on shutdown.
pub fn spawn_pruner(
  store: Arc<dyn CheckpointStore>,
  interval_secs: u64,
  ttl_secs: u64,
) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    let mut interval = time::interval(Duration::from_secs(interval_secs));
    loop {
      interval.tick().await;
      let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        - ttl_secs as i64;
      match store.prune_before(cutoff) {
        Ok(0) => debug!("checkpoint pruner: nothing to prune"),
        Ok(n) => info!(pruned = n, "checkpoint pruner: pruned stale entries"),
        Err(e) => warn!("checkpoint pruner failed: {}", e),
      }
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::checkpoint::InMemoryCheckpointStore;

  #[tokio::test]
  async fn test_pruner_starts_and_can_be_aborted() {
    let store = Arc::new(InMemoryCheckpointStore::new());
    let handle = spawn_pruner(store, 3600, 86400);

    // Pruner should be running
    assert!(!handle.is_finished());

    // Abort it
    handle.abort();
    let _ = handle.await;
  }
}
