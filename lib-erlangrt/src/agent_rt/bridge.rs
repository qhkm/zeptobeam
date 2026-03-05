use crate::agent_rt::types::*;
use crossbeam_channel::{
  bounded, Receiver, Sender, TrySendError,
};

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

/// Scheduler-side handle.
pub struct BridgeHandle {
  request_tx: Sender<IoRequest>,
  response_rx: Receiver<IoResponse>,
  pub next_correlation_id: u64,
}

/// Tokio-side worker.
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
  pub fn submit(
    &mut self,
    pid: AgentPid,
    op: IoOp,
  ) -> Result<u64, String> {
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
      Err(TrySendError::Full(_)) => {
        Err("request queue full".into())
      }
      Err(TrySendError::Disconnected(_)) => {
        Err("bridge worker disconnected".into())
      }
    }
  }

  /// Non-blocking drain of all pending responses.
  /// Each response is wrapped into
  /// `(AgentPid, Message::System(SystemMsg::IoResponse))`.
  pub fn drain_responses(
    &self,
  ) -> Vec<(AgentPid, Message)> {
    let mut out = Vec::new();
    loop {
      match self.response_rx.try_recv() {
        Ok(resp) => {
          let msg = Message::System(
            SystemMsg::IoResponse {
              correlation_id: resp.correlation_id,
              result: resp.result,
            },
          );
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
      let recv_result =
        tokio::task::spawn_blocking(move || rx.recv())
          .await;
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
async fn execute_io_op(op: &IoOp) -> IoResult {
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
      body: _,
    } => {
      // Placeholder: real HTTP comes later.
      IoResult::Ok(serde_json::json!({
        "kind": "http",
        "method": method,
        "url": url,
        "status": "placeholder"
      }))
    }
  }
}
