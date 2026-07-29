# moa-migrations

Central Postgres migrations for MOA.

Runtime crates use SQLx for queries. They do not own embedded migration runners
or local `migrations/` directories. Production startup applies this crate once
through refinery.

## Files

`migrations/postgres/` holds the full ordered migration sequence — the baselines
below plus forward migrations (`V000302__...` onward). Key entry points:

- `migrations/postgres/V000001__session_baseline.sql`
- `migrations/postgres/V000101__auth_baseline.sql`
- `migrations/postgres/V000201__orchestrator_baseline.sql`
- `migrations/postgres/V000301__ocsf_baseline.sql`
- `sql/lineage_schema.sql` for the standalone lineage writer bootstrap path
- `sql/pgaudit.sql` for focused pgaudit smoke coverage
- `migration-ownership.toml` for table ownership

## Numbering

Version numbers are allocated, not compacted. The sequence is intentionally
sparse: the four baselines sit on `1 / 101 / 201 / 301` block boundaries, and
individual numbers are retired when a migration is deleted or when a
pre-allocated slot never ships.

Retired or never-allocated numbers: **332, 350, 352–357**.

Do not renumber to close a gap. Refinery records applied versions in its history
table, so renumbering invalidates recorded history in every environment that has
applied the sequence. For the same reason, never reuse a retired number that sits
below the current maximum — refinery does not back-apply a lower-numbered
migration, so it would silently never run on an existing database.

Take the next number above the current maximum. Gaps are expected and
`check-migrations` does not flag them.

## Rules

- New DDL goes through this crate.
- Do not add crate-local or service-local `migrations/` directories.
- Keep table ownership in `migration-ownership.toml`.
- Use `moa_migrations::run` as the only full-database setup path, including
  physical staging databases used by test templates.
- Use `run_auth_schema`, `run_orchestrator_schema`, and `run_ocsf_schema` only
  for their focused schema-isolated test surfaces.
- Run `cargo run -p xtask -- check-migrations` to enforce centralization and
  duplicate table ownership checks.

Keep baseline files clean and direct. Do not add compatibility shims for
obsolete table shapes. After a released database has recorded a migration
checksum, repair schema drift with a new forward migration rather than editing
that released file.
