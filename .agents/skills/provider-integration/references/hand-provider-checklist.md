# Hand Provider Checklist

For implementing `HandProvider` from `moa-core::traits`. Use this when adding a sandbox or runtime backend (Docker, Daytona, E2B, future runtimes) or when integrating an MCP server.

## Trait Surface

The trait lives at `crates/moa-core/src/traits/mod.rs` (function `HandProvider` near line 330). It exposes the operations the brain needs to dispatch tools to a sandbox: list available tools, execute a tool, return results, handle approval.

Reference implementations:

- `crates/moa-hands/src/adapters/local/` — runs commands on the host (development only)
- `crates/moa-hands/src/adapters/daytona/` — Daytona-managed workspaces
- `crates/moa-hands/src/adapters/e2b/` — E2B-hosted sandboxes (HTTP API)
- `crates/moa-hands/src/adapters/mcp/` — MCP server router; not a sandbox itself, but a hand-provider that fans out to MCP servers

## Two Categories of Hand Provider

The repo distinguishes:

1. **Sandbox providers** that execute arbitrary commands or code in an isolated environment. These implement `HandProvider` directly and route a fixed set of `BuiltInTool` variants (bash, edit_file, read_file, etc.) into sandbox primitives.

2. **MCP-based providers** that expose a discovered tool set from an MCP server. These also implement `HandProvider` but the tool set is dynamic and per-server.

The wiring patterns differ:

- For a new sandbox: implement the trait, declare which `BuiltInTool` variants you support, add a sandbox-kind tag for tool routing.
- For a new MCP server: usually no new code; configure a server URL and credentials, and the existing MCP router handles dispatch.

## Required Behaviors (Sandbox)

1. **Tool support declaration.** Explicitly list every `BuiltInTool` variant the sandbox supports. Wildcard support is forbidden; the security model depends on knowing the supported set at registration time.
2. **Workspace isolation.** Each session runs in its own sandbox instance. Two sessions for the same user must not see each other's files.
3. **Lifecycle.** The sandbox must support: create on session start, exec on tool dispatch, capture stdout/stderr/exit code, destroy on session end. The destroy step must run even on session failure.
4. **Approval integration.** Tools that require approval must surface the approval prompt before executing. The hand provider does not own approval policy; it exposes the operation that the orchestrator wraps in approval logic.
5. **Resource limits.** Sandbox runtime, memory, and disk limits must be configurable per session. Default values come from `docs/08-security.md`.
6. **Cancellation.** A long-running command must be cancellable mid-execution. The sandbox API needs to expose either a cancel token or a separate cancel call.

## Required Behaviors (MCP)

1. **Server discovery.** Read tool definitions from the MCP server at startup; cache the schema; expose tools via `HandProvider::list_tools`.
2. **Credential proxy.** MCP servers often need their own credentials (a Linear token, a Slack bot token, etc.). These flow through `moa-security`'s credential vault and are injected into MCP requests at dispatch, not stored in the MCP server config.
3. **Tool name namespacing.** MCP servers expose tools with names like `linear_create_issue`. Underscore-separated, prefixed with the server name. Do not let two MCP servers expose the same tool name without prefixing.
4. **Schema validation.** The MCP server defines tool schemas in JSON Schema. Validate tool arguments against the schema at the boundary; return a structured error on invalid arguments rather than dispatching.
5. **Connection lifecycle.** MCP connections may go down. The router must reconnect on transient failures and surface persistent failures to the brain as an unavailable-tool error.

## Test Patterns

For sandbox providers:

- Offline test that exercises the trait API against a mock sandbox (the simpler the mock, the better — `local` provider's tests are a good reference).
- Live test with `#[ignore]` and an env flag like `MOA_RUN_LIVE_DAYTONA_TESTS=1`.
- Lifecycle test that asserts sandbox creation, execution, and destruction in order.
- Concurrency test that runs two sessions and asserts isolation.

For MCP integrations:

- Offline test with a fake MCP server that responds to `tools/list` and `tools/call`.
- Live test against a real MCP server (`MOA_RUN_LIVE_MCP_<NAME>_TESTS=1`).
- Schema-validation test that asserts the router rejects malformed arguments before dispatch.

## Wiring Points

1. The hand-provider registry in `moa-hands` — add a sandbox kind or an MCP server config.
2. The brain's tool router — usually no code change for MCP; for sandboxes, the router needs to know which `BuiltInTool` variants the new sandbox supports.
3. The credential vault — register any new credential keys.
4. The approval policy table — if the new sandbox introduces tools that need approval (most do).

## Common Mistakes

- Hardcoding workspace paths inside the sandbox provider; use `WorkspaceId`-derived paths instead.
- Forgetting the destroy step on session failure; sandboxes leak.
- Treating MCP tool names as opaque; they need namespacing to prevent collisions.
- Implementing approval inside the hand provider; approval belongs to the orchestrator. The hand provider just dispatches.
- Skipping the schema validation for MCP; you'll see mysterious tool failures when the LLM hallucinates argument names.
