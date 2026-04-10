# Step 35: Neon Branching + Cost Optimization

## What this step is about
Neon's copy-on-write database branching enables agent state checkpointing — create a branch before risky operations, discard on failure, merge on success. However, branches cost $1.50/month each on paid plans (10 included free), so the design must be cost-conscious: use branches as ephemeral checkpoints with automatic cleanup, not as permanent per-workspace isolation.

This step adds an optional `NeonBranchManager` that the orchestrator can use for session-level checkpointing, plus configuration for Neon's serverless optimizations (scale-to-zero, connection pooling via their pooler endpoint).

## Files to read
- `moa-session/src/lib.rs` — `PostgresSessionStore` from step 34
- `moa-core/src/config.rs` — database config section
- `moa-core/src/traits.rs` — `SessionStore` trait
- `moa-orchestrator/src/local.rs` — session lifecycle, where checkpointing would trigger
- `docs/05-session-event-log.md` — compaction and checkpoint concept
- `docs/08-security.md` — defense-in-depth, reversibility

## Goal
The orchestrator can snapshot agent state (sessions + events) before a high-risk operation via Neon branching, and roll back if the operation fails. Branches are ephemeral (auto-cleaned after a configurable TTL). Normal workspace scoping continues to use column-level filtering (`workspace_id`), NOT branches.

## Rules

### Cost optimization — critical
- Do NOT create one branch per workspace. Use the existing `workspace_id` column for workspace isolation. This is how Turso works too — no architectural difference.
- Branches are for **ephemeral checkpoints only**: snapshot before risky multi-tool operations, discard within hours/days. Think git stash, not git branch.
- Auto-delete branches older than a configurable TTL (default: 24 hours). The cleanup runs as a cron job (via `schedule_cron`).
- Track active branch count. Warn when approaching the included limit (10 on Free/Launch). Hard-cap at a configurable maximum (default: 5 active branches) to prevent runaway costs.
- Log branch creation/deletion with cost context so users understand the billing impact.

### Architecture
- Neon branching is accessed via the **Neon API** (REST), not via SQL. It requires a Neon API key and project ID — separate from the database connection.
- The `NeonBranchManager` is optional and independent of `PostgresSessionStore`. Postgres works fine without Neon branching (plain Postgres, Supabase, RDS, self-hosted).
- Branching creates a new database endpoint (connection string) that is an instant copy-on-write fork of the parent. The branch database is a full Postgres instance accessible via a different connection string.
- The branch manager should be exposed as a standalone utility, not baked into `SessionStore`. The orchestrator decides WHEN to checkpoint; the branch manager handles HOW.

## Tasks

### 1. Define the `BranchManager` trait in `moa-core`
```rust
/// Optional database-level state checkpointing.
#[async_trait]
pub trait BranchManager: Send + Sync {
    /// Creates a checkpoint branch. Returns a handle for rollback or cleanup.
    async fn create_checkpoint(
        &self,
        label: &str,
        session_id: Option<SessionId>,
    ) -> Result<CheckpointHandle>;

    /// Rolls back to a checkpoint, discarding all changes since it was created.
    /// The current database state is replaced with the checkpoint state.
    async fn rollback_to(&self, handle: &CheckpointHandle) -> Result<()>;

    /// Discards a checkpoint branch (the current state is kept).
    async fn discard_checkpoint(&self, handle: &CheckpointHandle) -> Result<()>;

    /// Lists active checkpoints.
    async fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>>;

    /// Cleans up expired checkpoints older than the configured TTL.
    async fn cleanup_expired(&self) -> Result<u32>;
}

pub struct CheckpointHandle {
    pub id: String,           // Neon branch ID
    pub label: String,        // Human-readable label
    pub connection_url: String, // Connection string to the branch
    pub created_at: DateTime<Utc>,
    pub session_id: Option<SessionId>,
}

pub struct CheckpointInfo {
    pub handle: CheckpointHandle,
    pub size_bytes: Option<u64>,
    pub parent_branch: String,
}
```

### 2. Implement `NeonBranchManager`
Create `moa-session/src/neon.rs`:

```rust
pub struct NeonBranchManager {
    api_key: String,
    project_id: String,
    parent_branch_id: String,  // "main" branch
    http_client: reqwest::Client,
    max_branches: usize,       // default: 5
    ttl: Duration,             // default: 24 hours
}
```

**Neon API calls** (all REST, JSON):
- `POST /projects/{project_id}/branches` — create branch
- `DELETE /projects/{project_id}/branches/{branch_id}` — delete branch
- `GET /projects/{project_id}/branches` — list branches
- `GET /projects/{project_id}/branches/{branch_id}` — get branch details + endpoint

**`create_checkpoint`:**
1. Check active branch count against `max_branches`. If at limit, clean up expired first. If still at limit, return error.
2. Call Neon API to create a branch from the parent branch at the current point in time.
3. Wait for the branch endpoint to become active (poll status).
4. Return the `CheckpointHandle` with the branch's connection URL.

**`rollback_to`:**
1. This is the destructive operation. Two strategies:
   - **Strategy A (recommended)**: Delete the main branch's data since the checkpoint. This is NOT natively supported by Neon — you'd need to: create a NEW branch from the checkpoint, swap the application's connection to the new branch, delete the old main branch, rename the new branch. This is complex.
   - **Strategy B (simpler)**: Just provide the checkpoint's connection URL. The orchestrator reconnects to the checkpoint branch and continues from there. The "main" branch with the bad state is discarded later.
2. Implement Strategy B for now. The orchestrator swaps its `PostgresSessionStore` connection to the checkpoint branch's URL.

**`discard_checkpoint`:**
1. Call Neon API to delete the branch.
2. Remove from local tracking.

**`cleanup_expired`:**
1. List all branches via Neon API.
2. Filter branches created by MOA (use a naming convention: `moa-checkpoint-{label}-{timestamp}`).
3. Delete any older than `ttl`.
4. Return count of deleted branches.

### 3. Add Neon config fields
```toml
[database.neon]
enabled = false                    # opt-in
api_key_env = "NEON_API_KEY"
project_id = ""
parent_branch_id = "main"         # or "br-xxx"
max_checkpoints = 5               # hard cap on active branches
checkpoint_ttl_hours = 24         # auto-cleanup threshold
```

### 4. Wire checkpoint cleanup into the cron scheduler
In the orchestrator's cron setup (or wherever `schedule_cron` is called):

```rust
if config.database.neon.enabled {
    let branch_manager = NeonBranchManager::from_config(config)?;
    orchestrator.schedule_cron(CronSpec {
        schedule: "0 */6 * * *".to_string(), // every 6 hours
        task: Box::new(move || {
            let bm = branch_manager.clone();
            async move {
                match bm.cleanup_expired().await {
                    Ok(count) if count > 0 => tracing::info!(count, "cleaned up expired checkpoint branches"),
                    Ok(_) => {},
                    Err(e) => tracing::warn!(error = %e, "checkpoint cleanup failed"),
                }
            }
        }),
    }).await?;
}
```

### 5. Expose checkpointing as brain-accessible (optional)
The brain doesn't need to call branching directly. The orchestrator can checkpoint automatically before high-risk operations:

```rust
// In the orchestrator, before executing a high-risk tool:
if let Some(branch_mgr) = &self.branch_manager {
    if tool_risk_level == RiskLevel::High {
        let checkpoint = branch_mgr.create_checkpoint(
            &format!("pre-{}", tool_name),
            Some(session_id),
        ).await?;
        // Store checkpoint handle in session metadata for potential rollback
    }
}
```

This is a natural extension point but NOT required in this step. Document it as a future hook.

### 6. Add a CLI command for manual checkpoint management
```bash
moa checkpoint create "before-deploy"    # create named checkpoint
moa checkpoint list                      # list active checkpoints
moa checkpoint rollback <id>             # rollback to checkpoint
moa checkpoint cleanup                   # force cleanup expired
```

This is useful for debugging and manual intervention. Implement as subcommands in `moa-cli`.

## Deliverables
```
moa-core/src/traits.rs                    # BranchManager trait, CheckpointHandle
moa-core/src/config.rs                    # Neon config section
moa-session/src/neon.rs          # NeonBranchManager implementation
moa-session/src/lib.rs           # Re-export NeonBranchManager
moa-orchestrator/src/local.rs             # Checkpoint cleanup cron (if neon enabled)
moa-cli/src/main.rs                       # checkpoint subcommands
```

## Acceptance criteria
1. `NeonBranchManager` can create, list, and delete checkpoint branches via the Neon API.
2. Checkpoint branches are auto-cleaned after the configured TTL.
3. Active branch count is hard-capped at `max_checkpoints` (default 5).
4. Branch creation logs the cost context ("1 of 5 checkpoint branches active").
5. The feature is fully opt-in: Postgres works without Neon branching. Turso is unaffected.
6. `moa checkpoint list` shows active checkpoints with age and label.
7. `moa checkpoint cleanup` removes expired branches.
8. All existing tests pass (branching is additive, default-off).

## Tests

**Unit tests (mocked Neon API):**
- `create_checkpoint` sends correct API request, returns handle
- `create_checkpoint` at max capacity → returns error (not silent)
- `cleanup_expired` deletes branches older than TTL, keeps newer ones
- `discard_checkpoint` sends delete API request
- Branch naming convention: `moa-checkpoint-{label}-{timestamp}` format validated
- Non-MOA branches are NOT touched by cleanup

**Integration tests (require Neon API key, `#[ignore]`):**
- Create checkpoint → list → verify it appears → discard → verify it's gone
- Create checkpoint → read session from branch connection → data matches main
- Cleanup with no expired branches → returns 0
- Create 2 checkpoints → expire 1 → cleanup → 1 deleted, 1 remains

**Config tests:**
- `neon.enabled = false` → no branch manager created
- Missing API key when `neon.enabled = true` → clear config error
- `max_checkpoints = 0` → error at config validation time

```bash
# Without Neon (default)
cargo test -p moa-session --features postgres

# With Neon API
NEON_API_KEY="..." NEON_PROJECT_ID="..." \
  cargo test -p moa-session --features postgres -- --ignored neon
```
