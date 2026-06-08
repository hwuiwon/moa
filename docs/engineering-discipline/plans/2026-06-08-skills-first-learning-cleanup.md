# Skills-First Learning Cleanup Implementation Plan

> **Worker note:** Execute this plan task-by-task using the run-plan skill. Each step uses checkbox (`- [ ]`) syntax for progress tracking.

**Goal:** Remove durable tenant intent assignment from MOA and make learning skills-first, segment-based, and outcome-driven.

**Architecture:** Task segments remain the durable unit of work, but they no longer carry tenant intent labels. The agent loop, query rewrite, memory retrieval, and skill injection make live decisions dynamically; durable learning records store task summaries, skills, tools, resolution, and memory updates. Query rewrite keeps a short-horizon task classification, renamed from intent to task kind, so it cannot be confused with a persistent taxonomy.

**Tech Stack:** Rust workspace, Restate services/workflows, Postgres migrations/materialized views, `sqlx`, `serde`, `tokio`, `tracing`.

**Work Scope:**
- **In scope:** Remove tenant intent taxonomy, intent discovery, intent manager service, online intent classifier, intent-specific segment fields/views, and intent-specific config/docs/tests. Preserve and relocate `LearningEntry`, task segments, skill distillation/improvement learning, resolution scoring, and skill outcome ranking.
- **Out of scope:** Changing graph memory ingestion semantics, changing the skill document format, changing provider routing, or adding a new background clustering system.

**Verification Strategy:**
- **Level:** integration plus workspace build.
- **Command:** `cargo fmt --all && cargo test -p moa-core -p moa-session -p moa-brain -p moa-orchestrator -p moa-skills -p moa-auth-providers -p moa-edge && cargo clippy --workspace --all-targets --all-features --locked -- -D warnings && cargo build --workspace && git diff --check`
- **What it validates:** The old intent taxonomy is gone, the skills-first segment/learning path still compiles and passes tests, and no stale public Rust references remain.

**Assumption:** This is a hard cleanup. Existing development databases may be reset or migrated destructively. Do not keep compatibility wrappers, deprecated aliases, no-op services, or old wire names.

---

## Target Model

Keep:
- `task_segments` as the durable work-unit table.
- `learning_log` as the append-only learning stream.
- `SegmentStarted` and `SegmentCompleted` events, without intent fields.
- Skill activation, skill distillation, skill improvement, memory consolidation, and resolution scoring.
- Query rewrite metadata for task boundaries, task summaries, suggested tools, clarification, and task kind.

Remove:
- Tenant intent taxonomy and global intent catalog.
- Intent assignment/classification on segments.
- Intent discovery workflow and intent manager service.
- Intent transition analytics.
- Intent-specific skill ranking and structural baselines.

Rename:
- `QueryIntent` to `TaskKind`.
- `QueryRewriteResult.intent` to `task_kind`.
- Query rewrite prompt/schema JSON key `intent` to `task_kind`.

---

### Task 1: Update Architecture Docs First

**Dependencies:** None.

**Files:**
- Modify: `docs/01-architecture-overview.md`
- Modify: `docs/02-brain-orchestration.md`
- Modify: `docs/05-session-event-log.md`
- Modify: `docs/07-context-pipeline.md`
- Modify: `docs/09-skills-and-learning.md`
- Modify: `docs/13-task-segmentation.md`
- Modify: `docs/14-multi-tenancy-and-learning.md`
- Modify: `docs/12-restate-architecture.md`
- Modify: `docs/analytics.md`
- Modify: `docs/sample-config.toml`
- Modify: `sequence/106-adaptive-intents-learning.md`

**Acceptance Criteria:**
- [ ] Docs no longer describe tenant intents, global intent catalog, intent assignment, or intent transitions as MOA architecture.
- [ ] Docs describe task segments as skill/outcome learning units.
- [ ] Docs describe `learning_log` as the durable learning stream for skills, memory, and resolution.
- [ ] Docs rename query rewrite intent to task kind.

**Steps:**
- [ ] Remove `Tenant intents`, `IntentManager`, and `IntentDiscovery` from architecture diagrams and service/workflow lists.
- [ ] Replace intent-specific learning sections with skills-first learning: task segments -> resolution scores -> learning log -> skill ranking and memory consolidation.
- [ ] Update analytics docs so `skill_resolution_rates` is tenant/skill-level and `segment_baselines` is tenant-level.
- [ ] Rewrite or retire `sequence/106-adaptive-intents-learning.md`; do not leave it as a future plan for tenant intent taxonomy.
- [ ] Remove `intents.*` config examples from `docs/sample-config.toml`.

Run:

```bash
rg -n "IntentManager|IntentDiscovery|tenant_intents|global_intent_catalog|intent_transitions|intent_label|intent_confidence|intent_classified|intent_discovered|intent_confirmed|QueryIntent|intent =" docs sequence
```

Expected: only removed-file references or clearly obsolete sequence filenames remain. No active architecture doc should describe persistent intent assignment.

---

### Task 2: Remove Durable Intent Types And Config From `moa-core`

**Dependencies:** Task 1.

**Files:**
- Create: `crates/moa-core/src/types/learning.rs`
- Modify: `crates/moa-core/src/types/mod.rs`
- Modify: `crates/moa-core/src/types/segments.rs`
- Modify: `crates/moa-core/src/types/resolution.rs`
- Modify: `crates/moa-core/src/events.rs`
- Modify: `crates/moa-core/src/wire.rs`
- Modify: `crates/moa-core/src/traits/mod.rs`
- Modify: `crates/moa-core/src/config/context.rs`
- Modify: `crates/moa-core/src/config/loader.rs`
- Modify: `crates/moa-core/src/config/mod.rs`
- Modify: `crates/moa-core/src/session_replay.rs`
- Delete: `crates/moa-core/src/types/intents.rs`

**Acceptance Criteria:**
- [ ] `TenantIntent`, `CatalogIntent`, `IntentStatus`, `IntentSource`, and `IntentConfig` no longer exist.
- [ ] `LearningEntry` and `TenantId` live in `types/learning.rs`.
- [ ] `TaskSegment`, `ActiveSegment`, `SegmentStarted`, `SegmentCompleted`, and session replay no longer include intent fields.
- [ ] `GetSegmentBaselineRequest` and `ListSkillResolutionRatesRequest` no longer accept `intent_label`.
- [ ] `SessionStore::get_segment_baseline` and `SessionStore::list_skill_resolution_rates` take only `tenant_id`.

**Steps:**
- [ ] Move `TenantId` and `LearningEntry` from `types/intents.rs` into `types/learning.rs`.
- [ ] Update `types/mod.rs` exports and remove intent taxonomy exports.
- [ ] Remove `intent_label` and `intent_confidence` from segment DTOs and events.
- [ ] Update doc comments from "tenant and optional intent" to "tenant".
- [ ] Delete `IntentConfig` and remove `MoaConfig.intents`.
- [ ] Remove loader defaults for `intents.enabled`, `intents.discovery_interval_hours`, `intents.min_segments`, `intents.max_segments_per_run`, and `intents.classification_threshold`.
- [ ] Update `session_replay.rs` size accounting and event handling for segment events without intent payload.

Run:

```bash
cargo test -p moa-core
```

Expected: `moa-core` tests pass and `rg -n "TenantIntent|CatalogIntent|IntentStatus|IntentSource|IntentConfig|intent_label|intent_confidence" crates/moa-core/src` returns no matches.

---

### Task 3: Hard-Clean Postgres Schema, Store APIs, And Analytics

**Dependencies:** Task 2.

**Files:**
- Create: `crates/moa-session/src/store/learning.rs`
- Modify: `crates/moa-session/src/store/mod.rs`
- Modify: `crates/moa-session/src/store/session_store.rs`
- Modify: `crates/moa-session/src/store/segments.rs`
- Modify: `crates/moa-session/src/queries/columns.rs`
- Modify: `crates/moa-session/src/queries/rows.rs`
- Modify: `crates/moa-session/src/queries/enums.rs`
- Modify: `crates/moa-session/src/queries/mod.rs`
- Modify: `crates/moa-session/migrations/postgres/008_task_segments.sql`
- Modify: `crates/moa-session/migrations/postgres/009_resolution_views.sql`
- Modify: `crates/moa-session/migrations/postgres/010_intents_learning_log.sql`
- Modify: `crates/moa-session/migrations/postgres/011_three_tier_rls.sql`
- Modify: `crates/moa-session/tests/postgres_store.rs`
- Delete: `crates/moa-session/src/store/intents.rs`

**Acceptance Criteria:**
- [ ] `task_segments` has no `intent_label` or `intent_confidence`.
- [ ] `tenant_intents` and `global_intent_catalog` tables are not created.
- [ ] `learning_log` is still created and RLS-applied.
- [ ] `skill_resolution_rates` groups by `tenant_id, skill_name`; it has no `intent_label` column.
- [ ] `segment_baselines` groups by `tenant_id`; it has no `intent_label` column.
- [ ] `intent_transitions` view is removed.
- [ ] Store APIs keep learning-log functions and delete taxonomy/catalog/classification functions.

**Steps:**
- [ ] Move `append_learning`, `list_learnings`, and `rollback_learning_batch` into `store/learning.rs`.
- [ ] Delete `create_intent`, `get_intent`, `list_intents`, `classify_segment`, `get_intent_by_embedding`, catalog, adoption, retroactive classification, and intent status methods.
- [ ] Update `store/segments.rs` insert/upsert SQL and row mapping to remove intent columns.
- [ ] Change `get_segment_baseline(&tenant_id)` to read tenant-level baseline.
- [ ] Change `list_skill_resolution_rates(&tenant_id)` to return tenant-level skill rates.
- [ ] Edit migrations in place as a hard break. Do not add compatibility columns or views.
- [ ] Update `postgres_store.rs`: delete intent taxonomy/catalog tests; add tests for tenant-level skill resolution rates, tenant-level segment baselines, and learning-log append/list/rollback.

Run:

```bash
cargo test -p moa-session
```

Expected: `moa-session` tests pass and these searches return no matches in `crates/moa-session`:

```bash
rg -n "tenant_intents|global_intent_catalog|intent_transitions|IntentStatus|IntentSource|TenantIntent|CatalogIntent|intent_label|intent_confidence" crates/moa-session
```

---

### Task 4: Remove Online Intent Classification From Orchestrator

**Dependencies:** Tasks 2 and 3.

**Files:**
- Modify: `crates/moa-orchestrator/src/main.rs`
- Modify: `crates/moa-orchestrator/src/services/mod.rs`
- Modify: `crates/moa-orchestrator/src/services/session_store/mod.rs`
- Modify: `crates/moa-orchestrator/src/services/session_store/inner.rs`
- Modify: `crates/moa-orchestrator/src/services/session_store/handlers.rs`
- Modify: `crates/moa-orchestrator/src/services/session_store/requests.rs`
- Modify: `crates/moa-orchestrator/src/objects/session/segments.rs`
- Modify: `crates/moa-orchestrator/src/objects/session/scoring.rs`
- Modify: `crates/moa-orchestrator/src/workflows/turn_execution.rs`
- Modify: `crates/moa-orchestrator/src/workflows/mod.rs`
- Modify: `crates/moa-orchestrator/tests/replay_determinism.rs`
- Modify: `crates/moa-orchestrator/tests/session_store.rs`
- Modify: `crates/moa-orchestrator/tests/turn_execution_smoke.rs`
- Delete: `crates/moa-orchestrator/src/services/intent_manager.rs`
- Delete: `crates/moa-orchestrator/src/workflows/intent_discovery.rs`

**Acceptance Criteria:**
- [ ] Restate endpoint binding no longer includes `IntentManager` or `IntentDiscovery`.
- [ ] `EXPECTED_SERVICE_NAMES` no longer lists `IntentManager` or `IntentDiscovery`.
- [ ] Session segment startup no longer embeds or classifies tenant intents.
- [ ] `TurnExecution` no longer imports or constructs `IntentClassifier`.
- [ ] Resolution scoring requests tenant-level baselines only.
- [ ] Intent discovery replay-determinism tests are deleted or replaced with tests for remaining workflows only.

**Steps:**
- [ ] Remove `intent_manager` and `intent_discovery` module declarations.
- [ ] Remove imports, constructor calls, and endpoint `.bind(...)` entries from `main.rs`.
- [ ] Remove `classify_started_segment` and related `IntentClassification` code from both session object and turn workflow paths.
- [ ] Simplify `ensure_current_segment` to create/persist task segments directly after segment transition.
- [ ] Update scoring helper signatures from `(tenant_id, intent_label)` to `(tenant_id)`.
- [ ] Update SessionStore Restate request/handler payloads to match core wire types without `intent_label`.
- [ ] Delete or rewrite intent-specific tests. Do not retain tests that only prove removed functionality.

Run:

```bash
cargo test -p moa-orchestrator
```

Expected: `moa-orchestrator` tests pass and this search returns no matches:

```bash
rg -n "IntentManager|IntentDiscovery|IntentClassifier|intent_classified|intent_discovered|intent_confirmed|intent_label|intent_confidence" crates/moa-orchestrator
```

---

### Task 5: Remove Brain Intent Module And Rename Query Rewrite Classification

**Dependencies:** Task 2.

**Files:**
- Modify: `crates/moa-brain/src/lib.rs`
- Modify: `crates/moa-brain/src/pipeline/segments.rs`
- Modify: `crates/moa-brain/src/pipeline/query_rewrite/prompt.rs`
- Modify: `crates/moa-brain/src/pipeline/query_rewrite/postprocess.rs`
- Modify: `crates/moa-brain/src/pipeline/query_rewrite/mod.rs`
- Modify: `crates/moa-brain/src/pipeline/query_rewrite/llm_call.rs`
- Modify: `crates/moa-brain/src/pipeline/skills/activation.rs`
- Modify: `crates/moa-brain/src/pipeline/skills/tier1_metadata.rs`
- Modify: `crates/moa-brain/tests/query_rewrite_offline.rs`
- Modify: `crates/moa-brain/tests/query_rewrite_live.rs`
- Delete: `crates/moa-brain/src/intents/mod.rs`
- Delete: `crates/moa-brain/src/intents/classifier.rs`

**Acceptance Criteria:**
- [ ] `moa-brain::intents` no longer exists.
- [ ] `QueryIntent` is renamed to `TaskKind` in `moa-core`; all brain code uses `task_kind`.
- [ ] Query rewrite model schema requires `task_kind`, not `intent`.
- [ ] Prompt text asks for task kind, not intent.
- [ ] Query rewrite tests assert `task_kind`.
- [ ] Segment tracker tests no longer construct intent-labeled segments.

**Steps:**
- [ ] Remove `pub mod intents` from `moa-brain/src/lib.rs`.
- [ ] Update query rewrite prompt and response parsing from `intent` to `task_kind`.
- [ ] Fail open to `TaskKind::Unknown` when `task_kind` is invalid or missing.
- [ ] Remove intent fields from segment transition structs and event conversion.
- [ ] Keep skill ranking based on keyword overlap, tenant-level skill resolution rates, normalized use count, and recency.
- [ ] Update snapshots or inline test expectations only where observable field names changed.

Run:

```bash
cargo test -p moa-brain
```

Expected: `moa-brain` tests pass and this search returns no matches:

```bash
rg -n "QueryIntent|\\.intent|\"intent\"|moa_brain::intents|IntentClassifier|intent_label|intent_confidence" crates/moa-brain crates/moa-core/src/types/query_rewrite.rs
```

If `"intent"` remains in prose for unrelated English text, inspect manually and remove only stale technical references.

---

### Task 6: Preserve Skills-First Learning Paths

**Dependencies:** Tasks 2, 3, 4, and 5.

**Files:**
- Modify: `crates/moa-skills/src/distiller.rs`
- Modify: `crates/moa-skills/src/regression.rs`
- Modify: `crates/moa-orchestrator/src/objects/session/scoring.rs`
- Modify: `crates/moa-orchestrator/src/workflows/consolidate.rs`
- Modify: `crates/moa-orchestrator/src/workflows/turn_execution.rs`
- Modify: `crates/moa-session/tests/postgres_store.rs`
- Modify: any compile-time callers of `LearningEntry`.

**Acceptance Criteria:**
- [ ] `skill_created`, `skill_improved`, `memory_updated`, and `resolution_scored` learning entries still append through `PostgresSessionStore`.
- [ ] `learning_log` list/rollback operations remain available where still needed.
- [ ] Skill ranking still consumes `SkillResolutionRate` values.
- [ ] No learning entry type writes `intent_*`.

**Steps:**
- [ ] Update imports from `moa_core::LearningEntry` if the export path changes.
- [ ] Ensure `append_learning` remains a `PostgresSessionStore` method after moving to `store/learning.rs`.
- [ ] Update scoring code to append `resolution_scored` without intent payload fields.
- [ ] Update skill distillation/regression tests if learning-log setup changed.
- [ ] Add one focused integration test proving a skill-created learning entry round-trips through the new learning module.

Run:

```bash
cargo test -p moa-skills -p moa-session -p moa-orchestrator
```

Expected: skills, learning-log, and orchestrator tests pass.

---

### Task 7: Remove Stale Tests, Fixtures, Docs, And Config References

**Dependencies:** Tasks 1 through 6.

**Files:**
- Modify: `Cargo.toml` files only if dependency cleanup is required.
- Modify: `crates/moa-brain/Cargo.toml`
- Modify: `crates/moa-orchestrator/Cargo.toml`
- Modify: `crates/moa-session/Cargo.toml`
- Modify: remaining tests and snapshots surfaced by search.
- Delete: any tests that only cover removed tenant intent behavior.

**Acceptance Criteria:**
- [ ] No test asserts tenant intent taxonomy behavior.
- [ ] No fixture contains required `intent_label`, `intent_confidence`, `tenant_intents`, or `global_intent_catalog`.
- [ ] Unused dependencies introduced solely for intent classification are removed.
- [ ] `cargo tree -p moa-brain` and `cargo tree -p moa-orchestrator` do not include dependencies only needed by deleted intent modules, unless another module uses them.

**Steps:**
- [ ] Run the stale-reference search below and resolve every match.
- [ ] For each deleted test, verify it fails AGENTS criterion D or A because it only covers removed behavior.
- [ ] Remove unused imports and dependencies after compilation points them out.
- [ ] Update snapshots after reviewing each diff manually.

Run:

```bash
rg -n "TenantIntent|CatalogIntent|IntentStatus|IntentSource|IntentConfig|IntentManager|IntentDiscovery|IntentClassifier|tenant_intents|global_intent_catalog|intent_transitions|intent_label|intent_confidence|intent_classified|intent_discovered|intent_confirmed|QueryIntent|classification_threshold|intents\\." . --glob '!docs/engineering-discipline/plans/**' --glob '!task_plan.md' --glob '!findings.md' --glob '!progress.md'
```

Expected: no matches.

---

### Task 8: Add Replacement Behavioral Tests

**Dependencies:** Tasks 1 through 7.

**Files:**
- Modify: `crates/moa-session/tests/postgres_store.rs`
- Modify: `crates/moa-brain/src/pipeline/segments.rs`
- Modify: `crates/moa-brain/tests/query_rewrite_offline.rs`
- Modify: `crates/moa-orchestrator/tests/turn_execution_smoke.rs`
- Modify: `crates/moa-orchestrator/tests/session_store.rs`

**Acceptance Criteria:**
- [ ] Tests pin tenant-level skill resolution rates without intent grouping.
- [ ] Tests pin tenant-level structural baselines without intent grouping.
- [ ] Tests pin segment creation without intent fields.
- [ ] Tests pin query rewrite `task_kind` metadata.
- [ ] Tests pin turn execution does not append `intent_classified` learning records.

**Steps:**
- [ ] In `postgres_store.rs`, add a test with two resolved segments using the same skill and assert one tenant-level `SkillResolutionRate` row with exact `uses`, `resolution_rate`, `avg_token_cost`, and `avg_turn_count`.
- [ ] In `postgres_store.rs`, add a baseline test with completed segments and assert tenant-level baseline exact sample count and approximate averages.
- [ ] In `pipeline/segments.rs`, update unit tests to assert started/completed events contain segment id, index, task summary, and previous segment id only.
- [ ] In query rewrite tests, assert valid `task_kind` values parse and invalid values fail open to `TaskKind::Unknown`.
- [ ] In orchestrator turn smoke tests, inspect persisted learning entries after a turn and assert no `learning_type` starts with `intent_`.

Run:

```bash
cargo test -p moa-session postgres_store -- --nocapture
cargo test -p moa-brain query_rewrite -- --nocapture
cargo test -p moa-orchestrator turn_execution -- --nocapture
```

Expected: focused tests pass and would fail if intent grouping or intent learning writes return.

---

### Task 9: Full Workspace Verification

**Dependencies:** All preceding tasks.

**Files:** None, read-only verification.

**Acceptance Criteria:**
- [ ] Formatting passes.
- [ ] Focused package tests pass.
- [ ] Clippy passes with warnings denied.
- [ ] Workspace build passes.
- [ ] Diff check passes.
- [ ] Stale intent reference search is clean.

**Steps:**
- [ ] Run:

```bash
cargo fmt --all
cargo test -p moa-core -p moa-session -p moa-brain -p moa-orchestrator -p moa-skills
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
git diff --check
rg -n "TenantIntent|CatalogIntent|IntentStatus|IntentSource|IntentConfig|IntentManager|IntentDiscovery|IntentClassifier|tenant_intents|global_intent_catalog|intent_transitions|intent_label|intent_confidence|intent_classified|intent_discovered|intent_confirmed|QueryIntent|classification_threshold|intents\\." crates docs sequence --glob '!docs/engineering-discipline/plans/**'
```

Expected:
- All commands succeed.
- Final `rg` has no matches.

---

### Task 10: Add Configurable Disabled Authentication

**Dependencies:** Auth provider config and edge routing.

**Files:**
- Create: `crates/moa-auth/providers/src/disabled.rs`
- Modify: `crates/moa-core/src/config/auth.rs`
- Modify: `crates/moa-core/src/traits/auth.rs`
- Modify: `crates/moa-auth/providers/src/bundle.rs`
- Modify: `crates/moa-auth/providers/src/lib.rs`
- Modify: `crates/moa-edge/src/routes.rs`
- Modify: `crates/moa-orchestrator/src/config.rs`
- Modify: auth docs and sample config.

**Acceptance Criteria:**
- [x] `auth.provider = "disabled"` and `MOA__AUTH__PROVIDER=disabled` select a disabled auth provider.
- [x] `"none"` is accepted as an alias for disabled auth.
- [x] Disabled auth authenticates requests as a fixed service identity.
- [x] Edge requests without `Authorization` are accepted only when the configured provider reports that credentials are optional.
- [x] Strict auth providers still reject requests with missing credentials.
- [x] Config, provider bundle, and edge policy tests pin the behavior.

**Verification:**

```bash
env -u MOA__AUTH__AUTH0__DOMAIN -u MOA__AUTH__AUTH0__AUDIENCE -u MOA_RUN_LIVE_PROVIDER_TESTS \
  /opt/homebrew/bin/timeout 420 cargo test -p moa-core -p moa-auth-providers -p moa-edge -p moa-orchestrator
```

Expected: all tests pass.

---

## Self-Review Checklist

- [ ] No compatibility wrappers or aliases remain.
- [ ] No no-op Restate services remain for removed intent surfaces.
- [ ] Learning log is preserved and has tests.
- [ ] Task segmentation is preserved and has tests.
- [ ] Skill ranking still has outcome-based signal.
- [ ] Query rewrite uses task kind, not intent.
- [ ] Docs match code and do not describe removed architecture.
