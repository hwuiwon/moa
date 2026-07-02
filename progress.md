# Progress

## 2026-07-02

- Read Rust, planning, test-authoring, Graphify, repo-rule, and architecture
  guidance.
- Queried Graphify for vector sync/retrieval relationships.
- Reviewed current vector sync, backend factory, graph write, fast-path, slow-path,
  retrieval, and test files.
- Created this implementation plan.
- Refactored graph post-commit sync into `VectorPostCommitSync` attached through
  `PostgresGraphStore`, leaving `VectorStore` focused on vector operations.
- Batched vector outbox drain by storage partition and operation with batch
  source-row fetch and batch processed/failed updates.
- Added `GraphMemoryMaint.sync_vectors` plus a default cron job for background
  outbox draining.
- Made brain retrieval scoped runtime construction async so it can select the
  configured vector backend once per cached runtime.
- Made fast-path read-side vector selection lazy; forget no longer constructs
  the configured read vector backend before graph invalidation.
- `cargo check -p moa-memory-vector -p moa-memory-graph -p moa-memory-ingest -p moa-brain -p moa-memory-lifecycle -p moa-orchestrator` passed after removing one unused import.
- Focused vector outbox, backend, graph post-commit, and fast-path lazy-vector
  tests passed.
- Error: attempted to pass two Cargo test filters in one command for `moa-brain`;
  Cargo rejected the second filter. Next run uses a module-level filter.
- Error: `moa-brain` test-only scoped runtime factories still used unqualified
  `Result` and synchronous `runtime_for_scope` calls after the async factory
  change. Fixed by qualifying `moa_core::Result` and awaiting the helper.
- Error: `user_scoped_runtime_is_not_cached_in_process_lifetime_map` used the
  real default runtime factory with a lazy unauthenticated pool after the factory
  began selecting configured vector backends. Fixed by injecting the no-DB
  counting runtime factory.
- Error: focused clippy flagged a useless `.into_iter()` on the vector sync
  delete-job chain. Removed it.
- Error: focused clippy flagged `apply_one_decision` for too many arguments
  after adding the vector cache. Fixed by introducing a private slow-path apply
  dependency context that owns the turn-local vector cache.
- Error: `slow_path_orchestration_db_memory` requires the `test-util` feature.
  Rerunning the same target with `--features test-util`.
- `cargo test -p moa-brain pipeline::memory::tests -- --nocapture` passed.
- `cargo test -p moa-orchestrator default_cron_jobs_include_vector_sync_drain -- --nocapture` passed.
- `cargo test -p moa-orchestrator graph_memory_maint --lib -- --nocapture` passed.
- `cargo test -p moa-migrations -- --nocapture` passed.
- `cargo test -p moa-memory-ingest --features test-util --test slow_path_orchestration_db_memory -- --nocapture` passed.
- Final focused clippy passed with `-D warnings`.
- `cargo build --workspace` passed.
- `git diff --check` passed.
- `graphify update .` passed; removed generated `graphify-out/2026-07-02/`
  backup directory.
