use crate::agent_rt::types::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use std::error::Error as StdError;

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

/// Scheduler-side handle for submitting I/O operations
/// and draining responses.
pub struct BridgeHandle {
  request_tx: Sender<IoRequest>,
  response_rx: Receiver<IoResponse>,
  pub next_correlation_id: u64,
}

/// Tokio-side worker that receives I/O requests
/// and executes them asynchronously.
pub struct BridgeWorker {
  request_rx: Receiver<IoRequest>,
  response_tx: Sender<IoResponse>,
}

/// Create a bridge pair: one handle for the scheduler
/// thread and one worker for the Tokio runtime.
pub fn create_bridge() -> (BridgeHandle, BridgeWorker) {
  let (req_tx, req_rx) = bounded(REQUEST_QUEUE_SIZE);
  let (resp_tx, resp_rx) = bounded(RESPONSE_QUEUE_SIZE);
  (
    BridgeHandle {
      request_tx: req_tx,
      response_rx: resp_rx,
      next_correlation_id: 0,
    },
    BridgeWorker {
      request_rx: req_rx,
      response_tx: resp_tx,
    },
  )
}

impl BridgeHandle {
  /// Submit an I/O operation. Returns the correlation ID
  /// on success, or an error string if the queue is full.
  pub fn submit(&mut self, pid: AgentPid, op: IoOp) -> Result<u64, String> {
    let cid = self.next_correlation_id;
    let req = IoRequest {
      correlation_id: cid,
      source_pid: pid,
      op,
    };
    match self.request_tx.try_send(req) {
      Ok(()) => {
        self.next_correlation_id += 1;
        Ok(cid)
      }
      Err(TrySendError::Full(_)) => Err("request queue full".into()),
      Err(TrySendError::Disconnected(_)) => Err("bridge worker disconnected".into()),
    }
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
  /// Run the worker loop. Receives requests from the
  /// scheduler via crossbeam, spawns a Tokio task for
  /// each, and sends the result back. Uses
  /// `spawn_blocking` so the blocking crossbeam recv
  /// does not starve the Tokio runtime.
  pub async fn run(self) {
    let req_rx = self.request_rx;
    let resp_tx = self.response_tx;
    loop {
      let rx = req_rx.clone();
      let recv_result = tokio::task::spawn_blocking(move || rx.recv()).await;
      let req = match recv_result {
        Ok(Ok(r)) => r,
        // Channel disconnected or join error: stop.
        _ => break,
      };
      let tx = resp_tx.clone();
      tokio::spawn(async move {
        let result = execute_io_op(&req.op).await;
        let resp = IoResponse {
          correlation_id: req.correlation_id,
          source_pid: req.source_pid,
          result,
        };
        let _ = tx.send(resp);
      });
    }
  }
}

/// Execute an I/O operation asynchronously.
pub(crate) async fn execute_io_op(op: &IoOp) -> IoResult {
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
      let method = method.clone();
      let url = url.clone();
      let headers = headers.clone();
      let body = body.clone();
      let timeout_ms = *timeout_ms;
      match tokio::task::spawn_blocking(move || {
        execute_http_blocking(&method, &url, &headers, body.as_deref(), timeout_ms)
      })
      .await
      {
        Ok(result) => result,
        Err(e) => IoResult::Error(format!("http task panicked: {}", e)),
      }
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
