# moa-authz

OpenFGA enforcement for MOA: the production authorization client, the canonical
`require_authz` check helpers, and the transactional outbox that keeps tuple
writes consistent with the data they protect.

## Structure

- `client.rs` — `FgaClient`, the production OpenFGA HTTP client
  (`FgaConfig`, `FgaTuple`).
- `require.rs` — `require_authz` / `require_authz_with_delegation` check
  helpers plus the security-audit hook (`configure_security_audit`).
- `outbox.rs` — transactional enqueue helpers that write tuple operations to
  `authz_outbox` inside the caller's data transaction.
- `poller.rs` — `OutboxPoller`, the background worker that drains
  `authz_outbox` rows into OpenFGA.
- `awakeable.rs` — shared `AwakeableResolver` trait for approval providers.
- `error.rs` — `AuthzError`.

## Rules

- Every handler that touches caller-owned data calls `require_authz` or
  `require_authz_with_delegation` with the right
  `(ObjectType, object_id, Relation)` before protected reads, or carries a
  one-line `// SAFETY: ...` comment above the handler signature (see
  `AGENTS.md`, Authorization).
- Agent-initiated writes use the delegated variant, not plain `require_authz`.
- Deleting a resource must enqueue the inverse tuple operation through the
  outbox in the same transaction.
