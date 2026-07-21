# moa-analytics

Safe generic analytics query surface: a static dataset catalog, request
validation, a parameterized SQL compiler, and execution backends for Postgres
and ClickHouse. Requests are validated against the catalog before compilation;
they never carry raw SQL.

## Structure

- `catalog` — static catalog of queryable analytics datasets and fields.
- `clickhouse_exec` — ClickHouse execution backend for compiled analytics
  queries (`AnalyticsClickHouseClient`).
- `compiler` — SQL compiler turning validated queries into parameterized
  statements (`AnalyticsCompiler`, `CompiledAnalyticsQuery`).
- `dialect` — SQL dialect selection and ClickHouse source/field mappings
  (`AnalyticsBackend`).
- `error` — error types for catalog and query handling.
- `executor` — `AnalyticsService` entrypoint used by edge route
  integration; bound to one backend at construction.
- `query` — request validation producing `ValidatedAnalyticsQuery`.
