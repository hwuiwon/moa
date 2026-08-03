# moa-migrations

Central PostgreSQL migrations for MOA.

Runtime crates use SQLx for queries but never apply migrations. The
orchestrator migration command runs this crate once with the database admin
URL; runtime startup validates that the complete history is already present.

## Contiguous history epoch

`migrations/postgres/` is a flat, append-only sequence with exactly one regular
file for every version from `V000001` through the current maximum, currently
`V000052`. Filenames
must match `V<six digits>__<lowercase_snake_case>.sql`.

`V000001__contiguous_history_epoch.sql` marks the current fresh-install-only
epoch. It intentionally owns no schema. Databases carrying the retired sparse
history, databases with product relations but no history, and partial or
checksum-divergent runtime histories are rejected. There is no compatibility
or translation path:

- Local development must use `make dev-wipe` when adopting this epoch.
- Production rollout must build or reset a database from the complete current
  sequence before starting runtime services.

Do not rewrite a migration after it has shipped in this epoch. Add the next
contiguous version (`N + 1`) for every future schema change; never leave gaps,
reuse a version, or add non-SQL files and subdirectories to the migration
directory.

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
