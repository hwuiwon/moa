# moa-hands

Tool routing, hand provisioning, and built-in tools for MOA. Implements the
`HandProvider` execution surface: the `ToolRouter`/`ToolRegistry` core
dispatches agent tool calls to built-in tools, sandbox backends, and
MCP-discovered tools. See `docs/06-hands-and-mcp.md` for the architecture.

## Structure

- `core/` — tool routing core: registry, policy, dispatch, leases, lifecycle,
  output budgets, normalization, telemetry, and recovery.
- `adapters/` — hand provider adapters: `LocalHandProvider` (direct host
  execution with optional Docker sandboxes), `DaytonaHandProvider`,
  `E2BHandProvider`, and the `MCPClient` for remote MCP servers.
- `tools/` — built-in hand and memory tool implementations: bash, file
  read/write/edit/search/outline, grep, str_replace, session search, and
  memory tools.
