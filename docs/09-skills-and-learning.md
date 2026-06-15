# 09 — Skills & Learning

_Agent Skills, resolution-weighted ranking, and the unified learning log._

## Skill Format

MOA uses Agent Skills-style packages:

```text
.moa/skills/
  deploy-to-fly/
    SKILL.md
    scripts/
    references/
    assets/
```

`SKILL.md` contains YAML frontmatter plus markdown instructions. MOA-specific metadata is stored under `metadata` with `moa-` keys such as source session, version, estimated tokens, use count, last used, and success signals.

`SKILL.md` is required. Supporting files are optional, but when present they
are part of the same package revision and may include scripts, references,
templates, or other resources.

## Storage

Postgres is the only durable skill package store:

- `moa.skill` stores package metadata, scope, versioning, hashes, file counts,
  total size, tags, and a JSONB manifest derived from `SKILL.md`.
- `moa.skill_file` stores each package file as `BYTEA`, keyed by skill revision
  and normalized package path.

Skill packages are scoped with the same `MemoryScope` tiers used by memory:

| Scope | Stored as | Visibility | Typical use |
|---|---|---|---|
| Global | `workspace_id IS NULL`, `user_id IS NULL` | Every workspace and user | Operator-curated deployment-wide skills |
| Workspace | `workspace_id` set, `user_id IS NULL` | All users in one workspace | Team/project conventions and reusable workflows |
| User | `workspace_id` and `user_id` set | One user inside one workspace | Personal preferences, shortcuts, and learned habits |

Visible skill resolution is name-based. If global, workspace, and user scopes
all provide the same skill name, the user-scoped package is selected first, then
the workspace package, then the global package. Global imports require a service
identity with tenant-admin authorization and can be bootstrapped through
`/v1/skills/bootstrap-global`; workspace and user imports go through
`/v1/skills/import` after workspace authorization.

MOA does not duplicate skill package bytes in object storage. Import/export uses
package documents containing base64-encoded files. On each turn, selected skill
packages are registered with the tool router and materialized into the active
hand under `.moa/skills/<skill>/...` before the first hand tool executes.

## Progressive Disclosure

| Tier | Loaded into context | When |
|---|---|---|
| Metadata | name, description, tags, allowed tools, estimates | stage 4 skill manifest |
| `SKILL.md` | full instructions | read from `.moa/skills/<skill>/SKILL.md` when the agent activates the skill |
| Resources | scripts, references, assets | only when needed for execution |

The skill manifest is budgeted and sorted deterministically for cache stability.

## Skill Ranking

`SkillInjector` ranks all visible skills using:

- keyword overlap with the current task
- tenant-level resolution rate for the skill
- normalized use count
- recency

Resolution-rate data comes from the `skill_resolution_rates` materialized view over `task_segments`. This means a skill that often leads to resolved tasks for a tenant can outrank a merely popular skill.

## Distillation And Improvement

Skill package import, export, rendering, and turn-time injection are production
surfaces. Automatic skill distillation and improvement are internal learning
surfaces compiled only with the `moa-skills/skill-learning` feature, and eval
backed regression execution additionally requires `internal-eval-runner`.

When enabled, skill distillation runs after successful multi-step work. The
current learning flow creates or improves workspace-scoped skills;
deployment-wide global skills are operator imported, and user-scoped skills are
imported explicitly. Current flow:

1. Count tool calls; short/simple sessions are skipped.
2. Extract a task summary from recent user input.
3. Compare against existing workspace-scoped skills.
4. If a similar skill exists, attempt improvement.
5. Otherwise ask the configured model to produce a complete skill document.
6. Write the skill package into the workspace skill scope.
7. Generate a regression test suite for the skill.
8. Append a `skill_created` learning entry when a learning store is present.

Skill improvement writes an updated `SKILL.md`, preserves supporting package
files from the previous revision, and appends `skill_improved`.

## Unified Learning Pipeline

```text
Conversations
  -> task_segments
  -> resolution scores
  -> learning_log
       -> resolution-weighted skill ranking
       -> memory consolidation
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

- `skill_created`
- `skill_improved`
- `memory_updated`
- `resolution_scored`

## Memory Learning

Memory consolidation appends `memory_updated` with the consolidation report. Memory pages explain what the system knows; the learning log explains where the update came from and whether it is still current.

## Audit And Rollback

Learning entries carry source refs, actor identity, confidence, and optional batch IDs. Admin services can list learning entries by tenant/type and invalidate a batch through rollback.

Rollback does not automatically rewrite every derived product table. It marks the learning entries invalid so consumers and admin tooling can distinguish current knowledge from superseded knowledge.
