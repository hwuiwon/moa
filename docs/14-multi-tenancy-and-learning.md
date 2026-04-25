# 14 — Multi-Tenancy And Learning

_Tenant model, adaptive intents, global catalog, learning log, and rollback._

## Tenant Model

MOA's tenant is a team. Users and workspaces belong to tenants; learning state is scoped to the tenant unless it is explicitly workspace-local.

```text
Platform
  -> Tenant
       -> Users
       -> Workspaces
       -> Intent taxonomy
       -> Learning log
       -> Skill ranking signals
       -> Memory consolidation signals
```

Workspace memory and skill files remain workspace-scoped. Intent taxonomies and resolution aggregates are tenant-scoped because a team's work patterns usually span projects.

## Blank Slate Rule

A new tenant starts with zero active intents. MOA does not classify into platform-provided labels until the tenant:

- confirms discovered intents
- creates manual intents
- adopts catalog intents

This prevents cross-tenant assumptions from leaking into routing, memory retrieval, or skill ranking.

## Intent Storage

Tenant intents live in `tenant_intents`:

- `id`
- `tenant_id`
- `label`
- `description`
- `status`: `proposed`, `active`, `deprecated`
- `source`: `discovered`, `manual`, `catalog`
- `catalog_ref`
- `example_queries`
- `embedding`
- `segment_count`
- `resolution_rate`

The global catalog lives in `global_intent_catalog` and is opt-in.

## Intent Lifecycle

### 1. Undefined

Segments begin with no tenant intent when there are no active matches or confidence is too low.

### 2. Discovery

`IntentDiscovery` is a Restate workflow. It loads recent undefined segments for a tenant, skips until the configured minimum segment count is met, asks an LLM to cluster similar task summaries, embeds cluster members, computes centroids, stores proposed intents, and appends `intent_discovered`.

### 3. Confirmation

`IntentManager` exposes admin operations:

- list tenant intents
- confirm
- reject
- rename
- merge
- deprecate
- create manual intent
- adopt catalog intent
- list catalog intents
- read learning log
- rollback a learning batch

Confirming an intent activates it and can retroactively classify recent undefined segments. Those updates share a batch ID so they can be invalidated together.

### 4. Classification

`IntentClassifier` uses nearest-centroid classification:

```text
segment text = task_summary + first user message
  -> embedding provider
  -> tenant_intents where tenant_id = ? and status = active
  -> order by embedding cosine distance
  -> accept best match below threshold
```

Confidence is `1.0 - distance`. No per-tenant model is trained.

### 5. Evolution

As new undefined segments accumulate, discovery can propose new intents. Admins can deprecate unused or low-performing intents. Intent transitions can be mined from `intent_transitions` once enough segment data exists.

## Global Catalog

The platform catalog is a curated library of reusable intent definitions. Adoption creates a normal tenant intent with `source = catalog` and `catalog_ref` set.

Tenants can customize or deprecate adopted intents. Catalog entries never apply directly to a tenant.

## Learning Log

`learning_log` is the audit trail for learned state:

| Field | Purpose |
|---|---|
| `tenant_id` | tenant scope |
| `learning_type` | machine-readable event kind |
| `target_id` | intent, skill, memory, segment, or other target |
| `target_label` | human-readable label |
| `payload` | structured full detail |
| `confidence` | score when available |
| `source_refs` | contributing sessions or segments |
| `actor` | system, admin, or brain/session identity |
| `valid_from` / `valid_to` | bitemporal validity |
| `batch_id` | groups related learning entries |
| `version` | target version |

Current learning types include:

- `intent_discovered`
- `intent_confirmed`
- `intent_classified`
- `skill_created`
- `skill_improved`
- `memory_updated`
- `resolution_scored`

## Resolution-Weighted Skills

Skills are files in workspace memory, but ranking uses tenant-level outcomes. `skill_resolution_rates` aggregates resolved, partial, and failed segments by tenant, intent, and skill name.

`SkillInjector` combines those rates with query relevance, use count, and recency to decide which skill metadata fits inside the prompt budget.

## Memory Learning

Memory consolidation records `memory_updated` with counts for rewritten pages, deleted pages, normalized dates, resolved contradictions, and confidence decay. The wiki describes current knowledge; the learning log records provenance and validity.

## Rollback

Rollback invalidates learning entries by setting `valid_to` for a batch. It returns the count of invalidated rows. It does not erase the audit trail and does not silently delete historical evidence.

Consumers should treat `valid_to IS NULL` as current learning.

## Proactive Patterns

`intent_transitions` records frequent transitions between intents. This can later power proactive skill preloading, for example when one tenant's debug segments are commonly followed by fix segments. That optimization should wait for enough tenant-specific data.
