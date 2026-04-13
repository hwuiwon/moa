# Step 65 — Per-Day Workspace Cost Budget Enforcement

_Track rolling daily spend per workspace. Enforce hard caps before each LLM call. Graceful degradation with user notification._

---

## 1. What this step is about

MOA already tracks cost at the event level (`BrainResponse.cost_cents`) and aggregates it in `SessionMeta.total_cost_cents`. But there is no enforcement — the brain will happily spend $100 in a runaway loop. This step adds per-day workspace budgets that are checked before every LLM call and enforced with a hard stop + user notification.

---

## 2. Files to read

- **`moa-core/src/config.rs`** — `MoaConfig` struct. Add budget config here.
- **`moa-core/src/types/` (after Step 61)** — `SessionMeta.total_cost_cents`, `CompletionResponse` with token/cost fields.
- **`moa-core/src/error.rs`** — `MoaError` variants. Add a `BudgetExhausted` variant.
- **`moa-brain/src/harness.rs` (or `harness/` after Step 63)** — The brain turn loop where LLM calls happen. Budget check goes here.
- **`moa-session/src/backend.rs`** — `SessionStore` trait. Need a query to sum today's cost across all sessions in a workspace.
- **`moa-core/src/traits.rs`** — `SessionStore` trait definition. Add the cost query method.
- **`moa-orchestrator/src/local.rs`** — Where sessions are created/managed. May need budget awareness.

---

## 3. Goal

After this step:
1. Config has `[budgets]` section with `daily_workspace_cents` (default: 2000 = $20/day)
2. Before each LLM call, the brain checks today's rolling workspace spend
3. If spend ≥ budget, the brain emits a `BudgetExhausted` error event, sends a user-visible notice, and stops the turn
4. The user sees "Daily workspace budget exhausted ($20.00/day). 4 hours 23 minutes until reset."
5. Budget is rolling 24h from midnight UTC (simple), not calendar-day

---

## 4. Rules

- **Budget check happens in the brain harness, before `llm.complete()`.** Not in the provider, not in the orchestrator. The brain owns the "should I call the LLM?" decision.
- **Cost is tracked in cents (integers) to avoid floating-point drift.** Already the case in `total_cost_cents`.
- **The budget query is a new `SessionStore` method**, not a full-table scan. It should be an efficient SQL query: `SELECT COALESCE(SUM(total_cost_cents), 0) FROM sessions WHERE workspace_id = ? AND updated_at >= ?`.
- **Budget enforcement is NOT retroactive.** If a single LLM call costs $5 and the budget is $20, and the workspace is at $18, the call still executes. The check is pre-call, not mid-stream. A future improvement could estimate cost and reject pre-emptively, but that's out of scope here.
- **Budget config of 0 means unlimited.** Don't enforce when `daily_workspace_cents == 0`.

---

## 5. Tasks

### 5a. Add budget config to `MoaConfig`

```rust
// In config.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Maximum daily spend per workspace in cents. 0 = unlimited.
    pub daily_workspace_cents: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_workspace_cents: 2000, // $20/day
        }
    }
}
```

Add `pub budgets: BudgetConfig` to `MoaConfig`.

Config file format:
```toml
[budgets]
daily_workspace_cents = 2000  # $20/day, 0 = unlimited
```

### 5b. Add `BudgetExhausted` error variant

```rust
// In error.rs
pub enum MoaError {
    // ... existing variants
    #[error("daily workspace budget exhausted: {0}")]
    BudgetExhausted(String),
}
```

### 5c. Add cost query to `SessionStore` trait

```rust
// In traits.rs, inside the SessionStore trait
async fn workspace_cost_since(
    &self,
    workspace_id: &WorkspaceId,
    since: DateTime<Utc>,
) -> Result<u32>;  // returns total cents
```

### 5d. Implement the query in both session store backends

**Turso/SQLite:**
```sql
SELECT COALESCE(SUM(total_cost_cents), 0) AS total
FROM sessions
WHERE workspace_id = ? AND updated_at >= ?
```

**Postgres:**
```sql
SELECT COALESCE(SUM(total_cost_cents), 0)::BIGINT AS total
FROM sessions
WHERE workspace_id = $1 AND updated_at >= $2
```

Don't forget to add the `match` arm in `backend.rs` (or the `enum_dispatch` handles it after Step 64).

### 5e. Add budget check in the brain harness

Before each `llm.complete()` call:

```rust
async fn check_workspace_budget(
    session_store: &dyn SessionStore,
    workspace_id: &WorkspaceId,
    budget_cents: u32,
) -> Result<()> {
    if budget_cents == 0 {
        return Ok(()); // unlimited
    }

    let today_start = Utc::now().date_naive().and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc())
        .unwrap_or_else(Utc::now);

    let spent = session_store.workspace_cost_since(workspace_id, today_start).await?;

    if spent >= budget_cents {
        let budget_dollars = budget_cents as f64 / 100.0;
        let hours_until_reset = {
            let now = Utc::now();
            let tomorrow_start = today_start + chrono::Duration::days(1);
            let remaining = tomorrow_start - now;
            remaining.num_minutes()
        };

        return Err(MoaError::BudgetExhausted(format!(
            "Daily workspace budget exhausted (${:.2}/day). Resets in {}h {}m.",
            budget_dollars,
            hours_until_reset / 60,
            hours_until_reset % 60,
        )));
    }

    Ok(())
}
```

### 5f. Handle `BudgetExhausted` in the turn result

When the brain catches a `BudgetExhausted` error:
1. Emit an `Event::Error` to the session log
2. Send a `RuntimeEvent::Notice` with the human-readable budget message
3. Send `RuntimeEvent::Error` with the message
4. Return `TurnResult::Error` (don't retry, don't continue)

### 5g. Surface budget info in the Tauri desktop app

Add the budget status to `RuntimeInfoDto` or `SessionMetaDto` so the frontend can show a budget bar in the info panel:
- Add `daily_budget_cents: u32` and `daily_spent_cents: u32` fields
- The detail panel can show "Budget: $14.32 / $20.00 today"

Remember to add `#[derive(TS)]` to any new DTOs and regenerate bindings.

---

## 6. Deliverables

- [ ] `moa-core/src/config.rs` — `BudgetConfig` added to `MoaConfig`
- [ ] `moa-core/src/error.rs` — `BudgetExhausted` variant
- [ ] `moa-core/src/traits.rs` — `workspace_cost_since` added to `SessionStore`
- [ ] `moa-session/src/turso.rs` — Query implementation
- [ ] `moa-session/src/postgres.rs` — Query implementation
- [ ] `moa-session/src/backend.rs` — Delegation (or auto via enum_dispatch)
- [ ] `moa-brain/src/harness*` — Budget check before `llm.complete()`
- [ ] `src-tauri/src/dto.rs` — Budget fields added (with `#[derive(TS)]`)

---

## 7. Acceptance criteria

1. Setting `daily_workspace_cents = 500` ($5) and running sessions that exceed $5 → the brain stops with "Daily workspace budget exhausted" notice.
2. Setting `daily_workspace_cents = 0` → no budget enforcement, unlimited spend.
3. Budget resets at midnight UTC — sessions after midnight don't count previous day's spend.
4. The budget check is efficient (single SQL query, not full scan).
5. `BudgetExhausted` errors appear in the session event log.
6. The user sees the budget notice in the Tauri app (via RuntimeEvent::Notice).