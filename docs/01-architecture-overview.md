# 01 — Architecture Overview

_System model, trait map, data flow, and workspace layout._

## System Model

```text
Clients
  REST/gateway | API automation | Slack
        |
        v
Runtime boundary
  Edge HTTP reads/writes plus `moa-orchestrator` Restate handler service
        |
        v
Brain and execution
  TurnExecution -> Respond | Execute | NeedsInput
  Context pipeline -> provider router -> LLM
  Tool router -> built-ins / hands / operator MCP / tenant connector actions
  Execute Inline delegation -> Restate Worker virtual objects
  Execute Durable planning/compiler -> Restate ExecutionRun / ExecutionTask workflows
        |
        v
Product data in Postgres / Neon
  sessions, events, pending_signals, context_snapshots
  task_segments, experience_records, experience_attributions,
  learning_candidates, segment and strategy materialized views
  graph nodes, graph edges, sidecar indexes, configured vector records
  connector connections, HTTP action bindings, invocation ledgers
  knowledge projections, sync runs, document versions, chunks
  execution runs, execution tasks, plan history, budgets, completion checks
  learning_log
  analytics.turn_lineage, analytics.score_run, analytics.scores,
  moa.experiment_run, compliance audit tables
  security_events
        |
        v
Learning loop
  segments -> segment assessments -> experience records
  experience records -> attributions -> learning candidates
  task-conditioned strategy rates -> skill ranking
  learning log -> skill ranking, memory consolidation, rollback audit
```

Restate owns durable cloud execution. Postgres owns product-visible data and
cross-pod correctness records. The optional runtime cache is selected through
typed config: Redis coordinates short-lived runtime pacing or references across
replicas when configured, while the in-memory backend is process-local and must
not be used as an authoritative Kubernetes store. Graph memory is the canonical
memory source, with sidecar and vector indexes maintained by graph writes.
Tenant knowledge-base ingestion is a separate product surface owned by
`moa-knowledge`; it writes tenant knowledge into the same graph/vector substrate
without turning Nango/Merge provider sync into session memory ingestion.
`moa-connectors` owns the generic tenant connection parent and exact-generation
HTTP action bindings; `KnowledgeConnection` remains the knowledge-owned
Nango/Merge capability projection beneath its code-owned managed parent.

## Agent Building Blocks

MOA has one user-facing capability artifact kind: the skill. A skill can remain
instructions only, can expose governed actions or code capabilities, and may
also carry an optional `execution_plan` in `skill.moa.yaml`. Instruction-only
skills are selected normally in Inline Execute and may be pinned on an `Agent`
node in Durable Execute; no skill is required to define a plan.

`TurnExecution` chooses exactly one public route:

- `Respond`: one user-facing model response, no tools, and no plan-generation
  call. An ordinary user turn may first make one separate bounded auxiliary
  classifier call to select this route.
- `Execute`: authorized work with a deterministic internal strategy. The
  router supplies `Inline` or `Durable` explicitly. A classifier may attach one
  bounded free-form rationale for the active turn, but that text never controls
  execution and is not persisted.
- `NeedsInput`: one deterministic clarification with bounded missing fields.

Inline Execute is the bounded root model/tool loop, including repeat and
tool-call limits, visible skills, and conversational `Worker` delegation.
Durable Execute instantiates or compiles an immutable plan, starts a detached
`ExecutionRun`, publishes compact progress, and synthesizes its terminal result
into the owning session automatically. An initial root Inline turn may make one
evidence-preserving upgrade to Durable; it cannot downgrade or classify again.
The workflow exposes `request_durable_execution` only to that eligible turn,
requires it to be the sole tool call in the model response, and validates its
typed evidence before upgrading. Ordinary tool results cannot change strategy.
Difficulty alone does not select Durable. `Worker` is not the bulk DAG primitive.

Ordinary language routing is one strict-schema auxiliary-model classification,
not phrase matching. The classifier has no tools, retrieval, or web search and
cannot retry or invoke the planner. Provider, stream, size, schema, matrix, or
confidence failures conservatively select Execute/Inline. Blank objectives and
exact pinned templates use trusted zero-classifier-call routes; a typed Durable
upgrade is a trusted control transition and is not classifier input. The
normalized route audit persists `respond | execute | needs_input`, optional
`inline | durable`, and bounded provenance (source/outcome, model/prompt,
hashes, confidence, usage, cost, and duration), never raw objective, classifier
rationale, or classifier response text.

Agents, skills, connectors, actions, and behavior-lab experiment plans are
canonical artifacts. `moa-artifacts` owns their persisted document model,
structural plan types, stable references, revision history, and registry.
For connectors, the artifact is the immutable reviewed definition;
`moa-connectors` separately owns tenant connection lifecycle, health,
generation, installed bindings, discovery revisions, and invocation ledgers.
`moa-skills` owns skill package parsing, ranking, distillation, improvement,
review, and artifact-backed package helpers. Generic graph execution does not
live in `moa-skills`. The pure `moa-execution` core is the source of truth for
compiled plan validation, binding resolution, scheduling, budget transitions,
completion, and replan-stop evaluation. These entrypoints perform no database,
network, filesystem, Restate, provider, or tool I/O. Persistence adapters and
`moa-orchestrator` Restate handlers drive those pure domain transitions without
redefining their state enums. JSON is the canonical persisted shape; YAML is an
authoring and import/export format. Optional `ui` metadata is non-semantic.

### Execution ownership

| Boundary | Owner | Contract |
|---|---|---|
| `ExecutionRouter` | `moa-brain` | Uses trusted typed bypasses or at most one bounded auxiliary-model call to select Respond, Execute, or concrete missing input; Execute carries an explicit Inline/Durable strategy, any free-form rationale remains turn-local, and uncertainty falls back to Execute/Inline. |
| `ExecutionPlanner` | `moa-brain` | Chooses a pinned skill template or asks the auxiliary model for a strict candidate plan and immutable goal contract. |
| `ExecutionCompiler` | `moa-execution` | Validates, canonicalizes, estimates, and hashes initial plans and amendments against the capability catalog and remaining budget. |
| `ExecutionProjection` | `moa-execution` | Supplies ordered node/task state to the pure scheduler; it contains no repository or provider handle. |
| `ExecutionRepository` | `moa-execution` | Owns scoped run/task persistence, idempotent materialization, atomic budget accounting, generation-fenced outcomes, amendment history, and cancellation. It depends only on shared database/core types, never on Restate or runtime owners. |
| `ExecutionRun` | `moa-orchestrator` Restate workflow | Drives one durable plan from persisted state, including amendment, cancellation, progress, and terminal completion. |
| `ExecutionTask` | `moa-orchestrator` Restate workflow | Executes one stable logical node or map-item instance and records one typed outcome. |
| Connector definition | `moa-artifacts` | Owns immutable reviewed HTTP transport, schema, data-class, credential-slot, and action contracts; the platform supplies the fixed external-write/high-risk/admin-review floor. |
| Connector connection | `moa-connectors` | Owns tenant lifecycle/health/generation, HTTP action bindings, and durable send outcomes. |
| Knowledge connection | `moa-knowledge` | Owns Nango/Merge provider records, cursor/deletion behavior, ACL capture, parsing, and ingestion beneath code-owned managed parents. |

Every run starts with an immutable `ExecutionGoalContract` containing
`objective`, individually identified `requirements`, `deliverables`, `coverage`,
`constraints`, and `completion_checks`. Nodes identify the requirement IDs they
serve. Completion checks deterministically verify output schemas, required
node/task coverage, missing or failed work, citation/provenance requirements,
and budget/deadline state. A bounded verifier agent may perform a semantic
check, but its evidence and verdict are persisted. Missing required coverage or
deliverables can produce `partial`, `blocked`, or `unsupported`, never a false
`completed`. Final synthesis receives the contract, check results, aggregate
outputs, citations, and explicit gaps.

An `ExecutionPlanDefinition` carries `schema_version`, `input_schema`,
`output_schema`, and `nodes`. Each node carries `id`, `depends_on`, optional
`when`, `input`, `output_schema`, one `operation`, retry policy, optional budget,
and the goal requirement IDs it serves. The dependency graph is acyclic, and
its operation enum has exactly seven variants:

1. `Capability { reference }` invokes one registered governed capability.
2. `Agent { instructions, skill_refs, capability_refs, max_turns }` runs one
   bounded task-local agent.
3. `Map { items, item_key, max_items, item_output_schema, task }` creates one
   stable task per item up to the declared accounting bound; its task is only
   `Capability` or `Agent`, so maps cannot recurse.
4. `Reduce { items, max_items, reducer, batch_size }` reduces bounded structured
   results through a deterministic capability or a bounded hierarchical agent
   reducer.
5. `Review { prompt }` waits for a tenant review decision.
6. `WaitSignal { signal_name }` waits for one external or user signal.
7. `Output { value }` resolves and validates the terminal output.

Dependencies provide parallelism and joins; there are no implicit start,
parallel, join, worker, tool, action, skill-action, or memory node kinds. Dynamic
values use only whole-value `{ "$ref": "$.input.query" }`,
`{ "$ref": "$.nodes.resolve.output.items" }`, `{ "$item": true }`, and
`{ "$item_key": true }` objects. The compiler rejects unknown or
non-dependency paths, recursion, and item variables outside a map task. There is
no string interpolation, script, JSONata, JQ, or general expression evaluator.

Serving skill revisions provide pinned reusable plan-template provenance. A
selected high-confidence template is instantiated without a planning-model call.
A one-off request instead stores planner model/prompt provenance, candidate JSON,
compiler report, and final plan hash with its immutable compiled snapshot. A
one-off plan is not a serving artifact revision and is never promoted
automatically. Both sources compile to the same canonical run snapshot.

An agent node may reason freely within its declared skills, capabilities,
turns, and budget, but it cannot mutate the durable graph invisibly. Every task
returns `ExecutionTaskOutcome { schema_version, usage, result }`; cumulative
`usage` is common to every result. The flattened result is
`Completed { output, citations }`, `NeedsInput { question, audience }`,
`NeedsReplan { reason, evidence }`, `Cancelled { reason }`, or
`Failed { class, message }`. `NeedsReplan` requests a compiler-validated amendment that may add only downstream work,
replace or remove pending work, narrow a map, switch to a registered capability,
or add review/signal input. It cannot alter running or completed tasks, create a
cycle or recursive map, reuse a task identity with new meaning, exceed remaining
budget, or broaden authorization. Accepted amendments increment
`plan_revision` and append their canonical patch, hash, reason, and requirement
mapping to replayable `plan_history`. Replanning stops on resource/deadline
exhaustion, repeated plan hashes or failure fingerprints, or no measurable
progress—not an arbitrary revision count.

Run resources use one integer `ExecutionBudgetLimit`: cost in microusd, tokens,
tasks, tool calls, retrieved bytes, and deadline. Each task atomically reserves
its worst case before dispatch and reconciles actual usage afterward; work that
cannot reserve never starts. Default and tenant/user-approved envelopes govern
resource consumption only. Authorization, action policy, the capability
catalog, and node declarations govern what the run may do. Raising a resource
budget never grants a new skill, tool, task shape, strategy, or permission.

Compilation pins a sorted, duplicate-free `ExecutionCapabilityCatalog` and its
canonical hash. Scheduling requires the caller to supply that exact immutable
snapshot, rejects any hash drift, validates resolved capability inputs and
outputs against its Draft 2020-12 schemas, and derives each task reservation
from the catalog capability estimate. Each capability also pins the governed
runtime-contract revision that policy evaluation and dispatch must still serve;
a live mismatch fails closed rather than mutating the run's catalog. There is no
hidden/global catalog refresh inside a run revision. Capability estimates declare exactly one logical task;
retries and agent turns multiply only cost, tokens, tool calls, and retrieved
bytes. Nonterminal cumulative outcomes charge only their nonnegative usage
delta and retain the remaining reservation; terminal outcomes release it and
consume one logical task.

`ExecutionConfig` provides one planner repair attempt,
`repeated_failure_limit = 3`, and tenant-independent defaults of
`max_tasks = 10_000`, `max_tokens = 10_000_000`,
`max_tool_calls = 100_000`, `max_retrieved_bytes = 10_000_000_000`, and
`max_cost_microusd = 100_000_000` ($100). The unattended threshold is
`5_000_000` microusd ($5). One agent turn reserves 100,000 microusd, 8,000
tokens, 8 tool calls, and 10,000,000 retrieved bytes; one verifier turn reserves
200,000 microusd, 16,000 tokens, 4 tool calls, and 1,000,000 retrieved bytes.
Tenant policy or explicit user approval may raise or lower these envelopes. A
compiled worst-case estimate above the unattended threshold is persisted as
`awaiting_confirmation` and starts no task until the owning user confirms it.

Ready map items are materialized as stable tasks keyed by
`(run_uid, node_id, item_key)` and all are submitted durably. There is no
application-level active-worker or fan-out cap for execution tasks; run budget
and `max_tasks` bound logical expansion, while Restate concurrency rules and
provider pacing provide physical backpressure.

`ExecutionTaskId` is UUIDv5 over length-framed run UUID, node ID, and item key.
Ordinary tasks use item key `""`, map tasks use the typed canonical extracted
key, reducer tasks use `r{round}:b{batch}`, and completion verifiers use
`check:{completion_check_id}`. New work starts at attempt/generation one;
retries increment both, input resumes increment only generation, and stale
generation results cannot persist.

Behavior Lab uses a single `experiment_plan` artifact. Every run names one exact
immutable plan revision; callers cannot submit a raw target, variant, scorecard,
or resource envelope. Personas, profiles, data bundles, and scenarios are typed
embedded blocks under `definition.spec.simulation`, each with stable IDs for UI
round trips, trial fanout, scoring, and analytics. Their product boundary, UI
expectations, and verification lanes are documented in
[`docs/product/behavior-lab.md`](product/behavior-lab.md).

Every durable session is created inside one tenant for a contact, or by an
admin/operator actor, and uses a pinned agent revision. The session row owns the
tenant, contact, and creator attribution, while the `session_agent_context`
sidecar stores the selected agent artifact revision, deployment pointers when
present, policy hash, locked artifact/tool dependencies, and serialized runtime
policy snapshot. Per-agent guardrail policy is stored in the DB-backed agent
artifact JSON and pinned into this `session_agent_context` snapshot as
`guardrail_policy`. The context pipeline still ranks the skill revisions the
tenant's serving pointers resolve to and materializes selected artifact files for
the tool router, but that selection now runs inside the configured agent policy
for the session.
Execution APIs list, start, inspect, cancel, review, and signal runs through the
common execution DTOs. Run admission originates from a persisted user message:
it either selects one exact pinned `skill://...` template revision plus structured
input or uses the strict internal planner/compiler path. Callers submit neither a
compiled-plan identifier nor raw plan JSON.
`moa.execution_run` stores the goal contract, immutable initial and active plan,
revision history/hashes, provenance, status, budgets/usage, completion results,
session scope, and timestamps. `moa.execution_task` stores stable node/item
instances, requirement IDs, generation fence, input/output/error, reserved and
actual usage, citations, and timestamps. These are the source-of-truth run
tables. Skill packages remain in `moa.artifact`, `moa.artifact_revision`, and
`moa.artifact_file`. Automatic learning still follows `skill proposal -> draft
skill artifact + learning_candidate -> LearningReview accept -> activation`; the
last step atomically marks the accepted revision `ready`, marks the predecessor
`superseded`, and moves the tenant's serving pointer. The pointer, not status,
is the authority for what serves. Active sessions never mutate skill revisions.

Skill, action, and agent revisions have no `published` status. Storing or
validating a candidate never makes it visible: normal sessions resolve a
type-owned serving pointer for skills/actions and the installation pointer for
agents. A revision becomes `ready` only after deterministic release evidence
passes; `ready` means activatable, not serving.

`ArtifactRelease/submit` is the hand-authored release entry point. It resolves
the platform gate server-side, snapshots the candidate, dependency/runtime/tool
policy and activated tool catalog, writes the candidate plus durable dispatch in
one transaction, and starts `ArtifactReleaseEvaluation`. The approved plan must
declare exactly one `agent_loop` target template. Skill and action release plans
pin an exact host-agent revision; agent-deployment evaluation removes any
authored host selector and substitutes the exact candidate or baseline agent
revision for its arm. An `execution_template` release plan fails closed because
that target has no release-overlay resolver.

The workflow runs one production Behavior Lab experiment. First activation runs
the candidate alone. A later release runs candidate and serving-baseline arms,
but their paired comparison remains diagnostic until that exact design has a
passing operating-characteristic assessment; activation authority remains the
candidate's absolute deterministic score evidence. A server-approved case cohort
selects exact scenario, persona, profile, and repetition tuples from the
published platform release plan; tenants cannot replace or supplement this
gate. Release-policy and case-pack hashes cover their complete decision and
executable authority. The database permits only lifecycle closure of an existing
row; policy changes and hidden-cohort rotations insert a new immutable revision,
and repository resolution recomputes the digest before constructing a release
subject. The ordinary plan Cartesian product is not run for a release. Every
case/repetition/arm trial receives a distinct overlay and
eval-owned session, and dispatch verifies each exact binding once. Release cases
that reference simulation data bundles fail admission: the supported AgentLoop
lane has no fixture-backed target capability, so persisted fixture identifiers
would be provenance without enforcement. Run-scoped hand state is isolated by
the trial's unique session and sandbox, but is not release gate authority.
The approved plan must produce the policy's deterministic blocking score rows
(`scenario_outcome`, `target_completed`, `result_produced`, and
`privacy_safe_output` in the platform policy), and the workflow derives its
verdict from those persisted scores and their provenance. Release case authority
currently accepts only `text_match@1` positive assertions and
`prohibited_actions@1` safety assertions. `required_actions@1` remains available
to the general assertion registry but cannot block a release until reviewed
approval and execution observations share a stable effect identity.
Only that workflow may mint the expiring single-use activation attestation;
there is no caller-supplied verdict endpoint. Evaluation-only overlays are bound
to one secret and one eval-owned session; normal resolution never reads them.

The authenticated public ArtifactRelease surface contains exactly four `POST`
routes:

| Public route | Restate handler |
|---|---|
| `/v1/artifact-releases/submit` | `ArtifactRelease/submit` |
| `/v1/artifact-releases/activate` | `ArtifactRelease/activate` |
| `/v1/artifact-releases/attempts/list` | `ArtifactRelease/list_attempts` |
| `/v1/artifact-releases/attempts/review` | `ArtifactRelease/review_attempt` |

`ArtifactRelease/activate` spends the attestation to compare-and-set a skill or
action pointer. Agent activation uses `AgentDefinitions/deploy` so the existing
agent-principal authorization boundary remains intact while the same release
repository consumes the attestation and writes the exact `AgentRevisionLock`.
Install creates an inactive, non-serving installation. In all three cases the
pointer move, attestation consumption, revision state transition, and activation
audit commit together; serving-baseline, policy, catalog, candidate-byte, and
pointer drift fail closed. Learned-skill acceptance adapts its regression result
to this same activation contract rather than maintaining another serving path.
Submission and activation also re-resolve the published release plan, case
cohort, evaluator versions, certified simulator policy, and tool catalog. Any
drift from the digested evaluation subject invalidates the attempt or
attestation instead of silently changing what was measured.
The platform simulator certification itself requires a fixed migration-owned
mandate plus a separate exact-artifact evidence import. The mandate, not the
submitted study, owns bounds, cohort and authorization pins, budget, study
window, and the external source-manifest digest. The initial mandate is
unprovisioned and fails closed until a reviewed code-and-migration revision
supplies real evidence authority; `moa_promoter` cannot rewrite or delete it.

The artifact release-control schema makes `published` unrepresentable for
skill, action, and agent revisions. `published` survives only for kinds whose
activation seam is owned elsewhere, including experiment plans and connector
catalog snapshots. Serving pointers are created only through the release gate;
schema installation does not synthesize one. Agent installations retain their
exact installation pointer, and every later deployment transition is gated.
Contact-scoped release artifacts are unrepresentable because the release
subject is tenant-scoped.

Tenant knowledge-base rows are `moa.knowledge_connections`,
`moa.knowledge_sync_runs`, `moa.knowledge_ingestion_steps`,
`moa.knowledge_objects`, `moa.knowledge_document_versions`,
`moa.knowledge_blocks`, and `moa.knowledge_chunks`. These rows describe linked
external accounts, sync-run inspection state, parser output, block/chunk
identity, and graph write status. They are not session events and are not
written through `Memory.ingest_documents`.

MOA's runtime boundary is the tenant. Runtime operators can run local mode for
development and incident response, but the product model assumes organizations
need governed execution, audit trails, tenant-owned learning, and clear rollback
paths.

## Tenant Model

```text
workspace
  -> tenant
       -> contact
            -> session
```

This is the complete runtime hierarchy. The workspace is the deployment
administration boundary. Workspace admins are super-admin principals whose
OpenFGA `workspace#admin` relation inherits into every linked tenant's
`tenant#admin` relation. A tenant remains the hard runtime isolation boundary:
sessions, contacts, memory, learning, artifacts, analytics, policies, events,
and audit evidence are tenant-owned.

Contacts are end users inside a tenant. Operators are admin/control-plane
principals: workspace admins, tenant admins, tenant operators, service users,
and API-key subjects. Operators are authorized to administer or operate tenants,
but they are not contact memory subjects and are not part of the contact/session lineage.
Contact credentials cannot access platform-internal control-plane surfaces such
as skills, experiments, knowledge management, or tenant administration.

Contact memory is contact-local. A contact session never inherits another
contact's memory or tenant admin/operator memory. When graph memory is enabled,
the default answer-time retrieval path combines tenant knowledge-base chunks
with admitted memory for the current contact, then keeps those source tiers
separate in prompt context and query trace records. Knowledge-base chunks from a
permission-bearing connector are admitted only under the source system's own
ACL, so tenant membership alone does not grant access to synced content; see
[Tenant Knowledge Base](21-tenant-knowledge-base.md). Tenant learning is
tenant-local and never globally promoted. Skills and policies are tenant-owned.

## Core Traits

Most trait definitions live under `crates/moa-core/src/traits/` and
`crates/moa-core/src/traits/mod.rs`; shared DTOs live under
`crates/moa-core/src/types/`. Imports name the owning category, such as
`moa_core::traits::SessionStore`, `moa_core::types::session::SessionMeta`, and
`moa_core::events::Event`; `moa-core` does not provide a flattened type facade.
Its crate root exports only `MoaError`, `Result`, and `WORKSPACE_ID`. A few
traits are owned by the crate that
implements them: `Reranker` in `moa-providers`, and `LinkedIntegrationProvider`
and `DocumentParser` in `moa-knowledge`. Session orchestration is not a trait —
it is realized as Restate services and virtual objects in `moa-orchestrator`
(see `docs/12-restate-architecture.md`).

| Trait | Purpose | Main implementations |
|---|---|---|
| `SessionStore` | Append-only event log, sessions, pending signals, snapshots, task segments, experience records, learning candidates, analytics, skill rates | `PostgresSessionStore` |
| `BlobStore` | Claim-check storage for large session artifacts | `PostgresBlobStore` by default; explicit `FileBlobStore` for local or mounted-path use |
| `RuntimeCacheStore` | Short-lived runtime coordination/cache values with TTL | in-process memory fallback; optional Redis backend |
| `BranchManager` | Optional database checkpoint branches | `NeonBranchManager` |
| `HandProvider` | Declare enforceable capabilities per sandbox tier, then provision, execute, pause/resume, destroy hands. `capabilities()` is required with no default body, and every provisioned hand carries one required six-dimension `EffectiveSandboxProfile` the provider must translate or reject. | local, Docker, Daytona, E2B |
| `LLMProvider` | Provider completion interface | Anthropic, OpenAI, Gemini through `moa-providers` |
| `EmbeddingProvider` | Shared embedding interface | OpenAI embedding, Cohere v4, ZeroEntropy zembed-1, Gemini embedding, and test/mock adapters |
| `Reranker` | Shared reranking interface | Noop, Cohere Rerank, and ZeroEntropy rerank through `moa-providers` |
| `ChannelAdapter` | Channel inbound/outbound normalization | Slack |
| `BuiltInTool` | Built-in tool execution | memory/search/web and other built-ins |
| `ContextProcessor` | One stage in context compilation | identity, agent instructions, instructions, tools, query rewrite, skills, digest, memory, history, runtime context |
| `LinkedIntegrationProvider` | Tenant knowledge linked-account flow, provider sync trigger, changed-record listing, and webhook verification | Nango and Merge adapters in `moa-knowledge` |
| `DocumentParser` | Structure-aware parsing into normalized document elements for tenant knowledge ingestion | Native parser backed by `liteparse` for local file parsing, plus LlamaParse, Unstructured, and Reducto adapters in `moa-knowledge` |
| `CredentialVault` | Staged write/activate/rollback, audited active credential resolution, readiness, revocation, and bounded purge for versioned tenant connector slots | `PostgresCredentialVault` in `moa-auth-providers`, constructed once per process |
| `LineageHandle` | Transport-neutral lineage capture | null handle, async sink, OTel bridge |

Runtime entrypoints share these seams through the Restate-backed orchestrator.
Authentication and approval providers expose `AuthProvider` and
`AsyncAuthzProvider` through `moa-core::traits`; see ADR-0002 and ADR-0006.

Retrieval and tenant knowledge persistence use domain-owned ports rather than
`moa-core` traits. `moa-retrieval::MemoryRetrievalEngine` is the one scoped
retrieval implementation used by both the brain context adapter and the
orchestrator memory handlers. The tenant-scoped `moa-knowledge` repository is
split across six capabilities: `KnowledgeConnectionRepository`,
`KnowledgeSyncRepository`, `KnowledgeIngestionRepository`,
`KnowledgeAclRepository`, `KnowledgeContactGroupRepository`, and
`KnowledgeEventRepository`. A service or pipeline receives only the capabilities
it uses.

## Runtime Modes

### Cloud

`moa-orchestrator` exposes Restate handlers from one production binary. Domain
logic behind those handlers should live in in-process application services,
repositories, or domain crates so a future extraction can replace a composition
binding without changing handler contracts.

Core production bindings:

- Virtual objects: `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO`.
  `Session` and `Worker` additionally own the generation fence and the derived
  scheduling index for the action reviews their own turns raise.
- Services: `ActionReviews`, `AgentDefinitions`, `Agents`,
  `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`,
  `ConnectorConnections`, `Contacts`, `Execution`, `Experiments`,
  `GraphMemoryMaint`, `Knowledge`,
  `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`,
  `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `ActionPolicy`
- Workflows: `ExecutionRun`, `ExecutionTask`, `KnowledgeSyncIngestion`,
  `Consolidate`, `ExperimentRun`, `ExperimentTrialRun`,
  `SkillLearning`, `TenantPurge`, `TurnExecution`, `WorkerTurnExecution`


Internal application boundaries are in-process modules or domain crates behind
these handlers, not separate network services. Current examples include action
review policy and storage, builtin async-authz challenge storage, learning
review promotion, experiments, privacy, provider routing, and graph memory
retrieval. Read-only analytics, identity diagnostics, audit verification, and
lineage read routes are served directly from `moa-edge` against Postgres/domain
stores instead of going through Restate.

`moa-edge` also serves the inbound, stateless tenant-operations MCP protected
resource at `/mcp`. It is a transport adapter over those same direct read
models and typed Restate handlers: MCP does not own domain state, accept raw
SQL, or choose a tenant from tool input. This inbound operator surface is
separate from the outbound operator-owned agent-tool MCP clients in
`moa-hands`. Outbound MCP is immutable deployment configuration; tenant
connector connections do not host MCP servers or MCP actions.

The binary composition root constructs one `RuntimeDeps`, including one shared
`IngestRuntime`, one shared `MemoryRetrievalEngine`, connector services, provider
delivery, credential storage, and explicit turn/authz dependencies. It passes
those concrete dependencies through implementation constructors before
`build_endpoint` binds the Restate services, virtual objects, and workflows.
There is no process-global `OrchestratorCtx` or ingest-runtime singleton. The
architecture scanner rejects reintroduced raw context access under `objects/`,
`services/`, and `workflows/`; `docs/15-architecture-policy.md` holds the
authoritative composition rule.

`Session` is the durable actor for one session key. It queues messages, admits `TurnExecution` workflows, tracks the active task segment, records tool/skill usage, and writes learning entries. Segment assessment happens at turn, segment, idle, cancellation, and timeout boundaries as an auditable learning artifact, not as a live-loop control signal. `Worker` owns conversational delegated state with depth and budget limits, while `WorkerTurnExecution` runs one admitted child turn and reports turn-scoped mutations back to the VO.

`ExecutionRun` and `ExecutionTask` are the separate durable bulk-execution
family. Their full state and aggregate counters come from execution persistence,
not the `Session` VO. A run links to its owning session only for compact
progress, exact input requests, and one deduplicated terminal synthesis turn.

Coordinator turns can return while detached workers keep running across
non-sticky replicas. The coordinator and its children coordinate over two planes —
a high-frequency telemetry plane (progress, heartbeat, one-call-per-period
narration) that stays off the single-writer `Session` VO, and a low-frequency
control plane (attention signals, terminal-wake, guarded resume) that routes
through the coordinator VO. All of this is correct on Kubernetes because its state
lives in Restate VO/workflow state and Postgres (idempotent event appends guarded
by `session_event_dedupe`); Redis is a runtime cache only and never a correctness
owner. The root coordinator is sandbox-free, and each worker owns one ephemeral
sandbox keyed `(session_id, worker_id, provider)` that is released on the
worker's self-cleanup. `docs/02-brain-orchestration.md` and
`docs/12-restate-architecture.md` describe these planes in detail.

### Hosted API Clients

MOA ships no embedded command/runtime client. Local development and automation
exercise the same hosted surface as production: callers send HTTP requests to
`moa-edge` public routes or directly to Restate ingress in test fixtures.
Client code does not own sessions, memory, sandbox lifecycle, tool execution,
approvals, or code execution.

## Turn Data Flow

```text
User message
  -> Input guardrail evaluates configured-agent text policy
  -> SessionStore emits `UserMessage`
  -> Session VO prepares a turn
  -> Context pipeline runs
       1 identity
       2 instructions
       3 tools
       4 query_rewrite (when enabled)
       5 skills
       6 memory_digest (when enabled)
       7 memory
       8 history
       9 runtime_context
  -> TurnExecution selects Respond, Execute, or NeedsInput
       Respond: one model response, no tools or planning call
       Execute/Inline: bounded model/tool loop; optional Worker delegation
       Execute/Durable: instantiate/compile, persist, and detach ExecutionRun
       NeedsInput: deterministic bounded clarification
  -> Query rewrite may mark `is_new_task`
  -> SegmentTracker opens or rolls a task segment
  -> LLM response is streamed/collected
  -> Tool calls route through ToolExecutor and ToolRouter
  -> Every tool output is classified at its raw source and travels only as a
     SecuredToolOutput; a non-safe class scores the owner's security circuit
  -> Output guardrail evaluates buffered visible text
  -> BrainResponse and tool events are persisted
  -> Detached runs emit compact progress and request one guarded terminal synthesis
  -> Segment counters are updated
  -> SegmentAssessor assesses completed or idle segments
  -> Assessed segments emit experience records and attributions
  -> Learning candidates propose skill, memory, policy, prompt, or eval updates
  -> LearningEntry rows record promoted segment, skill, or memory learning
```

If query rewriting is disabled, the `query_rewrite` processor is omitted and
the remaining processors still report their configured stage numbers.

V1 guardrails are optional configured-agent LLM-judge text policies. Input
guardrails run before a `UserMessage` event is appended; output guardrails run
after the main model response text is buffered and before the visible
`BrainResponse` event is appended. `GuardrailCheck` events record decision
metadata for audit without storing the raw guarded text. PII guardrails,
response-schema guardrails, and tool input/output guardrails are explicitly out
of scope for V1, and guardrails are not a replacement for action or tool
policy.

## Storage Overview

| Area | Store | Notes |
|---|---|---|
| Session metadata and events | Postgres | `sessions`, `events`, `pending_signals`, `context_snapshots` |
| Task segmentation | Postgres | `task_segments`, segment baselines, skill resolution rates |
| Experience learning | Postgres | `experience_records`, `experience_attributions`, `learning_candidates`, `learning_candidate_source` (typed provenance), `learning_candidate_decision` (durable review history), task-conditioned strategy rates |
| Live behavior experiments | Postgres | `moa.experiment_run`, `moa.experiment_run_artifact_revision`, and linked `analytics.score_run` rows |
| Connector connections | Postgres | `moa-connectors` owns tenant-RLS connection, direct-use, reviewed HTTP action binding, invocation, and Nango/Merge managed-parent state; credentials remain in the vault owner |
| Tenant knowledge base | Postgres | `moa-knowledge` owns Nango/Merge projections beneath code-owned managed parents, sync runs, ingestion steps, document versions, blocks, chunks, ACLs, and provider event state; `moa-memory-*` owns the resulting graph/vector storage |
| Graph memory | Postgres | Nodes, edges, sidecar indexes, changelog, and RLS-protected scope state |
| Memory vectors | Postgres or configured vector backend | pgvector embeddings or Turbopuffer namespaces for graph retrieval; graph storage remains relational Postgres |
| Skill packages | Postgres | `moa.artifact`, `moa.artifact_revision`, and `moa.artifact_file` store tenant-owned skill documents and package bytes; generated tenant updates first land as tenant-scoped draft skill artifacts plus proposed `learning_candidates` and only become active after review acceptance |
| Execution runs | Postgres | `moa.execution_run` and `moa.execution_task` store immutable plan snapshots, provenance, amendments, budgets, stable logical tasks, outcomes, citations, completion checks, and terminal results |
| Learning audit | Postgres | `learning_log` append-only rows with bitemporal validity, plus `learning_log_source` typed provenance |
| Learning attribution | Postgres | `moa.artifact_revision_contribution` and `moa.artifact_suite_contribution` record whose data produced which derived artifact bytes; `moa.privacy_erasure_record_decision` records one durable disposition per record per erasure operation |
| Hand leases | Postgres | `moa.hand_leases` stores session/provider sandbox bindings, serialized handles, generation fencing, status, and expiry for cross-pod reuse and cleanup |
| Claim-check blobs | Postgres by default | large event payloads use `session_blobs`; local filesystem blobs require explicit configuration and a persistent mounted path in cloud |
| Session attachments | Postgres + object storage | `session_attachments` stores metadata and object keys; bytes live in RustFS locally or AWS S3/GCS in cloud; session events carry `Attachment` refs with durable ids. `SessionAttachmentStore::put` takes a deterministic slot (tenant, session, client message id, ordinal) whose UUIDv5 is the row's primary key, claims that row before writing the object create-only, and reports whether the write created storage or replayed an identical one |
| Cloud orchestration state | Restate | VO/workflow state and journals, not product record |
| Runtime cache | Redis or memory | optional TTL cache/coordination for pacing and transient references; memory is per-process and non-authoritative |
| Optional checkpoints | Neon | branch manager for database checkpoints |
| Security events | Postgres | Signed OCSF v1.3 events in `security_events` |

The central PostgreSQL migration inventory is a fresh-install-only chain of
exactly 53 files, `V000001..V000053`. The ownership manifest contains one entry
for every logical table family. `xtask check-migrations` rejects gaps, extra
files, and missing or stale ownership entries. The 2026-08-03 hard-reset epoch
removes the retired per-user token-vault tables from their original catalog
definitions; checksum divergence requires rebuilding Postgres and resetting
Restate durable state rather than an in-place compatibility migration.

## Auth Layer

ADR-0002 is the canonical decision record for MOA's identity,
authorization, naming, and security-event audit posture.
The implementation crates are grouped under `crates/moa-auth/` as a namespace
folder while preserving package names and Rust imports.

### Identity

`moa-edge` validates API keys by default, or OIDC tokens in Auth0/OIDC modes.
For local development and isolated tests, `auth.provider = "disabled"` accepts
unauthenticated edge requests as a fixed service identity. After authentication,
the edge injects `X-Moa-Identity-Type`, `X-Moa-Identity-Id`,
`X-Moa-Tenant-Id`, `X-Moa-Acting-On-Behalf-Of`, and
`X-Moa-Api-Key-Id` headers before forwarding to the orchestrator. The
orchestrator trusts these headers, so the Restate handler port (`9080`) must be
network-isolated in production; see
[`docs/operations/edge-network-isolation.md`](operations/edge-network-isolation.md).

Local deployments use `LocalAuthProvider` and `BuiltinAsyncAuthzProvider` by
default. Builtin approvals are documented in
[`docs/operations/builtin-approvals.md`](operations/builtin-approvals.md).
Auth0 setup is documented in
[`docs/operations/auth0-setup.md`](operations/auth0-setup.md).
Agent lifecycle operations are documented in
[`docs/operations/agent-lifecycle.md`](operations/agent-lifecycle.md).
SCIM v2 provisioning is documented in
[`docs/auth/scim.md`](auth/scim.md). OCSF security-event audit setup is
documented in [`docs/operations/ocsf-audit.md`](operations/ocsf-audit.md).

### Caller Identity Vs Contact

MOA identity is the authenticated control-plane caller: tenant admins, tenant
operators, API keys, service users, and future SSO/OIDC users. These
admin/operator principals are authorized through OpenFGA before protected reads
or writes. Runtime access to tenant-owned data uses tenant relations.

Contacts are agent-facing people or anonymous browser/device handles that
interact with an agent inside one tenant. Contacts are not admin/operator users
and are not provisioned through SCIM. A tenant admin, tenant operator, or
authorized integration issues a bounded MOA contact JWT; unverified contacts can
create/send agent-session messages with low-assurance scopes, and verification
workflows can promote the session to a verified contact. Sessions, memory,
analytics, traces, and privacy workflows carry the tenant id, session id, and
contact id for observability.

### Authorization

OpenFGA, backed by Postgres, is the default authorization engine. The v1
authorization schema lives in `moa-authz-schema` as Rust constants. Handlers
call `require_authz(authz, identity, object_type, object_id, relation)` at the
behavior boundary; there are no procedural macros or implicit handler guards.

### Audit

OCSF v1.3 security events are signed per tenant and written to the Postgres
`security_events` table by a bounded, non-blocking background sink. Detection
Findings for prompt-injection circuit transitions are the exception: they are
written synchronously and fail closed, because an agent must never be halted
without a durable record of why. Operational setup is documented in
[`docs/operations/ocsf-audit.md`](operations/ocsf-audit.md).

## Eval, Experiments, And Insights

Lineage records are captured through the hot-path `LineageHandle` bridge and
written asynchronously to `analytics.turn_lineage`. Experiment, online-judge,
and human-review scores use the same sink via `LineageEvent::Eval(ScoreRecord)`
and land in `analytics.scores`, keyed by turn, session, or dataset replay item.
`analytics.score_run` and `analytics.scores` are the durable score lineage used
by Behavior Lab. The platform regression harness produces the same typed
`ScoreRecord` contract for comparison, but does not itself persist those rows.

Acceptance is owned by Postgres. `record_durable_batch` commits the whole batch
to `analytics.lineage_journal` and returns only after that commit, so a record
the caller was told is durable survives the pod that accepted it. Replicas claim
queue rows in acceptance order under an expiring lease with
`FOR UPDATE SKIP LOCKED`; the store and the dequeue commit in one transaction,
as do a permanent failure's dead-letter and its dequeue. A recoverable failure
defers the rows with backoff and preserves them. Dequeue, defer, and dead-letter
updates are fenced by the current `lease_owner`; an expired claimant that loses
ownership rolls back its row-store transaction rather than mutating a
successor's claim.

Before writing, the drain takes the same ordered tenant-then-subject advisory
locks as destruction. Tenant fences suppress all matching partition rows;
subject fences suppress only rows for that user UUID or `contact:<UUID>`.
Writer-first rows are subsequently erased, while destruction-first fences are
observed by the writer in the same transaction, so no ordering can restore
purged lineage.

The local channel in front of this is best-effort ingress and a payload-free
wake signal, never durability. There is no pod-local journal: acceptance that
lived on one replica's filesystem could not survive a rollout, and no
configuration of such a path was ever correct.

Regression evals, live behavior experiments, and analytics insights are
separate surfaces:

- Regression eval: `moa-eval` is a platform-only library and feature-gated
  `xtask` surface used by CI, nightly jobs, explicit live lanes, and the internal
  skill-regression gate. It owns deterministic datasets, replay plans,
  regression runs, and score comparisons. It is not a tenant product: there is
  no `Eval` Restate service, no tenant eval MCP tool, and the public edge does
  not translate `/v1/evals/*`. Durable execution honesty is evaluated from the
  runtime's own typed projection and task rows as documented in
  [Execution Honesty Evaluation](eval/execution-honesty.md).
- Live behavior experiments: `moa-experiments` owns the typed domain model and
  storage repository; the `Experiments` service accepts and tracks runs against
  production execution paths. Agent-loop targets always create eval-owned
  `Session` state and submit messages through normal `Session/start_turn` and
  `TurnExecution` routing.
  Experiment plans pin an exact certified simulator policy revision. Admission
  persists its immutable provider/model, decoding, prompt, context, and response
  protocol snapshot; `ExperimentTrialRun` verifies that snapshot against the
  registry and dispatches simulator turns through the production provider
  gateway. The typed decision and policy binding are terminal evidence.
  Execution targets invoke a serving skill revision's exact pinned `execution_plan`
  through the same origin-bound planning/admission path, start the common
  `ExecutionRun`, and link its `execution_run_uid`. The `moa.experiment_run` row is the experiment ledger and
  links to the session, execution run, pinned artifact revisions, and
  `analytics.score_run`.
  `ExperimentTrialRun` owns per-trial simulator execution. The public edge
  routes are `POST /v1/experiments/generate-plan`,
  `/v1/experiments/run-plan`, `/v1/experiments/status`,
  `/v1/experiments/list`, `/v1/experiments/plans/list`,
  `/v1/experiments/trials`, `/v1/experiments/trial-status`,
  `/v1/experiments/cancel`, `/v1/experiments/propose-improvements`,
  `/v1/experiments/scores`, and `/v1/experiments/compare`; stale aliases such
  as `/v1/experiments/run` are
  not product routes. `Experiments/generate_plan`, `run`, `cancel`, and
  `propose_improvements` require a tenant admin or tenant operator relation;
  `status`, `list`, `trials`, `trial_status`, `scores`, and `compare` require
  tenant participation, tenant operator, or tenant admin authorization according
  to the target resource.
- Analytics and insights: `moa-edge` exposes `GET /v1/analytics/catalog` and
  `POST /v1/analytics/query` for tenant operator dashboards backed by curated
  analytics read models. The handlers read Postgres/domain stores directly after
  authz instead of paying a Restate hop for single-query reads. Future analytics
  agents must call these typed routes or application services, not raw SQL or
  unscoped `SessionStore` methods. Analytics catalog and query reads require
  tenant admin or tenant operator authorization and always scope rows to the
  explicitly requested tenant when supplied, otherwise to the authenticated
  identity's tenant. Audit verification and lineage explain/query/verify
  remain direct read use cases with their own typed handlers.
  Lineage export and erase are intentionally not direct read handlers until a
  durable product workflow owns those side effects.

Live behavior experiment-derived improvements have one review boundary: they
must become `learning_candidates` before any skill change is
accepted. Experiment runs do not auto-create those proposals, and no experiment
path may auto-promote skills. The explicit
`Experiments/propose_improvements` operation attaches experiment run IDs, score
run IDs, and artifact revision references to the candidate so reviewers can
reproduce the evidence. Those references are normalized `learning_candidate_source`
rows, not payload strings, so the same evidence a reviewer reads is the evidence a
privacy erasure can reach.

A candidate also declares a `proposal_kind` separate from its `candidate_type`:
only `skill_draft` and `skill_rollback` have a materializer and can be accepted.
Experiment proposals carry `no_automatic_artifact_publish`, so they are authoring
work (`NeedsAuthoring`), never a reviewable proposal that could be promoted. See
`docs/09-skills-and-learning.md` for the full kind/status table.

Skill-derived improvements use the same boundary. `TurnExecution` may dispatch a
detached `SkillLearning` workflow after experience persistence, but that
workflow can only create tenant-scoped draft skill artifacts and proposed
`LearningCandidateType::Skill` rows. Tenant-learned skills remain tenant-local
and are never promoted into shared defaults automatically. `LearningReview`
is the only runtime path that can activate those drafts inside the tenant, and it
does so by atomically appending an activation audit, compare-and-set moving the
tenant's serving pointer against the baseline evaluated by regression, recording
`skill_created`/`skill_improved`, and marking the candidate promoted.

`LearningReview` exposes four decisions — `accept_skill`, `accept_rollback`,
`reject`, and `dismiss` — each reachable through its own authorized HTTP route
and each admitting only the proposal kinds it can actually apply. They are
separate handlers rather than one endpoint with an action switch because the
operations differ in blast radius: accepting a rollback archives a *serving*
revision, and routing that by a field in a caller-supplied body would put it one
typo away from a draft promotion. Rollback tombstones the pointer and leaves the
skill unserved; it never restores a predecessor, because selecting a replacement
requires a separate reviewed activation. `dismiss` is the only decision an
informational candidate admits; nothing on this surface can promote one.

Inbound MCP is a transport adapter over product/default services such as
`Experiments`, direct edge analytics/lineage reads, `Skills`, and other typed
surfaces. Regression eval is never exposed through MCP: MCP must not publish
`/v1/evals/*` semantics, own experiment, eval, analytics, learning, or lineage
domain models, or bypass service-level authorization.

Grafana dashboards live in `dashboards/grafana/` and alert rules live in
`ops/prometheus/alerts/`. The `sync-grafana-dashboards` workflow imports the
dashboards after changes land on `main`, using repository secrets for the
Grafana URL and dashboard-write service-account token. Postgres panels select a
datasource named `DS_POSTGRES`; Prometheus panels separately select
`DS_PROMETHEUS`. The tenant selector is populated from
`analytics.turn_lineage`.

Telemetry leaves MOA by push, never by scrape: both binaries export traces and
runtime metrics over OTLP to one collector base URL, and neither exposes a
metrics port. A single Grafana Alloy replica in the `observability` namespace
receives them, remote-writes metrics to Mimir, and is also the one component
that synchronizes alert rules into Mimir — rules are authored as `PrometheusRule`
resources and adopted by their `moa.dev/rule-sync` label, so the rules Mimir
evaluates are the rules in this repository. The collector buffers undelivered
telemetry on a persistent volume, which is why it runs exactly one replica with
the `Recreate` strategy. Deployment shape, the settings that are load-bearing
rather than defaults, and the offline validation contracts are in
[`docs/10-technology-stack.md`](10-technology-stack.md).

## Compliance Audit Tier

Compliance audit is an opt-in superset of the engineering lineage tier. A
control-plane enrollment row enables tenant-local BLAKE3 chain links on
`analytics.turn_lineage`, periodic Merkle roots in `analytics.audit_roots`, PII
pseudonymization side data in `pii_vault`, and hosted verification through
`POST /v1/lineage/verify`. Export and erase side effects stay out of direct edge
read routes until a durable DSAR workflow owns them. Tenants that are not
enabled keep the L01-L03 behavior and store `prev_hash = NULL`.

Audit bucket bootstrap lives in `scripts/bootstrap-audit-bucket.sh`. Buckets
must be created with Object Lock enabled at creation time; production uses
Compliance mode and development uses a separate bucket, usually Governance mode
with short retention. Audit-root signing always uses the in-process Ed25519
signer configured by `MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX`; production must
provision that key through the runtime secret manager. Switching signing keys
starts new windows with the new label; old verifying keys remain required for
old audit roots. The hosted lineage root verifier is fail-closed: audit-root
window verification requires `MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX` and a matching
`MOA_LINEAGE_AUDIT_SIGNING_KEY_ID` for the root's stored signing-key label.
Lineage DSAR bundle export uses the privacy export signing key contract,
`MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX` plus optional
`MOA_PRIVACY_EXPORT_SIGNING_KEY_ID`.

**ATTESTATION GATE - DO NOT REPRESENT THIS AS COMPLIANCE EVIDENCE TO REGULATORS
OR CUSTOMERS UNTIL EXTERNAL CRYPTOGRAPHIC REVIEW IS COMPLETE.**
`moa-lineage-audit` must receive external cryptographer or appsec review before
DSAR exports, regulator responses, audit attestations, or certifications rely on
this layer as compliance-grade evidence. Internal debugging and forensics may
use it before that review. The review must cover BLAKE3 canonicalization and
chain extension, Ed25519 key handling, Merkle inclusion and consistency proof
construction, PII crypto-shredding semantics, S3 Object Lock configuration,
timestamp discipline, and replay resistance on the verify path.

## Workspace Layout

The complete package inventory changes often enough that duplicating it here
creates drift. Use the root [`README.md`](../README.md#workspace-layout) for a
scan-friendly crate map, and [`docs/10-technology-stack.md`](10-technology-stack.md)
for the current full workspace list from `cargo metadata`.

## Where To Look Next

- Orchestration details: `docs/02-brain-orchestration.md` and `docs/12-restate-architecture.md`
- Memory details: `docs/04-memory-architecture.md`
- Shared type placement: `docs/15-architecture-policy.md`
- Event and segment schema: `docs/05-session-event-log.md`
- Live behavior experiments: `docs/eval/live-behavior-experiments.md`
- Context pipeline: `docs/07-context-pipeline.md`
- Multi-tenant learning: `docs/14-multi-tenancy-and-learning.md`
