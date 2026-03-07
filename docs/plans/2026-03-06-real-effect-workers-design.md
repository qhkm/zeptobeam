# Real Effect Workers (Phase 2.6) — Design

## Goal

Replace the placeholder reactor with real async effect workers for LLM calls (OpenAI + Anthropic) and HTTP requests, with streaming support from the start.

## Architecture

The reactor's `execute_effect_once()` currently returns hardcoded placeholder responses. We introduce a `ProviderClient` trait abstracting LLM providers, with concrete `OpenAiClient` and `AnthropicClient` implementations. A real `reqwest`-based HTTP worker handles `EffectKind::Http`.

### Components

| File | Purpose |
|------|---------|
| `effects/mod.rs` | Module root, re-exports |
| `effects/provider.rs` | `ProviderClient` trait: `complete()`, `complete_stream()` |
| `effects/openai.rs` | OpenAI-compatible client (`/v1/chat/completions`) — covers OpenAI, Ollama, vLLM |
| `effects/anthropic.rs` | Anthropic Messages API (`/v1/messages`) |
| `effects/http_worker.rs` | Real HTTP client for `EffectKind::Http` (GET/POST/PUT/DELETE) |
| `effects/config.rs` | `ProviderConfig` with env var fallback + runtime override |
| `kernel/reactor.rs` (modified) | Route LlmCall/Http to real workers, handle streaming |

### New dependency

`reqwest` with `json` and `stream` features, plus `reqwest-eventsource` or manual SSE parsing for streaming.

## Provider Abstraction

```rust
#[async_trait]
pub trait ProviderClient: Send + Sync {
    async fn complete(&self, input: &serde_json::Value) -> Result<EffectResult, String>;
    async fn complete_stream(
        &self,
        input: &serde_json::Value,
        tx: tokio::sync::mpsc::Sender<EffectResult>,
    ) -> Result<(), String>;
}
```

Both `OpenAiClient` and `AnthropicClient` implement this trait. The `complete()` method is a convenience that buffers the full response. `complete_stream()` sends partial `EffectResult`s with `status: Streaming` through the channel, then a final `Succeeded`.

## Provider Resolution

The `model` field in the effect input determines the provider:
- `"gpt-*"`, `"o*"` → OpenAI
- `"claude-*"` → Anthropic
- Explicit `"provider": "openai"` or `"provider": "anthropic"` overrides
- Unknown models default to OpenAI-compatible endpoint

## Streaming Protocol

New variant added to `EffectStatus`:
```rust
pub enum EffectStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
    Streaming,  // NEW: partial result, more coming
}
```

Streaming flow:
1. Reactor spawns tokio task for LLM effect
2. As tokens arrive: `EffectResult { status: Streaming, output: { "delta": "token text" } }`
3. Each streaming result sent through completion channel → delivered to process mailbox
4. Final: `EffectResult { status: Succeeded, output: { "content": "full text", "tokens_used": N, "cost_microdollars": M } }`

Processes that don't need streaming simply ignore `Streaming` results and wait for `Succeeded`.

## Configuration

```rust
pub struct ProviderConfig {
    pub openai_api_key: Option<String>,
    pub openai_base_url: String,       // default: "https://api.openai.com/v1"
    pub anthropic_api_key: Option<String>,
    pub anthropic_base_url: String,    // default: "https://api.anthropic.com/v1"
}
```

Resolution order:
1. Runtime override via `SchedulerRuntime::with_provider_config(config)`
2. Environment variables: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`
3. Missing key → immediate `EffectResult::failure("API key not configured")`

## HTTP Effect Worker

For `EffectKind::Http`, the input JSON specifies:
```json
{
    "method": "GET|POST|PUT|DELETE",
    "url": "https://...",
    "headers": { "Authorization": "Bearer ..." },
    "body": { ... }
}
```

Response:
```json
{
    "status": 200,
    "headers": { "content-type": "application/json" },
    "body": "..."
}
```

## Error Handling

| Error | Behavior |
|-------|----------|
| Network error | Retryable via `RetryPolicy` |
| 429 (rate limit) | Retryable, respect `Retry-After` header |
| 401 (auth) | Not retryable, immediate failure |
| 500+ (server) | Retryable |
| Timeout | `EffectStatus::TimedOut` |
| Missing API key | Immediate failure, no retry |
| Stream interrupted | `EffectStatus::Failed` |

## Backward Compatibility

When no `ProviderConfig` is set and no env vars are present, the reactor falls back to placeholder responses (current behavior). All existing tests continue to pass without API keys.

## Testing Strategy

- **Unit tests:** Test request building, response parsing, error mapping using mock responses
- **Integration tests:** Behind `#[cfg(feature = "integration_tests")]`, skip if no API key
- **Reactor tests:** Verify streaming results flow through completion channel
- **Existing tests:** Must pass unchanged (placeholder fallback)
