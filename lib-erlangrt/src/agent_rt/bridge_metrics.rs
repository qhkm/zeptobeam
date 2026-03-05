use std::sync::atomic::{AtomicU64, Ordering};

/// Counters for bridge-level operational metrics.
#[derive(Default)]
pub struct BridgeMetrics {
  destroy_failures: AtomicU64,
  busy_rejections: AtomicU64,
  chat_panics: AtomicU64,
}

impl BridgeMetrics {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn inc_destroy_failures(&self) {
    self.destroy_failures.fetch_add(1, Ordering::Relaxed);
  }

  pub fn inc_busy_rejections(&self) {
    self.busy_rejections.fetch_add(1, Ordering::Relaxed);
  }

  pub fn inc_chat_panics(&self) {
    self.chat_panics.fetch_add(1, Ordering::Relaxed);
  }

  pub fn agent_destroy_failures(&self) -> u64 {
    self.destroy_failures.load(Ordering::Relaxed)
  }

  pub fn worker_busy_rejections(&self) -> u64 {
    self.busy_rejections.load(Ordering::Relaxed)
  }

  pub fn agent_chat_panics(&self) -> u64 {
    self.chat_panics.load(Ordering::Relaxed)
  }
}
