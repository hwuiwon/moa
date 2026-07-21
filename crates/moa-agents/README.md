# moa-agents

Tenant-configurable agent resolution and runtime policy locking for MOA. The
crate resolves installed and exact configured-agent revisions from artifact
storage and exposes the resolved runtime policy applied during execution.

## Structure

- `definition.rs` — database-facing configured-agent deployment pointers
  (`AgentInstallationPointer`).
- `policy.rs` — resolved runtime policy returned by the configured-agent
  resolver (`AgentRuntimePolicy`).
- `resolver.rs` — resolver for installed and exact configured-agent revisions
  (`AgentResolver`).
