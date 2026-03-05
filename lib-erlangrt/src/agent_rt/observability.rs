use std::{
  sync::atomic::{AtomicU64, Ordering},
  time::{Duration, Instant},
};

use serde::Serialize;

use crate::agent_rt::{
  process::{Priority, ProcessStatus},
  types::IoResult,
};

/// Point-in-time process metadata for runtime introspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessSnapshot {
  pub pid: u64,
  pub status: ProcessStatus,
  pub priority: Priority,
  pub mailbox_depth: usize,
  pub link_count: usize,
  pub monitor_count: usize,
  pub monitored_by_count: usize,
  pub trap_exit: bool,
}

/// Point-in-time runtime metrics.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeMetricsSnapshot {
  pub uptime_secs: u64,
  pub active_processes: u64,
  pub messages_sent_total: u64,
  pub messages_processed_total: u64,
  pub mailbox_rejections_total: u64,
  pub messages_per_sec_avg: f64,
  pub io_requests_total: u64,
  pub io_responses_total: u64,
  pub io_errors_total: u64,
  pub io_timeouts_total: u64,
  pub io_latency_samples: u64,
  pub io_latency_total_ms: u64,
  pub io_latency_avg_ms: f64,
  pub rate_limiter_waits_total: u64,
  pub rate_limiter_wait_ms_total: u64,
  pub rate_limiter_rejections_total: u64,
  pub process_terminations_total: u64,
}

/// Shared runtime metrics updated by scheduler/bridge/rate limiter.
pub struct RuntimeMetrics {
  started_at: Instant,
  messages_sent_total: AtomicU64,
  messages_processed_total: AtomicU64,
  mailbox_rejections_total: AtomicU64,
  io_requests_total: AtomicU64,
  io_responses_total: AtomicU64,
  io_errors_total: AtomicU64,
  io_timeouts_total: AtomicU64,
  io_latency_samples: AtomicU64,
  io_latency_total_ms: AtomicU64,
  rate_limiter_waits_total: AtomicU64,
  rate_limiter_wait_ms_total: AtomicU64,
  rate_limiter_rejections_total: AtomicU64,
  process_terminations_total: AtomicU64,
}

impl RuntimeMetrics {
  pub fn new() -> Self {
    Self {
      started_at: Instant::now(),
      messages_sent_total: AtomicU64::new(0),
      messages_processed_total: AtomicU64::new(0),
      mailbox_rejections_total: AtomicU64::new(0),
      io_requests_total: AtomicU64::new(0),
      io_responses_total: AtomicU64::new(0),
      io_errors_total: AtomicU64::new(0),
      io_timeouts_total: AtomicU64::new(0),
      io_latency_samples: AtomicU64::new(0),
      io_latency_total_ms: AtomicU64::new(0),
      rate_limiter_waits_total: AtomicU64::new(0),
      rate_limiter_wait_ms_total: AtomicU64::new(0),
      rate_limiter_rejections_total: AtomicU64::new(0),
      process_terminations_total: AtomicU64::new(0),
    }
  }

  pub fn record_message_sent(&self) {
    self.messages_sent_total.fetch_add(1, Ordering::Relaxed);
  }

  pub fn record_message_processed(&self) {
    self
      .messages_processed_total
      .fetch_add(1, Ordering::Relaxed);
  }

  pub fn record_mailbox_rejection(&self) {
    self
      .mailbox_rejections_total
      .fetch_add(1, Ordering::Relaxed);
  }

  pub fn record_io_request(&self) {
    self.io_requests_total.fetch_add(1, Ordering::Relaxed);
  }

  pub fn record_io_result(&self, result: &IoResult, latency: Duration) {
    self.io_responses_total.fetch_add(1, Ordering::Relaxed);
    self.io_latency_samples.fetch_add(1, Ordering::Relaxed);
    self
      .io_latency_total_ms
      .fetch_add(duration_to_ms(latency), Ordering::Relaxed);
    match result {
      IoResult::Error(_) => {
        self.io_errors_total.fetch_add(1, Ordering::Relaxed);
      }
      IoResult::Timeout => {
        self.io_timeouts_total.fetch_add(1, Ordering::Relaxed);
      }
      IoResult::Ok(_) => {}
    }
  }

  pub fn record_rate_limiter_rejection(&self) {
    self
      .rate_limiter_rejections_total
      .fetch_add(1, Ordering::Relaxed);
  }

  pub fn record_rate_limiter_wait(&self, waited: Duration) {
    self
      .rate_limiter_waits_total
      .fetch_add(1, Ordering::Relaxed);
    self
      .rate_limiter_wait_ms_total
      .fetch_add(duration_to_ms(waited), Ordering::Relaxed);
  }

  pub fn record_process_termination(&self) {
    self
      .process_terminations_total
      .fetch_add(1, Ordering::Relaxed);
  }

  pub fn snapshot(&self, active_processes: usize) -> RuntimeMetricsSnapshot {
    let uptime = self.started_at.elapsed();
    let uptime_secs = uptime.as_secs();
    let uptime_secs_nonzero = uptime.as_secs_f64().max(0.000_001);

    let messages_sent_total = self.messages_sent_total.load(Ordering::Relaxed);
    let messages_processed_total = self.messages_processed_total.load(Ordering::Relaxed);
    let io_latency_samples = self.io_latency_samples.load(Ordering::Relaxed);
    let io_latency_total_ms = self.io_latency_total_ms.load(Ordering::Relaxed);

    RuntimeMetricsSnapshot {
      uptime_secs,
      active_processes: active_processes as u64,
      messages_sent_total,
      messages_processed_total,
      mailbox_rejections_total: self.mailbox_rejections_total.load(Ordering::Relaxed),
      messages_per_sec_avg: messages_processed_total as f64 / uptime_secs_nonzero,
      io_requests_total: self.io_requests_total.load(Ordering::Relaxed),
      io_responses_total: self.io_responses_total.load(Ordering::Relaxed),
      io_errors_total: self.io_errors_total.load(Ordering::Relaxed),
      io_timeouts_total: self.io_timeouts_total.load(Ordering::Relaxed),
      io_latency_samples,
      io_latency_total_ms,
      io_latency_avg_ms: if io_latency_samples == 0 {
        0.0
      } else {
        io_latency_total_ms as f64 / io_latency_samples as f64
      },
      rate_limiter_waits_total: self.rate_limiter_waits_total.load(Ordering::Relaxed),
      rate_limiter_wait_ms_total: self.rate_limiter_wait_ms_total.load(Ordering::Relaxed),
      rate_limiter_rejections_total: self
        .rate_limiter_rejections_total
        .load(Ordering::Relaxed),
      process_terminations_total: self.process_terminations_total.load(Ordering::Relaxed),
    }
  }
}

impl Default for RuntimeMetrics {
  fn default() -> Self {
    Self::new()
  }
}

fn duration_to_ms(duration: Duration) -> u64 {
  let ms = duration.as_millis();
  if ms > u64::MAX as u128 {
    u64::MAX
  } else {
    ms as u64
  }
}
