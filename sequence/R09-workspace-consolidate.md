# R09 — `Workspace` VO + `Consolidate` Workflow

## Purpose

Ship the `Workspace` Virtual Object and the `Consolidate` Workflow. `Workspace` holds per-workspace orchestration state (approval policy, sub-agent configs, scheduled consolidation metadata). `Consolidate` is a one-shot Workflow that runs the memory consolidation ("dream cycle") for a workspace on a schedule, writing page updates to the file-wiki.

End state: each workspace has a VO that schedules a daily `Consolidate` Workflow via delayed self-send. The workflow fetches recent events, identifies consolidation targets (entities, topics, relationships), updates memory pages via the `MemoryStore` Service, and emits a report event.

## Prerequisites

- R01–R08 complete.
- `moa-memory` crate has a stable pages API: `read_page`, `write_page`, `list_pages_for_entity`.
- Consolidation logic exists (or is defined in `docs/08-memory-consolidation.md`) and can be ported.

## Read before starting

- `docs/12-restate-architecture.md` — Consolidate and Workspace sections
- `docs/08-memory-consolidation.md` (if present) — consolidation algorithm
- `docs/04-file-wiki-memory.md` — memory page structure
- `moa-memory/src/lib.rs` — existing memory API

## Steps

### 1. Define `Workspace` VO

`moa-orchestrator/src/objects/workspace.rs`:

```rust
use restate_sdk::prelude::*;
use moa_core::types::*;

#[restate_sdk::object]
pub trait Workspace {
    /// Initialize a workspace with defaults. Called at workspace creation.
    async fn init(
        ctx: ObjectContext<'_>,
        config: WorkspaceConfig,
    ) -> Result<(), HandlerError>;

    /// Fetch or update approval policy.
    #[shared]
    async fn get_approval_policy(
        ctx: SharedObjectContext<'_>,
    ) -> Result<ApprovalPolicy, HandlerError>;

    async fn add_always_allow(
        ctx: ObjectContext<'_>,
        pattern: AlwaysAllowPattern,
    ) -> Result<(), HandlerError>;

    /// Schedule the next daily consolidation. Called from init and from consolidation_completed callback.
    async fn schedule_consolidation(
        ctx: ObjectContext<'_>,
    ) -> Result<(), HandlerError>;

    /// Called by the Consolidate workflow when it finishes. Triggers scheduling of the next run.
    async fn consolidation_completed(
        ctx: ObjectContext<'_>,
        report: ConsolidateReport,
    ) -> Result<(), HandlerError>;

    /// Read-only status query.
    #[shared]
    async fn status(
        ctx: SharedObjectContext<'_>,
    ) -> Result<WorkspaceStatus, HandlerError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub id: uuid::Uuid,
    pub tenant_id: uuid::Uuid,
    pub name: String,
    pub consolidation_hour_utc: u8,  // 0–23, hour to run daily consolidation
    pub approval_policy: ApprovalPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    pub last_consolidation_at: Option<chrono::DateTime<chrono::Utc>>,
    pub next_consolidation_at: Option<chrono::DateTime<chrono::Utc>>,
    pub consolidation_in_progress: bool,
    pub pages_count: u64,
}
```

### 2. Workspace state shape

```rust
const K_CONFIG: &str = "config";
const K_APPROVAL_POLICY: &str = "approval_policy";
const K_LAST_CONSOLIDATION: &str = "last_consolidation";
const K_NEXT_CONSOLIDATION: &str = "next_consolidation";
const K_CONSOLIDATION_IN_PROGRESS: &str = "consolidation_in_progress";
```

### 3. Schedule consolidation via delayed self-send

Restate VOs can send delayed messages to themselves or other handlers. The pattern:

```rust
async fn schedule_consolidation(ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
    let config: WorkspaceConfig = ctx.get(K_CONFIG).await?
        .ok_or_else(|| HandlerError::from("workspace not initialized"))?;

    // Compute next consolidation time (tomorrow at configured hour UTC).
    let now = chrono::Utc::now();
    let next = compute_next_consolidation_utc(now, config.consolidation_hour_utc);
    let delay = (next - now).to_std()
        .unwrap_or(std::time::Duration::from_secs(24 * 3600));

    // Deterministic jitter to avoid thundering herd at exact hour.
    // Derived from workspace UUID so replay is stable.
    let jitter_secs = (uuid_to_u64(config.id) % 600) as u64; // 0–599 seconds
    let delay_with_jitter = delay + std::time::Duration::from_secs(jitter_secs);

    ctx.set(K_NEXT_CONSOLIDATION, next + chrono::Duration::seconds(jitter_secs as i64));

    // Schedule the actual workflow start via delayed send.
    let workflow_id = format!("{}:{}", config.id, next.format("%Y-%m-%d"));
    ctx.workflow_client::<ConsolidateClient>(workflow_id)
        .run(ConsolidateRequest {
            workspace_id: config.id,
            tenant_id: config.tenant_id,
            target_date: next.date_naive(),
        })
        .send_with_delay(delay_with_jitter);

    tracing::info!(workspace = %config.id, next_run = %next, "consolidation scheduled");
    Ok(())
}

fn compute_next_consolidation_utc(now: chrono::DateTime<chrono::Utc>, hour: u8) -> chrono::DateTime<chrono::Utc> {
    let today_target = now.date_naive().and_hms_opt(hour as u32, 0, 0).unwrap().and_utc();
    if today_target > now {
        today_target
    } else {
        today_target + chrono::Duration::days(1)
    }
}
```

`send_with_delay` (or `.send().with_delay(...)`, exact SDK API per `restate-sdk` 0.8 docs) is durable: if the pod dies, Restate still fires the invocation at the scheduled time.

### 4. Define `Consolidate` Workflow

`moa-orchestrator/src/workflows/consolidate.rs`:

```rust
use restate_sdk::prelude::*;

#[restate_sdk::workflow]
pub trait Consolidate {
    async fn run(
        ctx: WorkflowContext<'_>,
        req: ConsolidateRequest,
    ) -> Result<ConsolidateReport, HandlerError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidateRequest {
    pub workspace_id: uuid::Uuid,
    pub tenant_id: uuid::Uuid,
    pub target_date: chrono::NaiveDate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidateReport {
    pub workspace_id: uuid::Uuid,
    pub ran_at: chrono::DateTime<chrono::Utc>,
    pub pages_examined: u64,
    pub pages_updated: u64,
    pub pages_created: u64,
    pub pages_merged: u64,
    pub entities_extracted: u64,
    pub llm_calls: u64,
    pub cost_cents: u64,
    pub duration_ms: u64,
    pub errors: Vec<String>,
}
```

### 5. Implement `Consolidate::run`

```rust
pub struct ConsolidateImpl;

impl Consolidate for ConsolidateImpl {
    async fn run(
        ctx: WorkflowContext<'_>,
        req: ConsolidateRequest,
    ) -> Result<ConsolidateReport, HandlerError> {
        let start = std::time::Instant::now();
        let ran_at = chrono::Utc::now();

        // Mark workspace as in-progress.
        ctx.object_client::<WorkspaceClient>(req.workspace_id.to_string())
            .set_in_progress(true)
            .call()
            .await?;

        let mut report = ConsolidateReport {
            workspace_id: req.workspace_id,
            ran_at,
            pages_examined: 0,
            pages_updated: 0,
            pages_created: 0,
            pages_merged: 0,
            entities_extracted: 0,
            llm_calls: 0,
            cost_cents: 0,
            duration_ms: 0,
            errors: vec![],
        };

        // 1. Fetch completed sessions from target_date.
        let sessions = ctx.run("fetch_recent_sessions", || async {
            fetch_sessions_for_consolidation(req.workspace_id, req.target_date).await
        })
        .await?;

        // 2. Extract consolidation candidates (entities, topics, relationships).
        for session_id in sessions {
            // Per-session extraction is a side effect; wrap each in ctx.run for journaling.
            let extraction = ctx.run(
                &format!("extract_session_{}", session_id),
                || async {
                    extract_consolidation_targets(session_id).await
                }
            )
            .await?;

            report.entities_extracted += extraction.entities.len() as u64;

            // 3. For each target, update the corresponding memory page.
            for target in extraction.entities {
                let update = ctx.service_client::<MemoryStoreClient>()
                    .consolidate_entity(target)
                    .call()
                    .await?;

                match update {
                    MemoryUpdate::Created => report.pages_created += 1,
                    MemoryUpdate::Updated => report.pages_updated += 1,
                    MemoryUpdate::Merged => report.pages_merged += 1,
                    MemoryUpdate::NoChange => {}
                }
                report.pages_examined += 1;
            }
        }

        report.duration_ms = start.elapsed().as_millis() as u64;

        // 4. Notify workspace that we're done; it will schedule the next run.
        ctx.object_client::<WorkspaceClient>(req.workspace_id.to_string())
            .consolidation_completed(report.clone())
            .send();

        Ok(report)
    }
}
```

### 6. Wire `consolidation_completed` to reschedule

```rust
async fn consolidation_completed(
    ctx: ObjectContext<'_>,
    report: ConsolidateReport,
) -> Result<(), HandlerError> {
    ctx.set(K_LAST_CONSOLIDATION, report.ran_at);
    ctx.set(K_CONSOLIDATION_IN_PROGRESS, false);

    // Log the report as a workspace event (optional: extend SessionStore or dedicated audit).
    tracing::info!(
        workspace = %report.workspace_id,
        pages_updated = report.pages_updated,
        entities = report.entities_extracted,
        duration_ms = report.duration_ms,
        "consolidation completed"
    );

    // Reschedule.
    ctx.object_client::<WorkspaceClient>(ctx.key())
        .schedule_consolidation()
        .send();

    Ok(())
}
```

The loop — `schedule_consolidation` → delayed workflow invocation → `consolidation_completed` → `schedule_consolidation` — is stable and durable. Pod restarts never miss a run.

### 7. `MemoryStore` Service (if not already built)

R02 focused on `SessionStore`. If `MemoryStore` isn't yet a Restate Service, scaffold it now:

```rust
#[restate_sdk::service]
pub trait MemoryStore {
    async fn read_page(ctx: Context<'_>, workspace_id: Uuid, page_path: String)
        -> Result<Option<MemoryPage>, HandlerError>;

    async fn write_page(ctx: Context<'_>, workspace_id: Uuid, page: MemoryPage)
        -> Result<(), HandlerError>;

    async fn search_pages(ctx: Context<'_>, workspace_id: Uuid, query: String, limit: u32)
        -> Result<Vec<MemoryPage>, HandlerError>;

    async fn consolidate_entity(ctx: Context<'_>, target: ConsolidationTarget)
        -> Result<MemoryUpdate, HandlerError>;
}
```

Back with Postgres + pgvector for search. Port existing `moa-memory` storage impl into the Service handlers. For R09, the important operation is `consolidate_entity` which is a multi-step journaled operation:

1. Read existing page(s) for entity.
2. Call LLM (`LLMGateway::complete`) to synthesize updates.
3. Write updated page(s).
4. Update embeddings.
5. Return diff type.

### 8. Wire into main

```rust
// main.rs
HttpServer::new(
    Endpoint::builder()
        .bind(/* ... existing bindings ... */)
        .bind(services::memory_store::MemoryStoreImpl { pool: pool.clone() }.serve())
        .bind(objects::workspace::WorkspaceImpl.serve())
        .bind(workflows::consolidate::ConsolidateImpl.serve())
        .build(),
)
.listen_and_serve(...)
.await
```

### 9. Bootstrap: initial scheduling

On workspace creation (by `moa-gateway` or another tenant-management flow), call `Workspace/init` + `Workspace/schedule_consolidation`. If no consolidation is currently scheduled, `schedule_consolidation` computes the next slot and fires the delayed send.

Backfill existing workspaces: write a one-time migration script that iterates all workspaces in Postgres and calls `Workspace/init` on each.

### 10. Unit tests

`moa-orchestrator/tests/workspace.rs`:

- `init_populates_config_and_schedules` — after init, next_consolidation is in state
- `compute_next_consolidation_hour_0` — 11pm UTC now with hour=0 → tomorrow 00:00
- `compute_next_consolidation_same_day` — 1am UTC now with hour=3 → today 03:00
- `jitter_is_deterministic` — same workspace ID → same jitter value

`moa-orchestrator/tests/consolidate.rs`:

- `run_with_no_sessions_returns_empty_report` — 0 pages examined
- `run_creates_pages_for_new_entities` — mock memory says no existing page → Created
- `run_resolves_workspace_awakeable_on_completion` — after run, workspace::consolidation_completed invoked

### 11. Integration test

`moa-orchestrator/tests/integration/consolidate_e2e.rs`:

- Create workspace, init.
- Run a short session to generate events.
- Manually invoke `Consolidate/run` (don't wait for scheduled).
- Assert memory pages created/updated in Postgres.
- Assert `Workspace::consolidation_completed` called.
- Assert next consolidation is scheduled with delay ≈ 24h.

## Files to create or modify

- `moa-orchestrator/src/objects/workspace.rs` — new
- `moa-orchestrator/src/workflows/mod.rs` — new module
- `moa-orchestrator/src/workflows/consolidate.rs` — new
- `moa-orchestrator/src/services/memory_store.rs` — new or expand existing
- `moa-orchestrator/src/main.rs` — wire new services and VOs
- `moa-memory/src/` — extract pure library functions for use by MemoryStore Service
- `moa-orchestrator/tests/workspace.rs`, `consolidate.rs` — unit tests
- `moa-orchestrator/tests/integration/consolidate_e2e.rs` — integration test

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] Unit tests pass.
- [ ] Integration test: consolidation runs end-to-end against a seeded workspace.
- [ ] `restate kv get Workspace/<ws_id>/next_consolidation` returns a future timestamp.
- [ ] Killing the orchestrator for 2 hours then restarting: scheduled consolidations still fire at the correct time.
- [ ] Running `Consolidate/run` with the same workflow_id twice: second call is a no-op (Workflow's runs-once-per-ID guarantee).
- [ ] Workspace `consolidation_completed` triggers `schedule_consolidation` for the next run.

## Notes

- **Why Consolidate is a Workflow, not a VO**: running consolidation twice for the same workspace-date corrupts memory. Workflow's runs-once-per-ID constraint is the correctness property. Key format `{workspace_id}:{YYYY-MM-DD}` makes re-runs for the same date impossible.
- **Jitter via UUID hash**: deterministic so replay is stable. The alternative (`ctx.rand()`) is also deterministic within an invocation but varies across invocations; UUID-derived jitter gives stability across schedule cycles.
- **Missed consolidation runs**: if the orchestrator was down at the scheduled time, Restate's delayed send still fires when the next pod is alive. Up to the pod to handle stale scheduled time (within 1 hour of target = normal; beyond that = log warning, still run).
- **MemoryStore service overlap**: parts of `MemoryStore` may already exist as library code in `moa-memory`. The service is a Restate wrapper that exposes a subset of the library as durable handlers. Keep the library functions; the service calls into them via `ctx.run()`.
- **Why not cron in Kubernetes**: Kubernetes CronJobs lack visibility into orchestrator state, don't coordinate with in-flight consolidations, and don't survive orchestrator outages gracefully. Restate's delayed send + workspace VO gives a cleaner model: every workspace schedules its own work, and the scheduler lives next to the workload.

## What R10 expects

- All handlers (Services, VOs, Workflows) registered and working locally.
- Integration test suite passes end-to-end.
- This is the last prompt before Kubernetes deployment. Before moving on, confirm: `moa-orchestrator` builds a minimal container image that runs all handlers on a local `restate-server`, and the full session-brain-subagent-consolidate test suite passes.
