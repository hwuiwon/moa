# 106 — Per-Tenant Adaptive Intents & Learning Pipeline

## Purpose

Add per-tenant intent taxonomies that learn from conversations, starting from a completely blank slate per tenant. Intents are discovered automatically from conversation patterns, editable by tenant admins, and used to improve skill ranking, memory retrieval, and proactive tool selection. A global intent catalog exists as a curated library that tenants can opt into — never force-applied.

This prompt also introduces the unified **Learning Log** — a single event-sourced audit trail that tracks every learned pattern (intent discovery, skill improvement, memory update) with provenance, confidence, and version history. The Learning Log is the foundation for reliable, versioned, rollback-capable learning across all of MOA's learning subsystems.

## Prerequisites

- Prompt 104 (task segmentation) landed — `task_segments` table exists with `intent_label`.
- Prompt 105 (resolution detection) landed — segments have `resolution` scores.
- Prompt 91 (pgvector semantic memory) landed — embedding infrastructure available.
- `moa-session` uses Postgres with pgvector extension.

## Read before starting

```
cat moa-core/src/types.rs
cat moa-core/src/config.rs
cat moa-session/src/schema.rs
cat moa-session/src/postgres.rs
cat moa-brain/src/pipeline/skills.rs
cat moa-brain/src/pipeline/query_rewrite.rs
cat moa-brain/src/resolution/scorer.rs
cat moa-orchestrator/src/workflows/consolidate.rs
cat moa-skills/src/registry.rs
```

## Architecture

### Multi-tenancy model

```
Platform (global)
  └── Tenant (team)
        ├── Users (individuals within the team)
        ├── Workspaces (projects/repos)
        ├── Intent taxonomy (per-tenant, evolving)
        ├── Skills (workspace-scoped, ranked by tenant-level resolution data)
        └── Memory (workspace-scoped, consolidation at tenant level)
```

A tenant is a team. Users belong to tenants. Workspaces belong to tenants. Intent taxonomies are per-tenant (not per-workspace) because a team's intent patterns are consistent across their projects. Skills and memory remain workspace-scoped but their ranking signals (resolution rates) aggregate at tenant level.

### Intent lifecycle

```
1. UNDEFINED (cold start)
   - All conversations tagged with intent = NULL
   - No classification, no routing effect

2. DISCOVERY (automatic, async)
   - After ≥50 segments accumulate for a tenant:
     run clustering on segment embeddings (task_summary + first user message)
   - Clusters with ≥5 members are proposed as candidate intents
   - Candidate intents stored with status = "proposed", confidence = cluster_quality
   - Tenant admin sees proposed intents in dashboard

3. CONFIRMATION (tenant admin action)
   - Admin reviews proposed intents: approve, reject, rename, merge
   - Approved intents become status = "active"
   - Retroactive: existing undefined segments matching the new intent get reclassified

4. CLASSIFICATION (ongoing, automatic)
   - New segments classified against active intents using embedding similarity
   - High confidence (≥0.80): auto-apply
   - Medium confidence (0.50-0.79): apply with confidence score, flagged for review
   - Low confidence (<0.50): leave as undefined

5. EVOLUTION (ongoing)
   - New clusters emerge from undefined segments → new proposals
   - Intents with declining usage (no matches in 90 days) → flagged for deprecation
   - Intents with low resolution rates → flagged for review
   - Admin can merge, split, rename, deprecate intents at any time

6. GLOBAL CATALOG (opt-in)
   - Platform curates a global intent catalog (coding, research, deployment, debugging, etc.)
   - Tenants browse the catalog and opt-in to specific intents
   - Opted-in intents become active for the tenant with global examples
   - Tenant can customize: add examples, rename, disable
```

### Intent storage schema

```sql
-- Per-tenant intents
CREATE TABLE IF NOT EXISTS {schema}.tenant_intents (
    id              UUID PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    label           TEXT NOT NULL,
    description     TEXT,
    status          TEXT NOT NULL DEFAULT 'proposed',  -- proposed|active|deprecated
    source          TEXT NOT NULL DEFAULT 'discovered', -- discovered|manual|catalog
    catalog_ref     UUID,                               -- FK to global catalog if adopted
    example_queries TEXT[] NOT NULL DEFAULT '{}',
    embedding       vector(1536),                       -- centroid embedding for classification
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deprecated_at   TIMESTAMPTZ,
    segment_count   INT NOT NULL DEFAULT 0,
    resolution_rate NUMERIC(4,3),
    UNIQUE(tenant_id, label)
);

CREATE INDEX IF NOT EXISTS idx_tenant_intents_tenant
    ON {schema}.tenant_intents (tenant_id, status);

-- Global intent catalog (platform-curated)
CREATE TABLE IF NOT EXISTS {schema}.global_intent_catalog (
    id              UUID PRIMARY KEY,
    label           TEXT NOT NULL UNIQUE,
    description     TEXT NOT NULL,
    category        TEXT,                     -- coding|research|devops|data|creative|admin
    example_queries TEXT[] NOT NULL,
    embedding       vector(1536),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Learning log (unified, bitemporal, append-only)
CREATE TABLE IF NOT EXISTS {schema}.learning_log (
    id              UUID PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    learning_type   TEXT NOT NULL,            -- intent_discovered|intent_confirmed|intent_deprecated|
                                              -- skill_improved|skill_created|memory_updated|
                                              -- resolution_scored|intent_classified
    target_id       TEXT NOT NULL,            -- intent_id, skill_name, memory_path, segment_id
    target_label    TEXT,                     -- human-readable label
    payload         JSONB NOT NULL,           -- full details of what was learned
    confidence      NUMERIC(4,3),
    source_refs     UUID[] NOT NULL DEFAULT '{}',  -- session_ids or segment_ids that contributed
    actor           TEXT NOT NULL DEFAULT 'system', -- system|admin:{user_id}|brain:{session_id}
    valid_from      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_to        TIMESTAMPTZ,             -- NULL = current; set on supersede
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    batch_id        UUID,                    -- groups related learnings for rollback
    version         INT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_learning_log_tenant_type
    ON {schema}.learning_log (tenant_id, learning_type, valid_to);
CREATE INDEX IF NOT EXISTS idx_learning_log_target
    ON {schema}.learning_log (tenant_id, target_id, valid_from DESC);
CREATE INDEX IF NOT EXISTS idx_learning_log_batch
    ON {schema}.learning_log (batch_id) WHERE batch_id IS NOT NULL;
```

### Intent classification (zero-shot, no per-tenant model training)

MOA uses **embedding-based nearest-centroid classification** — not per-tenant model training. This is the right tradeoff for a multi-tenant platform: no training infrastructure, O(1) with tenant count, instant updates when intents change.

```
New segment arrives
  → Embed (task_summary + first user message) using the platform embedding model
  → Query: SELECT id, label, embedding <=> $1 AS distance
            FROM tenant_intents
            WHERE tenant_id = $2 AND status = 'active'
            ORDER BY distance ASC LIMIT 3
  → If best match distance < threshold (0.35 cosine):
      confidence = 1.0 - distance
      assign intent with confidence
  → Else: leave as undefined
```

The embedding model is the same one used for pgvector memory search (prompt 91). No separate model needed.

### Intent discovery (async clustering)

Runs as a Restate scheduled invocation (like the Consolidate workflow), not in the hot path:

```
Every 24 hours per tenant (configurable):
1. Query all undefined segments from last 30 days for this tenant
2. If count < 50: skip (not enough data)
3. Embed all segment (task_summary + first_user_message) pairs
4. Ask LLM to cluster and label:
   Prompt: "Given these {N} task descriptions from a single team, identify
   groups of similar tasks. For each group of ≥5 similar tasks, suggest:
   - A short intent label (2-4 words)
   - A one-sentence description
   - 3 representative example queries from the group
   Respond with JSON array. Only include groups with ≥5 members."
5. For each discovered cluster:
   - Compute centroid embedding from member embeddings
   - Create tenant_intent with status='proposed'
   - Log to learning_log with learning_type='intent_discovered'
6. Retroactively classify undefined segments against new proposed intents
```

Why LLM clustering instead of BERTopic: keeps MOA as a pure Rust binary. No Python dependency, no UMAP/HDBSCAN. The LLM can name and describe clusters in one pass. The tradeoff is cost (~1 LLM call per discovery run) vs simplicity.

## Steps

### 1. Add tenant and intent types to `moa-core/src/types.rs`

```rust
pub type TenantId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantIntent {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    pub label: String,
    pub description: Option<String>,
    pub status: IntentStatus,
    pub source: IntentSource,
    pub example_queries: Vec<String>,
    pub segment_count: u32,
    pub resolution_rate: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus { Proposed, Active, Deprecated }

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentSource { Discovered, Manual, Catalog }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningEntry {
    pub id: uuid::Uuid,
    pub tenant_id: TenantId,
    pub learning_type: String,
    pub target_id: String,
    pub target_label: Option<String>,
    pub payload: serde_json::Value,
    pub confidence: Option<f64>,
    pub source_refs: Vec<uuid::Uuid>,
    pub actor: String,
    pub valid_from: chrono::DateTime<chrono::Utc>,
    pub valid_to: Option<chrono::DateTime<chrono::Utc>>,
    pub batch_id: Option<uuid::Uuid>,
    pub version: i32,
}
```

### 2. Add schema migrations

Add the tables defined above to `moa-session/src/schema.rs`. Include the global catalog table and learning log.

### 3. Implement intent CRUD in `PostgresSessionStore`

```rust
pub async fn create_intent(&self, intent: &TenantIntent) -> Result<()>;
pub async fn update_intent_status(&self, id: Uuid, status: IntentStatus) -> Result<()>;
pub async fn list_intents(&self, tenant_id: &str, status: Option<IntentStatus>) -> Result<Vec<TenantIntent>>;
pub async fn classify_segment(&self, segment_id: Uuid, intent_id: Uuid, confidence: f64) -> Result<()>;
pub async fn get_intent_by_embedding(&self, tenant_id: &str, embedding: &[f32], limit: usize) -> Result<Vec<(TenantIntent, f64)>>;
pub async fn append_learning(&self, entry: &LearningEntry) -> Result<()>;
pub async fn list_learnings(&self, tenant_id: &str, learning_type: Option<&str>, limit: usize) -> Result<Vec<LearningEntry>>;
pub async fn rollback_batch(&self, batch_id: Uuid) -> Result<u64>; // returns count of invalidated entries
```

### 4. Implement intent classifier

Create `moa-brain/src/intents/classifier.rs`:

```rust
pub struct IntentClassifier {
    session_store: Arc<PostgresSessionStore>,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    threshold: f64,  // default: 0.35 cosine distance
}

impl IntentClassifier {
    /// Classify a segment against the tenant's active intents.
    pub async fn classify(
        &self,
        tenant_id: &str,
        task_summary: &str,
        first_user_message: &str,
    ) -> Result<Option<(TenantIntent, f64)>> {
        let text = format!("{} {}", task_summary, first_user_message);
        let embedding = self.embedding_provider.embed(&text).await?;
        let matches = self.session_store
            .get_intent_by_embedding(tenant_id, &embedding, 3).await?;
        
        if let Some((intent, distance)) = matches.first() {
            if *distance < self.threshold {
                let confidence = 1.0 - distance;
                return Ok(Some((intent.clone(), confidence)));
            }
        }
        Ok(None) // no match above threshold
    }
}
```

### 5. Implement intent discovery workflow

Create `moa-orchestrator/src/workflows/intent_discovery.rs`:

```rust
/// Restate service or scheduled invocation that discovers new intents for a tenant.
pub async fn discover_intents(tenant_id: &str) -> Result<Vec<TenantIntent>> {
    // 1. Query undefined segments from last 30 days
    // 2. If < 50 segments, return empty
    // 3. Format task summaries for LLM
    // 4. Call LLM with clustering prompt
    // 5. Parse JSON response
    // 6. For each cluster:
    //    a. Compute centroid embedding
    //    b. Create tenant_intent with status='proposed'
    //    c. Log to learning_log
    // 7. Return proposed intents
}
```

Register as a Restate scheduled invocation in `Consolidate` or as a new `IntentDiscovery` workflow.

### 6. Wire intent classification into segment creation

In `moa-brain/src/pipeline/segments.rs`, when a new segment starts:
1. If `QueryRewriteResult.intent` is available, use it as a hint
2. Run `IntentClassifier::classify` against the tenant's active intents
3. If match found: set `segment.intent_label` and `segment.intent_confidence`
4. If no match: leave as `undefined`

### 7. Implement global intent catalog CRUD

Add methods for:
- Listing the global catalog
- Adopting a catalog intent for a tenant (creates a `tenant_intent` with `source=catalog` and `catalog_ref`)
- Removing a catalog adoption (sets tenant intent to deprecated)

### 8. Add tenant admin API for intent management

Create Restate service methods (or REST endpoints via the orchestrator):

```rust
// In a new IntentManager service or as part of WorkspaceStore
pub async fn list_tenant_intents(tenant_id: &str, status: Option<IntentStatus>) -> Vec<TenantIntent>;
pub async fn confirm_intent(intent_id: Uuid) -> (); // proposed → active
pub async fn reject_intent(intent_id: Uuid) -> ();  // proposed → deleted
pub async fn rename_intent(intent_id: Uuid, new_label: &str) -> ();
pub async fn merge_intents(source_ids: Vec<Uuid>, target_label: &str) -> Uuid; // returns new intent id
pub async fn deprecate_intent(intent_id: Uuid) -> ();
pub async fn create_manual_intent(tenant_id: &str, label: &str, description: &str, examples: Vec<String>) -> Uuid;
pub async fn adopt_catalog_intent(tenant_id: &str, catalog_id: Uuid) -> Uuid;
pub async fn list_catalog_intents(category: Option<&str>) -> Vec<CatalogIntent>;
pub async fn get_learning_log(tenant_id: &str, limit: usize) -> Vec<LearningEntry>;
pub async fn rollback_learning_batch(batch_id: Uuid) -> u64;
```

### 9. Wire learning log into existing learning events

Update the following to emit learning log entries:
- **Skill distillation** (`moa-skills`): when a skill is auto-generated → `learning_type='skill_created'`
- **Skill improvement** (`moa-skills`): when a skill is updated → `learning_type='skill_improved'`
- **Memory consolidation** (`moa-orchestrator/workflows/consolidate.rs`): when memory is compacted → `learning_type='memory_updated'`
- **Resolution scoring** (prompt 105): when a segment is scored → `learning_type='resolution_scored'`
- **Intent classification**: when a segment is classified → `learning_type='intent_classified'`

Each entry includes `source_refs` pointing to the session/segment IDs that contributed.

### 10. Retroactive classification on intent confirmation

When an admin confirms a proposed intent (status: proposed → active):
1. Query all undefined segments from the last 90 days for this tenant
2. Run the classifier against the newly active intent
3. For matching segments (confidence ≥ 0.60), update `intent_label`
4. Log retroactive classifications to learning_log with `batch_id`
5. If the admin wants to undo: `rollback_batch(batch_id)` invalidates all entries

### 11. Tests

- Unit: `IntentClassifier` — exact match → high confidence, no match → None
- Unit: `IntentClassifier` — embedding below threshold → returns None
- Unit: intent discovery — 50+ undefined segments → produces proposed intents
- Unit: intent discovery — <50 segments → returns empty
- Unit: retroactive classification — new active intent → matching segments updated
- Unit: learning log — entries created with correct provenance
- Unit: rollback_batch — invalidates all entries in batch
- Unit: catalog adoption → creates tenant intent with catalog_ref
- Integration: full lifecycle — discovery → admin confirms → classification active → resolution tracked

## Files to create or modify

- `moa-core/src/types.rs` — add `TenantId`, `TenantIntent`, `IntentStatus`, `IntentSource`, `LearningEntry`
- `moa-core/src/config.rs` — add `IntentConfig` (discovery schedule, classification threshold, min segments)
- `moa-session/src/schema.rs` — add `tenant_intents`, `global_intent_catalog`, `learning_log` tables
- `moa-session/src/postgres.rs` — add intent CRUD, learning log CRUD, retroactive classification
- `moa-brain/src/intents/mod.rs` — new module
- `moa-brain/src/intents/classifier.rs` — embedding-based classifier
- `moa-orchestrator/src/workflows/intent_discovery.rs` — new: discovery workflow
- `moa-orchestrator/src/services/` — add intent management service methods
- `moa-brain/src/pipeline/segments.rs` — wire classification into segment creation
- `moa-skills/src/distiller.rs` — emit learning_log entries on skill creation/improvement
- `moa-orchestrator/src/workflows/consolidate.rs` — emit learning_log entries on consolidation

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] New tenant starts with zero intents (completely blank).
- [ ] After 50+ segments, intent discovery proposes clusters.
- [ ] Admin can confirm/reject/rename proposed intents.
- [ ] Confirmed intents are used to classify new segments.
- [ ] Retroactive classification works on existing undefined segments.
- [ ] Learning log contains entries for all learning events with correct provenance.
- [ ] `rollback_batch` correctly invalidates a batch of learnings.
- [ ] Global catalog intents can be adopted and customized per tenant.
- [ ] Tenants that never adopt coding intents never see coding classifications.
- [ ] No per-tenant model training is required (embedding-based classification only).

## Notes

- **No per-tenant model training.** Classification uses embedding similarity against intent centroids. This scales to thousands of tenants with zero training infrastructure. If a tenant needs higher accuracy, they add more examples to their intents — the centroid embedding improves.
- **Discovery uses LLM clustering, not BERTopic.** This keeps MOA as a pure Rust binary. The LLM call costs ~$0.01 per discovery run and happens at most daily. The tradeoff is justified.
- **The Learning Log is append-only and bitemporal.** Never delete entries. Supersede by setting `valid_to`. Rollback by writing compensating entries. This enables complete audit trails and "what did we know when" queries.
- **Start simple.** The global intent catalog can ship with 10-20 curated intents (coding, debugging, deployment, research, data_analysis, file_management, configuration, documentation, testing, monitoring). Add more based on cross-tenant patterns observed in production.
- **Intent transitions (from prompt 105's materialized views) power proactive skill pre-loading.** When 70% of "debug" segments are followed by "fix" segments, the SkillInjector can pre-load fix-related skills during debug segments. This is a future optimization — defer until intent transition data accumulates.
