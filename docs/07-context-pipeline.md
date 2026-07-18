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

The code reports fixed stage numbers through each `ContextProcessor`; execution
order is the builder's stage list, which intentionally runs history before the
per-turn dynamic sections. With query rewriting and memory digests enabled, the
default graph-backed pipeline contains ten processors, executed in this
order:

| Execution | Processor | Cache role | Purpose |
|---|---|---|---|
| 1 | `IdentityProcessor` | Stable prefix | MOA identity and high-level behavior |
| 2 | `AgentInstructionProcessor` | Stable prefix | session-pinned configured-agent instructions and execution affordances |
| 3 | `InstructionProcessor` | Stable prefix | tenant and contact/session instructions |
| 4 | `ToolDefinitionProcessor` | Stable prefix | deterministic tool schema list, capped at 30 and filtered by pinned agent tool policy |
| 5 | `QueryRewriter` | Dynamic metadata | retrieval query preparation and task transition signal |
| 6 | `HistoryCompiler` | Frozen history | replayed events, checkpoints, recent turns, errors, checkpoint compaction |
| 7 | `SkillInjector` | Dynamic tail | budgeted visible skill manifest ranked within pinned agent skill policy |
| 8 | `DigestProcessor` | Dynamic tail | standing contact digest for contact sessions |
| 9 | `MemoryRetriever` | Dynamic tail | tenant knowledge plus admitted contact memory filtered by pinned agent knowledge policy |
| 10 | `RuntimeContextProcessor` | Dynamic tail | current date, tenant, working directory, branch, and contact when present |

History runs before the skill manifest, digest, and memory retrieval so those
per-turn sections insert near the active user turn instead of ahead of replayed
history. If they preceded history, their per-turn byte churn would break
provider prompt-cache reuse of the entire history span and invalidate the
incremental context snapshot on every turn. The history stage publishes the
frozen-history boundary in request metadata
(`STABLE_HISTORY_END_METADATA_KEY`); provider adapters may mark a moving cache
breakpoint there.

If query rewriting or memory digests are disabled, those processors are
omitted; later processors keep their configured stage numbers.

## Stable Prefix

The stable prefix is produced by stages 1-3. These stages avoid per-turn values such as timestamps, working directory, branch, counters, usage stats, query-shaped ranking signals, or retrieved memory that would break byte-stable prompt caching.

The brain does not emit provider cache breakpoints, TTLs, or cached-content names. Provider-specific prompt-cache mechanics belong in the LLM gateway/provider layer. The brain's cache responsibility is only prompt section ordering: keep static instructions and deterministic tool schemas first, then put task-shaped sections in the dynamic tail.

## Query Rewriting

`QueryRewriter` is retrieval-scoped and gated. It only calls the rewrite LLM when graph memory retrieval and a vector leg are available and cheap heuristics indicate that memory search is likely to benefit, such as multi-turn coreference, vague follow-ups, vector-first history/preference/similarity questions, or multi-hop queries without clear seeds. Empty, command-like, exact-identifier, file/path-heavy, URL-heavy, and explicit first-turn queries use the original query.

The stage is fail-open. On timeout, parsing error, circuit-breaker open, or skipped input, it stores an original-query `QueryRewriteResult` and lets the turn continue. A turn execution reuses the same rewrite metadata across repeated compile steps for one user message, so tool-result follow-up requests do not call the rewriter again.

Rewrite calls optimize for latency and cost. By default they use the fastest
cheap model configured for the provider stack, request the lowest available
reasoning effort when the provider supports reasoning controls, and do not
attach MOA tools or provider-native tools such as web search. The rewriter only
needs a direct structured response for retrieval metadata.

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
- expose allowed execution affordances without granting new capabilities
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

The manifest also states whether a published revision carries a pinned
`execution_plan` template and its stable reference/hash. Instruction-only
skills remain fully valid and selectable in Inline Execute and in Durable
`Agent` nodes. Skills are optional execution inputs: selection, absence, or a
template marker never chooses a public route or gates admission. Only an
Execute/Durable decision may instantiate the template, after which the compiler
validates the immutable snapshot against the current capability catalog and
budget.

The selected manifest is not part of the stable prefix because query keywords
and tenant-level learning can legitimately change which skills are shown for one
turn.

## Execution Routing And Planning

Execution routing happens after context compilation and is not retrieval or
skill routing. `TurnExecution` selects exactly one public route: Respond,
Execute, or NeedsInput. Execute carries its internal strategy from an explicit
classifier or trusted-route field: Inline for bounded interactive
work and Durable when the work must persist independently. A classifier may
also return one trimmed, single-line, free-form rationale of at most 240 UTF-8
bytes. That explanation remains local to the active turn, is not persisted, and
is never interpreted to select a strategy. Classifier uncertainty falls back
to Execute/Inline.

Respond and Execute/Inline make no execution-planning call. For
Execute/Durable, a selected high-confidence skill template is instantiated
without a model planning call. Otherwise `ExecutionPlanner` gives the auxiliary
model only the immutable user goal, selected skill metadata, current governed
capability catalog, resource budget, and strict output schema. It first
preserves scope, definitions, time range, universe, output form, evidence
expectations, and exclusions as stable goal-contract requirement IDs, then
emits only the exact seven-node acyclic DSL. One repair call may receive
compiler violations; an invalid second candidate becomes a typed missing-input
or unsupported result rather than a silent direct answer.

`ExecutionCompiler` validates capability/schema references, dependencies,
non-recursive maps, reducer bounds, authorization metadata, data bindings,
worst-case resources, completion coverage, and amendments. Planner provenance,
candidate JSON, compiler report, typed route source, and final canonical hash
are persisted. Classifier rationale is not. One-off compiled snapshots are not
skills and are never auto-published.

Only an initial root Execute/Inline turn may make one evidence-preserving,
one-way upgrade to Durable through the workflow-owned
`request_durable_execution` control tool. That tool is available only to the
eligible turn, must be called alone, and cannot be synthesized from an ordinary
tool result. The transition does not classify again and cannot downgrade.
Conversational `Worker` remains an interactive Inline delegation tool, not a
bulk graph scheduler. `ExecutionRun` materializes map items as stable
`ExecutionTask` rows and submits all ready work without an application fan-out
cap.

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

History compilation is the only checkpoint/compaction owner. It uses a cheap session-row watermark before doing a full-log read, emits a cumulative `Checkpoint` event through the configured summarization LLM only when thresholds are crossed, then folds that checkpoint into the same in-memory event list before rendering context. Compaction summaries are emitted as dynamic `user` messages in the history tail so the stable identity/instruction/tool prefix remains byte-stable for provider prompt caching.

## Runtime Context

`RuntimeContextProcessor` inserts volatile facts at the end of the prompt:

- current date
- tenant and admin control-plane identifiers when available
- current working directory
- git branch
- contact or admin/operator actor

These values are intentionally outside the stable prefix.

## Compaction

There is no separate compactor stage. Threshold checks, checkpoint writes, snapshot reuse, file-read deduplication, recent-turn preservation, and old-error carry-forward are all coordinated by `HistoryCompiler`. Keeping one owner prevents a later processor from rewriting already-budgeted history, mutating snapshots after history has produced them, or issuing a second summarization pass for the same turn.

## Cache-Stable File-Read Deduplication

Between checkpoints, compiled history is append-only: already-emitted messages
are never rewritten, so provider prompt caches keep matching the frozen
prefix. Dedup decisions are deterministic over the event stream:

- A full re-read whose replayed text is byte-identical to the previous
  content-bearing read of the same path renders as a short pointer on the
  **new** side; the earlier read keeps its bytes.
- A changed-content re-read renders in full with a `supersedes_stale_read`
  marker; the stale older copy is replaced with a placeholder only once a
  `Checkpoint` event lands after the superseding read — the same compile in
  which the history head changes and the cache is invalidated anyway.
- The budget boundary for older history is quantized to user-turn starts, so
  replay never opens mid-exchange and small per-turn budget variation does not
  churn the window.
- Any non-file-read result superseded by a newer successful run of the same
  `(tool, canonical input)` invocation is demoted to a placeholder under the
  same checkpoint gate; the newest run stays verbatim and old output remains
  reachable via `session_search`.

The history stage also reports per-turn divergence attribution
(`history_divergence_cause`, `history_divergence_index`,
`tokens_invalidated_downstream`) against the prior context snapshot so cache
regressions surface as metadata instead of only as provider bills.

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
