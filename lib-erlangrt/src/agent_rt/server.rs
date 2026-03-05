use std::sync::Arc;

use axum::{extract::State, routing::get, Json, Router};
use tokio::sync::oneshot;

use crate::agent_rt::observability::{RuntimeMetrics, RuntimeMetricsSnapshot};

#[derive(Clone)]
pub struct ServerState {
    pub metrics: Arc<RuntimeMetrics>,
    pub process_count: Arc<std::sync::atomic::AtomicUsize>,
}

pub struct HealthServer {
    shutdown_tx: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl HealthServer {
    pub async fn start(bind: &str, state: ServerState) -> Result<Self, std::io::Error> {
        let app = Router::new()
            .route("/health", get(health_handler))
            .route("/metrics", get(metrics_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(bind).await?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });

        Ok(Self { shutdown_tx, handle })
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.handle.await;
    }
}

async fn health_handler(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let uptime = state
        .metrics
        .snapshot(
            state
                .process_count
                .load(std::sync::atomic::Ordering::Relaxed),
        )
        .uptime_secs;
    Json(serde_json::json!({
        "status": "ok",
        "uptime_secs": uptime,
    }))
}

async fn metrics_handler(State(state): State<ServerState>) -> Json<RuntimeMetricsSnapshot> {
    let snap = state.metrics.snapshot(
        state
            .process_count
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    Json(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_endpoint() {
        let state = ServerState {
            metrics: Arc::new(RuntimeMetrics::new()),
            process_count: Arc::new(std::sync::atomic::AtomicUsize::new(5)),
        };
        let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();

        // Server should start without error
        // (Full HTTP client testing is integration-level)

        server.shutdown().await;
    }
}
