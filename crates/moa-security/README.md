# moa-security

Runtime security enforcement for MOA tool execution: prompt-injection
defenses, tool action policies, MCP credential loading, and MCP egress
governance. See `docs/08-security.md` for the surrounding threat model.

## Structure

- `injection.rs` — prompt-injection heuristics (`inspect_input`), canary
  token injection and leak screening for tool inputs, and untrusted
  tool-output wrapping.
- `policies.rs` — tool action-policy evaluation (`ActionPolicies`), shell
  command parsing and glob matching, rule validation, and the
  `ActionPolicyRuleStore` abstraction.
- `mcp_credentials.rs` — `McpDeploymentCredentials`: fail-closed loading and
  header shaping for deployment-owned MCP credentials.
- `mcp_egress.rs` — `McpEgressGuard`: data-class egress governance for
  outbound MCP tool calls, since an external MCP server is a place where
  restricted data can leave the trust boundary.
