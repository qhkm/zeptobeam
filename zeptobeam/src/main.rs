use std::{
  sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
  },
  thread::JoinHandle,
  time::Duration,
};

use clap::Parser;
use tracing::{info, warn};

use erlangrt::agent_rt::{
  checkpoint::{CheckpointStore, FileCheckpointStore, InMemoryCheckpointStore},
  checkpoint_sqlite::SqliteCheckpointStore,
  config::{load_config, AppConfig},
  mcp_types::ProcessInfo,
  observability::RuntimeMetrics,
  pruner::spawn_pruner,
  scheduler::AgentScheduler,
  server::{HealthServer, McpRuntimeOps, McpServerStateExt, ServerState},
  types::{AgentPid, Message, Reason},
};

enum McpRuntimeCommand {
  ListProcesses {
    reply: mpsc::Sender<Result<Vec<ProcessInfo>, String>>,
  },
  SendMessage {
    pid: u64,
    message: Message,
    reply: mpsc::Sender<Result<(), String>>,
  },
  CancelProcess {
    pid: u64,
    reason: Reason,
    reply: mpsc::Sender<Result<(), String>>,
  },
  Shutdown,
}

#[derive(Clone)]
struct DaemonMcpRuntimeOps {
  tx: mpsc::Sender<McpRuntimeCommand>,
  timeout: Duration,
}

impl DaemonMcpRuntimeOps {
  fn call<T>(
    &self,
    make_cmd: impl FnOnce(mpsc::Sender<Result<T, String>>) -> McpRuntimeCommand,
  ) -> Result<T, String> {
    let (reply_tx, reply_rx) = mpsc::channel();
    self
      .tx
      .send(make_cmd(reply_tx))
      .map_err(|_| "MCP runtime worker disconnected".to_string())?;
    reply_rx
      .recv_timeout(self.timeout)
      .map_err(|_| "MCP runtime worker response timed out".to_string())?
  }
}

impl McpRuntimeOps for DaemonMcpRuntimeOps {
  fn list_processes(&self) -> Result<Vec<ProcessInfo>, String> {
    self.call(|reply| McpRuntimeCommand::ListProcesses { reply })
  }

  fn send_message(&self, pid: u64, message: Message) -> Result<(), String> {
    self.call(|reply| McpRuntimeCommand::SendMessage {
      pid,
      message,
      reply,
    })
  }

  fn cancel_process(&self, pid: u64, reason: Reason) -> Result<(), String> {
    self.call(|reply| McpRuntimeCommand::CancelProcess { pid, reason, reply })
  }
}

fn spawn_mcp_runtime_worker(
  process_count: Arc<AtomicUsize>,
) -> (
  Arc<dyn McpRuntimeOps>,
  mpsc::Sender<McpRuntimeCommand>,
  JoinHandle<()>,
) {
  let (tx, rx) = mpsc::channel::<McpRuntimeCommand>();
  let shutdown_tx = tx.clone();
  let handle = std::thread::spawn(move || {
    let mut scheduler = AgentScheduler::new();
    process_count.store(scheduler.registry.count(), Ordering::Relaxed);

    while let Ok(cmd) = rx.recv() {
      match cmd {
        McpRuntimeCommand::ListProcesses { reply } => {
          let processes = scheduler
            .list_processes()
            .into_iter()
            .map(|snap| ProcessInfo {
              pid: snap.pid,
              status: format!("{:?}", snap.status),
              priority: format!("{:?}", snap.priority),
              mailbox_depth: snap.mailbox_depth,
            })
            .collect();
          let _ = reply.send(Ok(processes));
        }
        McpRuntimeCommand::SendMessage {
          pid,
          message,
          reply,
        } => {
          let result = scheduler.send(AgentPid::from_raw(pid), message);
          let _ = reply.send(result);
        }
        McpRuntimeCommand::CancelProcess { pid, reason, reply } => {
          scheduler.terminate_process(AgentPid::from_raw(pid), reason);
          let _ = reply.send(Ok(()));
        }
        McpRuntimeCommand::Shutdown => break,
      }

      process_count.store(scheduler.registry.count(), Ordering::Relaxed);
    }
  });

  let runtime_ops: Arc<dyn McpRuntimeOps> = Arc::new(DaemonMcpRuntimeOps {
    tx,
    timeout: Duration::from_secs(2),
  });
  (runtime_ops, shutdown_tx, handle)
}

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
      tracing_subscriber::fmt().with_env_filter(env_filter).init();
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
      eprintln!("Config file '{}' not found, using defaults", cli.config);
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
  let checkpoint_store: Arc<dyn CheckpointStore> = match config.checkpoint.store.as_str()
  {
    "memory" => Arc::new(InMemoryCheckpointStore::new()),
    "file" => Arc::new(
      FileCheckpointStore::new(&config.checkpoint.path)
        .expect("Failed to create file checkpoint store"),
    ),
    _ => {
      // "sqlite" or default
      Arc::new(
        SqliteCheckpointStore::open(&config.checkpoint.path)
          .expect("Failed to open SQLite checkpoint store"),
      )
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

  // Start HTTP server for health and/or MCP endpoints.
  let http_enabled = config.server.enabled || config.mcp.server.enabled;
  let mut mcp_runtime_shutdown: Option<mpsc::Sender<McpRuntimeCommand>> = None;
  let mut mcp_runtime_handle: Option<JoinHandle<()>> = None;
  let server = if http_enabled {
    let state = ServerState {
      metrics: metrics.clone(),
      process_count: process_count.clone(),
      approval_registry: None,
    };

    if config.mcp.server.enabled {
      let mcp_auth_token = config.mcp.server.auth_token_env.as_ref().and_then(
        |env_name| match std::env::var(env_name) {
          Ok(value) if !value.trim().is_empty() => Some(value),
          Ok(_) | Err(_) => None,
        },
      );

      if let Some(env_name) = &config.mcp.server.auth_token_env {
        if mcp_auth_token.is_none() {
          warn!(
              env = %env_name,
              "MCP auth env var not set or empty; MCP endpoints will run without auth"
          );
        }
      }

      let mcp_state = McpServerStateExt::with_timeout(
        state,
        mcp_auth_token,
        true,
        config.mcp.server.session_timeout_secs,
      );
      let (runtime_ops, shutdown_tx, worker_handle) =
        spawn_mcp_runtime_worker(process_count.clone());
      let mcp_state = mcp_state.with_runtime_ops(runtime_ops);

      match HealthServer::start_with_mcp(&config.server.bind, mcp_state).await {
        Ok(s) => {
          mcp_runtime_shutdown = Some(shutdown_tx);
          mcp_runtime_handle = Some(worker_handle);
          info!(bind = %config.server.bind, "health + MCP server started");
          Some(s)
        }
        Err(e) => {
          let _ = shutdown_tx.send(McpRuntimeCommand::Shutdown);
          let _ = worker_handle.join();
          warn!("Failed to start health + MCP server: {}", e);
          None
        }
      }
    } else {
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

  if let Some(tx) = mcp_runtime_shutdown {
    let _ = tx.send(McpRuntimeCommand::Shutdown);
  }
  if let Some(handle) = mcp_runtime_handle {
    let _ = handle.join();
  }

  info!("zeptobeam stopped");
}
