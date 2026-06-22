# 09 — Skills & Learning

_Agent Skills, outcome-weighted ranking, and the unified learning log._

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

Packages may also include `skill.moa.yaml`. That file declares the canonical
skill artifact metadata: input and output schemas, connector references, named
actions, allowed tools, and UI metadata. When it is absent, MOA converts the
package to a minimal skill artifact that points at `SKILL.md`.

## Storage

Postgres is the only durable skill package store:

- `moa.artifact` stores the stable skill artifact identity, scope, name,
  description, and tags.
- `moa.artifact_revision` stores each immutable skill revision, status,
  canonical hash, source text, validation report, and artifact-local version.
- `moa.artifact_file` stores package files such as `SKILL.md`, scripts,
  references, assets, and optional `skill.moa.yaml`, keyed by artifact revision.

The context pipeline reads published skill artifact revisions directly. There is
no separate active skill mirror for turn context injection.

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
| Metadata | name, description, tags, allowed tools, action names, estimates | stage 4 skill manifest |
| `SKILL.md` | full instructions | read from `.moa/skills/<skill>/SKILL.md` when the agent activates the skill |
| Resources | scripts, references, assets | only when needed for execution |

The skill manifest is budgeted and sorted deterministically for cache stability.

## Skill Ranking

`SkillInjector` ranks all visible skills using:

- keyword overlap with the current task
- task-conditioned strategy success for the current task fingerprint
- tenant-level resolution rate for the skill
- normalized use count
- recency

Resolution-rate data comes from the `skill_resolution_rates` materialized view over `task_segments`. This means a skill that often leads to resolved tasks for a tenant can outrank a merely popular skill.
Task-conditioned data comes from `task_strategy_success_rates`, which groups
experience attributions by tenant, task fingerprint, subject type, and subject
ID. It is smoothed by sample count and confidence, then falls back to the
tenant-level rate when no similar task evidence exists.

## Distillation And Improvement

Skill package import, export, rendering, and turn-time injection are production
surfaces. Automatic skill distillation and improvement are learning surfaces
compiled with the `moa-skills/skill-learning` feature. When compiled, they run by
default after qualifying experience persistence and create draft proposals only.
Eval-backed regression execution is owned by
`moa-orchestrator` and additionally requires `internal-eval-runner`; `moa-skills`
only generates reviewable regression suite source.

Skill distillation runs after successful multi-step work that passes the
configured evidence threshold. The current learning flow proposes
workspace-scoped skill changes;
deployment-wide global skills are operator imported, and user-scoped skills are
imported explicitly. Current generation flow:

1. Count tool calls; short/simple sessions are skipped.
2. Extract a task summary from recent user input.
3. Compare against existing workspace-scoped skills.
4. If a similar skill exists, attempt improvement.
5. Otherwise ask the configured model to produce a complete skill document.
6. Validate the generated package and store it as a workspace-scoped
   `ArtifactKind::Skill` draft revision.
7. Generate reviewable regression suite TOML and store it in the candidate
   payload without writing or running the suite.
8. Append one `LearningCandidateType::Skill` row with status `Proposed`,
   source experience IDs, operation, draft artifact revision ID, and review
   evidence.

Skill improvement builds an updated `SKILL.md`, preserves supporting package
files from the previous revision, and stores the result as a draft artifact.
It does not publish the artifact or append `skill_improved` during generation.

Current review flow:

1. A workspace editor loads the full candidate through `LearningReview/get`.
2. `LearningReview/accept_skill` validates that the candidate is a proposed
   skill candidate and that the referenced draft artifact is publishable.
3. Review-time regression evidence is attached to the candidate
   `evaluation_payload`. When `internal-eval-runner` is disabled, this records
   `"regression_execution": "unavailable"` while still requiring human review
   and artifact validation.
4. Accept publishes the existing draft artifact revision.
5. Accept marks the candidate `Promoted` and appends `skill_created` or
   `skill_improved` to `learning_log`.
6. `LearningReview/reject` marks the candidate `Rejected`, preserves draft
   artifacts for audit, and never mutates active skill rows.

The experience-native path uses `ExperienceRecord` as the learning unit. It
requires a resolved outcome, or a high-confidence partial outcome with helpful
verification attribution. It creates a `learning_candidates` row before any
active skill package mutation, moves the candidate through `proposed ->
promoted` or `rejected`, and records the candidate ID plus source experience IDs
in the learning log when promotion succeeds.

Live behavior experiments use the same review boundary for any derived workflow
or skill improvement. Experiment-derived workflow proposals capture recurring
escalations, shared failure modes, and workflow-shape changes as
`LearningCandidateType::Workflow`; skill proposals capture reusable handling
instructions and optimized execution patterns as `LearningCandidateType::Skill`.
An experiment run may provide evidence through its linked session, workflow run,
artifact revisions, and `analytics.score_run`, but the experiment path itself
does not auto-promote skills or workflows. Any experiment-derived improvement
writer must first append a `learning_candidates` proposal with the experiment
evidence attached, then rely on explicit evaluation and human or operator review
before promotion.

## Unified Learning Pipeline

```text
Conversations
  -> task_segments
  -> segment assessments
  -> experience_records
  -> experience_attributions
  -> learning_candidates
  -> promotion gates
  -> learning_log
       -> task-conditioned skill ranking
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
- `workflow_improved`
- `memory_updated`
- `segment_assessed`

`learning_candidates` is not a replacement for `learning_log`. Candidates are
mutable proposal state with evaluation payloads and explicit status transitions.
They are also the required boundary for experiment-derived skill or workflow
improvements; experiment outcomes must not mutate skill packages or workflow
artifacts directly. `learning_log` remains the append-only audit stream for
promoted learning.

## Memory Learning

Memory consolidation appends `memory_updated` with the consolidation report. Memory pages explain what the system knows; the learning log explains where the update came from and whether it is still current.

## Audit And Rollback

Learning entries carry source refs, actor identity, confidence, and optional batch IDs. Admin services can list learning entries by tenant/type and invalidate a batch through rollback.

Rollback does not automatically rewrite every derived product table. It marks the learning entries invalid so consumers and admin tooling can distinguish current knowledge from superseded knowledge.
