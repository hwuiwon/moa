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
- `mcp_proxy.rs` — `MCPCredentialProxy`: per-call credential resolution for
  MCP-backed tool calls, backed by the durable tenant credential vault. The
  caller supplies the typed credential source and resolution context; the
  plaintext never outlives the header-shaping call.
- `mcp_egress.rs` — `McpEgressGuard`: data-class egress governance for
  outbound MCP tool calls, since an external MCP server is a place where
  restricted data can leave the trust boundary.
