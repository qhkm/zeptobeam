use std::sync::{atomic::AtomicUsize, Arc};

use erlangrt::agent_rt::{
    config::{load_config_from_str, AppConfig},
    observability::RuntimeMetrics,
    server::{HealthServer, ServerState},
};

#[tokio::test]
async fn test_health_server_responds() {
    let config = AppConfig::default();
    let metrics = Arc::new(RuntimeMetrics::new());
    let process_count = Arc::new(AtomicUsize::new(3));

    let state = ServerState {
        metrics,
        process_count,
    };

    // Bind to port 0 to get a random available port
    let server = HealthServer::start("127.0.0.1:0", state).await.unwrap();

    // Note: To do full HTTP client testing we'd need reqwest or ureq.
    // For now, verify the server starts and stops cleanly.
    server.shutdown().await;
}

#[test]
fn test_config_defaults_are_sensible() {
    let config = AppConfig::default();
    assert!(config.runtime.worker_count > 0);
    assert!(config.runtime.mailbox_capacity > 0);
    assert!(config.checkpoint.ttl_hours > 0);
    assert!(config.server.enabled);
}

#[test]
fn test_config_from_full_toml() {
    let toml = r#"
[runtime]
worker_count = 8
mailbox_capacity = 512
max_reductions = 100

[checkpoint]
store = "memory"
path = ""
ttl_hours = 48
prune_interval_secs = 1800

[server]
enabled = false
bind = "0.0.0.0:3000"

[logging]
level = "warn"
format = "json"
"#;
    let config = load_config_from_str(toml).unwrap();
    assert_eq!(config.runtime.worker_count, 8);
    assert_eq!(config.checkpoint.ttl_hours, 48);
    assert!(!config.server.enabled);
    assert_eq!(config.logging.format, "json");
}
