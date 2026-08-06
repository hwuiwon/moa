# moa-migrations

Central PostgreSQL migrations for MOA.

Runtime crates use SQLx for queries but never apply migrations. The
orchestrator migration command runs this crate once with the database admin
URL; runtime startup validates that the complete history is already present.

## Contiguous history epoch

`migrations/postgres/` is a flat, append-only sequence with exactly one regular
file for every version from `V000001` through the current maximum, currently
`V000056`. Filenames
must match `V<six digits>__<lowercase_snake_case>.sql`.

`V000001__contiguous_history_epoch.sql` marks the current fresh-install-only
epoch. It intentionally owns no schema. Databases carrying the retired sparse
history, databases with product relations but no history, and partial or
checksum-divergent runtime histories are rejected. There is no compatibility
or translation path:

- Local development must use `make dev-wipe` and reset Restate durable state
  when adopting this epoch. A checksum mismatch is not upgradeable in place.
- Production rollout must build or reset a database from the complete current
  sequence and start with fresh Restate durable state before runtime services.

The 2026-08-03 hard-reset epoch removes the retired per-user token-vault tables
from the migrations that originally created them. V29 remains a no-op marker so
the sequence stays contiguous, typed connector origins are V53, the one-way
session `paused` to `idle` lifecycle cutover is V54, durable execution-plan
compensation is V55, and replay-stable ingestion apply outcomes are V56. Any database
that applied an earlier checksum for the rewritten files must be rebuilt; the
runner intentionally rejects that divergence before DDL.

Do not rewrite a migration after it has shipped in this epoch. Add the next
contiguous version (`N + 1`) for every future schema change; never leave gaps,
reuse a version, or add non-SQL files and subdirectories to the migration
directory.

V54 is a coordinated one-way data and Restate-state cutover. Stop edge
admission, drain active Session, turn, and worker invocations, and deploy the
exact immutable image as the dedicated migration-only Job. Its init container
owns the migration runner; its endpoint exposes only the raw
`Session/migrate_status_idle` path and cannot satisfy product Health or turn
admission. Bootstrap invokes that handler for every Postgres session id,
deregisters the migration endpoint, verifies zero exact `paused` lifecycle
values, and writes `session_status_idle_v54` to
`deployment_cutover_receipts`. Normal runtime init containers wait on that
receipt with the runtime database credential before the hard reader can start.
Do not emulate the admission stop by setting an admission limit to zero: zero
means unlimited in the runtime admission model.

## Ownership and focused fragments

`migration-ownership.toml` is an exact inventory of the logical tables created
by the final migration sequence. Every table has one owning crate and optional
readers; stale ownership rows are errors.

`run_auth_schema`, `run_orchestrator_schema`, and `run_ocsf_schema` replay
selected retained SQL as focused, schema-isolated test fragments. The auth
helper also extracts the explicitly marked credential-slot section from V50 and
the staged-operation audit section from V51, so the central migrations remain
the single source for that standalone vault DDL.
These helpers do not create or update refinery history and are not full-database
setup paths.

## Checks

Run:

```bash
cargo run -p xtask -- check-migrations
```

The check enforces the filename grammar, exact contiguous numbering, flat
regular-file layout, centralized migration directory, and ownership bijection.
