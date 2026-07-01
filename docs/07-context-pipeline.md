# 07 — Context Compilation Pipeline

_Ordered processors, query rewriting, skill injection, memory retrieval, compaction, and cache stability._

## Purpose

The context pipeline turns durable session state into one provider request. It must balance four goals:

- preserve prompt-cache stability
- include task-relevant memory and skills
- keep history within budget
- produce metadata for task segmentation and learning

The implementation lives in `crates/moa-brain/src/pipeline/`.

## Current Stage Order

The code reports fixed stage numbers through each `ContextProcessor`. With query rewriting and memory digests enabled, the default graph-backed pipeline contains twelve processors:

| Stage | Processor | Cache role | Purpose |
|---|---|---|---|
| 1 | `IdentityProcessor` | Stable prefix | MOA identity and high-level behavior |
| 2 | `AgentInstructionProcessor` | Stable prefix | session-pinned configured-agent instructions and workflow affordances |
| 2 | `InstructionProcessor` | Stable prefix | tenant and contact/session instructions |
| 3 | `ToolDefinitionProcessor` | Stable prefix | deterministic tool schema list, capped at 30 and filtered by pinned agent tool policy |
| 4 | `QueryRewriter` | Dynamic metadata | retrieval query preparation and task transition signal |
| 5 | `SkillInjector` | Dynamic tail | budgeted visible skill manifest ranked within pinned agent skill policy |
| 6 | `DigestProcessor` | Dynamic tail | standing contact digest for contact sessions |
| 7 | `MemoryRetriever` | Dynamic tail | tenant knowledge plus admitted contact memory filtered by pinned agent knowledge policy |
| 8 | `HistoryCompiler` | Dynamic/history tail | replayed events, checkpoints, recent turns, errors |
| 8 | `DelegationPlanningProcessor` | Dynamic tail | conservative coordinator DAG candidate for high-confidence multi-workstream tasks |
| 9 | `RuntimeContextProcessor` | Dynamic tail | current date, tenant, working directory, branch, contact or admin/operator actor |
| 10 | `Compactor` | Dynamic maintenance | checkpoint/compaction when thresholds are exceeded |

If query rewriting or memory digests are disabled, those processors are omitted; later processors keep their configured stage numbers.

## Stable Prefix

The stable prefix is produced by stages 1-3. These stages avoid per-turn values such as timestamps, working directory, branch, counters, usage stats, query-shaped ranking signals, or retrieved memory that would break byte-stable prompt caching.

The brain does not emit provider cache breakpoints, TTLs, or cached-content names. Provider-specific prompt-cache mechanics belong in the LLM gateway/provider layer. The brain's cache responsibility is only prompt section ordering: keep static instructions and deterministic tool schemas first, then put task-shaped sections in the dynamic tail.

## Query Rewriting

`QueryRewriter` is retrieval-scoped and gated. It only calls the rewrite LLM when graph memory retrieval and a vector leg are available and cheap heuristics indicate that memory search is likely to benefit, such as multi-turn coreference, vague follow-ups, vector-first history/preference/similarity questions, or multi-hop queries without clear seeds. Empty, command-like, exact-identifier, file/path-heavy, URL-heavy, and explicit first-turn queries use the original query.

The stage is fail-open. On timeout, parsing error, circuit-breaker open, or skipped input, it stores an original-query `QueryRewriteResult` and lets the turn continue. A turn execution reuses the same rewrite metadata across repeated compile steps for one user message, so tool-result follow-up requests do not call the rewriter again.

The query-rewrite circuit breaker is shared across rewriter instances inside
one process. It is a per-pod best-effort cost/latency guard, not a global
provider-protection mechanism. If a deployment needs cluster-wide rewrite
throttling, that state must move behind `RuntimeCacheStore` with Redis selected
or another shared coordination store.

The rewriter produces:

- `retrieval_query`
- `source`
- `reason`
- `is_new_task`
- `task_summary`

The query rewrite result is not an intent router. The old advisory tool, freshness, repository, memory-action, clarification, and promptlet fields are not part of the response contract. `is_new_task` and `task_summary` feed the segment tracker. `retrieval_query` feeds memory retrieval. When rewriting is skipped or fails, memory retrieval uses the full original user query rather than reducing it to keyword-only text.

## Configured-Agent Policy

Configured-agent sessions pin an `AgentContext` at session creation. Context
compilation reads only that pinned snapshot, so new deployments do not change
running sessions. The agent artifact JSON can include an optional
`guardrail_policy`; resolution copies it into `AgentPolicySnapshot` and pins it
in `session_agent_context` with the rest of the runtime policy. The pinned
snapshot can:

- inject agent-specific stable-prefix instructions
- filter prompt-visible tool schemas
- constrain skill selection by `auto`, `allowlist`, `pinned`, or `denylist`
- constrain graph-memory retrieval mode, filters, budget, and PII floor
- expose allowed workflow affordances without starting workflows implicitly
- configure input and output LLM-judge text guardrails

Durable execution still enforces policy again in the orchestrator tool/action
paths; prompt filtering is not treated as a security boundary.

## Guardrails

Guardrails are durable turn gates, not context processors. Input guardrails run
in `TurnExecution` before `Event::UserMessage`, so blocked user text does not
enter the event history as a user message. Output guardrails run after the main
model response is buffered and before the visible `Event::BrainResponse`, so the
persisted visible response is the allowed text or the configured block message.

V1 guardrails are LLM-judge text policies only. They do not implement PII,
response-schema, or tool input/output guardrails, and they do not replace action
or tool policy enforcement. `GuardrailCheck` events are metadata/audit records
for the decision and policy hash; they must not be treated as storage for the
raw guarded text.

## Skill Injection

`SkillInjector` loads visible tenant skill metadata from Postgres artifact
revisions and ranks skills with:

- keyword overlap against the current query
- tenant-level skill resolution rates from `skill_resolution_rates`
- task-conditioned strategy success from `task_strategy_success_rates`

When multiple visible skills share a name, the latest published tenant row wins.
There is no contact-scoped skill inheritance, and tenant-learned skills stay
tenant-local.

When a configured-agent policy is pinned to the session, selection first applies
that policy. Pinned skills are included before ranked fill, allowlists bound the
eligible set, denylists exclude matching refs, and `max_visible` caps the final
manifest. Selected package files load by exact locked artifact revision when the
agent dependency lock provides one.

It emits only a compact dynamic manifest. Full skill bodies and supporting package files
are carried as trusted selected-skill files. The root coordinator can read exact
selected skill paths from that manifest without provisioning a hand; worker tool
calls materialize the files under `.moa/skills/<skill>/...` when a hand tool is
first invoked. The manifest is budget-aware through `SkillBudgetConfig`.
Artifact-backed skills can expose named actions. When present, action names are
included in the compact manifest so the model can choose a linked capability
without loading the full package body.

The selected manifest is not part of the stable prefix because query keywords
and tenant-level learning can legitimately change which skills are shown for one
turn.

## Delegation Planning

`DelegationPlanningProcessor` runs after history compilation so it can read the
actual recent user event instead of synthetic user-role context such as the skill
manifest. It emits a structured `delegation_plan` metadata object plus a concise
dynamic hint when the request has high-confidence independent workstreams, such
as explicit reports from several inputs, readiness checks across named areas,
reconciliations, incident investigations, audits, or option comparisons.

The processor itself does not route workflows and does not add strict
`selected_skill` or `selected_action` fields to the worker contract. Root
`TurnExecution` consumes the metadata once per admitted user message: when
`spawn_worker` is available, it auto-spawns dependency-free ready nodes as
ordinary `ToolCall` / `ToolResult` history events, capped by worker fan-out and
the remaining tool-call cap. It also raises a low requested coordinator
model-loop turn cap to `4 + 2 * ready_node_count`, still bounded by the global
session hard cap, so delegated turns have room for fan-in and synthesis.
After auto-spawn, the root workflow waits for tracked ready-node workers through
the existing worker result awakeable path and emits a single `WorkerResultBundle`
event when they are terminal. History replay renders that bundle as one system
directive for synthesis. Dependent DAG nodes are left for coordinator synthesis
after worker results are available.

## Memory Retrieval

`MemoryRetriever` loads ranked graph hits through the graph, sidecar, and vector
memory crates, and assembles tenant knowledge chunks with admitted contact
memory when graph memory is enabled. See
`docs/15-architecture-policy.md` for the
current privacy boundary and `crates/moa-memory/README.md` for crate-level
details.

Search uses `retrieval_query` metadata when present, otherwise the full latest
contact or admin/operator message. Lexical search still derives terms
internally, while semantic retrieval keeps the natural-language query intact.
Retrieval can be keyword, semantic, or hybrid depending on the memory store
configuration.

For contact sessions, retrieval reads tenant knowledge plus the current
tenant/contact memory. It does not inherit tenant admin/operator memory or any
other contact's memory. Sessions without an admitted contact retrieve tenant
knowledge only. Tenant admin/operator memory inspection uses explicit tenant
admin paths rather than the default contact-session retrieval path.

The assembled context keeps source tiers visible:

```text
<knowledge_context>
  <tenant_knowledge>...</tenant_knowledge>
  <user_memory>...</user_memory>
</knowledge_context>
```

Tenant knowledge entries carry source URI/title, document version, chunk
identity, and citation metadata from `moa-knowledge` and `moa-memory-graph`.
Contact memory entries carry minimal provenance and privacy-filtered summaries.
The query trace records the scopes searched, retrieval legs run, candidate
counts, selected chunks/facts, source tiers, filters, citations, and stage
latencies. Memory is inserted as a reminder near the active turn so runtime
facts and retrieved context do not disturb the stable prefix.

## History Compilation

`HistoryCompiler` reads durable events from `SessionStore`, applies checkpoints and context snapshots when available, preserves recent turns, and keeps errors visible. It is segment-aware because `SegmentStarted` and `SegmentCompleted` events remain in the replay stream.

## Runtime Context

`RuntimeContextProcessor` inserts volatile facts at the end of the prompt:

- current date
- tenant and admin control-plane identifiers when available
- current working directory
- git branch
- contact or admin/operator actor

These values are intentionally outside the stable prefix.

## Compaction

`Compactor` watches event and token thresholds. When compaction is needed, it can ask an LLM for a checkpoint summary, persist a `Checkpoint` event, and let future history compilation start from a compact representation while preserving durable history.

## Observability

Each processor returns `ProcessorOutput` with:

- tokens added and removed
- included and excluded items
- excluded item details
- duration
- metadata

The pipeline records structured tracing spans with tenant, contact, session,
admin/operator actor, model, stage number, stage name, token counts, and
stable-prefix metrics derived from prompt ordering.

Knowledge retrieval spans and metrics include source tier, retrieval leg,
candidate count, selected count, and redaction outcome. They must not include
provider credentials, account tokens, full raw documents, or contact points.
