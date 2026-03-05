# Phase 7: MCP Integration Design

**Goal:** Add bidirectional Model Context Protocol (MCP) support — expose the runtime as an MCP server for external clients, and consume external MCP tool servers natively from within agents.

**Working directory:** `~/ios/zeptoclaw-rt`

**Prerequisite:** Phase 5 (config system, health server, CLI). Phase 3 (ToolFactory for pluggable tools).

**Core principle:** An agent with no MCP config behaves exactly like today. MCP activates only when configured.

---

## Architecture

Two subsystems, symmetric design:

```
External MCP Clients          zeptoclaw-rt               External MCP Servers
(Claude, Cursor, etc.)    +---------------------+     (GitHub, DB, filesystem)
         |                |                     |              |
         |  MCP protocol  |   MCP Server        |              |
         |--------------->|   (axum routes +    |              |
         |  (HTTP + stdio)|    stdio listener)   |              |
         |                |         |            |              |
         |                |    Runtime Core      |              |
         |                |   (scheduler,        |              |
         |                |    bridge,           |              |
         |                |    processes)        |              |
         |                |         |            |              |
         |                |   MCP Client        |  MCP protocol |
         |                |   (per-agent         |------------->|
         |                |    sessions)         |  (HTTP + stdio)
         |                +---------------------+              |
```

**MCP Server** — Extends existing axum HealthServer with MCP Streamable HTTP endpoints + optional stdio listener. Exposes runtime operations as MCP tools. Bearer token auth.

**MCP Client** — New McpClient that connects to configured external MCP servers. Wraps discovered remote tools as `Box<dyn Tool>` so agents use them transparently. One session per agent process.

**New files:**

```
lib-erlangrt/src/agent_rt/
+-- mcp_server.rs      (NEW -- MCP protocol handler, tool registry, session state)
+-- mcp_stdio.rs       (NEW -- stdio transport read/write loop)
+-- mcp_client.rs      (NEW -- McpClientManager, McpClientSession, McpRemoteTool)
+-- server.rs          (MODIFY -- mount /mcp routes on existing HealthServer)
+-- config.rs          (MODIFY -- add McpConfig section)
+-- tool_factory.rs    (MODIFY -- McpToolFactory wrapping DefaultToolFactory)
+-- bridge.rs          (MODIFY -- session lifecycle on agent spawn/terminate)
+-- mod.rs             (MODIFY -- declare new modules)
```

---

## 1. MCP Server

### Streamable HTTP Transport

New axum routes mounted on the existing HealthServer under `/mcp`:

- `POST /mcp` — Main MCP request endpoint (handles `initialize`, `tools/list`, `tools/call`)
- `GET /mcp` — SSE stream for server-to-client notifications (optional, for future use)
- `DELETE /mcp` — Session termination

### stdio Transport

Separate `McpStdioListener` that reads JSON-RPC from stdin, writes to stdout. Activated via CLI flag (`--mcp-stdio`). Shares the same handler logic as HTTP.

### Authentication

Middleware on `/mcp` routes checks `Authorization: Bearer <token>` against token from env var configured in TOML. stdio has no auth (trusted local pipe).

### Exposed MCP Tools (initial set)

| Tool | Description |
|------|-------------|
| `spawn_orchestration` | Start a new orchestration with a goal |
| `get_orchestration_status` | Check status/results of an orchestration |
| `list_processes` | List active agent processes |
| `send_message` | Send a message to a process by PID |
| `get_metrics` | Get runtime metrics snapshot |
| `cancel_process` | Terminate a process |

### Session Management

Server-side sessions tracked in `HashMap<String, McpSession>`. Each session gets a UUID. Sessions expire after configurable idle timeout (default: 3600s).

```rust
pub struct McpSession {
    id: String,
    created_at: std::time::Instant,
    last_active: std::time::Instant,
    initialized: bool,
}

pub struct McpServerState {
    sessions: HashMap<String, McpSession>,
    auth_token: Option<String>,
    session_timeout: Duration,
    runtime_handle: RuntimeHandle,  // access to scheduler, metrics, etc.
}
```

---

## 2. MCP Client / Tool Consumer

### McpClientManager

Reads `[[mcp.servers]]` from TOML config at startup. Manages connections to external MCP servers.

### Per-Agent Sessions

When an agent process needs MCP tools, the bridge creates/retrieves an `McpClientSession` for that `AgentPid`. Session lifecycle:

1. Agent spawns -> session created lazily on first MCP tool use
2. Agent calls MCP tool -> routed through session to remote server
3. Agent terminates -> session closed, child process killed (stdio) or connection dropped (HTTP)

### Remote Tools as Native Tools

Each discovered remote tool is wrapped in `McpRemoteTool` that implements the zeptoclaw `Tool` trait:

```rust
pub struct McpRemoteTool {
    server_name: String,
    tool_name: String,       // "github__create_issue"
    description: String,
    parameters: serde_json::Value,
    client: Arc<McpClientSession>,
}

impl Tool for McpRemoteTool {
    fn name(&self) -> &str { &self.tool_name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> Value { self.parameters.clone() }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        self.client.call_tool(&self.tool_name, args).await
    }
}
```

### McpToolFactory

Wraps `DefaultToolFactory` and appends MCP remote tools:

```rust
pub struct McpToolFactory {
    inner: DefaultToolFactory,
    mcp_manager: Arc<McpClientManager>,
}
```

When `build_tools` is called, it builds local tools from `DefaultToolFactory` then adds any MCP tools matching the whitelist. MCP tools are namespaced with server name prefix: `"github__create_issue"`.

### Transport Handling

- **stdio** — Spawn child process via `tokio::process::Command`, communicate via stdin/stdout JSON-RPC
- **HTTP** — POST to server URL, parse JSON-RPC responses. Uses existing `ureq` or `reqwest`.

---

## 3. MCP Protocol Implementation

### JSON-RPC 2.0

MCP uses JSON-RPC 2.0. Core message types:

```rust
pub struct JsonRpcRequest {
    pub jsonrpc: String,  // "2.0"
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}
```

### MCP Methods Handled (Server)

- `initialize` — Negotiate capabilities, return server info
- `tools/list` — Return available tools with schemas
- `tools/call` — Execute a tool, return result
- `ping` — Health check

### MCP Methods Called (Client)

- `initialize` — Handshake with remote server
- `tools/list` — Discover available tools
- `tools/call` — Call a remote tool

---

## 4. Config Integration

Extend `AppConfig` TOML:

```toml
[mcp.server]
enabled = false                          # opt-in
auth_token_env = "ZEPTOCLAW_MCP_TOKEN"   # env var holding bearer token
session_timeout_secs = 3600              # idle session expiry

[mcp.stdio]
enabled = false                          # also via --mcp-stdio CLI flag

[[mcp.servers]]                          # external MCP servers to consume
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "GITHUB_TOKEN" }

[[mcp.servers]]
name = "database"
transport = "http"
url = "http://localhost:8080/mcp"
auth_token_env = "DB_MCP_TOKEN"
timeout_ms = 30000
```

New config structs:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub server: McpServerConfig,
    pub stdio: McpStdioConfig,
    pub servers: Vec<McpServerEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub enabled: bool,
    pub auth_token_env: Option<String>,
    pub session_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpStdioConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: String,           // "stdio" or "http"
    pub command: Option<String>,     // for stdio
    pub args: Option<Vec<String>>,   // for stdio
    pub env: Option<HashMap<String, String>>,  // for stdio
    pub url: Option<String>,         // for http
    pub auth_token_env: Option<String>,  // for http
    pub timeout_ms: Option<u64>,
}
```

CLI flags on `zeptoclaw-rtd`:
- `--mcp-stdio` — Enable stdio MCP server transport
- `--mcp-server` — Enable HTTP MCP server (overrides config)

Defaults: MCP server disabled, stdio disabled, empty server list. No MCP config = identical to current behavior.

---

## 5. Implementation Order

Build bottom-up — each layer independently testable:

1. **JSON-RPC types** — Request/Response/Error structs, serialize/deserialize (pure data)
2. **MCP protocol types** — Initialize, ToolsList, ToolsCall request/response types (pure data)
3. **McpConfig** — Config structs + TOML parsing + tests
4. **MCP server handler** — Protocol logic: initialize, tools/list, tools/call (no transport)
5. **MCP server HTTP transport** — Mount on axum HealthServer with bearer auth middleware
6. **MCP server stdio transport** — stdin/stdout JSON-RPC read/write loop
7. **MCP server tools** — Implement the 6 runtime operation tools
8. **MCP client session** — JSON-RPC client for stdio and HTTP transports
9. **McpClientManager** — Config-driven session lifecycle, tool discovery
10. **McpRemoteTool** — Wrap remote tools as `Box<dyn Tool>`
11. **McpToolFactory** — Extend DefaultToolFactory with MCP tools
12. **Wire into bridge** — Session lifecycle on agent spawn/terminate
13. **Integration tests** — End-to-end server + client scenarios
14. **Update ROADMAP** — Mark Phase 7 as done

---

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Transports | Streamable HTTP + stdio | HTTP for daemon mode, stdio for CLI/local MCP clients |
| Auth (server) | Bearer token from env var | Simple, sufficient for single-tenant; upgradeable to OAuth |
| MCP server capabilities | Tools only | Most natural MCP primitive; Resources/Prompts can come later |
| MCP server discovery | Static TOML config | Predictable, auditable; matches Claude Code/Cursor pattern |
| Session management | One session per agent | Maps cleanly to BEAM process model; simple lifecycle |
| Remote tool naming | `servername__toolname` | Prevents collisions; matches MCP convention |
| Tool consumer integration | McpToolFactory wrapping DefaultToolFactory | Transparent to agents; MCP tools look like native tools |
| Session creation | Lazy (on first tool use) | Avoids connecting to servers that won't be used |
| stdio child process | tokio::process::Command | Non-blocking, fits async runtime |
| HTTP client | Existing ureq (sync in spawn_blocking) | Already a dependency; avoids adding reqwest |
| Protocol handler | Shared between HTTP and stdio | DRY; transport is just I/O framing |
| Backward compat | All features opt-in (disabled by default) | No config = identical to current behavior |

---

## Future Improvements

Decisions made now with natural upgrade paths:

| Current Decision | Future Upgrade | When |
|---|---|---|
| Bearer token auth | OAuth 2.1 with PKCE | Multi-tenant / public-facing deployment |
| Tools only (MCP server) | Add Resources + Prompts | When clients need read-only state views or prompt templates |
| Static config for MCP servers | Runtime registration API (`POST /mcp/servers`) | When dynamic server discovery is needed |
| One session per agent | Shared session pool with multiplexing | At scale when connection count becomes a bottleneck |
| Streamable HTTP + stdio | Additional transports (WebSocket, gRPC) | If ecosystem demands it |
| No MCP tool filtering per-agent | Per-agent MCP tool access policies | When security boundaries between agents matter |
| Server-side sessions in-memory | Persistent sessions (SQLite/Redis) | When session survival across restarts matters |
| ureq for HTTP client | reqwest with connection pooling | When concurrent MCP calls need better throughput |
| Single auth token for all clients | Per-client tokens or API keys | When multiple external clients need distinct access |
| No rate limiting on MCP server | Per-session rate limiting | When exposed to untrusted clients |
