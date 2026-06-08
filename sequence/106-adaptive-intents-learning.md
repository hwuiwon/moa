# 106 — Skills-First Learning Pipeline

## Goal

Keep MOA's learning loop adaptive without assigning durable routing labels to every session or task segment. The agent loop, query rewrite metadata, skills, memory retrieval, and tool routing decide dynamically from context. Durable learning records measured outcomes and reusable improvements.

## End State

- `task_segments` stores segment boundaries, summaries, counters, skill usage, tool usage, and resolution outcomes.
- `learning_log` stores append-only tenant-scoped learning entries.
- Skill distillation writes `skill_created`.
- Skill improvement writes `skill_improved`.
- Memory consolidation writes `memory_updated`.
- Resolution scoring writes `resolution_scored`.
- `skill_resolution_rates` aggregates by tenant and skill name.
- `segment_baselines` aggregates by tenant.

## Learning Log Schema

```sql
CREATE TABLE IF NOT EXISTS {schema}.learning_log (
    id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    learning_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    target_label TEXT,
    payload JSONB NOT NULL,
    confidence NUMERIC(4,3),
    source_refs UUID[] NOT NULL DEFAULT '{}',
    actor TEXT NOT NULL DEFAULT 'system',
    valid_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_to TIMESTAMPTZ,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    batch_id UUID,
    version INT NOT NULL DEFAULT 1
);
```

## Store API

```rust
pub async fn append_learning(&self, entry: &LearningEntry) -> Result<()>;
pub async fn list_learnings(
    &self,
    tenant_id: &str,
    learning_type: Option<&str>,
    limit: usize,
) -> Result<Vec<LearningEntry>>;
pub async fn rollback_batch(&self, batch_id: Uuid) -> Result<u64>;
```

## Ranking Inputs

Skill ranking should combine:

1. Keyword overlap with the current query and recent user messages.
2. Tenant-level skill resolution rate.
3. Normalized use count.
4. Recency.

No durable taxonomy is required for this ranking. The current task shape comes from query rewrite, active context, available tools, and skill metadata.

## Tests

- Learning-log entry round-trips for `skill_created`.
- Rollback invalidates current entries by batch.
- `skill_resolution_rates` returns one tenant-level row per skill.
- `segment_baselines` returns one tenant-level baseline.
- Turn execution does not append removed classification learning entries.

## Acceptance

- No durable task taxonomy tables or services remain.
- No segment row stores classification label or confidence fields.
- Learning-log CRUD remains available.
- Skill and memory learning paths still append entries.
- Query rewrite emits `task_kind`, not a routing label.
