# moa-security

Runtime security enforcement for MOA tool execution: prompt-injection
defenses, tool action policies, MCP credential proxying, and MCP egress
governance. See `docs/08-security.md` for the surrounding threat model.

## Structure

- `injection.rs` — prompt-injection heuristics (`inspect_input`), canary
  token injection and leak screening for tool inputs, and untrusted
  tool-output wrapping.
- `policies.rs` — tool action-policy evaluation (`ActionPolicies`), shell
  command parsing and glob matching, rule validation, and the
  `ActionPolicyRuleStore` abstraction.
- `mcp_proxy.rs` — `MCPCredentialProxy`: session-scoped credential resolution
  for MCP-backed tool calls, with `EnvironmentCredentialVault` as a simple
  vault backend.
- `mcp_egress.rs` — `McpEgressGuard`: data-class egress governance for
  outbound MCP tool calls, since an external MCP server is a place where
  restricted data can leave the trust boundary.
