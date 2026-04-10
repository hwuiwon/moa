# Step 18: Daytona + E2B Hand Providers + MCP Client

## What this step is about
Cloud hand providers for container and microVM execution, plus the MCP client for external tool integration.

## Files to read
- `docs/06-hands-and-mcp.md` — DaytonaHandProvider, E2B, MCP client, credential proxy, lazy provisioning

## Goal
Brains can provision cloud sandboxes on demand. External tools (GitHub, databases, browsers) accessible via MCP servers. Credentials flow through a proxy — never visible to the sandbox.

## Tasks
1. **`moa-hands/src/daytona.rs`**: `DaytonaHandProvider` — provision via Daytona API, exec commands, lifecycle (pause/resume/destroy), auto-resume on tool call.
2. **`moa-hands/src/e2b.rs`**: `E2BHandProvider` — Firecracker microVM provisioning via E2B API.
3. **`moa-hands/src/mcp.rs`**: `MCPClient` — connect to MCP servers (stdio, SSE, HTTP transports), discover tools, call tools, handle responses.
4. **`moa-security/src/mcp_proxy.rs`**: `MCPCredentialProxy` — session-scoped opaque tokens, credential injection from vault, transparent to brain.
5. **Update `moa-hands/src/router.rs`**: Route MCP tool calls through the proxy. Add MCP tools to the registry at startup.
6. **Feature gates**: `daytona` and `e2b` features.

## Deliverables
`moa-hands/src/daytona.rs`, `moa-hands/src/e2b.rs`, `moa-hands/src/mcp.rs`, `moa-security/src/mcp_proxy.rs`

## Acceptance criteria
1. Daytona: Provision container, exec command, get output, destroy
2. E2B: Provision microVM, exec, destroy
3. MCP: Connect to stdio MCP server, list tools, call tool, get result
4. Credential proxy: Brain sends opaque token → proxy injects real credentials → external service called
5. Lazy provisioning: Hand not created until first tool call
6. Auto-resume: Stopped hand resumes on next tool call

## Tests
- Mock test: Daytona API mocked → verify provision/exec/destroy sequence
- Mock test: MCP stdio server → verify tool discovery and execution
- Unit test: Credential proxy creates session tokens, enriches requests, never leaks real credentials
- Integration test (requires Daytona API key): Provision, run `echo hello`, verify output, destroy

---

