# 14 — Multi-Tenancy And Learning

_Tenant model, skills-first learning, learning log, and rollback._

## Tenant Model

MOA's tenant is a team. Users and workspaces belong to tenants; learning state is scoped to the tenant unless it is explicitly workspace-local.

```text
Platform
  -> Tenant
       -> Users
       -> Workspaces
       -> Learning log
       -> Skill ranking signals
       -> Memory consolidation signals
```

Workspace memory and skill files remain workspace-scoped. Learning entries and resolution aggregates are tenant-scoped because a team's recurring work patterns usually span projects.

## Skills-First Learning

MOA does not require a durable session intent taxonomy. The agent loop selects tools and skills dynamically from the compiled context, while the learning layer records measured outcomes:

- task segments capture tool and skill usage
- resolution scoring records whether the task worked
- skill distillation and improvement create reusable Agent Skills
- memory consolidation updates graph memory
- the learning log records provenance and rollback metadata

This keeps routing flexible while preserving auditable, tenant-scoped adaptation.

## Learning Log

`learning_log` is the audit trail for learned state:

| Field | Purpose |
|---|---|
| `tenant_id` | tenant scope |
| `learning_type` | machine-readable event kind |
| `target_id` | skill, memory, segment, or other target |
| `target_label` | human-readable label |
| `payload` | structured full detail |
| `confidence` | score when available |
| `source_refs` | contributing sessions or segments |
| `actor` | system, admin, or brain/session identity |
| `valid_from` / `valid_to` | bitemporal validity |
| `batch_id` | groups related learning entries |
| `version` | target version |

Current learning types include:

- `skill_created`
- `skill_improved`
- `memory_updated`
- `resolution_scored`

## Resolution-Weighted Skills

Skills are stored through the skill registry, while ranking uses tenant-level outcomes. `skill_resolution_rates` aggregates resolved, partial, and failed segments by tenant and skill name.

`SkillInjector` combines those rates with query relevance, use count, and recency to decide which skill metadata fits inside the prompt budget.

## Memory Learning

Memory consolidation records `memory_updated` with counts for graph updates such as superseded facts, expired nodes, merged duplicates, and resolved contradictions. Graph memory describes current knowledge; the learning log records provenance and validity.

## Rollback

Rollback invalidates learning entries by setting `valid_to` for a batch. It returns the count of invalidated rows. It does not erase the audit trail and does not silently delete historical evidence.

Consumers should treat `valid_to IS NULL` as current learning.
