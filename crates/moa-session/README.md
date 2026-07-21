# moa-session

Postgres-backed session storage for MOA. This crate owns the runtime
session-store queries (the `PostgresSessionStore` behind
`create_session_store`); database migrations live in `moa-migrations`.

## Modules

- `store` — PostgreSQL-backed `SessionStore` implementation.
- `queries` — query helpers for mapping PostgreSQL rows into MOA core types.
- `analytics` — typed analytics reads over session summary, tool summary, and
  rollup views.
- `blob` — blob storage and claim-check helpers for large session event
  payloads.
- `neon` — Neon API-backed checkpoint branch management.
- `testing` — shared Postgres test helpers for MOA crates (isolated test
  databases cloned from a cached template).
- `failpoints` (feature `failpoints`) — deterministic storage failpoints for
  chaos tests.

## Features

- `failpoints` — deterministic storage failpoints for chaos tests; never
  enabled in production builds.
