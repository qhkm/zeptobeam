use crate::agent_rt::{
  bridge_metrics::BridgeMetrics,
  observability::RuntimeMetrics,
  rate_limiter::RateLimiter,
  tool_factory::{DefaultToolFactory, ToolFactory},
  types::*,
};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::{
  collections::HashMap,
  error::Error as StdError,
  sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
  },
  time::Duration,
};
use tokio::sync::Mutex as TokioMutex;
use tracing::{debug, warn};
use zeptoclaw::{agent::ZeptoAgent, providers::LLMProvider};

const REQUEST_QUEUE_SIZE: usize = 1024;
const RESPONSE_QUEUE_SIZE: usize = 4096;
/// Request sent from scheduler thread to Tokio thread
/// pool.
#[derive(Debug)]
pub struct IoRequest {
  pub correlation_id: u64,
  pub source_pid: AgentPid,
  pub op: IoOp,
}

/// Response sent from Tokio thread pool back to
/// scheduler thread.
#[derive(Debug)]
pub struct IoResponse {
  pub correlation_id: u64,
  pub source_pid: AgentPid,
  pub result: IoResult,
}

#[derive(Debug)]
enum WorkerRequest {
  Io(IoRequest),
  Shutdown { timeout: Duration, ack: Sender<()> },
}

/// Scheduler-side handle for submitting I/O operations
/// and draining responses.
pub struct BridgeHandle {
  request_tx: Sender<WorkerRequest>,
  response_rx: Receiver<IoResponse>,
  next_correlation_id: Arc<AtomicU64>,
  shutdown_enabled: bool,
}

/// Tokio-side worker that receives I/O requests
/// and executes them asynchronously.
pub struct BridgeWorker {
  request_rx: Receiver<WorkerRequest>,
  response_tx: Sender<IoResponse>,
  rate_limiter: Option<RateLimiter>,
  metrics: Option<Arc<RuntimeMetrics>>,
  // Agent-related fields
  agent_registry: Arc<std::sync::Mutex<HashMap<AgentPid, Arc<TokioMutex<ZeptoAgent>>>>>,
  agent_provider_registry: Arc<HashMap<String, Arc<dyn LLMProvider + Send + Sync>>>,
  agent_tool_factory: Arc<dyn ToolFactory>,
  agent_metrics: Arc<BridgeMetrics>,
}

impl Clone for BridgeHandle {
  fn clone(&self) -> Self {
    Self {
      request_tx: self.request_tx.clone(),
      response_rx: self.response_rx.clone(),
      next_correlation_id: self.next_correlation_id.clone(),
      shutdown_enabled: self.shutdown_enabled,
    }
  }
}

/// Create a bridge pair: one handle for the scheduler
/// thread and one worker for the Tokio runtime.
pub fn create_bridge() -> (BridgeHandle, BridgeWorker) {
  let (handle, mut workers) = create_bridge_pool(1);
  (handle, workers.pop().expect("bridge pool with 1 worker"))
}

/// Create a shared bridge handle and a worker pool.
/// The returned handle can be cloned and shared across
/// schedulers; workers consume from one request queue.
pub fn create_bridge_pool(worker_count: usize) -> (BridgeHandle, Vec<BridgeWorker>) {
  let worker_count = worker_count.max(1);
  let (req_tx, req_rx) = bounded(REQUEST_QUEUE_SIZE);
  let (resp_tx, resp_rx) = bounded(RESPONSE_QUEUE_SIZE);
  let handle = BridgeHandle {
    request_tx: req_tx,
    response_rx: resp_rx,
    next_correlation_id: Arc::new(AtomicU64::new(0)),
    shutdown_enabled: true,
  };
  let mut workers = Vec::with_capacity(worker_count);
  for _ in 0..worker_count {
    workers.push(BridgeWorker {
      request_rx: req_rx.clone(),
      response_tx: resp_tx.clone(),
      rate_limiter: None,
      metrics: None,
      agent_registry: Arc::new(std::sync::Mutex::new(HashMap::new())),
      agent_provider_registry: Arc::new(HashMap::new()),
      agent_tool_factory: Arc::new(DefaultToolFactory::from_env()),
      agent_metrics: Arc::new(BridgeMetrics::new()),
    });
  }
  (handle, workers)
}

pub fn create_bridge_with_metrics(
  metrics: Arc<RuntimeMetrics>,
) -> (BridgeHandle, BridgeWorker) {
  let (handle, mut worker) = create_bridge();
  worker.set_metrics(metrics);
  (handle, worker)
}

impl BridgeHandle {
  /// Disable shutdown signaling on this handle clone.
  /// Useful when multiple scheduler handles share one
  /// worker pool and shutdown is coordinated elsewhere.
  pub fn with_shutdown_disabled(mut self) -> Self {
    self.shutdown_enabled = false;
    self
  }

  /// Submit an I/O operation. Returns the correlation ID
  /// on success, or an error string if the queue is full.
  pub fn submit(&mut self, pid: AgentPid, op: IoOp) -> Result<u64, String> {
    let cid = self.next_correlation_id.fetch_add(1, Ordering::Relaxed);
    let req = IoRequest {
      correlation_id: cid,
      source_pid: pid,
      op,
    };
    match self.request_tx.try_send(WorkerRequest::Io(req)) {
      Ok(()) => Ok(cid),
      Err(TrySendError::Full(_)) => Err("request queue full".into()),
      Err(TrySendError::Disconnected(_)) => Err("bridge worker disconnected".into()),
    }
  }

  /// Ask the bridge worker to stop accepting new I/O and
  /// wait for in-flight operations to complete up to
  /// `timeout`.
  pub fn shutdown(&mut self, timeout: Duration) -> Result<(), String> {
    if !self.shutdown_enabled {
      return Ok(());
    }
    let (ack_tx, ack_rx) = bounded(1);
    self
      .request_tx
      .send(WorkerRequest::Shutdown {
        timeout,
        ack: ack_tx,
      })
      .map_err(|_| "bridge worker disconnected".to_string())?;
    ack_rx
      .recv_timeout(timeout)
      .map_err(|_| "bridge shutdown timed out".to_string())
  }

  /// Non-blocking drain of all pending responses.
  /// Each response is wrapped into
  /// `(AgentPid, Message::System(SystemMsg::IoResponse))`.
  pub fn drain_responses(&self) -> Vec<(AgentPid, Message)> {
    let mut out = Vec::new();
    loop {
      match self.response_rx.try_recv() {
        Ok(resp) => {
          let msg = Message::System(SystemMsg::IoResponse {
            correlation_id: resp.correlation_id,
            result: resp.result,
          });
          out.push((resp.source_pid, msg));
        }
        Err(_) => break,
      }
    }
    out
  }
}

impl BridgeWorker {
  /// Attach a rate limiter. Must be called before `run`.
  pub fn set_rate_limiter(&mut self, rl: RateLimiter) {
    if let Some(metrics) = &self.metrics {
      rl.set_metrics(metrics.clone());
    }
    self.rate_limiter = Some(rl);
  }

  pub fn set_metrics(&mut self, metrics: Arc<RuntimeMetrics>) {
    if let Some(ref rl) = self.rate_limiter {
      rl.set_metrics(metrics.clone());
    }
    self.metrics = Some(metrics);
  }

  pub fn set_agent_provider_registry(
    &mut self,
    registry: Arc<HashMap<String, Arc<dyn LLMProvider + Send + Sync>>>,
  ) {
    self.agent_provider_registry = registry;
  }

  pub fn set_agent_tool_factory(&mut self, factory: Arc<dyn ToolFactory>) {
    self.agent_tool_factory = factory;
  }

  pub fn set_agent_metrics(&mut self, metrics: Arc<BridgeMetrics>) {
    self.agent_metrics = metrics;
  }

  pub async fn execute_agent_chat(&self, pid: AgentPid, op: &IoOp) -> IoResult {
    execute_agent_chat_standalone(
      pid,
      op,
      self.agent_registry.clone(),
      self.agent_provider_registry.clone(),
      self.agent_tool_factory.clone(),
      self.agent_metrics.clone(),
    )
    .await
  }

  pub fn execute_agent_destroy(&self, target_pid: &AgentPid) -> IoResult {
    let mut registry = self.agent_registry.lock().unwrap();
    let removed = registry.remove(target_pid).is_some();
    IoResult::Ok(serde_json::json!({ "destroyed": removed }))
  }

  /// Run the worker loop. Receives requests from the
  /// scheduler via crossbeam, spawns a Tokio task for
  /// each, and sends the result back. Uses
  /// `spawn_blocking` so the blocking crossbeam recv
  /// does not starve the Tokio runtime.
  pub async fn run(self) {
    let mut in_flight = tokio::task::JoinSet::new();
    let req_rx = self.request_rx;
    let resp_tx = self.response_tx;
    let rate_limiter = self.rate_limiter;
    let metrics = self.metrics;
    let agent_registry = self.agent_registry;
    let agent_provider_registry = self.agent_provider_registry;
    let agent_tool_factory = self.agent_tool_factory;
    let agent_metrics = self.agent_metrics;
    loop {
      while in_flight.try_join_next().is_some() {}

      let rx = req_rx.clone();
      let recv_result = tokio::task::spawn_blocking(move || rx.recv()).await;
      let request = match recv_result {
        Ok(Ok(r)) => r,
        // Channel disconnected or join error: stop.
        _ => break,
      };
      match request {
        WorkerRequest::Io(req) => {
          let tx = resp_tx.clone();
          let rl = rate_limiter.clone();
          let metrics = metrics.clone();
          let agent_reg = agent_registry.clone();
          let agent_prov = agent_provider_registry.clone();
          let agent_tf = agent_tool_factory.clone();
          let agent_met = agent_metrics.clone();
          in_flight.spawn(async move {
            let op_kind = io_op_kind(&req.op);
            if let Some(ref m) = metrics {
              m.record_io_request();
            }
            debug!(
              pid = req.source_pid.raw(),
              correlation_id = req.correlation_id,
              op_kind = op_kind,
              "bridge io start"
            );
            let start = std::time::Instant::now();
            let result = match &req.op {
              IoOp::AgentChat { .. } => {
                execute_agent_chat_standalone(
                  req.source_pid,
                  &req.op,
                  agent_reg,
                  agent_prov,
                  agent_tf,
                  agent_met,
                )
                .await
              }
              IoOp::AgentDestroy { target_pid } => {
                let mut reg = agent_reg.lock().unwrap();
                let removed = reg.remove(target_pid).is_some();
                IoResult::Ok(serde_json::json!({ "destroyed": removed }))
              }
              _ => execute_io_op(&req.op, rl.as_ref()).await,
            };
            let latency = start.elapsed();
            if let Some(ref m) = metrics {
              m.record_io_result(&result, latency);
            }
            match &result {
              IoResult::Ok(_) => {
                debug!(
                  pid = req.source_pid.raw(),
                  correlation_id = req.correlation_id,
                  op_kind = op_kind,
                  latency_ms = latency.as_millis() as u64,
                  "bridge io success"
                );
              }
              IoResult::Timeout => {
                warn!(
                  pid = req.source_pid.raw(),
                  correlation_id = req.correlation_id,
                  op_kind = op_kind,
                  latency_ms = latency.as_millis() as u64,
                  "bridge io timeout"
                );
              }
              IoResult::Error(err) => {
                warn!(
                  pid = req.source_pid.raw(),
                  correlation_id = req.correlation_id,
                  op_kind = op_kind,
                  latency_ms = latency.as_millis() as u64,
                  error = err,
                  "bridge io error"
                );
              }
            }
            let resp = IoResponse {
              correlation_id: req.correlation_id,
              source_pid: req.source_pid,
              result,
            };
            let _ = tx.send(resp);
          });
        }
        WorkerRequest::Shutdown { timeout, ack } => {
          let deadline = tokio::time::Instant::now() + timeout;
          while !in_flight.is_empty() {
            let now = tokio::time::Instant::now();
            if now >= deadline {
              in_flight.abort_all();
              break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let _ = tokio::time::timeout(remaining, in_flight.join_next()).await;
          }
          let _ = ack.send(());
          break;
        }
      }
    }
    in_flight.abort_all();
  }
}

fn io_op_kind(op: &IoOp) -> &'static str {
  match op {
    IoOp::HttpRequest { .. } => "HttpRequest",
    IoOp::Timer { .. } => "Timer",
    IoOp::Custom { .. } => "Custom",
    IoOp::AgentChat { .. } => "AgentChat",
    IoOp::AgentDestroy { .. } => "AgentDestroy",
  }
}

async fn execute_agent_chat_standalone(
  pid: AgentPid,
  op: &IoOp,
  agent_registry: Arc<std::sync::Mutex<HashMap<AgentPid, Arc<TokioMutex<ZeptoAgent>>>>>,
  provider_registry: Arc<HashMap<String, Arc<dyn LLMProvider + Send + Sync>>>,
  tool_factory: Arc<dyn ToolFactory>,
  metrics: Arc<BridgeMetrics>,
) -> IoResult {
  let (provider_name, model, system_prompt, prompt, tools_whitelist, max_iterations) =
    match op {
      IoOp::AgentChat {
        provider,
        model,
        system_prompt,
        prompt,
        tools,
        max_iterations,
        ..
      } => (
        provider,
        model,
        system_prompt,
        prompt,
        tools,
        max_iterations,
      ),
      _ => return IoResult::Error("not an AgentChat op".into()),
    };

  let agent_arc = {
    let mut registry = agent_registry.lock().unwrap();
    registry
      .entry(pid)
      .or_insert_with(|| {
        let llm_provider = provider_registry
          .get(provider_name)
          .cloned()
          .unwrap_or_else(|| {
            provider_registry
              .values()
              .next()
              .expect("at least one provider")
              .clone()
          });

        let tools =
          tool_factory.build_tools(tools_whitelist.as_ref().map(|v| v.as_slice()));

        let mut builder = ZeptoAgent::builder()
          .provider_arc(llm_provider)
          .tools(tools);

        if let Some(sp) = system_prompt {
          builder = builder.system_prompt(sp);
        }
        if let Some(m) = model {
          builder = builder.model(m);
        }
        if let Some(mi) = max_iterations {
          builder = builder.max_iterations(*mi);
        }

        Arc::new(TokioMutex::new(builder.build().expect("agent build")))
      })
      .clone()
  };

  // Panic containment: spawn a task to isolate panics
  let agent = agent_arc.clone();
  let prompt_owned = prompt.to_string();

  let chat_result = tokio::task::spawn(async move {
    let guard = agent.lock().await;
    guard.chat(&prompt_owned).await
  })
  .await;

  match chat_result {
    Ok(Ok(response)) => IoResult::Ok(serde_json::json!({ "response": response })),
    Ok(Err(e)) => IoResult::Error(format!("agent chat error: {}", e)),
    Err(join_err) => {
      metrics.inc_chat_panics();
      IoResult::Error(format!("agent chat panicked: {}", join_err))
    }
  }
}

/// Execute an I/O operation asynchronously.
pub(crate) async fn execute_io_op(
  op: &IoOp,
  rate_limiter: Option<&RateLimiter>,
) -> IoResult {
  match op {
    IoOp::Timer { duration } => {
      tokio::time::sleep(*duration).await;
      IoResult::Ok(serde_json::json!({
        "kind": "timer",
        "elapsed_ms": duration.as_millis() as u64
      }))
    }
    IoOp::HttpRequest {
      method,
      url,
      headers,
      body,
      timeout_ms,
    } => {
      execute_http_op(
        method.clone(),
        url.clone(),
        headers.clone(),
        body.clone(),
        *timeout_ms,
        rate_limiter,
      )
      .await
    }
    IoOp::Custom { kind, payload } => {
      // Custom ops return the payload as-is for now.
      // Real implementations will be pluggable.
      IoResult::Ok(serde_json::json!({
        "kind": kind,
        "payload": payload,
        "status": "placeholder"
      }))
    }
    IoOp::AgentChat { .. } | IoOp::AgentDestroy { .. } => {
      IoResult::Error("agent operations not yet implemented".to_string())
    }
  }
}

async fn execute_http_op(
  method: String,
  url: String,
  headers: std::collections::HashMap<String, String>,
  body: Option<Vec<u8>>,
  timeout_ms: Option<u64>,
  rate_limiter: Option<&RateLimiter>,
) -> IoResult {
  let rl = rate_limiter.cloned();
  match tokio::task::spawn_blocking(move || {
    if let Some(ref rl) = rl {
      let provider = extract_provider(&url);
      rl.acquire_blocking(&provider);
    }
    execute_http_blocking(&method, &url, &headers, body.as_deref(), timeout_ms)
  })
  .await
  {
    Ok(result) => result,
    Err(e) => IoResult::Error(format!("http task panicked: {}", e)),
  }
}

fn execute_http_blocking(
  method: &str,
  url: &str,
  headers: &std::collections::HashMap<String, String>,
  body: Option<&[u8]>,
  timeout_ms: Option<u64>,
) -> IoResult {
  let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(30_000));
  let agent = ureq::AgentBuilder::new()
    .timeout_connect(timeout)
    .timeout_read(timeout)
    .build();

  let mut req = match method.to_uppercase().as_str() {
    "GET" => agent.get(url),
    "POST" => agent.post(url),
    "PUT" => agent.put(url),
    "DELETE" => agent.delete(url),
    "PATCH" => agent.request("PATCH", url),
    "HEAD" => agent.head(url),
    other => agent.request(other, url),
  };

  for (k, v) in headers {
    req = req.set(k, v);
  }

  let response = match body {
    Some(b) => req.send_bytes(b),
    None => req.call(),
  };

  match response {
    Ok(resp) => {
      let status = resp.status();
      let resp_body = resp.into_string().unwrap_or_default();
      IoResult::Ok(serde_json::json!({
        "status": status,
        "body": resp_body
      }))
    }
    Err(ureq::Error::Status(code, resp)) => {
      let resp_body = resp.into_string().unwrap_or_default();
      IoResult::Ok(serde_json::json!({
        "status": code,
        "body": resp_body
      }))
    }
    Err(ureq::Error::Transport(t)) => {
      if is_timeout_error(&t) {
        return IoResult::Timeout;
      }
      match t.kind() {
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => {
          IoResult::Error(format!("connection error: {}", t))
        }
        _ => IoResult::Error(format!("http error: {}", t)),
      }
    }
  }
}

fn is_timeout_error(t: &ureq::Transport) -> bool {
  let msg = t.to_string();
  if msg.contains("timed out") || msg.contains("Timeout") {
    return true;
  }
  if let Some(source) = t.source() {
    if let Some(io_err) = source.downcast_ref::<std::io::Error>() {
      return io_err.kind() == std::io::ErrorKind::TimedOut;
    }
  }
  false
}

/// Extract a provider key from a URL for rate limiting.
/// Uses the hostname (e.g., "api.openai.com").
pub(crate) fn extract_provider(url: &str) -> String {
  url
    .split("://")
    .nth(1)
    .unwrap_or(url)
    .split('/')
    .next()
    .unwrap_or(url)
    .split(':')
    .next()
    .unwrap_or(url)
    .to_string()
}
