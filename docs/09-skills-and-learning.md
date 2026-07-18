# 09 — Skills & Learning

_Agent Skills, outcome-weighted ranking, and the unified learning log._

## Skill Format

MOA uses Agent Skills-style packages:

```text
.moa/skills/
  deploy-to-staging/
    SKILL.md
    scripts/
    references/
    assets/
```

`SKILL.md` contains YAML frontmatter plus markdown instructions. MOA only
interprets package-descriptive frontmatter:

| Field | Purpose |
|---|---|
| `name` | Stable package name and artifact name |
| `description` | Human-readable summary used for search and compact manifests |
| `license`, `compatibility` | Agent Skills-compatible descriptive metadata |
| `allowed-tools` | Tool expectations copied into the canonical skill definition when `skill.moa.yaml` is absent |
| `metadata.moa-version` | Human-authored package semantic version |
| `metadata.moa-tags` | Search and ranking tags |
| `metadata.moa-estimated-tokens` | Optional deterministic override for instruction token estimates |

Runtime provenance and quality signals such as source session, use count, last
used time, success rate, brain affinity, generated/improved flags, and rollback
counts are not `SKILL.md` fields. They belong to artifact revisions, learning
candidates, `learning_log`, regression evidence, and tenant-scoped analytics
views. Imports reject unsupported `metadata.moa-*` keys so stale runtime fields
do not re-enter package revisions.

`SKILL.md` is required. Supporting files are optional, but when present they
are part of the same package revision and may include scripts, references,
templates, or other resources.

Packages may also include `skill.moa.yaml`. That file declares the canonical
skill artifact metadata: input and output schemas, connector references, named
actions, allowed tools, and UI metadata. When it is absent, MOA converts the
package to a minimal skill artifact that points at `SKILL.md`.

## Execution-Plan Templates

A skill is an optional execution input, not a route or an admission gate. The
context pipeline may select and materialize it for Inline Execute, while a
Durable `Agent` node may use an explicitly declared skill reference. Custom
instruction-only skills remain valid in both paths. A skill may also declare an
optional `execution_plan` in `skill.moa.yaml`; this is a pinned reusable plan
template, not a second skill type.

The template uses the shared acyclic `ExecutionPlanDefinition` with exactly
seven operations: `Capability`, `Agent`, `Map`, `Reduce`, `Review`,
`WaitSignal`, and `Output`. A map task can only be a capability or bounded agent
and cannot recursively map. Instruction text belongs in an `Agent` node's
instructions; labels and canvas layout belong in non-semantic `ui` metadata.
Visual editors round-trip the same artifact document and preserve stable node
IDs.

When routing selects Execute/Durable and a high-confidence published skill
template matches, admission pins the artifact revision, template hash, and
input, then compiles an immutable run snapshot without a planning-model call. A
one-off generated plan instead stores its planner model/prompt, candidate JSON,
compiler report, capability-catalog snapshot, and canonical hash. It is not a
skill artifact and is never auto-published. Both sources enter the same
`ExecutionRun` runtime and `moa.execution_run`/`moa.execution_task`
persistence.

`Agent` nodes may activate instruction-only skills and reason freely within
their declared skill references, capability references, turns, and resource
budget. They return `Completed`, `NeedsInput`, `NeedsReplan`, or `Failed`; they
cannot mutate the graph. A `NeedsReplan` amendment is compiler-validated and may
change only pending/downstream work without broadening authorization. Accepted
patches and reasons are persisted in `plan_history` and remain replayable.

Skill-template changes use normal artifact revisions: generated or
experiment-derived improvements first become draft skill revisions plus
`LearningCandidateType::Skill` rows. A live run never mutates or publishes a
skill. Skills without an `execution_plan` retain identical ranking and context
injection and remain usable in Inline Execute and in Durable `Agent` nodes.

## Execution Capability Catalog

One read-only execution capability catalog feeds planners, compilers, builders,
Inline Execute, and Durable nodes. It is tenant-authorized and deterministically ordered.
Every entry includes a stable reference/version, description, input/output
schemas, action/risk and idempotency classes, execution class, source
provenance, authorization metadata, and optional cost estimate.

The catalog merges typed built-ins, published actions and connector actions,
published skill actions/code, memory operations, currently connected MCP tools
with stable schemas and policies, and datasource reads backed by typed query
operations. A connection ID alone is not a capability. Every invocation goes
through the existing action-policy and `ToolExecutor` or typed service owner;
the execution interpreter never bypasses governance.

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

Skill packages use tenant scope, not runtime memory scope:

| Scope | Stored as | Visibility | Typical use |
|---|---|---|---|
| Tenant | `tenant_id` set | One tenant | Tenant conventions, approved learned skills, and optional execution-plan templates |

Visible skill resolution is name-based within a tenant. Tenant imports go
through `/v1/skills/import` after tenant authorization. There is no
contact-scoped skill inheritance.

MOA does not duplicate skill package bytes in object storage. Import/export uses
package documents containing base64-encoded files. On each turn, selected skill
packages are registered with the tool router and materialized into the active
hand under `.moa/skills/<skill>/...` before the first hand tool executes.

## Progressive Disclosure

| Tier | Loaded into context | When |
|---|---|---|
| Metadata | name, description, tags, action names, estimates | stage 7 skill manifest |
| `SKILL.md` | full instructions | read from `.moa/skills/<skill>/SKILL.md` when the agent activates the skill |
| Resources | scripts, references, assets | only when needed for execution |

The skill manifest is budgeted and sorted deterministically for cache stability.
In Inline Execute, the coordinator can activate `SKILL.md`, invoke its governed
actions, or use a conversational `Worker` for interactive delegation. Worker
remains a bounded child-agent primitive, not a bulk DAG scheduler. If an
initial root Inline turn discovers durable fan-out, joins, reviews, or recovery,
it may call the workflow-owned `request_durable_execution` control tool for one
typed, evidence-preserving upgrade to Durable. The tool is available only to
that eligible turn, must be called alone, and cannot be replaced by arbitrary
tool-result data. The turn cannot classify again or downgrade; the execution
compiler and `ExecutionTask` runtime own the graph, with no application fan-out
cap below the approved run budget.

Skill selection alone does not choose Execute or Durable. A published template
is used only after routing chooses Execute/Durable and the template matches with
high confidence. Otherwise a strict one-off plan is compiled from the current
capability catalog.

## Skill Ranking

`SkillInjector` ranks all visible skills using:

- keyword overlap with the current task
- task-conditioned strategy success for the current task fingerprint
- tenant-level resolution rate for the skill

Resolution-rate data comes from the `skill_resolution_rates` materialized view over `task_segments`. This means a skill that often leads to resolved tasks for a tenant can outrank a merely popular skill.
Task-conditioned data comes from `task_strategy_success_rates`, which groups
experience attributions by tenant, task fingerprint, subject type, and subject
ID. It is smoothed by sample count and confidence, then falls back to the
tenant-level rate when no similar task evidence exists.

## Distillation And Improvement

Skill package import, export, rendering, and turn-time injection are production
surfaces. Automatic skill distillation and improvement are learning surfaces
always compiled in. They run by default after qualifying experience
persistence and create draft proposals only.
Eval-backed regression execution is owned by `moa-orchestrator`; `moa-skills`
only generates reviewable regression suite source.

Every learned skill change requires human review: generation of any kind —
distillation, improvement, experiment-derived, or mined — only ever produces a
`Proposed` learning candidate, and a tenant operator or admin must accept it
through `LearningReview` before anything about the active skill changes. There
is no unreviewed mutation path.

Skill distillation runs after successful multi-step work that passes the
configured evidence threshold. The current learning flow proposes tenant-local
skill changes. Tenant learning is never globally promoted and never rewrites
shared defaults automatically. Current generation flow:

1. Gate on the assessed experience: resolved outcomes need confidence >= 0.7,
   partial outcomes need >= 0.85 plus helpful verification attribution, and the
   segment must contain enough tool calls. The turn driver applies the same
   gates before dispatching the detached workflow.
2. Preflight against open proposals: an open `Proposed` candidate for the same
   task fingerprint (or, for improvements, the same skill name) is returned
   without any model call.
3. Compare the experience's task summary, fingerprint, and facets against
   existing tenant skills.
4. If a similar skill exists, attempt improvement.
5. Otherwise ask the configured model to produce a complete skill document.
   Generation prompts truncate per-event text and carry an explicit output cap.
6. Validate the generated package and store it as a tenant-scoped
   `ArtifactKind::Skill` draft revision.
7. Generate reviewable regression suite TOML deterministically from the
   segment events. It is stored in the candidate payload and rides the draft
   package as `tests/regression-suite.toml`, so every promoted revision carries
   the suite derived from its own source session; nothing runs at generation
   time. When a recurring task dedupes onto an open proposal, the new session's
   suite accumulates onto the candidate as sibling held-out material instead of
   being discarded.
8. Append one `LearningCandidateType::Skill` row with status `Proposed`,
   source experience IDs, operation, draft artifact revision ID, and an
   `evidence` payload carrying the assessed outcome and confidence,
   segment-assessment evidence rows, attribution summaries, tools used, and
   the similarity routing that chose improve-vs-create.

Proposal filing dedupes twice before creating a draft: an open `Proposed`
skill candidate for the same skill name, or for the same task fingerprint
(the generator may name the same recurring work differently), is returned
instead of filing a near-duplicate review item.

Skill improvement builds an updated `SKILL.md`, preserves supporting package
files from the previous revision, and stores the result as a draft artifact.
It does not publish the artifact or append `skill_improved` during generation.

Current review flow:

1. A tenant admin or tenant operator loads the full candidate through
   `LearningReview/get`.
2. `LearningReview/accept_skill` validates that the candidate is a proposed
   skill candidate and that the referenced draft artifact is publishable.
3. The review-time regression gate fails closed. Candidate-content defects — a
   missing, unparseable, or empty generated suite, a missing skill name, or an
   estimated execution cost over the review budget — terminally reject the
   candidate with the failing state preserved in `evaluation_payload`. An
   unavailable provider is an operational failure and errors the accept request
   instead of waiving the gate. Held-in check: when a previous active revision
   exists, both revisions execute the candidate's own suite and scores are
   compared; a first revision executes its suite alone as a smoke gate.
   Held-out check: the previous revision's own suite plus any accumulated
   sibling suites — material the candidate was not derived from — execute the
   same way, and the candidate must not regress on them (a stale pooled case
   that fails both revisions equally neutralizes itself). The acceptance checks
   recorded on the promoted candidate are derived from what actually executed,
   including whether any held-out material existed.
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

Live behavior experiments use the same review boundary for any derived skill
improvement. Experiment-derived skill proposals capture reusable handling
instructions, optimized execution patterns, and execution-plan-template changes as
`LearningCandidateType::Skill`.
An experiment run may provide evidence through its linked session, execution run,
artifact revisions, and `analytics.score_run`, but the experiment path itself
does not auto-promote skills. Any experiment-derived improvement
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

- `storage_partition_id`
- `user_id`
- generated `scope`
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
- `segment_assessed`

Weakness mining is the failure-driven counterpart to distillation: after each
assessed segment, durable tool errors and denied action reviews in the session
window are clustered deterministically (no model call) and recurring patterns
file `Proposed` candidates naming the implicated editable surface. Re-observed
patterns bump the open candidate's occurrence evidence instead of filing
duplicates, and candidates a reviewer already claimed keep their review state.

`learning_candidates` is not a replacement for `learning_log`. Candidates are
mutable proposal state with evaluation payloads and explicit status transitions.
They are also the required boundary for experiment-derived skill improvements;
experiment outcomes must not mutate skill packages or execution-plan templates directly.
`learning_log` remains the append-only audit stream for promoted learning.

## Memory Learning

Memory consolidation appends `memory_updated` with the consolidation report. Memory pages explain what the system knows; the learning log explains where the update came from and whether it is still current.

## Audit And Rollback

Learning entries carry source refs, actor identity, confidence, and optional batch IDs. Admin services can list learning entries by tenant/type and invalidate a batch through rollback.

Rollback does not automatically rewrite every derived product table. It marks the learning entries invalid so consumers and admin tooling can distinguish current knowledge from superseded knowledge.
