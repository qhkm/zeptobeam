use zeptobeam::agent_adapter;
use zeptobeam::config_loader;

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
  agent_worker::{AgentWorkerBehavior, AgentWorkerState},
  bridge::create_bridge,
  checkpoint::{CheckpointStore, FileCheckpointStore, InMemoryCheckpointStore},
  checkpoint_sqlite::SqliteCheckpointStore,
  config::{load_config, AgentConfig, AppConfig},
  mcp_types::ProcessInfo,
  observability::RuntimeMetrics,
  process::ProcessStatus,
  pruner::spawn_pruner,
  scheduler::AgentScheduler,
  server::{HealthServer, McpRuntimeOps, McpServerStateExt, ServerState},
  startup::spawn_configured_agents,
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
  agents: Vec<AgentConfig>,
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

    // Spawn configured agents
    match spawn_configured_agents(&mut scheduler, &agents) {
      Ok(spawned) => {
        for (pid, name) in &spawned {
          info!(name = %name, pid = pid.raw(), "agent started from config");
          scheduler.enqueue(*pid);
        }
        if !spawned.is_empty() {
          info!(count = spawned.len(), "configured agents spawned");
        }
      }
      Err(e) => {
        warn!(error = %e, "failed to spawn configured agents");
      }
    }
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
#[command(name = "zeptobeam", version, about = "Zeptoclaw agent runtime")]
struct Cli {
  #[command(subcommand)]
  command: Option<Commands>,

  /// Config file path
  #[arg(short, long, default_value = "zeptobeam.toml", global = true)]
  config: String,

  /// Override log level (trace|debug|info|warn|error)
  #[arg(short, long, global = true)]
  log_level: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
  /// Start the persistent daemon with HTTP/MCP server
  Daemon {
    /// Override worker count
    #[arg(short, long)]
    workers: Option<usize>,
    /// Override server bind address
    #[arg(short, long)]
    bind: Option<String>,
  },
  /// Run a one-shot agent interaction
  Agent {
    /// Message to send to the agent
    #[arg(short, long)]
    message: String,
    /// LLM provider name
    #[arg(short, long, default_value = "openrouter")]
    provider: String,
    /// Model identifier
    #[arg(long)]
    model: Option<String>,
    /// System prompt
    #[arg(short, long)]
    system_prompt: Option<String>,
    /// Output format (text or json)
    #[arg(short, long, default_value = "text")]
    format: String,
  },
  /// Start the daemon using the new zeptovm runtime
  DaemonV2 {
    /// Override worker count
    #[arg(short, long)]
    workers: Option<usize>,
    /// Override server bind address
    #[arg(short, long)]
    bind: Option<String>,
  },
  /// Run an orchestrated multi-agent workflow
  Orchestrate {
    /// Goal to decompose and execute
    #[arg(short, long)]
    message: String,
    /// Max concurrent workers
    #[arg(long, default_value = "3")]
    max_concurrency: usize,
    /// Output format (text or json)
    #[arg(short, long, default_value = "text")]
    format: String,
  },
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

async fn run_agent(
  message: String,
  provider: String,
  model: Option<String>,
  system_prompt: Option<String>,
  format: String,
) {
  let (bridge_handle, worker) = create_bridge();
  let worker_handle = tokio::spawn(async move { worker.run().await });

  let mut scheduler = AgentScheduler::new();
  scheduler.set_bridge(bridge_handle);

  let behavior = std::sync::Arc::new(AgentWorkerBehavior::new(
    "oneshot".into(),
    provider,
    model,
    system_prompt,
    vec![],
    None,
    None,
  ));

  let pid = scheduler
    .registry
    .spawn(behavior, serde_json::json!({}))
    .expect("failed to spawn agent");
  scheduler.enqueue(pid);
  let _ = scheduler.send(pid, Message::Text(message));

  // Tick until agent is no longer busy (got response) or exits
  let start = std::time::Instant::now();
  let timeout = Duration::from_secs(120);
  let mut got_first_tick = false;
  loop {
    let did_work = scheduler.tick();
    if did_work {
      got_first_tick = true;
    }

    // Check if process exited
    match scheduler.registry.lookup(&pid) {
      None => break,
      Some(p) if p.status == ProcessStatus::Exiting => break,
      Some(p) => {
        // Check if agent finished processing (no longer busy, after initial
        // processing)
        if got_first_tick {
          if let Some(ref state) = p.state {
            if let Some(ws) = state.as_any().downcast_ref::<AgentWorkerState>()
            {
              if !ws.busy {
                break;
              }
            }
          }
        }
      }
    }

    if start.elapsed() > timeout {
      eprintln!("Error: agent timed out after {:?}", timeout);
      std::process::exit(1);
    }

    tokio::time::sleep(Duration::from_millis(10)).await;
  }

  // Print result
  if format == "json" {
    let snapshots = scheduler.list_processes();
    println!(
      "{}",
      serde_json::to_string_pretty(&snapshots).unwrap_or_default()
    );
  } else {
    // The agent logged the response via tracing; for text mode just confirm
    // completion
    info!("agent completed");
  }

  scheduler.graceful_shutdown(Duration::from_secs(2));
  worker_handle.abort();
}

async fn run_orchestrate(
  message: String,
  max_concurrency: usize,
  format: String,
) {
  use erlangrt::agent_rt::orchestration::OrchestratorBehavior;

  let (bridge_handle, worker) = create_bridge();
  let worker_handle = tokio::spawn(async move { worker.run().await });

  let mut scheduler = AgentScheduler::new();
  scheduler.set_bridge(bridge_handle);

  let behavior = std::sync::Arc::new(OrchestratorBehavior {
    max_concurrency,
    ..OrchestratorBehavior::default()
  });

  let pid = scheduler
    .registry
    .spawn(behavior, serde_json::json!({}))
    .expect("failed to spawn orchestrator");
  scheduler.enqueue(pid);
  let _ = scheduler.send(
    pid,
    Message::Json(serde_json::json!({
      "goal": message,
    })),
  );

  // Tick until orchestrator completes
  let start = std::time::Instant::now();
  let timeout = Duration::from_secs(300);
  loop {
    scheduler.tick();

    match scheduler.registry.lookup(&pid) {
      None => break,
      Some(p) if p.status == ProcessStatus::Exiting => break,
      _ => {}
    }

    if scheduler.registry.count() == 0 {
      break;
    }

    if start.elapsed() > timeout {
      eprintln!("Error: orchestration timed out after {:?}", timeout);
      std::process::exit(1);
    }

    tokio::time::sleep(Duration::from_millis(10)).await;
  }

  if format == "json" {
    let snapshots = scheduler.list_processes();
    println!(
      "{}",
      serde_json::to_string_pretty(&snapshots).unwrap_or_default()
    );
  } else {
    info!("orchestration completed");
  }

  scheduler.graceful_shutdown(Duration::from_secs(2));
  worker_handle.abort();
}

async fn run_daemon(
  config_path: String,
  log_level: Option<String>,
  workers: Option<usize>,
  bind: Option<String>,
) {
  // Load config (optional — use defaults if file not found)
  let mut config = match load_config(&config_path) {
    Ok(c) => {
      eprintln!("Loaded config from {}", config_path);
      c
    }
    Err(_) => {
      eprintln!("Config file '{}' not found, using defaults", config_path);
      AppConfig::default()
    }
  };

  // Apply CLI overrides
  if let Some(workers) = workers {
    config.runtime.worker_count = workers;
  }
  if let Some(ref bind) = bind {
    config.server.bind = bind.clone();
  }

  // Init tracing
  init_tracing(&config, log_level.as_deref());

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
  let checkpoint_store: Arc<dyn CheckpointStore> =
    match config.checkpoint.store.as_str() {
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
        spawn_mcp_runtime_worker(process_count.clone(), config.agents.clone());
      let mcp_state = mcp_state.with_runtime_ops(runtime_ops);

      match HealthServer::start_with_mcp(&config.server.bind, mcp_state).await
      {
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

async fn run_daemon_v2(
  config_path: String,
  log_level: Option<String>,
  _workers: Option<usize>,
  _bind: Option<String>,
) {
  use std::sync::Arc;
  use zeptovm::durability::SqliteCheckpointStore as ZeptoCheckpointStore;
  use zeptovm::process::spawn_process;

  // Load config (same pattern as legacy daemon)
  let config = match load_config(&config_path) {
    Ok(c) => {
      eprintln!("Loaded config from {}", config_path);
      c
    }
    Err(_) => {
      eprintln!("Config file '{}' not found, using defaults", config_path);
      AppConfig::default()
    }
  };

  // Init tracing
  init_tracing(&config, log_level.as_deref());

  info!("zeptobeam v2 starting (zeptovm runtime)");

  // Resolve agents from config
  let resolved = match config_loader::resolve_agents_from_config(&config) {
    Ok(r) => r,
    Err(errors) => {
      for e in &errors {
        tracing::error!(agent = %e.agent_name, error = %e.message, "config validation failed");
      }
      eprintln!("Aborting: {} agent config error(s)", errors.len());
      std::process::exit(1);
    }
  };

  info!(
    agent_count = resolved.agents.len(),
    config_hash = resolved.config_hash,
    "resolved agents from config"
  );

  // Create checkpoint store
  let checkpoint_store = Arc::new(
    ZeptoCheckpointStore::new(&config.checkpoint.path)
      .expect("failed to open zeptovm checkpoint store"),
  );

  // Spawn a zeptovm process for each resolved agent
  let mut handles: Vec<(String, zeptovm::mailbox::ProcessHandle)> = Vec::new();
  let mut joins: Vec<(String, tokio::task::JoinHandle<zeptovm::process::ProcessExit>)> =
    Vec::new();

  for agent_cfg in resolved.agents {
    let name = agent_cfg.name.clone();
    let adapter = agent_adapter::create_agent_adapter(agent_cfg);
    let (pid, handle, join) =
      spawn_process(adapter, 64, None, Some(checkpoint_store.clone()));
    info!(agent = %name, pid = %pid, "spawned agent process");
    handles.push((name.clone(), handle));
    joins.push((name, join));
  }

  info!(
    process_count = handles.len(),
    "all agents spawned, waiting for shutdown signal"
  );

  // Wait for shutdown signal (SIGINT / SIGTERM)
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

  // Kill all processes
  for (name, handle) in &handles {
    handle.kill();
    info!(agent = %name, "sent kill to agent");
  }

  // Wait for all join handles
  for (name, join) in joins {
    match join.await {
      Ok(exit) => {
        info!(agent = %name, reason = %exit.reason, "agent exited");
      }
      Err(e) => {
        warn!(agent = %name, error = %e, "agent join failed");
      }
    }
  }

  info!("zeptobeam v2 stopped");
}

#[tokio::main]
async fn main() {
  let cli = Cli::parse();

  match cli.command {
    Some(Commands::Agent {
      message,
      provider,
      model,
      system_prompt,
      format,
    }) => {
      let config = load_config(&cli.config).unwrap_or_default();
      init_tracing(&config, cli.log_level.as_deref());
      run_agent(message, provider, model, system_prompt, format).await;
    }
    Some(Commands::Orchestrate {
      message,
      max_concurrency,
      format,
    }) => {
      let config = load_config(&cli.config).unwrap_or_default();
      init_tracing(&config, cli.log_level.as_deref());
      run_orchestrate(message, max_concurrency, format).await;
    }
    Some(Commands::Daemon { workers, bind }) => {
      run_daemon(cli.config, cli.log_level, workers, bind).await;
    }
    Some(Commands::DaemonV2 { workers, bind }) => {
      run_daemon_v2(cli.config, cli.log_level, workers, bind).await;
    }
    None => {
      run_daemon(cli.config, cli.log_level, None, None).await;
    }
  }
}
