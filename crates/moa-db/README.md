# moa-db

Database helpers shared by MOA storage crates. The crate owns `ScopedConn`, a
Postgres transaction wrapper that installs MOA's row-level-security GUCs
(`moa.tenant_id`, `moa.storage_partition_id`, `moa.contact_id`,
`moa.cleared_barriers`, `moa.control_plane`) before any query runs.

## Entry points

- `ScopedConn::begin` — transaction scoped from an `RlsContext`
- `ScopedConn::begin_as_app` — same, optionally promoted to the `moa_app` role
- `ScopedConn::begin_tenant` / `begin_contact` — tenant- or contact-scoped
  shortcuts
- `ScopedConn::begin_control_plane` — explicit tenant control-plane
  transaction
- `ScopedConn::assume_app_role` — `SET LOCAL ROLE moa_app`, required before
  touching RLS-protected tables as the application role

## Rules

- Reads and writes against RLS-protected tables go through `ScopedConn` so
  the request scope is always installed before SQL executes.
- Information-barrier clearances fail closed: an empty or missing clearance
  list installs an empty clearance, and malformed tags (containing the comma
  delimiter) are dropped rather than split, which can only hide rows, never
  leak them.
