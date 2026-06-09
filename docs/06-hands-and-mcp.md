# 06 - Hands & MCP

_Hand providers, tool routing, MCP, sandbox lifecycle, and recovery._

## Contract

Hands are temporary execution environments. They are provisioned on first use,
reused while a session is active, and destroyed when the session reaches a
terminal state. The brain never talks to hands directly; it asks the
`ToolRouter` to execute a named tool with structured input.

Credentials must not be visible to generated code. Git, MCP, and external API
credentials are fetched or injected by trusted host-side code, not placed in
tool-call arguments.

## Provider Map

| Provider | Use | Notes |
|---|---|---|
| Local | Zero-setup tests and development | Uses a workspace directory and optional Docker support. |
| Docker | Local/containerized execution | Hardened by `moa-hands` and `moa-security` policies. |
| Daytona | Default cloud workspace provider | Supports pause/resume/destroy around idle sessions. |
| E2B | MicroVM isolation | Use for untrusted or security-sensitive execution. |
| MCP | External tools and SaaS integrations | Routed through `MCPClient` and the credential proxy. |

All providers implement the `HandProvider` trait from `moa-core`. Tool routing
code should depend on the trait, not on provider-specific clients.

## Tool Router

`moa-hands::ToolRouter` owns the execution decision:

1. Look up the tool in `ToolRegistry`.
2. Normalize and budget tool input/output.
3. Apply tool policy and approval rules.
4. Execute through one of:
   - built-in tool handler,
   - cached hand provider,
   - MCP client.
5. Record lineage and route the result back to the turn loop.

The default provider name is `local`. Workspace roots, active hand handles,
MCP clients, approval rules, session store hooks, and optional memory executor
hooks live behind async locks so the router can be shared across handlers.

## Registry

Tools come from three sources:

| Source | Examples | Execution |
|---|---|---|
| Built-ins | memory tools, search helpers | In-process Rust handlers |
| Hand tools | `bash`, `file_read`, `file_write`, `file_search` | Local/Docker/Daytona/E2B hand |
| MCP tools | GitHub, browser, database, SaaS tools | MCP transport |

Tool descriptors include name, schema, execution backend, risk level, approval
default, and output budget. The context pipeline injects only the currently
active subset to protect prompt budget and cache stability.

## Lifecycle

Active hands are keyed by session and provider. A first tool call provisions
the hand. Later tool calls reuse the handle if it is healthy. On terminal
session status, cancellation, failure, or panic cleanup, the orchestrator calls
`destroy_session_hands(session_id)`.

Provider implementations must make cleanup best-effort and observable. Failed
cleanup should warn through `tracing`, not panic or hide the terminal session
outcome.

## Recovery

Hand providers classify failures into:

| Class | Meaning | Router action |
|---|---|---|
| Retryable | Transient provider or transport failure | Retry according to tool policy |
| ReProvision | Handle is stale or sandbox died | Destroy/recreate hand, then retry when safe |
| Fatal | Input, policy, or non-recoverable provider error | Return failure to the turn loop |

`health_check(handle)` lets the router replace dead sandboxes before a user
tool call discovers the failure.

Tool calls must also declare their idempotency behavior:

- `Idempotent`: safe to retry.
- `IdempotentWithKey`: safe when the remote API supports the idempotency key.
- `NonIdempotent`: retry only when no remote side effect was confirmed.

## MCP

MCP is the primary protocol for external integrations. Supported transports are
stdio, SSE, and streamable HTTP. Startup discovers tool definitions through
MCP, then the router exposes the selected tools exactly like built-ins and hand
tools.

Credential handling is host-side:

1. The brain emits a normal tool call.
2. The MCP credential proxy resolves session-scoped access.
3. The proxy fetches real credentials from the configured vault.
4. The remote MCP request is enriched.
5. The result is returned with credentials stripped.

HTTP/SSE MCP servers get the strongest credential isolation because the proxy
can inject headers per request. Stdio MCP servers may still need startup
environment variables, so treat them as a weaker isolation boundary.

## Security Rules

- Never place provider secrets in tool-call input or model-visible context.
- Prefer MCP or host-side helpers for external APIs instead of raw shell
  commands with secrets.
- Use parsed command approval matching for shell tools.
- Keep generated-code sandboxes ephemeral by default.
- Destroy or pause hands when sessions stop so stale credentials and state do
  not linger.
