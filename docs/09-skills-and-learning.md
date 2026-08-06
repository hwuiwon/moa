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
views. Package parsing rejects unsupported `metadata.moa-*` keys so stale
runtime fields do not re-enter package revisions.

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

The template uses the shared acyclic `ExecutionPlanDefinition`, with an
explicit `retain_effects` or `compensate_committed` cancellation policy and
exactly seven operations: `Capability`, `Agent`, `Map`, `Reduce`, `Review`,
`WaitSignal`, and `Output`. A map task can only be a capability or bounded agent
and cannot recursively map. Instruction text belongs in an `Agent` node's
instructions; labels and canvas layout belong in non-semantic `ui` metadata.
Visual editors round-trip the same artifact document and preserve stable node
IDs.

Every node carries an explicit nullable compensation field. A non-null contract
is valid only for a direct side-effecting capability whose pinned catalog entry
promises the exact compensator and bounded original-input/original-output
mapping; templates cannot invent rollback authority.

Learned revisions may currently activate catalog-independent templates made of
`Review`, `WaitSignal`, and `Output` nodes. Templates containing `Capability`,
`Agent`, `Map`, or `Reduce` remain draft-only until skill activation owns an
exact tool-catalog snapshot; the release gate rejects them rather than binding
them to whatever catalog happens to serve later.

When routing selects Execute/Durable and a high-confidence template on a serving
skill revision matches, admission pins the artifact revision, template hash, and
input, then compiles an immutable run snapshot without a planning-model call. A
one-off generated plan instead stores its planner model/prompt, candidate JSON,
compiler report, capability-catalog snapshot, and canonical hash. It is not a
skill artifact and is never activated automatically. Both sources enter the same
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
`LearningCandidateType::Skill` rows. A live run never mutates a skill revision
and never moves a serving pointer. Skills without an `execution_plan` retain
identical ranking and context injection and remain usable in Inline Execute and
in Durable `Agent` nodes.

## Execution Capability Catalog

One read-only execution capability catalog feeds planners, compilers, builders,
Inline Execute, and Durable nodes. It is tenant-authorized and deterministically ordered.
Every entry includes a stable reference/version, description, input/output
schemas, action/risk and idempotency classes, execution class, source
provenance, authorization metadata, and optional cost estimate.

The immutable deployment catalog merges typed built-ins, serving actions,
serving skill actions/code, memory operations, operator-owned MCP tools with
stable schemas/policies, and datasource reads backed by typed query operations.
For one authenticated request, MOA may add an ephemeral tenant connector
overlay only from delegated `Use`, the exact agent connector bindings, and
enabled action bindings at the current non-quarantined Active connection
generation. The overlay carries immutable definition, binding, generation,
contract, action, and policy-floor pins; a `conn__...` model name or connection
ID alone is not a capability and is never parsed for authority.

Nango/Merge knowledge connections are intentionally absent from the execution
catalog. They run through the knowledge sync workflow and never become
model-visible connector actions. Every reviewed HTTP connector action still
goes through action policy and `ToolExecutor`, and the execution interpreter
never bypasses governance. See
[Connectors And Connections](24-connectors-and-connections.md).

## Storage

Postgres is the only durable skill package store:

- `moa.artifact` stores the stable skill artifact identity, scope, name,
  description, and tags.
- `moa.artifact_revision` stores each immutable skill revision, status,
  canonical hash, source text, validation report, and artifact-local version.
- `moa.artifact_file` stores package files such as `SKILL.md`, scripts,
  references, assets, and optional `skill.moa.yaml`, keyed by artifact revision.

The context pipeline reads the skill artifact revisions the tenant's serving
pointers resolve to. There is no separate active skill mirror for turn context
injection.

Normal sessions remain serving/activation-fenced. They may materialize a skill
only through its current serving pointer or an exact activation-pinned revision
in the session's `AgentContext` dependency lock; revision status by itself is
never visibility. Eval-owned `Experiment` sessions have one narrow exception:
they may materialize an exact `draft` or `evaluating` skill only when that exact
revision is already pinned in their `AgentContext` lock. This preview does not
move a pointer or expose the revision to ordinary sessions.

Skill packages use tenant scope, not runtime memory scope:

| Scope | Stored as | Visibility | Typical use |
|---|---|---|---|
| Tenant | `tenant_id` set | One tenant | Released hand-authored or approved learned skills and optional execution-plan templates |

Visible skill resolution is name-based within a tenant and reads only the
type-owned serving pointer. Generic artifact authoring may store a skill draft,
but generic publish rejects skills. A hand-authored draft reaches the shared
`ArtifactRelease` evaluation and attested activation path; an accepted
`skill_draft` learning candidate reaches the same release repository through the
learning regression adapter. Neither path can make a revision visible by
changing status alone. There is no contact-scoped skill inheritance.

Hand-authored release evaluation executes the exact server-approved case tuples
against the candidate and, after first activation, a diagnostic serving-baseline
arm with paired seeds. Every trial owns a distinct overlay and eval session. The
platform release plan binds immutable platform-owned cases, personas, profiles,
and deterministic blocking evaluators; tenants cannot replace or supplement
that gate. Submission fails closed when any required binding is missing. Hidden
cases are exposed only to the internal experiment binding. The supported release lane
uses exactly one `agent_loop` target template; execution-template release plans
fail closed because they have no release-overlay resolver. Release cases that
reference data bundles also fail closed because this lane has no target-side
fixture resolver; trial sandboxes remain session-isolated, but their mutable
state is not release evidence.

MOA does not duplicate skill package bytes in object storage. Skill export uses
package documents containing base64-encoded files; generic artifact draft
authoring carries the canonical source plus package files. On each turn,
selected serving/activation-fenced skill packages are registered with the tool
router and materialized into the active hand under `.moa/skills/<skill>/...`
before the first hand tool executes. The eval-owned locked-draft exception above
uses the same exact-package materialization path.

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

Skill selection alone does not choose Execute or Durable. A template on a serving
skill revision is used only after routing chooses Execute/Durable and the
template matches with high confidence. Otherwise a strict one-off plan is
compiled from the current capability catalog.

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

Skill export, rendering, and turn-time injection are production surfaces.
Generic artifact authoring can store a skill draft but cannot activate it.
Automatic skill distillation and improvement are learning surfaces always
compiled in. They run by default after qualifying experience persistence and
create draft proposals only.
Eval-backed regression execution is owned by `moa-orchestrator`; `moa-skills`
only generates reviewable regression suite source.

Every learned skill change requires human review: generation of any kind —
distillation, improvement, experiment-derived, or mined — only ever produces a
`Proposed` learning candidate, and a tenant operator or admin must accept it
through `LearningReview` before anything about the active skill changes. There
is no unreviewed mutation path.

### Sanitized learning evidence

Nothing on the automatic learning path reads a raw transcript. Distillation,
improvement, sibling regression-suite generation, provider prompt formatting,
and task-summary embedding all take
`moa_skills::evidence::SanitizedLearningEvidence`, and there is no overload,
wrapper, or deprecated path that takes `EventRecord` instead.
The type has private fields, no raw-string or raw-event constructor, and no
`Deserialize`, so raw transcript evidence is not merely discouraged at those
boundaries — it is unrepresentable.

The one constructor, `sanitize_segment_evidence`, takes an injected
`&dyn PiiClassifier` and runs every text carrier through
`moa_memory_pii::sanitized::sanitize_with`. Production supplies the
deterministic local heuristic, the same classifier lineage capture uses, so
sanitization stays synchronous and free of network IO inside a durable step.
`moa-skills` owns no classifier and no detection policy.

The carriers covered are the caller's messages, queued messages, assistant
responses and reasoning summaries, tool arguments, tool results, tool errors,
memory paths, the task summary, and each segment-assessment evidence summary.
Tool arguments are walked as JSON and sanitized in both key and value position,
because a free-form argument map can put caller text in a key.

Sanitization is irreversible. PII and PHI proceed only after redaction leaves a
category placeholder in place of the original bytes; the original is not
recoverable from the result. This is deliberately unlike the DLP implementation
inside `moa-providers`' provider-governance layer, whose request-scoped tokens
are *reversible* by design so a value can be restored into a tool argument later
in the same request. The two must never be mixed: text
that already carries the reserved DLP delimiters is refused outright, before the
classifier is even consulted, because a restorable token inside a durable
learning artifact would let the original value be reconstructed after the fact.

Before any provider call or derived write, the gate refuses:

- content classified `Restricted`, and any span carrying the secret/credential
  category — a redacted credential is still a credential that reached the
  learning boundary;
- a classifier error or abstention, which must never degrade into an implicit
  "no PII found";
- spans that cannot be applied exactly as detected: empty or inverted,
  past the end of the text, straddling a UTF-8 character boundary, or
  overlapping another span;
- residual sensitivity found by re-classifying the redacted text, which catches
  a detector that located one of two occurrences;
- reserved reversible DLP token delimiters.

A refusal ends the pass. Nothing partial is written: no experience row, no
attribution, no candidate, no draft, no suite, and zero provider calls. Refusing
the whole segment rather than dropping the offending carrier is the point — a
partially sanitized corpus would let a reviewer approve a draft built from
evidence they could not tell was incomplete. Sibling and recurrence paths gate
each member independently, so one unreleasable session neither suppresses its
cluster nor rides through on its siblings.

Errors and log lines carry the stable carrier label and reason code only —
`restricted_class`, `classifier_abstained`, `span_out_of_range`,
`residual_sensitivity`, and so on. They never carry the refused text or the
classifier's own error string, either of which would re-leak what the gate
refused to release.

Derived rows keep provenance without content: tenant and contact scope, the
exact session, segment, and experience identifiers, the source event ids, the
detector version, the original sensitivity class, the redacted categories, and
one constant privacy-policy revision. The raw session event log remains the
separate source-of-truth owner of the unredacted transcript; erasure and
retention are enforced there, not by re-deriving learning artifacts.

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
   segment events. It rides the draft package as `tests/regression-suite.toml`,
   so every promoted revision carries the suite derived from its own source
   session; nothing runs at generation time. When a recurring task dedupes onto
   an open proposal, the new session's suite accumulates as sibling held-out
   material instead of being discarded. It never rewrites or re-synthesizes the
   filed draft.
8. Append one `LearningCandidateType::Skill` row with status `Proposed` and
   `proposal_kind = SkillDraft`, its typed provenance rows, operation, draft
   artifact revision ID, and an `evidence` payload carrying the assessed
   outcome and confidence, segment-assessment evidence rows, attribution
   summaries, tools used, and the similarity routing that chose
   improve-vs-create.
9. Record who the derived bytes belong to, in the same transaction: one
   `artifact_suite_contribution` row for the generated suite and one
   `artifact_revision_contribution` row for the draft's definition plus one per
   package file.

### Where suite bytes live, and why not in the payload

Suite TOML used to sit inside `learning_candidates.payload` as a JSON string,
and sibling suites accumulated into a payload array. That put attributable
generated text in a column nothing could join, enumerate, or selectively delete
— an erasure could not reach it, and a reviewer could not tell which session
produced which pooled suite without parsing JSON.

The bytes now belong to the artifact registry in
`moa.artifact_suite_contribution`, one row per suite, each naming the session
and experience it was generated from. The consequences are the point:

- **Erasure can reach them.** The rows are enumerable by a typed join from the
  subject's sessions and experiences.
- **The cap and the dedupe are the database's.** Sibling accumulation is bounded
  by a row count and deduped by a unique `(candidate, kind, suite_name)` index,
  not by scanning a JSON array.
- **Review input has one assembler.** The regression gate asks the artifact owner
  for the pool rather than re-parsing a payload shape, so there is exactly one
  place that knows how these bytes are stored.

A `generated` row is the candidate's own suite; `accumulated` rows are sibling
sessions' suites and are the only ones the held-out pool draws on. Pooling the
candidate's own suite would grade a draft on the cases it was derived from and
report a passing held-out split that held nothing out.

`artifact_revision_contribution` answers the same question one level up: which
candidate's evidence produced a revision's model-written definition and each of
its package files. Erasure uses those rows to find every attributable revision,
archive it, and clear its definition, source, files, and serving state in place;
the stable revision identity remains for pinned foreign keys. Dependent
candidates are then followed recursively rather than stopping at the first
promoted revision.

Proposal filing dedupes twice before creating a draft: an open `Proposed`
skill candidate for the same skill name, or for the same task fingerprint
(the generator may name the same recurring work differently), is returned
instead of filing a near-duplicate review item.

Skill improvement builds an updated `SKILL.md`, preserves supporting package
files from the previous revision, and stores the result as a draft artifact.
It does not activate the artifact or append `skill_improved` during generation.

Current review flow:

1. A tenant admin or tenant operator loads the full candidate through
   `LearningReview/get`.
2. `LearningReview/accept_skill` validates that the candidate is a proposed
   skill candidate and that the referenced artifact is the exact draft under
   review.
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
   including whether any held-out material existed. A terminal gate rejection
   records the canonical request digest and exact rejected response with the
   candidate status, so an identical retry does not rerun the gate.
4. Accept activates the existing draft artifact revision inside the caller-owned
   transaction: the revision becomes `ready`, its predecessor becomes
   `superseded`, and the tenant's serving pointer moves to the accepted revision.
   The pointer, not either status, remains the serving authority. A canonical
   activation-input digest binds the candidate revision and package, regression
   report, evaluator version, and the exact serving baseline captured before
   evaluation; the transaction rejects a changed baseline and appends the
   activation audit before moving the pointer.
   There is no `published` status to write for a skill, action, or agent. The
   learning regression result is an evidence adapter into the shared release
   repository; hand-authored candidates use production Behavior Lab evidence.
5. Accept marks the candidate `Promoted`, appends `skill_created` or
   `skill_improved` to `learning_log`, and records an `accepted_skill` decision
   with the canonical review-request digest and exact response. All three writes
   commit in the activation transaction. A matching terminal retry returns that
   response without rerunning the gate or activation; changed reviewer or reason
   inputs conflict.
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
- `actor`
- `valid_from`
- `valid_to`
- `recorded_at`
- `batch_id`
- `version`

Provenance is a separate `learning_log_source` table with one typed column per
referent kind, not a `UUID[]` on the row. The array it replaced declared no
referent type, so a reader could not tell whether a given uuid named a session, a
segment, an experience, or a row that no longer existed — which meant an erasure
walking it had to guess, and a guess is not a derivation chain. Entry and sources
commit together, so no entry can stand without a traceable derivation.

Rollback invalidates entries by setting `valid_to`. It does not delete rows.

Current learning types include:

- `skill_created`
- `skill_improved`
- `skill_rollback`
- `segment_assessed`

Weakness mining is the failure-driven counterpart to distillation: after each
assessed segment, durable tool errors and denied action reviews in the session
window are clustered deterministically (no model call) and recurring patterns
file `NeedsAuthoring` candidates naming the implicated editable surface. Mining
observes that something keeps failing; it does not produce a change anything can
apply, so its output is authoring work rather than a reviewable proposal. Once
filed, a weakness candidate's JSON and evaluation evidence stay immutable.
Repeated observations remain attributable source events and do not rewrite the
candidate. Evaluation-probe clustering is not modeled until a typed,
attributable producer exists.

`learning_candidates` is not a replacement for `learning_log`. Candidates are
mutable proposal state with evaluation payloads and explicit status transitions.
They are also the required boundary for experiment-derived skill improvements;
experiment outcomes must not mutate skill packages or execution-plan templates directly.
`learning_log` remains the append-only audit stream for promoted learning.

### Proposal kinds: what a reviewer can actually do

`candidate_type` says which domain a candidate targets. `proposal_kind` says what
a reviewer can do with it, and the two are deliberately separate fields.

They used to be one. Memory, policy, prompt, and eval suggestions were written as
`Proposed` and appeared on the review queue beside skill drafts, even though no
code existed that could promote them — so a reviewer could press accept on a
policy suggestion, get a success response, and nothing would happen. That is a
review contract the system could not keep.

| Kind | Reviewable | Lifecycle |
|---|---|---|
| `skill_draft` | yes | `Proposed -> Evaluating -> Promoted \| Rejected`; a promoted draft may later go `-> RolledBack` |
| `skill_rollback` | yes | `Proposed -> Evaluating -> Promoted \| Rejected` |
| `memory_advisory` | no | `Advisory -> Dismissed` |
| `skill_authoring`, `policy_authoring`, `prompt_authoring`, `eval_authoring` | no | `NeedsAuthoring -> Dismissed` |

Both reviewable kinds also permit an owner-only `Evaluating -> Proposed` claim
release, so a transient execution failure never strands a proposal mid-review.

The database enforces both the legal `(kind, status)` pairs and the legal
transitions. The pairs are a `CHECK`; the transitions need a trigger, because a
`CHECK` sees one row version and only a trigger sees the pair — and the pair is
where "an advisory item was walked to `Promoted` one legal-looking step at a
time" would live. `proposal_kind` itself is immutable, so an advisory item cannot
be relabelled into a reviewable draft to escape its lifecycle. Repository-level
compare-and-set sits on top as defense in depth; it does not constrain a direct
SQL writer, which is why the authority is in the database.

`LearningProposalKind` is a closed enum with no catch-all, so adding a kind is a
compile error at every match plus a migration to widen the constraint, rather
than a silently unconstrained row.

#### The review surface, and what each route may do

| Route | Handler | Admits |
|---|---|---|
| `POST /v1/learning-candidates/accept-skill` | `LearningReview/accept_skill` | `skill_draft` only |
| `POST /v1/learning-candidates/accept-rollback` | `LearningReview/accept_rollback` | `skill_rollback` only |
| `POST /v1/learning-candidates/reject` | `LearningReview/reject` | either reviewable kind |
| `POST /v1/learning-candidates/dismiss` | `LearningReview/dismiss` | informational kinds only |

Each entry point checks `proposal_kind`, not `candidate_type` and not a payload
string. The target domain does not say whether a materializer exists: a skill
suggestion with no draft behind it is also `candidate_type = Skill`, and
accepting one would run the activation path against a revision nobody generated.
Routing a revision-archiving rollback by a JSON `kind` field was the same
mistake in a different place — a payload key is writable by whatever produced
the candidate, while `proposal_kind` is a closed enum the database constrains
and refuses to let a row change.

Accepting a rollback archives the regressed revision and tombstones the serving
pointer, leaving the skill unserved. It does not restore a predecessor; any
replacement must pass a separate `accept_skill` review and activation. The
rollback transition, candidate status, learning log, and `accepted_rollback`
decision commit together. Its canonical request digest includes the exact
activation audit and pointer epoch, so a matching retry returns the recorded
response while a changed request conflicts. A stale or already-unserved proposal
instead commits its `Rejected` status, digest, and response together without a
second pointer transition or rollback learning entry.

Both accept routes bind their `Evaluating` claim to that request digest. A
durable-step retry can resume the same in-progress review, but cannot take over a
claim made with different authenticated inputs.

Rejection walks `Proposed -> Evaluating -> Rejected` rather than jumping
straight to `Rejected`. There is no direct edge, for the same reason acceptance
has none: the claim is what stops two reviewers from both succeeding at
contradictory decisions. A lost race after a successful claim releases the claim
rather than stranding the proposal in `Evaluating`.

Dismissal is the only decision an informational item admits, and it is a
distinct action rather than a flavor of rejection — rejecting means a reviewer
declined a proposal that *could* have been accepted, and there is no such
proposal here. It permits only `Advisory | NeedsAuthoring -> Dismissed`; every
other state is a typed conflict, and a candidate already `Dismissed` is a
replayed success rather than an error. The status change and its durable audit
in `learning_candidate_decision` commit in one transaction keyed
`(candidate, decision)`, so a re-execution converges on exactly one audit and no
item is ever left closed with no record of who closed it. There is deliberately
no generic promotion switch on any of these routes.

## Memory Learning

Memory consolidation writes **no** learning-log entry. It used to append a
tenant-wide `memory_updated` row whose provenance was an empty array: nothing
could say which subject's data the counts came from, so nothing could erase or
export it, and no reader consumed the type either. Giving it invented
tenant-wide provenance would have made it enumerable and still wrong, so the
emission was deleted instead. The consolidation counts live on the returned
report and in metrics.

Memory pages explain what the system knows; the learning log explains where a
*derived* update came from and whether it is still current.

## Audit And Rollback

Learning entries carry source refs, actor identity, confidence, and optional batch IDs. Admin services can list learning entries by tenant/type and invalidate a batch through rollback.

Rollback does not automatically rewrite every derived product table. It marks the learning entries invalid so consumers and admin tooling can distinguish current knowledge from superseded knowledge.
