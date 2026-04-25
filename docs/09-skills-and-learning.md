# 09 — Skills & Learning

_Agent Skills, resolution-weighted ranking, adaptive intents, and the unified learning log._

## Skill Format

MOA uses Agent Skills-style directories:

```text
skills/
  deploy-to-fly/
    SKILL.md
    scripts/
    references/
    assets/
```

`SKILL.md` contains YAML frontmatter plus markdown instructions. MOA-specific metadata is stored under `metadata` with `moa-` keys such as source session, version, estimated tokens, use count, last used, and success signals.

## Progressive Disclosure

| Tier | Loaded into context | When |
|---|---|---|
| Metadata | name, description, tags, allowed tools, estimates | stage 4 skill manifest |
| Body | full `SKILL.md` | only when the agent activates the skill |
| Resources | scripts, references, assets | only when needed for execution |

The skill manifest is budgeted and sorted deterministically for cache stability.

## Skill Ranking

`SkillInjector` ranks workspace skills using:

- keyword overlap with the current task
- tenant-level resolution rate for the skill
- normalized use count
- recency

Resolution-rate data comes from the `skill_resolution_rates` materialized view over `task_segments`. This means a skill that often leads to resolved tasks for a tenant can outrank a merely popular skill.

## Distillation And Improvement

Skill distillation runs after successful multi-step work. Current flow:

1. Count tool calls; short/simple sessions are skipped.
2. Extract a task summary from recent user input.
3. Compare against existing workspace skills.
4. If a similar skill exists, attempt improvement.
5. Otherwise ask the configured model to produce a complete skill document.
6. Write the skill into workspace memory.
7. Generate a regression test suite for the skill.
8. Append a `skill_created` learning entry when a learning store is present.

Skill improvement writes updated skill content and appends `skill_improved`.

## Unified Learning Pipeline

```text
Conversations
  -> task_segments
  -> resolution scores
  -> learning_log
       -> intent discovery
       -> intent classification
       -> resolution-weighted skill ranking
       -> memory consolidation
       -> intent transition analytics
```

Learning is not a single subsystem. It is the record of all durable derived knowledge produced by MOA.

## Learning Log

`learning_log` is append-only and bitemporal:

- `tenant_id`
- `learning_type`
- `target_id`
- `target_label`
- `payload`
- `confidence`
- `source_refs`
- `actor`
- `valid_from`
- `valid_to`
- `recorded_at`
- `batch_id`
- `version`

Rollback invalidates entries by setting `valid_to`. It does not delete rows.

Current learning types include:

- `intent_discovered`
- `intent_confirmed`
- `intent_classified`
- `skill_created`
- `skill_improved`
- `memory_updated`
- `resolution_scored`

## Per-Tenant Intents

Each tenant starts with an empty taxonomy. MOA classifies only against active tenant intents. It does not apply platform catalog intents by default.

Intent lifecycle:

1. **Undefined:** no tenant intent is assigned.
2. **Discovery:** `IntentDiscovery` clusters recent undefined task segments and proposes labels.
3. **Confirmation:** admins confirm, reject, rename, merge, or deprecate.
4. **Classification:** `IntentClassifier` embeds task text and searches active tenant intent centroids.
5. **Evolution:** new clusters become proposals; stale or low-resolution intents can be reviewed.

Classification is embedding-based nearest-centroid matching, not tenant-specific model training.

## Global Catalog

`global_intent_catalog` is a curated library. A tenant can adopt an entry, which creates a tenant intent with `source = catalog` and `catalog_ref` set. The tenant can then rename, customize examples, or deprecate that adoption.

No tenant receives catalog classifications unless it adopts or manually creates the matching intent.

## Memory Learning

Memory consolidation appends `memory_updated` with the consolidation report. Memory pages explain what the system knows; the learning log explains where the update came from and whether it is still current.

## Audit And Rollback

Learning entries carry source refs, actor identity, confidence, and optional batch IDs. Admin services can list learning entries by tenant/type and invalidate a batch through rollback.

Rollback does not automatically rewrite every derived product table. It marks the learning entries invalid so consumers and admin tooling can distinguish current knowledge from superseded knowledge.
