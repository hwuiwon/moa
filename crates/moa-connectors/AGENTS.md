# Connector Instructions

Read `docs/08-security.md`, `docs/21-tenant-knowledge-base.md`, and
`docs/24-connectors-and-connections.md`. `moa-connectors` owns generic tenant
connection lifecycle, bindings, constrained HTTP execution, and invocation
ledgers; immutable definitions stay in `moa-artifacts`, knowledge projections in
`moa-knowledge`, and destination admission in `moa-security`. Do not add the
forbidden dependencies recorded in `docs/15-architecture-policy.md`.

Use `fast-pr`, `db-session`, and `db-memory` locally. Service/provider checks
need the named clean-E2E flags, services, credentials, and separate live
authorization recorded in the subsystem registry.
