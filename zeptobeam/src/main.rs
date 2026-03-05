use std::sync::{atomic::AtomicUsize, Arc};

use clap::Parser;
use tracing::{info, warn};

use erlangrt::agent_rt::{
    checkpoint::{CheckpointStore, FileCheckpointStore, InMemoryCheckpointStore},
    checkpoint_sqlite::SqliteCheckpointStore,
    config::{load_config, AppConfig},
    observability::RuntimeMetrics,
    pruner::spawn_pruner,
    server::{HealthServer, ServerState},
};

#[derive(Parser, Debug)]
#[command(name = "zeptobeam", version, about = "Zeptoclaw agent runtime daemon")]
struct Cli {
    /// Config file path
    #[arg(short, long, default_value = "zeptobeam.toml")]
    config: String,

    /// Override log level (trace|debug|info|warn|error)
    #[arg(short, long)]
    log_level: Option<String>,

    /// Override worker count
    #[arg(short, long)]
    workers: Option<usize>,

    /// Override server bind address
    #[arg(short, long)]
    bind: Option<String>,
}

fn init_tracing(config: &AppConfig, cli_level: Option<&str>) {
    let level = cli_level.unwrap_or(&config.logging.level);
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));

    match config.logging.format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .init();
        }
        "compact" => {
            tracing_subscriber::fmt()
                .compact()
                .with_env_filter(env_filter)
                .init();
        }
        _ => {
            // "pretty" or default
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .init();
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Load config (optional — use defaults if file not found)
    let mut config = match load_config(&cli.config) {
        Ok(c) => {
            eprintln!("Loaded config from {}", cli.config);
            c
        }
        Err(_) => {
            eprintln!(
                "Config file '{}' not found, using defaults",
                cli.config
            );
            AppConfig::default()
        }
    };

    // Apply CLI overrides
    if let Some(workers) = cli.workers {
        config.runtime.worker_count = workers;
    }
    if let Some(ref bind) = cli.bind {
        config.server.bind = bind.clone();
    }

    // Init tracing
    init_tracing(&config, cli.log_level.as_deref());

    info!("zeptobeam starting");
    info!(
        workers = config.runtime.worker_count,
        mailbox_capacity = config.runtime.mailbox_capacity,
        checkpoint_store = %config.checkpoint.store,
        "runtime configuration"
    );

    // Create shared metrics
    let metrics = Arc::new(RuntimeMetrics::new());
    let process_count = Arc::new(AtomicUsize::new(0));

    // Create checkpoint store based on config
    let checkpoint_store: Arc<dyn CheckpointStore> = match config.checkpoint.store.as_str() {
        "memory" => Arc::new(InMemoryCheckpointStore::new()),
        "file" => {
            Arc::new(FileCheckpointStore::new(&config.checkpoint.path)
                .expect("Failed to create file checkpoint store"))
        }
        _ => {
            // "sqlite" or default
            Arc::new(SqliteCheckpointStore::open(&config.checkpoint.path)
                .expect("Failed to open SQLite checkpoint store"))
        }
    };

    // Start checkpoint pruner
    let pruner = if config.checkpoint.ttl_hours > 0 {
        let ttl_secs = config.checkpoint.ttl_hours * 3600;
        Some(spawn_pruner(
            checkpoint_store.clone(),
            config.checkpoint.prune_interval_secs,
            ttl_secs,
        ))
    } else {
        None
    };

    // Start health server (if enabled)
    let server = if config.server.enabled {
        let state = ServerState {
            metrics: metrics.clone(),
            process_count: process_count.clone(),
        };
        match HealthServer::start(&config.server.bind, state).await {
            Ok(s) => {
                info!(bind = %config.server.bind, "health server started");
                Some(s)
            }
            Err(e) => {
                warn!("Failed to start health server: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Wait for shutdown signal
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("register SIGTERM");
            tokio::select! {
                _ = ctrl_c => info!("received SIGINT"),
                _ = sigterm.recv() => info!("received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
            info!("received SIGINT");
        }
    };

    shutdown.await;
    info!("shutting down gracefully...");

    // Stop pruner
    if let Some(p) = pruner {
        p.abort();
        info!("checkpoint pruner stopped");
    }

    // Shutdown health server
    if let Some(s) = server {
        s.shutdown().await;
        info!("health server stopped");
    }

    info!("zeptobeam stopped");
}
