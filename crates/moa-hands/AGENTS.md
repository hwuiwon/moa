# Hands Instructions

Read `docs/06-hands-and-mcp.md`, `docs/08-security.md`, and
`docs/25-sandbox-workspaces.md`. This crate owns governed tool routing and the
sandbox-workspace domain, repositories, fences, ledgers, checkpoints, and
provider adapters. Preserve operation-ledger order, provider I/O boundaries,
commit-before-release, receipt fencing, and fail-closed policy intersections.

Use `fast-pr`, `db-session`, and `db-memory` for focused checks. Sandbox service
and recovery lanes require their named Docker-backed fixture or E2E harness,
but deterministic lanes do not require live authorization. Set
`MOA_RUN_LIVE_E2E=1` only for an explicitly live target.
