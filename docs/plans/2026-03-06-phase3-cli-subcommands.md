# Phase 3: CLI Subcommands — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `daemon`, `agent`, and `orchestrate` subcommands to `zeptobeam` so users can run one-shot agents, orchestrated multi-agent workflows, or the persistent daemon from the command line.

**Architecture:** Refactor the flat `Cli` struct into clap subcommands. Extract the current `main()` body into `run_daemon()`. Add `run_agent()` that creates a scheduler + bridge, spawns a single `AgentWorkerBehavior`, sends the user's message, ticks until the response arrives, and prints it. Add `run_orchestrate()` that does the same with an `OrchestratorBehavior`. Both one-shot modes use a Tokio-spawned `BridgeWorker` for real LLM calls.

**Tech Stack:** Rust, clap (derive), existing `agent_rt` scheduler/bridge/agent_worker/orchestration

---

### Task 1: Refactor CLI to subcommands + implement `agent` one-shot

**Files:**
- Modify: `zeptobeam/src/main.rs`

**Step 1: Rewrite the Cli struct with subcommands**

Replace the current `Cli` struct with:

```rust
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
```

**Step 2: Add `run_agent` function**

```rust
async fn run_agent(
    message: String,
    provider: String,
    model: Option<String>,
    system_prompt: Option<String>,
    format: String,
) {
    use erlangrt::agent_rt::{
        agent_worker::AgentWorkerBehavior,
        bridge::{create_bridge, BridgeWorker},
        process::ProcessStatus,
    };

    let (bridge_handle, worker) = create_bridge();
    let worker_handle = tokio::spawn(async move { worker.run().await });

    let mut scheduler = AgentScheduler::new();
    scheduler.set_bridge(bridge_handle);

    let behavior = Arc::new(AgentWorkerBehavior::new(
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
        .expect("spawn agent");
    scheduler.enqueue(pid);
    let _ = scheduler.send(pid, Message::Text(message));

    // Tick until agent responds or exits
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(120);
    loop {
        scheduler.tick();

        // Check if process exited
        let proc = scheduler.registry.lookup(&pid);
        if proc.is_none() || proc.map(|p| p.status == ProcessStatus::Exiting).unwrap_or(false) {
            break;
        }

        // Check if agent is no longer busy (got a response)
        // The AgentWorkerBehavior sets busy=false after IoResponse
        if let Some(p) = proc {
            if let Some(ref state) = p.state {
                if let Some(ws) = state.as_any().downcast_ref::<erlangrt::agent_rt::agent_worker::AgentWorkerState>() {
                    if !ws.busy && start.elapsed() > Duration::from_millis(500) {
                        break;
                    }
                }
            }
        }

        if start.elapsed() > timeout {
            eprintln!("Error: agent timed out after {:?}", timeout);
            break;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Collect dead letters or last response from observability
    let snapshots = scheduler.list_processes();
    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&snapshots).unwrap_or_default());
    }

    scheduler.graceful_shutdown(Duration::from_secs(2));
    worker_handle.abort();
}
```

**Step 3: Update main() to dispatch subcommands**

```rust
#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Agent { message, provider, model, system_prompt, format }) => {
            // Minimal tracing for one-shot
            let config = load_config(&cli.config).unwrap_or_default();
            init_tracing(&config, cli.log_level.as_deref());
            run_agent(message, provider, model, system_prompt, format).await;
        }
        Some(Commands::Orchestrate { message, max_concurrency, format }) => {
            let config = load_config(&cli.config).unwrap_or_default();
            init_tracing(&config, cli.log_level.as_deref());
            run_orchestrate(message, max_concurrency, format).await;
        }
        Some(Commands::Daemon { workers, bind }) | None => {
            // Default to daemon mode (backwards compatible)
            run_daemon(cli.config, cli.log_level, workers, bind).await;
        }
    }
}
```

The existing daemon code moves into `async fn run_daemon(...)`.

**Step 4: Build**

Run: `cargo +nightly build --bin zeptobeam`
Expected: Compiles

**Step 5: Verify help**

Run: `cargo +nightly run --bin zeptobeam -- --help`
Run: `cargo +nightly run --bin zeptobeam -- agent --help`
Run: `cargo +nightly run --bin zeptobeam -- daemon --help`

**Step 6: Commit**

```bash
git add zeptobeam/src/main.rs
git commit -m "feat(cli): add agent, orchestrate, daemon subcommands"
```

---

### Task 2: Final verification

**Step 1: Run all tests**

Run: `cargo +nightly test -p erlangrt --lib`
Expected: 449+ tests pass

**Step 2: Build**

Run: `cargo +nightly build --bin zeptobeam`

**Step 3: Verify all subcommand help outputs**

Run: `cargo +nightly run --bin zeptobeam -- --help`
Run: `cargo +nightly run --bin zeptobeam -- agent --help`
Run: `cargo +nightly run --bin zeptobeam -- orchestrate --help`
Run: `cargo +nightly run --bin zeptobeam -- daemon --help`

---

## Summary

| Task | What | New Tests |
|------|------|-----------|
| 1 | Subcommands + agent/orchestrate one-shot runners | 0 (CLI integration) |
| 2 | Final verification | 0 (regression) |
| **Total** | | **0 new tests** (CLI verified manually) |
