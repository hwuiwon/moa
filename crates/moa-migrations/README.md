# moa-migrations

Central Postgres migrations for MOA.

Runtime crates use SQLx for queries. They do not own embedded migration runners
or local `migrations/` directories. Production startup applies this crate once
through refinery.

## Files

- `migrations/postgres/V000001__session_baseline.sql`
- `migrations/postgres/V000101__auth_baseline.sql`
- `migrations/postgres/V000201__orchestrator_baseline.sql`
- `migrations/postgres/V000301__ocsf_baseline.sql`
- `sql/lineage_schema.sql` for the standalone lineage writer bootstrap path
- `sql/pgaudit.sql` for focused pgaudit smoke coverage
- `migration-ownership.toml` for table ownership

## Rules

- New DDL goes through this crate.
- Do not add crate-local or service-local `migrations/` directories.
- Keep table ownership in `migration-ownership.toml`.
- Use `moa_migrations::run` for full database setup.
- Use `moa_migrations::run_session_schema`, `run_auth_schema`,
  `run_orchestrator_schema`, and `run_ocsf_schema` for schema-isolated tests.
- Run `cargo run -p xtask -- check-migrations` to enforce centralization and
  duplicate table ownership checks.

Keep baseline files clean and direct. Do not add compatibility shims for
obsolete table shapes. After a released database has recorded a migration
checksum, repair schema drift with a new forward migration rather than editing
that released file.
